//! Background jobs: `task` (subagents) and `bash` with `async` return ids at once; finished results
//! are delivered to the model as a `<system-notice>` message, and `wait` blocks until a job finishes.

use std::cell::{Cell, RefCell};
use std::future::Future;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, SystemTime};

use serde_json::Value;
use tokio::sync::Notify;
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

use crate::events::{AgentEvent, EventTx, JobKind, SubagentState, SubagentStatus};
use crate::tools::bash::BackgroundResult;
use crate::tools::task::{MAX_TASKS, run_child_observed};
use crate::tui::line::truncate_text;

const WAIT_LIMIT: Duration = Duration::from_secs(30 * 60);

struct Job {
    /// Distinguishes a job from a later one that reuses its id after `clear`.
    seq: u64,
    id: String,
    agent: String,
    kind: JobKind,
    cwd: PathBuf,
    /// The task prompt or command, truncated; kept for the jobs panel.
    task: String,
    started: SystemTime,
    ended: Option<SystemTime>,
    /// The command of a bash job; taken on delivery, like `text`.
    command: Option<String>,
    tools: u32,
    last: String,
    state: SubagentState,
    text: String,
    /// How a bash job ended ("exit 3", "timed out", …).
    note: Option<String>,
    /// A subagent's session file, once the child has opened it.
    session: Option<PathBuf>,
    delivered: bool,
    cancel: CancellationToken,
}

/// What a new job is made of.
struct Spec {
    id: String,
    agent: String,
    kind: JobKind,
    cwd: PathBuf,
    task: String,
    last: String,
    command: Option<String>,
}

pub struct Done {
    pub id: String,
    pub agent: String,
    pub ok: bool,
    pub text: String,
    pub command: Option<String>,
    pub note: Option<String>,
}

pub enum WaitResult {
    Done(Vec<Done>),
    Nothing,
    Cancelled,
    Timeout(Vec<String>),
}

pub struct JobsHandle {
    inner: RefCell<Vec<Job>>,
    next_seq: Cell<u64>,
    notify: Notify,
    wake: UnboundedSender<()>,
    events: EventTx,
}

impl JobsHandle {
    pub fn new(wake: UnboundedSender<()>, events: EventTx) -> Rc<Self> {
        Rc::new(Self { inner: RefCell::default(), next_seq: Cell::new(0), notify: Notify::new(), wake, events })
    }

