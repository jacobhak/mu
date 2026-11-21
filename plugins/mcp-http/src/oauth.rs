//! OAuth for one MCP server, on rmcp's OAuth client. The credential lives in core's secret store
//! (`core/mcpSecret`); this process only holds it while it works. `login` runs the authorization-code
//! flow with a loopback callback and a paste fallback, `access_token` returns a usable token.

use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use rmcp::transport::auth::{
    AuthError, AuthorizationManager, AuthorizationRequest, AuthorizationSession, CredentialStore, OAuthClientConfig,
    StoredCredentials,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use url::Url;

use crate::core_rpc::CoreRpc;

const LOGIN_LIMIT: Duration = Duration::from_secs(10 * 60);
const READ_LIMIT: Duration = Duration::from_secs(5);
const MAX_HEAD: usize = 16 * 1024;
/// rmcp refreshes this long before a token expires; so does the shortcut in `fresh`.
const REFRESH_BUFFER_SECS: u64 = 30;
const PAGE: &str = "<!doctype html><meta charset=\"utf-8\"><title>mu</title>\
    <p>You can close this tab and return to mu.</p>";
const PAGE_NOT_FOUND: &str = "<!doctype html><meta charset=\"utf-8\"><title>mu</title><p>Not found.</p>";

/// The settings from `MU_MCP_URL` and `MU_MCP_OAUTH`.
#[derive(Clone)]
pub struct OAuthCfg {
    pub url: String,
    /// `MU_MCP_OAUTH` is set: the bridge looks for a stored credential before connecting.
    pub enabled: bool,
    client_id: Option<String>,
    client_secret: Option<String>,
    scope: Option<String>,
    callback_port: Option<u16>,
    callback_path: String,
    redirect_uri: Option<String>,
    token_url: Option<String>,
}

impl OAuthCfg {
    pub fn parse(url: String, env: Option<String>) -> Result<Self, String> {
        let mut cfg = OAuthCfg {
            url,
            enabled: false,
            client_id: None,
            client_secret: None,
            scope: None,
            callback_port: None,
            callback_path: "/callback".into(),
            redirect_uri: None,
            token_url: None,
        };
        let Some(s) = env.filter(|s| !s.trim().is_empty()) else { return Ok(cfg) };
        let v: Value = serde_json::from_str(&s).map_err(|_| "MU_MCP_OAUTH is not valid JSON")?;
        let Value::Object(o) = v else { return Err("MU_MCP_OAUTH must be a JSON object".into()) };
        let text = |k: &str| o.get(k).and_then(Value::as_str).filter(|s| !s.is_empty()).map(String::from);
        cfg.enabled = true;
        cfg.client_id = text("clientId");
        cfg.client_secret = text("clientSecret");
        cfg.scope = text("scope");
        cfg.redirect_uri = text("redirectUri");
        cfg.token_url = text("tokenUrl");
        if let Some(p) = text("callbackPath") {
            cfg.callback_path = if p.starts_with('/') { p } else { format!("/{p}") };
        }
        cfg.callback_port = match o.get("callbackPort") {
            Some(Value::Number(n)) => n.as_u64(),
            Some(Value::String(s)) => s.trim().parse().ok(),
            _ => None,
        }
        .and_then(|p| u16::try_from(p).ok());
        Ok(cfg)
    }
}

/// What is stored for a server. `credentials` is rmcp's own record.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Blob {
    version: u32,
    client_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    client_secret: Option<String>,
    redirect_uri: String,
    credentials: StoredCredentials,
}

pub enum Fail {
    NeedsLogin,
    Other(String),
}

impl From<String> for Fail {
    fn from(s: String) -> Self {
        Fail::Other(s)
    }
}

async fn load_blob(rpc: &CoreRpc, probe: bool) -> Result<Option<Blob>, String> {
    let v = rpc.call("core/mcpSecret", json!({"op": "get", "probe": probe})).await?;
    match v["value"].as_str() {
        None => Ok(None),
        Some(s) => {
            serde_json::from_str(s).map(Some).map_err(|_| "the stored credential is unreadable; log in again".into())
        }
    }
}

struct RpcStore {
    rpc: CoreRpc,
    client_secret: Option<String>,
    redirect_uri: String,
}

#[async_trait]
impl CredentialStore for RpcStore {
    async fn load(&self) -> Result<Option<StoredCredentials>, AuthError> {
        let blob = load_blob(&self.rpc, false).await.map_err(AuthError::CredentialStoreError)?;
        Ok(blob.map(|b| b.credentials))
    }

    async fn save(&self, credentials: StoredCredentials) -> Result<(), AuthError> {
        let blob = Blob {
            version: 1,
            client_id: credentials.client_id.clone(),
            client_secret: self.client_secret.clone(),
            redirect_uri: self.redirect_uri.clone(),
            credentials,
        };
        let value = serde_json::to_string(&blob).map_err(|e| AuthError::CredentialStoreError(e.to_string()))?;
        self.rpc
            .call("core/mcpSecret", json!({"op": "put", "value": value}))
            .await
            .map(|_| ())
            .map_err(AuthError::CredentialStoreError)
    }

