//! The plugin's one worker thread. It owns the engine and runs jobs in arrival order. A ghost request that
//! a newer one has overtaken while queued is answered with no ghost without being computed; the request
//! being computed is never interrupted.

use crate::engine::State;
use pi_predict::Predictor;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use tokio::sync::oneshot;

/// Most prompts kept for SmolLM while it loads; older ones are dropped beyond this.
const WAITING_MAX: usize = 256;

pub enum Job {
    Complete {
        /// Who asked: a newer request overtakes only that client's older ones.
        client: String,
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

/// Queues a ghost request, answering any older ghost request of the same client still waiting in the queue
/// with no ghost.
pub fn queue_complete(mb: &Mailbox<Job>, job: Job) {
    let who = match &job {
        Job::Complete { client, .. } => client.clone(),
        _ => String::new(),
    };
    for old in mb.push_superseding(job, |j| matches!(j, Job::Complete { client, .. } if *client == who)) {
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
            Job::Complete { text, cwd, assistant, threshold, reply, .. } => {
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
                crate::notice::warn(crate::smollm::fallback_notice(&format!("SmolLM failed to load ({why})")));
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
        complete_for("", text)
    }

    fn complete_for(client: &str, text: &str) -> (Job, oneshot::Receiver<String>) {
        let (reply, rx) = oneshot::channel();
        let job = Job::Complete {
            client: client.into(),
            text: text.into(),
            cwd: String::new(),
            assistant: String::new(),
            threshold: 0.3,
            reply,
        };
        (job, rx)
    }

    #[test]
    fn a_client_overtakes_only_its_own_requests() {
        let mb: Mailbox<Job> = Mailbox::default();
        let (a1, mut r_a1) = complete_for("a", "a1");
        let (b1, mut r_b1) = complete_for("b", "b1");
        let (a2, _r_a2) = complete_for("a", "a2");
        queue_complete(&mb, a1);
        queue_complete(&mb, b1);
        queue_complete(&mb, a2);
        assert_eq!(r_a1.try_recv().unwrap(), "", "a's older request was overtaken by a's newer one");
        assert!(r_b1.try_recv().is_err(), "b's request is still queued, not answered");
        let texts: Vec<String> = (0..2)
            .map(|_| match mb.pop() {
                Job::Complete { text, .. } => text,
                _ => String::new(),
            })
            .collect();
        assert_eq!(texts, ["b1", "a2"]);
    }

    /// A worker whose fake engine blocks on `release` for each ghost; it reports each computation on `started`.
    fn slow_worker(
        mb: Arc<Mailbox<Job>>,
        started: mpsc::Sender<String>,
        release: mpsc::Receiver<()>,
    ) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            loop {
                match mb.pop() {
                    Job::Complete { text, reply, .. } => {
                        started.send(text.clone()).unwrap();
                        release.recv_timeout(Duration::from_secs(30)).expect("test releases the engine");
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
        let (release_tx, release) = mpsc::channel();
        let worker = slow_worker(mb.clone(), started_tx, release);

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
        release_tx.send(()).unwrap();
        release_tx.send(()).unwrap();

        assert_eq!(rx_b.blocking_recv().unwrap(), "", "b was overtaken before it started");
        assert_eq!(rx_c.blocking_recv().unwrap(), "", "c was overtaken before it started");
        assert_eq!(rx_a.blocking_recv().unwrap(), "ghost:a", "the request in flight finishes");
        assert_eq!(rx_d.blocking_recv().unwrap(), "ghost:d", "the newest is computed");
        assert_eq!(started.recv_timeout(Duration::from_secs(5)).unwrap(), "d", "only d reached the engine after a");
        let (done, rx) = oneshot::channel();
        mb.push(Job::Shutdown(done));
        rx.blocking_recv().unwrap();
        worker.join().unwrap();
        assert!(started.try_recv().is_err(), "b and c never reached the engine");
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
