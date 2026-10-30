//! The plugin's one worker thread. It owns the engine and runs jobs in arrival order. A ghost request that
//! a newer one has overtaken while queued is answered with no ghost without being computed; the request
//! being computed is never interrupted.

use crate::engine::State;
use pi_predict::Predictor;
use serde_json::json;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use tokio::sync::oneshot;

/// Most prompts kept for SmolLM while it loads; older ones are dropped beyond this.
const WAITING_MAX: usize = 256;

pub enum Job {
    Complete {
        text: String,
        cwd: String,
        assistant: String,
        threshold: f32,
        reply: oneshot::Sender<String>,
    },
    Observe {
        text: String,
        cwd: String,
    },
    /// SmolLM finished loading, or failed with this reason.
    Attach(Result<Box<dyn Predictor>, String>),
    Shutdown(oneshot::Sender<()>),
}

impl Job {
    /// Answers a superseded ghost request with no ghost.
    fn supersede(self) {
        if let Job::Complete { reply, .. } = self {
            let _ = reply.send(String::new());
        }
    }
}

pub struct Mailbox<J> {
    queue: Mutex<VecDeque<J>>,
    ready: Condvar,
}

impl<J> Default for Mailbox<J> {
    fn default() -> Self {
        Self { queue: Mutex::new(VecDeque::new()), ready: Condvar::new() }
    }
}

impl<J> Mailbox<J> {
    fn lock(&self) -> MutexGuard<'_, VecDeque<J>> {
        self.queue.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn push(&self, job: J) {
        self.lock().push_back(job);
        self.ready.notify_one();
    }

    /// Queues `job` after removing every queued job that `superseded` names, and returns those.
    pub fn push_superseding(&self, job: J, superseded: impl Fn(&J) -> bool) -> Vec<J> {
        let mut q = self.lock();
        let mut old = Vec::new();
        let mut i = 0;
        while i < q.len() {
            if superseded(&q[i]) {
                old.extend(q.remove(i));
            } else {
                i += 1;
            }
        }
        q.push_back(job);
        drop(q);
        self.ready.notify_one();
        old
    }

    pub fn try_pop(&self) -> Option<J> {
        self.lock().pop_front()
    }

    pub fn pop(&self) -> J {
        let mut q = self.lock();
        loop {
            if let Some(job) = q.pop_front() {
                return job;
            }
            q = self.ready.wait(q).unwrap_or_else(|e| e.into_inner());
        }
    }
}

/// Queues a ghost request, answering any older ghost request still waiting in the queue with no ghost.
pub fn queue_complete(mb: &Mailbox<Job>, job: Job) {
    for old in mb.push_superseding(job, |j| matches!(j, Job::Complete { .. })) {
        old.supersede();
    }
}

