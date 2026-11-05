//! Background jobs: `task` (subagents) and `bash` with `async` return ids at once; finished results
//! are delivered to the model as a `<system-notice>` message.

use std::cell::{Cell, RefCell, RefMut};
use std::future::Future;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant, SystemTime};

use serde_json::Value;
#[cfg(test)]
use tokio::sync::Notify;
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

use crate::tools::subagent::{BackgroundResult, run_child_observed};
use crate::tools::task::MAX_TASKS;
use mu_base::events::{AgentEvent, EventTx, JobKind, SubagentState, SubagentStatus};
use mu_base::text::truncate_text;

/// Newest bytes of a running bash job's output kept for the jobs panel.
const OUTPUT_KEEP: usize = 64 * 1024;
/// How much of that goes to the UI per update, and how often at most.
const OUTPUT_EMIT: usize = 16 * 1024;
const OUTPUT_EVERY: Duration = Duration::from_millis(250);

/// Where a running bash job's output goes: a bounded buffer whose newest part is sent to the UI as
/// `AgentEvent::JobOutput` (redacted, throttled), so the panel can show a job while it runs.
#[derive(Clone)]
pub struct OutputSink(Rc<SinkInner>);

struct SinkInner {
    id: String,
    cwd: PathBuf,
    events: EventTx,
    buf: RefCell<String>,
    last: Cell<Option<Instant>>,
    /// A deferred update is already waiting.
    pending: Cell<bool>,
}

impl OutputSink {
    fn new(id: String, cwd: PathBuf, events: EventTx) -> Self {
        Self(Rc::new(SinkInner {
            id,
            cwd,
            events,
            buf: RefCell::default(),
            last: Cell::new(None),
            pending: Cell::new(false),
        }))
    }

