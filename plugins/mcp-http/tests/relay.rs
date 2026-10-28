//! Bridge tests: the real `mu-mcp-http` binary talks NDJSON on stdio to an in-process rmcp
//! streamable-HTTP server. The test plays the core: it answers `core/mcpToken` from a script.

use std::collections::VecDeque;
use std::process::{ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::Response;
use rmcp::model::{
    CustomRequest, ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig, ServerRequest, Tool,
};
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData, RoleServer, ServerHandler};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};

/// Test-server knobs and observations, shared between the middleware and the test.
#[derive(Default)]
struct Shared {
    /// When set, POSTs without `Authorization: Bearer <this>` get a 401 with a challenge.
    accept_token: Mutex<Option<String>>,
    /// Authorization header of every POST, `-` when absent.
    seen_auth: Mutex<Vec<String>>,
    /// Values of the custom `x-custom` header on every POST.
    seen_custom: Mutex<Vec<String>>,
    /// When set, the next `tools/list` carrying a session id gets a 404 (session dropped).
    expire_next_tools_list: AtomicBool,
    initialize_count: AtomicU32,
}

#[derive(Clone)]
struct Echo;

impl ServerHandler for Echo {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
    }

    async fn list_tools(
        &self,
        request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        if request.as_ref().and_then(|r| r.cursor.as_deref()) == Some("probe") {
            let req = ServerRequest::CustomRequest(CustomRequest::new("core/mcpToken", Some(json!({"force": true}))));
            let r = context.peer.send_request(req).await;
            *PROBE.lock().unwrap() = Some(format!("{r:?}"));
        }
        let tool = Tool::new("echo", "Echo the input", Arc::new(serde_json::Map::new()));
        Ok(ListToolsResult::with_all_items(vec![tool]))
    }
}

/// What the server got back for its `core/mcpToken` probe.
static PROBE: Mutex<Option<String>> = Mutex::new(None);

async fn gate(State(shared): State<Arc<Shared>>, req: Request, next: Next) -> Response {
    let (parts, body) = req.into_parts();
    let bytes = to_bytes(body, 1 << 20).await.unwrap_or_default();
    let text = String::from_utf8_lossy(&bytes).into_owned();
    if parts.method == Method::POST {
        let auth = parts.headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).unwrap_or("-").to_owned();
        shared.seen_auth.lock().unwrap().push(auth.clone());
        let custom = parts.headers.get("x-custom").and_then(|v| v.to_str().ok()).unwrap_or("-").to_owned();
        shared.seen_custom.lock().unwrap().push(custom);
        if text.contains("\"method\":\"initialize\"") {
            shared.initialize_count.fetch_add(1, Ordering::SeqCst);
        }
        if let Some(token) = shared.accept_token.lock().unwrap().clone()
            && auth != format!("Bearer {token}")
        {
            let mut resp = Response::new(Body::from("unauthorized"));
            *resp.status_mut() = StatusCode::UNAUTHORIZED;
            resp.headers_mut().insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer realm=\"test\""));
            return resp;
        }
        if parts.headers.contains_key("mcp-session-id")
            && text.contains("\"method\":\"tools/list\"")
            && shared.expire_next_tools_list.swap(false, Ordering::SeqCst)
        {
            let mut resp = Response::new(Body::from("session not found"));
            *resp.status_mut() = StatusCode::NOT_FOUND;
            return resp;
        }
    }
    next.run(Request::from_parts(parts, Body::from(bytes))).await
}

/// Starts the server on a free loopback port and returns its MCP endpoint.
async fn start_server(shared: Arc<Shared>) -> String {
    let service = StreamableHttpService::new(
        || Ok(Echo),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    );
    let app = Router::new().nest_service("/mcp", service).layer(middleware::from_fn_with_state(shared, gate));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}/mcp")
}

/// Plays the core against the bridge process.
struct Harness {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: Lines<BufReader<ChildStdout>>,
    stderr: Option<ChildStderr>,
    /// One entry per `core/mcpToken` request: Ok(token) or Err(message), in order.
    token_script: VecDeque<Result<String, String>>,
    /// The `force` flag of each `core/mcpToken` request seen.
    token_requests: Vec<bool>,
    /// Server messages that were not awaited by `response`.
    others: Vec<Value>,
}

impl Harness {
    fn spawn(env: &[(&str, &str)], script: Vec<Result<String, String>>) -> Self {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_mu-mcp-http"));
        cmd.env_remove("MU_MCP_URL").env_remove("MU_MCP_HEADERS").env_remove("MU_MCP_AUTH");
        for (k, v) in env {
            cmd.env(k, v);
        }
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
        let mut child = cmd.spawn().expect("spawn mu-mcp-http");
        let stdin = child.stdin.take();
        let stdout = BufReader::new(child.stdout.take().unwrap()).lines();
        let stderr = child.stderr.take();
        Harness {
            child,
            stdin,
            stdout,
            stderr,
            token_script: script.into(),
            token_requests: Vec::new(),
            others: Vec::new(),
        }
    }

