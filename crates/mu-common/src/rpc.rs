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
        let _ = tx.send(serde_json::to_string(&Request::new(None, method, params)).expect("request serializes"));
    }
}

/// Requests this plugin sent to the host (`request`), awaiting replies.
static PENDING: std::sync::OnceLock<Pending> = std::sync::OnceLock::new();

fn pending() -> &'static Pending {
    PENDING.get_or_init(Pending::default)
}

/// Calls a host method (such as `core/webSearch`) and waits for the reply. `Err` carries the
/// host's error message, `"not serving"` outside `serve_stdio`, or `"host closed"`.
pub async fn request(method: &str, params: Value) -> Result<Value, RpcError> {
    let tx =
        OUTBOX.lock().unwrap_or_else(|e| e.into_inner()).clone().ok_or_else(|| RpcError::new(-32000, "not serving"))?;
    let (id, rx) = pending().register();
    let line = serde_json::to_string(&Request::new(Some(Value::from(id)), method, params)).expect("request serializes");
    if tx.send(line).is_err() {
        pending().cancel(id);
        return Err(RpcError::new(-32000, "host closed"));
    }
    rx.await.unwrap_or_else(|_| Err(RpcError::new(-32000, "host closed")))
}

/// Hands a host reply to the `request` waiting for it.
fn route_response(p: &Pending, r: Response) {
    let Some(id) = r.id.as_u64() else { return };
    let result = match r.error {
        Some(e) => Err(e),
        None => Ok(r.result.unwrap_or(Value::Null)),
    };
    p.resolve(id, result);
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
    /// Machine-readable cause (see `kind`); `message` is for display only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}

/// Values of `RpcError::kind` that a caller branches on.
pub mod kind {
    /// `core/complete`: no model serves the requested role.
    pub const NO_MODEL: &str = "no_model";
}

impl RpcError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self { code, message: message.into(), kind: None }
    }

    pub fn with_kind(mut self, kind: Option<&str>) -> Self {
        self.kind = kind.map(str::to_string);
        self
    }
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
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
        Self { jsonrpc: "2.0".into(), id, result: None, error: Some(RpcError::new(code, message)) }
    }
    pub fn err_kind(id: Value, code: i64, message: impl Into<String>, kind: Option<&str>) -> Self {
        Self { jsonrpc: "2.0".into(), id, result: None, error: Some(RpcError::new(code, message).with_kind(kind)) }
    }
}

/// A line is a request/notification (has `method`) or a response (has `result`/`error`).
pub enum Frame {
    Request(Request),
    Response(Response),
}

