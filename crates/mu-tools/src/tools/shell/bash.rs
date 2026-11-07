use crate::jobs::JobsHandle;
use crate::tools::spill::{Collected, Collector};
use crate::tools::subagent::BackgroundResult;
use crate::tools::{ToolCtx, Tools};
use mu_base::events::AgentEvent;
use mu_base::tool::{BashMeta, ToolMeta, ToolOutput};
use pi_shell::{
    cancel::{AbortReason, CancelToken},
    shell::ShellRunOptions,
};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::rc::Rc;
use std::time::Instant;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio_util::sync::CancellationToken;

#[derive(Deserialize)]
pub struct Args {
    command: String,
    timeout: Option<f64>,
    cwd: Option<String>,
    #[serde(default)]
    pty: bool,
    /// Run as a background job (interactive sessions; not with `pty`).
    #[serde(default)]
    r#async: bool,
}

/// The command's timeout, parsed once for both execution paths.
struct Limits {
    /// Seconds as requested (default 300; 0 disables).
    secs: f64,
    /// `None` when disabled; otherwise clamped to 1 s – 1 h.
    timeout: Option<std::time::Duration>,
}

impl Limits {
    fn new(requested: Option<f64>) -> Self {
        let secs = requested.unwrap_or(300.0);
        let timeout =
            (secs != 0.0).then(|| std::time::Duration::from_millis((secs.clamp(1.0, 3600.0) * 1000.0) as u64));
        Self { secs, timeout }
    }

    fn millis(&self) -> Option<u32> {
        self.timeout.map(|d| d.as_millis() as u32)
    }
}

/// A foreground command in a pty: the child, its terminal side, the output so far and the
/// termination state. Termination sends SIGTERM and a hangup, SIGKILL after `KILL_AFTER`, and gives
/// the run up `GIVE_UP_AFTER` later.
struct PtyRun {
    child: tokio::process::Child,
    io: std::rc::Rc<mu_base::pty::PtyIo>,
    input: tokio::sync::mpsc::UnboundedReceiver<mu_base::events::PtyInput>,
    input_open: bool,
    /// Keystrokes waiting for the pty to accept them; writing never blocks reads, cancel or timeout.
    pending_in: std::collections::VecDeque<u8>,
    write_buf: Vec<u8>,
    decoder: pi_shell::output_decode::OutputDecoder,
    cleaner: mu_base::pty::Cleaner,
    collector: Collector,
    buf: Vec<u8>,
    eof: bool,
    status: Option<std::process::ExitStatus>,
    cancelled: bool,
    timed_out: bool,
    term_at: Option<tokio::time::Instant>,
    killed: bool,
}

/// Offers the pty at most 4 KB of the queued keystrokes; the queue itself is unbounded.
async fn write_some(
    io: &mu_base::pty::PtyIo,
    pending: &std::collections::VecDeque<u8>,
    buf: &mut Vec<u8>,
) -> std::io::Result<usize> {
    buf.clear();
    buf.extend(pending.iter().take(4096));
    io.write_some(buf).await
}

impl PtyRun {
    /// Appends raw output: the UI gets it raw, the model gets it cleaned (and spilled when long).
    fn emit(&mut self, raw: String, ctx: &ToolCtx, tools: &Tools) {
        if !raw.is_empty() {
            let _ = ctx.events.send(AgentEvent::ToolOutput { id: ctx.call_id.clone(), chunk: raw.clone() });
            let clean = self.cleaner.push(&raw);
            self.collector.push(&clean, &|| tools.next_artifact());
        }
    }

    /// Runs the command until it exits, or until it is cancelled or times out and has been terminated.
    async fn pump(&mut self, tools: &Tools, ctx: &ToolCtx, timeout: Option<std::time::Duration>) {
        let timeout_sleep = tokio::time::sleep(timeout.unwrap_or(std::time::Duration::from_secs(1)));
        tokio::pin!(timeout_sleep);
        loop {
            let deadline = self.deadline();
            let esc_sleep = async {
                match deadline {
                    Some(d) => tokio::time::sleep_until(d).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                r = self.io.read(&mut self.buf), if !self.eof => match r {
                    Ok(0) | Err(_) => self.eof = true,
                    Ok(n) => {
                        let raw = self.decoder.push(&self.buf[..n]);
                        self.emit(raw, ctx, tools);
                    }
                },
                w = write_some(&self.io, &self.pending_in, &mut self.write_buf), if !self.pending_in.is_empty() && self.term_at.is_none() => match w {
                    Ok(n) => { self.pending_in.drain(..n); }
                    Err(_) => self.pending_in.clear(),
                },
                m = self.input.recv(), if self.input_open => match m {
                    Some(mu_base::events::PtyInput::Bytes(b)) => self.pending_in.extend(b),
                    Some(mu_base::events::PtyInput::Resize { cols, rows }) => self.io.resize(cols, rows),
                    Some(mu_base::events::PtyInput::Kill) => {
                        if self.term() {
                            self.cancelled = true;
                        }
                    }
                    None => self.input_open = false,
                },
                _ = ctx.cancel.cancelled(), if self.term_at.is_none() => {
                    self.cancelled = true;
                    self.term();
                }
                _ = &mut timeout_sleep, if timeout.is_some() && self.term_at.is_none() => {
                    self.timed_out = true;
                    self.term();
                }
                _ = esc_sleep => {
                    if self.escalate() {
                        break;
                    }
                }
                st = self.child.wait() => {
                    self.status = st.ok();
                    break;
                }
            }
        }
        // Drain: until EOF, or 300 ms without data (a background descendant may hold the slave open).
        while !self.eof {
            match tokio::time::timeout(std::time::Duration::from_millis(300), self.io.read(&mut self.buf)).await {
                Ok(Ok(n)) if n > 0 => {
                    let raw = self.decoder.push(&self.buf[..n]);
                    self.emit(raw, ctx, tools);
                }
                _ => break,
            }
        }
    }
}

const KILL_AFTER: std::time::Duration = std::time::Duration::from_millis(200);
const GIVE_UP_AFTER: std::time::Duration = std::time::Duration::from_millis(800);

impl PtyRun {
    /// Starts termination; false when it had already started.
    fn term(&mut self) -> bool {
        if self.term_at.is_some() {
            return false;
        }
        self.term_at = Some(tokio::time::Instant::now());
        self.io.signal(libc::SIGTERM);
        self.io.hangup();
        true
    }