    async fn send(&mut self, v: Value) {
        let stdin = self.stdin.as_mut().expect("stdin open");
        stdin.write_all(format!("{v}\n").as_bytes()).await.unwrap();
        stdin.flush().await.unwrap();
    }

    async fn next_line(&mut self) -> Option<Value> {
        let line = tokio::time::timeout(Duration::from_secs(20), self.stdout.next_line())
            .await
            .expect("timed out waiting for the bridge")
            .unwrap()?;
        Some(serde_json::from_str(&line).expect("bridge wrote a non-JSON line"))
    }

    /// Returns the response to request `id`, answering any `core/mcpToken` requests on the way.
    async fn response(&mut self, id: u64) -> Value {
        match self.try_response(id).await {
            Some(v) => v,
            None => panic!("bridge stdout closed: {}", self.drain_stderr().await),
        }
    }

    async fn drain_stderr(&mut self) -> String {
        use tokio::io::AsyncReadExt;
        let mut s = String::new();
        if let Some(mut e) = self.stderr.take() {
            let _ = tokio::time::timeout(Duration::from_secs(5), e.read_to_string(&mut s)).await;
        }
        s
    }

    /// Like `response`, but returns None when the bridge closes stdout first.
    async fn try_response(&mut self, id: u64) -> Option<Value> {
        loop {
            let v = self.next_line().await?;
            if v.get("method") == Some(&json!("core/mcpToken")) {
                let force = v["params"]["force"].as_bool().expect("force is a bool");
                self.token_requests.push(force);
                let reply = match self.token_script.pop_front().expect("unexpected core/mcpToken request") {
                    Ok(token) => json!({"jsonrpc": "2.0", "id": v["id"], "result": {"token": token}}),
                    Err(msg) => json!({"jsonrpc": "2.0", "id": v["id"], "error": {"code": -32000, "message": msg}}),
                };
                self.send(reply).await;
                continue;
            }
            if v.get("method").is_none() && v.get("id") == Some(&json!(id)) {
                return Some(v);
            }
            self.others.push(v);
        }
    }

    /// Closes stdin and waits for the bridge to exit. Returns its status and stderr.
    async fn finish(mut self) -> (ExitStatus, String) {
        drop(self.stdin.take());
        let status = tokio::time::timeout(Duration::from_secs(20), self.child.wait())
            .await
            .expect("bridge did not exit")
            .unwrap();
        let mut stderr = String::new();
        if let Some(mut e) = self.stderr.take() {
            use tokio::io::AsyncReadExt;
            e.read_to_string(&mut stderr).await.unwrap();
        }
        (status, stderr)
    }
}

fn init_request(id: u64) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": {"name": "test-core", "version": "0"}
        }
    })
}

fn initialized() -> Value {
    json!({"jsonrpc": "2.0", "method": "notifications/initialized"})
}

fn tools_list(id: u64) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": "tools/list", "params": {}})
}

