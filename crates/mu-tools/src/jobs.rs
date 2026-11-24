//! Background jobs: `task` (subagents) and `bash` with `async` return ids at once; finished results
//! are delivered to the model as a `<system-notice>` message.

use std::cell::{Cell, RefCell, RefMut};
use std::collections::HashSet;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant, SystemTime};

use serde_json::Value;
use tokio::sync::Notify;
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

use crate::permissions::Permissions;
use crate::tools::relay::RelayServer;
use crate::tools::subagent::{BackgroundResult, ChildCmd, Mirror, Outcome, preview_result, run_child_observed};
use crate::tools::task::MAX_TASKS;
use mu_base::events::{AgentEvent, EventTx, JobChanges, JobKind, SubagentState, SubagentStatus};
use mu_base::text::truncate_text;
use mu_base::tool::BashMeta;

/// Newest bytes of a running bash job's output kept for the jobs panel.
const OUTPUT_KEEP: usize = 64 * 1024;
/// How much of that goes to the UI per update, and how often at most.
const OUTPUT_EMIT: usize = 16 * 1024;
const OUTPUT_EVERY: Duration = Duration::from_millis(250);
/// How often a running bash job's listening ports are looked up.
const PORT_EVERY: Duration = Duration::from_secs(2);

thread_local! {
    /// A port scan is already running.
    static PORT_SCAN: Cell<bool> = const { Cell::new(false) };
}
/// Output lines that match a job's `notify` pattern within this long of each other go out as one notice.
const NOTIFY_BATCH: Duration = Duration::from_millis(200);
/// Most lines, and most characters per line, one notice carries.
const NOTICE_LINES: usize = 50;
const NOTICE_LINE_CHARS: usize = 500;

/// Lines of a running job's output that matched its `notify` pattern, not yet handed to the model.
pub struct Matched {
    pub id: String,
    pub pattern: String,
    pub lines: Vec<String>,
}

/// A job's `notify` pattern and the matching lines waiting for the batch window to close.
struct Watch {
    re: regex::Regex,
    pattern: String,
    jobs: Weak<JobsHandle>,
    pending: Vec<String>,
    scheduled: bool,
    /// The job has ended: its result follows, so matches not yet sent are dropped.
    closed: bool,
}

/// Where a running bash job's output goes: a bounded buffer whose newest part is sent to the UI as
/// `AgentEvent::JobOutput` (redacted, throttled), so the panel can show a job while it runs.
#[derive(Clone)]
pub struct OutputSink(Rc<SinkInner>);

struct SinkInner {
    id: String,
    cwd: PathBuf,
    events: EventTx,
    /// The tool call of a foreground call that waits on this job: its output streams there too, so
    /// the call's card shows progress. Cleared once the call stops waiting.
    call: RefCell<Option<String>>,
    buf: RefCell<String>,
    last: Cell<Option<Instant>>,
    /// A deferred update is already waiting.
    pending: Cell<bool>,
    /// The process group of the command behind it; 0 until the shell has started.
    pgid: Arc<AtomicU32>,
    /// The throttle window; a field so tests need not race the real clock.
    every: Duration,
    watch: RefCell<Option<Watch>>,
}

impl OutputSink {
    fn new(id: String, cwd: PathBuf, events: EventTx, call: Option<String>) -> Self {
        Self::with_every(id, cwd, events, call, OUTPUT_EVERY)
    }

    fn with_every(id: String, cwd: PathBuf, events: EventTx, call: Option<String>, every: Duration) -> Self {
        Self(Rc::new(SinkInner {
            id,
            cwd,
            events,
            call: RefCell::new(call),
            buf: RefCell::default(),
            last: Cell::new(None),
            pending: Cell::new(false),
            pgid: Arc::new(AtomicU32::new(0)),
            every,
            watch: RefCell::new(None),
        }))
    }

    /// The process group of the command, once it has started.
    pub fn pgid(&self) -> Arc<AtomicU32> {
        self.0.pgid.clone()
    }

    /// Reports output lines that match `re` to the model through `jobs`.
    pub fn watch(&self, re: regex::Regex, jobs: Weak<JobsHandle>) {
        let pattern = re.as_str().to_string();
        *self.0.watch.borrow_mut() =
            Some(Watch { re, pattern, jobs, pending: Vec::new(), scheduled: false, closed: false });
    }

    /// Cleaned output (whole lines): the lines that match the watch pattern are batched for a notice.
    pub fn lines(&self, clean: &str) {
        let mut guard = self.0.watch.borrow_mut();
        let Some(w) = guard.as_mut().filter(|w| !w.closed) else { return };
        for line in clean.lines() {
            if w.re.is_match(line) {
                w.pending.push(line.to_string());
            }
        }
        if w.pending.is_empty() || w.scheduled {
            return;
        }
        w.scheduled = true;
        let me = self.clone();
        tokio::task::spawn_local(async move {
            tokio::time::sleep(NOTIFY_BATCH).await;
            me.send_matches();
        });
    }

    fn send_matches(&self) {
        let (lines, pattern, jobs) = {
            let mut guard = self.0.watch.borrow_mut();
            let Some(w) = guard.as_mut() else { return };
            w.scheduled = false;
            if w.closed || w.pending.is_empty() {
                return;
            }
            (std::mem::take(&mut w.pending), w.pattern.clone(), w.jobs.clone())
        };
        let Some(jobs) = jobs.upgrade() else { return };
        let lines = mu_base::secrets::redact(&lines.join("\n"), &self.0.cwd).text.lines().map(str::to_string).collect();
        jobs.push_matches(Matched { id: self.0.id.clone(), pattern, lines });
    }

    /// The job has ended: matches still waiting are dropped, since its result follows.
    pub fn close(&self) {
        if let Some(w) = self.0.watch.borrow_mut().as_mut() {
            w.closed = true;
            w.pending.clear();
        }
    }

    /// Stops streaming to the call that was waiting: the job runs on as a background job.
    pub fn detach(&self) {
        self.0.call.borrow_mut().take();
    }

    /// Adds output. The first chunk after a quiet spell is sent at once; later ones wait for the
    /// throttle, so the last chunk is never left unsent.
    pub fn push(&self, s: &str) {
        let i = &self.0;
        if let Some(call) = i.call.borrow().as_ref() {
            let _ = i.events.send(AgentEvent::ToolOutput { id: call.clone(), chunk: s.to_string() });
        }
        {
            let mut b = i.buf.borrow_mut();
            b.push_str(s);
            if b.len() > OUTPUT_KEEP {
                let mut cut = b.len() - OUTPUT_KEEP;
                while !b.is_char_boundary(cut) {
                    cut += 1;
                }
                b.drain(..cut);
            }
        }
        if i.pending.get() {
            return;
        }
        match i.last.get().map(|t| t.elapsed()) {
            Some(since) if since < i.every => {
                i.pending.set(true);
                let me = self.clone();
                let wait = i.every - since;
                tokio::task::spawn_local(async move {
                    tokio::time::sleep(wait).await;
                    me.0.pending.set(false);
                    me.flush();
                });
            }
            _ => self.flush(),
        }
    }