    /// When the loop must wake for the next escalation step.
    fn deadline(&self) -> Option<tokio::time::Instant> {
        let kill_at = self.term_at.map(|t| t + KILL_AFTER);
        if self.killed { kill_at.map(|t| t + GIVE_UP_AFTER) } else { kill_at }
    }

    /// Takes the escalation step that is due; true when the run is to be given up.
    fn escalate(&mut self) -> bool {
        if self.killed {
            return true;
        }
        self.killed = true;
        self.io.signal(libc::SIGKILL);
        false
    }
}

impl Tools {
    pub async fn bash(&self, a: Args, ctx: &ToolCtx) -> ToolOutput {
        if let Some(jobs) = &self.jobs
            && a.r#async
            && !a.pty
        {
            return self.bash_background(a, jobs).await;
        }
        let fallback = a.pty;
        if a.pty
            && let Some(out) = self.bash_pty(&a, ctx).await
        {
            return out;
        }
        self.baseline_shell_env().await;
        let mut out = self.bash_plain(a, ctx).await;
        if let Some(note) = self.after_foreground().await {
            out.text.push_str("\n\n");
            out.text.push_str(&note);
        }
        if fallback {
            out.text.push_str("\n\n[pty unavailable without the terminal UI; ran without a terminal]");
        }
        out
    }

    /// `async`: the command runs as a tracked job; its result arrives as a system notice. The job
    /// starts from the persistent shell's cwd and exported environment as of launch.
    async fn bash_background(&self, a: Args, jobs: &Rc<JobsHandle>) -> ToolOutput {
        if let Err(e) = jobs.check_capacity(1) {
            return ToolOutput::err(e);
        }
        let snapshot = self.shell_snapshot().await;
        let inherited = snapshot.is_some();
        let (base_cwd, vars) = snapshot.unwrap_or_else(|| (self.cwd.clone(), process_env()));
        let cwd = a.cwd.as_ref().map(|c| self.abs(c)).unwrap_or(base_cwd);
        let env = job_env(vars, &cwd, self.session_env());
        let artifact_dir = self.artifact_dir();
        let (command, timeout) = (a.command.clone(), a.timeout);
        let job_cwd = cwd.clone();
        let id = jobs.spawn_bash(a.command, job_cwd, move |cancel, sink| {
            run_background(
                command,
                cwd,
                env,
                timeout,
                cancel,
                move || Tools::next_artifact_in(&artifact_dir),
                move |s| sink.push(s),
            )
        });
        let note = if inherited {
            String::new()
        } else {
            "\nShell state from earlier calls was not inherited (the shell was busy or did not answer in time): the job uses mu's own environment.".to_string()
        };
        ToolOutput::ok(format!(
            "Started background job {id}.{note}\nWhen it finishes, mu automatically starts your next turn with its result; no user input is needed. Until then, do other work that does not depend on it. When your next step needs its result, end your turn now with a short status and no tool call."
        ))
    }

    /// The persistent shell's cwd and exported variables, from one state-neutral command. `None`
    /// when the shell does not answer within `SNAPSHOT_TIMEOUT` (a foreground command holds it) or
    /// the answer does not parse.
    async fn shell_snapshot(&self) -> Option<(PathBuf, Vec<(String, String)>)> {
        let ms = SNAPSHOT_TIMEOUT.as_millis() as u32;
        let (tx, rx) = flume::unbounded::<String>();
        let opts = ShellRunOptions { command: SNAPSHOT_COMMAND.into(), timeout_ms: Some(ms), ..Default::default() };
        let mut run = self.shell().spawn(opts, Some(tx), CancelToken::new(Some(ms)));
        let res = tokio::time::timeout(SNAPSHOT_TIMEOUT, run.join()).await.ok()?.ok()?;
        if res.cancelled || res.timed_out || res.exit_code != Some(0) {
            return None;
        }
        parse_snapshot(&rx.drain().collect::<String>())
    }

    /// Before the first foreground command: what the shell starts with, so later changes are the command's.
    async fn baseline_shell_env(&self) {
        if self.shell_env.borrow().is_some() {
            return;
        }
        if let Some((cwd, vars)) = self.shell_snapshot().await {
            *self.shell_cwd.borrow_mut() = Some(cwd);
            *self.shell_env.borrow_mut() = Some(vars.into_iter().collect());
        }
    }

    /// After a foreground command: records the shell's directory (for the file tools' relative-path note) and
    /// returns a reminder when the command set or changed an exported variable, which then stays set for every
    /// later call and for anything those start. Names only, never values.
    async fn after_foreground(&self) -> Option<String> {
        let (cwd, vars) = self.shell_snapshot().await?;
        *self.shell_cwd.borrow_mut() = Some(cwd);
        let session: Vec<&str> = self.session_env().iter().map(|(k, _)| *k).collect();
        let now: std::collections::BTreeMap<String, String> = vars.into_iter().collect();
        let before = self.shell_env.borrow_mut().replace(now.clone())?;
        let changed = env_changes(&before, &now, &session);
        if changed.is_empty() {
            return None;
        }
        Some(format!(
            "[mu: this command set or changed {} in the persistent shell. Exports outlive the call: every later bash \
             command, and anything it starts (install scripts, builds), sees them. Unset {} when done, or pass values \
             for one command with `env NAME=value cmd`.]",
            changed.iter().map(|n| format!("`{n}`")).collect::<Vec<_>>().join(", "),
            if changed.len() == 1 { "it" } else { "them" }
        ))
    }

