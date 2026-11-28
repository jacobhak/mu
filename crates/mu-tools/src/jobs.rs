//! Background jobs: `task` (subagents) and `bash` with `async` return ids at once; finished results
//! are delivered to the model as a `<system-notice>` message.

use std::cell::{Cell, RefCell, RefMut};
use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant, SystemTime};

use regex::Regex;
use serde_json::{Value, json};
use tokio::sync::Notify;
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
use tokio_util::sync::CancellationToken;

use crate::permissions::Permissions;
use crate::tools::relay::RelayServer;
use crate::tools::subagent::{
    BackgroundResult, ChildCmd, ChildControl, Mirror, Outcome, RESULT_PREVIEW_CHARS, WorkerLaunch, prepare_launch,
    preview_result, run_child_controlled,
};
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

/// Most subagents that may wait for a slot or for the jobs they follow.
const MAX_QUEUED: usize = 16;
/// Tool calls a running subagent's status keeps.
const RECENT_TOOLS: usize = 10;

/// Lines of a running job's output that matched its `notify` pattern, not yet handed to the model.
#[derive(Debug, PartialEq, Eq)]
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
    /// The newest cleaned output (at most `OUTPUT_KEEP` bytes), for `job_status` and `wait`'s `until`.
    clean: RefCell<String>,
    /// Bytes of cleaned output ever appended.
    clean_total: Cell<u64>,
    /// Woken when output arrives, so a `wait` with `until` rescans.
    jobs: RefCell<Weak<JobsHandle>>,
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
            clean: RefCell::default(),
            clean_total: Cell::new(0),
            jobs: RefCell::new(Weak::new()),
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

    fn set_jobs(&self, jobs: Weak<JobsHandle>) {
        *self.0.jobs.borrow_mut() = jobs;
    }

    /// Keeps cleaned output, newest `OUTPUT_KEEP` bytes.
    fn keep(&self, clean: &str) {
        let i = &self.0;
        i.clean_total.set(i.clean_total.get() + clean.len() as u64);
        let mut c = i.clean.borrow_mut();
        c.push_str(clean);
        if c.len() > OUTPUT_KEEP {
            let mut cut = c.len() - OUTPUT_KEEP;
            while !c.is_char_boundary(cut) {
                cut += 1;
            }
            c.drain(..cut);
        }
    }

    /// The last `lines` lines of cleaned output, redacted.
    pub fn clean_tail(&self, lines: usize) -> String {
        let c = self.0.clean.borrow();
        let all: Vec<&str> = c.lines().collect();
        let text = all[all.len().saturating_sub(lines)..].join("\n");
        mu_base::secrets::redact(&text, &self.0.cwd).text
    }

    /// Complete lines that match `re`, from absolute output offset `*from` (or the oldest byte still
    /// kept, when output before it is gone); advances `*from` past the last whole line looked at.
    fn scan(&self, re: &Regex, from: &mut u64) -> Vec<String> {
        let i = &self.0;
        let c = i.clean.borrow();
        let base = i.clean_total.get() - c.len() as u64;
        let start = (*from).max(base);
        let Some(region) = c.get((start - base) as usize..) else { return Vec::new() };
        let Some(end) = region.rfind('\n') else { return Vec::new() };
        *from = start + end as u64 + 1;
        let hits: Vec<&str> = region[..end].lines().filter(|l| re.is_match(l)).collect();
        if hits.is_empty() {
            return Vec::new();
        }
        mu_base::secrets::redact(&hits.join("\n"), &i.cwd).text.lines().map(str::to_string).collect()
    }

    /// Reports output lines that match `re` to the model through `jobs`.
    pub fn watch(&self, re: regex::Regex, jobs: Weak<JobsHandle>) {
        let pattern = re.as_str().to_string();
        *self.0.watch.borrow_mut() =
            Some(Watch { re, pattern, jobs, pending: Vec::new(), scheduled: false, closed: false });
    }

    /// Cleaned output (whole lines): the lines that match the watch pattern are batched for a notice.
    pub fn lines(&self, clean: &str) {
        self.keep(clean);
        if let Some(jobs) = self.0.jobs.borrow().upgrade() {
            jobs.notify.notify_waiters();
        }
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
        /// The last `RECENT_TOOLS` tool calls, as "name summary".
        recent: VecDeque<String>,
        /// Lines for the child's stdin (steering); `None` for a job that takes none.
        steer: Option<UnboundedSender<String>>,
        /// Messages sent with `send`.
        sent: u32,
        /// The task payload, kept while the job waits to start.
        payload: Option<Value>,
        /// Runs on a remote worker.
        launched: bool,
        /// The session this job continues (`task` with `resume`).
        resumes: Option<PathBuf>,
    },
    Bash {
        /// Taken on delivery, like `text`.
        command: Option<String>,
        meta: Option<BashMeta>,
        /// How it ended ("exit 3", "timed out", …).
        note: Option<String>,
        /// The command's process group, once its shell is running (0 until then).
        pgid: Arc<AtomicU32>,
        /// Where its output goes; also kept for `job_status` and `wait`'s `until`.
        sink: OutputSink,
    },
}

impl Work {
    fn subagent() -> Self {
        Work::Subagent {
            tools: 0,
            session: None,
            model: None,
            changes: None,
            progress: None,
            recent: VecDeque::new(),
            steer: None,
            sent: 0,
            payload: None,
            launched: false,
            resumes: None,
        }
    }