    /// Sends the newest output now (nothing when there is none).
    pub fn flush(&self) {
        let i = &self.0;
        let b = i.buf.borrow();
        if b.is_empty() {
            return;
        }
        let mut from = b.len().saturating_sub(OUTPUT_EMIT);
        while !b.is_char_boundary(from) {
            from += 1;
        }
        let tail = mu_base::secrets::redact(&b[from..], &i.cwd).text;
        i.last.set(Some(Instant::now()));
        let _ = i.events.send(AgentEvent::JobOutput { id: i.id.clone(), tail });
    }
}

/// What a job is: its id, who runs it, where, and what it was asked to do.
struct Spec {
    id: String,
    agent: String,
    cwd: PathBuf,
    /// The task prompt (truncated) or the command (whole up to `COMMAND_KEEP`); kept for the jobs panel.
    task: String,
}

/// What differs between the kinds of job.
enum Work {
    Subagent {
        tools: u32,
        /// The child's session file, once it has opened it.
        session: Option<PathBuf>,
        model: Option<String>,
        /// What a launched worker left, once it has finished.
        changes: Option<JobChanges>,
        /// The remote's latest start step, until the worker is connected.
        progress: Option<String>,
    },
    Bash {
        /// Taken on delivery, like `text`.
        command: Option<String>,
        meta: Option<BashMeta>,
        /// How it ended ("exit 3", "timed out", …).
        note: Option<String>,
        /// The command's process group, once its shell is running (0 until then).
        pgid: Arc<AtomicU32>,
    },
}

impl Work {
    fn subagent() -> Self {
        Work::Subagent { tools: 0, session: None, model: None, changes: None, progress: None }
    }

    fn bash(command: String, pgid: Arc<AtomicU32>) -> Self {
        Work::Bash { command: Some(command), meta: None, note: None, pgid }
    }

    fn pgid(&self) -> Option<&Arc<AtomicU32>> {
        match self {
            Work::Bash { pgid, .. } => Some(pgid),
            Work::Subagent { .. } => None,
        }
    }

    fn kind(&self) -> JobKind {
        match self {
            Work::Subagent { .. } => JobKind::Subagent,
            Work::Bash { .. } => JobKind::Bash,
        }
    }
}

struct Job {
    /// Distinguishes a job from a later one that reuses its id after `clear`.
    seq: u64,
    spec: Spec,
    work: Work,
    last: String,
    state: SubagentState,
    text: String,
    started: SystemTime,
    ended: Option<SystemTime>,
    delivered: bool,
    notice_owned: bool,
    /// Counted and shown as a background job. A foreground call's job is not, so the call keeps its
    /// usual running card; it is listed once the call is interrupted and the command runs on.
    listed: bool,
    /// The ports its processes listen on, as the last scan found them.
    ports: Vec<u16>,
    cancel: CancellationToken,
}

/// A job's outcome once its work has ended.
struct Finished {
    ok: bool,
    text: String,
    note: Option<String>,
    meta: Option<BashMeta>,
}

pub struct Done {
    pub id: String,
    pub agent: String,
    pub ok: bool,
    pub text: String,
    pub command: Option<String>,
    pub note: Option<String>,
    pub meta: Option<BashMeta>,
}

/// A scoped registration for an interruptible wait.
pub struct ActiveWait {
    jobs: Rc<JobsHandle>,
    generation: u64,
    interrupt: CancellationToken,
}

impl ActiveWait {
    pub fn token(&self) -> CancellationToken {
        self.interrupt.clone()
    }
}

impl Drop for ActiveWait {
    fn drop(&mut self) {
        let mut active = self.jobs.active_wait.borrow_mut();
        if active.as_ref().is_some_and(|(generation, _)| *generation == self.generation) {
            *active = None;
        }
    }
}

/// Readiness does not consume results; callers choose when to deliver them.
#[derive(Debug, PartialEq, Eq)]
pub enum WaitReady {
    Ready(Vec<String>),
    /// Ids of jobs whose `notify` pattern matched; only `wait_ready_or_matches` returns it.
    Matched(Vec<String>),
    Nothing,
    Interrupted,
    Cancelled,
    Unknown(Vec<String>),
}

pub struct JobsHandle {
    inner: RefCell<Vec<Job>>,
    next_seq: Cell<u64>,
    notify: Notify,
    wait_generation: Cell<u64>,
    active_wait: RefCell<Option<(u64, CancellationToken)>>,
    matches: RefCell<Vec<Matched>>,
    wake: UnboundedSender<()>,
    events: EventTx,
}

impl JobsHandle {
    pub fn new(wake: UnboundedSender<()>, events: EventTx) -> Rc<Self> {
        Rc::new(Self {
            inner: RefCell::default(),
            next_seq: Cell::new(0),
            notify: Notify::new(),
            wait_generation: Cell::new(0),
            active_wait: RefCell::new(None),
            matches: RefCell::default(),
            wake,
            events,
        })
    }

    /// Registers the current wait. Dropping an older guard cannot unregister a newer wait.
    pub fn begin_wait(self: &Rc<Self>) -> ActiveWait {
        let generation = self.wait_generation.get().wrapping_add(1);
        self.wait_generation.set(generation);
        let interrupt = CancellationToken::new();
        *self.active_wait.borrow_mut() = Some((generation, interrupt.clone()));
        ActiveWait { jobs: Rc::clone(self), generation, interrupt }
    }

    /// Interrupts only the currently registered wait, not its jobs.
    pub fn interrupt_wait(&self) -> bool {
        if let Some((_, token)) = self.active_wait.borrow().as_ref() {
            token.cancel();
            true
        } else {
            false
        }
    }

    /// Waits for selected jobs, or any notice-owned job when `ids` is empty.
    /// Unknown ids and an exhausted selection return immediately.
    pub async fn wait_ready(
        &self,
        ids: &[String],
        cancel: &CancellationToken,
        interrupt: &CancellationToken,
    ) -> WaitReady {
        self.wait_inner(ids, cancel, interrupt, false).await
    }

    /// `wait_ready`, also ending when output matching a selected job's `notify` pattern arrives.
    pub async fn wait_ready_or_matches(
        &self,
        ids: &[String],
        cancel: &CancellationToken,
        interrupt: &CancellationToken,
    ) -> WaitReady {
        self.wait_inner(ids, cancel, interrupt, true).await
    }

