//! JSON-RPC 2.0 over NDJSON (MCP's stdio framing): one JSON object per line.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub jsonrpc: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub jsonrpc: String,
    pub id: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

impl Request {
    pub fn new(id: Option<Value>, method: &str, params: Value) -> Self {
        Self { jsonrpc: "2.0".into(), id, method: method.into(), params }
    }
}

impl Response {
    pub fn ok(id: Value, result: Value) -> Self {
        Self { jsonrpc: "2.0".into(), id, result: Some(result), error: None }
    }
    pub fn err(id: Value, code: i64, message: impl Into<String>) -> Self {
        Self { jsonrpc: "2.0".into(), id, result: None, error: Some(RpcError { code, message: message.into() }) }
    }
}

/// A line is a request/notification (has `method`) or a response (has `result`/`error`).
pub enum Frame {
    Request(Request),
    Response(Response),
}

pub fn parse_frame(line: &str) -> Option<Frame> {
    let v: Value = serde_json::from_str(line).ok()?;
    if v.get("method").is_some() {
        serde_json::from_value(v).ok().map(Frame::Request)
    } else if v.get("id").is_some() {
        serde_json::from_value(v).ok().map(Frame::Response)
    } else {
        None
    }
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, RpcError>>>>>;

/// Handle for a plugin to call back into the core.
#[derive(Clone)]
pub struct Client {
    tx: mpsc::UnboundedSender<String>,
    pending: Pending,
    next: Arc<Mutex<u64>>,
}

impl Client {
    pub async fn call(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        let id = {
            let mut n = self.next.lock().unwrap();
            *n += 1;
            *n
        };
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let line = serde_json::to_string(&Request::new(Some(json!(id)), method, params)).unwrap();
        if self.tx.send(line).is_err() {
            self.pending.lock().unwrap().remove(&id);
            return Err(RpcError { code: -32000, message: "core connection closed".into() });
        }
        rx.await.unwrap_or_else(|_| Err(RpcError { code: -32000, message: "core connection closed".into() }))
    }

    pub fn notify(&self, method: &str, params: Value) {
        let line = serde_json::to_string(&Request::new(None, method, params)).unwrap();
        let _ = self.tx.send(line);
    }
}

/// Serves requests on stdin/stdout until stdin closes. Requests run concurrently;
/// `shutdown` is answered and then ends the loop. Notifications get no response.
pub async fn serve_stdio<F, Fut>(handler: F)
where
    F: Fn(Request, Client) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Value, RpcError>> + Send + 'static,
{
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    let writer = tokio::spawn(async move {
        let mut out = tokio::io::stdout();
        while let Some(line) = rx.recv().await {
            if out.write_all(line.as_bytes()).await.is_err() || out.write_all(b"\n").await.is_err() {
                break;
            }
            let _ = out.flush().await;
        }
    });
    let client = Client { tx: tx.clone(), pending: Default::default(), next: Default::default() };
    let handler = Arc::new(handler);
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        match parse_frame(&line) {
            Some(Frame::Response(r)) => {
                let Some(id) = r.id.as_u64() else { continue };
                if let Some(w) = client.pending.lock().unwrap().remove(&id) {
                    let _ = w.send(match r.error {
                        Some(e) => Err(e),
                        None => Ok(r.result.unwrap_or(Value::Null)),
                    });
                }
            }
            Some(Frame::Request(req)) => {
                let stop = req.method == "shutdown";
                let (h, c, tx) = (handler.clone(), client.clone(), tx.clone());
                let task = tokio::spawn(async move {
                    let id = req.id.clone();
                    let res = h(req, c).await;
                    if let Some(id) = id {
                        let resp = match res {
                            Ok(v) => Response::ok(id, v),
                            Err(e) => Response::err(id, e.code, e.message),
                        };
                        let _ = tx.send(serde_json::to_string(&resp).unwrap());
                    }
                });
                if stop {
                    let _ = task.await;
                    break;
                }
            }
            None => {}
        }
    }
    // dropping every sender lets the writer drain and finish
    drop(client);
    drop(tx);
    let _ = writer.await;
}