    async fn bash_pty(&self, a: &Args, ctx: &ToolCtx) -> Option<ToolOutput> {
        use mu_base::events::PtyInput;
        use tokio::sync::{mpsc, oneshot};

        let (input, input_rx) = mpsc::unbounded_channel::<PtyInput>();
        let (reply, rx) = oneshot::channel();
        ctx.events.send(AgentEvent::PtyOpen { id: ctx.call_id.clone(), input, reply }).ok()?;
        let (cols, rows) = rx.await.ok()?;
        let cwd = a.cwd.as_ref().map(|c| self.abs(c)).unwrap_or_else(|| self.cwd.clone());
        let limits = Limits::new(a.timeout);
        let secs = limits.secs;
        let pty = match mu_base::pty::spawn(&a.command, &cwd, cols, rows, &self.session_env()) {
            Ok(p) => p,
            Err(e) => return Some(ToolOutput::err(format!("PTY error: {e}"))),
        };
        let mut run = PtyRun {
            child: pty.child,
            io: pty.io.clone(),
            input: input_rx,
            input_open: true,
            pending_in: Default::default(),
            write_buf: Vec::new(),
            decoder: pi_shell::output_decode::OutputDecoder::new(),
            cleaner: mu_base::pty::Cleaner::default(),
            collector: Collector::new(cwd),
            buf: vec![0u8; 64 * 1024],
            eof: false,
            status: None,
            cancelled: false,
            timed_out: false,
            term_at: None,
            killed: false,
        };
        let start = Instant::now();
        run.pump(self, ctx, limits.timeout).await;
        // The command is over. Anything it left running in the background goes with it; the
        // group is gone once the leader is reaped only if nothing else is in it.
        mu_base::process::terminate_group(run.io.pid()).await;
        let raw = run.decoder.finish();
        run.emit(raw, ctx, self);
        let tail = run.cleaner.finish();
        run.collector.push(&tail, &|| self.next_artifact());
        let wall = start.elapsed().as_secs_f64();
        let end = RunEnd {
            wall,
            secs,
            cancelled: run.cancelled,
            timed_out: run.timed_out,
            exit_code: exit_code(run.status),
            quiet_wall: false,
        };
        Some(finish(run.collector.finish(), end))
    }