    async fn wait_inner(
        &self,
        ids: &[String],
        cancel: &CancellationToken,
        interrupt: &CancellationToken,
        on_match: bool,
    ) -> WaitReady {
        let mut seen = HashSet::new();
        let ids: Vec<&str> = ids.iter().map(String::as_str).filter(|id| seen.insert(*id)).collect();
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if cancel.is_cancelled() {
                return WaitReady::Cancelled;
            }
            if interrupt.is_cancelled() {
                return WaitReady::Interrupted;
            }
            if on_match {
                let mut matched: Vec<String> = Vec::new();
                for m in self.matches.borrow().iter().filter(|m| ids.is_empty() || ids.contains(&m.id.as_str())) {
                    if !matched.contains(&m.id) {
                        matched.push(m.id.clone());
                    }
                }
                if !matched.is_empty() {
                    return WaitReady::Matched(matched);
                }
            }
            let (ready, unknown, running) = {
                let jobs = self.inner.borrow();
                let unknown = ids
                    .iter()
                    .filter(|id| !jobs.iter().any(|j| j.spec.id == **id))
                    .map(|id| (*id).to_string())
                    .collect::<Vec<_>>();
                let eligible = |j: &&Job| j.notice_owned && !j.delivered;
                let selected: Vec<&Job> = if ids.is_empty() {
                    jobs.iter().filter(eligible).collect()
                } else {
                    ids.iter().filter_map(|id| jobs.iter().find(|j| j.spec.id == *id)).filter(eligible).collect()
                };
                let ready = selected
                    .iter()
                    .filter(|j| j.state != SubagentState::Running)
                    .map(|j| j.spec.id.clone())
                    .collect::<Vec<_>>();
                let running = selected.iter().any(|j| j.state == SubagentState::Running);
                (ready, unknown, running)
            };
            if !unknown.is_empty() {
                return WaitReady::Unknown(unknown);
            }
            if !ready.is_empty() {
                return WaitReady::Ready(ready);
            }
            if !running {
                return WaitReady::Nothing;
            }
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return WaitReady::Cancelled,
                _ = interrupt.cancelled() => return WaitReady::Interrupted,
                _ = &mut notified => {}
            }
        }
    }

    /// Queues matching output for the model, wakes an idle session and ends a blocked `wait`.
    fn push_matches(&self, m: Matched) {
        self.matches.borrow_mut().push(m);
        self.notify.notify_waiters();
        let _ = self.wake.send(());
    }

    pub fn has_matches(&self) -> bool {
        !self.matches.borrow().is_empty()
    }

    /// Matching output not yet handed to the model.
    pub fn take_matches(&self) -> Vec<Matched> {
        std::mem::take(&mut *self.matches.borrow_mut())
    }

    /// `base`, or `base-2`, `base-3`, … when taken this session.
    pub fn unique_id(&self, base: &str) -> String {
        let jobs = self.inner.borrow();
        let taken = |id: &str| jobs.iter().any(|j| j.spec.id == id);
        if !taken(base) {
            return base.to_string();
        }
        (2..).map(|n| format!("{base}-{n}")).find(|id| !taken(id)).unwrap_or_default()
    }

    pub fn running(&self) -> usize {
        self.inner.borrow().iter().filter(|j| j.state == SubagentState::Running).count()
    }

    /// Running jobs that count against the cap: the listed ones. A foreground call's command is
    /// not listed, so it never uses a slot nor is refused for want of one.
    fn background_running(&self) -> usize {
        self.inner.borrow().iter().filter(|j| j.listed && j.state == SubagentState::Running).count()
    }

    /// Errs when `n` more background jobs would go over the cap.
    pub fn check_capacity(&self, n: usize) -> Result<(), String> {
        let running = self.background_running();
        if running + n > MAX_TASKS {
            return Err(format!("too many background jobs running ({running}/{MAX_TASKS}); wait for some to finish"));
        }
        Ok(())
    }

    /// Every listed job: the background jobs this session shows. A foreground call's job is left
    /// out until the call is interrupted.
    pub fn snapshot(&self) -> Vec<SubagentStatus> {
        self.inner
            .borrow()
            .iter()
            .filter(|j| j.listed)
            .map(|j| {
                let (tools, note, session, model, changes, progress) = match &j.work {
                    Work::Subagent { tools, session, model, changes, progress } => {
                        (*tools, None, session.clone(), model.clone(), changes.clone(), progress.clone())
                    }
                    Work::Bash { note, .. } => (0, note.clone(), None, None, None, None),
                };
                SubagentStatus {
                    name: j.spec.id.clone(),
                    agent: j.spec.agent.clone(),
                    tools,
                    last: j.last.clone(),
                    state: j.state.clone(),
                    kind: j.work.kind(),
                    cwd: j.spec.cwd.clone(),
                    task: j.spec.task.clone(),
                    started: j.started,
                    ended: j.ended,
                    delivered: j.delivered,
                    note,
                    session,
                    model,
                    changes,
                    progress,
                    ports: j.ports.clone(),
                }
            })
            .collect()
    }

    fn emit(&self) {
        let _ = self.events.send(AgentEvent::Jobs(self.snapshot()));
    }

    /// Watches the listening ports of the running bash jobs, every `PORT_EVERY`, until none is left.
    /// One scan runs at a time, however many jobs start.
    fn scan_ports(self: &Rc<Self>) {
        if PORT_SCAN.replace(true) {
            return;
        }
        let handle = Rc::downgrade(self);
        tokio::task::spawn_local(async move {
            loop {
                tokio::time::sleep(PORT_EVERY).await;
                let Some(jobs) = handle.upgrade() else { return };
                let groups: Vec<(u64, u32)> = jobs
                    .inner
                    .borrow()
                    .iter()
                    .filter(|j| j.listed && j.state == SubagentState::Running)
                    .filter_map(|j| j.work.pgid().map(|p| (j.seq, p.load(Ordering::Relaxed))))
                    .filter(|&(_, pgid)| pgid > 1)
                    .collect();
                if groups.is_empty() {
                    PORT_SCAN.set(false);
                    return;
                }
                let found = tokio::task::spawn_blocking(move || {
                    groups.into_iter().map(|(seq, pgid)| (seq, crate::ports::listening(pgid))).collect::<Vec<_>>()
                })
                .await
                .unwrap_or_default();
                drop(jobs);
                let Some(jobs) = handle.upgrade() else { return };
                let mut changed = false;
                for (seq, ports) in found {
                    if let Some(mut j) = jobs.job_mut(seq)
                        && j.ports != ports
                    {
                        j.ports = ports;
                        changed = true;
                    }
                }
                if changed {
                    jobs.emit();
                }
            }
        });
    }

    /// The job `seq`, for mutation.
    fn job_mut(&self, seq: u64) -> Option<RefMut<'_, Job>> {
        RefMut::filter_map(self.inner.borrow_mut(), |jobs| jobs.iter_mut().find(|j| j.seq == seq)).ok()
    }

    fn start(
        &self,
        spec: Spec,
        work: Work,
        last: String,
        cancel: CancellationToken,
        notice_owned: bool,
        listed: bool,
    ) -> u64 {
        let seq = self.next_seq.get();
        self.next_seq.set(seq + 1);
        self.inner.borrow_mut().push(Job {
            seq,
            spec,
            work,
            last,
            state: SubagentState::Running,
            text: String::new(),
            started: SystemTime::now(),
            ended: None,
            delivered: false,
            ports: Vec::new(),
            notice_owned,
            listed,
            cancel,
        });
        self.emit();
        seq
    }

    /// Lists a job started for a foreground call: the call was interrupted, so its command runs on
    /// as an ordinary background job from now on. `false` when there is no such job.
    pub fn promote(&self, id: &str) -> bool {
        let found = {
            let mut jobs = self.inner.borrow_mut();
            match jobs.iter_mut().find(|j| j.spec.id == id) {
                Some(j) if !j.listed => {
                    j.listed = true;
                    true
                }
                _ => false,
            }
        };
        if found {
            self.emit();
        }
        found
    }

    /// The one place a job ends: records its outcome. `handed_back`: the caller waiting on the job
    /// gets the result itself, so it is marked delivered and no idle agent is woken for it.
    fn complete(&self, seq: u64, done: Finished, handed_back: bool) {
        if let Some(mut j) = self.job_mut(seq) {
            j.state = if done.ok { SubagentState::Done } else { SubagentState::Failed };
            j.ended = Some(SystemTime::now());
            if handed_back {
                j.delivered = true;
            }
            let keep = !j.delivered;
            if keep {
                j.text = done.text;
            }
            if let Work::Bash { meta, note, .. } = &mut j.work {
                *note = done.note;
                if keep {
                    *meta = done.meta;
                }
            }
        }
        self.notify.notify_waiters();
        if !handed_back {
            let _ = self.wake.send(());
        }
        self.emit();
    }

    /// The one path a job starts on: records it, runs `work` on the local set and records what
    /// `work` finishes with.
    fn spawn_job<F>(
        self: &Rc<Self>,
        spec: Spec,
        kind: Work,
        last: String,
        cancel: CancellationToken,
        listed: bool,
        work: impl FnOnce(u64) -> F,
    ) where
        F: Future<Output = Finished> + 'static,
    {
        let seq = self.start(spec, kind, last, cancel, true, listed);
        let fut = work(seq);
        let me = self.clone();
        tokio::task::spawn_local(async move {
            let done = fut.await;
            me.complete(seq, done, false);
        });
    }

    #[allow(clippy::too_many_arguments)]
    pub fn spawn(
        self: &Rc<Self>,
        id: String,
        agent: String,
        child: ChildCmd,
        cwd: PathBuf,
        payload: Value,
        perms: Rc<Permissions>,
        artifacts: PathBuf,
        mirror: Mirror,
        relay: Option<RelayServer>,
    ) {
        let cancel = CancellationToken::new();
        let child_cancel = cancel.clone();
        let label = format!("{id} ({agent})");
        let spec = Spec { id, agent, cwd: cwd.clone(), task: command_line(payload["task"].as_str().unwrap_or("")) };
        let me = self.clone();
        self.spawn_job(spec, Work::subagent(), String::new(), cancel, true, move |seq| async move {
            let mut child = child;
            if let ChildCmd::Launched(l) = &mut child {
                let me = me.clone();
                l.on_progress(move |p| {
                    if let Some(mut j) = me.job_mut(seq)
                        && let Work::Subagent { progress, .. } = &mut j.work
                    {
                        *progress = p.map(str::to_string);
                    }
                    me.emit();
                });
            }
            let out = me
                .run_subagent(
                    seq,
                    child,
                    &cwd,
                    &payload,
                    &child_cancel,
                    |_, _| {},
                    &perms,
                    &label,
                    mirror,
                    relay.as_ref(),
                )
                .await;
            // `run_child` already says "cancelled" when a cancelled child produced no result; a child
            // that finished before the kill landed keeps its real result.
            let text = preview_result(&out.text, &cwd, || crate::tools::Tools::next_artifact_in(&artifacts));
            if let Some(c) = out.changes
                && let Some(mut j) = me.job_mut(seq)
                && let Work::Subagent { changes, .. } = &mut j.work
            {
                *changes = Some(c);
            }
            Finished { ok: out.ok, text, note: None, meta: None }
        });
    }

    /// Runs a subagent as a job the caller waits for: listed in the jobs panel, cancellable there,
    /// and finished as delivered, so its result never reaches the model as a notice. Cancelling
    /// `cancel` cancels the job too. `on_tool` sees each tool call, as for `run_child`. `id` is
    /// made unique if it is taken.
    #[allow(clippy::too_many_arguments)]
    pub async fn run_owned(
        self: &Rc<Self>,
        id: &str,
        agent: &str,
        exe: &Path,
        cwd: &Path,
        payload: &Value,
        cancel: &CancellationToken,
        on_tool: impl FnMut(&str, &str),
        perms: Rc<Permissions>,
        mirror: Mirror,
    ) -> Outcome {
        let id = self.unique_id(id);
        let own = cancel.child_token();
        let spec = Spec {
            id,
            agent: agent.into(),
            cwd: cwd.to_path_buf(),
            task: command_line(payload["task"].as_str().unwrap_or("")),
        };
        let seq = self.start(spec, Work::subagent(), String::new(), own.clone(), false, true);
        let out = self
            .run_subagent(
                seq,
                ChildCmd::Local(exe.to_path_buf()),
                cwd,
                payload,
                &own,
                on_tool,
                &perms,
                "review",
                mirror,
                None,
            )
            .await;
        self.complete(seq, Finished { ok: out.ok, text: String::new(), note: None, meta: None }, true);
        out
    }

    /// Runs the child of job `seq`, keeping its tool count, last activity, session and model current.
    /// An approval it needs is asked in the parent's UI, labelled `from` it.
    #[allow(clippy::too_many_arguments)]
    async fn run_subagent(
        &self,
        seq: u64,
        child: ChildCmd,
        cwd: &Path,
        payload: &Value,
        cancel: &CancellationToken,
        mut on_tool: impl FnMut(&str, &str),
        perms: &Rc<Permissions>,
        from: &str,
        mirror: Mirror,
        relay: Option<&RelayServer>,
    ) -> Outcome {
        run_child_observed(
            child,
            cwd,
            payload,
            cancel,
            |name, summary| {
                if let Some(mut j) = self.job_mut(seq) {
                    if let Work::Subagent { tools, .. } = &mut j.work {
                        *tools += 1;
                    }
                    j.last = format!("{name} {summary}");
                }
                on_tool(name, summary);
                self.emit();
            },
            |path| {
                if let Some(mut j) = self.job_mut(seq)
                    && let Work::Subagent { session, .. } = &mut j.work
                {
                    *session = Some(PathBuf::from(path));
                }
                self.emit();
            },
            |model| {
                if let Some(mut j) = self.job_mut(seq)
                    && let Work::Subagent { model: m, .. } = &mut j.work
                {
                    *m = Some(model.to_string());
                }
                self.emit();
            },
            Some(&|r| perms.forward(&self.events, from, r)),
            mirror,
            relay,
        )
        .await
    }

    /// Starts a bash job and returns its id. `run` gets the job's cancel token, which `kill` and
    /// `clear` fire, and the sink its output goes to.
    pub fn spawn_bash<F>(
        self: &Rc<Self>,
        command: String,
        cwd: PathBuf,
        run: impl FnOnce(CancellationToken, OutputSink) -> F,
    ) -> (String, OutputSink)
    where
        F: Future<Output = BackgroundResult> + 'static,
    {
        self.spawn_bash_job(command, cwd, None, None, run)
    }

    /// `spawn_bash`, with output lines that match `notify` reported to the model as the job runs.
    pub fn spawn_bash_notify<F>(
        self: &Rc<Self>,
        command: String,
        cwd: PathBuf,
        notify: Option<regex::Regex>,
        run: impl FnOnce(CancellationToken, OutputSink) -> F,
    ) -> (String, OutputSink)
    where
        F: Future<Output = BackgroundResult> + 'static,
    {
        self.spawn_bash_job(command, cwd, None, notify, run)
    }

    /// Starts a foreground call's command: a job so an interrupted call leaves it running, but not
    /// listed among the background jobs while the call waits for it. Its output also streams to
    /// `call`, the waiting tool call, until the sink is detached.
    pub(crate) fn spawn_bash_waiting<F>(
        self: &Rc<Self>,
        command: String,
        cwd: PathBuf,
        call: String,
        run: impl FnOnce(CancellationToken, OutputSink) -> F,
    ) -> (String, OutputSink)
    where
        F: Future<Output = BackgroundResult> + 'static,
    {
        self.spawn_bash_job(command, cwd, Some(call), None, run)
    }

    fn spawn_bash_job<F>(
        self: &Rc<Self>,
        command: String,
        cwd: PathBuf,
        call: Option<String>,
        notify: Option<regex::Regex>,
        run: impl FnOnce(CancellationToken, OutputSink) -> F,
    ) -> (String, OutputSink)
    where
        F: Future<Output = BackgroundResult> + 'static,
    {
        let id = self.unique_id(&format!("bg_{}", self.next_seq.get() + 1));
        let cancel = CancellationToken::new();
        let line = command_line(&command);
        let listed = call.is_none();
        let sink = OutputSink::new(id.clone(), cwd.clone(), self.events.clone(), call);
        if let Some(re) = notify {
            sink.watch(re, Rc::downgrade(self));
        }
        let spec = Spec { id: id.clone(), agent: "bash".into(), cwd, task: one_line(&command, COMMAND_KEEP) };
        let run_cancel = cancel.clone();
        let kept = sink.clone();
        self.spawn_job(spec, Work::bash(command, sink.pgid()), line, cancel, listed, move |_| {
            let fut = run(run_cancel, kept.clone());
            async move {
                let r = fut.await;
                kept.close();
                kept.flush();
                Finished { ok: r.ok, text: r.text, note: Some(r.note), meta: r.meta }
            }
        });
        self.scan_ports();
        (id, sink)
    }

    /// Finished, undelivered jobs; marks them delivered.
    pub fn take_undelivered(&self) -> Vec<Done> {
        self.take_undelivered_for(&[])
    }

    /// Takes completed notice-owned results for the selected ids, or all when empty.
    pub fn take_undelivered_for(&self, ids: &[String]) -> Vec<Done> {
        let done: Vec<Done> = self
            .inner
            .borrow_mut()
            .iter_mut()
            .filter(|j| {
                j.notice_owned
                    && j.state != SubagentState::Running
                    && !j.delivered
                    && (ids.is_empty() || ids.contains(&j.spec.id))
            })
            .map(|j| {
                j.delivered = true;
                let (command, note, meta) = match &mut j.work {
                    Work::Bash { command, note, meta, .. } => (command.take(), note.clone(), meta.take()),
                    Work::Subagent { .. } => (None, None, None),
                };
                Done {
                    id: j.spec.id.clone(),
                    agent: j.spec.agent.clone(),
                    ok: j.state == SubagentState::Done,
                    // Delivered output is not kept: only the snapshot fields stay resident.
                    text: std::mem::take(&mut j.text),
                    command,
                    note,
                    meta,
                }
            })
            .collect();
        if !done.is_empty() {
            self.emit();
        }
        done
    }

    pub fn has_undelivered(&self) -> bool {
        self.inner.borrow().iter().any(|j| j.notice_owned && j.state != SubagentState::Running && !j.delivered)
    }

    /// Discards a notice-owned result, including one that has yet to finish.
    pub fn discard_result(&self, id: &str) -> bool {
        let found = {
            let mut jobs = self.inner.borrow_mut();
            if let Some(j) = jobs.iter_mut().find(|j| j.notice_owned && j.spec.id == id && !j.delivered) {
                j.delivered = true;
                j.text.clear();
                if let Work::Bash { command, meta, .. } = &mut j.work {
                    *command = None;
                    *meta = None;
                }
                true
            } else {
                false
            }
        };
        if found {
            self.notify.notify_waiters();
            self.emit();
        }
        found
    }

    /// Ids of the running listed jobs, oldest first.
    pub fn running_ids(&self) -> Vec<String> {
        let jobs = self.inner.borrow();
        jobs.iter().filter(|j| j.listed && j.state == SubagentState::Running).map(|j| j.spec.id.clone()).collect()
    }

    /// The state of job `id`, `None` when this session has no such job.
    pub fn state_of(&self, id: &str) -> Option<SubagentState> {
        self.inner.borrow().iter().find(|j| j.spec.id == id).map(|j| j.state.clone())
    }

    #[cfg(test)]
    pub(crate) fn is_cancelled(&self, id: &str) -> bool {
        self.inner.borrow().iter().any(|j| j.spec.id == id && j.cancel.is_cancelled())
    }

    /// Cancels running jobs (all when `id` is `None`); returns how many.
    pub fn kill(&self, id: Option<&str>) -> usize {
        let jobs = self.inner.borrow();
        let targets: Vec<&Job> =
            jobs.iter().filter(|j| j.state == SubagentState::Running && id.is_none_or(|i| i == j.spec.id)).collect();
        for j in &targets {
            j.cancel.cancel();
        }
        targets.len()
    }

    /// Kills everything and forgets all jobs (session end).
    pub fn clear(&self) {
        self.kill(None);
        self.inner.borrow_mut().clear();
        self.matches.borrow_mut().clear();
        self.notify.notify_waiters();
        self.emit();
    }

    /// Resolves with the first finished jobs, delivering them; for tests to wait on a job.
    #[cfg(test)]
    pub(crate) async fn finished(&self) -> Vec<Done> {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let done = self.take_undelivered();
            if !done.is_empty() {
                return done;
            }
            notified.await;
        }
    }

    #[cfg(test)]
    pub(crate) fn finish(&self, id: &str, ok: bool, text: &str) {
        if let Some(j) = self.inner.borrow_mut().iter_mut().find(|j| j.spec.id == id) {
            j.state = if ok { SubagentState::Done } else { SubagentState::Failed };
            j.text = text.into();
        }
        self.notify.notify_waiters();
    }

    #[cfg(test)]
    pub(crate) fn push_running(&self, id: &str) {
        let seq = self.next_seq.get();
        self.next_seq.set(seq + 1);
        self.inner.borrow_mut().push(Job {
            seq,
            spec: Spec { id: id.into(), agent: "scout".into(), cwd: PathBuf::new(), task: String::new() },
            work: Work::subagent(),
            last: String::new(),
            state: SubagentState::Running,
            text: String::new(),
            started: SystemTime::now(),
            ended: None,
            delivered: false,
            ports: Vec::new(),
            notice_owned: true,
            listed: true,
            cancel: CancellationToken::new(),
        });
    }
}

