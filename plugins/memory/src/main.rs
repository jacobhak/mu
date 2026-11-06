//! Memory plugin: curated markdown memory per repository plus a global user file, versioned in git, with
//! a background review of sessions and keyword search over past sessions. See docs/memory.md.

mod admin;
mod ctx;
mod gitx;
mod mem;
mod review;
mod search;
mod snapshot;
mod store;
mod tool;
mod transcript;

use ctx::Ctx;
use mem::{Config, Mem};
use mu_common::rpc::{Request, RpcError, serve_stdio};
use serde_json::{Value, json};
use std::path::PathBuf;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    serve_stdio(|req: Request| async move { dispatch(req).await }).await;
}

fn rpc_err(code: i64, message: impl Into<String>) -> RpcError {
    RpcError { code, message: message.into() }
}

async fn dispatch(req: Request) -> Result<Value, RpcError> {
    let p = &req.params;
    match req.method.as_str() {
        "initialize" => {
            ctx::set(Ctx {
                cwd: p["cwd"]
                    .as_str()
                    .map(PathBuf::from)
                    .unwrap_or_else(|| std::env::current_dir().unwrap_or_default()),
                session_id: p["sessionId"].as_str().unwrap_or_default().into(),
                session_file: p["sessionFile"].as_str().map(PathBuf::from),
                subagent: p["subagent"].as_bool().unwrap_or(false),
            });
            Ok(json!({}))
        }
        "shutdown" => Ok(json!({})),
        "tool/call" => {
            let name = p["name"].as_str().unwrap_or_default();
            let args = p.get("arguments").cloned().unwrap_or(Value::Null);
            let c = ctx::get();
            let (content, is_error) = match name {
                "memory" => tool::call(&args, &c).await,
                "session_search" => search::call(&args, &c).await,
                other => return Err(rpc_err(-32601, format!("unknown tool {other}"))),
            };
            Ok(json!({ "content": content, "isError": is_error }))
        }
        "hook/prompt_submit" => Ok(prompt_submit(p).await),
        "hook/pre_compaction" => {
            let c = ctx::get();
            let cfg = Config::load();
            if let (true, false, Some(file)) = (cfg.review, c.subagent, c.session_file.clone()) {
                review::spawn_review(review::Job { session_file: file, session_id: c.session_id, cwd: c.cwd }, cfg);
            }
            Ok(json!({}))
        }
        "hook/session_end" => {
            let c = ctx::get();
            let cfg = Config::load();
            if let (true, false, Some(file)) = (cfg.review, c.subagent, c.session_file.clone()) {
                let job = review::Job { session_file: file, session_id: c.session_id, cwd: c.cwd };
                if let Err(e) = review::enqueue(&job, &cfg) {
                    mu_common::rpc::notify(
                        "ui/notify",
                        json!({"level": "warning", "message": format!("memory: cannot queue a review: {e}")}),
                    );
                }
            }
            Ok(json!({}))
        }
        m if m.starts_with("memory/") => admin::call(m, p).await.map_err(|e| rpc_err(-32000, e)),
        other => Err(rpc_err(-32601, format!("unknown method {other}"))),
    }
}

/// The first prompt of a fresh session gets the snapshot; later prompts and resumed sessions keep what the
/// host already holds. The first hook of a process also starts draining queued reviews.
async fn prompt_submit(p: &Value) -> Value {
    let c = ctx::get();
    let cfg = Config::load();
    if cfg.review && !c.subagent {
        review::spawn_drain_once(cfg.clone());
    }
    if p["userTurn"].as_u64() != Some(1) || p["resumed"].as_bool().unwrap_or(false) {
        return json!({});
    }
    let mem = Mem::open(&c.cwd, cfg).await;
    json!({ "context": snapshot::render(&mem.load(), &mem.checkout) })
}
