//! Web plugin: `fetch` and `web_search`, both native.

mod fetch;
mod github;
mod html;
mod net;
mod search;

use mu_common::rpc::{Request, RpcError, serve_stdio};
use net::Guard;
use serde_json::{Value, json};

#[tokio::main(flavor = "current_thread")]
async fn main() {
    serve_stdio(|req: Request| async move { dispatch(req).await }).await;
}

async fn dispatch(req: Request) -> Result<Value, RpcError> {
    match req.method.as_str() {
        "initialize" => {
            if let Some(id) = req.params.get("sessionId").and_then(Value::as_str) {
                search::set_session(id);
            }
            Ok(json!({}))
        }
        "shutdown" => Ok(json!({})),
        "tool/call" => {
            let name = req.params.get("name").and_then(Value::as_str).unwrap_or_default();
            let args = req.params.get("arguments").cloned().unwrap_or(Value::Null);
            let (content, is_error) = match name {
                "web_search" => search::search(&args).await,
                "fetch" => fetch_tool(&args).await,
                other => return Err(RpcError::new(-32601, format!("unknown tool {other}"))),
            };
            Ok(json!({ "content": content, "isError": is_error }))
        }
        other => Err(RpcError::new(-32601, format!("unknown method {other}"))),
    }
}

async fn fetch_tool(args: &Value) -> (String, bool) {
    let Some(url) = str_arg(args, "url") else {
        return ("url is required".into(), true);
    };
    let raw = args.get("raw").and_then(Value::as_bool).unwrap_or(false);
    match fetch::fetch(&Guard::from_env(), url, raw).await {
        Ok(out) => (out, false),
        Err(e) => (e, true),
    }
}

pub(crate) fn str_arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str).filter(|s| !s.trim().is_empty())
}