/// Most characters of a bash job's command the jobs panel keeps: it wraps the command, so it is
/// shown whole unless it is huge.
const COMMAND_KEEP: usize = 4000;

fn one_line(text: &str, max: usize) -> String {
    truncate_text(&text.split_whitespace().collect::<Vec<_>>().join(" "), max)
}

/// A command on one line for headings and the jobs list.
fn command_line(command: &str) -> String {
    one_line(command, 120)
}

/// One job's result in the same shape the blocking `task` result uses. A bash job's heading names
/// its command and how it ended.
pub fn result_text(d: &Done) -> String {
    let status = if d.ok { "ok" } else { "failed" };
    match &d.command {
        Some(cmd) => {
            let note = d.note.as_ref().map(|n| format!(", {n}")).unwrap_or_default();
            format!("## {} (bash: {}) — {status}{note}\n{}", d.id, command_line(cmd), d.text)
        }
        None => format!("## {} ({}) — {status}\n{}", d.id, d.agent, d.text),
    }
}

/// The user message that delivers finished jobs to the model.
pub fn notice(done: &[Done]) -> String {
    if let [d] = done {
        return format!(
            "<system-notice>\nBackground job {} has completed. Resume your work using the result below.\n{}\n</system-notice>",
            d.id,
            result_text(d)
        );
    }
    let body = done
        .iter()
        .map(|d| format!("── Job {} ({}) ──\n{}", d.id, d.agent, result_text(d)))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "<system-notice>\n{} background jobs have completed. Resume your work using the results below.\n\n{body}\n</system-notice>",
        done.len()
    )
}

