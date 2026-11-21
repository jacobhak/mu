//! `mu-mcp-http`: a raw relay between mu's NDJSON JSON-RPC on stdio and one MCP server over
//! Streamable HTTP. It uses rmcp's transport but not rmcp's service loop, so every message passes
//! through unchanged; the bridge only handles auth and session recovery.
//!
//! Configuration comes from the environment only:
//! - `MU_MCP_URL` (required): the server endpoint, which is also the key of its OAuth credential.
//! - `MU_MCP_HEADERS` (optional): a JSON object of header name to string value, sent on every request.
//! - `MU_MCP_OAUTH` (optional): a JSON object of OAuth settings (`clientId`, `clientSecret`, `scope`,
//!   `callbackPort`, `callbackPath`, `redirectUri`, `tokenUrl`). When set, the stored credential is
//!   looked up before the first request; after a 401 it is always asked for.
//!
//! The credential is kept by the core: the bridge sends `core/mcpSecret` (`get`, `put`, `delete`) on
//! stdout and reads the reply on stdin. Tokens live in memory only. Headers and tokens are never printed.
//!
//! Modes: no argument relays; `login` runs the sign-in (notifications `login/url` and `login/notice`,
//! `login/paste` in, `login/done` out); `token` prints one access token as `token/result`.