    /// Adds output. The first chunk after a quiet spell is sent at once; later ones wait for the
    /// throttle, so the last chunk is never left unsent.
    pub fn push(&self, s: &str) {
        let i = &self.0;
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
            Some(since) if since < OUTPUT_EVERY => {
                i.pending.set(true);
                let me = self.clone();
                let wait = OUTPUT_EVERY - since;
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
    kind: JobKind,
    cwd: PathBuf,
    /// The task prompt or command, truncated; kept for the jobs panel.
    task: String,
    /// The command of a bash job; taken on delivery, like `text`.
    command: Option<String>,
}

struct Job {
    /// Distinguishes a job from a later one that reuses its id after `clear`.
    seq: u64,
    spec: Spec,
    tools: u32,
    last: String,
    state: SubagentState,
    text: String,
    started: SystemTime,
    ended: Option<SystemTime>,
    /// How a bash job ended ("exit 3", "timed out", …).
    note: Option<String>,
    /// A subagent's session file, once the child has opened it.
    session: Option<PathBuf>,
    model: Option<String>,
    delivered: bool,
    cancel: CancellationToken,
}

/// A job's outcome once its work has ended.
struct Finished {
    ok: bool,
    text: String,
    note: Option<String>,
}

pub struct Done {
    pub id: String,
    pub agent: String,
    pub ok: bool,
    pub text: String,
    pub command: Option<String>,
    pub note: Option<String>,
}

pub struct JobsHandle {
    inner: RefCell<Vec<Job>>,
    next_seq: Cell<u64>,
    #[cfg(test)]
    notify: Notify,
    wake: UnboundedSender<()>,
    events: EventTx,
}

impl JobsHandle {
    pub fn new(wake: UnboundedSender<()>, events: EventTx) -> Rc<Self> {
        Rc::new(Self {
            inner: RefCell::default(),
            next_seq: Cell::new(0),
            #[cfg(test)]
            notify: Notify::new(),
            wake,
            events,
        })
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

    /// Errs when `n` more running jobs would go over the cap.
    pub fn check_capacity(&self, n: usize) -> Result<(), String> {
        let running = self.running();
        if running + n > MAX_TASKS {
            return Err(format!("too many background jobs running ({running}/{MAX_TASKS}); wait for some to finish"));
        }
        Ok(())
    }

    pub fn snapshot(&self) -> Vec<SubagentStatus> {
        self.inner
            .borrow()
            .iter()
            .map(|j| SubagentStatus {
                name: j.spec.id.clone(),
                agent: j.spec.agent.clone(),
                tools: j.tools,
                last: j.last.clone(),
                state: j.state.clone(),
                kind: j.spec.kind,
                cwd: j.spec.cwd.clone(),
                task: j.spec.task.clone(),
                started: j.started,
                ended: j.ended,
                delivered: j.delivered,
                note: j.note.clone(),
                session: j.session.clone(),
                model: j.model.clone(),
            })
            .collect()
    }

    fn emit(&self) {
        let _ = self.events.send(AgentEvent::Jobs(self.snapshot()));
    }

    /// The job `seq`, for mutation.
    fn job_mut(&self, seq: u64) -> Option<RefMut<'_, Job>> {
        RefMut::filter_map(self.inner.borrow_mut(), |jobs| jobs.iter_mut().find(|j| j.seq == seq)).ok()
    }

    fn start(&self, spec: Spec, last: String, cancel: CancellationToken) -> u64 {
        let seq = self.next_seq.get();
        self.next_seq.set(seq + 1);
        self.inner.borrow_mut().push(Job {
            seq,
            spec,
            tools: 0,
            last,
            state: SubagentState::Running,
            text: String::new(),
            started: SystemTime::now(),
            ended: None,
            note: None,
            session: None,
            model: None,
            delivered: false,
            cancel,
        });
        self.emit();
        seq
    }

    /// Records a finished job and wakes an idle agent.
    fn complete(&self, seq: u64, ok: bool, text: String, note: Option<String>) {
        if let Some(mut j) = self.job_mut(seq) {
            j.state = if ok { SubagentState::Done } else { SubagentState::Failed };
            j.ended = Some(SystemTime::now());
            j.text = text;
            j.note = note;
        }
        #[cfg(test)]
        self.notify.notify_waiters();
        let _ = self.wake.send(());
        self.emit();
    }

    /// The one path a job starts on: records it, runs `work` on the local set and records what
    /// `work` finishes with.
    fn spawn_job<F>(self: &Rc<Self>, spec: Spec, last: String, cancel: CancellationToken, work: impl FnOnce(u64) -> F)
    where
        F: Future<Output = Finished> + 'static,
    {
        let seq = self.start(spec, last, cancel);
        let fut = work(seq);
        let me = self.clone();
        tokio::task::spawn_local(async move {
            let done = fut.await;
            me.complete(seq, done.ok, done.text, done.note);
        });
    }

    pub fn spawn(self: &Rc<Self>, id: String, agent: String, exe: PathBuf, cwd: PathBuf, payload: Value) {
        let cancel = CancellationToken::new();
        let child_cancel = cancel.clone();
        let spec = Spec {
            id,
            agent,
            kind: JobKind::Subagent,
            cwd: cwd.clone(),
            task: command_line(payload["task"].as_str().unwrap_or("")),
            command: None,
        };
        let me = self.clone();
        self.spawn_job(spec, String::new(), cancel, move |seq| async move {
            let out = run_child_observed(
                &exe,
                &cwd,
                &payload,
                &child_cancel,
                |name, summary| {
                    if let Some(mut j) = me.job_mut(seq) {
                        j.tools += 1;
                        j.last = format!("{name} {summary}");
                    }
                    me.emit();
                },
                |path| {
                    if let Some(mut j) = me.job_mut(seq) {
                        j.session = Some(PathBuf::from(path));
                    }
                    me.emit();
                },
                |model| {
                    if let Some(mut j) = me.job_mut(seq) {
                        j.model = Some(model.to_string());
                    }
                    me.emit();
                },
            )
            .await;
            // `run_child` already says "cancelled" when a cancelled child produced no result; a child
            // that finished before the kill landed keeps its real result.
            Finished { ok: out.ok, text: out.text, note: None }
        });
    }

    /// Starts a bash job and returns its id. `run` gets the job's cancel token, which `kill` and
    /// `clear` fire, and the sink its output goes to.
    pub fn spawn_bash<F>(
        self: &Rc<Self>,
        command: String,
        cwd: PathBuf,
        run: impl FnOnce(CancellationToken, OutputSink) -> F,
    ) -> String
    where
        F: Future<Output = BackgroundResult> + 'static,
    {
        let id = self.unique_id(&format!("bg_{}", self.next_seq.get() + 1));
        let cancel = CancellationToken::new();
        let line = command_line(&command);
        let sink = OutputSink::new(id.clone(), cwd.clone(), self.events.clone());
        let spec = Spec {
            id: id.clone(),
            agent: "bash".into(),
            kind: JobKind::Bash,
            cwd,
            task: line.clone(),
            command: Some(command),
        };
        let run_cancel = cancel.clone();
        self.spawn_job(spec, line, cancel, move |_| {
            let fut = run(run_cancel, sink.clone());
            async move {
                let r = fut.await;
                sink.flush();
                Finished { ok: r.ok, text: r.text, note: Some(r.note) }
            }
        });
        id
    }

    /// Finished, undelivered jobs; marks them delivered.
    pub fn take_undelivered(&self) -> Vec<Done> {
        let done: Vec<Done> = self
            .inner
            .borrow_mut()
            .iter_mut()
            .filter(|j| j.state != SubagentState::Running && !j.delivered)
            .map(|j| {
                j.delivered = true;
                Done {
                    id: j.spec.id.clone(),
                    agent: j.spec.agent.clone(),
                    ok: j.state == SubagentState::Done,
                    // Delivered output is not kept: only the snapshot fields stay resident.
                    text: std::mem::take(&mut j.text),
                    command: j.spec.command.take(),
                    note: j.note.clone(),
                }
            })
            .collect();
        if !done.is_empty() {
            self.emit();
        }
        done
    }

    pub fn has_undelivered(&self) -> bool {
        self.inner.borrow().iter().any(|j| j.state != SubagentState::Running && !j.delivered)
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
    fn finish(&self, id: &str, ok: bool, text: &str) {
        if let Some(j) = self.inner.borrow_mut().iter_mut().find(|j| j.spec.id == id) {
            j.state = if ok { SubagentState::Done } else { SubagentState::Failed };
            j.text = text.into();
        }
        self.notify.notify_waiters();
    }

    #[cfg(test)]
    pub(crate) fn push_running(&self, id: &str) {
        self.inner.borrow_mut().push(Job {
            seq: 0,
            spec: Spec {
                id: id.into(),
                agent: "scout".into(),
                kind: JobKind::Subagent,
                cwd: PathBuf::new(),
                task: String::new(),
                command: None,
            },
            tools: 0,
            last: String::new(),
            state: SubagentState::Running,
            text: String::new(),
            started: SystemTime::now(),
            ended: None,
            note: None,
            session: None,
            model: None,
            delivered: false,
            cancel: CancellationToken::new(),
        });
    }
}

/// A command on one line for headings and the jobs list.
fn command_line(command: &str) -> String {
    truncate_text(&command.split_whitespace().collect::<Vec<_>>().join(" "), 120)
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

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn handle() -> Rc<JobsHandle> {
        let (wake, _) = tokio::sync::mpsc::unbounded_channel();
        let (events, _) = tokio::sync::mpsc::unbounded_channel();
        JobsHandle::new(wake, events)
    }

    fn done(id: &str, ok: bool, text: &str) -> Done {
        Done { id: id.into(), agent: "scout".into(), ok, text: text.into(), command: None, note: None }
    }

    #[test]
    fn delivered_output_is_released() {
        let h = handle();
        h.inner.borrow_mut().push(Job {
            seq: 0,
            spec: Spec {
                id: "A".into(),
                agent: "scout".into(),
                kind: JobKind::Bash,
                cwd: PathBuf::from("/w"),
                task: "echo".into(),
                command: Some("echo".into()),
            },
            tools: 0,
            last: String::new(),
            state: SubagentState::Done,
            text: "big output".into(),
            started: SystemTime::now(),
            ended: Some(SystemTime::now()),
            note: Some("exit 0".into()),
            session: None,
            model: None,
            delivered: false,
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
        assert!(h.inner.borrow()[0].spec.command.is_none());
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
                h.spawn("A".into(), "scout".into(), exe, base.clone(), serde_json::json!({"task": "find  x\nnow"}));
                assert_eq!(h.finished().await.len(), 1);
            })
            .await;
        let snap = h.snapshot();
        assert_eq!(snap[0].session.as_deref(), Some(std::path::Path::new("/s/child.jsonl")));
        assert_eq!(snap[0].task, "find x now", "the task is one line");
        assert_eq!((snap[0].cwd.as_path(), snap[0].state.clone()), (base.as_path(), SubagentState::Done));
        let _ = std::fs::remove_dir_all(&base);
    }

    fn sink() -> (OutputSink, tokio::sync::mpsc::UnboundedReceiver<AgentEvent>) {
        let (events, rx) = tokio::sync::mpsc::unbounded_channel();
        (OutputSink::new("bg_1".into(), PathBuf::from("/"), events), rx)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn output_goes_out_at_once_then_throttled_and_never_unsent() {
        let (sink, mut rx) = sink();
        tokio::task::LocalSet::new()
            .run_until(async {
                sink.push("one\n");
                assert!(
                    matches!(rx.try_recv(), Ok(AgentEvent::JobOutput { id, tail }) if id == "bg_1" && tail == "one\n")
                );
                sink.push("two\n");
                sink.push("three\n");
                assert!(rx.try_recv().is_err(), "chunks inside the window wait");
                tokio::time::sleep(OUTPUT_EVERY + Duration::from_millis(150)).await;
                match rx.try_recv() {
                    Ok(AgentEvent::JobOutput { tail, .. }) => assert_eq!(tail, "one\ntwo\nthree\n"),
                    _ => panic!("the deferred update never came"),
                }
                assert!(rx.try_recv().is_err(), "one update covers both chunks");
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