/// The user message that hands matching output of running jobs to the model.
pub fn matches_notice(matches: &[Matched]) -> String {
    let body = matches
        .iter()
        .map(|m| {
            let shown: Vec<String> =
                m.lines.iter().take(NOTICE_LINES).map(|l| truncate_text(l, NOTICE_LINE_CHARS)).collect();
            let more = m.lines.len().saturating_sub(NOTICE_LINES);
            let more = if more > 0 { format!("\n(+{more} more matching lines)") } else { String::new() };
            format!("Background job {} printed output matching `{}`:\n{}{more}", m.id, m.pattern, shown.join("\n"))
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    format!("<system-notice>\n{body}\nThe job's final result arrives separately when it ends.\n</system-notice>")
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn handle() -> Rc<JobsHandle> {
        let (wake, _) = tokio::sync::mpsc::unbounded_channel();
        let (events, _) = tokio::sync::mpsc::unbounded_channel();
        JobsHandle::new(wake, events)
    }

    #[tokio::test]
    async fn wait_priority_and_guard_generation() {
        let h = handle();
        h.push_running("A");
        let old = h.begin_wait();
        let current = h.begin_wait();
        drop(old);
        h.finish("A", true, "result");
        assert!(h.interrupt_wait());
        assert!(h.interrupt_wait());
        let cancel = CancellationToken::new();
        assert_eq!(h.wait_ready(&[], &cancel, &current.token()).await, WaitReady::Interrupted);
        cancel.cancel();
        assert_eq!(h.wait_ready(&[], &cancel, &current.token()).await, WaitReady::Cancelled);
        drop(current);
        assert!(!h.interrupt_wait());
        let next = h.begin_wait();
        assert_eq!(
            h.wait_ready(&[], &CancellationToken::new(), &next.token()).await,
            WaitReady::Ready(vec!["A".into()])
        );
    }

    #[tokio::test]
    async fn wait_selects_deduplicates_and_excludes_owned_jobs() {
        let h = handle();
        h.push_running("A");
        h.push_running("B");
        h.push_running("review");
        h.inner.borrow_mut().iter_mut().find(|j| j.spec.id == "review").unwrap().notice_owned = false;
        h.finish("A", true, "a");
        h.finish("B", true, "b");
        h.finish("review", true, "private");
        let guard = h.begin_wait();
        let ids = vec!["A".into(), "missing".into(), "A".into(), "other".into(), "missing".into(), "review".into()];
        let cancel = CancellationToken::new();
        let interrupt = guard.token();
        assert_eq!(
            h.wait_ready(&ids, &cancel, &interrupt).await,
            WaitReady::Unknown(vec!["missing".into(), "other".into()])
        );
        assert_eq!(h.wait_ready(&["review".into()], &cancel, &interrupt).await, WaitReady::Nothing);
        assert_eq!(
            h.wait_ready(&["B".into(), "A".into(), "B".into()], &cancel, &interrupt).await,
            WaitReady::Ready(vec!["B".into(), "A".into()])
        );
        assert_eq!(h.take_undelivered_for(&ids).len(), 1);
        assert_eq!(h.wait_ready(&["A".into()], &cancel, &interrupt).await, WaitReady::Nothing);
        assert_eq!(h.take_undelivered()[0].id, "B");
        assert!(!h.has_undelivered());
    }

    #[tokio::test]
    async fn wait_wakes_on_completion_clear_and_discard() {
        let h = handle();
        h.push_running("A");
        let guard = h.begin_wait();
        let cancel = CancellationToken::new();
        let interrupt = guard.token();
        let wait = h.wait_ready(&[], &cancel, &interrupt);
        let finish = async {
            tokio::task::yield_now().await;
            h.finish("A", true, "a");
        };
        let (result, ()) = tokio::join!(wait, finish);
        assert_eq!(result, WaitReady::Ready(vec!["A".into()]));
        h.take_undelivered();
        h.push_running("B");
        let wait = h.wait_ready(&[], &cancel, &interrupt);
        let clear = async {
            tokio::task::yield_now().await;
            h.clear();
        };
        let (result, ()) = tokio::join!(wait, clear);
        assert_eq!(result, WaitReady::Nothing);
        h.push_running("C");
        let wait = h.wait_ready(&[], &cancel, &interrupt);
        let discard = async {
            tokio::task::yield_now().await;
            assert!(h.discard_result("C"));
        };
        let (result, ()) = tokio::join!(wait, discard);
        assert_eq!(result, WaitReady::Nothing);
        h.complete(0, Finished { ok: true, text: "discarded".into(), note: None, meta: None }, false);
        assert!(h.take_undelivered().is_empty());
        assert!(h.inner.borrow()[0].text.is_empty());
    }

    fn done(id: &str, ok: bool, text: &str) -> Done {
        Done { id: id.into(), agent: "scout".into(), ok, text: text.into(), command: None, note: None, meta: None }
    }

    #[test]
    fn delivered_output_is_released() {
        let h = handle();
        h.inner.borrow_mut().push(Job {
            seq: 0,
            spec: Spec { id: "A".into(), agent: "scout".into(), cwd: PathBuf::from("/w"), task: "echo".into() },
            work: Work::Bash {
                command: Some("echo".into()),
                meta: None,
                note: Some("exit 0".into()),
                pgid: Arc::new(AtomicU32::new(0)),
            },
            last: String::new(),
            state: SubagentState::Done,
            text: "big output".into(),
            started: SystemTime::now(),
            ended: Some(SystemTime::now()),
            delivered: false,
            ports: Vec::new(),
            notice_owned: true,
            listed: true,
            cancel: CancellationToken::new(),
        });
        let snap = h.snapshot();
        assert_eq!(
            (snap[0].delivered, snap[0].task.as_str(), snap[0].note.as_deref()),
            (false, "echo", Some("exit 0"))
        );
        assert!(snap[0].ended.is_some());
        let got = h.take_undelivered();
        assert_eq!(got[0].text, "big output");
        assert_eq!((got[0].command.as_deref(), got[0].note.as_deref()), (Some("echo"), Some("exit 0")));
        assert!(h.inner.borrow()[0].text.is_empty());
        assert!(matches!(&h.inner.borrow()[0].work, Work::Bash { command: None, .. }));
        assert!(h.take_undelivered().is_empty());
        let snap = h.snapshot();
        assert!(snap[0].delivered, "the snapshot follows delivery");
        assert_eq!(snap[0].note.as_deref(), Some("exit 0"), "the note stays for the panel");
    }

    #[test]
    fn a_snapshot_event_follows_delivery() {
        let (wake, _) = tokio::sync::mpsc::unbounded_channel();
        let (events, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let h = JobsHandle::new(wake, events);
        h.push_running("A");
        h.finish("A", true, "x");
        assert_eq!(h.take_undelivered().len(), 1);
        match rx.try_recv() {
            Ok(AgentEvent::Jobs(v)) => assert!(v[0].delivered),
            _ => panic!("no snapshot after delivery"),
        }
        assert!(h.take_undelivered().is_empty());
        assert!(rx.try_recv().is_err(), "nothing delivered, nothing emitted");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_subagent_job_records_its_task_and_session_path() {
        use std::os::unix::fs::PermissionsExt;
        let base = std::env::temp_dir().join(format!("mu-jobs-session-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&base).unwrap();
        let exe = base.join("fake-mu");
        let script = "#!/bin/sh\ncat <<'EOF'\n{\"type\":\"session\",\"path\":\"/s/child.jsonl\"}\n{\"type\":\"result\",\"text\":\"ok\"}\nEOF\n";
        std::fs::write(&exe, script).unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        let h = handle();
        tokio::task::LocalSet::new()
            .run_until(async {
                h.spawn(
                    "A".into(),
                    "scout".into(),
                    ChildCmd::Local(exe),
                    base.clone(),
                    serde_json::json!({"task": "find  x\nnow"}),
                    Rc::default(),
                    base.join("artifacts"),
                    Mirror::Off,
                    None,
                );
                assert_eq!(h.finished().await.len(), 1);
            })
            .await;
        let snap = h.snapshot();
        assert_eq!(snap[0].session.as_deref(), Some(std::path::Path::new("/s/child.jsonl")));
        assert_eq!(snap[0].task, "find x now", "the task is one line");
        assert_eq!((snap[0].cwd.as_path(), snap[0].state.clone()), (base.as_path(), SubagentState::Done));
        let _ = std::fs::remove_dir_all(&base);
    }

    fn fake_child(script_body: &str) -> (PathBuf, PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let base = std::env::temp_dir().join(format!("mu-jobs-owned-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&base).unwrap();
        let exe = base.join("fake-mu");
        std::fs::write(&exe, format!("#!/bin/sh\n{script_body}")).unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        (base, exe)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn an_owned_job_is_listed_and_finishes_delivered_without_waking() {
        let (base, exe) = fake_child(
            "cat <<'EOF'\n{\"type\":\"session\",\"path\":\"/s/r.jsonl\"}\n{\"type\":\"tool\",\"name\":\"read\",\"summary\":\"a.rs\"}\n{\"type\":\"result\",\"text\":\"report\"}\nEOF\n",
        );
        let (wake, mut woken) = tokio::sync::mpsc::unbounded_channel();
        let (events, _rx) = tokio::sync::mpsc::unbounded_channel();
        let h = JobsHandle::new(wake, events);
        let mut seen = Vec::new();
        let out = tokio::task::LocalSet::new()
            .run_until(h.run_owned(
                "review",
                "review-lead",
                &exe,
                &base,
                &serde_json::json!({"task": "review it"}),
                &CancellationToken::new(),
                |n, s| seen.push(format!("{n} {s}")),
                Rc::default(),
                Mirror::Off,
            ))
            .await;
        assert_eq!((out.ok, out.text.as_str()), (true, "report"));
        assert_eq!(seen, ["read a.rs"]);
        let snap = h.snapshot();
        assert_eq!((snap[0].name.as_str(), snap[0].agent.as_str()), ("review", "review-lead"));
        assert_eq!((snap[0].state.clone(), snap[0].tools, snap[0].delivered), (SubagentState::Done, 1, true));
        assert_eq!(snap[0].session.as_deref(), Some(std::path::Path::new("/s/r.jsonl")));
        assert!(!h.has_undelivered() && woken.try_recv().is_err(), "the model is not told");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn an_owned_job_is_cancelled_from_the_panel_or_by_its_owner() {
        let (base, exe) = fake_child("sleep 30\n");
        let h = handle();
        let payload = serde_json::json!({"task": "t"});
        let owner = CancellationToken::new();
        tokio::task::LocalSet::new()
            .run_until(async {
                let killer = async {
                    while h.running() == 0 {
                        tokio::task::yield_now().await;
                    }
                    assert_eq!(h.kill(Some("review")), 1);
                };
                let (out, ()) = tokio::join!(
                    h.run_owned(
                        "review",
                        "review-lead",
                        &exe,
                        &base,
                        &payload,
                        &owner,
                        |_, _| {},
                        Rc::default(),
                        Mirror::Off
                    ),
                    killer
                );
                assert!(!out.ok && out.text == "cancelled", "{}", out.text);
                assert!(!owner.is_cancelled(), "killing the job does not cancel its owner");
                assert_eq!(h.snapshot()[0].state, SubagentState::Failed);

                let canceller = async {
                    while h.running() == 0 {
                        tokio::task::yield_now().await;
                    }
                    owner.cancel();
                };
                let (out, ()) = tokio::join!(
                    h.run_owned(
                        "review",
                        "review-lead",
                        &exe,
                        &base,
                        &payload,
                        &owner,
                        |_, _| {},
                        Rc::default(),
                        Mirror::Off
                    ),
                    canceller
                );
                assert!(!out.ok);
                assert_eq!(h.snapshot()[1].name, "review-2");
            })
            .await;
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_long_command_is_cut_for_headings_but_not_for_the_panel() {
        let cmd = format!("cd /w &&\n{}", "x".repeat(300));
        assert!(command_line(&cmd).chars().count() <= 120);
        let whole = one_line(&cmd, COMMAND_KEEP);
        assert_eq!(whole, format!("cd /w && {}", "x".repeat(300)));
    }

    fn sink() -> (OutputSink, tokio::sync::mpsc::UnboundedReceiver<AgentEvent>) {
        sink_every(OUTPUT_EVERY)
    }

    fn sink_every(every: Duration) -> (OutputSink, tokio::sync::mpsc::UnboundedReceiver<AgentEvent>) {
        let (events, rx) = tokio::sync::mpsc::unbounded_channel();
        (OutputSink::with_every("bg_1".into(), PathBuf::from("/"), events, None, every), rx)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn output_goes_out_at_once_then_throttled_and_never_unsent() {
        // A window no stall outlasts: the later chunks are held however slowly the test runs.
        let (sink, mut rx) = sink_every(Duration::from_secs(3600));
        tokio::task::LocalSet::new()
            .run_until(async {
                sink.push("one\n");
                assert!(
                    matches!(rx.try_recv(), Ok(AgentEvent::JobOutput { id, tail }) if id == "bg_1" && tail == "one\n")
                );
                sink.push("two\n");
                sink.push("three\n");
                assert!(rx.try_recv().is_err(), "chunks inside the window wait");
                assert!(sink.0.pending.get(), "one deferred update covers both chunks");
            })
            .await;

        // A short window: whichever way the pushes fall, the last chunk goes out in the end.
        let (sink, mut rx) = sink_every(Duration::from_millis(100));
        tokio::task::LocalSet::new()
            .run_until(async {
                sink.push("one\n");
                sink.push("two\n");
                sink.push("three\n");
                let mut last = String::new();
                tokio::time::timeout(Duration::from_secs(60), async {
                    while !last.ends_with("three\n") {
                        match rx.recv().await {
                            Some(AgentEvent::JobOutput { tail, .. }) => last = tail,
                            _ => panic!("no job output event"),
                        }
                    }
                })
                .await
                .expect("the deferred update never came");
                assert_eq!(last, "one\ntwo\nthree\n");
                assert!(rx.try_recv().is_err(), "nothing is left unsent or sent twice");
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn output_is_bounded_and_flush_sends_what_is_left() {
        let (sink, mut rx) = sink();
        tokio::task::LocalSet::new()
            .run_until(async {
                sink.push(&"é".repeat(100 * 1024));
                match rx.try_recv() {
                    Ok(AgentEvent::JobOutput { tail, .. }) => {
                        assert!(tail.len() <= OUTPUT_EMIT && tail.chars().all(|c| c == 'é'), "{}", tail.len());
                    }
                    _ => panic!("no update"),
                }
                assert!(sink.0.buf.borrow().len() <= OUTPUT_KEEP);
                sink.push("last");
                sink.flush();
                let mut got = None;
                while let Ok(AgentEvent::JobOutput { tail, .. }) = rx.try_recv() {
                    got = Some(tail);
                }
                assert!(got.is_some_and(|t| t.ends_with("last")), "flush sends the end at once");
            })
            .await;
        let (empty, mut rx) = self::sink();
        empty.flush();
        assert!(rx.try_recv().is_err(), "nothing to send");
    }

    #[test]
    fn notice_formats_one_and_many() {
        assert_eq!(
            notice(&[done("A", true, "ALPHA")]),
            "<system-notice>\nBackground job A has completed. Resume your work using the result below.\n## A (scout) — ok\nALPHA\n</system-notice>"
        );
        assert_eq!(
            notice(&[done("A", true, "x"), done("B", false, "y")]),
            "<system-notice>\n2 background jobs have completed. Resume your work using the results below.\n\n\
             ── Job A (scout) ──\n## A (scout) — ok\nx\n── Job B (scout) ──\n## B (scout) — failed\ny\n</system-notice>"
        );
    }

    #[test]
    fn bash_heading_names_the_command_and_exit() {
        let d = Done {
            id: "bg_1".into(),
            agent: "bash".into(),
            ok: false,
            text: "out".into(),
            command: Some("make\n  test".into()),
            note: Some("exit 2".into()),
            meta: None,
        };
        assert_eq!(result_text(&d), "## bg_1 (bash: make test) — failed, exit 2\nout");
    }

    #[test]
    fn unique_id_suffixes() {
        let h = handle();
        assert_eq!(h.unique_id("Task1"), "Task1");
        h.push_running("Task1");
        assert_eq!(h.unique_id("Task1"), "Task1-2");
        h.push_running("Task1-2");
        assert_eq!(h.unique_id("Task1"), "Task1-3");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_finished_job_is_handed_over_once() {
        let h = handle();
        h.push_running("A");
        h.finish("A", true, "ALPHA");
        let d = h.finished().await;
        assert_eq!((d[0].id.as_str(), d[0].text.as_str(), d[0].ok), ("A", "ALPHA", true));
        assert!(h.take_undelivered().is_empty());
        assert!(!h.has_undelivered());
    }

    fn watched(h: &Rc<JobsHandle>, pattern: &str) -> OutputSink {
        let (events, _rx) = tokio::sync::mpsc::unbounded_channel();
        let sink = OutputSink::new("bg_1".into(), PathBuf::from("/"), events, None);
        sink.watch(regex::Regex::new(pattern).unwrap(), Rc::downgrade(h));
        sink
    }

    #[tokio::test(flavor = "current_thread")]
    async fn matching_lines_within_a_window_arrive_as_one_notice() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let h = handle();
                let sink = watched(&h, "^ready");
                sink.lines("starting\nready one\n");
                sink.lines("noise\nready two\n");
                assert!(!h.has_matches(), "nothing is sent before the window closes");
                mu_testutil::wait_until("the batch", || h.has_matches()).await;
                let m = h.take_matches();
                assert_eq!(m.len(), 1);
                assert_eq!(
                    (m[0].id.as_str(), m[0].lines.as_slice()),
                    ("bg_1", &["ready one".into(), "ready two".into()][..])
                );
                let text = matches_notice(&m);
                assert!(text.contains("bg_1 printed output matching `^ready`:\nready one\nready two"), "{text}");
                assert!(!h.has_matches());
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn matches_not_yet_sent_when_the_job_ends_are_dropped() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let h = handle();
                let sink = watched(&h, "ready");
                sink.lines("ready\n");
                sink.close();
                tokio::time::sleep(NOTIFY_BATCH * 3).await;
                assert!(!h.has_matches());
                sink.lines("ready again\n");
                tokio::time::sleep(NOTIFY_BATCH * 3).await;
                assert!(!h.has_matches(), "a closed sink takes no more");
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_match_wakes_the_session_and_ends_a_wait_for_matches_only() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let (wake, mut woke) = tokio::sync::mpsc::unbounded_channel();
                let (events, _rx) = tokio::sync::mpsc::unbounded_channel();
                let h = JobsHandle::new(wake, events);
                h.push_running("bg_1");
                let sink = watched(&h, "ready");
                let (cancel, interrupt) = (CancellationToken::new(), CancellationToken::new());
                let waiting = h.wait_ready_or_matches(&[], &cancel, &interrupt);
                let feed = async {
                    tokio::task::yield_now().await;
                    sink.lines("ready\n");
                };
                let (r, ()) = tokio::join!(waiting, feed);
                assert_eq!(r, WaitReady::Matched(vec!["bg_1".into()]));
                assert!(woke.try_recv().is_ok(), "an idle session is woken");
                // The ordinary wait (a foreground call's own) ignores matches and so keeps waiting.
                let plain =
                    tokio::time::timeout(Duration::from_millis(100), h.wait_ready(&[], &cancel, &interrupt)).await;
                assert!(plain.is_err());
                // Matches of other jobs do not end a wait on a named one.
                h.push_running("other");
                let named = tokio::time::timeout(
                    Duration::from_millis(100),
                    h.wait_ready_or_matches(&["other".into()], &cancel, &interrupt),
                )
                .await;
                assert!(named.is_err());
            })
            .await;
    }

    #[test]
    fn a_long_match_list_is_cut_in_the_notice() {
        let lines: Vec<String> = (0..NOTICE_LINES + 5).map(|i| format!("l{i}")).collect();
        let text = matches_notice(&[Matched { id: "bg_1".into(), pattern: "l".into(), lines }]);
        assert!(text.contains("l49") && !text.contains("l50\n") && text.contains("(+5 more matching lines)"), "{text}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn finished_wakes_when_a_running_job_ends() {
        let h = handle();
        h.push_running("A");
        let finisher = async {
            tokio::task::yield_now().await;
            h.finish("A", false, "boom");
        };
        let (d, ()) = tokio::join!(h.finished(), finisher);
        assert!(!d[0].ok);
    }
}