/// Longest line kept in memory. A longer one is dropped and reported as `Line::Bad`.
pub const MAX_LINE: usize = 64 * 1024 * 1024;

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

    /// The next non-blank line, or `None` at end of input or on a read error. A line over `MAX_LINE`
    /// is consumed to its end and reported as `Line::Bad`, so the stream goes on.
    pub async fn next_line(&mut self) -> Option<Line> {
        loop {
            self.buf.clear();
            let mut dropped = false;
            let mut saw_bytes = false;
            loop {
                let chunk = self.reader.fill_buf().await.ok()?;
                if chunk.is_empty() {
                    break;
                }
                saw_bytes = true;
                let (end, used) = match chunk.iter().position(|b| *b == b'\n') {
                    Some(i) => (true, i + 1),
                    None => (false, chunk.len()),
                };
                if !dropped {
                    if self.buf.len() + used > MAX_LINE {
                        dropped = true;
                        self.buf = Vec::new();
                    } else {
                        self.buf.extend_from_slice(&chunk[..used]);
                    }
                }
                self.reader.consume(used);
                if end {
                    break;
                }
            }
            if !saw_bytes {
                return None;
            }
            if dropped {
                return Some(Line::Bad(format!("line longer than {} MiB dropped", MAX_LINE >> 20)));
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
    waiting: HashMap<u64, oneshot::Sender<Result<Value, RpcError>>>,
}

impl Pending {
    /// A new id and the receiver its reply arrives on.
    pub fn register(&self) -> (u64, oneshot::Receiver<Result<Value, RpcError>>) {
        let mut g = self.inner.lock().expect("pending lock poisoned");
        g.next += 1;
        let id = g.next;
        let (tx, rx) = oneshot::channel();
        g.waiting.insert(id, tx);
        (id, rx)
    }

    /// Delivers `result` to the waiter for `id`, if it is still waiting.
    pub fn resolve(&self, id: u64, result: Result<Value, RpcError>) {
        if let Some(tx) = self.inner.lock().expect("pending lock poisoned").waiting.remove(&id) {
            let _ = tx.send(result);
        }
    }

    /// Forgets `id` (its caller gave up).
    pub fn cancel(&self, id: u64) {
        self.inner.lock().expect("pending lock poisoned").waiting.remove(&id);
    }

    /// Fails every waiter, e.g. when the connection ended.
    pub fn fail_all(&self, message: &str) {
        for (_, tx) in self.inner.lock().expect("pending lock poisoned").waiting.drain() {
            let _ = tx.send(Err(RpcError::new(-32000, message)));
        }
    }

    pub fn is_empty(&self) -> bool {
        self.inner.lock().expect("pending lock poisoned").waiting.is_empty()
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
        let req = match frame {
            Frame::Request(req) => req,
            Frame::Response(r) => {
                route_response(pending(), r);
                continue;
            }
        };
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
                let _ = tx.send(serde_json::to_string(&resp).expect("response serializes"));
            }
        });
        if stop {
            let _ = task.await;
            break;
        }
    }
    OUTBOX.lock().unwrap_or_else(|e| e.into_inner()).take();
    pending().fail_all("host closed");
    // dropping every sender lets the writer drain and finish
    drop(tx);
    let _ = writer.await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_request_to_the_host_gets_its_reply_or_its_error() {
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        rt.block_on(async {
            assert_eq!(request("core/x", json!({})).await.unwrap_err().message, "not serving");
            let (tx, mut out) = mpsc::unbounded_channel::<String>();
            OUTBOX.lock().unwrap().replace(tx);
            let host = async {
                for reply in [
                    |id: Value| Response::ok(id, json!({"backend": "exa"})),
                    |id: Value| Response::err(id, -32603, "no key"),
                    |id: Value| Response::err_kind(id, -32603, "none", Some(kind::NO_MODEL)),
                ] {
                    let line = out.recv().await.unwrap();
                    let sent: Request = serde_json::from_str(&line).unwrap();
                    assert_eq!(sent.method, "core/webSearch");
                    route_response(pending(), reply(sent.id.unwrap()));
                }
            };
            let calls = async {
                let a = request("core/webSearch", json!({})).await;
                let b = request("core/webSearch", json!({})).await;
                let c = request("core/webSearch", json!({})).await;
                (a, b, c)
            };
            let ((a, b, c), ()) = tokio::join!(calls, host);
            assert_eq!(a.unwrap(), json!({"backend": "exa"}));
            let b = b.unwrap_err();
            assert_eq!((b.message.as_str(), b.kind), ("no key", None));
            assert_eq!(c.unwrap_err().kind.as_deref(), Some(kind::NO_MODEL));
            // A reply nobody waits for is dropped; closing the host fails the waiters.
            route_response(pending(), Response::ok(json!(999), Value::Null));
            OUTBOX.lock().unwrap().take();
            assert!(pending().is_empty());
        });
    }

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
    }

    #[test]
    fn an_overlong_line_is_dropped_and_the_stream_goes_on() {
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        rt.block_on(async {
            let mut data = vec![b'a'; MAX_LINE + 1024 * 1024];
            data.extend_from_slice(b"\n{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":1}\n");
            let mut f = Framed::new(BufReader::new(&data[..]));
            match f.next_line().await {
                Some(Line::Bad(why)) => assert_eq!(why, "line longer than 64 MiB dropped"),
                _ => panic!("expected the overlong line to be reported as bad"),
            }
            assert!(matches!(f.next_line().await, Some(Line::Value(_))));
            assert!(f.next_line().await.is_none());
        });
    }

    #[test]
    fn pending_routes_replies() {
        let p = Pending::default();
        let (id, rx) = p.register();
        let (id2, rx2) = p.register();
        assert_ne!(id, id2);
        p.resolve(id, Ok(json!(5)));
        p.fail_all("gone");
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        assert_eq!(rt.block_on(rx).unwrap().unwrap(), json!(5));
        assert_eq!(rt.block_on(rx2).unwrap().unwrap_err().message, "gone");
        assert!(p.is_empty());
    }
}