#[tokio::test(flavor = "current_thread")]
async fn relays_initialize_and_tools_list_with_custom_headers() {
    let shared = Arc::new(Shared::default());
    let url = start_server(shared.clone()).await;
    let mut h = Harness::spawn(&[("MU_MCP_URL", &url), ("MU_MCP_HEADERS", r#"{"x-custom":"hello"}"#)], vec![]);

    h.send(init_request(1)).await;
    let init = h.response(1).await;
    assert_eq!(init["result"]["protocolVersion"], "2025-11-25", "{init}");
    h.send(initialized()).await;
    h.send(tools_list(2)).await;
    let list = h.response(2).await;
    assert_eq!(list["result"]["tools"][0]["name"], "echo", "{list}");

    assert!(h.token_requests.is_empty());
    assert!(shared.seen_custom.lock().unwrap().iter().any(|v| v == "hello"));
    assert!(h.others.is_empty(), "unexpected messages: {:?}", h.others);
    let (status, stderr) = h.finish().await;
    assert!(status.success(), "exit {status}: {stderr}");
}

#[tokio::test(flavor = "current_thread")]
async fn a_server_cannot_reach_the_cores_private_channel() {
    let shared = Arc::new(Shared::default());
    let url = start_server(shared.clone()).await;
    let mut h = Harness::spawn(&[("MU_MCP_URL", &url)], vec![Ok("must-not-be-used".into())]);
    h.send(init_request(1)).await;
    h.response(1).await;
    h.send(initialized()).await;
    // `cursor: "probe"` makes the test server send `core/mcpToken` to its client mid-request.
    h.send(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {"cursor": "probe"}})).await;
    let list = h.response(2).await;
    assert_eq!(list["result"]["tools"][0]["name"], "echo", "{list}");
    let probe = PROBE.lock().unwrap().clone().expect("the server sent the probe");
    assert!(probe.contains("method not found"), "the bridge answered -32601 itself: {probe}");
    assert!(h.token_requests.is_empty(), "the core never saw a token request");
    assert!(h.others.is_empty(), "nothing core/* reached the core: {:?}", h.others);
    let (status, stderr) = h.finish().await;
    assert!(status.success(), "exit {status}: {stderr}");
}

#[tokio::test(flavor = "current_thread")]
async fn a_401_asks_the_core_for_a_token_and_retries_with_it() {
    let shared = Arc::new(Shared::default());
    *shared.accept_token.lock().unwrap() = Some("secret".into());
    let url = start_server(shared.clone()).await;
    let mut h = Harness::spawn(&[("MU_MCP_URL", &url)], vec![Ok("secret".into())]);

    h.send(init_request(1)).await;
    let init = h.response(1).await;
    assert_eq!(init["result"]["protocolVersion"], "2025-11-25", "{init}");
    assert_eq!(h.token_requests, vec![false]);

    h.send(initialized()).await;
    h.send(tools_list(2)).await;
    let list = h.response(2).await;
    assert_eq!(list["result"]["tools"][0]["name"], "echo", "{list}");
    assert!(shared.seen_auth.lock().unwrap().iter().any(|v| v == "Bearer secret"));
    assert!(h.others.is_empty(), "the token reply leaked to the server: {:?}", h.others);
    let (status, _) = h.finish().await;
    assert!(status.success());
}

#[tokio::test(flavor = "current_thread")]
async fn a_stale_token_is_refreshed_once_with_force() {
    let shared = Arc::new(Shared::default());
    *shared.accept_token.lock().unwrap() = Some("fresh".into());
    let url = start_server(shared.clone()).await;
    let mut h = Harness::spawn(&[("MU_MCP_URL", &url)], vec![Ok("stale".into()), Ok("fresh".into())]);

    h.send(init_request(1)).await;
    let init = h.response(1).await;
    assert_eq!(init["result"]["protocolVersion"], "2025-11-25", "{init}");
    assert_eq!(h.token_requests, vec![false, true]);
    let (status, _) = h.finish().await;
    assert!(status.success());
}

#[tokio::test(flavor = "current_thread")]
async fn a_token_that_still_fails_exits_non_zero() {
    let shared = Arc::new(Shared::default());
    *shared.accept_token.lock().unwrap() = Some("fresh".into());
    let url = start_server(shared.clone()).await;
    let mut h =
        Harness::spawn(&[("MU_MCP_URL", &url)], vec![Ok("stale".into()), Err("no credential for this server".into())]);

    h.send(init_request(1)).await;
    assert!(h.try_response(1).await.is_none(), "the bridge should exit before answering");
    let (status, stderr) = h.finish().await;
    assert!(!status.success(), "expected a failure exit");
    assert!(stderr.contains("no credential for this server"), "stderr: {stderr}");
    assert!(!stderr.contains("fresh") && !stderr.contains("stale"), "token leaked: {stderr}");
}

#[tokio::test(flavor = "current_thread")]
async fn a_dropped_session_reinitializes_and_retries() {
    let shared = Arc::new(Shared::default());
    let url = start_server(shared.clone()).await;
    let mut h = Harness::spawn(&[("MU_MCP_URL", &url)], vec![]);

    h.send(init_request(1)).await;
    let init = h.response(1).await;
    assert_eq!(init["result"]["protocolVersion"], "2025-11-25", "{init}");
    h.send(initialized()).await;

    shared.expire_next_tools_list.store(true, Ordering::SeqCst);
    h.send(tools_list(2)).await;
    let list = h.response(2).await;
    assert_eq!(list["result"]["tools"][0]["name"], "echo", "{list}");

    assert_eq!(shared.initialize_count.load(Ordering::SeqCst), 2, "expected a replayed initialize");
    // The replayed handshake's response is dropped; the core never sees a second id 1.
    assert!(
        h.others.iter().all(|v| v.get("id") != Some(&json!(1))),
        "replayed initialize response leaked: {:?}",
        h.others
    );
    let (status, _) = h.finish().await;
    assert!(status.success());
}

#[tokio::test(flavor = "current_thread")]
async fn missing_url_is_a_usage_error() {
    let h = Harness::spawn(&[], vec![]);
    let (status, stderr) = h.finish().await;
    assert_eq!(status.code(), Some(2));
    assert!(stderr.contains("MU_MCP_URL"), "{stderr}");
}

#[tokio::test(flavor = "current_thread")]
async fn bad_headers_are_rejected_without_echoing_the_value() {
    let h = Harness::spawn(&[("MU_MCP_URL", "http://127.0.0.1:9/mcp"), ("MU_MCP_HEADERS", r#"{"x-bad":42}"#)], vec![]);
    let (status, stderr) = h.finish().await;
    assert_eq!(status.code(), Some(2));
    assert!(stderr.contains("x-bad"), "{stderr}");
    assert!(!stderr.contains("42"), "value echoed: {stderr}");
}

/// Manual: serves the test MCP server for two minutes, for measuring the bridge's RSS.
#[tokio::test(flavor = "current_thread")]
#[ignore]
async fn serve_for_measurement() {
    let url = start_server(Arc::new(Shared::default())).await;
    println!("URL={url}");
    tokio::time::sleep(Duration::from_secs(120)).await;
}
