//! `mu-mcp-http`: a raw relay between mu's NDJSON JSON-RPC on stdio and one MCP server over
//! Streamable HTTP. It uses rmcp's transport but not rmcp's service loop, so every message passes
//! through unchanged; the bridge only handles auth and session recovery.
//!
//! Configuration comes from the environment only:
//! - `MU_MCP_URL` (required): the server endpoint.
//! - `MU_MCP_HEADERS` (optional): a JSON object of header name to string value, sent on every request.
//! - `MU_MCP_AUTH` (optional): a JSON object such as `{"type":"oauth"}`. When set, a token is fetched
//!   before the first request instead of after the first 401.
//!
//! Bridge to core: on a 401 the bridge writes `{"method":"core/mcpToken","params":{"force":bool}}` on
//! stdout and expects `{"result":{"token":"..."}}` or an error on stdin. The token lives in memory only.
//! Headers and tokens are never printed.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use futures_util::stream::{FuturesUnordered, StreamExt};
use reqwest::header::{HeaderName, HeaderValue};
use rmcp::model::{ClientJsonRpcMessage, ServerJsonRpcMessage};
use rmcp::transport::streamable_http_client::{StreamableHttpClientTransportConfig, StreamableHttpError};
use rmcp::transport::{StreamableHttpClientTransport, Transport};
use serde_json::{Map, Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// The bridge's own request ids start here, so they do not collide with the core's small ids.
const TOKEN_ID_BASE: u64 = 1 << 40;
/// A message may be sent again once after a session re-initialise; a second expiry is fatal.
const MAX_SESSION_ATTEMPTS: u8 = 1;
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

type Http = StreamableHttpClientTransport<reqwest::Client>;
type SendErr = StreamableHttpError<reqwest::Error>;
type Sends = FuturesUnordered<Pin<Box<dyn Future<Output = Sent> + Send>>>;

struct Config {
    url: String,
    headers: HashMap<HeaderName, HeaderValue>,
    auth_up_front: bool,
}

impl Config {
    fn from_env() -> Result<Self, String> {
        let url = std::env::var("MU_MCP_URL").ok().filter(|u| !u.is_empty()).ok_or("MU_MCP_URL is not set")?;
        let headers = match std::env::var("MU_MCP_HEADERS") {
            Ok(s) if !s.trim().is_empty() => parse_headers(&s)?,
            _ => HashMap::new(),
        };
        let auth_up_front = match std::env::var("MU_MCP_AUTH") {
            Ok(s) if !s.trim().is_empty() => {
                let v: Value = serde_json::from_str(&s).map_err(|_| "MU_MCP_AUTH is not valid JSON")?;
                if !v.is_object() {
                    return Err("MU_MCP_AUTH must be a JSON object".into());
                }
                true
            }
            _ => false,
        };
        Ok(Config { url, headers, auth_up_front })
    }
}

/// Parses `MU_MCP_HEADERS`. Error messages name the header but never print its value.
fn parse_headers(s: &str) -> Result<HashMap<HeaderName, HeaderValue>, String> {
    let obj: Map<String, Value> = serde_json::from_str(s).map_err(|_| "MU_MCP_HEADERS must be a JSON object")?;
    let mut out = HashMap::new();
    for (name, value) in obj {
        let value = value.as_str().ok_or_else(|| format!("header {name} needs a string value"))?;
        let n = HeaderName::from_bytes(name.as_bytes()).map_err(|_| format!("invalid header name {name}"))?;
        let v = HeaderValue::from_str(value).map_err(|_| format!("invalid value for header {name}"))?;
        out.insert(n, v);
    }
    Ok(out)
}

/// A message on its way to the server. `replay` marks a copy of the initialize handshake that the
/// core does not await, so its response is dropped.
struct Retry {
    msg: Value,
    attempt: u8,
    replay: bool,
}

/// The outcome of one `transport.send`, tagged with the transport generation it used.
struct Sent {
    generation: u64,
    r: Retry,
    result: Result<(), SendErr>,
}

struct Bridge {
    cfg: Config,
    client: reqwest::Client,
    token: Option<String>,
    transport: Option<Http>,
    /// The transport's worker has ended. rmcp ends it when an initialize POST fails, and the
    /// failure reaches `sends` as an error, so recovery can still replace the transport.
    closed: bool,
    /// Bumped on every new transport, so stale failures can be told apart from current ones.
    generation: u64,
    sends: Sends,
    stored_init: Option<Value>,
    stored_initialized: Option<Value>,
    /// Ids of replayed initialize requests. Their responses are dropped: the core has its own.
    swallow: Vec<Value>,
    /// Messages waiting for a fresh token.
    waiting: Vec<Retry>,
    /// The outstanding core/mcpToken request: (id, forced).
    token_wait: Option<(u64, bool)>,
    next_id: u64,
    /// 401s from the current transport since the server last answered. 1 asks for a token, 2 forces
    /// one, and 3 gives up.
    episode_401s: u32,
    stdout: tokio::io::Stdout,
}

fn method_of(v: &Value) -> Option<&str> {
    v.get("method").and_then(Value::as_str)
}

impl Bridge {
    fn new(cfg: Config) -> Self {
        Bridge {
            cfg,
            client: reqwest::Client::new(),
            token: None,
            transport: None,
            closed: false,
            generation: 0,
            sends: FuturesUnordered::new(),
            stored_init: None,
            stored_initialized: None,
            swallow: Vec::new(),
            waiting: Vec::new(),
            token_wait: None,
            next_id: 0,
            episode_401s: 0,
            stdout: tokio::io::stdout(),
        }
    }

    /// True when the worker is gone and nothing is left that could replace it.
    fn dead(&self) -> bool {
        self.closed && self.sends.is_empty() && self.waiting.is_empty() && self.token_wait.is_none()
    }

    fn build(&self) -> Http {
        let mut c = StreamableHttpClientTransportConfig::with_uri(self.cfg.url.as_str())
            .custom_headers(self.cfg.headers.clone())
            // The bridge re-initialises itself, so the core sees the replay and no hidden handshake.
            .reinit_on_expired_session(false);
        if let Some(t) = &self.token {
            c = c.auth_header(t.clone());
        }
        StreamableHttpClientTransport::with_client(self.client.clone(), c)
    }

    /// Starts a new transport with the current token. Callers replay the session as needed.
    fn connect(&mut self) {
        self.generation += 1;
        self.closed = false;
        self.transport = Some(self.build());
    }

    /// Hands `r` to the transport. Send futures are pushed and polled in order, so rmcp's ordering
    /// barrier for initialize and initialized keeps the handshake in order.
    async fn dispatch(&mut self, r: Retry) -> Result<(), String> {
        if r.replay
            && let Some(id) = r.msg.get("id")
            && !self.swallow.contains(id)
        {
            self.swallow.push(id.clone());
        }
        let typed: ClientJsonRpcMessage = match serde_json::from_value(r.msg.clone()) {
            Ok(t) => t,
            Err(e) => {
                let reason = format!("dropped a message the MCP schema rejects: {e}");
                eprintln!("mu-mcp-http: {reason}");
                // A rejected core request must still get its answer, or the core waits for it.
                if let Some(id) = r.msg.get("id")
                    && method_of(&r.msg).is_some()
                {
                    let reply = json!({"jsonrpc":"2.0","id":id,"error":{"code":-32600,"message":reason}});
                    self.emit(&reply).await?;
                }
                return Ok(());
            }
        };
        let Some(t) = self.transport.as_mut() else {
            return Err("internal: no transport".into());
        };
        let fut = t.send(typed);
        let generation = self.generation;
        self.sends.push(Box::pin(async move { Sent { generation, r, result: fut.await } }));
        Ok(())
    }

    /// Sends a message from the core, connecting first, or queues it while a token is missing.
    async fn submit(&mut self, r: Retry) -> Result<(), String> {
        if self.transport.is_none() {
            if self.cfg.auth_up_front && self.token.is_none() {
                self.waiting.push(r);
                return self.want_token(false).await;
            }
            self.connect();
        }
        self.dispatch(r).await
    }

    /// Asks the core for a token, unless a request is already outstanding.
    async fn want_token(&mut self, force: bool) -> Result<(), String> {
        if self.token_wait.is_some() {
            return Ok(());
        }
        let id = TOKEN_ID_BASE + self.next_id;
        self.next_id += 1;
        self.token_wait = Some((id, force));
        self.emit(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "core/mcpToken",
            "params": { "force": force },
        }))
        .await
    }

    /// Handles the core's answer to `core/mcpToken`.
    async fn on_token_reply(&mut self, v: &Value, forced: bool) -> Result<(), String> {
        if let Some(token) = v.pointer("/result/token").and_then(Value::as_str) {
            self.token = Some(token.to_owned());
            let waiting = std::mem::take(&mut self.waiting);
            return self.rebuild(waiting).await;
        }
        if !forced {
            return self.want_token(true).await;
        }
        let why = v.pointer("/error/message").and_then(Value::as_str).unwrap_or("no token");
        Err(format!("the core gave no token for this server: {why}"))
    }

    /// A send was refused with 401. Ask for a token (the second time forced) and retry, or give up.
    async fn on_auth(&mut self, generation: u64, r: Retry) -> Result<(), String> {
        if self.token_wait.is_some() {
            self.waiting.push(r);
            return Ok(());
        }
        if generation < self.generation {
            // Sent on a transport that was replaced since; the current one carries the latest token.
            return self.dispatch(r).await;
        }
        self.episode_401s += 1;
        if self.episode_401s > 2 {
            return Err("the server still answers 401 after a token refresh".into());
        }
        let force = self.episode_401s == 2;
        self.waiting.push(r);
        self.want_token(force).await
    }

    /// Starts a new transport and sends `msgs` on it. The stored initialize handshake is replayed
    /// first unless one of `msgs` already is the handshake.
    async fn rebuild(&mut self, msgs: Vec<Retry>) -> Result<(), String> {
        self.connect();
        let core_init = msgs.iter().any(|r| method_of(&r.msg) == Some("initialize"));
        let core_initialized = msgs.iter().any(|r| method_of(&r.msg) == Some("notifications/initialized"));
        let mut queue = Vec::new();
        if !core_init && let Some(init) = self.stored_init.clone() {
            queue.push(Retry { msg: init, attempt: 1, replay: true });
            if !core_initialized && let Some(n) = self.stored_initialized.clone() {
                queue.push(Retry { msg: n, attempt: 0, replay: true });
            }
        }
        queue.extend(msgs);
        for r in queue {
            self.dispatch(r).await?;
        }
        Ok(())
    }

    async fn on_sent(&mut self, s: Sent) -> Result<(), String> {
        let Sent { generation, r, result } = s;
        let Err(err) = result else { return Ok(()) };
        let is_auth = match &err {
            StreamableHttpError::AuthRequired(_) => true,
            StreamableHttpError::UnexpectedServerResponse(m) => m.starts_with("HTTP 401"),
            _ => false,
        };
        if is_auth {
            return self.on_auth(generation, r).await;
        }
        if matches!(err, StreamableHttpError::SessionExpired) {
            if r.attempt >= MAX_SESSION_ATTEMPTS {
                return Err("the server dropped the session again right after re-initializing".into());
            }
            let retry = Retry { attempt: r.attempt + 1, ..r };
            return if generation < self.generation {
                self.dispatch(retry).await
            } else {
                self.rebuild(vec![retry]).await
            };
        }
        Err(format!("transport error: {err}"))
    }

    /// A line from the core on stdin.
    async fn on_stdin_line(&mut self, line: &str) -> Result<(), String> {
        let line = line.trim();
        if line.is_empty() {
            return Ok(());
        }
        let v: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => {
                eprintln!("mu-mcp-http: dropped a stdin line that is not JSON");
                return Ok(());
            }
        };
        if let Some((id, forced)) = self.token_wait
            && method_of(&v).is_none()
            && v.get("id") == Some(&json!(id))
        {
            self.token_wait = None;
            return self.on_token_reply(&v, forced).await;
        }
        match method_of(&v) {
            Some("initialize") => self.stored_init = Some(v.clone()),
            Some("notifications/initialized") => self.stored_initialized = Some(v.clone()),
            _ => {}
        }
        self.submit(Retry { msg: v, attempt: 0, replay: false }).await
    }

    /// A message from the server, written to stdout unless it answers a replayed handshake.
    async fn on_server(&mut self, msg: ServerJsonRpcMessage) -> Result<(), String> {
        self.episode_401s = 0;
        let v = serde_json::to_value(&msg).map_err(|e| format!("cannot encode a server message: {e}"))?;
        // `core/*` is the private channel between this bridge and the core. A server must never reach
        // it (it could make the core fetch tokens and hand them back): answer "method not found".
        if let Some(m) = method_of(&v)
            && m.starts_with("core/")
        {
            if let Some(id) = v.get("id") {
                let reply = json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"method not found"}});
                return self.submit(Retry { msg: reply, attempt: 0, replay: false }).await;
            }
            return Ok(());
        }
        if method_of(&v).is_none()
            && let Some(id) = v.get("id")
            && let Some(pos) = self.swallow.iter().position(|s| s == id)
        {
            self.swallow.swap_remove(pos);
            return Ok(());
        }
        self.emit(&v).await
    }

    async fn emit(&mut self, v: &Value) -> Result<(), String> {
        let mut line = serde_json::to_vec(v).map_err(|e| e.to_string())?;
        line.push(b'\n');
        self.stdout.write_all(&line).await.map_err(|e| format!("stdout: {e}"))?;
        self.stdout.flush().await.map_err(|e| format!("stdout: {e}"))
    }
}

