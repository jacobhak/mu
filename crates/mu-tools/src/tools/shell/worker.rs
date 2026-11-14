//! The persistent in-process shell, on threads of its own.
//!
//! mu's runtime has one thread, shared by the UI, the agent and every tool. The shell runs builtins
//! (`grep`, `sed`, ...) that do synchronous I/O; one that blocks, or a pipeline whose stages wait
//! on each other, would stop the redraw, Ctrl-C and every timeout with it. So the shell lives on a
//! small runtime of its own: a call awaits the worker, the UI thread never blocks on it, and a
//! call that never returns is given up on and the shell replaced.
//!
//! The runtime is created by the first call and kept: background jobs (`cmd &`) are tasks on it.

use pi_shell::{
    cancel::CancelToken,
    shell::{Shell, ShellRunOptions, ShellRunResult},
};
use std::cell::RefCell;
use std::future::Future;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinHandle;

/// How long past a timeout or an abort a call may stay unanswered before the shell is declared
/// hung. The shell honours both itself within moments unless a thread of it is stuck.
pub const HANG_GRACE: Duration = Duration::from_secs(5);

/// Two threads: a builtin that blocks one still leaves the other to run the stage it waits for.
const WORKERS: usize = 2;

pub type RunResult = Result<ShellRunResult, String>;

struct Inner {
    shell: Arc<Shell>,
    rt: Option<tokio::runtime::Runtime>,
}

impl Inner {
    fn start() -> Self {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(WORKERS)
            .thread_name("mu-shell")
            .enable_all()
            .build()
            .expect("the shell runtime starts");
        Self { shell: Arc::new(Shell::new(None)), rt: Some(rt) }
    }

    fn handle(&self) -> &tokio::runtime::Handle {
        self.rt.as_ref().expect("the runtime lives until drop").handle()
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        // Never joins: a thread stuck in a syscall is left behind, and this may run inside the UI's
        // runtime, where a joining drop panics.
        if let Some(rt) = self.rt.take() {
            rt.shutdown_background();
        }
    }
}

pub struct ShellWorker {
    slot: Rc<RefCell<Option<Arc<Inner>>>>,
    grace: Duration,
}

impl Default for ShellWorker {
    fn default() -> Self {
        Self::with_grace(HANG_GRACE)
    }
}

impl ShellWorker {
    /// A worker whose runs get `grace` past a timeout or abort before the shell is declared hung.
    pub fn with_grace(grace: Duration) -> Self {
        Self { slot: Default::default(), grace }
    }

    fn inner(&self) -> Arc<Inner> {
        self.slot.borrow_mut().get_or_insert_with(|| Arc::new(Inner::start())).clone()
    }

    /// Starts one run on the worker. Dropping the `Run` abandons it.
    pub fn spawn(&self, opts: ShellRunOptions, on_chunk: Option<flume::Sender<String>>, cancel: CancelToken) -> Run {
        let inner = self.inner();
        let shell = inner.shell.clone();
        let task =
            inner.handle().spawn(async move { shell.run(opts, on_chunk, cancel).await.map_err(|e| e.to_string()) });
        Run { task, inner, slot: self.slot.clone(), grace: self.grace }
    }

    /// Runs `opts` in a fork of the persistent shell: its state as of the last command boundary,
    /// without waiting for a running command. Nothing the run sets reaches the persistent shell.
    /// Only `Run::join` may be used on the result: `Run::hung` would kill the persistent shell's
    /// pids, which the fork does not own.
    pub fn spawn_fork(
        &self,
        opts: ShellRunOptions,
        on_chunk: Option<flume::Sender<String>>,
        cancel: CancelToken,
    ) -> Run {
        let inner = self.inner();
        let shell = inner.shell.clone();
        let task = inner.handle().spawn(async move {
            let fork = shell.fork();
            fork.run(opts, on_chunk, cancel).await.map_err(|e| e.to_string())
        });
        Run { task, inner, slot: self.slot.clone(), grace: self.grace }
    }

    /// Test seam: stops every worker thread for `secs` seconds, as a blocking builtin would.
    #[cfg(test)]
    pub fn jam(&self, secs: u64) {
        let inner = self.inner();
        let barrier = Arc::new(std::sync::Barrier::new(WORKERS + 1));
        for _ in 0..WORKERS {
            let b = barrier.clone();
            inner.handle().spawn(async move {
                b.wait();
                std::thread::sleep(Duration::from_secs(secs));
            });
        }
        barrier.wait();
    }
}

pub struct Run {
    task: JoinHandle<RunResult>,
    inner: Arc<Inner>,
    slot: Rc<RefCell<Option<Arc<Inner>>>>,
    grace: Duration,
}

impl Drop for Run {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Run {
    /// Waits for the run. No hang detection: for a caller that bounds the wait itself and abandons
    /// the run on expiry.
    pub async fn join(&mut self) -> RunResult {
        joined((&mut self.task).await)
    }