/// Runs jobs until `Shutdown`. `loading` says SmolLM is still opening; observed prompts are kept for it.
pub fn run(mb: Arc<Mailbox<Job>>, mut st: State, loading: bool, state_dir: PathBuf) {
    let mut waiting: Option<Vec<String>> = loading.then(Vec::new);
    // SmolLM's head is re-scored after every new prompt; `warm` pays that while the queue is idle.
    let mut warm = false;
    loop {
        let job = match mb.try_pop() {
            Some(job) => job,
            None if warm => {
                st.warm();
                warm = false;
                continue;
            }
            None => mb.pop(),
        };
        match job {
            Job::Complete { text, cwd, assistant, threshold, reply } => {
                let _ = reply.send(st.complete(&text, &cwd, &assistant, threshold));
            }
            Job::Observe { text, cwd } => {
                if let Some(w) = waiting.as_mut() {
                    if w.len() == WAITING_MAX {
                        w.remove(0);
                    }
                    w.push(text.clone());
                }
                st.observe(&text, &cwd);
                warm = true;
            }
            Job::Attach(Ok(p)) => {
                st.attach_smollm(p, &waiting.take().unwrap_or_default());
                warm = true;
            }
            Job::Attach(Err(why)) => {
                waiting = None;
                let note = crate::smollm::fallback_notice(&format!("SmolLM failed to load ({why})"));
                mu_common::rpc::notify("ui/notify", json!({ "level": "warn", "message": note }));
            }
            Job::Shutdown(reply) => {
                if let Err(e) = st.persist() {
                    eprintln!("predict: cannot save SmolLM state in {}: {e}", state_dir.display());
                }
                let _ = reply.send(());
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    fn complete(text: &str) -> (Job, oneshot::Receiver<String>) {
        let (reply, rx) = oneshot::channel();
        let job =
            Job::Complete { text: text.into(), cwd: String::new(), assistant: String::new(), threshold: 0.3, reply };
        (job, rx)
    }

    /// A worker with a fake engine that takes 150 ms per ghost; it reports each computation on `started`.
    fn slow_worker(mb: Arc<Mailbox<Job>>, started: mpsc::Sender<String>) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            loop {
                match mb.pop() {
                    Job::Complete { text, reply, .. } => {
                        started.send(text.clone()).unwrap();
                        std::thread::sleep(Duration::from_millis(150));
                        let _ = reply.send(format!("ghost:{text}"));
                    }
                    Job::Shutdown(reply) => {
                        let _ = reply.send(());
                        return;
                    }
                    _ => {}
                }
            }
        })
    }

    #[test]
    fn overtaken_ghost_requests_are_answered_empty_without_being_computed() {
        let mb = Arc::new(Mailbox::default());
        let (started_tx, started) = mpsc::channel();
        let worker = slow_worker(mb.clone(), started_tx);

        let (a, rx_a) = complete("a");
        queue_complete(&mb, a);
        assert_eq!(started.recv_timeout(Duration::from_secs(5)).unwrap(), "a", "a is in flight");

        // Typing faster than the engine: b and c are overtaken while a is computed.
        let (b, rx_b) = complete("b");
        queue_complete(&mb, b);
        let (c, rx_c) = complete("c");
        queue_complete(&mb, c);
        let (d, rx_d) = complete("d");
        queue_complete(&mb, d);

        assert_eq!(rx_b.blocking_recv().unwrap(), "", "b was overtaken before it started");
        assert_eq!(rx_c.blocking_recv().unwrap(), "", "c was overtaken before it started");
        assert_eq!(rx_a.blocking_recv().unwrap(), "ghost:a", "the request in flight finishes");
        assert_eq!(rx_d.blocking_recv().unwrap(), "ghost:d", "the newest is computed");
        assert_eq!(started.recv_timeout(Duration::from_secs(5)).unwrap(), "d", "only d reached the engine after a");
        assert!(started.recv_timeout(Duration::from_millis(50)).is_err(), "b and c never reached the engine");

        let (done, rx) = oneshot::channel();
        mb.push(Job::Shutdown(done));
        rx.blocking_recv().unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn observes_and_attaches_are_never_superseded() {
        let mb: Mailbox<Job> = Mailbox::default();
        let (reply, _rx) = oneshot::channel();
        mb.push(Job::Shutdown(reply));
        mb.push(Job::Observe { text: "x".into(), cwd: String::new() });
        let (c1, _r1) = complete("one");
        queue_complete(&mb, c1);
        let (c2, _r2) = complete("two");
        queue_complete(&mb, c2);
        let kinds: Vec<&str> = (0..3)
            .map(|_| match mb.pop() {
                Job::Shutdown(_) => "shutdown",
                Job::Observe { .. } => "observe",
                Job::Complete { text, .. } => {
                    if text == "two" {
                        "two"
                    } else {
                        "stale"
                    }
                }
                Job::Attach(_) => "attach",
            })
            .collect();
        assert_eq!(kinds, ["shutdown", "observe", "two"]);
    }
}