    /// `base`, or `base-2`, `base-3`, … when taken this session.
    pub fn unique_id(&self, base: &str) -> String {
        let jobs = self.inner.borrow();
        let taken = |id: &str| jobs.iter().any(|j| j.id == id);
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
                name: j.id.clone(),
                agent: j.agent.clone(),
                tools: j.tools,
                last: j.last.clone(),
                state: j.state.clone(),
                kind: j.kind,
                cwd: j.cwd.clone(),
                task: j.task.clone(),
                started: j.started,
                ended: j.ended,
                delivered: j.delivered,
                note: j.note.clone(),
                session: j.session.clone(),
            })
            .collect()
    }

    fn emit(&self) {
        let _ = self.events.send(AgentEvent::Jobs(self.snapshot()));
    }

    fn start(&self, spec: Spec, cancel: CancellationToken) -> u64 {
        let seq = self.next_seq.get();
        self.next_seq.set(seq + 1);
        self.inner.borrow_mut().push(Job {
            seq,
            id: spec.id,
            agent: spec.agent,
            kind: spec.kind,
            cwd: spec.cwd,
            task: spec.task,
            started: SystemTime::now(),
            ended: None,
            command: spec.command,
            tools: 0,
            last: spec.last,
            state: SubagentState::Running,
            text: String::new(),
            note: None,
            session: None,
            delivered: false,
            cancel,
        });
        self.emit();
        seq
    }

    /// Records a finished job and wakes an idle agent.
    fn complete(&self, seq: u64, ok: bool, text: String, note: Option<String>) {
        if let Some(j) = self.inner.borrow_mut().iter_mut().find(|j| j.seq == seq) {
            j.state = if ok { SubagentState::Done } else { SubagentState::Failed };
            j.ended = Some(SystemTime::now());
            j.text = text;
            j.note = note;
        }
        self.notify.notify_waiters();
        let _ = self.wake.send(());
        self.emit();
    }

    pub fn spawn(self: &Rc<Self>, id: String, agent: String, exe: PathBuf, cwd: PathBuf, payload: Value) {
        let cancel = CancellationToken::new();
        let task = command_line(payload["task"].as_str().unwrap_or(""));
        let spec = Spec {
            id,
            agent: agent.clone(),
            kind: JobKind::Subagent,
            cwd: cwd.clone(),
            task,
            last: String::new(),
            command: None,
        };
        let seq = self.start(spec, cancel.clone());
        let me = self.clone();
        tokio::task::spawn_local(async move {
            let out = run_child_observed(
                &exe,
                &cwd,
                &payload,
                &cancel,
                |name, summary| {
                    if let Some(j) = me.inner.borrow_mut().iter_mut().find(|j| j.seq == seq) {
                        j.tools += 1;
                        j.last = format!("{name} {summary}");
                    }
                    me.emit();
                },
                |path| {
                    if let Some(j) = me.inner.borrow_mut().iter_mut().find(|j| j.seq == seq) {
                        j.session = Some(PathBuf::from(path));
                    }
                    me.emit();
                },
            )
            .await;
            // `run_child` already says "cancelled" when a cancelled child produced no result; a child
            // that finished before the kill landed keeps its real result.
            me.complete(seq, out.ok, out.text, None);
        });
    }

    /// Starts a bash job and returns its id. `run` gets the job's cancel token, which `kill` and
    /// `clear` fire.
    pub fn spawn_bash<F>(
        self: &Rc<Self>,
        command: String,
        cwd: PathBuf,
        run: impl FnOnce(CancellationToken) -> F,
    ) -> String
    where
        F: Future<Output = BackgroundResult> + 'static,
    {
        let id = self.unique_id(&format!("bg_{}", self.next_seq.get() + 1));
        let cancel = CancellationToken::new();
        let line = command_line(&command);
        let spec = Spec {
            id: id.clone(),
            agent: "bash".into(),
            kind: JobKind::Bash,
            cwd,
            task: line.clone(),
            last: line,
            command: Some(command),
        };
        let seq = self.start(spec, cancel.clone());
        let fut = run(cancel);
        let me = self.clone();
        tokio::task::spawn_local(async move {
            let r = fut.await;
            me.complete(seq, r.ok, r.text, Some(r.note));
        });
        id
    }

    /// Finished, undelivered jobs (only `ids` when non-empty); marks them delivered.
    pub fn take_undelivered(&self, ids: &[String]) -> Vec<Done> {
        let done: Vec<Done> = self
            .inner
            .borrow_mut()
            .iter_mut()
            .filter(|j| j.state != SubagentState::Running && !j.delivered && (ids.is_empty() || ids.contains(&j.id)))
            .map(|j| {
                j.delivered = true;
                Done {
                    id: j.id.clone(),
                    agent: j.agent.clone(),
                    ok: j.state == SubagentState::Done,
                    // Delivered output is not kept: only the snapshot fields stay resident.
                    text: std::mem::take(&mut j.text),
                    command: j.command.take(),
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
            jobs.iter().filter(|j| j.state == SubagentState::Running && id.is_none_or(|i| i == j.id)).collect();
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

    pub async fn wait(&self, ids: &[String], cancel: &CancellationToken) -> WaitResult {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let done = self.take_undelivered(ids);
            if !done.is_empty() {
                return WaitResult::Done(done);
            }
            let running: Vec<String> = self
                .inner
                .borrow()
                .iter()
                .filter(|j| j.state == SubagentState::Running && (ids.is_empty() || ids.contains(&j.id)))
                .map(|j| j.id.clone())
                .collect();
            if running.is_empty() {
                return WaitResult::Nothing;
            }
            tokio::select! {
                _ = &mut notified => {}
                _ = cancel.cancelled() => return WaitResult::Cancelled,
                _ = tokio::time::sleep(WAIT_LIMIT) => return WaitResult::Timeout(running),
            }
        }
    }

    #[cfg(test)]
    fn finish(&self, id: &str, ok: bool, text: &str) {
        if let Some(j) = self.inner.borrow_mut().iter_mut().find(|j| j.id == id) {
            j.state = if ok { SubagentState::Done } else { SubagentState::Failed };
            j.text = text.into();
        }
        self.notify.notify_waiters();
    }

    #[cfg(test)]
    pub(crate) fn push_running(&self, id: &str) {
        self.inner.borrow_mut().push(Job {
            seq: 0,
            id: id.into(),
            agent: "scout".into(),
            kind: JobKind::Subagent,
            cwd: PathBuf::new(),
            task: String::new(),
            started: SystemTime::now(),
            ended: None,
            command: None,
            tools: 0,
            last: String::new(),
            state: SubagentState::Running,
            text: String::new(),
            note: None,
            session: None,
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
            id: "A".into(),
            agent: "scout".into(),
            kind: JobKind::Bash,
            cwd: PathBuf::from("/w"),
            task: "echo".into(),
            started: SystemTime::now(),
            ended: Some(SystemTime::now()),
            command: Some("echo".into()),
            tools: 0,
            last: String::new(),
            state: SubagentState::Done,
            text: "big output".into(),
            note: Some("exit 0".into()),
            session: None,
            delivered: false,
            cancel: CancellationToken::new(),
        });
        let snap = h.snapshot();
        assert_eq!(
            (snap[0].delivered, snap[0].task.as_str(), snap[0].note.as_deref()),
            (false, "echo", Some("exit 0"))
        );
        assert!(snap[0].ended.is_some());
        let got = h.take_undelivered(&[]);
        assert_eq!(got[0].text, "big output");
        assert_eq!((got[0].command.as_deref(), got[0].note.as_deref()), (Some("echo"), Some("exit 0")));
        assert!(h.inner.borrow()[0].text.is_empty());
        assert!(h.inner.borrow()[0].command.is_none());
        assert!(h.take_undelivered(&[]).is_empty());
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
        assert_eq!(h.take_undelivered(&[]).len(), 1);
        match rx.try_recv() {
            Ok(AgentEvent::Jobs(v)) => assert!(v[0].delivered),
            _ => panic!("no snapshot after delivery"),
        }
        assert!(h.take_undelivered(&[]).is_empty());
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
                assert!(matches!(h.wait(&[], &CancellationToken::new()).await, WaitResult::Done(_)));
            })
            .await;
        let snap = h.snapshot();
        assert_eq!(snap[0].session.as_deref(), Some(std::path::Path::new("/s/child.jsonl")));
        assert_eq!(snap[0].task, "find x now", "the task is one line");
        assert_eq!((snap[0].cwd.as_path(), snap[0].state.clone()), (base.as_path(), SubagentState::Done));
        let _ = std::fs::remove_dir_all(&base);
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
    async fn wait_returns_nothing_without_jobs_and_done_once() {
        let h = handle();
        let cancel = CancellationToken::new();
        assert!(matches!(h.wait(&[], &cancel).await, WaitResult::Nothing));
        h.push_running("A");
        h.finish("A", true, "ALPHA");
        match h.wait(&[], &cancel).await {
            WaitResult::Done(d) => assert_eq!((d[0].id.as_str(), d[0].text.as_str(), d[0].ok), ("A", "ALPHA", true)),
            _ => panic!("expected Done"),
        }
        assert!(h.take_undelivered(&[]).is_empty());
        assert!(!h.has_undelivered());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn wait_wakes_when_a_running_job_finishes_and_honours_cancel() {
        let h = handle();
        h.push_running("A");
        let cancel = CancellationToken::new();
        let ids = ["A".to_string()];
        let waiter = h.wait(&ids, &cancel);
        let finisher = async {
            tokio::task::yield_now().await;
            h.finish("A", false, "boom");
        };
        let (res, ()) = tokio::join!(waiter, finisher);
        assert!(matches!(res, WaitResult::Done(d) if !d[0].ok));

        h.push_running("B");
        cancel.cancel();
        assert!(matches!(h.wait(&[], &cancel).await, WaitResult::Cancelled));
    }
}
