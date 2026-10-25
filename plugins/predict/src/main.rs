//! Predict plugin: ghost-text completion for the composer, plus `mu-predict bench`.

mod bench;
mod corpus;
mod engine;

use engine::State;
use mu_common::rpc::{Request, RpcError, serve_stdio};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

static STATE: OnceLock<Mutex<State>> = OnceLock::new();

pub fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
}

fn rpc_err(code: i64, message: impl Into<String>) -> RpcError {
    RpcError { code, message: message.into() }
}

fn load_state(agent_dir: &std::path::Path, mu_home: &std::path::Path) -> Result<State, String> {
    let mut st = State::open(&mu_home.join("predict"))?;
    for r in corpus::load_corpus(agent_dir, mu_home, &home()) {
        st.observe_n(&r.text, &r.cwd, r.count);
    }
    Ok(st)
}

fn state() -> Result<std::sync::MutexGuard<'static, State>, RpcError> {
    STATE.get().ok_or_else(|| rpc_err(-32002, "not initialized"))?.lock().map_err(|_| rpc_err(-32000, "state poisoned"))
}

fn dispatch(req: Request) -> Result<Value, RpcError> {
    let p = &req.params;
    let s = |k: &str| p.get(k).and_then(Value::as_str).unwrap_or_default().to_owned();
    match req.method.as_str() {
        "initialize" => {
            let agent_dir = PathBuf::from(s("agentDir"));
            let mu_home = PathBuf::from(s("muHome"));
            let st = load_state(&agent_dir, &mu_home).map_err(|e| rpc_err(-32000, e))?;
            let _ = STATE.set(Mutex::new(st));
            Ok(json!({}))
        }
        "shutdown" => Ok(json!({})),
        "predict/complete" => {
            let threshold = p.get("threshold").and_then(Value::as_f64).unwrap_or(0.3) as f32;
            let ghost = state()?.complete(&s("text"), &s("cwd"), &s("assistant"), threshold);
            Ok(json!({ "ghost": ghost }))
        }
        "predict/observe" => {
            state()?.observe(&s("text"), &s("cwd"));
            Ok(json!({}))
        }
        other => Err(rpc_err(-32601, format!("unknown method {other}"))),
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("bench") {
        if let Err(e) = bench::run(&args[1..]) {
            eprintln!("mu-predict bench: {e}");
            std::process::exit(1);
        }
        return;
    }
    serve_stdio(|req: Request, _client| async move { dispatch(req) }).await;
}
