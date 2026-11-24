use crate::jobs::{JobsHandle, WaitReady};
use crate::tools::spill::{Collected, Collector};
use crate::tools::subagent::BackgroundResult;
use crate::tools::{ToolCtx, Tools};
use mu_base::events::AgentEvent;
use mu_base::tool::{BashMeta, ToolMeta, ToolOutput};
use omp_shell::{
    cancel::{AbortReason, CancelToken},
    shell::ShellRunOptions,
};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};
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
    /// Regex; with `async`, output lines that match reach the model while the job runs.
    notify: Option<String>,
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
    stream: Stream,
    collector: Collector,
    buf: Vec<u8>,
    eof: bool,
    status: Option<std::process::ExitStatus>,
    cancelled: bool,
    timed_out: bool,
    term_at: Option<tokio::time::Instant>,
    killed: bool,
    wait: InputWait,
}

/// How often a running pty is checked for waiting on the keyboard, and how long it must have been
/// quiet before the probes are worth running.
const INPUT_PROBE_EVERY: std::time::Duration = std::time::Duration::from_millis(500);
/// Silence after which text with the cursor behind it counts as a prompt, when no probe says so.
const PROMPT_SILENCE: std::time::Duration = std::time::Duration::from_secs(2);
/// Not told that a command waits for input while the user has typed this recently.
const USER_ACTIVE: std::time::Duration = std::time::Duration::from_secs(10);

/// Whether a pty command is waiting for the keyboard, and whether the user has been told.
struct InputWait {
    last_output: tokio::time::Instant,
    last_typed: Option<tokio::time::Instant>,
    /// The command was seen waiting, and nothing has happened since.
    waiting: bool,
    /// The front end was told so.
    told: bool,
}

impl InputWait {
    fn new() -> Self {
        Self { last_output: tokio::time::Instant::now(), last_typed: None, waiting: false, told: false }
    }
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
    fn emit(&mut self, (raw, clean): (String, String), ctx: &ToolCtx, tools: &Tools) {
        if !raw.is_empty() {
            self.wait.last_output = tokio::time::Instant::now();
            self.not_waiting(ctx);
            let _ = ctx.events.send(AgentEvent::ToolOutput { id: ctx.call_id.clone(), chunk: raw });
        }
        self.collector.push(&clean, &|| tools.next_artifact());
    }

    /// Output or typing ended a wait: the front end is told if it had been.
    fn not_waiting(&mut self, ctx: &ToolCtx) {
        self.wait.waiting = false;
        if std::mem::take(&mut self.wait.told) {
            let _ = ctx.events.send(AgentEvent::PtyWaiting { id: ctx.call_id.clone(), waiting: false });
        }
    }

    /// Checks whether the command waits for the keyboard: echo off or a read blocked on the
    /// terminal (see `PtyIo::awaiting_input`), or, failing those, text with the cursor behind it
    /// and no output for `PROMPT_SILENCE`. The user is told once per wait, never while typing.
    async fn probe_input(&mut self, ctx: &ToolCtx) {
        let quiet = self.wait.last_output.elapsed();
        let waiting = quiet >= INPUT_PROBE_EVERY
            && (self.io.awaiting_input().await || (quiet >= PROMPT_SILENCE && self.stream.cleaner.has_partial_line()));
        if !waiting {
            self.wait.waiting = false;
            return;
        }
        if self.wait.waiting {
            return;
        }
        self.wait.waiting = true;
        if self.wait.last_typed.is_none_or(|t| t.elapsed() >= USER_ACTIVE) {
            self.wait.told = true;
            let _ = ctx.events.send(AgentEvent::PtyWaiting { id: ctx.call_id.clone(), waiting: true });
        }
    }

    /// Runs the command until it exits, or until it is cancelled or times out and has been terminated.
    async fn pump(&mut self, tools: &Tools, ctx: &ToolCtx, timeout: Option<std::time::Duration>) {
        let timeout_sleep = tokio::time::sleep(timeout.unwrap_or(std::time::Duration::from_secs(1)));
        tokio::pin!(timeout_sleep);
        let mut probe = tokio::time::interval(INPUT_PROBE_EVERY);
        probe.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
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
                        let text = self.stream.push(&self.buf[..n]);
                        self.emit(text, ctx, tools);
                    }
                },
                w = write_some(&self.io, &self.pending_in, &mut self.write_buf), if !self.pending_in.is_empty() && self.term_at.is_none() => match w {
                    Ok(n) => { self.pending_in.drain(..n); }
                    Err(_) => self.pending_in.clear(),
                },
                m = self.input.recv(), if self.input_open => match m {
                    Some(mu_base::events::PtyInput::Bytes(b)) => {
                        self.wait.last_typed = Some(tokio::time::Instant::now());
                        self.not_waiting(ctx);
                        self.pending_in.extend(b);
                    }
                    Some(mu_base::events::PtyInput::Resize { cols, rows }) => self.io.resize(cols, rows),
                    Some(mu_base::events::PtyInput::Kill) => {
                        if self.term() {
                            self.cancelled = true;
                        }
                    }
                    None => self.input_open = false,
                },
                _ = probe.tick(), if self.term_at.is_none() && !self.eof => self.probe_input(ctx).await,
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
        self.drain(tools, ctx).await;
    }

    /// After the leader is gone: whatever it left in the group is terminated first, then the
    /// remaining output is read under `Drain`'s policy.
    async fn drain(&mut self, tools: &Tools, ctx: &ToolCtx) {
        if self.eof {
            return;
        }
        self.io.signal(libc::SIGTERM);
        let kill_at = tokio::time::Instant::now() + KILL_AFTER;
        let mut drain = Drain::start();
        let mut killed = false;
        while !self.eof {
            let wake = if killed { drain.ends_at() } else { kill_at.min(drain.ends_at()) };
            tokio::select! {
                r = self.io.read(&mut self.buf) => match r {
                    Ok(n) if n > 0 => {
                        let text = self.stream.push(&self.buf[..n]);
                        self.emit(text, ctx, tools);
                        drain.touch();
                    }
                    _ => self.eof = true,
                },
                _ = tokio::time::sleep_until(wake) => {
                    if drain.over() {
                        break;
                    }
                    if !killed && tokio::time::Instant::now() >= kill_at {
                        killed = true;
                        self.io.signal(libc::SIGKILL);
                    }
                }
                _ = ctx.cancel.cancelled(), if !self.cancelled => {
                    self.cancelled = true;
                    break;
                }
            }
        }
    }
}

/// How long a terminal request waits for a front end to size it before the call runs without one.
const PTY_OPEN_WAIT: std::time::Duration = std::time::Duration::from_secs(60);
const KILL_AFTER: std::time::Duration = std::time::Duration::from_millis(200);
const GIVE_UP_AFTER: std::time::Duration = std::time::Duration::from_millis(800);

/// The one policy for the output a command's descendants write after the leader has exited, for
/// the pty and the piped runs alike: whatever is left in the group is terminated at once, and the
/// output is read until EOF, `IDLE` without data, `CAP` in all, or cancel.
#[derive(Clone, Copy)]
struct Drain {
    cap_at: tokio::time::Instant,
    idle_at: tokio::time::Instant,
}

impl Drain {
    const CAP: std::time::Duration = std::time::Duration::from_millis(1000);
    const IDLE: std::time::Duration = std::time::Duration::from_millis(300);

    fn start() -> Self {
        let now = tokio::time::Instant::now();
        Self { cap_at: now + Self::CAP, idle_at: now + Self::IDLE }
    }

    /// Data arrived: the quiet period starts over.
    fn touch(&mut self) {
        self.idle_at = tokio::time::Instant::now() + Self::IDLE;
    }

    /// When the drain ends if nothing more arrives.
    fn ends_at(&self) -> tokio::time::Instant {
        self.cap_at.min(self.idle_at)
    }

    fn over(&self) -> bool {
        tokio::time::Instant::now() >= self.ends_at()
    }
}

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

struct ForegroundJobGuard {
    jobs: Rc<JobsHandle>,
    id: String,
    armed: bool,
}