/// Receives the next server message. With no transport yet it waits forever, so the select loop
/// only reacts to stdin.
async fn recv(t: &mut Option<Http>) -> Option<ServerJsonRpcMessage> {
    match t {
        Some(t) => t.receive().await,
        None => std::future::pending().await,
    }
}

async fn run(cfg: Config) -> Result<(), String> {
    let mut b = Bridge::new(cfg);
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    loop {
        tokio::select! {
            line = lines.next_line() => {
                let line: Option<String> = line.map_err(|e| format!("stdin: {e}"))?;
                match line {
                    Some(line) => b.on_stdin_line(&line).await?,
                    None => break,
                }
            }
            Some(sent) = b.sends.next(), if !b.sends.is_empty() => b.on_sent(sent).await?,
            msg = recv(&mut b.transport), if !b.closed => match msg {
                Some(msg) => b.on_server(msg).await?,
                None => b.closed = true,
            },
        }
        if b.dead() {
            return Err("the MCP server closed the connection".into());
        }
    }
    // stdin closed: end the session politely (DELETE), within a short grace period.
    if let Some(mut t) = b.transport.take() {
        let _ = tokio::time::timeout(SHUTDOWN_GRACE, t.close()).await;
    }
    Ok(())
}

fn main() {
    let cfg = match Config::from_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("mu-mcp-http: {e}");
            std::process::exit(2);
        }
    };
    // Already-installed is fine: the provider is process-wide.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("tokio runtime");
    let result = rt.block_on(run(cfg));
    // A blocked stdin read would keep a normal runtime drop waiting, so do not wait for it.
    rt.shutdown_background();
    match result {
        Ok(()) => std::process::exit(0),
        Err(e) => {
            eprintln!("mu-mcp-http: {e}");
            std::process::exit(1);
        }
    }
}
