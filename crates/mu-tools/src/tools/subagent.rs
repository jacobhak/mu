//! The subagent child protocol: running a `mu --headless` child and reading its result, and the
//! result of a background command. Jobs and `task` both use it.
use super::changes;
use super::relay::{RelayAllow, RelayServer};
use super::task::depth;
use mu_base::events::{AgentEvent, EventTx, JobChanges};
use mu_base::git::{git, prune_refs, repo_root, snapshot_tree};
use mu_base::permissions::{ApprovalDecision, ApprovalRequest};
use mu_provider::provider::relay::Call;
use mu_session::session::file::MirrorFile;
use serde::Deserialize;
use serde_json::{Value, json};
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::rc::Rc;
use std::time::Duration;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

pub struct Outcome {
    pub ok: bool,
    /// The final message, or the validated `data` as a fenced JSON block when there is any.
    pub text: String,
    /// Validated result of a task given an `outputSchema`.
    pub data: Option<Value>,
    /// The result a launched worker left, applied or kept.
    pub changes: Option<JobChanges>,
}

impl Outcome {
    pub(super) fn failed(text: impl Into<String>) -> Self {
        Self { ok: false, text: text.into(), data: None, changes: None }
    }
}

/// Characters of a child's result the parent reads inline (omp's `FULL_OUTPUT_THRESHOLD`); past
/// this the head is shown and the whole text goes to an `artifact://` file.
pub const RESULT_PREVIEW_CHARS: usize = 5000;
/// Names the next artifact of the parent session: its number and path.
pub type ArtifactAlloc<'a> = &'a dyn Fn() -> (u32, std::path::PathBuf);

/// What the parent reads of a child's result: `text` itself while it is short, else its head plus a
/// pointer to the whole text. `cwd` scopes the redaction, so a secret the child echoed never lands
/// in the artifact.
pub fn preview_result(text: &str, cwd: &Path, alloc: impl FnOnce() -> (u32, std::path::PathBuf)) -> String {
    let chars = text.chars().count();
    if chars <= RESULT_PREVIEW_CHARS {
        return text.to_string();
    }
    let end = text.char_indices().nth(RESULT_PREVIEW_CHARS).map_or(text.len(), |(i, _)| i);
    let head = &text[..end];
    // Prefer a line boundary, unless the first one is near the top (pretty-printed JSON would
    // preview as a lone `{`).
    let head = match head.rfind('\n') {
        Some(i) if i >= end / 2 => &head[..i],
        _ => head,
    };
    let (id, path) = alloc();
    let saved = path
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|()| std::fs::write(&path, mu_base::secrets::redact(text, cwd).text))
        .is_ok();
    let lines = text.lines().count();
    let kb = text.len().div_ceil(1024);
    let more = chars - head.chars().count();
    let note = if saved {
        format!("[…{more} more characters; the whole result ({lines} lines, {kb} KB) is at artifact://{id}]")
    } else {
        format!("[…{more} more characters; this result could not be saved]")
    };
    format!("{head}\n\n{note}")
}

/// The version a parent puts in a task payload; a worker of another version refuses the task.
pub const MU_VERSION: &str = env!("CARGO_PKG_VERSION");

/// How long a dead parent's task ref is kept.
const REF_MAX_AGE: Duration = changes::KEPT_AFTER;

/// A pod name, a label value and a ref component all accept the result: a slug of `name` (at most
/// 40 characters) and 8 random hex digits.
pub fn launch_id(name: &str) -> String {
    let mut slug = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    slug.truncate(40);
    let slug = slug.trim_matches('-');
    let slug = if slug.is_empty() { "task" } else { slug };
    let random = uuid::Uuid::new_v4().simple().to_string();
    format!("{slug}-{}", &random[..8])
}

type ProgressFn = Rc<dyn Fn(Option<&str>)>;

/// Where `task` subagents of some kinds run: the `[task] launcher` command, for the kinds in `kinds`.
/// The launcher is the extension point of launcher contract 2 (docs/remote.md).
#[derive(Clone, Debug, Default)]
pub struct WorkerLaunch {
    pub launcher: Vec<String>,
    pub kinds: Vec<String>,
}

impl WorkerLaunch {
    pub fn runs(&self, agent: &str) -> bool {
        !self.launcher.is_empty() && self.kinds.iter().any(|k| k == agent)
    }
}

/// A subagent started through the launcher. Dropping it deletes the task ref and the bundles, so
/// they go on every way out of the run, including a launcher that never started.
pub struct Launch {
    argv: Vec<String>,
    id: String,
    root: PathBuf,
    task_ref: String,
    /// The snapshot commit the result is a change from.
    base: String,
    /// Holds the task bundle and the result bundle.
    dir: PathBuf,
    /// Told each progress line the launcher writes on stderr, then `None` once it sends a line.
    progress: Option<ProgressFn>,
}

impl Launch {
    pub fn on_progress(&mut self, f: impl Fn(Option<&str>) + 'static) {
        self.progress = Some(Rc::new(f));
    }

    fn task_bundle(&self) -> PathBuf {
        self.dir.join("task.bundle")
    }

    fn result_bundle(&self) -> PathBuf {
        self.dir.join("result.bundle")
    }
}