impl Drop for ForegroundJobGuard {
    fn drop(&mut self) {
        if self.armed {
            self.jobs.kill(Some(&self.id));
            self.jobs.discard_result(&self.id);
        }
    }
}

impl Tools {
    /// Where a bash call starts: mirrors `bash_interruptible`, `bash_pty`, `bash_plain` and
    /// `start_bash_job`, so the gate resolves relative words from the directory the command runs in.
    pub(crate) fn bash_dir(&self, cwd: Option<&str>, pty: bool, asynchronous: bool) -> PathBuf {
        if let Some(c) = cwd {
            return self.abs(c);
        }
        if pty || (self.jobs.is_some() && !asynchronous) {
            return self.cwd.clone();
        }
        self.shell_cwd.borrow().clone().unwrap_or_else(|| self.cwd.clone())
    }

    /// After `set_cwd`: moves the persistent shell to the new directory before anything reads or runs in it.
    async fn sync_shell_cwd(&self) {
        let Some(dir) = self.shell_cd.borrow_mut().take() else { return };
        let ms = SNAPSHOT_TIMEOUT.as_millis() as u32;
        let command = format!("cd -- '{}'", dir.to_string_lossy().replace('\'', "'\\''"));
        let opts = ShellRunOptions { command, timeout_ms: Some(ms), ..Default::default() };
        let mut run = self.shell().spawn(opts, None, CancelToken::new(Some(ms)));
        let _ = tokio::time::timeout(SNAPSHOT_TIMEOUT, run.join()).await;
    }