    async fn bash_plain(&self, a: Args, ctx: &ToolCtx) -> ToolOutput {
        let run_cwd = a.cwd.as_ref().map(|c| self.abs(c)).unwrap_or_else(|| self.cwd.clone());
        let limits = Limits::new(a.timeout);
        let (secs, timeout_ms) = (limits.secs, limits.millis());
        let mut cancel = CancelToken::new(timeout_ms);
        let abort = cancel.emplace_abort_token();
        let aborted = std::cell::Cell::new(false);
        let (tx, rx) = flume::unbounded::<String>();
        let opts = ShellRunOptions {
            command: a.command,
            cwd: a.cwd.map(|c| self.abs(&c).to_string_lossy().into_owned()),
            timeout_ms,
            env: Some(self.session_env().into_iter().map(|(k, v)| (k.to_string(), v)).collect()),
            ..Default::default()
        };
        let start = Instant::now();
        let mut shell_run = self.shell().spawn(opts, Some(tx), cancel);
        let run = shell_run.finish(
            ctx.cancel.cancelled(),
            || {
                aborted.set(true);
                abort.abort(AbortReason::Signal);
            },
            limits.timeout,
        );
        tokio::pin!(run);
        let mut c = Collector::new(run_cwd.clone());
        // The live tail shows cleaned text; only the pty view gets raw bytes.
        let mut cleaner = mu_base::pty::Cleaner::default();
        let mut feed = |s: String| {
            let clean = cleaner.push(&s);
            if !clean.is_empty() {
                let _ = ctx.events.send(AgentEvent::ToolOutput { id: ctx.call_id.clone(), chunk: clean });
            }
            c.push(&s, &|| self.next_artifact());
        };
        // Not "until the channel closes": a hung shell keeps its end open for good.
        let res = loop {
            tokio::select! {
                r = &mut run => break r,
                Ok(s) = rx.recv_async() => feed(s),
            }
        };
        rx.drain().for_each(&mut feed);
        let rest = cleaner.finish();
        if !rest.is_empty() {
            let _ = ctx.events.send(AgentEvent::ToolOutput { id: ctx.call_id.clone(), chunk: rest });
        }
        let output = c.finish();
        let res = match res {
            Ok(r) => r,
            Err(_) if aborted.get() => {
                let end = RunEnd {
                    wall: start.elapsed().as_secs_f64(),
                    secs,
                    cancelled: true,
                    timed_out: false,
                    exit_code: None,
                    quiet_wall: true,
                };
                return finish(output, end);
            }
            Err(e) => {
                let end = output.body.len();
                let mut out = ToolOutput::err(
                    format!("{}{}\n\nShell error: {e}", output.body, output.notes).trim_start().to_string(),
                );
                out.meta = Some(ToolMeta::Bash(BashMeta {
                    output_end: end.min(out.text.len()),
                    artifact: output.artifact,
                    spill_note: output.spill_note,
                    ..Default::default()
                }));
                return out;
            }
        };
        let wall = start.elapsed().as_secs_f64();
        let end = RunEnd {
            wall,
            secs,
            cancelled: res.cancelled,
            timed_out: res.timed_out,
            exit_code: res.exit_code,
            quiet_wall: false,
        };
        finish(output, end)
    }
}

/// How long a background job waits for the persistent shell's snapshot before starting without it.
const SNAPSHOT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Prints the cwd, then each exported name and value as NUL-terminated fields. Values hold anything
/// but NUL, so the fields split exactly; `export -p` does not quote values and cannot be split.
const SNAPSHOT_COMMAND: &str =
    r#"printf '%s\0' "$(pwd)"; for n in $(compgen -e); do printf '%s\0%s\0' "$n" "${!n}"; done"#;

/// Not inherited by a job: shell bookkeeping, and PWD, which is set from the job's cwd.
const NOT_INHERITED: [&str; 4] = ["_", "SHLVL", "OLDPWD", "PWD"];

/// Parses `SNAPSHOT_COMMAND`'s output: the cwd, then name/value pairs.
fn parse_snapshot(out: &str) -> Option<(PathBuf, Vec<(String, String)>)> {
    let fields: Vec<&str> = out.strip_suffix('\0')?.split('\0').collect();
    let (&cwd, pairs) = fields.split_first()?;
    if !Path::new(cwd).is_absolute() || pairs.len() % 2 != 0 {
        return None;
    }
    let mut env = Vec::with_capacity(pairs.len() / 2);
    for kv in pairs.chunks(2) {
        if kv[0].is_empty() || kv[0].contains('=') {
            return None;
        }
        env.push((kv[0].to_string(), kv[1].to_string()));
    }
    Some((PathBuf::from(cwd), env))
}

/// Names of variables added or changed from `before` to `after`, without shell bookkeeping and mu's session variables.
fn env_changes(
    before: &std::collections::BTreeMap<String, String>,
    after: &std::collections::BTreeMap<String, String>,
    session: &[&str],
) -> Vec<String> {
    after
        .iter()
        .filter(|(k, v)| {
            !NOT_INHERITED.contains(&k.as_str()) && !session.contains(&k.as_str()) && before.get(*k) != Some(*v)
        })
        .map(|(k, _)| k.clone())
        .collect()
}

/// mu's own environment, for a job whose shell state could not be read.
fn process_env() -> Vec<(String, String)> {
    std::env::vars_os().map(|(k, v)| (k.to_string_lossy().into_owned(), v.to_string_lossy().into_owned())).collect()
}

/// The environment of a job: `vars` without what is not inherited, PWD set to `cwd`, then mu's
/// session variables on top.
fn job_env(vars: Vec<(String, String)>, cwd: &Path, session: Vec<(&'static str, String)>) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = vars
        .into_iter()
        .filter(|(k, _)| !NOT_INHERITED.contains(&k.as_str()) && !session.iter().any(|(s, _)| *s == k.as_str()))
        .collect();
    env.push(("PWD".into(), cwd.to_string_lossy().into_owned()));
    env.extend(session.into_iter().map(|(k, v)| (k.to_string(), v)));
    env
}

/// `bash` on mu's `PATH`, the dialect of the foreground shell; `/bin/sh` when there is none.
fn job_shell() -> PathBuf {
    mu_base::pty::bash_on_path().unwrap_or_else(|| "/bin/sh".into())
}

/// How a run ended: what `finish` words into the result.
struct RunEnd {
    wall: f64,
    /// The timeout as requested, for the timed-out note.
    secs: f64,
    cancelled: bool,
    timed_out: bool,
    exit_code: Option<i32>,
    /// A cancelled-by-user run reports no wall time.
    quiet_wall: bool,
}

/// A finished command's exit code: a death by signal is 128 + the signal, as in a shell.
fn exit_code(status: Option<std::process::ExitStatus>) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    status.and_then(|s| s.code().or_else(|| s.signal().map(|g| 128 + g)))
}

/// Builds the result: the same text the model has always seen, plus `meta` carrying the facts the
/// UI shows (exit code, wall time, flags, artifact), so it never has to parse them back out.
fn finish(c: Collected, end: RunEnd) -> ToolOutput {
    let RunEnd { wall, secs, cancelled, timed_out, exit_code, quiet_wall } = end;
    let had_output = !c.body.is_empty();
    let mut text = if had_output { c.body } else { "(no output)".to_string() };
    let output_end = if had_output { text.len() } else { 0 };
    text.push_str(&c.notes);
    if quiet_wall && !had_output {
        text.clear(); // a user-cancelled run that printed nothing says only that it was cancelled
    }
    let wall_shown = !quiet_wall && wall >= 1.0;
    if wall_shown {
        text.push_str(&format!("\n\nWall time: {wall:.1} seconds"));
    }
    let exit_code = exit_code.filter(|c| *c != 0);
    if cancelled {
        text.push_str(if text.is_empty() { "[Command cancelled]" } else { "\n\n[Command cancelled]" });
    } else if timed_out {
        text.push_str(&format!("\n\n[Command timed out after {} seconds]", secs.clamp(1.0, 3600.0)));
    } else if let Some(code) = exit_code {
        text.push_str(&format!("\n\nCommand exited with code {code}"));
    }
    let mut out = if cancelled || timed_out { ToolOutput::err(text) } else { ToolOutput::ok(text) };
    out.meta = Some(ToolMeta::Bash(BashMeta {
        output_end,
        exit_code: if cancelled || timed_out { None } else { exit_code },
        wall_secs: wall_shown.then_some(wall),
        timed_out: timed_out && !cancelled,
        cancelled,
        artifact: c.artifact,
        spill_note: c.spill_note,
    }));
    out
}

/// Quiet time after the command exits before its pipes are given up: a background descendant
/// may hold them open.
const IDLE: std::time::Duration = std::time::Duration::from_millis(300);

/// Reads from a pipe, or never completes once the pipe is closed (`None`).
async fn read_or_pending<R: AsyncRead + Unpin>(r: &mut Option<R>, buf: &mut [u8]) -> std::io::Result<usize> {
    match r {
        Some(r) => r.read(buf).await,
        None => std::future::pending().await,
    }
}

/// Runs `command` for a background job: `bash -c` in its own process group, with `env` as its whole
/// environment and the foreground run's output limits and timeout. Cancel, timeout and the end of
/// the command all kill the group, so nothing the command started outlives the job.
pub async fn run_background(
    command: String,
    cwd: PathBuf,
    env: Vec<(String, String)>,
    timeout: Option<f64>,
    cancel: CancellationToken,
    alloc: impl Fn() -> (u32, PathBuf),
    on_output: impl Fn(&str),
) -> BackgroundResult {
    let limits = Limits::new(timeout);
    let start = Instant::now();
    let mut cmd = tokio::process::Command::new(job_shell());
    cmd.arg("-c")
        .arg(&command)
        .current_dir(&cwd)
        .env_clear()
        .envs(env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = match mu_base::process::spawn_grouped(&mut cmd, false) {
        Ok(c) => c,
        Err(e) => {
            return BackgroundResult {
                ok: false,
                text: format!("cannot start command: {e}"),
                note: "did not start".into(),
            };
        }
    };
    let pid = child.id().unwrap_or(0) as i32;
    let (mut stdout, mut stderr) = (child.stdout.take(), child.stderr.take());
    let (mut out_dec, mut err_dec) =
        (pi_shell::output_decode::OutputDecoder::new(), pi_shell::output_decode::OutputDecoder::new());
    let (mut obuf, mut ebuf) = (vec![0u8; 8192], vec![0u8; 8192]);
    let mut collector = Collector::new(cwd.clone());
    let mut status: Option<std::process::ExitStatus> = None;
    let (mut cancelled, mut timed_out, mut stopping) = (false, false, false);
    // Terminating takes `TERM_GRACE`; it runs beside the loop so output keeps draining meanwhile.
    let mut terminating: Option<tokio::task::JoinHandle<()>> = None;
    let timeout_sleep = tokio::time::sleep(limits.timeout.unwrap_or_default());
    tokio::pin!(timeout_sleep);
    loop {
        tokio::select! {
            r = read_or_pending(&mut stdout, &mut obuf) => match r {
                Ok(n) if n > 0 => {
                    let text = out_dec.push(&obuf[..n]);
                    on_output(&text);
                    collector.push(&text, &alloc)
                }
                _ => stdout = None,
            },
            r = read_or_pending(&mut stderr, &mut ebuf) => match r {
                Ok(n) if n > 0 => {
                    let text = err_dec.push(&ebuf[..n]);
                    on_output(&text);
                    collector.push(&text, &alloc)
                }
                _ => stderr = None,
            },
            st = child.wait(), if status.is_none() => status = st.ok(),
            _ = cancel.cancelled(), if !stopping => {
                cancelled = true;
                stopping = true;
                terminating = Some(tokio::spawn(mu_base::process::terminate_group(pid)));
            }
            _ = &mut timeout_sleep, if !stopping && limits.timeout.is_some() => {
                timed_out = true;
                stopping = true;
                terminating = Some(tokio::spawn(mu_base::process::terminate_group(pid)));
            }
            _ = tokio::time::sleep(IDLE), if status.is_some() => break,
        }
        if status.is_some() && stdout.is_none() && stderr.is_none() {
            break;
        }
    }
    match terminating {
        Some(t) => {
            let _ = t.await;
        }
        None => mu_base::process::terminate_group(pid).await,
    }
    collector.push(&out_dec.finish(), &alloc);
    collector.push(&err_dec.finish(), &alloc);
    let code = exit_code(status);
    let end = RunEnd {
        wall: start.elapsed().as_secs_f64(),
        secs: limits.secs,
        cancelled,
        timed_out,
        exit_code: code,
        quiet_wall: false,
    };
    let out = finish(collector.finish(), end);
    let failed = cancelled || timed_out || code.is_some_and(|c| c != 0);
    let note = if cancelled {
        "cancelled".to_string()
    } else if timed_out {
        "timed out".to_string()
    } else {
        code.map_or("no exit status".to_string(), |c| format!("exit {c}"))
    };
    BackgroundResult { ok: !failed, text: out.text, note }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::Duration;

    #[tokio::test(flavor = "current_thread")]
    async fn pty_falls_back_without_ui() {
        let tools = Tools::for_test();
        let (events, rx) = tokio::sync::mpsc::unbounded_channel();
        drop(rx);
        let ctx = ToolCtx { events, cancel: Default::default(), call_id: "c".into(), plan: false };
        let out = tools.call("bash", &json!({"command":"echo hi","pty":true}), &ctx).await;
        assert!(out.text.contains("hi"), "{}", out.text);
        assert!(out.text.contains("[pty unavailable without the terminal UI; ran without a terminal]"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_background_run_reports_its_output_as_it_arrives() {
        let seen = std::cell::RefCell::new(String::new());
        let r = run_background(
            "echo first; echo second >&2".into(),
            std::env::temp_dir(),
            std::env::vars().collect(),
            None,
            CancellationToken::new(),
            || (0, std::env::temp_dir()),
            |s| seen.borrow_mut().push_str(s),
        )
        .await;
        assert!(r.ok, "{}", r.text);
        let seen = seen.into_inner();
        assert!(seen.contains("first") && seen.contains("second"), "{seen:?}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn output_keeps_draining_while_a_cancelled_background_run_is_terminated() {
        let cancel = CancellationToken::new();
        let cancelled_at = std::cell::Cell::new(None);
        let seen = std::cell::RefCell::new(Vec::new());
        let canceller = async {
            tokio::time::sleep(Duration::from_millis(300)).await;
            cancelled_at.set(Some(Instant::now()));
            cancel.cancel();
        };
        let run = run_background(
            "trap '' TERM; while :; do echo tick; sleep 0.02; done".into(),
            std::env::temp_dir(),
            std::env::vars().collect(),
            None,
            cancel.clone(),
            || (0, std::env::temp_dir()),
            |_| seen.borrow_mut().push(Instant::now()),
        );
        let (r, ()) = tokio::join!(run, canceller);
        assert!(!r.ok);
        let t0 = cancelled_at.get().unwrap();
        let (from, to) = (t0 + Duration::from_millis(50), t0 + Duration::from_millis(180));
        let during_grace = seen.borrow().iter().filter(|t| **t > from && **t < to).count();
        assert!(during_grace > 0, "no output was read during the SIGTERM grace");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn session_env_reaches_commands() {
        let tools = Tools::for_test();
        let (events, _rx) = tokio::sync::mpsc::unbounded_channel();
        let ctx = ToolCtx { events, cancel: Default::default(), call_id: "c".into(), plan: false };
        let cmd = json!({"command": "echo \"[$PI_SESSION_ID|$PI_SESSION_FILE]\""});
        let out = tools.call("bash", &cmd, &ctx).await;
        assert!(out.text.contains("[|]"), "{}", out.text);
        tools.set_session("abc", Some(std::path::PathBuf::from("/tmp/x_abc.jsonl")));
        let out = tools.call("bash", &cmd, &ctx).await;
        assert!(out.text.contains("[abc|/tmp/x_abc.jsonl]"), "{}", out.text);
    }

    /// An export outlives its call, so the call that makes it says so: names only, once, and not for
    /// variables the command merely read or for ones that were already set.
    #[tokio::test(flavor = "current_thread")]
    async fn a_command_that_exports_a_variable_is_told_it_persists() {
        let tools = Tools::for_test();
        let (events, _rx) = tokio::sync::mpsc::unbounded_channel();
        let ctx = ToolCtx { events, cancel: Default::default(), call_id: "c".into(), plan: false };
        let run = |c: &str| {
            let tools = &tools;
            let ctx = &ctx;
            let cmd = json!({"command": c});
            async move { tools.call("bash", &cmd, ctx).await.text }
        };
        let quiet = run("echo $HOME > /dev/null; FOO_LOCAL=1 true").await;
        assert!(!quiet.contains("[mu: this command set"), "{quiet}");
        let out = run("export MU_TEST_LEAK=topsecretvalue; export MU_TEST_LEAK2=1").await;
        assert!(out.contains("`MU_TEST_LEAK`, `MU_TEST_LEAK2`") && out.contains("Unset them"), "{out}");
        assert!(!out.contains("topsecretvalue"), "values are never echoed: {out}");
        let next = run("echo $MU_TEST_LEAK").await;
        assert!(!next.contains("[mu: this command set"), "only the call that changed it: {next}");
        let changed = run("export MU_TEST_LEAK=other").await;
        assert!(changed.contains("`MU_TEST_LEAK`") && changed.contains("Unset it"), "{changed}");
    }

    #[test]
    fn env_changes_ignore_bookkeeping_and_session_variables() {
        let m = |kv: &[(&str, &str)]| kv.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        let before = m(&[("A", "1"), ("PWD", "/a")]);
        let after = m(&[("A", "1"), ("B", "2"), ("PWD", "/b"), ("SHLVL", "3"), ("PI_SESSION_ID", "x")]);
        assert_eq!(env_changes(&before, &after, &["PI_SESSION_ID"]), ["B"]);
        assert!(env_changes(&after, &before, &[]).is_empty(), "an unset variable is not reported");
    }

    /// After a `cd`, a file tool given a relative path says where it resolved it; an absolute path does not.
    #[tokio::test(flavor = "current_thread")]
    async fn a_file_tool_notes_a_relative_path_after_the_shell_moved() {
        let tools = Tools::for_test();
        let (events, _rx) = tokio::sync::mpsc::unbounded_channel();
        let ctx = ToolCtx { events, cancel: Default::default(), call_id: "c".into(), plan: false };
        let glob = json!({"pattern": "*.mu-no-such-ext", "path": "."});
        let out = tools.call("glob", &glob, &ctx).await;
        assert!(!out.text.contains("[mu:"), "no shell call yet: {}", out.text);
        tools.call("bash", &json!({"command": "cd /usr"}), &ctx).await;
        let out = tools.call("glob", &glob, &ctx).await;
        assert!(out.text.contains("[mu: the relative path `.`") && out.text.contains("/usr"), "{}", out.text);
        let abs = json!({"pattern": "*.mu-no-such-ext", "path": "/usr"});
        assert!(!tools.call("glob", &abs, &ctx).await.text.contains("[mu:"));
        tools.call("bash", &json!({"command": "cd /tmp"}), &ctx).await;
        assert!(!tools.call("glob", &glob, &ctx).await.text.contains("[mu:"));
    }

    /// True if a live process has exactly `sleep {arg}` as its command line. `$` anchors the
    /// match so the wrapping shell (whose command line also contains the text) is not counted.
    async fn alive(arg: &str) -> bool {
        std::process::Command::new("pgrep").args(["-f", &format!("sleep {arg}$")]).output().unwrap().status.success()
    }

    #[tokio::test(flavor = "current_thread")]
    async fn background_processes_in_plain_mode() {
        // `pgrep`/`pkill -f` see every process on the machine, so the sleep durations carry this
        // process's pid: concurrent test runs cannot match or kill each other's sleeps. BSD `sleep`
        // rejects more than i32::MAX seconds, so the durations stay below it for any pid.
        let pid = std::process::id();
        let (quiet, kept) = (format!("2{pid:07}"), format!("3{pid:07}"));
        let tools = Tools::for_test();
        let (events, _rx) = tokio::sync::mpsc::unbounded_channel();
        let ctx = ToolCtx { events, cancel: Default::default(), call_id: "c".into(), plan: false };
        let cmd = format!("(/bin/sleep {quiet} &); nohup /bin/sleep {kept} >/dev/null 2>&1 & echo started");
        let out = tools.call("bash", &json!({"command": cmd}), &ctx).await;
        assert!(out.text.contains("started"), "{}", out.text);
        // A `(… &)` subshell job ends with the command; `nohup … &` (pi-shell's builtin, which detaches) keeps one alive.
        assert!(!alive(&quiet).await);
        // The background process may not have exec'd `sleep` yet when the command returns.
        let mut up = false;
        for _ in 0..50 {
            if alive(&kept).await {
                up = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert!(up);
        let _ = std::process::Command::new("pkill").args(["-f", &format!("sleep {kept}$")]).status();
    }

    /// mu's runtime is current-thread: the persistent shell must still reap a finished `&` child
    /// while idle, and keep its status for `wait` in a later call.
    #[tokio::test(flavor = "current_thread")]
    async fn finished_background_job_is_reaped_and_keeps_its_status() {
        let tools = Tools::for_test();
        let (events, _rx) = tokio::sync::mpsc::unbounded_channel();
        let ctx = ToolCtx { events, cancel: Default::default(), call_id: "c".into(), plan: false };
        let out = tools.call("bash", &json!({"command": "/bin/sh -c 'sleep 0.3; exit 7' & echo $!"}), &ctx).await;
        let pid: i32 = out.text.trim().parse().unwrap_or_else(|_| panic!("no pid: {}", out.text));
        let reaped = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            // SAFETY: signal 0 only probes whether the pid exists; a zombie still does.
            while unsafe { libc::kill(pid, 0) } == 0 {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await;
        assert!(reaped.is_ok(), "finished background child stayed a zombie");
        let out = tools.call("bash", &json!({"command": format!("wait {pid}; echo status=$?")}), &ctx).await;
        assert!(out.text.contains("status=7"), "{}", out.text);
    }

    #[test]
    fn pty_input_queue_does_not_block_exit() {
        use mu_base::events::PtyInput;
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(tokio::task::LocalSet::new().run_until(async {
            let tools = Tools::for_test();
            let (events, mut rx) = tokio::sync::mpsc::unbounded_channel();
            tokio::task::spawn_local(async move {
                while let Some(e) = rx.recv().await {
                    if let AgentEvent::PtyOpen { input, reply, .. } = e {
                        let _ = reply.send((80, 24));
                        // Far more input than a pty buffers, for a command that never reads it.
                        let _ = input.send(PtyInput::Bytes(vec![b'x'; 4 * 1024 * 1024]));
                        std::mem::forget(input);
                    }
                }
            });
            let ctx = ToolCtx { events, cancel: Default::default(), call_id: "c".into(), plan: false };
            let t = std::time::Instant::now();
            let out =
                tools.call("bash", &json!({"command":"sleep 0.3; echo done","pty":true,"timeout":20}), &ctx).await;
            // The echoed keystrokes flood the output, so "done" may be elided: what matters is that
            // the command finished normally while 4 MB of input was still queued.
            assert!(!out.is_error, "{}", out.text.chars().take(200).collect::<String>());
            assert!(t.elapsed() < std::time::Duration::from_secs(10));
        }));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn result_carries_structured_facts_and_the_same_text() {
        let tools = Tools::for_test();
        let (events, _rx) = tokio::sync::mpsc::unbounded_channel();
        let ctx = ToolCtx { events, cancel: Default::default(), call_id: "c".into(), plan: false };
        let out = tools.call("bash", &json!({"command": "echo hi; exit 3"}), &ctx).await;
        let Some(ToolMeta::Bash(m)) = &out.meta else { panic!("no meta: {:?}", out.text) };
        assert_eq!(m.exit_code, Some(3));
        assert!(out.text[..m.output_end].contains("hi"));
        assert!(!out.text[..m.output_end].contains("Command exited"));
        assert!(out.text.ends_with("Command exited with code 3"), "the model's text is unchanged: {}", out.text);
        let quiet = tools.call("bash", &json!({"command": "true"}), &ctx).await;
        let Some(ToolMeta::Bash(q)) = &quiet.meta else { panic!() };
        assert_eq!((q.output_end, q.exit_code), (0, None));
        assert_eq!(quiet.text, "(no output)");
    }

    /// Aborting a foreground command ends the call promptly and kills the command, also after
    /// earlier calls have run in the persistent shell.
    #[tokio::test(flavor = "current_thread")]
    async fn abort_ends_a_foreground_command_promptly() {
        let arg = format!("6{}", std::process::id());
        let tools = Tools::for_test();
        for cmd in ["sleep 0.2; echo done", "date"] {
            assert!(!tools.call("bash", &json!({"command": cmd}), &ctx()).await.is_error);
        }
        let c = ctx();
        let cancel = c.cancel.clone();
        let t = std::time::Instant::now();
        let abort = async {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            cancel.cancel();
        };
        let cmd = json!({"command": format!("sleep {arg}")});
        let (out, ()) = tokio::join!(tools.call("bash", &cmd, &c), abort);
        assert!(t.elapsed() < std::time::Duration::from_secs(5), "abort took {:?}", t.elapsed());
        let Some(ToolMeta::Bash(m)) = &out.meta else { panic!("no meta: {:?}", out.text) };
        assert!(m.cancelled, "{}", out.text);
        assert!(!alive(&arg).await);
        let next = tools.call("bash", &json!({"command": "echo after"}), &ctx()).await;
        assert_eq!(next.text.trim(), "after");
    }

    /// A shell whose threads are all stuck (a builtin blocked in a syscall) does not freeze the caller:
    /// the call ends once its timeout and the grace pass, or at once on abort, and the next call gets a
    /// working shell.
    #[tokio::test(flavor = "current_thread")]
    async fn a_hung_shell_does_not_freeze_the_caller() {
        let tools = Tools::for_test();
        tools.shell().jam(30);
        let t = std::time::Instant::now();
        let out = tools.call("bash", &json!({"command": "true", "timeout": 1}), &ctx()).await;
        assert!(t.elapsed() < std::time::Duration::from_secs(5), "took {:?}", t.elapsed());
        assert!(out.is_error && out.text.contains("stopped responding"), "{}", out.text);
        assert_eq!(tools.call("bash", &json!({"command": "echo back"}), &ctx()).await.text.trim(), "back");

        tools.shell().jam(30);
        let c = ctx();
        let cancel = c.cancel.clone();
        let abort = async {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            cancel.cancel();
        };
        let cmd = json!({"command": "true"});
        let (out, ()) = tokio::join!(tools.call("bash", &cmd, &c), abort);
        let Some(ToolMeta::Bash(m)) = &out.meta else { panic!("no meta: {:?}", out.text) };
        assert!(m.cancelled, "{}", out.text);
        assert_eq!(tools.call("bash", &json!({"command": "echo again"}), &ctx()).await.text.trim(), "again");
    }

    /// Tools with a jobs handle attached; background jobs are spawned on the current LocalSet.
    fn tools_with_jobs() -> (Tools, std::rc::Rc<crate::jobs::JobsHandle>) {
        let mut tools = Tools::for_test();
        let jobs = crate::jobs::tests::handle();
        tools.jobs = Some(jobs.clone());
        (tools, jobs)
    }

    fn ctx() -> ToolCtx {
        let (events, _rx) = tokio::sync::mpsc::unbounded_channel();
        ToolCtx { events, cancel: Default::default(), call_id: "c".into(), plan: false }
    }

    async fn finished(jobs: &crate::jobs::JobsHandle) -> Vec<crate::jobs::Done> {
        jobs.finished().await
    }

    #[tokio::test(flavor = "current_thread")]
    async fn async_job_is_delivered_once_with_output_and_exit_code() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (tools, jobs) = tools_with_jobs();
                let out = tools.call("bash", &json!({"command": "echo hi; exit 3", "async": true}), &ctx()).await;
                assert!(!out.is_error && out.text.contains("bg_1"), "{}", out.text);
                let done = finished(&jobs).await;
                assert_eq!(done.len(), 1);
                assert!(!done[0].ok && done[0].note.as_deref() == Some("exit 3"), "{:?}", done[0].note);
                assert!(done[0].text.starts_with("hi"), "{}", done[0].text);
                assert!(jobs.take_undelivered().is_empty() && !jobs.has_undelivered());
                let msg = crate::jobs::notice(&done);
                assert!(msg.contains("## bg_1 (bash: echo hi; exit 3) — failed, exit 3\nhi"), "{msg}");
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn timeout_kills_the_background_command() {
        // The duration carries this pid so `alive` sees only this test's sleep.
        let arg = format!("4{}", std::process::id());
        tokio::task::LocalSet::new()
            .run_until(async {
                let (tools, jobs) = tools_with_jobs();
                let cmd = json!({"command": format!("sleep {arg}"), "async": true, "timeout": 1});
                tools.call("bash", &cmd, &ctx()).await;
                let done = finished(&jobs).await;
                assert!(!done[0].ok && done[0].note.as_deref() == Some("timed out"), "{:?}", done[0].note);
                assert!(done[0].text.contains("[Command timed out after 1 seconds]"), "{}", done[0].text);
                assert!(!alive(&arg).await);
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancel_kills_the_background_command() {
        let arg = format!("5{}", std::process::id());
        tokio::task::LocalSet::new()
            .run_until(async {
                let (tools, jobs) = tools_with_jobs();
                tools.call("bash", &json!({"command": format!("sleep {arg}"), "async": true}), &ctx()).await;
                assert_eq!(jobs.kill(None), 1);
                let done = finished(&jobs).await;
                assert!(!done[0].ok && done[0].note.as_deref() == Some("cancelled"), "{:?}", done[0].note);
                assert!(!alive(&arg).await);
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn job_cap_applies_to_bash_jobs() {
        let (tools, jobs) = tools_with_jobs();
        for i in 0..crate::tools::task::MAX_TASKS {
            jobs.push_running(&format!("R{i}"));
        }
        let out = tools.call("bash", &json!({"command": "echo no", "async": true}), &ctx()).await;
        assert!(out.is_error && out.text.contains("too many background jobs running (8/8)"), "{}", out.text);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn async_runs_in_the_foreground_without_jobs() {
        let tools = Tools::for_test();
        let out = tools.call("bash", &json!({"command": "echo fg", "async": true}), &ctx()).await;
        assert_eq!(out.text.trim(), "fg");
    }

    /// A job starts from the cwd and exported variables the persistent shell has at launch, and
    /// values survive newlines, quotes, spaces and `=`.
    #[tokio::test(flavor = "current_thread")]
    async fn async_job_inherits_the_shell_cwd_and_exports() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (tools, jobs) = tools_with_jobs();
                let setup = r#"cd /usr && export MU_SNAP_V="$(printf 'l1\nl2 "q" \\ $x `t` a=b\t')""#;
                let (fg, c) = (json!({"command": setup}), ctx());
                assert!(!tools.call("bash", &fg, &c).await.is_error);
                let (job, c) = (json!({"command": r#"printf '%s\n%s' "$PWD" "$MU_SNAP_V""#, "async": true}), ctx());
                tools.call("bash", &job, &c).await;
                let done = finished(&jobs).await;
                assert!(done[0].ok, "{}", done[0].text);
                assert_eq!(done[0].text, "/usr\nl1\nl2 \"q\" \\ $x `t` a=b\t");
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn variables_unset_in_the_shell_do_not_reappear() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (tools, jobs) = tools_with_jobs();
                let (fg, c) = (json!({"command": "unset HOME"}), ctx());
                tools.call("bash", &fg, &c).await;
                let (job, c) = (json!({"command": r#"printf '[%s]' "${HOME-unset}""#, "async": true}), ctx());
                tools.call("bash", &job, &c).await;
                let done = finished(&jobs).await;
                assert_eq!(done[0].text, "[unset]");
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn explicit_cwd_overrides_the_shell_cwd() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (tools, jobs) = tools_with_jobs();
                let (fg, c) = (json!({"command": "cd /usr"}), ctx());
                tools.call("bash", &fg, &c).await;
                let job = json!({"command": r#"printf '%s' "$PWD""#, "async": true, "cwd": "/"});
                tools.call("bash", &job, &ctx()).await;
                let done = finished(&jobs).await;
                assert_eq!(done[0].text, "/");
            })
            .await;
    }

    /// The job is `bash -c`, not the user's `$SHELL` and not a login shell (`~/.profile` is not read).
    #[tokio::test(flavor = "current_thread")]
    async fn job_runs_under_bash_without_a_login_profile() {
        let home = std::env::temp_dir().join(format!("mu-job-home-{}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join(".profile"), "echo PROFILE_SOURCED\n").unwrap();
        tokio::task::LocalSet::new()
            .run_until(async {
                let (tools, jobs) = tools_with_jobs();
                let setup = format!("export HOME='{}' SHELL=/nonexistent/zsh", home.display());
                let (fg, c) = (json!({"command": setup}), ctx());
                tools.call("bash", &fg, &c).await;
                let probe =
                    r#"printf '%s ' "${BASH_VERSION:+bash}"; shopt -q login_shell && echo login || echo nologin"#;
                let (job, c) = (json!({"command": probe, "async": true}), ctx());
                tools.call("bash", &job, &c).await;
                let done = finished(&jobs).await;
                assert!(done[0].ok, "{}", done[0].text);
                assert_eq!(done[0].text.trim(), "bash nologin");
            })
            .await;
        let _ = std::fs::remove_dir_all(&home);
    }

    /// A foreground command holds the persistent shell, so the snapshot times out: the job still
    /// runs, with mu's own environment, and the call says so.
    #[tokio::test(flavor = "current_thread")]
    async fn busy_shell_falls_back_to_mu_environment_and_says_so() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (tools, jobs) = tools_with_jobs();
                let (busy, c1) = (json!({"command": "sleep 2.5"}), ctx());
                let (job, c2) = (json!({"command": "echo fallback-ran", "async": true}), ctx());
                let start = async {
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    tools.call("bash", &job, &c2).await
                };
                let (_, out) = tokio::join!(tools.call("bash", &busy, &c1), start);
                assert!(
                    !out.is_error && out.text.contains("Shell state from earlier calls was not inherited"),
                    "{}",
                    out.text
                );
                let done = finished(&jobs).await;
                assert!(done[0].ok && done[0].text.starts_with("fallback-ran"), "{}", done[0].text);
            })
            .await;
    }

    #[test]
    fn snapshot_fields_split_exactly_and_reject_malformed_answers() {
        let s = |k: &str, v: &str| (k.to_string(), v.to_string());
        let out = "/w d\0A\0x\ny \"q\" a=b\0B\0\0";
        assert_eq!(parse_snapshot(out), Some((PathBuf::from("/w d"), vec![s("A", "x\ny \"q\" a=b"), s("B", "")])));
        assert!(parse_snapshot("rel\0").is_none());
        assert!(parse_snapshot("/a\0A\0").is_none());
        assert!(parse_snapshot("/a\0=x\0v\0").is_none());
        assert!(parse_snapshot("").is_none());
    }

    #[test]
    fn job_env_drops_bookkeeping_and_applies_session_vars_last() {
        let s = |k: &str, v: &str| (k.to_string(), v.to_string());
        let vars = vec![
            s("PATH", "/bin"),
            s("_", "/x"),
            s("SHLVL", "3"),
            s("OLDPWD", "/"),
            s("PWD", "/old"),
            s("PI_SESSION_ID", "stale"),
        ];
        let env = job_env(vars, Path::new("/new"), vec![("PI_SESSION_ID", "s1".into())]);
        assert_eq!(env, vec![s("PATH", "/bin"), s("PWD", "/new"), s("PI_SESSION_ID", "s1")]);
    }
}