    async fn clear(&self) -> Result<(), AuthError> {
        self.rpc
            .call("core/mcpSecret", json!({"op": "delete"}))
            .await
            .map(|_| ())
            .map_err(AuthError::CredentialStoreError)
    }
}

async fn manager(cfg: &OAuthCfg) -> Result<AuthorizationManager, String> {
    let mut m = AuthorizationManager::new(cfg.url.as_str()).await.map_err(|e| e.to_string())?;
    let mut metadata = m.resolve_metadata().await.map_err(|e| e.to_string())?.metadata;
    if let Some(t) = &cfg.token_url {
        metadata.token_endpoint = t.clone();
    }
    m.set_metadata(metadata);
    Ok(m)
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

/// The stored access token when it is not about to expire, so the common case needs no network.
fn fresh(b: &Blob) -> Option<String> {
    let tr = serde_json::to_value(b.credentials.token_response.as_ref()?).ok()?;
    let token = tr["access_token"].as_str()?.to_string();
    if let (Some(expires_in), Some(at)) = (tr["expires_in"].as_u64(), b.credentials.token_received_at)
        && (at + expires_in).saturating_sub(now()) < REFRESH_BUFFER_SECS
    {
        return None;
    }
    Some(token)
}

async fn delete(rpc: &CoreRpc) {
    let _ = rpc.call("core/mcpSecret", json!({"op": "delete"})).await;
}

async fn token_of(cfg: &OAuthCfg, rpc: &CoreRpc, blob: Blob, force: bool) -> Result<String, Fail> {
    if !force && let Some(t) = fresh(&blob) {
        return Ok(t);
    }
    let mut m = manager(cfg).await?;
    m.set_credential_store(RpcStore {
        rpc: rpc.clone(),
        client_secret: blob.client_secret.clone(),
        redirect_uri: blob.redirect_uri.clone(),
    });
    // Before `configure_client`: `initialize_from_store` configures the client without its secret.
    match m.initialize_from_store().await {
        Ok(true) => {}
        Ok(false) => return Err(Fail::NeedsLogin),
        Err(e) => return Err(Fail::Other(e.to_string())),
    }
    let mut oc = OAuthClientConfig::new(blob.client_id.clone(), blob.redirect_uri.clone());
    if let Some(s) = &blob.client_secret {
        oc = oc.with_client_secret(s.clone());
    }
    m.configure_client(oc).map_err(|e| e.to_string())?;
    if force {
        match m.refresh_token().await {
            Ok(_) => {}
            Err(AuthError::TokenRefreshRejected(_) | AuthError::AuthorizationRequired) => {
                delete(rpc).await;
                return Err(Fail::NeedsLogin);
            }
            Err(e) => return Err(Fail::Other(e.to_string())),
        }
    }
    match m.get_access_token().await {
        Ok(t) => Ok(t),
        Err(AuthError::AuthorizationRequired | AuthError::TokenRefreshRejected(_)) => {
            delete(rpc).await;
            Err(Fail::NeedsLogin)
        }
        Err(e) => Err(Fail::Other(e.to_string())),
    }
}

/// An access token for the server, refreshed when it is stale (or when `force`). `NeedsLogin` when
/// nothing is stored or the refresh was refused.
pub async fn access_token(cfg: &OAuthCfg, rpc: &CoreRpc, force: bool) -> Result<String, Fail> {
    match load_blob(rpc, false).await? {
        Some(blob) => token_of(cfg, rpc, blob, force).await,
        None => Err(Fail::NeedsLogin),
    }
}

/// Like `access_token`, but `None` when nothing is stored (the server may not need OAuth at all),
/// and without telling the core that a login is missing.
pub async fn up_front_token(cfg: &OAuthCfg, rpc: &CoreRpc) -> Result<Option<String>, Fail> {
    match load_blob(rpc, true).await? {
        Some(blob) => token_of(cfg, rpc, blob, false).await.map(Some),
        None => Ok(None),
    }
}

enum Step {
    Done {
        code: String,
        state: String,
        issuer: Option<String>,
    },
    /// Not this sign-in's callback: say why and keep waiting.
    Retry(String),
    Fail(String),
}

fn read_callback(url: &Url, state: Option<&str>) -> Step {
    let q: HashMap<String, String> = url.query_pairs().into_owned().collect();
    let get = |k: &str| q.get(k).map(String::as_str).filter(|v| !v.is_empty());
    if let Some(want) = state
        && get("state") != Some(want)
    {
        return Step::Retry("That callback belongs to another sign-in; keep waiting for this one.".into());
    }
    if let Some(e) = get("error") {
        return Step::Fail(format!("the server refused the sign-in: {}", e.chars().take(80).collect::<String>()));
    }
    match (get("code"), get("state")) {
        (Some(code), Some(state)) => {
            Step::Done { code: code.into(), state: state.into(), issuer: get("iss").map(String::from) }
        }
        _ => Step::Retry("The callback has no authorization code.".into()),
    }
}

fn read_paste(text: &str, want: &Url, state: Option<&str>) -> Step {
    let hint = || Step::Retry(format!("Paste the full address from the browser, starting with {want}"));
    let Ok(url) = Url::parse(text.trim()) else { return hint() };
    let ours = url.scheme() == want.scheme()
        && url.host_str() == want.host_str()
        && url.port_or_known_default() == want.port_or_known_default()
        && url.path() == want.path();
    if ours { read_callback(&url, state) } else { hint() }
}

async fn read_head(stream: &mut TcpStream) -> Option<String> {
    tokio::time::timeout(READ_LIMIT, async {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 1024];
        while buf.len() < MAX_HEAD && !buf.windows(4).any(|w| w == b"\r\n\r\n") {
            match stream.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        }
        (!buf.is_empty()).then(|| String::from_utf8_lossy(&buf).into_owned())
    })
    .await
    .ok()
    .flatten()
}

