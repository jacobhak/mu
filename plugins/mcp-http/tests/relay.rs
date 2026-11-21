//! Bridge tests: the real `mu-mcp-http` binary talks NDJSON on stdio to an in-process rmcp
//! streamable-HTTP server that also plays an OAuth authorization server. The test plays the core: it
//! answers `core/mcpSecret` from a stored string.

use std::process::{ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::{Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
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
    /// Bodies of the token endpoint's requests.
    grants: Mutex<Vec<String>>,
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
            let req = ServerRequest::CustomRequest(CustomRequest::new("core/mcpSecret", Some(json!({"op": "get"}))));
            let r = context.peer.send_request(req).await;
            *PROBE.lock().unwrap() = Some(format!("{r:?}"));
        }
        let tool = Tool::new("echo", "Echo the input", Arc::new(serde_json::Map::new()));
        Ok(ListToolsResult::with_all_items(vec![tool]))
    }
}

/// What the server got back for its `core/mcpSecret` probe.
static PROBE: Mutex<Option<String>> = Mutex::new(None);

fn origin(headers: &HeaderMap) -> String {
    format!("http://{}", headers.get(header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("127.0.0.1"))
}

async fn oauth_metadata(headers: HeaderMap) -> axum::Json<Value> {
    let o = origin(&headers);
    axum::Json(json!({
        "issuer": o,
        "authorization_endpoint": format!("{o}/authorize"),
        "token_endpoint": format!("{o}/token"),
        "registration_endpoint": format!("{o}/register"),
        "response_types_supported": ["code"],
        "code_challenge_methods_supported": ["S256"],
    }))
}

async fn register(body: String) -> axum::Json<Value> {
    let uris = serde_json::from_str::<Value>(&body).map(|v| v["redirect_uris"].clone()).unwrap_or_default();
    axum::Json(json!({"client_id": "dcr-client", "redirect_uris": uris}))
}

/// Sends the browser straight back to the redirect URI with a code.
async fn authorize(Query(q): Query<std::collections::HashMap<String, String>>) -> Response {
    let (Some(redirect), Some(state)) = (q.get("redirect_uri"), q.get("state")) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    Redirect::temporary(&format!("{redirect}?code=authcode&state={state}")).into_response()
}

async fn token(State(shared): State<Arc<Shared>>, body: String) -> axum::Json<Value> {
    let refresh = body.contains("grant_type=refresh_token");
    shared.grants.lock().unwrap().push(body);
    axum::Json(if refresh {
        json!({"access_token": "new-access", "token_type": "Bearer", "expires_in": 3600, "refresh_token": "r2"})
    } else {
        json!({"access_token": "login-access", "token_type": "Bearer", "expires_in": 3600, "refresh_token": "login-refresh"})
    })
}

async fn gate(State(shared): State<Arc<Shared>>, req: Request, next: Next) -> Response {
    let (parts, body) = req.into_parts();
    let bytes = to_bytes(body, 1 << 20).await.unwrap_or_default();
    let text = String::from_utf8_lossy(&bytes).into_owned();
    if parts.method == Method::POST && parts.uri.path().starts_with("/mcp") {
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
    let app = Router::new()
        .route("/.well-known/oauth-authorization-server", get(oauth_metadata))
        .route("/register", post(register))
        .route("/authorize", get(authorize))
        .route("/token", post(token))
        .with_state(shared.clone())
        .nest_service("/mcp", service)
        .layer(middleware::from_fn_with_state(shared, gate));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}/mcp")
}

/// A stored credential as the bridge writes it.
fn blob(access: &str, token_response: Value, received_at: Option<u64>) -> String {
    let mut tr = json!({"access_token": access, "token_type": "Bearer"});
    tr.as_object_mut().unwrap().extend(token_response.as_object().unwrap().clone());
    json!({
        "version": 1,
        "clientId": "c",
        "redirectUri": "http://127.0.0.1:1/callback",
        "credentials": {
            "client_id": "c",
            "token_response": tr,
            "granted_scopes": [],
            "token_received_at": received_at,
        },
    })
    .to_string()
}

/// Plays the core against the bridge process.
struct Harness {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: Lines<BufReader<ChildStdout>>,
    stderr: Option<ChildStderr>,
    /// What the core has stored for the server.
    secret: Option<String>,
    /// Every `core/mcpSecret` request seen: `(op, probe)`.
    ops: Vec<(String, bool)>,
    /// The values the bridge stored with `put`, in order.
    puts: Vec<String>,
    /// Server messages that were not awaited by `response`.
    others: Vec<Value>,
}

impl Harness {
    fn spawn(env: &[(&str, &str)], secret: Option<String>) -> Self {
        Self::spawn_mode(None, env, secret)
    }

    fn spawn_mode(mode: Option<&str>, env: &[(&str, &str)], secret: Option<String>) -> Self {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_mu-mcp-http"));
        cmd.env_remove("MU_MCP_URL").env_remove("MU_MCP_HEADERS").env_remove("MU_MCP_OAUTH");
        cmd.args(mode);
        for (k, v) in env {
            cmd.env(k, v);
        }
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
        let mut child = cmd.spawn().expect("spawn mu-mcp-http");
        let stdin = child.stdin.take();
        let stdout = BufReader::new(child.stdout.take().unwrap()).lines();
        let stderr = child.stderr.take();
        Harness { child, stdin, stdout, stderr, secret, ops: Vec::new(), puts: Vec::new(), others: Vec::new() }
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

    /// The next message that is not a `core/mcpSecret` request, which is answered on the way.
    async fn next_message(&mut self) -> Option<Value> {
        loop {
            let v = self.next_line().await?;
            if v.get("method") != Some(&json!("core/mcpSecret")) {
                return Some(v);
            }
            let p = &v["params"];
            let op = p["op"].as_str().expect("op is a string").to_string();
            self.ops.push((op.clone(), p["probe"].as_bool().unwrap_or(false)));
            let result = match op.as_str() {
                "get" => json!({"value": self.secret}),
                "put" => {
                    let value = p["value"].as_str().expect("put carries a value").to_string();
                    self.puts.push(value.clone());
                    self.secret = Some(value);
                    json!({})
                }
                "delete" => {
                    self.secret = None;
                    json!({})
                }
                other => panic!("unexpected op {other}"),
            };
            self.send(json!({"jsonrpc": "2.0", "id": v["id"], "result": result})).await;
        }
    }

    /// Returns the response to request `id`, answering any `core/mcpSecret` requests on the way.
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
            let v = self.next_message().await?;
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
    let mut h = Harness::spawn(&[("MU_MCP_URL", &url), ("MU_MCP_HEADERS", r#"{"x-custom":"hello"}"#)], None);

    h.send(init_request(1)).await;
    let init = h.response(1).await;
    assert_eq!(init["result"]["protocolVersion"], "2025-11-25", "{init}");
    h.send(initialized()).await;
    h.send(tools_list(2)).await;
    let list = h.response(2).await;
    assert_eq!(list["result"]["tools"][0]["name"], "echo", "{list}");

    assert!(h.ops.is_empty());
    assert!(shared.seen_custom.lock().unwrap().iter().any(|v| v == "hello"));
    assert!(h.others.is_empty(), "unexpected messages: {:?}", h.others);
    let (status, stderr) = h.finish().await;
    assert!(status.success(), "exit {status}: {stderr}");
}

#[tokio::test(flavor = "current_thread")]
async fn a_server_cannot_reach_the_cores_private_channel() {
    let shared = Arc::new(Shared::default());
    let url = start_server(shared.clone()).await;
    let mut h = Harness::spawn(&[("MU_MCP_URL", &url)], Some(blob("must-not-be-used", json!({}), None)));
    h.send(init_request(1)).await;
    h.response(1).await;
    h.send(initialized()).await;
    // `cursor: "probe"` makes the test server send `core/mcpSecret` to its client mid-request.
    h.send(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {"cursor": "probe"}})).await;
    let list = h.response(2).await;
    assert_eq!(list["result"]["tools"][0]["name"], "echo", "{list}");
    let probe = PROBE.lock().unwrap().clone().expect("the server sent the probe");
    assert!(probe.contains("method not found"), "the bridge answered -32601 itself: {probe}");
    assert!(h.ops.is_empty(), "the core never saw a secret request: {:?}", h.ops);
    assert!(h.others.is_empty(), "nothing core/* reached the core: {:?}", h.others);
    let (status, stderr) = h.finish().await;
    assert!(status.success(), "exit {status}: {stderr}");
}

#[tokio::test(flavor = "current_thread")]
async fn a_stored_token_answers_a_401_gated_server() {
    let shared = Arc::new(Shared::default());
    *shared.accept_token.lock().unwrap() = Some("secret".into());
    let url = start_server(shared.clone()).await;
    let mut h = Harness::spawn(&[("MU_MCP_URL", &url), ("MU_MCP_OAUTH", "{}")], Some(blob("secret", json!({}), None)));

    h.send(init_request(1)).await;
    let init = h.response(1).await;
    assert_eq!(init["result"]["protocolVersion"], "2025-11-25", "{init}");
    assert_eq!(h.ops, vec![("get".to_string(), true)], "one look before connecting");

    h.send(initialized()).await;
    h.send(tools_list(2)).await;
    let list = h.response(2).await;
    assert_eq!(list["result"]["tools"][0]["name"], "echo", "{list}");
    assert!(shared.seen_auth.lock().unwrap().iter().all(|v| v == "Bearer secret"));
    assert!(h.puts.is_empty());
    assert!(h.others.is_empty(), "the secret reply leaked to the server: {:?}", h.others);
    let (status, _) = h.finish().await;
    assert!(status.success());
}

#[tokio::test(flavor = "current_thread")]
async fn without_a_stored_credential_a_401_exits_non_zero_saying_not_logged_in() {
    let shared = Arc::new(Shared::default());
    *shared.accept_token.lock().unwrap() = Some("secret".into());
    let url = start_server(shared.clone()).await;
    let mut h = Harness::spawn(&[("MU_MCP_URL", &url), ("MU_MCP_OAUTH", "{}")], None);

    h.send(init_request(1)).await;
    assert!(h.try_response(1).await.is_none(), "the bridge should exit before answering");
    let (status, stderr) = h.finish().await;
    assert!(!status.success(), "expected a failure exit");
    assert!(stderr.contains("not logged in"), "stderr: {stderr}");
}

#[tokio::test(flavor = "current_thread")]
async fn an_expired_token_is_refreshed_and_the_new_credential_stored() {
    let shared = Arc::new(Shared::default());
    *shared.accept_token.lock().unwrap() = Some("new-access".into());
    let url = start_server(shared.clone()).await;
    let old = blob("old-access", json!({"expires_in": 60, "refresh_token": "r1"}), Some(1));
    let mut h = Harness::spawn(&[("MU_MCP_URL", &url), ("MU_MCP_OAUTH", "{}")], Some(old));

    h.send(init_request(1)).await;
    let init = h.response(1).await;
    assert_eq!(init["result"]["protocolVersion"], "2025-11-25", "{init}");

    assert_eq!(h.puts.len(), 1, "{:?}", h.ops);
    let saved: Value = serde_json::from_str(&h.puts[0]).unwrap();
    assert_eq!(saved["credentials"]["token_response"]["access_token"], "new-access");
    assert_eq!(saved["credentials"]["token_response"]["refresh_token"], "r2");
    assert_eq!(saved["clientId"], "c");
    let grants = shared.grants.lock().unwrap().clone();
    assert!(grants.len() == 1 && grants[0].contains("refresh_token=r1"), "{grants:?}");
    let (status, stderr) = h.finish().await;
    assert!(status.success(), "exit {status}: {stderr}");
}

/// Waits for `login/done`, calling `on_url` with the sign-in link on the way.
async fn finish_login(h: &mut Harness, mut on_url: impl AsyncFnMut(&mut Harness, &str)) -> Value {
    loop {
        let Some(v) = h.next_message().await else { panic!("bridge stdout closed: {}", h.drain_stderr().await) };
        match v["method"].as_str() {
            Some("login/url") => on_url(h, v["params"]["url"].as_str().unwrap()).await,
            Some("login/done") => return v["params"].clone(),
            _ => h.others.push(v),
        }
    }
}

/// A browser stand-in; reqwest needs the process-wide crypto provider before it builds a client.
fn browser(follow: bool) -> reqwest::Client {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let policy = if follow { reqwest::redirect::Policy::default() } else { reqwest::redirect::Policy::none() };
    reqwest::Client::builder().redirect(policy).build().unwrap()
}

fn login_env(url: &str) -> Vec<(String, String)> {
    vec![("MU_MCP_URL".into(), url.into()), ("MU_MCP_OAUTH".into(), "{}".into())]
}

async fn spawn_login(url: &str) -> Harness {
    let env = login_env(url);
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    Harness::spawn_mode(Some("login"), &env, None)
}

#[tokio::test(flavor = "current_thread")]
async fn login_registers_follows_the_callback_and_stores_the_credential() {
    let shared = Arc::new(Shared::default());
    let url = start_server(shared.clone()).await;
    let mut h = spawn_login(&url).await;

    let done = finish_login(&mut h, async |_, link| {
        assert!(link.contains("/authorize"), "{link}");
        let page = browser(true).get(link).send().await.expect("the browser follows the redirect");
        assert!(page.status().is_success(), "{}", page.status());
    })
    .await;
    assert_eq!(done, json!({}));
    assert_eq!(h.puts.len(), 1);
    let saved: Value = serde_json::from_str(&h.puts[0]).unwrap();
    assert_eq!(saved["clientId"], "dcr-client");
    assert_eq!(saved["credentials"]["token_response"]["access_token"], "login-access");
    assert!(shared.grants.lock().unwrap()[0].contains("grant_type=authorization_code"));
    let (status, stderr) = h.finish().await;
    assert!(status.success(), "exit {status}: {stderr}");
}

#[tokio::test(flavor = "current_thread")]
async fn login_accepts_a_pasted_redirect_url() {
    let shared = Arc::new(Shared::default());
    let url = start_server(shared.clone()).await;
    let mut h = spawn_login(&url).await;

    let done = finish_login(&mut h, async |h, link| {
        let r = browser(false).get(link).send().await.unwrap();
        let back = r.headers().get(header::LOCATION).expect("a redirect").to_str().unwrap().to_string();
        h.send(json!({"jsonrpc": "2.0", "method": "login/paste", "params": {"text": back}})).await;
    })
    .await;
    assert_eq!(done, json!({}));
    assert_eq!(h.puts.len(), 1);
    let (status, stderr) = h.finish().await;
    assert!(status.success(), "exit {status}: {stderr}");
}

#[tokio::test(flavor = "current_thread")]
async fn a_dropped_session_reinitializes_and_retries() {
    let shared = Arc::new(Shared::default());
    let url = start_server(shared.clone()).await;
    let mut h = Harness::spawn(&[("MU_MCP_URL", &url)], None);

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
    let h = Harness::spawn(&[], None);
    let (status, stderr) = h.finish().await;
    assert_eq!(status.code(), Some(2));
    assert!(stderr.contains("MU_MCP_URL"), "{stderr}");
}

#[tokio::test(flavor = "current_thread")]
async fn bad_headers_are_rejected_without_echoing_the_value() {
    let h = Harness::spawn(&[("MU_MCP_URL", "http://127.0.0.1:9/mcp"), ("MU_MCP_HEADERS", r#"{"x-bad":42}"#)], None);
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
