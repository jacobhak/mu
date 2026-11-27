//! Predict plugin: ghost-text completion for the composer, `mu-predict setup smollm`, and `mu-predict bench`.
//!
//! Sessions share one engine per profile: the plugin forwards to a `mu-predict daemon` it finds or starts, and runs
//! the engine itself only when it cannot (see `daemon.rs`).

mod bench;
mod client;
mod corpus;
mod daemon;
mod engine;
mod local;
mod notice;
mod settings;
mod smollm;
mod weights;
mod worker;

use client::CallError;
use local::{Submitted, ghost_reply, rpc_err};
use mu_common::rpc::{Request, RpcError, serve_stdio};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Mutex;

/// The directories `initialize` named, kept in case the plugin has to start its own engine later.
static PATHS: Mutex<Option<(PathBuf, PathBuf)>> = Mutex::new(None);
/// One recovery at a time: reconnecting to the daemon, or giving up on it.
static RECOVER: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn dispatch(req: Request) -> Result<Value, RpcError> {
    let p = &req.params;
    let s = |k: &str| p.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
    match req.method.as_str() {
        "initialize" => {
            let (agent_dir, mu_home) = (PathBuf::from(s("agentDir")), PathBuf::from(s("muHome")));
            *PATHS.lock().unwrap_or_else(|e| e.into_inner()) = Some((agent_dir.clone(), mu_home.clone()));
            if client::enabled() {
                let (a, m) = (agent_dir.clone(), mu_home.clone());
                match tokio::task::spawn_blocking(move || client::connect(&a, &m)).await {
                    Ok(Ok(())) => return Ok(json!({})),
                    Ok(Err(why)) => eprintln!("predict: not sharing an engine ({why}); running one here"),
                    Err(_) => {}
                }
            }
            local::initialize(&agent_dir, &mu_home).map_err(|e| rpc_err(-32000, e))?;
            Ok(json!({}))
        }
        "shutdown" => {
            local::shutdown().await;
            Ok(json!({}))
        }
        method @ ("predict/complete" | "predict/observe") => {
            if client::active()
                && let Some(done) = shared(method, p).await
            {
                return done;
            }
            match local::submit(method, p, "")? {
                Submitted::Done(v) => Ok(v),
                Submitted::Ghost(rx) => Ok(ghost_reply(rx.await.unwrap_or_default())),
            }
        }
        other => Err(rpc_err(-32601, format!("unknown method {other}"))),
    }
}

/// The daemon's answer, or `None` when it is gone for good and this process now runs its own engine.
async fn shared(method: &str, p: &Value) -> Option<Result<Value, RpcError>> {
    let first = call(method, p).await;
    if !matches!(first, Err(CallError::Down(_))) {
        return Some(answer(method, first));
    }
    let _one = RECOVER.lock().await;
    let paths = PATHS.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if client::active() {
        // The daemon may have idled out, been replaced or crashed: find or start one, once.
        if let Some((a, m)) = paths.clone()
            && tokio::task::spawn_blocking(move || client::connect(&a, &m)).await.is_ok_and(|r| r.is_ok())
        {
            let again = call(method, p).await;
            if !matches!(again, Err(CallError::Down(_))) {
                return Some(answer(method, again));
            }
        }
        eprintln!("predict: the shared engine is gone; running one here");
        client::deactivate();
    }
    if let Some((a, m)) = paths
        && let Err(e) = local::initialize(&a, &m)
    {
        return Some(Err(rpc_err(-32000, e)));
    }
    None
}

async fn call(method: &str, p: &Value) -> Result<Value, CallError> {
    let (method, p) = (method.to_owned(), p.clone());
    tokio::task::spawn_blocking(move || client::call(&method, p))
        .await
        .unwrap_or_else(|e| Err(CallError::Down(e.to_string())))
}

/// A daemon too slow to answer yields no ghost, and a lost observe; neither is worth an error.
fn answer(method: &str, r: Result<Value, CallError>) -> Result<Value, RpcError> {
    match r {
        Ok(v) => Ok(v),
        Err(CallError::Timeout) if method == "predict/complete" => Ok(ghost_reply(String::new())),
        Err(CallError::Timeout) => Ok(json!({})),
        Err(CallError::Remote(m) | CallError::Down(m)) => Err(rpc_err(-32000, m)),
    }
}

fn main() {
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
        Some("daemon") => std::process::exit(daemon::run(&args[1..])),
        _ => {}
    }
    match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(rt) => rt.block_on(serve_stdio(dispatch)),
        Err(e) => {
            eprintln!("mu-predict: cannot start the runtime: {e}");
            std::process::exit(1);
        }
    }
}