async fn respond(stream: &mut TcpStream, status: &str, body: &str) {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes()).await;
    let _ = stream.write_all(body.as_bytes()).await;
    let _ = stream.shutdown().await;
}

/// Answers one loopback connection and forwards a request for the callback path. Connections that
/// send nothing or ask for another path end no sign-in: browsers open idle ones ahead of time.
async fn serve(mut stream: TcpStream, path: String, hits: UnboundedSender<Url>) {
    let Some(head) = read_head(&mut stream).await else { return };
    let target = head.lines().next().and_then(|line| {
        let mut parts = line.split_whitespace();
        match (parts.next(), parts.next()) {
            (Some("GET"), Some(t)) => Some(t.to_string()),
            _ => None,
        }
    });
    let url = target.and_then(|t| Url::parse("http://127.0.0.1/").ok()?.join(&t).ok()).filter(|u| u.path() == path);
    match url {
        Some(u) => {
            respond(&mut stream, "200 OK", PAGE).await;
            let _ = hits.send(u);
        }
        None => respond(&mut stream, "404 Not Found", PAGE_NOT_FOUND).await,
    }
}

/// Runs the authorization-code flow. Emits `login/url` and `login/notice`; a pasted redirect URL
/// arrives on `paste`, whose closing (stdin EOF) cancels the sign-in.
pub async fn login(cfg: &OAuthCfg, rpc: &CoreRpc, paste: &mut UnboundedReceiver<String>) -> Result<(), String> {
    let listener = TcpListener::bind(("127.0.0.1", cfg.callback_port.unwrap_or(0)))
        .await
        .map_err(|e| format!("cannot listen for the sign-in callback: {e}"))?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let redirect = cfg.redirect_uri.clone().unwrap_or_else(|| format!("http://127.0.0.1:{port}{}", cfg.callback_path));
    let want = Url::parse(&redirect).map_err(|_| "the redirect URI is not a URL".to_string())?;

    let mut m = manager(cfg).await?;
    m.set_credential_store(RpcStore {
        rpc: rpc.clone(),
        client_secret: cfg.client_secret.clone(),
        redirect_uri: redirect.clone(),
    });
    let mut req = AuthorizationRequest::new(redirect.clone()).with_client_name("mu");
    if let Some(id) = &cfg.client_id {
        req = req.with_preregistered_client(id.clone());
        if let Some(s) = &cfg.client_secret {
            req = req.with_client_secret(s.clone());
        }
    }
    if let Some(s) = &cfg.scope {
        req = req.with_scopes(s.split_whitespace());
    }
    let session = AuthorizationSession::new(m, req).await.map_err(|(_, e)| e.to_string())?;
    let state = Url::parse(&session.auth_url)
        .ok()
        .and_then(|u| u.query_pairs().find(|(k, _)| k == "state").map(|(_, v)| v.into_owned()));
    rpc.notify("login/url", json!({"url": session.auth_url}));

    let (tx, mut hits) = unbounded_channel();
    let deadline = tokio::time::sleep(LOGIN_LIMIT);
    tokio::pin!(deadline);
    let (code, state, issuer) = loop {
        let step = tokio::select! {
            accepted = listener.accept() => {
                if let Ok((stream, _)) = accepted {
                    tokio::spawn(serve(stream, cfg.callback_path.clone(), tx.clone()));
                }
                continue;
            }
            Some(url) = hits.recv() => read_callback(&url, state.as_deref()),
            text = paste.recv() => match text {
                Some(t) => read_paste(&t, &want, state.as_deref()),
                None => return Err("cancelled".into()),
            },
            _ = &mut deadline => return Err("the sign-in timed out".into()),
        };
        match step {
            Step::Done { code, state, issuer } => break (code, state, issuer),
            Step::Retry(why) => rpc.notify("login/notice", json!({"text": why})),
            Step::Fail(why) => return Err(why),
        }
    };
    session.handle_callback_with_issuer(&code, &state, issuer.as_deref()).await.map_err(|e| e.to_string())?;
    Ok(())
}
