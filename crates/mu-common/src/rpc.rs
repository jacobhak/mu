//! JSON-RPC 2.0 over NDJSON (MCP's stdio framing): one JSON object per line.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

/// Writer of the `serve_stdio` loop that is running, for `notify`.
static OUTBOX: Mutex<Option<mpsc::UnboundedSender<String>>> = Mutex::new(None);

/// Sends a notification (a request without id) to the host from any thread; a no-op outside `serve_stdio`.
pub fn notify(method: &str, params: Value) {
    let g = OUTBOX.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(tx) = g.as_ref() {
        let _ = tx.send(serde_json::to_string(&Request::new(None, method, params)).unwrap());
    }
}

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

/// Reads newline-delimited JSON-RPC from any async reader. Lines are read as bytes, so one invalid
/// UTF-8 line is reported and skipped instead of ending the stream.
pub struct Framed<R> {
    reader: R,
    buf: Vec<u8>,
}

/// One line of input.
pub enum Line {
    Value(Value),
    /// A line that is not valid UTF-8 or not valid JSON; the stream goes on.
    Bad(String),
}

impl<R: tokio::io::AsyncBufRead + Unpin> Framed<R> {
    pub fn new(reader: R) -> Self {
        Self { reader, buf: Vec::new() }
    }

    /// The next non-blank line, or `None` at end of input or on a read error.
    pub async fn next_line(&mut self) -> Option<Line> {
        loop {
            self.buf.clear();
            match self.reader.read_until(b'\n', &mut self.buf).await {
                Ok(0) | Err(_) => return None,
                Ok(_) => {}
            }
            if self.buf.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            return Some(match serde_json::from_slice::<Value>(&self.buf) {
                Ok(v) => Line::Value(v),
                Err(e) => Line::Bad(e.to_string()),
            });
        }
    }

    /// The next request or response, skipping lines that are bad or neither.
    pub async fn next_frame(&mut self) -> Option<Frame> {
        loop {
            if let Line::Value(v) = self.next_line().await?
                && let Some(f) = frame_of(v)
            {
                return Some(f);
            }
        }
    }
}

pub fn frame_of(v: Value) -> Option<Frame> {
    if v.get("method").is_some() {
        serde_json::from_value(v).ok().map(Frame::Request)
    } else if v.get("id").is_some() {
        serde_json::from_value(v).ok().map(Frame::Response)
    } else {
        None
    }
}

/// Requests sent and not yet answered: hands out ids and routes each reply to its waiter.
#[derive(Default)]
pub struct Pending {
    inner: Mutex<PendingInner>,
}

#[derive(Default)]
struct PendingInner {
    next: u64,
    waiting: HashMap<u64, oneshot::Sender<Result<Value, String>>>,
}

impl Pending {
    /// A new id and the receiver its reply arrives on.
    pub fn register(&self) -> (u64, oneshot::Receiver<Result<Value, String>>) {
        let mut g = self.inner.lock().unwrap();
        g.next += 1;
        let id = g.next;
        let (tx, rx) = oneshot::channel();
        g.waiting.insert(id, tx);
        (id, rx)
    }

    /// Delivers `result` to the waiter for `id`, if it is still waiting.
    pub fn resolve(&self, id: u64, result: Result<Value, String>) {
        if let Some(tx) = self.inner.lock().unwrap().waiting.remove(&id) {
            let _ = tx.send(result);
        }
    }

    /// Forgets `id` (its caller gave up).
    pub fn cancel(&self, id: u64) {
        self.inner.lock().unwrap().waiting.remove(&id);
    }

    /// Fails every waiter, e.g. when the connection ended.
    pub fn fail_all(&self, message: &str) {
        for (_, tx) in self.inner.lock().unwrap().waiting.drain() {
            let _ = tx.send(Err(message.to_string()));
        }
    }

    pub fn is_empty(&self) -> bool {
        self.inner.lock().unwrap().waiting.is_empty()
    }
}

/// Serves requests on stdin/stdout until stdin closes. Requests run concurrently;
/// `shutdown` is answered and then ends the loop. Notifications get no response.
pub async fn serve_stdio<F, Fut>(handler: F)
where
    F: Fn(Request) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Value, RpcError>> + Send + 'static,
{
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    OUTBOX.lock().unwrap_or_else(|e| e.into_inner()).replace(tx.clone());
    let writer = tokio::spawn(async move {
        let mut out = tokio::io::stdout();
        while let Some(line) = rx.recv().await {
            if out.write_all(line.as_bytes()).await.is_err() || out.write_all(b"\n").await.is_err() {
                break;
            }
            let _ = out.flush().await;
        }
    });
    let handler = Arc::new(handler);
    let mut input = Framed::new(BufReader::new(tokio::io::stdin()));
    while let Some(frame) = input.next_frame().await {
        // Plugins served here only answer requests; replies to calls they never make are ignored.
        let Frame::Request(req) = frame else { continue };
        let stop = req.method == "shutdown";
        let (h, tx) = (handler.clone(), tx.clone());
        let task = tokio::spawn(async move {
            let id = req.id.clone();
            let res = h(req).await;
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
    OUTBOX.lock().unwrap_or_else(|e| e.into_inner()).take();
    // dropping every sender lets the writer drain and finish
    drop(tx);
    let _ = writer.await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn framed_skips_bad_lines_and_pending_routes_replies() {
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        rt.block_on(async {
            let data: &[u8] = b"\n{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"a\"}\n\xff\xfe not json\n{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":7}\n{\"x\":1}\n";
            let mut f = Framed::new(BufReader::new(data));
            assert!(matches!(f.next_frame().await, Some(Frame::Request(r)) if r.method == "a"));
            assert!(matches!(f.next_frame().await, Some(Frame::Response(r)) if r.id == 2));
            assert!(f.next_frame().await.is_none());
        });
        let p = Pending::default();
        let (id, rx) = p.register();
        let (id2, rx2) = p.register();
        assert_ne!(id, id2);
        p.resolve(id, Ok(json!(5)));
        p.fail_all("gone");
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        assert_eq!(rt.block_on(rx).unwrap().unwrap(), json!(5));
        assert_eq!(rt.block_on(rx2).unwrap().unwrap_err(), "gone");
        assert!(p.is_empty());
    }
}