    fn bash(command: String, sink: OutputSink) -> Self {
        Work::Bash { command: Some(command), meta: None, note: None, pgid: sink.pgid(), sink }
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
    /// Jobs that must succeed before this one starts.
    after: Vec<String>,
    /// A copy of the result for the jobs that wait for this one; kept after delivery.
    kept: String,
    /// Starts a queued job: given its seq and its (completed) payload.
    start: Option<StartFn>,
}

type StartFn = Box<dyn FnOnce(u64, Value) -> Pin<Box<dyn Future<Output = Finished>>>>;

/// What `JobsHandle::pump` does next.
enum Step {
    Idle,
    /// End a queued job that cannot start.
    Fail(u64, String),
    /// Run a queued job that now may.
    Start(u64, StartFn, Value),
}

/// How a subagent job is scheduled.
#[derive(Default)]
pub struct SpawnOpts {
    /// Jobs that must succeed first; the job gets their results in its context.
    pub after: Vec<String>,
    /// Time limit: the subagent is told to wrap up, and cancelled two minutes later.
    pub timeout: Option<Duration>,
    /// Run on a worker through this launcher instead of here.
    pub launch: Option<WorkerLaunch>,
}

impl Job {
    #[allow(clippy::too_many_arguments)]
    fn new(
        seq: u64,
        spec: Spec,
        work: Work,
        last: String,
        state: SubagentState,
        cancel: CancellationToken,
        notice_owned: bool,
        listed: bool,
    ) -> Self {
        Self {
            seq,
            spec,
            work,
            last,
            state,
            text: String::new(),
            started: SystemTime::now(),
            ended: None,
            delivered: false,
            ports: Vec::new(),
            notice_owned,
            listed,
            cancel,
            after: Vec::new(),
            kept: String::new(),
            start: None,
        }
    }
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
    /// The deadline passed: the selected jobs that had finished, and those still active.
    TimedOut {
        finished: Vec<String>,
        running: Vec<String>,
    },
    /// Output lines of the selected jobs matched `until`.
    Until(Vec<Matched>),
    Nothing,
    Interrupted,
    Cancelled,
    Unknown(Vec<String>),
}

/// What a `wait` is for.
pub struct WaitSpec<'a> {
    /// The jobs to wait for; empty means every job whose result is still to come.
    pub ids: &'a [String],
    /// Wait until every selected job has ended, instead of the first.
    pub all: bool,
    /// Return when an output line of a selected bash job matches, including output already printed.
    pub until: Option<&'a Regex>,
    pub deadline: Option<tokio::time::Instant>,
    /// Also return when a selected job's `notify` pattern matched.
    pub on_match: bool,
}

