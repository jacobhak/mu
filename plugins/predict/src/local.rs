//! The engine in this process: set up from the profile's corpus, and the methods it answers. The daemon runs
//! it for every session; a plugin runs it only when it cannot use the daemon.

use crate::engine::State;
use crate::worker::{self, Job, Mailbox};
use crate::{corpus, notice, settings, smollm, weights};
use mu_common::rpc::RpcError;
use serde_json::{Value, json};
use smollm::Start;
use std::path::Path;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::oneshot;

static MAILBOX: OnceLock<Arc<Mailbox<Job>>> = OnceLock::new();

pub fn rpc_err(code: i64, message: impl Into<String>) -> RpcError {
    RpcError::new(code, message)
}

fn mailbox() -> Result<&'static Arc<Mailbox<Job>>, RpcError> {
    MAILBOX.get().ok_or_else(|| rpc_err(-32002, "not initialized"))
}

/// Builds the n-gram synchronously, starts SmolLM in the background when it is selected and its weights are
/// there, and starts the worker that owns the engines.
pub fn initialize(agent_dir: &Path, mu_home: &Path) -> Result<(), String> {
    if MAILBOX.get().is_some() {
        return Ok(());
    }
    let profile_home = mu_common::paths::profile_home();
    let state_dir = profile_home.join("predict");
    let mut st = State::open(&state_dir)?;
    let cfg = settings::read(mu_home);
    let rows = corpus::load_corpus(agent_dir, &profile_home, &mu_common::paths::home(), cfg.other_histories);
    for r in &rows {
        st.observe_n(&r.text, &r.cwd, r.count);
    }
    let mb = Arc::new(Mailbox::default());
    let smollm_dir = state_dir.join("smollm");
    let loading = match smollm::start(&cfg, &weights::model_dir(mu_home), &weights::FILES, &smollm_dir, &rows) {
        Start::Ngram => false,
        Start::Fallback(note) => {
            notice::warn(note);
            false
        }
        Start::Load { history } => {
            let (mb, dir, threads) = (mb.clone(), weights::model_dir(mu_home), cfg.threads);
            let spawned = std::thread::Builder::new().name("predict-load".into()).spawn(move || {
                let t = Instant::now();
                let r = smollm::open(&dir, &smollm_dir, threads, &history);
                match &r {
                    Ok(_) => eprintln!("predict: SmolLM loaded in {:.0} ms", t.elapsed().as_secs_f64() * 1000.0),
                    Err(e) => eprintln!("predict: SmolLM failed to load: {e}"),
                }
                mb.push(Job::Attach(r));
            });
            spawned.is_ok()
        }
    };
    let worker_mb = mb.clone();
    std::thread::Builder::new()
        .name("predict".into())
        .spawn(move || worker::run(worker_mb, st, loading, state_dir))
        .map_err(|e| format!("cannot start the predict worker: {e}"))?;
    let _ = MAILBOX.set(mb);
    Ok(())
}

/// Waits (briefly) for the worker to persist SmolLM's state, so a shutdown keeps what was learned.
pub async fn shutdown() {
    let Some(mb) = MAILBOX.get() else { return };
    let (reply, rx) = oneshot::channel();
    mb.push(Job::Shutdown(reply));
    let _ = tokio::time::timeout(Duration::from_secs(5), rx).await;
}

/// As `shutdown`, for a thread that is not async (the daemon's).
pub fn shutdown_blocking() {
    let Some(mb) = MAILBOX.get() else { return };
    let (reply, rx) = oneshot::channel();
    mb.push(Job::Shutdown(reply));
    let (tx, done) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = rx.blocking_recv();
        let _ = tx.send(());
    });
    let _ = done.recv_timeout(Duration::from_secs(5));
}

/// What a method gives back: the answer, or a ghost still being computed.
pub enum Submitted {
    Done(Value),
    Ghost(oneshot::Receiver<String>),
}

pub fn ghost_reply(ghost: String) -> Value {
    json!({ "ghost": ghost })
}

/// Queues the work of `predict/complete` and `predict/observe` for `client`. A newer ghost request from the same
/// client overtakes its older ones; another client's are left alone.
pub fn submit(method: &str, p: &Value, client: &str) -> Result<Submitted, RpcError> {
    let s = |k: &str| p.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
    match method {
        "predict/complete" => {
            let threshold = p.get("threshold").and_then(Value::as_f64).unwrap_or(0.3) as f32;
            let (reply, rx) = oneshot::channel();
            let job = Job::Complete {
                client: client.to_owned(),
                text: s("text"),
                cwd: s("cwd"),
                assistant: s("assistant"),
                threshold,
                reply,
            };
            worker::queue_complete(mailbox()?, job);
            Ok(Submitted::Ghost(rx))
        }
        "predict/observe" => {
            mailbox()?.push(Job::Observe { text: s("text"), cwd: s("cwd") });
            Ok(Submitted::Done(json!({})))
        }
        other => Err(rpc_err(-32601, format!("unknown method {other}"))),
    }
}
