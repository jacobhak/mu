//! Background subagent jobs: `task` returns ids at once, finished results are delivered to the
//! model as a `<system-notice>` message, and `wait` blocks until a job finishes.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::Notify;
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

use crate::events::{AgentEvent, EventTx, SubagentState, SubagentStatus};
use crate::tools::task::run_child;

const WAIT_LIMIT: Duration = Duration::from_secs(30 * 60);

struct Job {
    /// Distinguishes a job from a later one that reuses its id after `clear`.
    seq: u64,
    id: String,
    agent: String,
    tools: u32,
    last: String,
    state: SubagentState,
    text: String,
    delivered: bool,
    cancel: CancellationToken,
}

pub struct Done {
    pub id: String,
    pub agent: String,
    pub ok: bool,
    pub text: String,
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

    pub fn snapshot(&self) -> Vec<SubagentStatus> {
        self.inner
            .borrow()
            .iter()
            .map(|j| SubagentStatus { name: j.id.clone(), agent: j.agent.clone(), tools: j.tools, last: j.last.clone(), state: j.state.clone() })
            .collect()
    }

    fn emit(&self) {
        let _ = self.events.send(AgentEvent::Jobs(self.snapshot()));
    }

    pub fn spawn(self: &Rc<Self>, id: String, agent: String, exe: PathBuf, cwd: PathBuf, payload: Value) {
        let seq = self.next_seq.get();
        self.next_seq.set(seq + 1);
        let cancel = CancellationToken::new();
        self.inner.borrow_mut().push(Job {
            seq,
            id,
            agent,
            tools: 0,
            last: String::new(),
            state: SubagentState::Running,
            text: String::new(),
            delivered: false,
            cancel: cancel.clone(),
        });
        self.emit();
        let me = self.clone();
        tokio::task::spawn_local(async move {
            let out = run_child(&exe, &cwd, &payload, &cancel, |name, summary| {
                if let Some(j) = me.inner.borrow_mut().iter_mut().find(|j| j.seq == seq) {
                    j.tools += 1;
                    j.last = format!("{name} {summary}");
                }
                me.emit();
            })
            .await;
            let (ok, text) = if cancel.is_cancelled() { (false, "cancelled".to_string()) } else { (out.ok, out.text) };
            if let Some(j) = me.inner.borrow_mut().iter_mut().find(|j| j.seq == seq) {
                j.state = if ok { SubagentState::Done } else { SubagentState::Failed };
                j.text = text;
            }
            me.notify.notify_waiters();
            let _ = me.wake.send(());
            me.emit();
        });
    }

    /// Finished, undelivered jobs (only `ids` when non-empty); marks them delivered.
    pub fn take_undelivered(&self, ids: &[String]) -> Vec<Done> {
        self.inner
            .borrow_mut()
            .iter_mut()
            .filter(|j| j.state != SubagentState::Running && !j.delivered && (ids.is_empty() || ids.contains(&j.id)))
            .map(|j| {
                j.delivered = true;
                Done { id: j.id.clone(), agent: j.agent.clone(), ok: j.state == SubagentState::Done, text: j.text.clone() }
            })
            .collect()
    }

    pub fn has_undelivered(&self) -> bool {
        self.inner.borrow().iter().any(|j| j.state != SubagentState::Running && !j.delivered)
    }

    /// Cancels running jobs (all when `id` is `None`); returns how many.
    pub fn kill(&self, id: Option<&str>) -> usize {
        let jobs = self.inner.borrow();
        let targets: Vec<&Job> = jobs.iter().filter(|j| j.state == SubagentState::Running && id.is_none_or(|i| i == j.id)).collect();
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
    fn push_running(&self, id: &str) {
        self.inner.borrow_mut().push(Job {
            seq: 0,
            id: id.into(),
            agent: "scout".into(),
            tools: 0,
            last: String::new(),
            state: SubagentState::Running,
            text: String::new(),
            delivered: false,
            cancel: CancellationToken::new(),
        });
    }
}

/// One job's result in the same shape the blocking `task` result uses.
pub fn result_text(d: &Done) -> String {
    format!("## {} ({}) — {}\n{}", d.id, d.agent, if d.ok { "ok" } else { "failed" }, d.text)
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
    let body = done.iter().map(|d| format!("── Job {} ({}) ──\n{}", d.id, d.agent, result_text(d))).collect::<Vec<_>>().join("\n");
    format!(
        "<system-notice>\n{} background jobs have completed. Resume your work using the results below.\n\n{body}\n</system-notice>",
        done.len()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handle() -> Rc<JobsHandle> {
        let (wake, _) = tokio::sync::mpsc::unbounded_channel();
        let (events, _) = tokio::sync::mpsc::unbounded_channel();
        JobsHandle::new(wake, events)
    }

    fn done(id: &str, ok: bool, text: &str) -> Done {
        Done { id: id.into(), agent: "scout".into(), ok, text: text.into() }
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