    /// Waits for the run. `cancelled` completing (once) calls `on_cancel`, the caller's cue to
    /// abort the run; the run then has the worker's grace to end. `limit` is how long the run may take
    /// at all, its own timeout included. An expired wait declares the shell hung: it is abandoned
    /// and the next run starts a new one (its directory and variables reset).
    pub async fn finish(
        &mut self,
        cancelled: impl Future<Output = ()>,
        on_cancel: impl FnOnce(),
        limit: Option<Duration>,
    ) -> RunResult {
        let hung = async {
            match limit {
                Some(d) => tokio::time::sleep(d + self.grace).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            r = &mut self.task => return joined(r),
            () = cancelled => on_cancel(),
            () = hung => return self.hung(),
        }
        tokio::select! {
            r = &mut self.task => joined(r),
            () = tokio::time::sleep(self.grace) => self.hung(),
        }
    }

    fn hung(&mut self) -> RunResult {
        self.task.abort();
        // The stuck thread cannot be stopped; what it started can.
        for pid in self.inner.shell.pids() {
            let _ = mu_base::process::signal_pid(pid as u32, libc::SIGKILL);
        }
        let mut slot = self.slot.borrow_mut();
        if slot.as_ref().is_some_and(|s| Arc::ptr_eq(s, &self.inner)) {
            *slot = None;
        }
        Err("the shell stopped responding and was restarted; its directory and exported variables were reset".into())
    }
}

fn joined(r: Result<RunResult, tokio::task::JoinError>) -> RunResult {
    r.unwrap_or_else(|e| Err(format!("the shell task failed: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(w: &ShellWorker, command: &str, timeout_ms: u32) -> Run {
        let opts = ShellRunOptions { command: command.into(), timeout_ms: Some(timeout_ms), ..Default::default() };
        w.spawn(opts, None, CancelToken::new(Some(timeout_ms)))
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_run_returns_its_result_to_a_single_threaded_caller() {
        let w = ShellWorker::default();
        let r = run(&w, "true", 5000).join().await.unwrap();
        assert_eq!(r.exit_code, Some(0));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn the_shell_persists_between_runs() {
        let w = ShellWorker::default();
        run(&w, "X_WORKER_TEST=kept", 5000).join().await.unwrap();
        let r = run(&w, "test \"$X_WORKER_TEST\" = kept", 5000).join().await.unwrap();
        assert_eq!(r.exit_code, Some(0));
    }

    /// A fork starts from the state before a command that is still running, without waiting for it,
    /// and what it sets never reaches the persistent shell.
    #[tokio::test(flavor = "current_thread")]
    async fn a_fork_sees_state_from_before_a_running_command_and_changes_nothing() {
        let w = ShellWorker::default();
        run(&w, "X_FORK=a", 5000).join().await.unwrap();
        let started = std::env::temp_dir().join(format!("mu-worker-fork-{}", uuid::Uuid::new_v4()));
        let mut busy = run(&w, &format!("X_FORK=b; touch {}; sleep 2", started.display()), 5000);
        mu_testutil::wait_until("the busy command to start", || started.exists()).await;
        let _ = std::fs::remove_file(&started);
        let opts = ShellRunOptions {
            command: "test \"$X_FORK\" = a && X_FORK=c".into(),
            timeout_ms: Some(5000),
            ..Default::default()
        };
        let fork = w.spawn_fork(opts, None, CancelToken::new(Some(5000))).join().await.unwrap();
        assert_eq!(fork.exit_code, Some(0), "the fork saw the pre-command state");
        assert_eq!(busy.join().await.unwrap().exit_code, Some(0));
        let after = run(&w, "test \"$X_FORK\" = b", 5000).join().await.unwrap();
        assert_eq!(after.exit_code, Some(0), "the fork's change stayed in the fork");
    }

    /// The caller's thread keeps running while every thread of the shell is stuck; the call is given
    /// up on once its limit and grace pass, and the next call gets a working shell.
    #[tokio::test(flavor = "current_thread")]
    async fn a_hung_shell_is_given_up_on_and_replaced() {
        let w = ShellWorker::with_grace(Duration::from_millis(200));
        w.jam(30);
        let mut r = run(&w, "true", 1000);
        let err = r.finish(std::future::pending(), || {}, Some(Duration::from_millis(100))).await.unwrap_err();
        assert!(err.contains("stopped responding"), "{err}");
        let r = run(&w, "true", 5000).join().await.unwrap();
        assert_eq!(r.exit_code, Some(0));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn an_abort_the_shell_never_answers_is_given_up_on() {
        let w = ShellWorker::with_grace(Duration::from_millis(200));
        w.jam(30);
        let mut r = run(&w, "true", 60_000);
        let mut cued = false;
        let err = r.finish(std::future::ready(()), || cued = true, None).await.unwrap_err();
        assert!(cued && err.contains("stopped responding"), "{err}");
    }

    /// A pipeline that pushes far more than a pipe holds between builtin stages: the shape that
    /// deadlocked a one-thread runtime (jacobhak/mu#19). Whether it does depends on how the reader
    /// happens to consume the pipe; session transcripts triggered it, this data does not, so this
    /// is a smoke test.
    #[tokio::test(flavor = "current_thread")]
    async fn a_large_pipeline_between_builtins_completes() {
        let dir = std::env::temp_dir().join(format!("mu-worker-pipe-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // Lines of very different sizes, most larger than a pipe page.
        for f in 0..4 {
            let big: String = (0..400u64)
                .map(|i| format!("summary {}\n", "x".repeat((50 + (i * 2654435761 + f) % 80_000) as usize)))
                .collect();
            std::fs::write(dir.join(format!("big{f}")), big).unwrap();
        }
        let w = ShellWorker::default();
        let cmd = format!("grep -rn 'summary\\|compaction' {}/* 2>/dev/null | grep -c 'x'", dir.display());
        let (tx, rx) = flume::unbounded();
        let opts = ShellRunOptions { command: cmd, timeout_ms: Some(20_000), ..Default::default() };
        let mut r = w.spawn(opts, Some(tx), CancelToken::new(Some(20_000)));
        let res = r.finish(std::future::pending(), || {}, Some(Duration::from_secs(20))).await.unwrap();
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(res.exit_code, Some(0));
        assert_eq!(rx.drain().collect::<String>().trim(), "1600");
    }
}