mod core_rpc;
mod oauth;

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use core_rpc::{CoreRpc, Pump};
use futures_util::stream::{FuturesUnordered, StreamExt};
use oauth::{Fail, OAuthCfg};
use reqwest::header::{HeaderName, HeaderValue};
use rmcp::model::{ClientJsonRpcMessage, ServerJsonRpcMessage};
use rmcp::transport::streamable_http_client::{StreamableHttpClientTransportConfig, StreamableHttpError};
use rmcp::transport::{StreamableHttpClientTransport, Transport};
use serde_json::{Map, Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// A message may be sent again once after a session re-initialise; a second expiry is fatal.
const MAX_SESSION_ATTEMPTS: u8 = 1;
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

type Http = StreamableHttpClientTransport<reqwest::Client>;
type SendErr = StreamableHttpError<reqwest::Error>;
type Sends = FuturesUnordered<Pin<Box<dyn Future<Output = Sent> + Send>>>;
type Tokens = FuturesUnordered<Pin<Box<dyn Future<Output = Result<Option<String>, Fail>>>>>;

struct Config {
    url: String,
    headers: HashMap<HeaderName, HeaderValue>,
    oauth: OAuthCfg,
}

fn oauth_from_env() -> Result<OAuthCfg, String> {
    let url = std::env::var("MU_MCP_URL").ok().filter(|u| !u.is_empty()).ok_or("MU_MCP_URL is not set")?;
    OAuthCfg::parse(url, std::env::var("MU_MCP_OAUTH").ok())
}

impl Config {
    fn from_env() -> Result<Self, String> {
        let oauth = oauth_from_env()?;
        let headers = match std::env::var("MU_MCP_HEADERS") {
            Ok(s) if !s.trim().is_empty() => parse_headers(&s)?,
            _ => HashMap::new(),
        };
        Ok(Config { url: oauth.url.clone(), headers, oauth })
    }
}

/// What a token fetch is for: the look before the first request, or a token after a 401.
#[derive(Clone, Copy)]
enum Want {
    UpFront,
    Token { force: bool },
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
    /// Token fetches under way; at most one.
    tokens: Tokens,
    rpc: CoreRpc,
    pump: Pump,
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
        let (rpc, pump) = core_rpc::channel();
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
            tokens: FuturesUnordered::new(),
            rpc,
            pump,
            episode_401s: 0,
            stdout: tokio::io::stdout(),
        }
    }

    /// True when the worker is gone and nothing is left that could replace it.
    fn dead(&self) -> bool {
        self.closed && self.sends.is_empty() && self.waiting.is_empty() && self.tokens.is_empty()
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
            if self.cfg.oauth.enabled {
                self.waiting.push(r);
                self.want_token(Want::UpFront);
                return Ok(());
            }
            self.connect();
        }
        self.dispatch(r).await
    }

    /// Starts fetching a token, unless a fetch is already under way.
    fn want_token(&mut self, want: Want) {
        if !self.tokens.is_empty() {
            return;
        }
        let (cfg, rpc) = (self.cfg.oauth.clone(), self.rpc.clone());
        self.tokens.push(Box::pin(async move {
            match want {
                Want::UpFront => oauth::up_front_token(&cfg, &rpc).await,
                Want::Token { force } => oauth::access_token(&cfg, &rpc, force).await.map(Some),
            }
        }));
    }

    /// A token fetch finished: connect with it, or fail the bridge.
    async fn on_token(&mut self, r: Result<Option<String>, Fail>) -> Result<(), String> {
        match r {
            Ok(token) => {
                if token.is_some() {
                    self.token = token;
                }
                let waiting = std::mem::take(&mut self.waiting);
                self.rebuild(waiting).await
            }
            Err(Fail::NeedsLogin) => Err("not logged in: run /login mcp:<name> in mu".into()),
            Err(Fail::Other(e)) => Err(e),
        }
    }

    /// A send was refused with 401. Ask for a token (the second time forced) and retry, or give up.
    async fn on_auth(&mut self, generation: u64, r: Retry) -> Result<(), String> {
        if !self.tokens.is_empty() {
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
        self.want_token(Want::Token { force });
        Ok(())
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
        if self.pump.on_reply(&v) {
            return Ok(());
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
        // it (it could make the core read the stored credential): answer "method not found".
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
            Some(call) = b.pump.next_line() => b.emit(&call).await?,
            Some(r) = b.tokens.next(), if !b.tokens.is_empty() => b.on_token(r).await?,
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

async fn write_json(v: &Value) -> Result<(), String> {
    let mut out = tokio::io::stdout();
    let mut line = serde_json::to_vec(v).map_err(|e| e.to_string())?;
    line.push(b'\n');
    out.write_all(&line).await.map_err(|e| format!("stdout: {e}"))?;
    out.flush().await.map_err(|e| format!("stdout: {e}"))
}

/// Runs `work` while serving the core's side of stdio: its calls go out, replies and `login/paste`
/// lines come in. Stdin closing ends `paste`, which `login` takes as a cancel.
async fn drive<T>(
    work: impl Future<Output = T>,
    mut pump: Pump,
    mut paste: Option<tokio::sync::mpsc::UnboundedSender<String>>,
) -> Result<T, String> {
    tokio::pin!(work);
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut open = true;
    let out = loop {
        tokio::select! {
            r = &mut work => break r,
            Some(call) = pump.next_line() => write_json(&call).await?,
            line = lines.next_line(), if open => match line {
                Ok(Some(l)) => {
                    let Ok(v) = serde_json::from_str::<Value>(l.trim()) else { continue };
                    if method_of(&v) == Some("login/paste") {
                        if let (Some(tx), Some(t)) = (&paste, v.pointer("/params/text").and_then(Value::as_str)) {
                            let _ = tx.send(t.to_string());
                        }
                    } else {
                        pump.on_reply(&v);
                    }
                }
                _ => {
                    open = false;
                    paste = None;
                }
            },
        }
    };
    for call in pump.drain() {
        write_json(&call).await?;
    }
    Ok(out)
}

async fn finish(method: &str, params: Value) -> Result<(), String> {
    write_json(&json!({"jsonrpc": "2.0", "method": method, "params": params})).await
}

/// `login`: the sign-in. Ends with `login/done`, `{}` or `{"error": …}`.
async fn run_login(cfg: OAuthCfg) -> Result<(), String> {
    let (rpc, pump) = core_rpc::channel();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let r = drive(oauth::login(&cfg, &rpc, &mut rx), pump, Some(tx)).await?;
    finish("login/done", r.as_ref().map_or_else(|e| json!({"error": e}), |_| json!({}))).await?;
    r
}

/// `token`: one access token as `token/result`: `{"token"}`, `{"needsLogin": true}` or `{"error"}`.
async fn run_token(cfg: OAuthCfg) -> Result<(), String> {
    let (rpc, pump) = core_rpc::channel();
    let r = drive(oauth::access_token(&cfg, &rpc, false), pump, None).await?;
    match r {
        Ok(token) => finish("token/result", json!({"token": token})).await,
        Err(Fail::NeedsLogin) => finish("token/result", json!({"needsLogin": true})).await,
        Err(Fail::Other(e)) => {
            finish("token/result", json!({"error": e})).await?;
            Err(e)
        }
    }
}

enum Mode {
    Relay(Config),
    Login(OAuthCfg),
    Token(OAuthCfg),
}

fn mode_from_args() -> Result<Mode, String> {
    match std::env::args().nth(1).as_deref() {
        None => Config::from_env().map(Mode::Relay),
        Some("login") => oauth_from_env().map(Mode::Login),
        Some("token") => oauth_from_env().map(Mode::Token),
        Some(other) => Err(format!("unknown mode `{other}` (expected `login` or `token`)")),
    }
}

fn main() {
    let mode = match mode_from_args() {
        Ok(m) => m,
        Err(e) => {
            eprintln!("mu-mcp-http: {e}");
            std::process::exit(2);
        }
    };
    // Already-installed is fine: the provider is process-wide.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("tokio runtime");
    let result = rt.block_on(async {
        match mode {
            Mode::Relay(cfg) => run(cfg).await,
            Mode::Login(cfg) => run_login(cfg).await,
            Mode::Token(cfg) => run_token(cfg).await,
        }
    });
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