impl Drop for Launch {
    fn drop(&mut self) {
        let _ = std::process::Command::new("git")
            .args(["update-ref", "-d", &self.task_ref])
            .current_dir(&self.root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// How a subagent is started: `mu --headless` here, or through the launcher.
pub enum ChildCmd {
    Local(PathBuf),
    Launched(Launch),
}

/// How long a launcher that was told to stop (`TERM`) has to write its result and exit.
const LAUNCHER_CANCEL_GRACE: Duration = Duration::from_secs(30);
/// How long a launcher that closed its stream has to finish (fetch its result) and exit.
const LAUNCHER_EXIT_WAIT: Duration = Duration::from_secs(300);

/// Commits the parent's working tree (untracked files included) as `refs/mu/task/<id>` and writes
/// it, with its full history, as a bundle for the launcher to start from.
pub async fn prepare_launch(on: &WorkerLaunch, cwd: &Path, name: &str) -> Result<Launch, String> {
    if on.launcher.is_empty() {
        return Err("task: [task] launcher is empty".into());
    }
    let root = repo_root(cwd).await.map_err(|_| "task: a launched subagent needs a git repository".to_string())?;
    let _ = changes::prune(&root, REF_MAX_AGE).await;
    let _ = prune_refs(&root, "refs/mu/task/", REF_MAX_AGE).await;
    let id = launch_id(name);
    let tree = snapshot_tree(&root).await?;
    let message = format!("mu task {id}");
    let mut args = vec!["-c", "user.name=mu", "-c", "user.email=mu@localhost", "commit-tree", &tree];
    if git(&root, &["rev-parse", "--verify", "-q", "HEAD"], None).await.is_ok() {
        args.extend(["-p", "HEAD"]);
    }
    args.extend(["-m", &message]);
    let commit = git(&root, &args, None).await?.trim().to_string();
    let task_ref = format!("refs/mu/task/{id}");
    git(&root, &["update-ref", &task_ref, &commit], None).await?;
    let launch = Launch {
        argv: on.launcher.clone(),
        dir: std::env::temp_dir().join(format!("mu-launch-{id}")),
        id,
        root,
        task_ref,
        base: commit,
        progress: None,
    };
    mu_common::fs::create_private_dir(&launch.dir).map_err(|e| format!("task: cannot make the bundle: {e}"))?;
    let bundle = launch.task_bundle();
    // An empty result bundle is what a launcher that changed nothing leaves.
    std::fs::File::create(launch.result_bundle()).map_err(|e| format!("task: cannot make the result bundle: {e}"))?;
    // No timeout: a long history takes as long as it takes.
    let out = tokio::process::Command::new("git")
        .args(["bundle", "create"])
        .arg(&bundle)
        .arg(&launch.task_ref)
        .current_dir(&launch.root)
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|e| format!("task: git bundle: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!("task: git bundle: {}", err.lines().next().unwrap_or("failed").trim()));
    }
    Ok(launch)
}

/// One line of a child's stdout: a progress event, or the final result.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum ChildEvent {
    Tool {
        #[serde(default)]
        name: String,
        #[serde(default)]
        summary: String,
    },
    Session {
        #[serde(default)]
        path: String,
    },
    Model {
        #[serde(default)]
        label: String,
    },
    /// One line of a mirrored child's session file (`file` names it), sent when the payload asked.
    Entry {
        #[serde(default)]
        file: String,
        #[serde(default)]
        line: String,
    },
    /// A tool call needs approval; the parent answers with an `approval` line on stdin.
    Approve {
        id: u64,
        request: ApprovalRequest,
    },
    /// A model or search call of a relayed child; the line is read again as a `Call`.
    Relay(serde::de::IgnoredAny),
    Result(ChildResult),
}

/// Reads one line into `buf`, newline included, keeping at most `cap` bytes of it. `None` at the
/// end of input; `Some(true)` when the line was longer than `cap`, which is read to its end and
/// dropped (`buf` is then empty).
pub(super) async fn read_line_capped<R: AsyncBufRead + Unpin>(
    r: &mut R,
    buf: &mut Vec<u8>,
    cap: usize,
) -> std::io::Result<Option<bool>> {
    let mut dropped = false;
    let mut seen = false;
    loop {
        let chunk = r.fill_buf().await?;
        if chunk.is_empty() {
            break;
        }
        seen = true;
        let (end, used) = match chunk.iter().position(|b| *b == b'\n') {
            Some(i) => (true, i + 1),
            None => (false, chunk.len()),
        };
        if !dropped {
            if buf.len() + used > cap {
                dropped = true;
                buf.clear();
                buf.shrink_to_fit();
            } else {
                buf.extend_from_slice(&chunk[..used]);
            }
        }
        r.consume(used);
        if end {
            break;
        }
    }
    Ok(seen.then_some(dropped))
}

/// What a child's `entry` line is, by its `type`.
enum EntryLine {
    /// A line of the session file the parent keeps.
    Session,
    /// A cost line: the parent's own ledger is the source of a launched worker's cost.
    Cost,
    /// Not a session entry.
    Bad,
}

fn entry_line(line: &str) -> EntryLine {
    const KINDS: [&str; 9] = [
        "title",
        "session",
        "message",
        "compaction",
        "model_change",
        "thinking_level_change",
        "title_change",
        "mode_change",
        "custom",
    ];
    let Ok(Value::Object(o)) = serde_json::from_str::<Value>(line) else { return EntryLine::Bad };
    match o.get("type").and_then(Value::as_str) {
        Some("cost") => EntryLine::Cost,
        Some(t) if KINDS.contains(&t) && (t != "message" || o.get("message").is_some_and(Value::is_object)) => {
            EntryLine::Session
        }
        _ => EntryLine::Bad,
    }
}

/// How long a launched task's result ref and a dead parent's task ref are kept.

#[derive(Deserialize)]
struct ChildResult {
    #[serde(default, rename = "isError")]
    is_error: bool,
    #[serde(default)]
    text: String,
    data: Option<Value>,
    #[serde(rename = "schemaOk")]
    schema_ok: Option<bool>,
}

/// What a parent does with a child's `entry` lines.
#[derive(Clone)]
pub enum Mirror {
    /// A local child of an unmirrored parent: they are dropped.
    Off,
    /// The top parent: each file is written beside the local sessions.
    Write,
    /// A mirrored worker: they pass on to its own parent through its events.
    Forward(EventTx),
}

/// The mirror files of one run. The first is the child's own session; later ones are its children's,
/// written beside it so the cost scan, which reads the child's directory, finds them.
#[derive(Default)]
struct Mirrors {
    /// `None` once mirroring that file ended.
    files: HashMap<String, Option<MirrorFile>>,
    first: bool,
    /// The child's own file: the first one it names.
    own: Option<String>,
    /// Lines of the own file before its header, kept until the header is checked.
    held: Vec<String>,
}

impl Mirrors {
    /// Appends `line` to the mirror of `file`. Returns the path of the child's own mirror when this
    /// call created it, and a notice when mirroring `file` ends. The child's own file must open
    /// with a header whose `parentSession` is `parent`, the session that started the task.
    fn write(
        &mut self,
        cwd: &Path,
        file: &str,
        line: &str,
        parent: Option<&str>,
        launch: &str,
    ) -> (Option<String>, Option<String>) {
        let mut created = None;
        let mut marked = None;
        if !self.files.contains_key(file) {
            if *self.own.get_or_insert_with(|| file.to_string()) == file {
                let header = serde_json::from_str::<Value>(line).ok();
                match header.as_ref().and_then(|v| v["type"].as_str()) {
                    Some("title") => {
                        self.held.push(line.to_string());
                        return (None, None);
                    }
                    Some("session") if header.as_ref().and_then(|v| v["parentSession"].as_str()) == parent => {
                        marked = header
                            .as_ref()
                            .filter(|_| !launch.is_empty())
                            .map(|h| mirrored_header(h.clone(), cwd, launch));
                    }
                    Some("session") => return self.refuse(file, "its header does not name this task's session"),
                    _ => return self.refuse(file, "it does not begin with a header"),
                }
            }
            match MirrorFile::create(cwd, file) {
                Ok(mut m) => {
                    for held in self.held.drain(..) {
                        if let Err(e) = m.append_line(&held) {
                            self.files.insert(file.to_string(), None);
                            return (None, Some(format!("[session mirror of {file} stopped: {e}]")));
                        }
                    }
                    if !self.first {
                        created = Some(m.path.to_string_lossy().into_owned());
                        self.first = true;
                    }
                    self.files.insert(file.to_string(), Some(m));
                }
                Err(e) => {
                    self.files.insert(file.to_string(), None);
                    return (None, Some(format!("[session mirror of {file} not kept: {e}]")));
                }
            }
        }
        let line = marked.as_deref().unwrap_or(line);
        let Some(Some(m)) = self.files.get_mut(file) else { return (created, None) };
        match m.append_line(line) {
            Ok(()) => (created, None),
            Err(e) => {
                self.files.insert(file.to_string(), None);
                (created, Some(format!("[session mirror of {file} stopped: {e}]")))
            }
        }
    }

    fn refuse(&mut self, file: &str, why: &str) -> (Option<String>, Option<String>) {
        self.files.insert(file.to_string(), None);
        self.held.clear();
        (None, Some(format!("[session mirror of {file} refused: {why}]")))
    }
}

/// The header of a launched worker's own session as the parent keeps it: marked as a mirror of
/// launch `launch`, and located in the parent's directory (the worker's own is on another machine).
fn mirrored_header(mut header: Value, cwd: &Path, launch: &str) -> String {
    header["mirrored"] = json!(true);
    header["launchId"] = json!(launch);
    header["cwd"] = json!(cwd.to_string_lossy());
    header.to_string()
}

/// Runs one `mu --headless` subagent in `cwd`, reporting each tool call to `on_tool(name, summary)`.
/// `cancel` terminates the child's process group.
pub async fn run_child(
    exe: &Path,
    cwd: &Path,
    payload: &Value,
    cancel: &CancellationToken,
    on_tool: impl FnMut(&str, &str),
) -> Outcome {
    run_child_observed(
        ChildCmd::Local(exe.to_path_buf()),
        cwd,
        payload,
        cancel,
        on_tool,
        |_| {},
        |_| {},
        None,
        Mirror::Off,
        None,
    )
    .await
}

/// `run_child` that also reports the child's session file to `on_session(path)` as soon as the child
/// has opened it, and its model label to `on_model` (older children never say, so either may not run).
/// `approve` answers the child's approval requests; `None` means nobody can. `mirror` says what
/// to do with the child's `entry` lines; a mirrored file's local path goes to `on_session`.
/// `relay`, when set, serves the child's model and search calls; without it they are dropped.
#[allow(clippy::too_many_arguments)]
pub async fn run_child_observed(
    child: ChildCmd,
    cwd: &Path,
    payload: &Value,
    cancel: &CancellationToken,
    mut on_tool: impl FnMut(&str, &str),
    mut on_session: impl FnMut(&str),
    mut on_model: impl FnMut(&str),
    approve: Option<&dyn Fn(ApprovalRequest) -> oneshot::Receiver<ApprovalDecision>>,
    mirror: Mirror,
    relay: Option<&RelayServer>,
) -> Outcome {
    let (wtx, wrx) = tokio::sync::mpsc::unbounded_channel::<String>();
    // What the parent put in the payload is what the child may call; a payload without it, none.
    let allow: RelayAllow =
        payload.get("relayModels").and_then(|v| serde_json::from_value(v.clone()).ok()).unwrap_or_default();
    let task = payload.to_string();
    let parent_session = payload["parentSession"].as_str();
    let mut launch: Option<Launch> = None;
    let mut cmd = match child {
        ChildCmd::Local(exe) => {
            let mut cmd = tokio::process::Command::new(exe);
            // A locked state leaves the child to fail where it needs omp's credentials.
            cmd.envs(mu_common::omp_state::envs().unwrap_or_default());
            cmd.arg("--headless");
            cmd
        }
        ChildCmd::Launched(l) => {
            // No credential reaches the launcher: it gets mu's own environment and the task's paths.
            let mut cmd = tokio::process::Command::new(&l.argv[0]);
            cmd.args(&l.argv[1..])
                .env("MU_TASK_CONTRACT", "2")
                .env("MU_TASK_NAME", &l.id)
                .env("MU_TASK_ROOT", &l.root)
                .env("MU_TASK_CWD", cwd)
                .env("MU_TASK_REF", &l.task_ref)
                .env("MU_TASK_BUNDLE", l.task_bundle())
                .env("MU_TASK_RESULT_BUNDLE", l.result_bundle())
                .env("MU_TASK_PARENT_PID", std::process::id().to_string())
                .env("MU_TASK_MU_HOME", mu_common::paths::mu_home())
                .env("MU_TASK_PROFILE_HOME", mu_common::paths::profile_home())
                .env("MU_TASK_AGENT_DIR", mu_common::paths::omp_agent_dir());
            match parent_session {
                Some(s) => cmd.env("MU_TASK_SESSION", s),
                None => cmd.env_remove("MU_TASK_SESSION"),
            };
            launch = Some(l);
            cmd
        }
    };
    let launched = launch.is_some();
    cmd.current_dir(cwd)
        .env("MU_TASK_DEPTH", (depth() + 1).to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = match mu_base::process::spawn_grouped(&mut cmd, true) {
        Ok(c) => c,
        Err(e) => {
            return Outcome::failed(if launched {
                format!("cannot start the launcher: {e}")
            } else {
                format!("cannot start subagent: {e}")
            });
        }
    };
    let pid = child.id().unwrap_or(0) as i32;
    let mut stdin = child.stdin.take().expect("stdin is piped");
    // The task payload, then one line per approval answer, for as long as the child reads them.
    let mut wrx = wrx;
    tokio::task::spawn_local(async move {
        let _ = stdin.write_all(task.as_bytes()).await;
        let _ = stdin.write_all(b"\n").await;
        while let Some(line) = wrx.recv().await {
            if stdin.write_all(line.as_bytes()).await.is_err() || stdin.write_all(b"\n").await.is_err() {
                break;
            }
        }
        let _ = stdin.shutdown().await;
    });
    // A launcher's `{"progress":"…"}` lines go to `progress`; every other line is kept as the error tail.
    let progress: Option<ProgressFn> = launch.as_ref().and_then(|l| l.progress.clone());
    let progress_shown = Rc::new(std::cell::Cell::new(false));
    let stderr = child.stderr.take().expect("stderr is piped");
    let err_task = tokio::task::spawn_local({
        let (progress, shown) = (progress.clone(), progress_shown.clone());
        async move {
            let mut tail: std::collections::VecDeque<String> = Default::default();
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(l)) = lines.next_line().await {
                if let Some(f) = &progress
                    && let Some(p) = progress_line(&l)
                {
                    shown.set(true);
                    f(Some(p.as_str()));
                    continue;
                }
                if tail.len() == 20 {
                    tail.pop_front();
                }
                tail.push_back(l);
            }
            tail.into_iter().collect::<Vec<_>>().join("\n")
        }
    });
    let mut source = BufReader::new(child.stdout.take().expect("stdout is piped"));
    let job = launch.as_ref().map_or_else(|| format!("pid-{pid}"), |l| l.id.clone());
    // A launcher gets TERM and time to write its result; a local child is killed after a short grace.
    let stop = move || {
        if launched {
            mu_base::process::kill_group(pid, libc::SIGTERM);
            tokio::task::spawn_local(async move {
                tokio::time::sleep(LAUNCHER_CANCEL_GRACE).await;
                mu_base::process::kill_group(pid, libc::SIGKILL);
            })
        } else {
            tokio::task::spawn_local(mu_base::process::terminate_group(pid))
        }
    };
    let mut stopper: Option<tokio::task::JoinHandle<()>> = None;
    let relay = relay.map(|r| r.start(wtx.clone(), &job, allow));
    let mut result: Option<ChildResult> = None;
    let mut cancelled = false;
    let mut bad_line: Option<String> = None;
    let mut mirrors = Mirrors::default();
    let mut notices: Vec<String> = Vec::new();
    let mut bad_entry = false;
    // Answers already given, so a child that asks again is not asked twice.
    let answers: Rc<RefCell<HashMap<u64, Option<String>>>> = Rc::default();
    let mut line: Vec<u8> = Vec::new();
    loop {
        line.clear();
        tokio::select! {
            n = read_line_capped(&mut source, &mut line, mu_common::rpc::MAX_LINE) => match n {
                Ok(None) => break,
                Ok(Some(true)) => {
                    bad_line.get_or_insert_with(|| {
                        format!("a line longer than {} MiB was dropped", mu_common::rpc::MAX_LINE >> 20)
                    });
                }
                Ok(Some(false)) => {
                    if progress_shown.replace(false)
                        && let Some(f) = &progress
                    {
                        f(None);
                    }
                    // Bytes, not `lines()`: a stray invalid UTF-8 byte must not end the read.
                    match serde_json::from_slice::<ChildEvent>(&line) {
                        Ok(ChildEvent::Tool { name, summary }) => on_tool(&name, &summary),
                        // A launched child's session file is on the worker.
                        Ok(ChildEvent::Session { path }) => {
                            if !launched {
                                on_session(&path)
                            }
                        }
                        Ok(ChildEvent::Model { label }) => on_model(&label),
                        Ok(ChildEvent::Entry { file, line }) if !matches!(mirror, Mirror::Off) => {
                            match entry_line(&line) {
                                EntryLine::Cost => {}
                                EntryLine::Bad => {
                                    if !bad_entry {
                                        bad_entry = true;
                                        notices.push("[a line that is not a session entry was dropped from the session mirror]".into());
                                    }
                                }
                                EntryLine::Session => match &mirror {
                                    Mirror::Off => {}
                                    Mirror::Forward(tx) => {
                                        let _ = tx.send(AgentEvent::ChildEntry { file, line });
                                    }
                                    Mirror::Write => {
                                        let launch = launch.as_ref().map_or("", |l| l.id.as_str());
                                        let (created, notice) = mirrors.write(cwd, &file, &line, parent_session, launch);
                                        if let Some(path) = created {
                                            on_session(&path);
                                        }
                                        notices.extend(notice);
                                    }
                                },
                            }
                        }
                        Ok(ChildEvent::Entry { .. }) => {}
                        Ok(ChildEvent::Approve { id, request }) => {
                            let known = answers.borrow().get(&id).cloned();
                            match known {
                                Some(Some(answer)) => {
                                    let _ = wtx.send(answer);
                                }
                                Some(None) => {}
                                None => {
                                    answers.borrow_mut().insert(id, None);
                                    let rx = match approve {
                                        Some(f) => f(request),
                                        None => {
                                            let (tx, rx) = oneshot::channel();
                                            let _ = tx.send(ApprovalDecision::Unavailable);
                                            rx
                                        }
                                    };
                                    let (out, answers) = (wtx.clone(), answers.clone());
                                    tokio::task::spawn_local(async move {
                                        let d = rx.await.unwrap_or(ApprovalDecision::Deny { note: None });
                                        let answer = json!({"type": "approval", "id": id, "decision": d}).to_string();
                                        answers.borrow_mut().insert(id, Some(answer.clone()));
                                        let _ = out.send(answer);
                                    });
                                }
                            }
                        }
                        Ok(ChildEvent::Relay(_)) => {
                            let call = serde_json::from_slice::<Value>(&line).ok().and_then(|v| Call::parse(&v));
                            if let (Some(run), Some((id, call))) = (&relay, call) {
                                run.handle_with(id, call, approve);
                            }
                        }
                        Ok(ChildEvent::Result(r)) => {
                            result = Some(r);
                            if let Some(run) = &relay {
                                run.finish();
                            }
                        }
                        Err(_) => {
                            let text = String::from_utf8_lossy(&line);
                            let text = text.trim();
                            if bad_line.is_none() && !text.is_empty() {
                                bad_line = Some(text.chars().take(BAD_LINE_CHARS).collect());
                            }
                        }
                    }
                }
                Err(_) => {
                    // The pipe is broken: stop the child instead of leaving it running unobserved.
                    stopper.get_or_insert_with(&stop);
                    break;
                }
            },
            _ = cancel.cancelled(), if !cancelled => {
                cancelled = true;
                stopper.get_or_insert_with(&stop);
            }
        }
    }
    let capped = relay.as_ref().and_then(|r| r.capped());
    drop(relay);
    drop(wtx);
    // A child that ignores everything must not hang the parent: bound the wait, then kill. A
    // launcher may still be fetching its result when its stream closes.
    let wait = match (launched, stopper.is_some()) {
        (false, _) => Duration::from_secs(5),
        (true, true) => LAUNCHER_CANCEL_GRACE,
        (true, false) => LAUNCHER_EXIT_WAIT,
    };
    if tokio::time::timeout(wait, child.wait()).await.is_err() {
        mu_base::process::kill_group(pid, libc::SIGKILL);
        let _ = tokio::time::timeout(Duration::from_secs(1), child.wait()).await;
    }
    if let Some(s) = stopper {
        s.abort();
    }
    if let (Some(f), true) = (&progress, progress_shown.get()) {
        f(None);
    }
    // A descendant that inherited stderr can hold it open after the child is gone.
    let err_abort = err_task.abort_handle();
    let stderr_tail = match tokio::time::timeout(Duration::from_secs(2), err_task).await {
        Ok(tail) => tail.unwrap_or_default(),
        Err(_) => {
            err_abort.abort();
            String::new()
        }
    };
    drop(mirrors);
    let mut out = outcome(result, payload, cancelled, stderr_tail, bad_line);
    for n in notices {
        out.text.push('\n');
        out.text.push_str(&n);
    }
    if let Some(cap) = capped {
        out.text.push_str(&format!("\n[task spend cap of ${cap:.2} reached; later model calls were refused]"));
    }
    if let Some(l) = &launch {
        match changes::import_bundle(&l.root, &l.result_bundle(), &l.id, &l.base).await {
            Ok(Some(result_ref)) => {
                // A task that failed or was cancelled leaves its changes for the user to look at.
                let auto = if out.ok {
                    changes::auto_apply(&l.root, &l.task_ref, &l.base, &result_ref).await
                } else {
                    changes::Auto::Failed("the task did not finish".into())
                };
                out.text.push('\n');
                out.text.push_str(&changes::summary(&auto, &result_ref, &l.base));
                let applied = matches!(auto, changes::Auto::Applied(_));
                out.changes = Some(JobChanges { r#ref: result_ref, base: l.base.clone(), applied });
            }
            Ok(None) => {}
            Err(e) => out.text.push_str(&format!("\n[result not kept: {e}]")),
        }
    }
    out
}

/// The text of a launcher's `{"progress":"…"}` stderr line.
fn progress_line(line: &str) -> Option<String> {
    let Ok(Value::Object(o)) = serde_json::from_str::<Value>(line) else { return None };
    o.get("progress").and_then(Value::as_str).map(|s| s.chars().take(200).collect())
}

const BAD_LINE_CHARS: usize = 300;

/// What the child's final result, or its lack of one, means to the caller.
fn outcome(
    result: Option<ChildResult>,
    payload: &Value,
    cancelled: bool,
    stderr_tail: String,
    bad_line: Option<String>,
) -> Outcome {
    match result {
        Some(r) => {
            let ok = !r.is_error;
            let text = r.text;
            match r.data {
                Some(d) if ok && r.schema_ok != Some(false) => {
                    let pretty = serde_json::to_string_pretty(&d).unwrap_or_else(|_| d.to_string());
                    Outcome { ok: true, text: format!("```json\n{pretty}\n```"), data: Some(d), changes: None }
                }
                // Asked for typed data and none came back: never pass the free text off as the result.
                None if ok && payload.get("outputSchema").is_some_and(|s| !s.is_null()) => {
                    Outcome::failed(format!("subagent returned no schema-validated data\n{text}"))
                }
                _ => Outcome { ok, text, data: None, changes: None },
            }
        }
        None => Outcome::failed(if cancelled {
            "cancelled".to_string()
        } else if let Some(line) = bad_line {
            format!("subagent sent a line that is not a valid event or result: {line}")
        } else if stderr_tail.is_empty() {
            "subagent exited without a result".to_string()
        } else {
            stderr_tail
        }),
    }
}

/// A background command's finished result: the text the model sees, and how it ended.
pub struct BackgroundResult {
    pub ok: bool,
    pub text: String,
    /// "exit 3", "timed out", "cancelled", …
    pub note: String,
    pub meta: Option<mu_base::tool::BashMeta>,
}

#[cfg(test)]
mod child_line_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    async fn run(script: &str) -> Outcome {
        let base = std::env::temp_dir().join(format!("mu-child-line-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&base).unwrap();
        let exe = base.join("fake-mu");
        std::fs::write(&exe, script).unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        let out = tokio::task::LocalSet::new()
            .run_until(run_child(&exe, &base, &json!({}), &CancellationToken::new(), |_, _| {}))
            .await;
        let _ = std::fs::remove_dir_all(&base);
        out
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_malformed_result_line_is_the_error() {
        let out = run("#!/bin/sh\necho '{\"type\":\"result\",\"isError\":\"nope\",\"text\":\"lost\"}'\n").await;
        assert!(!out.ok);
        assert!(out.text.contains("lost") && out.text.contains("not a valid"), "{}", out.text);
        assert!(!out.text.contains("without a result"), "{}", out.text);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_stray_line_does_not_hide_a_good_result() {
        let out = run("#!/bin/sh\necho 'stray'\necho '{\"type\":\"result\",\"text\":\"fine\"}'\n").await;
        assert!(out.ok && out.text == "fine", "{}", out.text);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_descendant_holding_stderr_does_not_hold_the_result() {
        let started = std::time::Instant::now();
        let out = run("#!/bin/sh\nsleep 20 >/dev/null &\necho '{\"type\":\"result\",\"text\":\"fine\"}'\n").await;
        assert!(out.ok && out.text == "fine", "{}", out.text);
        assert!(started.elapsed() < std::time::Duration::from_secs(10), "{:?}", started.elapsed());
    }
}

#[cfg(test)]
mod launch_tests {
    use super::*;
    use mu_testutil::{repo, sh, tmp};
    use tokio_util::sync::CancellationToken;

    fn task_refs(root: &Path) -> String {
        sh(root, &["for-each-ref", "refs/mu/task/"])
    }

    #[test]
    fn launch_ids_are_valid_pod_names_and_differ() {
        let long = "x".repeat(200);
        for name in ["Review Auth!", "Task1", "", "!!!", long.as_str(), "-a--b-"] {
            let id = launch_id(name);
            assert!(id.len() <= 49, "{id}");
            assert!(id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'), "{id}");
            assert!(!id.starts_with('-') && !id.contains("--"), "{id}");
            assert_ne!(id, launch_id(name));
        }
        assert!(launch_id("Review Auth!").starts_with("review-auth-"));
        assert!(launch_id("Task1").starts_with("task1-"));
        assert!(launch_id("").starts_with("task-"));
        assert_eq!(launch_id(&long).len(), 40 + 1 + 8);
    }

    /// A launcher that reads the payload line, then runs `body`.
    fn launcher(body: &str) -> Scripted {
        let dir = tmp();
        let file = dir.join("launcher.sh");
        std::fs::write(&file, format!("IFS= read -r payload\n{body}\n")).unwrap();
        let launcher = vec!["sh".into(), file.to_string_lossy().into_owned()];
        Scripted { launch: WorkerLaunch { launcher, kinds: vec!["task".into()] }, _dir: dir }
    }

    /// A launcher whose script is removed when this drops.
    struct Scripted {
        launch: WorkerLaunch,
        _dir: mu_testutil::TempDir,
    }

    impl std::ops::Deref for Scripted {
        type Target = WorkerLaunch;
        fn deref(&self) -> &WorkerLaunch {
            &self.launch
        }
    }

    const DONE: &str = r#"echo '{"type":"tool","name":"read","summary":"x"}'
echo '{"type":"result","text":"done"}'"#;

    /// Shell that commits `text` as `file` on the task snapshot and leaves the result bundle holding
    /// `result_ref`. `orphan` makes a commit that does not descend from the snapshot.
    fn commit(file: &str, text: &str, orphan: bool, result_ref: &str) -> String {
        let (start, base) = if orphan {
            ("git checkout -q --orphan other", "")
        } else {
            ("git checkout -q --detach \"$MU_TASK_REF\"", " \"^$MU_TASK_REF\"")
        };
        format!(
            r#"w=$(mktemp -d); cd "$w"; git init -q
git fetch -q "$MU_TASK_BUNDLE" "$MU_TASK_REF:$MU_TASK_REF"
{start}
printf '%s\n' '{text}' > '{file}'
git add -A; git -c user.name=t -c user.email=t@t commit -q -m work
git update-ref "{result_ref}" HEAD
git bundle create -q "$MU_TASK_RESULT_BUNDLE" "{result_ref}"{base}"#
        )
    }

    fn writing(file: &str, text: &str) -> String {
        commit(file, text, false, "refs/mu/result/$MU_TASK_NAME")
    }

    async fn go(
        root: &Path,
        on: &WorkerLaunch,
        name: &str,
        payload: &Value,
        cancel: &CancellationToken,
        tools: &RefCell<Vec<String>>,
    ) -> Outcome {
        let launch = prepare_launch(on, root, name).await.unwrap();
        run_child_observed(
            ChildCmd::Launched(launch),
            root,
            payload,
            cancel,
            |n, s| tools.borrow_mut().push(format!("{n} {s}")),
            |_| {},
            |_| {},
            None,
            Mirror::Off,
            None,
        )
        .await
    }

    async fn run_on(root: &Path, on: &WorkerLaunch, name: &str) -> Outcome {
        let tools = RefCell::new(Vec::new());
        tokio::task::LocalSet::new()
            .run_until(go(root, on, name, &json!({"task": "go"}), &CancellationToken::new(), &tools))
            .await
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_result_over_an_unchanged_tree_is_applied() {
        let root = repo();
        let out = run_on(&root, &launcher(&format!("{DONE}\n{}", writing("a.txt", "worked"))), "t").await;
        assert!(out.ok && out.text.contains("[changes: applied: 1 files; ref refs/mu/result/t-"), "{}", out.text);
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "worked\n");
        assert!(out.changes.is_some_and(|c| c.applied));
        assert_eq!(task_refs(&root), "", "the task ref is gone after the run");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_result_is_kept_when_a_file_it_touches_changed_meanwhile() {
        let root = repo();
        let body = format!("echo mine > {}/a.txt\n{DONE}\n{}", root.display(), writing("a.txt", "worked"));
        let out = run_on(&root, &launcher(&body), "t").await;
        assert!(out.ok, "{}", out.text);
        let line = out.text.lines().last().unwrap();
        assert!(
            line.starts_with("[changes: not applied: a.txt changed since the task started; ref refs/mu/result/t-"),
            "{line}"
        );
        assert!(line.contains("| git apply --3way]"), "{line}");
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "mine\n");
        let changes = out.changes.unwrap();
        assert!(!changes.applied && !sh(&root, &["for-each-ref", &changes.r#ref]).is_empty(), "the ref stays");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn two_results_finishing_together_are_both_applied() {
        let root = repo();
        let (one, two) = (
            launcher(&format!("{DONE}\n{}", writing("a.txt", "first"))),
            launcher(&format!("{DONE}\n{}", writing("b.txt", "second"))),
        );
        let (o1, o2) = tokio::task::LocalSet::new()
            .run_until(async {
                let go = |on: WorkerLaunch, name: &'static str| {
                    let root = root.to_path_buf();
                    async move {
                        go(&root, &on, name, &json!({"task": "go"}), &CancellationToken::new(), &RefCell::default())
                            .await
                    }
                };
                tokio::join!(go(one.launch.clone(), "one"), go(two.launch.clone(), "two"))
            })
            .await;
        assert!(
            o1.text.contains("applied: 1 files") && o2.text.contains("applied: 1 files"),
            "{} / {}",
            o1.text,
            o2.text
        );
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "first\n");
        assert_eq!(std::fs::read_to_string(root.join("b.txt")).unwrap(), "second\n");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_failed_task_leaves_its_result_unapplied() {
        let root = repo();
        let body = format!(
            "echo '{{\"type\":\"result\",\"isError\":true,\"text\":\"broke\"}}'\n{}",
            writing("a.txt", "worked")
        );
        let out = run_on(&root, &launcher(&body), "t").await;
        assert!(!out.ok && out.text.contains("not applied: the task did not finish"), "{}", out.text);
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "one\n");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_launcher_that_changed_nothing_leaves_no_changes_line() {
        let root = repo();
        let out = run_on(&root, &launcher(DONE), "t").await;
        assert!(out.ok && out.text == "done" && out.changes.is_none(), "{}", out.text);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn the_launcher_gets_the_payload_and_the_contract_environment() {
        let root = repo();
        std::fs::write(root.join("new.txt"), "untracked\n").unwrap();
        let log = tmp();
        let body = format!(
            r#"printf '%s' "$payload" > {l}/payload
env | grep '^MU_TASK_' | sort > {l}/env
pwd > {l}/pwd
git bundle list-heads "$MU_TASK_BUNDLE" > {l}/heads
[ -f "$MU_TASK_RESULT_BUNDLE" ] && [ ! -s "$MU_TASK_RESULT_BUNDLE" ] && echo empty > {l}/result
{DONE}"#,
            l = log.display()
        );
        let payload = json!({"task": "go", "parentSession": "/p/s.jsonl"});
        let tools = RefCell::new(Vec::new());
        let out = tokio::task::LocalSet::new()
            .run_until(go(&root, &launcher(&body), "Review Auth!", &payload, &CancellationToken::new(), &tools))
            .await;
        assert!(out.ok, "{}", out.text);
        assert_eq!(*tools.borrow(), ["read x"]);
        assert_eq!(
            serde_json::from_str::<Value>(&std::fs::read_to_string(log.join("payload")).unwrap()).unwrap(),
            payload
        );
        let env = std::fs::read_to_string(log.join("env")).unwrap();
        let var = |k: &str| {
            env.lines()
                .find_map(|l| l.strip_prefix(&format!("{k}=")))
                .unwrap_or_else(|| panic!("{k} in {env}"))
                .to_string()
        };
        let id = var("MU_TASK_NAME");
        assert!(id.starts_with("review-auth-"), "{id}");
        assert_eq!(var("MU_TASK_CONTRACT"), "2");
        assert_eq!(var("MU_TASK_DEPTH"), "1");
        assert_eq!(var("MU_TASK_ROOT"), root.to_string_lossy());
        assert_eq!(var("MU_TASK_CWD"), root.to_string_lossy());
        assert_eq!(var("MU_TASK_REF"), format!("refs/mu/task/{id}"));
        assert_eq!(var("MU_TASK_SESSION"), "/p/s.jsonl");
        assert_eq!(var("MU_TASK_PARENT_PID"), std::process::id().to_string());
        for k in ["MU_TASK_MU_HOME", "MU_TASK_PROFILE_HOME", "MU_TASK_AGENT_DIR"] {
            assert!(!var(k).is_empty(), "{k}");
        }
        assert_eq!(std::fs::read_to_string(log.join("pwd")).unwrap().trim(), root.to_string_lossy());
        let heads = std::fs::read_to_string(log.join("heads")).unwrap();
        assert!(heads.trim().ends_with(&format!("refs/mu/task/{id}")) && heads.lines().count() == 1, "{heads}");
        assert_eq!(std::fs::read_to_string(log.join("result")).unwrap().trim(), "empty");
        let bundle = var("MU_TASK_BUNDLE");
        assert!(
            !Path::new(&bundle).exists() && !Path::new(&var("MU_TASK_RESULT_BUNDLE")).exists(),
            "gone with the run"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn the_task_bundle_carries_the_working_tree_and_the_full_history() {
        let root = repo();
        std::fs::write(root.join("new.txt"), "untracked\n").unwrap();
        let before = sh(&root, &["rev-list", "--count", "HEAD"]);
        let out = run_on(&root, &launcher(&format!("{DONE}\n{}", writing("new.txt", "worked"))), "t").await;
        // The launcher built its result on the bundle's snapshot: the untracked file was in it.
        assert!(out.ok && out.text.contains("applied: 1 files"), "{}", out.text);
        assert_eq!(std::fs::read_to_string(root.join("new.txt")).unwrap(), "worked\n");
        assert_eq!(sh(&root, &["rev-list", "--count", "HEAD"]), before);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_result_bundle_of_the_wrong_shape_is_not_imported() {
        let root = repo();
        for (orphan, name, why) in [
            (false, "refs/mu/result/other", "does not hold exactly"),
            (false, "refs/heads/main", "does not hold exactly"),
            (true, "refs/mu/result/$MU_TASK_NAME", "does not descend from the task snapshot"),
        ] {
            let out = run_on(&root, &launcher(&format!("{DONE}\n{}", commit("a.txt", "x", orphan, name))), "t").await;
            assert!(out.ok && out.changes.is_none() && out.text.contains(why), "{why}: {}", out.text);
            assert_eq!(sh(&root, &["for-each-ref", "refs/mu/result/"]), "", "nothing was kept");
        }
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "one\n");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn progress_lines_on_stderr_go_to_the_job_and_the_rest_is_the_error() {
        let root = repo();
        let body = format!(
            r#"echo '{{"progress":"scheduling"}}' >&2
echo '{{"progress":"pulling image"}}' >&2
sleep 0.3
echo plain >&2
{DONE}"#
        );
        let seen: Rc<RefCell<Vec<Option<String>>>> = Rc::default();
        let out = tokio::task::LocalSet::new()
            .run_until(async {
                let scripted = launcher(&body);
                let mut launch = prepare_launch(&scripted, &root, "t").await.unwrap();
                let s = seen.clone();
                launch.on_progress(move |p| s.borrow_mut().push(p.map(str::to_string)));
                run_child_observed(
                    ChildCmd::Launched(launch),
                    &root,
                    &json!({"task": "go"}),
                    &CancellationToken::new(),
                    |_, _| {},
                    |_| {},
                    |_| {},
                    None,
                    Mirror::Off,
                    None,
                )
                .await
            })
            .await;
        assert!(out.ok && out.text == "done", "{}", out.text);
        assert_eq!(
            *seen.borrow(),
            [Some("scheduling".to_string()), Some("pulling image".to_string()), None],
            "each step, then cleared"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn the_launchers_stderr_is_the_error_when_it_left_no_result() {
        let root = repo();
        let out = run_on(&root, &launcher("echo 'boom, it broke' >&2; exit 1"), "t").await;
        assert!(!out.ok && out.text == "boom, it broke", "{}", out.text);
        assert_eq!(task_refs(&root), "");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn the_task_ref_is_gone_when_the_launcher_cannot_start() {
        let root = repo();
        let on = WorkerLaunch { launcher: vec!["/nonexistent/launcher".into()], kinds: vec!["task".into()] };
        let out = run_on(&root, &on, "t").await;
        assert!(!out.ok && out.text.starts_with("cannot start the launcher"), "{}", out.text);
        assert_eq!(task_refs(&root), "");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_launch_needs_a_git_repository() {
        let err = prepare_launch(&launcher(DONE), &tmp(), "t").await.err().unwrap();
        assert!(err.contains("needs a git repository"), "{err}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_repository_with_no_commit_gets_a_parentless_snapshot() {
        let root = tmp();
        sh(&root, &["init", "-q"]);
        std::fs::write(root.join("f"), "x\n").unwrap();
        let launch = prepare_launch(&launcher(DONE), &root, "t").await.unwrap();
        let parents = sh(&root, &["rev-list", "--parents", "-n1", &launch.task_ref]);
        assert_eq!(parents.split_whitespace().count(), 1, "{parents}");
    }

    fn alive(pid: &str) -> bool {
        let out = std::process::Command::new("ps").args(["-o", "stat=", "-p", pid]).output().unwrap();
        let stat = String::from_utf8_lossy(&out.stdout);
        !stat.trim().is_empty() && !stat.trim().starts_with('Z')
    }

    /// Cancel is `TERM` to the launcher's group: it writes the result it has and exits, and nothing it
    /// started is left.
    #[tokio::test(flavor = "current_thread")]
    async fn cancel_ends_the_launchers_group_after_it_wrote_what_it_has() {
        let root = repo();
        let log = tmp();
        let body = format!(
            r#"result() {{
{result}
}}
sleep 60 &
echo $! > {l}/sleeper
trap 'result; echo term > {l}/term; exit 0' TERM
echo '{{"type":"tool","name":"read","summary":"x"}}'
wait"#,
            l = log.display(),
            result = writing("a.txt", "partial")
        );
        let cancel = CancellationToken::new();
        let started = std::time::Instant::now();
        let out = tokio::task::LocalSet::new()
            .run_until(async {
                let scripted = launcher(&body);
                let launch = prepare_launch(&scripted, &root, "t").await.unwrap();
                // The first tool line proves the launcher is running and has set its trap.
                run_child_observed(
                    ChildCmd::Launched(launch),
                    &root,
                    &json!({"task": "go"}),
                    &cancel,
                    |_, _| cancel.cancel(),
                    |_| {},
                    |_| {},
                    None,
                    Mirror::Off,
                    None,
                )
                .await
            })
            .await;
        assert!(started.elapsed() < Duration::from_secs(20), "{:?}", started.elapsed());
        assert!(!out.ok && out.text.starts_with("cancelled"), "{}", out.text);
        assert!(out.text.contains("not applied: the task did not finish"), "its partial result is kept: {}", out.text);
        assert_eq!(std::fs::read_to_string(log.join("term")).unwrap().trim(), "term");
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "one\n");
        let sleeper = std::fs::read_to_string(log.join("sleeper")).unwrap();
        assert!(!alive(sleeper.trim()), "the group is gone");
    }
}

#[cfg(test)]
mod preview_tests {
    use super::*;

    /// omp's envelope: a short result as is, a long one as its head plus a pointer to the artifact.
    #[test]
    fn a_long_result_is_previewed_and_saved() {
        let d = std::env::temp_dir().join(format!("mu-preview-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        let path = d.join("7.log");
        let alloc = || (7, path.clone());
        let short = "x".repeat(RESULT_PREVIEW_CHARS);
        assert_eq!(preview_result(&short, &d, alloc), short, "a result at the budget is shown whole");

        let long: String = (0..400).map(|i| format!("line {i} of the report\n")).collect();
        let text = preview_result(&long, &d, alloc);
        assert!(text.len() < long.len(), "the preview is shorter");
        assert!(text.contains("the whole result (400 lines"), "{}", text);
        assert!(text.contains("artifact://7"), "{}", text);
        assert!(text.starts_with("line 0 of the report\n"), "the head is a line boundary: {text}");
        assert!(!text.contains("line 399"), "the tail is left to the artifact");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), long, "the artifact holds the whole text");
        std::fs::remove_dir_all(&d).ok();
    }

    /// A secret in a child's result must not reach the artifact the parent is pointed at.
    #[test]
    fn the_artifact_is_redacted() {
        let d = std::env::temp_dir().join(format!("mu-preview-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        mu_base::secrets::register_stored("PREVIEW_TEST_KEY", "Qz9-preview-test-value");
        let path = d.join("8.log");
        let long = format!("{}{}\n", "x".repeat(RESULT_PREVIEW_CHARS), "Qz9-preview-test-value");
        let _ = preview_result(&long, &d, || (8, path.clone()));
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(!saved.contains("Qz9-preview"), "{saved}");
        assert!(saved.contains("[REDACTED:PREVIEW_TEST_KEY]"), "{saved}");
        std::fs::remove_dir_all(&d).ok();
    }
}

#[cfg(test)]
mod mirror_tests {
    use super::*;
    use mu_base::process::try_flock;
    use mu_session::session::file::SessionFile;
    use mu_session::session::tail::Tail;
    use mu_session::session::tree_cost::{Child, TreeCost, session_cost};
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use tokio_util::sync::CancellationToken;

    /// A session file as a child would have written it: title slot, header, one reply costing `cost`.
    fn source(cwd: &Path, parent: Option<&Path>, cost: f64) -> PathBuf {
        let s = SessionFile::create(cwd, parent).unwrap();
        let path = s.path.clone();
        drop(s);
        let m = json!({"role":"assistant","content":[{"type":"text","text":"hi"}],"api":"a","provider":"p","model":"m","usage":{"input":1,"output":1,"cacheRead":0,"cacheWrite":0,
            "totalTokens":2,"cost":{"total":cost}},"stopReason":"stop","timestamp":1});
        let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(f, "{}", json!({"type":"message","id":"x","parentId":null,"message":m})).unwrap();
        path
    }

    fn name(p: &Path) -> String {
        p.file_name().unwrap().to_string_lossy().into_owned()
    }

    /// The `entry` lines a child sends for `lines` of the file `file`.
    fn entries(file: &str, lines: &[String]) -> Vec<String> {
        lines.iter().map(|l| json!({"type": "entry", "file": file, "line": l}).to_string()).collect()
    }

    fn lines_of(p: &Path) -> Vec<String> {
        std::fs::read_to_string(p).unwrap().lines().map(String::from).collect()
    }

    #[test]
    fn a_launched_workers_own_header_is_kept_as_a_mirror_in_the_parents_directory() {
        mu_testutil::home();
        let cwd = Path::new("/opt/mirror-header");
        let header =
            json!({"type":"session","version":3,"id":"w","timestamp":"t","cwd":"/work","parentSession":"/p.jsonl"});
        let kid =
            json!({"type":"session","version":3,"id":"k","timestamp":"t","cwd":"/work","parentSession":"/w.jsonl"});
        let mut m = Mirrors::default();
        let name = |n: &str| format!("2026-01-01T00-00-00-000Z_{n}.jsonl");
        let (created, notice) = m.write(cwd, &name("w"), &header.to_string(), Some("/p.jsonl"), "t-9");
        assert!(notice.is_none(), "{notice:?}");
        m.write(cwd, &name("k"), &kid.to_string(), Some("/p.jsonl"), "t-9");
        let own = lines_of(Path::new(&created.unwrap()));
        let v: Value = serde_json::from_str(&own[0]).unwrap();
        assert_eq!(
            (v["mirrored"].clone(), v["launchId"].clone(), v["cwd"].clone()),
            (json!(true), json!("t-9"), json!("/opt/mirror-header"))
        );
        assert_eq!(v["parentSession"], "/p.jsonl");
        let kid_path = mu_common::paths::session_dir(cwd).join(name("k"));
        assert_eq!(lines_of(&kid_path), [kid.to_string()], "a nested child's header is copied as it came");
    }

    /// A child that prints `stream`, then a result.
    fn child(dir: &Path, stream: &[String]) -> PathBuf {
        let data = dir.join("stream.ndjson");
        std::fs::write(&data, stream.iter().map(|l| format!("{l}\n")).collect::<String>()).unwrap();
        let exe = dir.join("fake-mu");
        let body = format!("#!/bin/sh\ncat '{}'\necho '{{\"type\":\"result\",\"text\":\"ok\"}}'\n", data.display());
        std::fs::write(&exe, body).unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        exe
    }

    async fn run(exe: PathBuf, cwd: &Path, on_session: impl FnMut(&str)) -> Outcome {
        run_with(exe, cwd, json!({}), on_session).await
    }

    async fn run_with(exe: PathBuf, cwd: &Path, payload: Value, on_session: impl FnMut(&str)) -> Outcome {
        tokio::task::LocalSet::new()
            .run_until(run_child_observed(
                ChildCmd::Local(exe),
                cwd,
                &payload,
                &CancellationToken::new(),
                |_, _| {},
                on_session,
                |_| {},
                None,
                Mirror::Write,
                None,
            ))
            .await
    }

    fn locked(path: &Path) -> bool {
        let f = std::fs::File::open(path).unwrap();
        try_flock(&f, libc::LOCK_EX).is_err()
    }

    #[tokio::test(flavor = "current_thread")]
    async fn entries_become_a_locked_session_file_beside_the_local_ones() {
        mu_testutil::home();
        let (src, cwd, dir) = (mu_testutil::tmp(), mu_testutil::tmp(), mu_testutil::tmp());
        let remote = source(&src, None, 0.25);
        let lines = lines_of(&remote);
        let exe = child(&dir, &entries(&name(&remote), &lines));
        let seen = std::cell::RefCell::new(Vec::new());
        let held = std::cell::Cell::new(false);
        let out = run(exe, &cwd, |p| {
            held.set(locked(Path::new(p)));
            seen.borrow_mut().push(p.to_string());
        })
        .await;
        assert!(out.ok && out.text == "ok", "{}", out.text);
        let mirror = mu_common::paths::session_dir(&cwd).join(name(&remote));
        assert_eq!(*seen.borrow(), [mirror.to_string_lossy()]);
        assert!(held.get(), "the file is locked while the run goes");
        // A child another test forks meanwhile shares the descriptor, and with it the lock, until its exec.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while locked(&mirror) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(!locked(&mirror), "and free after it");
        assert_eq!(std::fs::read(&mirror).unwrap(), std::fs::read(&remote).unwrap());
        assert!((session_cost(&mirror) - 0.25).abs() < 1e-9);
        let mut tail = Tail::new(&mirror, 10);
        tail.poll();
        assert_eq!(tail.messages().len(), 1);
        // A finished mirror is an ordinary session.
        let open = SessionFile::open_append(&mirror).unwrap();
        assert_eq!(mu_session::session::format::load(&open.path).unwrap().messages.len(), 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_title_change_entry_rewrites_the_title_slot() {
        mu_testutil::home();
        let (src, cwd, dir) = (mu_testutil::tmp(), mu_testutil::tmp(), mu_testutil::tmp());
        let remote = source(&src, None, 0.0);
        let mut lines = lines_of(&remote);
        lines.push(
            json!({"type":"title_change","id":"t","parentId":null,"timestamp":"x","title":"A new title","source":"auto"})
                .to_string(),
        );
        let exe = child(&dir, &entries(&name(&remote), &lines));
        assert!(run(exe, &cwd, |_| {}).await.ok);
        let mirror = mu_common::paths::session_dir(&cwd).join(name(&remote));
        let written = lines_of(&mirror);
        assert_eq!(written.len(), lines.len(), "no second title_change was appended");
        assert_eq!(&written[1..], &lines[1..]);
        let slot: Value = serde_json::from_str(&written[0]).unwrap();
        assert_eq!((slot["type"].as_str(), slot["title"].as_str()), (Some("title"), Some("A new title")));
        assert_eq!(std::fs::read(&mirror).unwrap()[255], b'\n');
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_bad_or_taken_name_ends_that_mirror_with_one_notice() {
        mu_testutil::home();
        let (src, cwd, dir) = (mu_testutil::tmp(), mu_testutil::tmp(), mu_testutil::tmp());
        let remote = source(&src, None, 0.0);
        let lines = lines_of(&remote);
        let taken = mu_common::paths::session_dir(&cwd).join(name(&remote));
        std::fs::create_dir_all(taken.parent().unwrap()).unwrap();
        std::fs::write(&taken, "mine\n").unwrap();
        let mut stream = entries(&name(&remote), &lines);
        stream.extend(entries("../escape.jsonl", &lines));
        stream.extend(entries("notes.txt", &lines));
        let exe = child(&dir, &stream);
        let called = std::cell::Cell::new(0);
        let out = run(exe, &cwd, |_| called.set(called.get() + 1)).await;
        assert!(out.ok && out.text.starts_with("ok"), "{}", out.text);
        assert_eq!(out.text.matches("not kept").count(), 3, "{}", out.text);
        assert_eq!(called.get(), 0);
        assert_eq!(std::fs::read_to_string(&taken).unwrap(), "mine\n", "an existing file is left alone");
        assert!(!mu_common::paths::session_dir(&cwd).join("../escape.jsonl").exists());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_mirror_whose_header_names_another_session_is_refused() {
        mu_testutil::home();
        let (src, cwd, dir) = (mu_testutil::tmp(), mu_testutil::tmp(), mu_testutil::tmp());
        let theirs = source(&src, None, 0.0);
        let foreign = source(&src, Some(&theirs), 0.5);
        let lines = lines_of(&foreign);
        let mirror = mu_common::paths::session_dir(&cwd).join(name(&foreign));
        let seen = std::cell::Cell::new(0);
        let payload = json!({"parentSession": "/sessions/this-one.jsonl"});
        let out =
            run_with(child(&dir, &entries(&name(&foreign), &lines)), &cwd, payload, |_| seen.set(seen.get() + 1)).await;
        assert!(out.ok && out.text.contains("refused: its header does not name this task's session"), "{}", out.text);
        assert!(!mirror.exists(), "nothing was written");
        assert_eq!(seen.get(), 0);
        // The same header is accepted when it names this session.
        let payload = json!({"parentSession": theirs.to_string_lossy()});
        let out = run_with(child(&dir, &entries(&name(&foreign), &lines)), &cwd, payload, |_| {}).await;
        assert!(out.ok && out.text == "ok", "{}", out.text);
        assert_eq!(std::fs::read(&mirror).unwrap(), std::fs::read(&foreign).unwrap());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_mirror_that_does_not_begin_with_a_header_is_refused() {
        mu_testutil::home();
        let (src, cwd, dir) = (mu_testutil::tmp(), mu_testutil::tmp(), mu_testutil::tmp());
        let remote = source(&src, None, 0.0);
        let lines = lines_of(&remote);
        // The header is missing: title slot, then a message.
        let stream = entries(&name(&remote), &[lines[0].clone(), lines[2].clone()]);
        let out = run(child(&dir, &stream), &cwd, |_| {}).await;
        assert!(out.text.contains("refused: it does not begin with a header"), "{}", out.text);
        assert!(!mu_common::paths::session_dir(&cwd).join(name(&remote)).exists());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cost_lines_and_lines_that_are_not_entries_never_reach_the_mirror() {
        mu_testutil::home();
        let (src, cwd, dir) = (mu_testutil::tmp(), mu_testutil::tmp(), mu_testutil::tmp());
        let remote = source(&src, None, 0.25);
        let mut lines = lines_of(&remote);
        let forged = [
            json!({"type": "cost", "total": 100.0}).to_string(),
            "not json".to_string(),
            json!({"type": "unheard-of"}).to_string(),
            json!({"type": "message", "message": "a string"}).to_string(),
            json!(["type", "message"]).to_string(),
        ];
        lines.splice(2..2, forged.iter().cloned());
        let out = run(child(&dir, &entries(&name(&remote), &lines)), &cwd, |_| {}).await;
        assert!(out.ok, "{}", out.text);
        assert_eq!(out.text.matches("not a session entry").count(), 1, "one notice: {}", out.text);
        let mirror = mu_common::paths::session_dir(&cwd).join(name(&remote));
        assert_eq!(std::fs::read(&mirror).unwrap(), std::fs::read(&remote).unwrap());
        assert!((session_cost(&mirror) - 0.25).abs() < 1e-9);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_line_over_the_cap_is_dropped_and_the_result_after_it_still_counts() {
        let dir = mu_testutil::tmp();
        let exe = dir.join("fake-mu");
        let over = mu_common::rpc::MAX_LINE + 1024;
        let body = format!(
            "#!/bin/sh\nhead -c {over} /dev/zero | tr '\\0' a\necho\necho '{{\"type\":\"result\",\"text\":\"fine\"}}'\n"
        );
        std::fs::write(&exe, body).unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        let out = run(exe, &dir, |_| {}).await;
        assert!(out.ok && out.text == "fine", "{}", out.text);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn read_line_capped_keeps_lines_up_to_the_cap_and_skips_longer_ones() {
        let mut r = tokio::io::BufReader::with_capacity(4, &b"abc\nabcdefghij\nxy\nlast"[..]);
        let mut buf = Vec::new();
        let mut got = Vec::new();
        loop {
            buf.clear();
            match read_line_capped(&mut r, &mut buf, 5).await.unwrap() {
                None => break,
                Some(dropped) => got.push((dropped, String::from_utf8_lossy(&buf).into_owned())),
            }
        }
        let want = [(false, "abc\n"), (true, ""), (false, "xy\n"), (false, "last")];
        assert_eq!(got, want.map(|(d, s)| (d, s.to_string())));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_nested_mirror_is_counted_with_its_parent_whatever_its_own_cwd() {
        mu_testutil::home();
        let (root_cwd, kid_cwd, grand_cwd, cwd, dir) =
            (mu_testutil::tmp(), mu_testutil::tmp(), mu_testutil::tmp(), mu_testutil::tmp(), mu_testutil::tmp());
        let root = source(&root_cwd, None, 5.0);
        let kid = source(&kid_cwd, Some(&root), 0.25);
        let grand = source(&grand_cwd, Some(&kid), 0.125);
        let mut stream = entries(&name(&kid), &lines_of(&kid));
        stream.extend(entries(&name(&grand), &lines_of(&grand)));
        let exe = child(&dir, &stream);
        let seen = std::cell::RefCell::new(Vec::new());
        let payload = json!({"parentSession": root.to_string_lossy()});
        assert!(run_with(exe, &cwd, payload, |p| seen.borrow_mut().push(p.to_string())).await.ok);
        let beside = mu_common::paths::session_dir(&cwd);
        let (kid_mirror, grand_mirror) = (beside.join(name(&kid)), beside.join(name(&grand)));
        assert_eq!(*seen.borrow(), [kid_mirror.to_string_lossy()], "only the child's own file is its session");
        assert!(grand_mirror.exists());
        let mut t = TreeCost::default();
        t.poll(Some(&root), &[Child { path: kid_mirror, running: false }]);
        assert!((t.total() - 0.375).abs() < 1e-9, "{}", t.total());
    }
}