pub struct JobsHandle {
    inner: RefCell<Vec<Job>>,
    /// What `session_cost` read of a session file at a given length, so `job_status` rereads only when it grew.
    costs: RefCell<HashMap<PathBuf, (u64, f64)>>,
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
            costs: RefCell::default(),
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
        let spec = WaitSpec { ids, all: false, until: None, deadline: None, on_match: false };
        self.wait_for(spec, cancel, interrupt).await
    }

    /// `wait_ready`, also ending when output matching a selected job's `notify` pattern arrives.
    pub async fn wait_ready_or_matches(
        &self,
        ids: &[String],
        cancel: &CancellationToken,
        interrupt: &CancellationToken,
    ) -> WaitReady {
        let spec = WaitSpec { ids, all: false, until: None, deadline: None, on_match: true };
        self.wait_for(spec, cancel, interrupt).await
    }

    /// The general wait: see `WaitSpec`. Unknown ids and an exhausted selection return immediately.
    pub async fn wait_for(
        &self,
        spec: WaitSpec<'_>,
        cancel: &CancellationToken,
        interrupt: &CancellationToken,
    ) -> WaitReady {
        let mut seen = HashSet::new();
        let ids: Vec<&str> = spec.ids.iter().map(String::as_str).filter(|id| seen.insert(*id)).collect();
        // Where each job's output was last scanned for `until`; 0 so retained output counts.
        let mut scanned: HashMap<String, u64> = HashMap::new();
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
            if let Some(re) = spec.until {
                let found = self.scan_until(re, &ids, &mut scanned);
                if !found.is_empty() {
                    return WaitReady::Until(found);
                }
            }
            if spec.on_match && !spec.all && spec.until.is_none() {
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
            let (ready, unknown, active) = {
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
                let ready =
                    selected.iter().filter(|j| !j.state.is_active()).map(|j| j.spec.id.clone()).collect::<Vec<_>>();
                let active =
                    selected.iter().filter(|j| j.state.is_active()).map(|j| j.spec.id.clone()).collect::<Vec<_>>();
                (ready, unknown, active)
            };
            if !unknown.is_empty() {
                return WaitReady::Unknown(unknown);
            }
            if !ready.is_empty() && (!spec.all || active.is_empty()) {
                return WaitReady::Ready(ready);
            }
            if active.is_empty() {
                return WaitReady::Nothing;
            }
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return WaitReady::Cancelled,
                _ = interrupt.cancelled() => return WaitReady::Interrupted,
                () = async {
                    match spec.deadline {
                        Some(d) => tokio::time::sleep_until(d).await,
                        None => std::future::pending().await,
                    }
                } => return WaitReady::TimedOut { finished: ready, running: active },
                _ = &mut notified => {}
            }
        }
    }

    /// Output lines of the selected bash jobs that match `re` and were not looked at before.
    fn scan_until(&self, re: &Regex, ids: &[&str], scanned: &mut HashMap<String, u64>) -> Vec<Matched> {
        let jobs = self.inner.borrow();
        let mut found = Vec::new();
        for j in jobs.iter().filter(|j| ids.contains(&j.spec.id.as_str())) {
            let Work::Bash { sink, .. } = &j.work else { continue };
            let from = scanned.entry(j.spec.id.clone()).or_insert(0);
            let lines = sink.scan(re, from);
            if !lines.is_empty() {
                found.push(Matched { id: j.spec.id.clone(), pattern: re.as_str().to_string(), lines });
            }
        }
        found
    }

    /// Matching output of the named jobs (all jobs when `ids` is empty) not yet handed to the model;
    /// takes it.
    pub fn take_matches_for(&self, ids: &[String]) -> Vec<Matched> {
        let mut all = self.matches.borrow_mut();
        let (taken, kept): (Vec<Matched>, Vec<Matched>) =
            std::mem::take(&mut *all).into_iter().partition(|m| ids.is_empty() || ids.contains(&m.id));
        *all = kept;
        taken
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

    /// Jobs not finished: running, or waiting to start.
    pub fn running(&self) -> usize {
        self.inner.borrow().iter().filter(|j| j.state.is_active()).count()
    }

    /// Subagents waiting for a slot or for the jobs they follow.
    pub fn queued(&self) -> usize {
        self.inner.borrow().iter().filter(|j| j.state == SubagentState::Queued).count()
    }

    /// Errs when `n` more subagents would overflow the queue.
    pub fn check_queue(&self, n: usize) -> Result<(), String> {
        let q = self.queued();
        if q + n > MAX_QUEUED {
            return Err(format!("too many jobs waiting to start ({q}/{MAX_QUEUED}); wait for some to finish"));
        }
        Ok(())
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
                    Work::Subagent { tools, session, model, changes, progress, .. } => {
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
        self.inner.borrow_mut().push(Job::new(
            seq,
            spec,
            work,
            last,
            SubagentState::Running,
            cancel,
            notice_owned,
            listed,
        ));
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
    fn complete(self: &Rc<Self>, seq: u64, done: Finished, handed_back: bool) {
        self.finish_job(seq, done, handed_back);
        self.pump();
    }

    /// `complete` without starting what was waiting for a slot or for this job.
    fn finish_job(&self, seq: u64, done: Finished, handed_back: bool) {
        if let Some(mut j) = self.job_mut(seq) {
            j.state = if done.ok { SubagentState::Done } else { SubagentState::Failed };
            j.ended = Some(SystemTime::now());
            if handed_back {
                j.delivered = true;
            }
            j.kept = truncate_text(&done.text, RESULT_PREVIEW_CHARS);
            j.start = None;
            if let Work::Subagent { payload, steer, .. } = &mut j.work {
                *payload = None;
                *steer = None;
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
    }

    /// Starts the queued jobs that may start: those whose `after` jobs all succeeded, while a slot
    /// is free, oldest first; one whose `after` job failed (or is gone) ends without starting.
    /// Runs whenever a job ends or one is queued.
    fn pump(self: &Rc<Self>) {
        loop {
            match self.next_step() {
                Step::Idle => break,
                Step::Fail(seq, text) => {
                    self.finish_job(seq, Finished { ok: false, text, note: None, meta: None }, false);
                }
                Step::Start(seq, start, payload) => {
                    let me = self.clone();
                    tokio::task::spawn_local(async move {
                        let done = start(seq, payload).await;
                        me.complete(seq, done, false);
                    });
                }
            }
        }
        self.emit();
    }

    /// The next thing `pump` does, if any. A job it starts is marked running, with the results of
    /// the jobs it waited for added to its context.
    fn next_step(&self) -> Step {
        let mut jobs = self.inner.borrow_mut();
        let running = jobs.iter().filter(|j| j.listed && j.state == SubagentState::Running).count();
        let free = running < MAX_TASKS;
        for i in 0..jobs.len() {
            if jobs[i].state != SubagentState::Queued {
                continue;
            }
            let mut ready = true;
            for dep in &jobs[i].after {
                match jobs.iter().find(|d| d.spec.id == *dep).map(|d| &d.state) {
                    Some(SubagentState::Done) => {}
                    Some(SubagentState::Failed) => {
                        let text = format!("not started: job `{dep}`, which it waited for, failed");
                        return Step::Fail(jobs[i].seq, text);
                    }
                    Some(_) => ready = false,
                    None => {
                        let text = format!("not started: job `{dep}`, which it waited for, no longer exists");
                        return Step::Fail(jobs[i].seq, text);
                    }
                }
            }
            if !ready || !free {
                continue;
            }
            let block = if jobs[i].after.is_empty() {
                None
            } else {
                let results: Vec<String> = jobs[i]
                    .after
                    .iter()
                    .filter_map(|dep| jobs.iter().find(|d| d.spec.id == *dep))
                    .map(|d| format!("## {} ({}) — ok\n{}", d.spec.id, d.spec.agent, d.kept))
                    .collect();
                Some(format!("# Results of the jobs this task waited for\n\n{}", results.join("\n\n")))
            };
            let job = &mut jobs[i];
            let Some(start) = job.start.take() else {
                return Step::Fail(job.seq, "not started: nothing to start".into());
            };
            let mut payload = match &mut job.work {
                Work::Subagent { payload, .. } => payload.take().unwrap_or(Value::Null),
                Work::Bash { .. } => Value::Null,
            };
            if let Some(block) = block {
                payload["context"] = match payload["context"].as_str().filter(|c| !c.is_empty()) {
                    Some(existing) => Value::String(format!("{existing}\n\n{block}")),
                    None => Value::String(block),
                };
            }
            job.state = SubagentState::Running;
            job.started = SystemTime::now();
            return Step::Start(job.seq, start, payload);
        }
        Step::Idle
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

    /// Queues a subagent job; it starts as soon as the jobs in `opts.after` have succeeded and fewer
    /// than `MAX_TASKS` run. `exe` is this executable, which runs the child unless `opts.launch`
    /// sends it to a worker (prepared when the job starts).
    #[allow(clippy::too_many_arguments)]
    pub fn spawn(
        self: &Rc<Self>,
        id: String,
        agent: String,
        exe: PathBuf,
        cwd: PathBuf,
        payload: Value,
        perms: Rc<Permissions>,
        artifacts: PathBuf,
        mirror: Mirror,
        relay: Option<RelayServer>,
        opts: SpawnOpts,
    ) {
        let cancel = CancellationToken::new();
        let child_cancel = cancel.clone();
        let label = format!("{id} ({agent})");
        let SpawnOpts { after, timeout, launch } = opts;
        let (stx, srx) = unbounded_channel::<String>();
        let job_id = id.clone();
        let spec = Spec { id, agent, cwd: cwd.clone(), task: command_line(payload["task"].as_str().unwrap_or("")) };
        let mut work = Work::subagent();
        if let Work::Subagent { steer, payload: kept, launched, resumes, .. } = &mut work {
            *steer = Some(stx);
            *kept = Some(payload.clone());
            *launched = launch.is_some();
            *resumes = payload["resume"].as_str().map(PathBuf::from);
        }
        let me = self.clone();
        let start: StartFn = Box::new(move |seq, payload| {
            Box::pin(async move {
                let mut child = match &launch {
                    None => ChildCmd::Local(exe),
                    Some(on) => match prepare_launch(on, &cwd, &job_id).await {
                        Ok(l) => ChildCmd::Launched(l),
                        Err(text) => return Finished { ok: false, text, note: None, meta: None },
                    },
                };
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
                let control = ChildControl { steer: Some(srx), timeout };
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
                        control,
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
            })
        });
        let seq = self.next_seq.get();
        self.next_seq.set(seq + 1);
        let mut job = Job::new(seq, spec, work, String::new(), SubagentState::Queued, cancel, true, true);
        job.after = after;
        job.start = Some(start);
        self.inner.borrow_mut().push(job);
        self.pump();
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
                ChildControl::default(),
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
        control: ChildControl,
    ) -> Outcome {
        run_child_controlled(
            child,
            cwd,
            payload,
            cancel,
            |name, summary| {
                if let Some(mut j) = self.job_mut(seq) {
                    let last = format!("{name} {summary}");
                    if let Work::Subagent { tools, recent, .. } = &mut j.work {
                        *tools += 1;
                        if recent.len() == RECENT_TOOLS {
                            recent.pop_front();
                        }
                        recent.push_back(last.clone());
                    }
                    j.last = last;
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
            control,
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
        sink.set_jobs(Rc::downgrade(self));
        if let Some(re) = notify {
            sink.watch(re, Rc::downgrade(self));
        }
        let spec = Spec { id: id.clone(), agent: "bash".into(), cwd, task: one_line(&command, COMMAND_KEEP) };
        let run_cancel = cancel.clone();
        let kept = sink.clone();
        self.spawn_job(spec, Work::bash(command, sink.clone()), line, cancel, listed, move |_| {
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
                j.notice_owned && !j.state.is_active() && !j.delivered && (ids.is_empty() || ids.contains(&j.spec.id))
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
        self.inner.borrow().iter().any(|j| j.notice_owned && !j.state.is_active() && !j.delivered)
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
        jobs.iter().filter(|j| j.listed && j.state.is_active()).map(|j| j.spec.id.clone()).collect()
    }

    /// Whether `id` names a subagent job.
    pub fn is_subagent(&self, id: &str) -> bool {
        self.inner.borrow().iter().any(|j| j.spec.id == id && j.work.kind() == JobKind::Subagent)
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
    pub fn kill(self: &Rc<Self>, id: Option<&str>) -> usize {
        let mut waiting = Vec::new();
        let n = {
            let jobs = self.inner.borrow();
            let mut n = 0;
            for j in jobs.iter().filter(|j| j.state.is_active() && id.is_none_or(|i| i == j.spec.id)) {
                n += 1;
                if j.state == SubagentState::Queued {
                    waiting.push(j.seq);
                } else {
                    j.cancel.cancel();
                }
            }
            n
        };
        // A queued job has no process to stop: it ends here, and nothing waiting for a slot may take
        // its place before the rest of the batch is ended too.
        for seq in waiting {
            let done = Finished { ok: false, text: "cancelled before it started".into(), note: None, meta: None };
            self.finish_job(seq, done, false);
        }
        self.pump();
        n
    }

    /// Kills everything and forgets all jobs (session end).
    pub fn clear(self: &Rc<Self>) {
        self.kill(None);
        self.inner.borrow_mut().clear();
        self.matches.borrow_mut().clear();
        self.notify.notify_waiters();
        self.emit();
    }

    /// Sends `text` to the subagent `id`: to its stdin as a steering line when it runs, into its task
    /// when it has not started. `urgent` also cancels the tool call it is in.
    pub fn send(&self, id: &str, text: &str, urgent: bool) -> Result<Sent, String> {
        let mut jobs = self.inner.borrow_mut();
        let Some(j) = jobs.iter_mut().find(|j| j.spec.id == id) else { return Err(format!("no job `{id}`")) };
        let finished = || format!("`{id}` has finished; continue it with `task` and `resume: \"{id}\"`");
        let state = j.state.clone();
        match &mut j.work {
            Work::Bash { .. } => Err(format!("`{id}` is a bash job; only subagents take messages")),
            Work::Subagent { payload, steer, sent, .. } => match state {
                SubagentState::Done | SubagentState::Failed => Err(finished()),
                SubagentState::Queued => {
                    let extra = format!("Additional instructions from the agent that started this task:\n{text}");
                    if let Some(p) = payload.as_mut() {
                        p["context"] = match p["context"].as_str().filter(|c| !c.is_empty()) {
                            Some(c) => Value::String(format!("{c}\n\n{extra}")),
                            None => Value::String(extra),
                        };
                    }
                    Ok(Sent::Queued)
                }
                SubagentState::Running => {
                    let Some(tx) = steer else { return Err(format!("`{id}` does not take messages")) };
                    let line = json!({
                        "type": "steer",
                        "text": format!("Message from the agent that started this task:\n\n{text}"),
                        "urgent": urgent,
                    });
                    tx.send(line.to_string()).map_err(|_| finished())?;
                    *sent += 1;
                    Ok(Sent::Live)
                }
            },
        }
    }

    /// What `task` needs to continue the finished subagent `id`.
    pub fn resumable(&self, id: &str) -> Result<Resumable, String> {
        let jobs = self.inner.borrow();
        let Some(j) = jobs.iter().find(|j| j.spec.id == id) else { return Err(format!("no job `{id}`")) };
        let Work::Subagent { session, launched, .. } = &j.work else { return Err(format!("`{id}` is a bash job")) };
        if j.state.is_active() {
            return Err(format!("`{id}` is still running; message it with job_send"));
        }
        if *launched {
            return Err(format!("`{id}` ran on a remote worker; only local subagents can be resumed"));
        }
        let Some(session) = session.clone() else { return Err(format!("`{id}` has no session to continue")) };
        let other = jobs.iter().find(|o| {
            o.spec.id != id
                && o.state.is_active()
                && matches!(&o.work, Work::Subagent { session: s, resumes, .. }
                    if s.as_ref() == Some(&session) || resumes.as_ref() == Some(&session))
        });
        if let Some(o) = other {
            return Err(format!("`{id}` is already being continued by `{}`", o.spec.id));
        }
        Ok(Resumable { agent: j.spec.agent.clone(), cwd: j.spec.cwd.clone(), session })
    }

    /// The text `job_status` returns: every listed job, or `id` in detail.
    pub fn status(&self, id: Option<&str>, tail: usize) -> Result<String, String> {
        let jobs = self.inner.borrow();
        let listed: Vec<&Job> = jobs.iter().filter(|j| j.listed).collect();
        let now = SystemTime::now();
        let Some(id) = id else {
            if listed.is_empty() {
                return Ok("No background jobs in this session.".into());
            }
            return Ok(listed.iter().map(|j| self.status_line(j, now)).collect::<Vec<_>>().join("\n"));
        };
        let Some(j) = listed.iter().find(|j| j.spec.id == id) else {
            let names = listed.iter().map(|j| j.spec.id.as_str()).collect::<Vec<_>>().join(", ");
            let names = if names.is_empty() { "none".to_string() } else { names };
            return Err(format!("job_status: no job `{id}`; jobs: {names}"));
        };
        Ok(self.status_detail(j, tail, now))
    }

    /// What the session file of a subagent has cost so far; read again only when the file grew.
    fn session_cost(&self, path: &Path) -> f64 {
        let len = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        if let Some((l, c)) = self.costs.borrow().get(path)
            && *l == len
        {
            return *c;
        }
        let cost = mu_session::session::tree_cost::session_cost(path);
        self.costs.borrow_mut().insert(path.to_path_buf(), (len, cost));
        cost
    }

    fn job_cost(&self, j: &Job) -> Option<f64> {
        match &j.work {
            Work::Subagent { session: Some(p), .. } => Some(self.session_cost(p)).filter(|c| *c > 0.0),
            _ => None,
        }
    }

    fn status_line(&self, j: &Job, now: SystemTime) -> String {
        let mut parts = vec![
            j.spec.id.clone(),
            if j.work.kind() == JobKind::Bash { "bash".into() } else { j.spec.agent.clone() },
            state_label(&j.state).to_string(),
            elapsed_label(j, now),
        ];
        if let Work::Subagent { tools, .. } = &j.work {
            parts.push(format!("{tools} tools"));
        }
        if let Some(c) = self.job_cost(j) {
            parts.push(format!("${c:.2}"));
        }
        let last = match (&j.state, j.after.is_empty()) {
            (SubagentState::Queued, false) => format!("waits for {}", j.after.join(", ")),
            (SubagentState::Queued, true) => "waits for a free slot".to_string(),
            _ => j.last.clone(),
        };
        if !last.is_empty() {
            parts.push(last);
        }
        let mut line = parts.join(" · ");
        if !j.ports.is_empty() {
            line.push_str(&format!(" · {}", j.ports.iter().map(|p| format!(":{p}")).collect::<Vec<_>>().join(" ")));
        }
        if !j.state.is_active() && j.notice_owned && !j.delivered {
            line.push_str(" · result waiting");
        }
        line
    }

    fn status_detail(&self, j: &Job, tail: usize, now: SystemTime) -> String {
        let bash = j.work.kind() == JobKind::Bash;
        let agent = if bash { "bash" } else { j.spec.agent.as_str() };
        let mut out = format!("Job {} ({agent}) — {}, {}", j.spec.id, state_label(&j.state), elapsed_label(j, now));
        if !j.spec.cwd.as_os_str().is_empty() {
            out.push_str(&format!("\ncwd: {}", j.spec.cwd.display()));
        }
        out.push_str(&format!("\n{}: {}", if bash { "command" } else { "task" }, j.spec.task));
        if j.state == SubagentState::Queued && !j.after.is_empty() {
            out.push_str(&format!("\nwaits for: {}", j.after.join(", ")));
        }
        match &j.work {
            Work::Subagent { model, session, sent, recent, .. } => {
                if let Some(m) = model {
                    out.push_str(&format!("\nmodel: {m}"));
                }
                if let Some(s) = session {
                    out.push_str(&format!("\nsession: {}", s.display()));
                }
                if let Some(c) = self.job_cost(j) {
                    out.push_str(&format!("\ncost: ${c:.2}"));
                }
                if *sent > 0 {
                    out.push_str(&format!("\nmessages sent: {sent}"));
                }
                if !j.ports.is_empty() {
                    out.push_str(&format!(
                        "\nports: {}",
                        j.ports.iter().map(u16::to_string).collect::<Vec<_>>().join(", ")
                    ));
                }
                self.status_tail(&mut out, j, now);
                out.push_str("\nRecent tool calls:");
                if recent.is_empty() {
                    out.push_str("\n- none yet");
                }
                for r in recent {
                    out.push_str(&format!("\n- {r}"));
                }
            }
            Work::Bash { note, sink, .. } => {
                if !j.ports.is_empty() {
                    out.push_str(&format!(
                        "\nports: {}",
                        j.ports.iter().map(u16::to_string).collect::<Vec<_>>().join(", ")
                    ));
                }
                if let Some(n) = note.as_deref().filter(|n| !n.is_empty()) {
                    out.push_str(&format!("\nended: {n}"));
                }
                self.status_tail(&mut out, j, now);
                let shown = sink.clean_tail(tail);
                let n = shown.lines().count();
                if n == 0 {
                    out.push_str("\nOutput: none yet");
                } else {
                    out.push_str(&format!("\nOutput (last {n} lines):\n{shown}"));
                }
            }
        }
        out
    }

    fn status_tail(&self, out: &mut String, j: &Job, _now: SystemTime) {
        if !j.state.is_active() && j.notice_owned {
            out.push_str(if j.delivered { "\nresult: delivered" } else { "\nresult: waiting (use `wait`)" });
        }
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
            j.kept = truncate_text(text, RESULT_PREVIEW_CHARS);
        }
        self.notify.notify_waiters();
    }

    #[cfg(test)]
    pub(crate) fn push_running(&self, id: &str) {
        let seq = self.next_seq.get();
        self.next_seq.set(seq + 1);
        let spec = Spec { id: id.into(), agent: "scout".into(), cwd: PathBuf::new(), task: String::new() };
        self.inner.borrow_mut().push(Job::new(
            seq,
            spec,
            Work::subagent(),
            String::new(),
            SubagentState::Running,
            CancellationToken::new(),
            true,
            true,
        ));
    }

    /// A running subagent that takes messages: the receiver gets the lines `send` writes.
    #[cfg(test)]
    pub(crate) fn push_steerable(&self, id: &str) -> tokio::sync::mpsc::UnboundedReceiver<String> {
        self.push_running(id);
        let (tx, rx) = unbounded_channel();
        if let Some(j) = self.inner.borrow_mut().iter_mut().find(|j| j.spec.id == id)
            && let Work::Subagent { steer, .. } = &mut j.work
        {
            *steer = Some(tx);
        }
        rx
    }

    /// A subagent waiting for the jobs in `after` (and a slot); once it may start its job returns "Q",
    /// and the payload it was given is sent to `payloads`.
    #[cfg(test)]
    pub(crate) fn push_queued(&self, id: &str, after: &[&str], payloads: tokio::sync::mpsc::UnboundedSender<Value>) {
        let seq = self.next_seq.get();
        self.next_seq.set(seq + 1);
        let spec = Spec { id: id.into(), agent: "scout".into(), cwd: PathBuf::new(), task: String::new() };
        let mut work = Work::subagent();
        if let Work::Subagent { payload, .. } = &mut work {
            *payload = Some(json!({"task": "t"}));
        }
        let mut job =
            Job::new(seq, spec, work, String::new(), SubagentState::Queued, CancellationToken::new(), true, true);
        job.after = after.iter().map(|a| a.to_string()).collect();
        job.start = Some(Box::new(move |_, p| {
            let _ = payloads.send(p);
            Box::pin(async { Finished { ok: true, text: "Q".into(), note: None, meta: None } })
        }));
        self.inner.borrow_mut().push(job);
    }
}

/// How `send` delivered a message.
#[derive(Debug, PartialEq, Eq)]
pub enum Sent {
    /// To the running subagent.
    Live,
    /// Into the task of a subagent that has not started.
    Queued,
}

/// What `task` needs to continue a finished subagent.
#[derive(Debug)]
pub struct Resumable {
    pub agent: String,
    pub cwd: PathBuf,
    pub session: PathBuf,
}

fn state_label(s: &SubagentState) -> &'static str {
    match s {
        SubagentState::Queued => "queued",
        SubagentState::Running => "running",
        SubagentState::Done => "done",
        SubagentState::Failed => "failed",
    }
}

fn elapsed_label(j: &Job, now: SystemTime) -> String {
    let secs = j.ended.unwrap_or(now).duration_since(j.started).map_or(0, |d| d.as_secs());
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m {}s", secs / 60, secs % 60),
        _ => format!("{}h {}m", secs / 3600, secs % 3600 / 60),
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

/// The matching lines of running jobs, as text.
pub fn matches_body(matches: &[Matched]) -> String {
    matches
        .iter()
        .map(|m| {
            let shown: Vec<String> =
                m.lines.iter().take(NOTICE_LINES).map(|l| truncate_text(l, NOTICE_LINE_CHARS)).collect();
            let more = m.lines.len().saturating_sub(NOTICE_LINES);
            let more = if more > 0 { format!("\n(+{more} more matching lines)") } else { String::new() };
            format!("Background job {} printed output matching `{}`:\n{}{more}", m.id, m.pattern, shown.join("\n"))
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// The user message that hands matching output of running jobs to the model.
pub fn matches_notice(matches: &[Matched]) -> String {
    let body = matches_body(matches);
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
        let (events, _rx) = tokio::sync::mpsc::unbounded_channel();
        let sink = OutputSink::new("A".into(), PathBuf::from("/w"), events, None);
        let mut job = Job::new(
            0,
            Spec { id: "A".into(), agent: "scout".into(), cwd: PathBuf::from("/w"), task: "echo".into() },
            Work::Bash {
                command: Some("echo".into()),
                meta: None,
                note: Some("exit 0".into()),
                pgid: Arc::new(AtomicU32::new(0)),
                sink,
            },
            String::new(),
            SubagentState::Done,
            CancellationToken::new(),
            true,
            true,
        );
        job.text = "big output".into();
        job.ended = Some(SystemTime::now());
        h.inner.borrow_mut().push(job);
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
                    exe,
                    base.clone(),
                    serde_json::json!({"task": "find  x\nnow"}),
                    Rc::default(),
                    base.join("artifacts"),
                    Mirror::Off,
                    None,
                    SpawnOpts::default(),
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

    async fn settled(h: &Rc<JobsHandle>, id: &str, state: SubagentState) {
        let what = format!("{id} to be {state:?}");
        mu_testutil::wait_until(&what, || h.state_of(id) == Some(state.clone())).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_queued_job_starts_when_a_slot_frees() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let h = handle();
                for i in 0..MAX_TASKS {
                    h.push_running(&format!("r{i}"));
                }
                let (tx, mut payloads) = unbounded_channel();
                h.push_queued("Q", &[], tx);
                h.pump();
                assert_eq!((h.state_of("Q"), h.queued(), h.running()), (Some(SubagentState::Queued), 1, MAX_TASKS + 1));
                h.finish("r0", true, "x");
                h.pump();
                assert_eq!(h.state_of("Q"), Some(SubagentState::Running));
                assert!(payloads.recv().await.is_some(), "its start function ran");
                settled(&h, "Q", SubagentState::Done).await;
                assert_eq!(h.queued(), 0);
            })
            .await;
    }

    #[test]
    fn the_queue_is_bounded() {
        let h = handle();
        let (tx, _rx) = unbounded_channel();
        for i in 0..MAX_QUEUED {
            h.push_queued(&format!("q{i}"), &[], tx.clone());
        }
        let e = h.check_queue(1).unwrap_err();
        assert_eq!(e, format!("too many jobs waiting to start ({MAX_QUEUED}/{MAX_QUEUED}); wait for some to finish"));
        assert!(h.check_queue(0).is_ok());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn after_passes_results_and_fails_fast() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let h = handle();
                let (tx, mut payloads) = unbounded_channel();
                h.push_running("A");
                h.push_queued("B", &["A"], tx.clone());
                h.pump();
                assert_eq!(h.state_of("B"), Some(SubagentState::Queued), "A has not finished");
                h.finish("A", true, "ALPHA");
                h.pump();
                let p = payloads.recv().await.unwrap();
                let context = p["context"].as_str().unwrap();
                assert!(context.contains("## A (scout) — ok\nALPHA"), "{context}");
                settled(&h, "B", SubagentState::Done).await;

                h.push_running("C");
                h.push_queued("D", &["C"], tx);
                h.finish("C", false, "boom");
                h.pump();
                assert_eq!(h.state_of("D"), Some(SubagentState::Failed));
                let d = h.take_undelivered_for(&["D".to_string()]);
                assert_eq!(d[0].text, "not started: job `C`, which it waited for, failed");
                assert!(payloads.try_recv().is_err(), "D never started");
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_dependency_result_is_added_to_an_existing_context() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let h = handle();
                let (tx, mut payloads) = unbounded_channel();
                h.push_running("A");
                h.push_queued("B", &["A"], tx);
                h.send("B", "also look at b.rs", false).unwrap();
                h.finish("A", true, "ALPHA");
                h.pump();
                let p = payloads.recv().await.unwrap();
                let context = p["context"].as_str().unwrap();
                let extra = "Additional instructions from the agent that started this task:\nalso look at b.rs";
                assert!(
                    context.starts_with(extra) && context.contains("\n\n# Results of the jobs this task waited for")
                );
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn kill_cancels_a_queued_job_without_starting_it() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let h = handle();
                let (tx, mut payloads) = unbounded_channel();
                h.push_running("A");
                h.push_queued("B", &["A"], tx.clone());
                h.push_queued("C", &["B"], tx);
                assert_eq!(h.kill(Some("B")), 1);
                assert_eq!(h.state_of("B"), Some(SubagentState::Failed));
                assert_eq!(h.state_of("C"), Some(SubagentState::Failed), "what waited for it does not start");
                assert!(!h.is_cancelled("A"));
                assert_eq!(h.take_undelivered_for(&["B".to_string()])[0].text, "cancelled before it started");
                h.finish("A", true, "");
                h.pump();
                assert!(payloads.try_recv().is_err());
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn wait_times_out_with_partial_results() {
        let h = handle();
        h.push_running("A");
        h.push_running("B");
        h.finish("B", true, "b");
        let (cancel, interrupt) = (CancellationToken::new(), CancellationToken::new());
        let spec = |all, ms| WaitSpec {
            ids: &[],
            all,
            until: None,
            deadline: Some(tokio::time::Instant::now() + Duration::from_millis(ms)),
            on_match: false,
        };
        // Without `all`, the finished one returns at once.
        assert_eq!(h.wait_for(spec(false, 5000), &cancel, &interrupt).await, WaitReady::Ready(vec!["B".into()]));
        // With `all`, the running one keeps the wait until the deadline.
        assert_eq!(
            h.wait_for(spec(true, 30), &cancel, &interrupt).await,
            WaitReady::TimedOut { finished: vec!["B".into()], running: vec!["A".into()] }
        );
        h.finish("A", true, "a");
        assert_eq!(
            h.wait_for(spec(true, 5000), &cancel, &interrupt).await,
            WaitReady::Ready(vec!["A".into(), "B".into()])
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn until_matches_retained_and_new_output() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let h = handle();
                let idle = |cancel: CancellationToken, _| async move {
                    cancel.cancelled().await;
                    BackgroundResult { ok: false, text: String::new(), note: "cancelled".into(), meta: None }
                };
                let (first, sink) = h.spawn_bash("serve".into(), "/".into(), idle);
                sink.lines("starting\nready on :3000\n");
                let re = Regex::new("ready").unwrap();
                let (cancel, interrupt) = (CancellationToken::new(), CancellationToken::new());
                let ids = [first.clone()];
                let spec = |ids| WaitSpec { ids, all: false, until: Some(&re), deadline: None, on_match: true };
                let got = h.wait_for(spec(&ids), &cancel, &interrupt).await;
                let expect =
                    Matched { id: first.clone(), pattern: "ready".into(), lines: vec!["ready on :3000".into()] };
                assert_eq!(got, WaitReady::Until(vec![expect]), "output printed before the call counts");

                let (second, sink2) = h.spawn_bash("watch".into(), "/".into(), idle);
                let ids = [second.clone()];
                let waiting = h.wait_for(spec(&ids), &cancel, &interrupt);
                let feed = async {
                    tokio::task::yield_now().await;
                    sink2.lines("noise\nthe app is ready\n");
                };
                let (got, ()) = tokio::join!(waiting, feed);
                match got {
                    WaitReady::Until(m) => assert_eq!(m[0].lines, ["the app is ready"]),
                    other => panic!("{other:?}"),
                }
                // A job that ended without a match returns its result instead.
                h.kill(None);
                settled(&h, &second, SubagentState::Failed).await;
                let ids = [second.clone()];
                let r = h.wait_for(spec(&ids), &cancel, &interrupt).await;
                assert!(matches!(r, WaitReady::Until(_) | WaitReady::Ready(_)), "{r:?}");
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn clean_output_is_kept_bounded_and_the_tail_is_redacted() {
        let (events, _rx) = tokio::sync::mpsc::unbounded_channel();
        let sink = OutputSink::new("bg_1".into(), PathBuf::from("/"), events, None);
        sink.lines("a\nb\nc\nd\n");
        assert_eq!(sink.clean_tail(2), "c\nd");
        assert_eq!(sink.clean_tail(50), "a\nb\nc\nd");
        let big = format!("{}\n", "é".repeat(100 * 1024));
        sink.lines(&big);
        assert!(sink.0.clean.borrow().len() <= OUTPUT_KEEP);
        assert_eq!(sink.0.clean_total.get(), 8 + big.len() as u64);
        // A scan resumes after what it has seen, and skips what has been dropped.
        sink.lines("tail line\n");
        let mut from = 0;
        let re = Regex::new("tail").unwrap();
        assert_eq!(sink.scan(&re, &mut from), ["tail line"]);
        assert!(sink.scan(&re, &mut from).is_empty(), "nothing new");
    }

    #[test]
    fn send_reaches_a_running_subagent_and_edits_a_queued_one() {
        let h = handle();
        let mut lines = h.push_steerable("A");
        assert_eq!(h.send("A", "go left", true), Ok(Sent::Live));
        let line: Value = serde_json::from_str(&lines.try_recv().unwrap()).unwrap();
        assert_eq!((line["type"].as_str(), line["urgent"].as_bool()), (Some("steer"), Some(true)));
        assert_eq!(line["text"], "Message from the agent that started this task:\n\ngo left");
        assert_eq!(h.snapshot().len(), 1);

        let (tx, _payloads) = unbounded_channel();
        h.push_queued("Q", &["A"], tx);
        assert_eq!(h.send("Q", "also b", false), Ok(Sent::Queued));
        let jobs = h.inner.borrow();
        let Work::Subagent { payload: Some(p), sent, .. } = &jobs[1].work else { panic!("a queued subagent") };
        assert!(p["context"].as_str().unwrap().ends_with("also b") && *sent == 0);
        drop(jobs);

        assert_eq!(h.send("nope", "x", false), Err("no job `nope`".into()));
        h.finish("A", true, "");
        let e = h.send("A", "x", false).unwrap_err();
        assert_eq!(e, "`A` has finished; continue it with `task` and `resume: \"A\"`");
    }

    #[test]
    fn only_a_finished_local_subagent_with_a_session_can_be_resumed() {
        let h = handle();
        h.push_running("A");
        assert_eq!(h.resumable("A").unwrap_err(), "`A` is still running; message it with job_send");
        assert_eq!(h.resumable("zz").unwrap_err(), "no job `zz`");
        h.finish("A", true, "x");
        assert_eq!(h.resumable("A").unwrap_err(), "`A` has no session to continue");
        let set = |id: &str, f: &dyn Fn(&mut Work)| {
            let mut jobs = h.inner.borrow_mut();
            f(&mut jobs.iter_mut().find(|j| j.spec.id == id).unwrap().work);
        };
        let path = PathBuf::from("/s/a.jsonl");
        set("A", &|w| {
            if let Work::Subagent { session, .. } = w {
                *session = Some(path.clone());
            }
        });
        let r = h.resumable("A").unwrap();
        assert_eq!((r.agent.as_str(), r.session.as_path()), ("scout", path.as_path()));
        h.push_running("B");
        set("B", &|w| {
            if let Work::Subagent { resumes, .. } = w {
                *resumes = Some(path.clone());
            }
        });
        assert_eq!(h.resumable("A").unwrap_err(), "`A` is already being continued by `B`");
        set("A", &|w| {
            if let Work::Subagent { launched, .. } = w {
                *launched = true;
            }
        });
        assert!(h.resumable("A").unwrap_err().contains("remote worker"));
    }
}