    pub async fn bash(&self, a: Args, ctx: &ToolCtx) -> ToolOutput {
        self.sync_shell_cwd().await;
        if let Some(jobs) = &self.jobs
            && a.r#async
            && !a.pty
        {
            return self.bash_background(a, jobs).await;
        }
        if !a.pty
            && !a.r#async
            && let Some(jobs) = &self.jobs
            && let Some(interrupt) = &ctx.wait_interrupt
        {
            return self.bash_interruptible(a, jobs, ctx, interrupt).await;
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
        let notify = match a.notify.as_deref().filter(|p| !p.is_empty()).map(regex::Regex::new).transpose() {
            Ok(re) => re,
            Err(e) => return ToolOutput::err(format!("notify: invalid regex: {e}")),
        };
        let watching = notify.is_some();
        let (id, note, _sink) = match self.start_bash_job(a, jobs, None, None, None, notify).await {
            Ok(started) => started,
            Err(e) => return ToolOutput::err(e),
        };
        let next = "When it finishes, mu automatically starts your next turn with its result; use `wait` when you need it now. Until then, do other work that does not depend on it.";
        let watch = if watching {
            "\nOutput lines that match `notify` arrive as a system notice while it runs (an idle session is woken, and a blocked `wait` ends)."
        } else {
            ""
        };
        let mut out = ToolOutput::ok(format!("Started background job {id}.{note}{watch}\n{next}"));
        out.meta = Some(ToolMeta::Bash(BashMeta { background: Some(id), ..Default::default() }));
        out
    }

    async fn bash_interruptible(
        &self,
        a: Args,
        jobs: &Rc<JobsHandle>,
        ctx: &ToolCtx,
        interrupt: &CancellationToken,
    ) -> ToolOutput {
        let (id, note, sink) = match self
            .start_bash_job(a, jobs, Some(&ctx.cancel), Some(self.cwd.clone()), Some(ctx.call_id.clone()), None)
            .await
        {
            Ok(started) => started,
            Err(e) => return ToolOutput::err(e),
        };
        let mut guard = ForegroundJobGuard { jobs: jobs.clone(), id: id.clone(), armed: true };
        let ids = [id.clone()];
        let mut ready = jobs.wait_ready(&ids, &ctx.cancel, interrupt).await;
        if ready == WaitReady::Cancelled {
            jobs.kill(Some(&id));
            ready = jobs.wait_ready(&ids, &CancellationToken::new(), &CancellationToken::new()).await;
        }
        // The call is about to return: whatever the command prints from here on is a job's output,
        // not this call's.
        sink.detach();
        match ready {
            WaitReady::Ready(_) => {
                if let Some(done) = jobs.take_undelivered_for(&ids).into_iter().next() {
                    guard.armed = false;
                    let mut out = if done.ok { ToolOutput::ok(done.text) } else { ToolOutput::err(done.text) };
                    out.text.push_str(&note);
                    let mut meta = done.meta.unwrap_or_default();
                    meta.background = None;
                    out.meta = Some(ToolMeta::Bash(meta));
                    out
                } else {
                    ToolOutput::err("Command job is no longer available.")
                }
            }
            WaitReady::Interrupted => {
                guard.armed = false;
                // The command runs on: from now on it is listed as an ordinary background job.
                jobs.promote(&id);
                let mut out = ToolOutput::ok(format!(
                    "Command continues as background job {id}. Your new prompt will be handled now; its result will arrive automatically.{note}"
                ));
                out.meta = Some(ToolMeta::Bash(BashMeta { background: Some(id), ..Default::default() }));
                out
            }
            WaitReady::Nothing | WaitReady::Unknown(_) | WaitReady::Cancelled | WaitReady::Matched(_) => {
                ToolOutput::err("Command job is no longer available.")
            }
        }
    }

    /// Starts one command as a job. `base`: the directory the command starts in, when the caller has
    /// its own shell instead of inheriting the persistent one's; `cancel` is checked again just
    /// before the spawn, so a cancelled call never launches. `waiting`: the tool call of a
    /// foreground call, whose job is not listed while it waits and streams its output to that call;
    /// `None` starts a listed background job.
    async fn start_bash_job(
        &self,
        a: Args,
        jobs: &Rc<JobsHandle>,
        cancel: Option<&CancellationToken>,
        base: Option<PathBuf>,
        waiting: Option<String>,
        notify: Option<regex::Regex>,
    ) -> Result<(String, String, crate::jobs::OutputSink), String> {
        if cancel.is_some_and(CancellationToken::is_cancelled) {
            return Err("Command cancelled before launch.".into());
        }
        // A foreground call has its own slot: only a background job is held to the cap.
        if waiting.is_none() {
            jobs.check_capacity(1)?;
        }
        let snapshot = self.shell_snapshot(true).await;
        let inherited = snapshot.is_some();
        let (snap_cwd, vars) = snapshot.unwrap_or_else(|| (self.cwd.clone(), process_env()));
        let cwd = a.cwd.as_ref().map(|c| self.abs(c)).unwrap_or(base.unwrap_or(snap_cwd));
        let env = job_env(vars, &cwd, self.session_env());
        let artifact_dir = self.artifact_dir();
        let (command, timeout) = (a.command.clone(), a.timeout);
        let job_cwd = cwd.clone();
        if cancel.is_some_and(CancellationToken::is_cancelled) {
            return Err("Command cancelled before launch.".into());
        }
        let run = move |cancel: CancellationToken, sink: crate::jobs::OutputSink| {
            let pgid = sink.pgid();
            run_background(
                command,
                cwd,
                env,
                timeout,
                cancel,
                move || Tools::next_artifact_in(&artifact_dir),
                move |raw, clean| {
                    sink.push(raw);
                    sink.lines(clean);
                },
                pgid,
            )
        };
        let (id, sink) = match waiting {
            Some(call) => jobs.spawn_bash_waiting(a.command, job_cwd, call, run),
            None => jobs.spawn_bash_notify(a.command, job_cwd, notify, run),
        };
        let note = if inherited {
            String::new()
        } else {
            "\nShell state from earlier calls was not inherited (the shell did not answer in time): the job uses mu's own environment.".to_string()
        };
        Ok((id, note, sink))
    }

    /// The persistent shell's cwd and exported variables, from one state-neutral command. `None`
    /// when the shell does not answer within `SNAPSHOT_TIMEOUT` or the answer does not parse.
    /// `fork`: ask a fork of the shell, which has its state as of the last command boundary and
    /// answers without waiting for a running command; otherwise wait for the shell itself.
    async fn shell_snapshot(&self, fork: bool) -> Option<(PathBuf, Vec<(String, String)>)> {
        let ms = SNAPSHOT_TIMEOUT.as_millis() as u32;
        let (tx, rx) = flume::unbounded::<String>();
        let opts = ShellRunOptions { command: SNAPSHOT_COMMAND.into(), timeout_ms: Some(ms), ..Default::default() };
        let cancel = CancelToken::new(Some(ms));
        let mut run = if fork {
            self.shell().spawn_fork(opts, Some(tx), cancel)
        } else {
            self.shell().spawn(opts, Some(tx), cancel)
        };
        let res = tokio::time::timeout(SNAPSHOT_TIMEOUT, run.join()).await.ok()?.ok()?;
        if res.cancelled || res.timed_out || res.exit_code != Some(0) {
            return None;
        }
        let snapshot = parse_snapshot(&rx.drain().collect::<String>())?;
        // A value exported inside the shell is unknown to the redactor's environment snapshot.
        mu_base::secrets::register_exported(snapshot.1.iter().cloned());
        Some(snapshot)
    }

    /// Before the first foreground command: what the shell starts with, so later changes are the command's.
    async fn baseline_shell_env(&self) {
        if self.shell_env.borrow().is_some() {
            return;
        }
        if let Some((cwd, vars)) = self.shell_snapshot(false).await {
            *self.shell_cwd.borrow_mut() = Some(cwd);
            *self.shell_env.borrow_mut() = Some(vars.into_iter().collect());
        }
    }

    /// After a foreground command: records the shell's directory (for the file tools' relative-path note) and
    /// returns a reminder when the command set or changed an exported variable, which then stays set for every
    /// later call and for anything those start. Names only, never values.
    async fn after_foreground(&self) -> Option<String> {
        let (cwd, vars) = self.shell_snapshot(false).await?;
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
        // With no client attached nobody answers: the call must not wait for ever, and Cancel breaks it.
        let (cols, rows) = tokio::select! {
            size = rx => size.ok()?,
            () = ctx.cancel.cancelled() => return None,
            () = tokio::time::sleep(PTY_OPEN_WAIT) => return None,
        };
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
            stream: Stream::default(),
            collector: Collector::new(cwd),
            buf: vec![0u8; 64 * 1024],
            eof: false,
            status: None,
            cancelled: false,
            timed_out: false,
            term_at: None,
            killed: false,
            wait: InputWait::new(),
        };
        let start = Instant::now();
        run.pump(self, ctx, limits.timeout).await;
        // The command is over. Anything it left running in the background goes with it; the
        // group is gone once the leader is reaped only if nothing else is in it.
        mu_base::process::terminate_group(run.io.pid()).await;
        let rest = run.stream.finish();
        run.emit(rest, ctx, self);
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
        // Only the pty view gets raw bytes; the live tail and the model get cleaned text.
        let mut cleaner = mu_base::pty::Cleaner::default();
        let mut feed = |s: String| {
            let clean = cleaner.push(&s);
            if !clean.is_empty() {
                c.push(&clean, &|| self.next_artifact());
                let _ = ctx.events.send(AgentEvent::ToolOutput { id: ctx.call_id.clone(), chunk: clean });
            }
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
            c.push(&rest, &|| self.next_artifact());
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
/// but NUL, so the fields split exactly. The tool profile has no `compgen -e` (oh-my-pi v18.17
/// dropped it), so the names come from `declare -px` and each value is re-read with `${!name}`:
/// that needs no unquoting of `declare`'s own escaping, so a value's bytes survive exactly.
const SNAPSHOT_COMMAND: &str = r#"printf '%s\0' "$(pwd)"; while IFS= read -r __mu_l; do case $__mu_l in 'declare -'*x*) __mu_n=${__mu_l#declare -}; __mu_n=${__mu_n#* }; __mu_n=${__mu_n%%=*}; printf '%s\0%s\0' "$__mu_n" "${!__mu_n}";; esac; done <<< "$(declare -px)""#;

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

/// One output stream on its way to the result: bytes are decoded to text (the pty view and the live
/// tail get it raw), then cleaned for the model. Each stream holds its own escape state and
/// unfinished line, so a run with two streams has two.
#[derive(Default)]
struct Stream {
    decoder: omp_shell::output_decode::OutputDecoder,
    cleaner: mu_base::pty::Cleaner,
}

impl Stream {
    /// `(raw, clean)` for the bytes read.
    fn push(&mut self, bytes: &[u8]) -> (String, String) {
        let raw = self.decoder.push(bytes);
        let clean = self.cleaner.push(&raw);
        (raw, clean)
    }

    /// What the decoder and cleaner still hold, at the end of the stream.
    fn finish(&mut self) -> (String, String) {
        let raw = self.decoder.finish();
        let clean = self.cleaner.push(&raw) + &self.cleaner.finish();
        (raw, clean)
    }
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
    let silent_success = !had_output && !cancelled && !timed_out && exit_code == Some(0);
    let mut text = if had_output {
        c.body
    } else if silent_success {
        "(no output, exit code 0)".to_string()
    } else {
        "(no output)".to_string()
    };
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
        background: None,
    }));
    out
}

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
#[allow(clippy::too_many_arguments)]
pub async fn run_background(
    command: String,
    cwd: PathBuf,
    env: Vec<(String, String)>,
    timeout: Option<f64>,
    cancel: CancellationToken,
    alloc: impl Fn() -> (u32, PathBuf),
    on_output: impl Fn(&str, &str),
    pgid_out: Arc<AtomicU32>,
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
                meta: None,
            };
        }
    };
    let pid = child.id().unwrap_or(0) as i32;
    // The child leads its own group (`spawn_grouped`), so this is the group's id too.
    pgid_out.store(u32::try_from(pid).unwrap_or(0), AtomicOrdering::Relaxed);
    let (mut stdout, mut stderr) = (child.stdout.take(), child.stderr.take());
    let (mut out_stream, mut err_stream) = (Stream::default(), Stream::default());
    let (mut obuf, mut ebuf) = (vec![0u8; 8192], vec![0u8; 8192]);
    let mut collector = Collector::new(cwd.clone());
    let mut status: Option<std::process::ExitStatus> = None;
    let (mut cancelled, mut timed_out, mut stopping) = (false, false, false);
    // Terminating takes `TERM_GRACE`; it runs beside the loop so output keeps draining meanwhile.
    let mut terminating: Option<tokio::task::JoinHandle<()>> = None;
    let timeout_sleep = tokio::time::sleep(limits.timeout.unwrap_or_default());
    tokio::pin!(timeout_sleep);
    let mut drain: Option<Drain> = None;
    loop {
        let ends_at = drain.map(|d| d.ends_at());
        let drain_end = async {
            match ends_at {
                Some(t) => tokio::time::sleep_until(t).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            r = read_or_pending(&mut stdout, &mut obuf) => match r {
                Ok(n) if n > 0 => {
                    let (raw, clean) = out_stream.push(&obuf[..n]);
                    on_output(&raw, &clean);
                    collector.push(&clean, &alloc);
                    drain.iter_mut().for_each(Drain::touch);
                }
                _ => stdout = None,
            },
            r = read_or_pending(&mut stderr, &mut ebuf) => match r {
                Ok(n) if n > 0 => {
                    let (raw, clean) = err_stream.push(&ebuf[..n]);
                    on_output(&raw, &clean);
                    collector.push(&clean, &alloc);
                    drain.iter_mut().for_each(Drain::touch);
                }
                _ => stderr = None,
            },
            st = child.wait(), if status.is_none() => {
                status = st.ok();
                // Whatever the command left running goes now; the drain below is capped.
                if terminating.is_none() {
                    terminating = Some(tokio::spawn(mu_base::process::terminate_group(pid)));
                }
                drain = Some(Drain::start());
            }
            _ = drain_end, if drain.is_some() => break,
            _ = cancel.cancelled(), if !stopping => {
                cancelled = true;
                stopping = true;
                if terminating.is_none() {
                    terminating = Some(tokio::spawn(mu_base::process::terminate_group(pid)));
                }
                if status.is_some() {
                    break;
                }
            }
            _ = &mut timeout_sleep, if !stopping && status.is_none() && limits.timeout.is_some() => {
                timed_out = true;
                stopping = true;
                terminating = Some(tokio::spawn(mu_base::process::terminate_group(pid)));
            }
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
    collector.push(&out_stream.finish().1, &alloc);
    collector.push(&err_stream.finish().1, &alloc);
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
    let meta = match out.meta {
        Some(ToolMeta::Bash(meta)) => Some(meta),
        _ => None,
    };
    BackgroundResult { ok: !failed, text: out.text, note, meta }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mu_base::events::EventTx;
    use serde_json::json;

    /// A fresh process-group slot, as a job's sink hands one to `run_background`.
    fn pgid() -> Arc<AtomicU32> {
        Arc::new(AtomicU32::new(0))
    }
    use std::future::Future;
    use std::time::Duration;

    #[tokio::test(flavor = "current_thread")]
    async fn pty_falls_back_without_ui() {
        let tools = Tools::for_test();
        let (events, rx) = tokio::sync::mpsc::unbounded_channel();
        drop(rx);
        let ctx = ToolCtx { events, cancel: Default::default(), call_id: "c".into(), mode: None, wait_interrupt: None };
        let out = tools.call("bash", &json!({"command":"echo hi","pty":true}), &ctx).await;
        assert!(out.text.contains("hi"), "{}", out.text);
        assert!(out.text.contains("[pty unavailable without the terminal UI; ran without a terminal]"));
    }

    /// Runs a pty command and returns every `PtyWaiting` flag the front end saw. Once the first one
    /// arrives, `typed` (if any) is sent as keystrokes and the run waits for the next, then is cancelled.
    async fn pty_waiting_flags(command: &str, typed: Option<&'static [u8]>) -> Vec<bool> {
        use std::cell::RefCell;
        tokio::task::LocalSet::new()
            .run_until(async {
                let tools = Tools::for_test();
                let (events, mut rx) = tokio::sync::mpsc::unbounded_channel();
                let flags = Rc::new(RefCell::new(Vec::new()));
                let input = Rc::new(RefCell::new(None));
                let (f, i) = (flags.clone(), input.clone());
                tokio::task::spawn_local(async move {
                    while let Some(e) = rx.recv().await {
                        match e {
                            AgentEvent::PtyOpen { input, reply, .. } => {
                                let _ = reply.send((80, 24));
                                *i.borrow_mut() = Some(input);
                            }
                            AgentEvent::PtyWaiting { waiting, .. } => f.borrow_mut().push(waiting),
                            _ => {}
                        }
                    }
                });
                let cancel = CancellationToken::new();
                let ctx =
                    ToolCtx { events, cancel: cancel.clone(), call_id: "c".into(), mode: None, wait_interrupt: None };
                let cmd = json!({"command": command, "pty": true, "timeout": 60});
                let driver = async {
                    mu_testutil::wait_until("the command to be seen waiting", || !flags.borrow().is_empty()).await;
                    if let Some(bytes) = typed {
                        let keys = input.borrow().clone().expect("the pty opened");
                        keys.send(mu_base::events::PtyInput::Bytes(bytes.to_vec())).unwrap();
                        mu_testutil::wait_until("the wait to end", || flags.borrow().len() > 1).await;
                    }
                    cancel.cancel();
                };
                let _ = tokio::join!(tools.call("bash", &cmd, &ctx), driver);
                flags.take()
            })
            .await
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_pty_command_with_echo_off_is_reported_waiting_until_the_user_types() {
        assert_eq!(pty_waiting_flags("stty -echo; cat", Some(b"hi\n")).await, [true, false]);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_prompt_left_with_the_cursor_after_it_is_reported_after_two_seconds_of_silence() {
        assert_eq!(pty_waiting_flags("printf 'Continue? '; sleep 30", None).await, [true]);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "current_thread")]
    async fn a_process_blocked_reading_the_terminal_is_reported_waiting() {
        assert_eq!(pty_waiting_flags("cat", None).await, [true]);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_busy_pty_command_is_not_reported_waiting() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let tools = Tools::for_test();
                let (events, mut rx) = tokio::sync::mpsc::unbounded_channel();
                let seen = Rc::new(std::cell::Cell::new(false));
                let s = seen.clone();
                tokio::task::spawn_local(async move {
                    while let Some(e) = rx.recv().await {
                        match e {
                            AgentEvent::PtyOpen { reply, input, .. } => {
                                let _ = reply.send((80, 24));
                                std::mem::forget(input);
                            }
                            AgentEvent::PtyWaiting { .. } => s.set(true),
                            _ => {}
                        }
                    }
                });
                let ctx = ToolCtx {
                    events,
                    cancel: Default::default(),
                    call_id: "c".into(),
                    mode: None,
                    wait_interrupt: None,
                };
                let out = tools.call("bash", &json!({"command": "sleep 3; echo done", "pty": true}), &ctx).await;
                assert!(out.text.contains("done"), "{}", out.text);
                assert!(!seen.get());
            })
            .await;
    }

    /// A parked terminal request (no client attached) is not an answer: Cancel ends the wait.
    #[tokio::test(flavor = "current_thread")]
    async fn cancel_ends_the_wait_for_a_terminal_nobody_sizes() {
        let tools = Tools::for_test();
        let (events, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        let ctx = ToolCtx { events, cancel: cancel.clone(), call_id: "c".into(), mode: None, wait_interrupt: None };
        let stop = cancel.clone();
        let call = async { tools.call("bash", &json!({"command":"echo hi","pty":true}), &ctx).await };
        let parked = async {
            // The request is out and unanswered; then the user cancels.
            let ev = rx.recv().await;
            assert!(matches!(ev, Some(AgentEvent::PtyOpen { .. })));
            stop.cancel();
            ev
        };
        let started = Instant::now();
        let (out, _kept) = tokio::join!(call, parked);
        assert!(started.elapsed() < Duration::from_secs(10), "no wait for the 60 s limit");
        assert!(out.text.contains("pty unavailable"), "{}", out.text);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_background_run_reports_its_output_as_it_arrives() {
        let seen = std::cell::RefCell::new(String::new());
        let group = pgid();
        let r = run_background(
            "echo first; echo second >&2".into(),
            std::env::temp_dir(),
            std::env::vars().collect(),
            None,
            CancellationToken::new(),
            || (0, std::env::temp_dir()),
            |s, _| seen.borrow_mut().push_str(s),
            group.clone(),
        )
        .await;
        assert!(r.ok, "{}", r.text);
        assert!(group.load(AtomicOrdering::Relaxed) > 1, "the group is recorded for the port scan");
        let seen = seen.into_inner();
        assert!(seen.contains("first") && seen.contains("second"), "{seen:?}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_background_run_gives_the_model_clean_text_and_the_ui_raw_text() {
        let seen = std::cell::RefCell::new(String::new());
        let r = run_background(
            r"printf '\033[31mred\033[0m\n10%%\r100%%\n'; printf 'err\033[1m!\033[0m' >&2".into(),
            std::env::temp_dir(),
            std::env::vars().collect(),
            None,
            CancellationToken::new(),
            || (0, std::env::temp_dir()),
            |s, _| seen.borrow_mut().push_str(s),
            pgid(),
        )
        .await;
        assert!(r.ok, "{}", r.text);
        assert!(r.text.starts_with("red\n100%\nerr!"), "{:?}", r.text);
        assert!(!r.text.contains('\x1b') && !r.text.contains('\r'), "{:?}", r.text);
        assert!(seen.into_inner().contains("\x1b[31m"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn output_keeps_draining_while_a_cancelled_background_run_is_terminated() {
        // SIGKILL is held far off and the command ignores SIGTERM, so the run cannot end here: what
        // it can do is keep reading. The command says when it gets the SIGTERM; seeing that, the
        // test lets the command leave. A loop that waited out the termination would never read it.
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                mu_base::process::set_term_grace_for_test(None);
            }
        }
        mu_base::process::set_term_grace_for_test(Some(Duration::from_secs(300)));
        let _reset = Reset;
        let flag = std::env::temp_dir().join(format!("mu-drain-{}", uuid::Uuid::new_v4()));
        let command =
            format!("trap 'echo termed' TERM; while [ ! -e '{}' ]; do echo tick; sleep 0.02; done", flag.display());
        let cancel = CancellationToken::new();
        let termed = tokio::sync::Notify::new();
        let run = run_background(
            command,
            std::env::temp_dir(),
            std::env::vars().collect(),
            None,
            cancel.clone(),
            || (0, std::env::temp_dir()),
            |s, _| {
                if s.contains("termed") {
                    let _ = std::fs::write(&flag, "");
                    termed.notify_one();
                } else {
                    cancel.cancel();
                }
            },
            pgid(),
        );
        tokio::pin!(run);
        // The timeout only turns a hang into a failure; a pass does not depend on how fast it comes.
        let seen = tokio::time::timeout(Duration::from_secs(60), async {
            tokio::select! {
                r = &mut run => panic!("ended before the SIGTERM was read: {}", r.text),
                () = termed.notified() => {}
            }
        })
        .await;
        let _ = std::fs::remove_file(&flag);
        assert!(seen.is_ok(), "no output was read while the SIGTERM grace was pending");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_writing_descendant_does_not_hold_a_background_run_open() {
        let t = Instant::now();
        let r = run_background(
            "(while :; do echo tick; sleep 0.02; done) & echo leader-done".into(),
            std::env::temp_dir(),
            std::env::vars().collect(),
            Some(30.0),
            CancellationToken::new(),
            || (0, std::env::temp_dir()),
            |_, _| {},
            pgid(),
        )
        .await;
        assert!(t.elapsed() < Duration::from_secs(5), "held open for {:?}", t.elapsed());
        assert!(r.ok && r.note == "exit 0", "{} / {}", r.note, r.text);
        assert!(r.text.contains("leader-done"), "{}", r.text);
    }

    #[test]
    fn one_drain_policy_bounds_pty_and_piped_runs_alike() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let mut d = Drain::start();
            let start = d.cap_at - Drain::CAP;
            assert_eq!(d.ends_at(), start + Drain::IDLE, "a quiet pipe ends at the idle limit");
            // The real clock decides `over()` below, so the cap is moved out of reach of a stall.
            d.cap_at += Duration::from_secs(3600);
            // A steady writer keeps the idle limit moving but never past the cap.
            tokio::time::sleep(Duration::from_millis(50)).await;
            d.touch();
            assert!(d.ends_at() > start + Drain::IDLE);
            d.idle_at = d.cap_at + Duration::from_secs(10);
            assert_eq!(d.ends_at(), d.cap_at);
            assert!(!d.over());
        });
        assert!(Drain::IDLE < Drain::CAP);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_background_run_stops_reading_a_descendant_that_has_gone_quiet() {
        let t = Instant::now();
        let wrote = std::env::temp_dir().join(format!("mu-quiet-desc-{}", uuid::Uuid::new_v4()));
        let r = run_background(
            // Ignore TERM before forking: the leader's exit sends TERM at once, and a subshell
            // that has not yet run its own `trap` would die before it wrote anything. The leader
            // exits only after the subshell has written.
            format!(
                "trap '' TERM; (echo once; touch {w}; sleep 30) & while [ ! -f {w} ]; do sleep 0.02; done; echo leader-done",
                w = wrote.display()
            ),
            std::env::temp_dir(),
            std::env::vars().collect(),
            Some(60.0),
            CancellationToken::new(),
            || (0, std::env::temp_dir()),
            |_, _| {},
            pgid(),
        )
        .await;
        let _ = std::fs::remove_file(&wrote);
        // Uncapped, the quiet descendant would hold the pipe for its whole 30 s.
        assert!(t.elapsed() < Duration::from_secs(20), "drain took {:?}", t.elapsed());
        assert!(r.ok && r.text.contains("leader-done") && r.text.contains("once"), "{}", r.text);
    }

    #[test]
    fn a_stream_holds_its_own_unfinished_line_and_flushes_it_at_the_end() {
        let mut a = Stream::default();
        let mut b = Stream::default();
        assert_eq!(a.push(b"par").1, "");
        assert_eq!(b.push(b"other\n").1, "other\n");
        assert_eq!(a.push(b"tial").1, "");
        assert_eq!(a.finish().1, "partial");
    }

    #[test]
    fn a_writing_descendant_does_not_hold_a_pty_run_open_and_cancel_ends_the_drain() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(tokio::task::LocalSet::new().run_until(async {
            let tools = Tools::for_test();
            let (events, mut rx) = tokio::sync::mpsc::unbounded_channel();
            tokio::task::spawn_local(async move {
                while let Some(e) = rx.recv().await {
                    if let AgentEvent::PtyOpen { input, reply, .. } = e {
                        let _ = reply.send((80, 24));
                        std::mem::forget(input);
                    }
                }
            });
            let cmd = json!({
                "command": "trap '' TERM; (trap '' TERM; while :; do echo tick; sleep 0.01; done) & echo leader-done",
                "pty": true,
                "timeout": 30
            });
            let ctx =
                ToolCtx { events, cancel: Default::default(), call_id: "c".into(), mode: None, wait_interrupt: None };
            let t = Instant::now();
            let out = tools.call("bash", &cmd, &ctx).await;
            assert!(t.elapsed() < Duration::from_secs(5), "held open for {:?}", t.elapsed());
            assert!(!out.is_error, "{}", out.text.chars().take(200).collect::<String>());
            assert!(out.text.contains("leader-done"), "{}", out.text.chars().take(200).collect::<String>());

            let cancel = CancellationToken::new();
            let ctx = ToolCtx { cancel: cancel.clone(), ..ctx };
            let canceller = async {
                tokio::time::sleep(Duration::from_millis(300)).await;
                cancel.cancel();
            };
            let t = Instant::now();
            let (out, ()) = tokio::join!(tools.call("bash", &cmd, &ctx), canceller);
            assert!(t.elapsed() < Duration::from_secs(5), "cancel took {:?}", t.elapsed());
            drop(out);
        }));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn session_env_reaches_commands() {
        let tools = Tools::for_test();
        let (events, _rx) = tokio::sync::mpsc::unbounded_channel();
        let ctx = ToolCtx { events, cancel: Default::default(), call_id: "c".into(), mode: None, wait_interrupt: None };
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
        let ctx = ToolCtx { events, cancel: Default::default(), call_id: "c".into(), mode: None, wait_interrupt: None };
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
        let ctx = ToolCtx { events, cancel: Default::default(), call_id: "c".into(), mode: None, wait_interrupt: None };
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

    /// A file for a test process to write its pid to, and the command that does: it records its
    /// own pid, then becomes `sleep 600`. Tests follow the process by that pid, never by pattern.
    fn sleeper() -> (PathBuf, String) {
        let file = std::env::temp_dir().join(format!("mu-bash-pid-{}", uuid::Uuid::new_v4()));
        let cmd = format!("/bin/sh -c 'echo $$ > {}; exec /bin/sleep 600'", file.display());
        (file, cmd)
    }

    fn kill_pid(pid: i32) {
        // SAFETY: SIGKILL to a pid this test started.
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }

    #[tokio::test(flavor = "current_thread")]
    async fn background_processes_in_plain_mode() {
        let tools = Tools::for_test();
        let (quiet_file, quiet) = sleeper();
        let (kept_file, kept) = sleeper();
        let cmd = format!("({quiet} &); nohup {kept} >/dev/null 2>&1 & echo started");
        let out = tools.call("bash", &json!({"command": cmd}), &ctx()).await;
        assert!(out.text.contains("started"), "{}", out.text);
        // A subshell's background external outlives the call, as it does in bash (oh-my-pi v18.18
        // stopped ending it), and so does a `nohup` job, which omp-shell detaches from the session.
        let (quiet_pid, kept_pid) = (mu_testutil::pid_in(&quiet_file).await, mu_testutil::pid_in(&kept_file).await);
        assert!(mu_testutil::pid_running(quiet_pid), "the subshell's job did not outlive the call");
        assert!(mu_testutil::pid_running(kept_pid), "the nohup job did not outlive the call");
        // The subshell's job is not in the table, so nothing mu exposes can stop it; `nohup … &` is.
        let jobs = tools.call("bash", &json!({"command": "jobs"}), &ctx()).await;
        let (q, k) = (quiet_file.display().to_string(), kept_file.display().to_string());
        assert!(!jobs.text.contains(&q) && jobs.text.contains(&k), "{}", jobs.text);
        kill_pid(quiet_pid);
        kill_pid(kept_pid);
    }

    /// mu's runtime is current-thread: the persistent shell must still reap a finished `&` child
    /// while idle, and keep its status for `wait` in a later call.
    #[tokio::test(flavor = "current_thread")]
    async fn finished_background_job_is_reaped_and_keeps_its_status() {
        let tools = Tools::for_test();
        let (events, _rx) = tokio::sync::mpsc::unbounded_channel();
        let ctx = ToolCtx { events, cancel: Default::default(), call_id: "c".into(), mode: None, wait_interrupt: None };
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

    /// `$!` of a background pipeline is its last program's OS pid (a virtual pid before oh-my-pi
    /// v18.18.6), so a later call can `kill` and `wait` that program by the pid it saved.
    #[tokio::test(flavor = "current_thread")]
    async fn a_background_pipelines_pid_is_its_last_program() {
        let (file, last) = sleeper();
        let tools = Tools::for_test();
        let ctx = ctx();
        let cmd = format!("/bin/sleep 0.2 | {last} & echo $!");
        let out = tools.call("bash", &json!({"command": cmd}), &ctx).await;
        let pid: i32 = out.text.trim().parse().unwrap_or_else(|_| panic!("no pid: {}", out.text));
        let os_pid = mu_testutil::pid_in(&file).await;
        if os_pid != pid {
            kill_pid(os_pid);
            panic!("$! was {pid}, the last program is {os_pid}");
        }
        let out =
            tools.call("bash", &json!({"command": format!("kill {pid}; wait {pid}; echo status=$?")}), &ctx).await;
        assert!(out.text.contains("status=143"), "{}", out.text);
        mu_testutil::wait_gone(pid).await;
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
            let ctx =
                ToolCtx { events, cancel: Default::default(), call_id: "c".into(), mode: None, wait_interrupt: None };
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
        let ctx = ToolCtx { events, cancel: Default::default(), call_id: "c".into(), mode: None, wait_interrupt: None };
        let out = tools.call("bash", &json!({"command": "echo hi; exit 3"}), &ctx).await;
        let Some(ToolMeta::Bash(m)) = &out.meta else { panic!("no meta: {:?}", out.text) };
        assert_eq!(m.exit_code, Some(3));
        assert!(out.text[..m.output_end].contains("hi"));
        assert!(!out.text[..m.output_end].contains("Command exited"));
        assert!(out.text.ends_with("Command exited with code 3"), "the model's text is unchanged: {}", out.text);
        let quiet = tools.call("bash", &json!({"command": "true"}), &ctx).await;
        let Some(ToolMeta::Bash(q)) = &quiet.meta else { panic!() };
        assert_eq!((q.output_end, q.exit_code), (0, None));
        assert_eq!(quiet.text, "(no output, exit code 0)");
    }

    /// A command that prints nothing says whether it succeeded; a failed one keeps the code.
    #[tokio::test(flavor = "current_thread")]
    async fn a_silent_command_reports_its_exit_code() {
        let tools = Tools::for_test();
        let out = tools.call("bash", &json!({"command": "true"}), &ctx()).await;
        assert_eq!(out.text, "(no output, exit code 0)");
        let out = tools.call("bash", &json!({"command": "exit 3"}), &ctx()).await;
        assert_eq!(out.text, "(no output)\n\nCommand exited with code 3");
    }

    /// Aborting a foreground command ends the call promptly and kills the command, also after
    /// earlier calls have run in the persistent shell.
    #[tokio::test(flavor = "current_thread")]
    async fn abort_ends_a_foreground_command_promptly() {
        let (file, sleep) = sleeper();
        let tools = Tools::for_test();
        for cmd in ["sleep 0.2; echo done", "date"] {
            assert!(!tools.call("bash", &json!({"command": cmd}), &ctx()).await.is_error);
        }
        let c = ctx();
        let cancel = c.cancel.clone();
        let t = std::time::Instant::now();
        let pid = std::cell::Cell::new(0);
        let abort = async {
            pid.set(mu_testutil::pid_in(&file).await);
            cancel.cancel();
        };
        let cmd = json!({"command": sleep});
        let (out, ()) = tokio::join!(tools.call("bash", &cmd, &c), abort);
        assert!(t.elapsed() < std::time::Duration::from_secs(5), "abort took {:?}", t.elapsed());
        let Some(ToolMeta::Bash(m)) = &out.meta else { panic!("no meta: {:?}", out.text) };
        assert!(m.cancelled, "{}", out.text);
        mu_testutil::wait_gone(pid.get()).await;
        let next = tools.call("bash", &json!({"command": "echo after"}), &ctx()).await;
        assert_eq!(next.text.trim(), "after");
    }

    /// A shell whose threads are all stuck (a builtin blocked in a syscall) does not freeze the caller:
    /// the call ends once its timeout and the grace pass, or at once on abort, and the next call gets a
    /// working shell.
    #[tokio::test(flavor = "current_thread")]
    async fn a_hung_shell_does_not_freeze_the_caller() {
        // Only a hang becomes a failure; how long the abandonment takes is not part of the pass.
        tokio::time::timeout(std::time::Duration::from_secs(120), async {
            let tools = Tools::for_test();
            tools.shell().jam(30);
            let out = tools.call("bash", &json!({"command": "true", "timeout": 1}), &ctx()).await;
            assert!(out.is_error && out.text.contains("stopped responding"), "{}", out.text);
            assert_eq!(tools.call("bash", &json!({"command": "echo back"}), &ctx()).await.text.trim(), "back");

            tools.shell().jam(30);
            let c = ctx();
            let cancel = c.cancel.clone();
            let abort = async {
                tokio::task::yield_now().await;
                cancel.cancel();
            };
            let cmd = json!({"command": "true"});
            let (out, ()) = tokio::join!(tools.call("bash", &cmd, &c), abort);
            let Some(ToolMeta::Bash(m)) = &out.meta else { panic!("no meta: {:?}", out.text) };
            assert!(m.cancelled, "{}", out.text);
            assert_eq!(tools.call("bash", &json!({"command": "echo again"}), &ctx()).await.text.trim(), "again");
        })
        .await
        .expect("the caller was frozen by a hung shell");
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
        ToolCtx { events, cancel: Default::default(), call_id: "c".into(), mode: None, wait_interrupt: None }
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
                assert!(
                    matches!(&out.meta, Some(ToolMeta::Bash(BashMeta { background: Some(id), .. })) if id == "bg_1"),
                    "{:?}",
                    out.meta
                );
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
    async fn notify_reports_matching_output_while_the_job_runs() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (tools, jobs) = tools_with_jobs();
                let cmd =
                    json!({"command": "echo booting; echo READY on 8080; sleep 30", "async": true, "notify": "^READY"});
                let out = tools.call("bash", &cmd, &ctx()).await;
                assert!(!out.is_error && out.text.contains("`notify`"), "{}", out.text);
                mu_testutil::wait_until("the match", || jobs.has_matches()).await;
                let m = jobs.take_matches();
                assert_eq!(m[0].lines, vec!["READY on 8080".to_string()]);
                assert!(!jobs.has_undelivered(), "the job itself is still running");
                jobs.kill(None);
                finished(&jobs).await;
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn an_invalid_notify_regex_is_refused_before_the_job_starts() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (tools, jobs) = tools_with_jobs();
                let out = tools.call("bash", &json!({"command": "true", "async": true, "notify": "("}), &ctx()).await;
                assert!(out.is_error && out.text.contains("notify: invalid regex"), "{}", out.text);
                assert_eq!(jobs.running(), 0);
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn timeout_kills_the_background_command() {
        let (file, sleep) = sleeper();
        tokio::task::LocalSet::new()
            .run_until(async {
                let (tools, jobs) = tools_with_jobs();
                let cmd = json!({"command": sleep, "async": true, "timeout": 2});
                tools.call("bash", &cmd, &ctx()).await;
                let done = finished(&jobs).await;
                assert!(!done[0].ok && done[0].note.as_deref() == Some("timed out"), "{:?}", done[0].note);
                assert!(done[0].text.contains("[Command timed out after 2 seconds]"), "{}", done[0].text);
                mu_testutil::wait_gone(mu_testutil::pid_in(&file).await).await;
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancel_kills_the_background_command() {
        let (file, sleep) = sleeper();
        tokio::task::LocalSet::new()
            .run_until(async {
                let (tools, jobs) = tools_with_jobs();
                tools.call("bash", &json!({"command": sleep, "async": true}), &ctx()).await;
                let pid = mu_testutil::pid_in(&file).await;
                assert_eq!(jobs.kill(None), 1);
                let done = finished(&jobs).await;
                assert!(!done[0].ok && done[0].note.as_deref() == Some("cancelled"), "{:?}", done[0].note);
                mu_testutil::wait_gone(pid).await;
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
        assert!(matches!(&out.meta, Some(ToolMeta::Bash(BashMeta { background: None, .. }))), "{:?}", out.meta);
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

    fn fresh_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mu-setcwd-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d.canonicalize().unwrap()
    }

    /// The persistent shell (no jobs: headless runs) keeps its own directory between calls, so a
    /// switch must move it, whatever an earlier `cd` left it in.
    #[tokio::test(flavor = "current_thread")]
    async fn a_switch_moves_the_persistent_shell() {
        let mut tools = Tools::for_test();
        let (old, new) = (fresh_dir("old"), fresh_dir("new"));
        tools.set_cwd(old.clone());
        let pwd = json!({"command": "pwd"});
        assert!(!tools.call("bash", &json!({"command": "cd /usr"}), &ctx()).await.is_error);
        tools.set_cwd(new.clone());
        let out = tools.call("bash", &pwd, &ctx()).await;
        assert_eq!(out.text.trim(), new.to_string_lossy(), "{}", out.text);
        // It stays there: the move is made once, not before every call.
        tools.call("bash", &json!({"command": "cd /usr"}), &ctx()).await;
        assert_eq!(tools.call("bash", &pwd, &ctx()).await.text.trim(), "/usr");
        let _ = (std::fs::remove_dir_all(&old), std::fs::remove_dir_all(&new));
    }

    /// With jobs (interactive sessions) a foreground call starts in `Tools.cwd`, and a background
    /// job starts where the persistent shell is: both must be the new directory.
    #[tokio::test(flavor = "current_thread")]
    async fn a_switch_moves_foreground_calls_and_new_jobs_in_an_interactive_session() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (mut tools, jobs) = tools_with_jobs();
                let new = fresh_dir("jobs");
                tools.call("bash", &json!({"command": "cd /usr"}), &ctx()).await;
                tools.set_cwd(new.clone());
                let fg = tools.call("bash", &json!({"command": "pwd"}), &ctx()).await;
                assert_eq!(fg.text.trim(), new.to_string_lossy(), "{}", fg.text);
                tools.call("bash", &json!({"command": "pwd", "async": true}), &ctx()).await;
                let done = finished(&jobs).await;
                assert_eq!(done[0].text.trim(), new.to_string_lossy(), "{}", done[0].text);
                let _ = std::fs::remove_dir_all(&new);
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

    /// A foreground command running in the persistent shell does not hide its earlier state from a
    /// job: the job's snapshot comes from a fork, so it neither waits for the command nor sees the
    /// variables that command exported.
    #[tokio::test(flavor = "current_thread")]
    async fn a_busy_shell_still_hands_its_state_to_a_job() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (tools, jobs) = tools_with_jobs();
                let (setup, c) = (json!({"command": "cd /usr && export MU_FORK_V=kept"}), ctx());
                assert!(!tools.call("bash", &setup, &c).await.is_error);
                let started = std::env::temp_dir().join(format!("mu-bash-busy-{}", uuid::Uuid::new_v4()));
                let release = started.with_extension("release");
                let busy_cmd = format!(
                    "export MU_FORK_V=during; touch {}; while [ ! -f {} ]; do sleep 0.02; done",
                    started.display(),
                    release.display()
                );
                let (busy, c1) = (json!({"command": busy_cmd}), ctx());
                let (job, c2) = (json!({"command": r#"printf '%s %s' "$PWD" "$MU_FORK_V""#, "async": true}), ctx());
                let start = async {
                    mu_testutil::wait_until("the busy command to start", || started.exists()).await;
                    // The busy command holds until the job call has returned, so the snapshot cannot
                    // have waited for it; if it did, the release never comes and the backstop fails.
                    let out = tokio::time::timeout(std::time::Duration::from_secs(60), tools.call("bash", &job, &c2))
                        .await
                        .expect("the snapshot waited for the busy command");
                    std::fs::write(&release, "").unwrap();
                    out
                };
                let (_, out) = tokio::join!(tools.call("bash", &busy, &c1), start);
                assert!(!out.text.contains("was not inherited"), "{}", out.text);
                let done = finished(&jobs).await;
                assert!(done[0].ok, "{}", done[0].text);
                assert_eq!(done[0].text, "/usr kept", "state from before the busy command: {}", done[0].text);
            })
            .await;
    }

    /// A shell whose threads are all stuck cannot answer a snapshot: the job still runs, with mu's
    /// own environment, and the call says so.
    #[tokio::test(flavor = "current_thread")]
    async fn a_stuck_shell_falls_back_to_mu_environment_and_says_so() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (tools, jobs) = tools_with_jobs();
                tools.shell().jam(30);
                let (job, c) = (json!({"command": "echo fallback-ran", "async": true}), ctx());
                let out = tools.call("bash", &job, &c).await;
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

    type Events = (EventTx, tokio::sync::mpsc::UnboundedReceiver<AgentEvent>);

    fn with_jobs() -> (Tools, Rc<crate::jobs::JobsHandle>, Events) {
        let mut tools = Tools::for_test();
        let (wake, _wake_rx) = tokio::sync::mpsc::unbounded_channel();
        let (events, rx) = tokio::sync::mpsc::unbounded_channel();
        let jobs = crate::jobs::JobsHandle::new(wake, events.clone());
        tools.jobs = Some(jobs.clone());
        (tools, jobs, (events, rx))
    }

    fn wait_ctx(
        jobs: &Rc<crate::jobs::JobsHandle>,
        events: EventTx,
        cancel: CancellationToken,
    ) -> (ToolCtx, crate::jobs::ActiveWait) {
        let wait = jobs.begin_wait();
        (ToolCtx { events, cancel, call_id: "c".into(), mode: None, wait_interrupt: Some(wait.token()) }, wait)
    }

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mu-fg-{name}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d.canonicalize().unwrap()
    }

    /// Polls `events` until a job output contains `needle`, or panics once `call` finishes first.
    async fn until_output<F: Future<Output = ToolOutput>>(
        mut call: std::pin::Pin<&mut F>,
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<AgentEvent>,
        needle: &str,
    ) {
        loop {
            tokio::select! {
                out = call.as_mut() => panic!("the call finished before `{needle}`: {}", out.text),
                ev = rx.recv() => {
                    if let Some(AgentEvent::JobOutput { tail, .. }) = ev
                        && tail.contains(needle)
                    {
                        return;
                    }
                }
            }
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_foreground_call_returns_its_output_and_claims_its_result() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (tools, jobs, (events, _rx)) = with_jobs();
                let (ctx, _wait) = wait_ctx(&jobs, events, CancellationToken::new());
                let out = tools.call("bash", &json!({"command": "printf 'finished\\n'"}), &ctx).await;
                assert!(!out.is_error && out.text.contains("finished"), "{}", out.text);
                assert!(matches!(&out.meta, Some(ToolMeta::Bash(m)) if m.background.is_none()), "{:?}", out.meta);
                assert!(jobs.snapshot().is_empty(), "the call's job is not listed as a background job");
                assert!(!jobs.has_undelivered(), "the call claimed the job's result");
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_foreground_call_streams_its_output_to_its_tool_call() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let dir = tmp("stream");
                let (mut tools, jobs, (events, mut rx)) = with_jobs();
                tools.cwd = dir.clone();
                let (ctx, _wait) = wait_ctx(&jobs, events, CancellationToken::new());
                let args = json!({"command": "printf 'started\\n'; while [ ! -f release ]; do sleep 0.02; done"});
                let call = tools.call("bash", &args, &ctx);
                tokio::pin!(call);
                let mut streamed = String::new();
                loop {
                    tokio::select! {
                        out = call.as_mut() => panic!("the call finished before its output streamed: {}", out.text),
                        ev = rx.recv() => {
                            if let Some(AgentEvent::ToolOutput { id, chunk }) = ev {
                                assert_eq!(id, "c", "the output streams to the waiting call");
                                streamed.push_str(&chunk);
                                if streamed.contains("started") {
                                    break;
                                }
                            }
                        }
                    }
                }
                std::fs::write(dir.join("release"), "go").unwrap();
                let out = call.await;
                assert!(!out.is_error, "{}", out.text);
                let _ = std::fs::remove_dir_all(&dir);
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_foreground_call_does_not_inherit_shell_state() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let dir = tmp("isolated");
                std::fs::create_dir_all(dir.join("sub")).unwrap();
                let (mut tools, jobs, (events, _rx)) = with_jobs();
                tools.cwd = dir.clone();
                let (ctx, _wait) = wait_ctx(&jobs, events, CancellationToken::new());
                let a = json!({"command": "export MU_WAIT_TEST_VALUE=set; cd sub"});
                assert!(!tools.call("bash", &a, &ctx).await.is_error);
                let b = json!({"command": "printf '%s\\n' \"${MU_WAIT_TEST_VALUE-unset}\"; pwd"});
                let out = tools.call("bash", &b, &ctx).await;
                assert!(out.text.starts_with("unset\n"), "{}", out.text);
                assert!(out.text.contains(&dir.display().to_string()), "{}", out.text);
                let c = json!({"command": "printf '%s\\n' \"$PWD\"", "cwd": "sub"});
                let out = tools.call("bash", &c, &ctx).await;
                assert!(out.text.contains(&dir.join("sub").display().to_string()), "{}", out.text);
                let _ = std::fs::remove_dir_all(&dir);
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_prompt_interrupt_backgrounds_the_command_and_its_result_arrives_once() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let dir = tmp("interrupt");
                let (mut tools, jobs, (events, mut rx)) = with_jobs();
                tools.cwd = dir.clone();
                let (ctx, wait) = wait_ctx(&jobs, events, CancellationToken::new());
                let args = json!({"command": "printf 'started\\n'; while [ ! -f release ]; do sleep 0.02; done; printf 'finished\\n'"});
                let call = tools.call("bash", &args, &ctx);
                tokio::pin!(call);
                until_output(call.as_mut(), &mut rx, "started").await;
                assert!(jobs.snapshot().is_empty(), "a waiting call's command is not listed as a background job");
                wait.token().cancel();
                let out = call.await;
                assert!(!out.is_error, "{}", out.text);
                assert!(out.text.starts_with("Command continues as background job "), "{}", out.text);
                assert!(matches!(&out.meta, Some(ToolMeta::Bash(m)) if m.background.is_some()), "{:?}", out.meta);
                assert_eq!(jobs.running(), 1, "the work keeps running");
                assert_eq!(jobs.snapshot().len(), 1, "listed as a background job once the prompt released the wait");
                assert_eq!(jobs.snapshot()[0].kind, mu_base::events::JobKind::Bash);
                assert!(!jobs.has_undelivered(), "no result before it finishes");
                std::fs::write(dir.join("release"), "go").unwrap();
                let done = jobs.finished().await;
                assert_eq!(done.len(), 1);
                assert!(done[0].text.contains("finished"), "{}", done[0].text);
                assert!(!jobs.has_undelivered(), "exactly one result");
                while let Ok(ev) = rx.try_recv() {
                    assert!(
                        !matches!(&ev, AgentEvent::ToolOutput { id, chunk } if id == "c" && chunk.contains("finished")),
                        "a released call's later output is not streamed to the finished call"
                    );
                }
                let _ = std::fs::remove_dir_all(&dir);
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancelling_a_foreground_call_terminates_and_claims_its_command() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (tools, jobs, (events, mut rx)) = with_jobs();
                let cancel = CancellationToken::new();
                let (ctx, _wait) = wait_ctx(&jobs, events, cancel.clone());
                let args = json!({"command": "printf 'started\\n'; sleep 30; printf 'done\\n'"});
                let call = tools.call("bash", &args, &ctx);
                tokio::pin!(call);
                until_output(call.as_mut(), &mut rx, "started").await;
                cancel.cancel();
                let out = call.await;
                assert!(out.is_error && out.text.contains("[Command cancelled]"), "{}", out.text);
                assert!(!jobs.has_undelivered(), "the cancelled result was claimed, not announced");
                assert_eq!(jobs.running(), 0);
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_cancelled_call_never_launches_and_a_dropped_call_kills_its_job() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (tools, jobs, (events, mut rx)) = with_jobs();
                let cancelling = CancellationToken::new();
                cancelling.cancel();
                let (ctx, _wait) = wait_ctx(&jobs, events.clone(), cancelling);
                let out = tools.call("bash", &json!({"command": "printf 'ran\\n'"}), &ctx).await;
                assert!(out.is_error && out.text == "Command cancelled before launch.", "{}", out.text);
                assert_eq!(jobs.snapshot().len(), 0, "no job was created");

                let dir = tmp("dropped");
                let mut tools = tools;
                tools.cwd = dir.clone();
                let (ctx, _wait) = wait_ctx(&jobs, events, CancellationToken::new());
                let args = json!({"command": "printf 'started\\n'; while [ ! -f release ]; do sleep 0.02; done"});
                let mut call = Box::pin(tools.call("bash", &args, &ctx));
                until_output(call.as_mut(), &mut rx, "started").await;
                drop(call);
                let killed = tokio::time::timeout(std::time::Duration::from_secs(10), async {
                    while jobs.running() > 0 {
                        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                    }
                })
                .await;
                assert!(killed.is_ok(), "the dropped call's job is cancelled");
                assert!(!jobs.has_undelivered(), "no surprise notice for a dropped call");
                let _ = std::fs::remove_dir_all(&dir);
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_foreground_call_has_its_own_slot_outside_the_background_cap() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (tools, jobs, (events, mut rx)) = with_jobs();
                let (ctx, _wait) = wait_ctx(&jobs, events, CancellationToken::new());
                for i in 0..crate::tools::task::MAX_TASKS {
                    jobs.push_running(&format!("full{i}"));
                }
                let out = tools.call("bash", &json!({"command": "printf 'yes\\n'"}), &ctx).await;
                assert!(
                    !out.is_error && out.text.contains("yes"),
                    "a full house still runs a foreground call: {}",
                    out.text
                );
                let out = tools.call("bash", &json!({"command": "echo no", "async": true}), &ctx).await;
                assert!(out.is_error && out.text.contains("too many background jobs running (8/8)"), "{}", out.text);
                jobs.clear();

                for i in 0..crate::tools::task::MAX_TASKS - 1 {
                    jobs.push_running(&format!("seven{i}"));
                }
                let args = json!({"command": "printf 'started\\n'; sleep 30; printf 'done\\n'"});
                let mut call = Box::pin(tools.call("bash", &args, &ctx));
                until_output(call.as_mut(), &mut rx, "started").await;
                assert_eq!(jobs.running(), crate::tools::task::MAX_TASKS, "the foreground job is a running job");
                assert!(jobs.check_capacity(1).is_ok(), "but it takes no background slot");
                assert!(jobs.check_capacity(2).is_err());
                assert_eq!(jobs.snapshot().len(), crate::tools::task::MAX_TASKS - 1, "and is not listed");
                assert_eq!(jobs.kill(None), crate::tools::task::MAX_TASKS);
                let out = call.await;
                assert!(out.is_error && out.text.contains("[Command cancelled]"), "{}", out.text);
                assert!(!jobs.has_undelivered(), "the cancelled result stays with the call");
            })
            .await;
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
