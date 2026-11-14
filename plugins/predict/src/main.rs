//! Predict plugin: ghost-text completion for the composer, `mu-predict setup smollm`, and `mu-predict bench`.

mod bench;
mod corpus;
mod engine;
mod settings;
mod smollm;
mod weights;
mod worker;

use engine::State;
use mu_common::rpc::{Request, RpcError, serve_stdio};
use serde_json::{Value, json};
use smollm::Start;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::oneshot;
use worker::{Job, Mailbox};

static MAILBOX: OnceLock<Arc<Mailbox<Job>>> = OnceLock::new();

fn rpc_err(code: i64, message: impl Into<String>) -> RpcError {
    RpcError::new(code, message)
}

fn mailbox() -> Result<&'static Arc<Mailbox<Job>>, RpcError> {
    MAILBOX.get().ok_or_else(|| rpc_err(-32002, "not initialized"))
}

/// Builds the n-gram synchronously (as before), starts SmolLM in the background when it is selected and
/// its weights are there, and starts the worker that owns the engines.
fn initialize(agent_dir: &Path, mu_home: &Path) -> Result<(), String> {
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
            mu_common::rpc::notify("ui/notify", json!({ "level": "warn", "message": note }));
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
async fn shutdown() {
    let Some(mb) = MAILBOX.get() else { return };
    let (reply, rx) = oneshot::channel();
    mb.push(Job::Shutdown(reply));
    let _ = tokio::time::timeout(Duration::from_secs(5), rx).await;
}

async fn dispatch(req: Request) -> Result<Value, RpcError> {
    let p = &req.params;
    let s = |k: &str| p.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
    match req.method.as_str() {
        "initialize" => {
            initialize(&PathBuf::from(s("agentDir")), &PathBuf::from(s("muHome"))).map_err(|e| rpc_err(-32000, e))?;
            Ok(json!({}))
        }
        "shutdown" => {
            shutdown().await;
            Ok(json!({}))
        }
        "predict/complete" => {
            let threshold = p.get("threshold").and_then(Value::as_f64).unwrap_or(0.3) as f32;
            let (reply, rx) = oneshot::channel();
            let job = Job::Complete { text: s("text"), cwd: s("cwd"), assistant: s("assistant"), threshold, reply };
            worker::queue_complete(mailbox()?, job);
            let ghost = rx.await.unwrap_or_default();
            Ok(json!({ "ghost": ghost }))
        }
        "predict/observe" => {
            mailbox()?.push(Job::Observe { text: s("text"), cwd: s("cwd") });
            Ok(json!({}))
        }
        other => Err(rpc_err(-32601, format!("unknown method {other}"))),
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("bench") => {
            if let Err(e) = bench::run(&args[1..]) {
                eprintln!("mu-predict bench: {e}");
                std::process::exit(1);
            }
            return;
        }
        Some("setup") => std::process::exit(weights::cli(&args[1..])),
        _ => {}
    }
    serve_stdio(dispatch).await;
}
