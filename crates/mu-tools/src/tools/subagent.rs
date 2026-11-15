//! The subagent child protocol: running a `mu --headless` child and reading its result, and the
//! result of a background command. Jobs and `task` both use it.
use super::relay::{RelayAllow, RelayServer};
use super::task::depth;
use mu_base::events::{AgentEvent, EventTx};
use mu_base::git::{git, prune_refs, repo_root, short_sha, snapshot_tree};
use mu_base::permissions::{ApprovalDecision, ApprovalRequest};
use mu_provider::provider::relay::Call;
use mu_session::session::file::MirrorFile;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
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
}

impl Outcome {
    pub(super) fn failed(text: impl Into<String>) -> Self {
        Self { ok: false, text: text.into(), data: None }
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

/// The file descriptor a launcher reports `changes` on. The worker's stdout cannot reach it.
const CHANGES_FD: i32 = 3;

/// How long a launched task's result ref and a dead parent's task ref are kept.
const REF_MAX_AGE: Duration = Duration::from_secs(14 * 24 * 3600);

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

/// A subagent started through `[task] launcher`. Dropping it deletes the task ref, so the ref goes
/// on every way out of the run, including a launcher that never started.
pub struct Launch {
    argv: Vec<String>,
    id: String,
    root: PathBuf,
    session: String,
    task_ref: String,
    result_ref: String,
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
    }
}

/// How a subagent is started: `mu --headless` here, or the launcher.
pub enum ChildCmd {
    Local(PathBuf),
    Launched(Launch),
}

/// Commits the parent's working tree (untracked files included) as `refs/mu/task/<id>` for the
/// launcher to ship. `session` is the parent's session id, empty when it has no file yet.
pub async fn prepare_launch(launcher: &[String], cwd: &Path, name: &str, session: &str) -> Result<Launch, String> {
    if launcher.is_empty() {
        return Err("task: the launcher is empty".into());
    }
    let root = repo_root(cwd).await.map_err(|_| "task: a launched subagent needs a git repository".to_string())?;
    for prefix in ["refs/mu/result/", "refs/mu/task/"] {
        let _ = prune_refs(&root, prefix, REF_MAX_AGE).await;
    }
    let id = launch_id(name);
    let tree = snapshot_tree(&root).await?;
    let message = format!("mu task {id}");
    let mut args = vec!["-c", "user.name=mu", "-c", "user.email=mu@localhost", "commit-tree", &tree];
    if git(&root, &["rev-parse", "--verify", "-q", "HEAD"], None).await.is_ok() {
        args.extend(["-p", "HEAD"]);
    }
    args.extend(["-m", &message]);
    let commit = git(&root, &args, None).await?;
    let task_ref = format!("refs/mu/task/{id}");
    git(&root, &["update-ref", &task_ref, commit.trim()], None).await?;
    Ok(Launch {
        argv: launcher.to_vec(),
        result_ref: format!("refs/mu/result/{id}"),
        id,
        root,
        session: session.to_string(),
        task_ref,
    })
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
    /// Only the launcher may report changes, on `CHANGES_FD`; on the worker's stdout it is ignored.
    Changes(serde::de::IgnoredAny),
}

/// A launcher's report of the worker's changes, after the `result` line.
#[derive(Deserialize)]
struct ChangesLine {
    #[serde(rename = "type")]
    ty: String,
    r#ref: String,
    base: String,
    #[serde(default)]
    files: u64,
}

/// The `changes` reports in what the launcher wrote on `CHANGES_FD`: the last one that names this
/// launch's own result ref and a commit hash as its base.
fn read_changes(pipe: &mut std::fs::File, result_ref: &str) -> Option<(String, String, u64)> {
    use std::io::Read;
    let mut data = Vec::new();
    let mut buf = [0u8; 4096];
    // The pipe is non-blocking and the launcher has written before it exited; a leftover process
    // that holds the write end open must not hold this read.
    while data.len() < 64 * 1024 {
        match pipe.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => data.extend_from_slice(&buf[..n]),
        }
    }
    let valid_base = |b: &str| matches!(b.len(), 40 | 64) && b.bytes().all(|c| c.is_ascii_hexdigit());
    String::from_utf8_lossy(&data)
        .lines()
        .filter_map(|l| serde_json::from_str::<ChangesLine>(l).ok())
        .rfind(|c| c.ty == "changes" && c.r#ref == result_ref && valid_base(&c.base))
        .map(|c| (c.r#ref, c.base, c.files))
}

/// Gives `cmd` a pipe as file descriptor `CHANGES_FD` and returns its read end.
fn changes_pipe(cmd: &mut tokio::process::Command) -> std::io::Result<(std::fs::File, std::os::fd::OwnedFd)> {
    use std::os::fd::AsRawFd;
    let (tx, rx) = tokio::net::unix::pipe::pipe()?;
    let (write, read) = (tx.into_blocking_fd()?, rx.into_nonblocking_fd()?);
    let w = write.as_raw_fd();
    // SAFETY: dup2 and fcntl are async-signal-safe.
    unsafe {
        cmd.pre_exec(move || {
            if w == CHANGES_FD {
                if libc::fcntl(w, libc::F_SETFD, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
            } else if libc::dup2(w, CHANGES_FD) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok((std::fs::File::from(read), write))
}

/// Reads one line into `buf`, newline included, keeping at most `cap` bytes of it. `None` at the
/// end of input; `Some(true)` when the line was longer than `cap`, which is read to its end and
/// dropped (`buf` is then empty).
async fn read_line_capped<R: AsyncBufRead + Unpin>(
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

/// What a worker's `entry` line is, by its `type`.
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
    fn write(&mut self, cwd: &Path, file: &str, line: &str, parent: Option<&str>) -> (Option<String>, Option<String>) {
        let mut created = None;
        if !self.files.contains_key(file) {
            if *self.own.get_or_insert_with(|| file.to_string()) == file {
                let header = serde_json::from_str::<Value>(line).ok();
                match header.as_ref().and_then(|v| v["type"].as_str()) {
                    Some("title") => {
                        self.held.push(line.to_string());
                        return (None, None);
                    }
                    Some("session") if header.as_ref().and_then(|v| v["parentSession"].as_str()) == parent => {}
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
    let launched = matches!(child, ChildCmd::Launched(_));
    let (job, result_ref) = match &child {
        ChildCmd::Launched(l) => (l.id.clone(), Some(l.result_ref.clone())),
        ChildCmd::Local(_) => (String::new(), None),
    };
    let mut cmd = match &child {
        ChildCmd::Local(exe) => {
            let mut cmd = tokio::process::Command::new(exe);
            cmd.arg("--headless");
            cmd
        }
        ChildCmd::Launched(l) => {
            let Some((program, args)) = l.argv.split_first() else {
                return Outcome::failed("cannot start subagent: the launcher is empty");
            };
            let mut cmd = tokio::process::Command::new(program);
            cmd.args(args)
                .env("MU_TASK_NAME", &l.id)
                .env("MU_TASK_ROOT", &l.root)
                .env("MU_TASK_CWD", cwd)
                .env("MU_TASK_REF", &l.task_ref)
                .env("MU_TASK_RESULT_REF", &l.result_ref)
                .env("MU_TASK_SESSION", &l.session)
                .env("MU_TASK_PARENT_PID", std::process::id().to_string())
                .env("MU_TASK_MU_HOME", mu_common::paths::mu_home())
                .env("MU_TASK_PROFILE_HOME", mu_common::paths::profile_home())
                .env("MU_TASK_AGENT_DIR", mu_common::paths::omp_agent_dir());
            cmd
        }
    };
    cmd.current_dir(cwd)
        .env("MU_TASK_DEPTH", (depth() + 1).to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // Only a launcher reports changes, on a pipe the worker's stdout has no way into.
    let mut changes_in = None;
    if launched {
        match changes_pipe(&mut cmd) {
            Ok((read, write)) => changes_in = Some((read, write)),
            Err(e) => return Outcome::failed(format!("cannot start subagent: {e}")),
        }
    }
    let mut child = match mu_base::process::spawn_grouped(&mut cmd, true) {
        Ok(c) => c,
        Err(e) => return Outcome::failed(format!("cannot start subagent: {e}")),
    };
    // The write end is the child's alone from here.
    let mut changes_in = changes_in.map(|(read, _write)| read);
    let pid = child.id().unwrap_or(0) as i32;
    let job = if job.is_empty() { format!("pid-{pid}") } else { job };
    let mut stdin = child.stdin.take().expect("stdin is piped");
    // The task payload, then one line per approval answer, for as long as the child reads them.
    let (wtx, mut wrx) = tokio::sync::mpsc::unbounded_channel::<String>();
    // What the parent put in the payload is what the child may call; a payload without it, none.
    let allow: RelayAllow =
        payload.get("relayModels").and_then(|v| serde_json::from_value(v.clone()).ok()).unwrap_or_default();
    let relay = relay.map(|r| r.start(wtx.clone(), &job, allow));
    let task = payload.to_string();
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
    let stderr = child.stderr.take().expect("stderr is piped");
    let err_task = tokio::task::spawn_local(async move {
        let mut tail: std::collections::VecDeque<String> = Default::default();
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(l)) = lines.next_line().await {
            if tail.len() == 20 {
                tail.pop_front();
            }
            tail.push_back(l);
        }
        tail.into_iter().collect::<Vec<_>>().join("\n")
    });
    let mut stdout = BufReader::new(child.stdout.take().expect("stdout is piped"));
    let mut result: Option<ChildResult> = None;
    let mut cancelled = false;
    let mut bad_line: Option<String> = None;
    let mut mirrors = Mirrors::default();
    let mut notices: Vec<String> = Vec::new();
    let mut bad_entry = false;
    let parent_session = payload["parentSession"].as_str();
    let mut line: Vec<u8> = Vec::new();
    loop {
        line.clear();
        tokio::select! {
            n = read_line_capped(&mut stdout, &mut line, mu_common::rpc::MAX_LINE) => match n {
                Ok(None) => break,
                Ok(Some(true)) => {
                    bad_line.get_or_insert_with(|| {
                        format!("a line longer than {} MiB was dropped", mu_common::rpc::MAX_LINE >> 20)
                    });
                }
                Ok(Some(false)) => {
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
                                        let (created, notice) = mirrors.write(cwd, &file, &line, parent_session);
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
                            let rx = match approve {
                                Some(f) => f(request),
                                None => {
                                    let (tx, rx) = oneshot::channel();
                                    let _ = tx.send(ApprovalDecision::Unavailable);
                                    rx
                                }
                            };
                            let out = wtx.clone();
                            tokio::task::spawn_local(async move {
                                let d = rx.await.unwrap_or(ApprovalDecision::Deny { note: None });
                                let _ = out.send(json!({"type": "approval", "id": id, "decision": d}).to_string());
                            });
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
                        Ok(ChildEvent::Changes(_)) => {}
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
                    if pid > 0 {
                        tokio::task::spawn_local(mu_base::process::terminate_group(pid));
                    }
                    break;
                }
            },
            _ = cancel.cancelled(), if !cancelled => {
                cancelled = true;
                if pid > 0 {
                    tokio::task::spawn_local(mu_base::process::terminate_group(pid));
                }
            }
        }
    }
    let capped = relay.as_ref().and_then(|r| r.capped());
    drop(relay);
    drop(wtx);
    // A child that ignores everything must not hang the parent: bound the wait, then kill.
    if tokio::time::timeout(std::time::Duration::from_secs(5), child.wait()).await.is_err() {
        mu_base::process::kill_group(pid, libc::SIGKILL);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(1), child.wait()).await;
    }
    // A descendant that inherited stderr can hold it open after the child is gone.
    let err_abort = err_task.abort_handle();
    let stderr_tail = match tokio::time::timeout(std::time::Duration::from_secs(2), err_task).await {
        Ok(tail) => tail.unwrap_or_default(),
        Err(_) => {
            err_abort.abort();
            String::new()
        }
    };
    drop(mirrors);
    let changes = changes_in.as_mut().zip(result_ref).and_then(|(pipe, r)| read_changes(pipe, &r));
    let mut out = outcome(result, payload, cancelled, stderr_tail, bad_line);
    for n in notices {
        out.text.push('\n');
        out.text.push_str(&n);
    }
    if let Some(cap) = capped {
        out.text.push_str(&format!("\n[task spend cap of ${cap:.2} reached; later model calls were refused]"));
    }
    if let Some((r#ref, base, files)) = changes {
        out.text.push_str(&format!(
            "\n[changes: {files} files at {}, base {}; see git diff {base} {}]",
            r#ref,
            short_sha(&base),
            r#ref
        ));
    }
    out
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
                    Outcome { ok: true, text: format!("```json\n{pretty}\n```"), data: Some(d) }
                }
                // Asked for typed data and none came back: never pass the free text off as the result.
                None if ok && payload.get("outputSchema").is_some_and(|s| !s.is_null()) => {
                    Outcome::failed(format!("subagent returned no schema-validated data\n{text}"))
                }
                _ => Outcome { ok, text, data: None },
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
    use mu_testutil::{pid_in, repo, sh, tmp, wait_gone};
    use std::os::unix::fs::PermissionsExt;
    use tokio_util::sync::CancellationToken;

    fn script(dir: &Path, body: &str) -> String {
        let path = dir.join("launcher");
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path.display().to_string()
    }

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

    const FAKE: &str = r#"#!/bin/sh
IFS= read -r line
echo '{"type":"session","path":"/s/worker.jsonl"}'
echo '{"type":"tool","name":"read","summary":"x"}'
snap=$(git -C "$MU_TASK_ROOT" show "$MU_TASK_REF:new.txt")
printf '{"type":"result","text":"args=%s name=%s root=%s cwd=%s ref=%s result=%s session=%s ppid=%s depth=%s snap=%s home=%s"}\n' \
  "$#" "$MU_TASK_NAME" "$MU_TASK_ROOT" "$MU_TASK_CWD" "$MU_TASK_REF" "$MU_TASK_RESULT_REF" "$MU_TASK_SESSION" \
  "$MU_TASK_PARENT_PID" "$MU_TASK_DEPTH" "$snap" "$(test -n "$MU_TASK_MU_HOME$MU_TASK_PROFILE_HOME$MU_TASK_AGENT_DIR" && echo set)"
echo '{"type":"changes","ref":"refs/mu/result/forged","base":"1111111111111111111111111111111111111111","files":99}'
printf '{"type":"changes","ref":"%s","base":"0123456789abcdef0123456789abcdef01234567","files":2}\n' "$MU_TASK_RESULT_REF" >&3
"#;

    #[tokio::test(flavor = "current_thread")]
    async fn a_launcher_gets_the_environment_and_a_snapshot_commit_and_reports_changes() {
        let root = repo();
        std::fs::write(root.join("new.txt"), "untracked\n").unwrap();
        let bin = tmp();
        let launcher = vec![script(&bin, FAKE)];
        let sessions = std::cell::RefCell::new(Vec::new());
        let tools = std::cell::Cell::new(0);
        let out = tokio::task::LocalSet::new()
            .run_until(async {
                let launch = prepare_launch(&launcher, &root, "Review Auth!", "sess-1").await.unwrap();
                assert!(task_refs(&root).contains(&launch.task_ref), "{}", task_refs(&root));
                run_child_observed(
                    ChildCmd::Launched(launch),
                    &root,
                    &json!({"task": "go"}),
                    &CancellationToken::new(),
                    |_, _| tools.set(tools.get() + 1),
                    |p| sessions.borrow_mut().push(p.to_string()),
                    |_| {},
                    None,
                    Mirror::Off,
                    None,
                )
                .await
            })
            .await;
        assert!(out.ok, "{}", out.text);
        let first = out.text.lines().next().unwrap();
        let id = first.split(" name=").nth(1).unwrap().split(' ').next().unwrap();
        assert!(id.starts_with("review-auth-"), "{first}");
        for want in [
            "args=0 ".to_string(),
            format!("root={} ", root.display()),
            format!("cwd={} ", root.display()),
            format!("ref=refs/mu/task/{id} "),
            format!("result=refs/mu/result/{id} "),
            "session=sess-1 ".to_string(),
            format!("ppid={} ", std::process::id()),
            "depth=".to_string(),
            "snap=untracked ".to_string(),
            "home=set".to_string(),
        ] {
            assert!(first.contains(&want), "{want} not in {first}");
        }
        assert_eq!(
            out.text.lines().last().unwrap(),
            format!(
                "[changes: 2 files at refs/mu/result/{id}, base 0123456; see git diff 0123456789abcdef0123456789abcdef01234567 refs/mu/result/{id}]"
            ),
            "the report came on fd 3; the forged one on stdout was ignored"
        );
        assert!(!out.text.contains("forged") && !out.text.contains("99 files"), "{}", out.text);
        assert!(sessions.borrow().is_empty(), "a launched child's session is on the worker");
        assert_eq!(tools.get(), 1);
        assert_eq!(task_refs(&root), "", "the task ref is gone after the run");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn the_task_ref_is_gone_when_the_launcher_cannot_start() {
        let root = repo();
        let launcher = vec!["/nonexistent/mu-launcher".to_string()];
        let out = tokio::task::LocalSet::new()
            .run_until(async {
                let launch = prepare_launch(&launcher, &root, "t", "").await.unwrap();
                assert_ne!(task_refs(&root), "");
                run_child_observed(
                    ChildCmd::Launched(launch),
                    &root,
                    &json!({}),
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
        assert!(!out.ok && out.text.contains("cannot start subagent"), "{}", out.text);
        assert_eq!(task_refs(&root), "");
    }

    /// Runs a launcher script as one launched task.
    async fn launched(body: &str) -> Outcome {
        let root = repo();
        let bin = tmp();
        let launcher = vec![script(&bin, body)];
        tokio::task::LocalSet::new()
            .run_until(async {
                let launch = prepare_launch(&launcher, &root, "t", "").await.unwrap();
                run_child_observed(
                    ChildCmd::Launched(launch),
                    &root,
                    &json!({}),
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
            .await
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_changes_line_on_stdout_alone_is_ignored() {
        let out = launched(
            "#!/bin/sh\nIFS= read -r line\necho '{\"type\":\"result\",\"text\":\"done\"}'\n\
             printf '{\"type\":\"changes\",\"ref\":\"%s\",\"base\":\"0123456789abcdef0123456789abcdef01234567\",\"files\":1}\\n' \"$MU_TASK_RESULT_REF\"\n",
        )
        .await;
        assert!(out.ok && out.text == "done", "{}", out.text);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_changes_report_must_name_the_launchs_ref_and_a_commit_hash() {
        let report = |r: &str, base: &str| {
            format!(
                "#!/bin/sh\nIFS= read -r line\necho '{{\"type\":\"result\",\"text\":\"done\"}}'\n\
                 printf '{{\"type\":\"changes\",\"ref\":\"%s\",\"base\":\"{base}\",\"files\":1}}\\n' \"{r}\" >&3\n"
            )
        };
        let sha = "0123456789abcdef0123456789abcdef01234567";
        for (r, base) in
            [("refs/mu/result/other", sha), ("$MU_TASK_RESULT_REF", "not-a-sha"), ("$MU_TASK_RESULT_REF", "abc")]
        {
            let out = launched(&report(r, base)).await;
            assert!(out.ok && out.text == "done", "{r} {base}: {}", out.text);
        }
        let out = launched(&report("$MU_TASK_RESULT_REF", sha)).await;
        assert!(out.text.contains("[changes: 1 files at refs/mu/result/t-"), "{}", out.text);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_descendant_holding_the_changes_pipe_does_not_hold_the_result() {
        let started = std::time::Instant::now();
        let out = launched(
            "#!/bin/sh\nIFS= read -r line\nsleep 20 >/dev/null 2>&1 &\necho '{\"type\":\"result\",\"text\":\"done\"}'\n",
        )
        .await;
        assert!(out.ok && out.text == "done", "{}", out.text);
        assert!(started.elapsed() < std::time::Duration::from_secs(10), "{:?}", started.elapsed());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_launch_needs_a_git_repository() {
        let err = prepare_launch(&["x".to_string()], &tmp(), "t", "").await.err().unwrap();
        assert!(err.contains("needs a git repository"), "{err}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_repository_with_no_commit_gets_a_parentless_snapshot() {
        let root = tmp();
        sh(&root, &["init", "-q"]);
        std::fs::write(root.join("f"), "x\n").unwrap();
        let launch = prepare_launch(&["x".to_string()], &root, "t", "").await.unwrap();
        let parents = sh(&root, &["rev-list", "--parents", "-n1", &launch.task_ref]);
        assert_eq!(parents.split_whitespace().count(), 1, "{parents}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancel_ends_the_launchers_process_group() {
        let root = repo();
        let bin = tmp();
        let (p1, p2) = (bin.join("launcher.pid"), bin.join("sleep.pid"));
        let body = format!(
            "#!/bin/sh\nIFS= read -r line\necho $$ > {}\nsleep 60 &\necho $! > {}\nwait\n",
            p1.display(),
            p2.display()
        );
        let launcher = vec![script(&bin, &body)];
        let cancel = CancellationToken::new();
        let out = tokio::task::LocalSet::new()
            .run_until(async {
                let launch = prepare_launch(&launcher, &root, "t", "").await.unwrap();
                let run = tokio::task::spawn_local({
                    let cancel = cancel.clone();
                    async move {
                        run_child_observed(
                            ChildCmd::Launched(launch),
                            &root,
                            &json!({}),
                            &cancel,
                            |_, _| {},
                            |_| {},
                            |_| {},
                            None,
                            Mirror::Off,
                            None,
                        )
                        .await
                    }
                });
                let (a, b) = (pid_in(&p1).await, pid_in(&p2).await);
                cancel.cancel();
                let out = run.await.unwrap();
                wait_gone(a).await;
                wait_gone(b).await;
                out
            })
            .await;
        assert!(!out.ok && out.text == "cancelled", "{}", out.text);
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
