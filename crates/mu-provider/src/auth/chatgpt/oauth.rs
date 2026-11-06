//! The Sign in with ChatGPT protocol. Pure network and crypto: nothing here reads or writes the store.

mod callback;
mod jwt;
#[cfg(test)]
mod tests;

use super::{AGENT_NAME, API_BASE, Account, Endpoints, Known, Tokens};
use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use callback::{Expect, Issued, Step};
use reqwest::Url;
use ring::rand::{SecureRandom, SystemRandom};
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::{self, UnboundedReceiver};
use tokio_util::sync::CancellationToken;

const DYNAMIC_CLIENT_ID: &str = "dynamic_agent_client";
/// A first registration must use this port: the redirect URI is registered with it.
const CALLBACK_PORT: u16 = 1455;
const CALLBACK_PATH: &str = "/auth/callback";
const SCOPE: &str = "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
/// Refresh this long before the access token expires, so a request never starts with a token about to expire.
const REFRESH_MARGIN: i64 = 180;
const SIGN_IN_LIMIT: Duration = Duration::from_secs(600);
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);
/// Accepted just past `exp` to absorb clock skew.
pub(super) const LEEWAY: i64 = 60;
const SIGN_IN_AGAIN: &str = "Run /login, or `mu auth login openai-chatgpt`, to sign in again.";
const UNUSABLE_REFRESH: [&str; 6] = [
    "invalid_grant",
    "invalid_refresh_token",
    "token_expired",
    "refresh_token_expired",
    "refresh_token_invalidated",
    "refresh_token_reused",
];
#[cfg(not(test))]
const REVOKE_BACKOFF: [Duration; 2] = [Duration::from_secs(1), Duration::from_secs(2)];
#[cfg(test)]
const REVOKE_BACKOFF: [Duration; 2] = [Duration::from_millis(10), Duration::from_millis(20)];

pub struct LoginRequest<'a> {
    pub endpoints: &'a Endpoints,
    /// `urn:uuid:…`, sent as `ext_agent_host_id`.
    pub host_id: &'a str,
    /// `Some`: reauthorize this registration (its client id, `id_token_hint`, `login_hint`; no
    /// `agent_name_hint`). `None`: register with `client_id=dynamic_agent_client`.
    pub known: Option<Known>,
    pub cancel: CancellationToken,
}

/// Signs in through the system browser.
///
/// Starts a listener on `127.0.0.1:1455` (a reauthorization falls back to any free port when that
/// one is busy), calls `on_url` once with the authorization URL (the caller shows it and tries to
/// open a browser), then waits for the loopback callback or for text arriving on `paste` (the
/// redirect URL, for machines the browser's callback cannot reach), whichever comes first. A
/// callback or paste that does not belong to this sign-in is reported through `notice` and the
/// wait goes on. Validates `state`, the issued client id, the ID token (RS256 signature against the
/// issuer's JWKS, issuer, audience, expiry, nonce), and returns the account with its tokens. The
/// caller must check `Account::plan_usage`: the grant may lack `chatgpt.tokens.use.direct`.
pub async fn login(
    req: LoginRequest<'_>,
    on_url: &mut dyn FnMut(&str),
    notice: &mut dyn FnMut(&str),
    paste: &mut UnboundedReceiver<String>,
) -> Result<Account> {
    sign_in(req, None, on_url, notice, paste).await
}

/// `port` overrides the callback port (tests bind 0). `None` is production: 1455, then any port for a reauthorization.
async fn sign_in(
    req: LoginRequest<'_>,
    port: Option<u16>,
    on_url: &mut dyn FnMut(&str),
    notice: &mut dyn FnMut(&str),
    paste: &mut UnboundedReceiver<String>,
) -> Result<Account> {
    let LoginRequest { endpoints: ep, host_id, known, cancel } = req;
    let listener = callback::bind(port, known.is_some()).await?;
    let bound = listener.local_addr()?.port();
    let redirect = format!("http://127.0.0.1:{bound}{CALLBACK_PATH}");
    let verifier = random_token()?;
    let state = random_token()?;
    let nonce = random_token()?;
    let url = authorize_url(ep, host_id, known.as_ref(), &redirect, &pkce_challenge(&verifier), &state, &nonce)?;
    let deadline = tokio::time::Instant::now() + SIGN_IN_LIMIT;
    on_url(url.as_str());

    let expect = Arc::new(Expect { state, known: known.as_ref().map(|k| k.client_id.clone()) });
    let (steps_tx, mut steps_rx) = mpsc::unbounded_channel::<Step>();
    let issued: Issued = loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => bail!("sign-in cancelled"),
            _ = tokio::time::sleep_until(deadline) => bail!("sign-in timed out after 10 minutes; start it again"),
            step = steps_rx.recv() => match step.context("the sign-in callback channel closed")? {
                Step::Done(issued) => break issued,
                Step::Fail(msg) => bail!(msg),
                Step::Retry(msg) => notice(msg.as_str()),
            },
            Some(text) = paste.recv() => match callback::read_paste(&text, bound, &expect) {
                Step::Done(issued) => break issued,
                Step::Fail(msg) => bail!(msg),
                Step::Retry(msg) => notice(msg.as_str()),
            },
            accepted = listener.accept() => {
                let (stream, _) = accepted.context("the sign-in callback listener failed")?;
                tokio::spawn(callback::serve(stream, expect.clone(), steps_tx.clone()));
            }
        }
    };
    drop(listener);

    let now = unix_now();
    let tokens = login_tokens(exchange_code(ep, &issued, &verifier, &redirect).await?, now)?;
    let claims = jwt::verify_id_token(ep, &tokens.id_token, &issued.client_id, &nonce, now).await?;
    Ok(Account {
        client_id: issued.client_id,
        subject: claims.sub,
        email: claims.email,
        tokens: Some(tokens),
        added: crate::auth::store::now(),
    })
}

fn authorize_url(
    ep: &Endpoints,
    host_id: &str,
    known: Option<&Known>,
    redirect: &str,
    challenge: &str,
    state: &str,
    nonce: &str,
) -> Result<Url> {
    let mut q: Vec<(&str, &str)> = Vec::new();
    match known {
        None => {
            q.push(("client_id", DYNAMIC_CLIENT_ID));
            q.push(("agent_name_hint", AGENT_NAME));
        }
        Some(k) => {
            q.push(("client_id", k.client_id.as_str()));
            if let Some(t) = &k.id_token {
                q.push(("id_token_hint", t.as_str()));
            }
            if let Some(e) = &k.email {
                q.push(("login_hint", e.as_str()));
            }
        }
    }
    q.extend([
        ("ext_agent_host_id", host_id),
        ("response_type", "code"),
        ("redirect_uri", redirect),
        ("resource", API_BASE),
        ("scope", SCOPE),
        ("state", state),
        ("nonce", nonce),
        ("code_challenge", challenge),
        ("code_challenge_method", "S256"),
    ]);
    Url::parse_with_params(&ep.authorize, &q).context("building the authorization URL")
}

/// A fresh 32-byte value, base64url without padding (43 characters: valid as a PKCE verifier).
fn random_token() -> Result<String> {
    let mut bytes = [0u8; 32];
    SystemRandom::new().fill(&mut bytes).map_err(|_| anyhow!("the system random source failed"))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

/// The S256 code challenge for a verifier (RFC 7636).
fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()).as_slice())
}

async fn exchange_code(ep: &Endpoints, issued: &Issued, verifier: &str, redirect: &str) -> Result<TokenBody> {
    let client = crate::net::http_client().context("building the HTTP client")?;
    let resp = form_post(
        &client,
        &ep.token,
        &[
            ("grant_type", "authorization_code"),
            ("client_id", issued.client_id.as_str()),
            ("code", issued.code.as_str()),
            ("code_verifier", verifier),
            ("redirect_uri", redirect),
            ("resource", API_BASE),
        ],
    )
    .send()
    .await
    .context("requesting tokens from ChatGPT")?;
    let status = resp.status();
    let body = resp.text().await.context("reading ChatGPT's token response")?;
    if !status.is_success() {
        bail!("ChatGPT token request failed ({})", describe(status.as_u16(), error_code(&body).as_deref()));
    }
    serde_json::from_str(&body).context("ChatGPT's token response is not JSON")
}

#[derive(Deserialize)]
struct TokenBody {
    access_token: Option<String>,
    refresh_token: Option<String>,
    id_token: Option<String>,
    scope: Option<String>,
    expires_in: Option<i64>,
}

fn login_tokens(body: TokenBody, now: i64) -> Result<Tokens> {
    let expires_in = body.expires_in.filter(|e| *e > 0).context("ChatGPT's token response has no valid expires_in")?;
    Ok(Tokens {
        access_token: nonempty(body.access_token).context("ChatGPT's token response has no access_token")?,
        refresh_token: nonempty(body.refresh_token).context("ChatGPT's token response has no refresh_token")?,
        id_token: nonempty(body.id_token).context("ChatGPT's token response has no id_token")?,
        refresh_at: now + expires_in - REFRESH_MARGIN,
        scopes: split_scopes(&nonempty(body.scope).context("ChatGPT's token response has no scope")?),
    })
}

#[derive(Debug, Clone, PartialEq)]
pub enum RefreshError {
    /// The refresh token is unusable (`invalid_grant`, `refresh_token_reused`, …): sign in again.
    SignInAgain(String),
    /// Network or server trouble: keep the credentials and try later.
    Transient(String),
}

impl std::fmt::Display for RefreshError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SignInAgain(m) | Self::Transient(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for RefreshError {}

/// Exchanges the refresh token for a new token set (access, replacement refresh token, scopes, and
/// the ID token when the response has one, else the old one).
pub async fn refresh(ep: &Endpoints, account: &Account) -> Result<Tokens, RefreshError> {
    let Some(old) = account.tokens.as_ref() else {
        return Err(RefreshError::SignInAgain(format!("There is no ChatGPT sign-in to refresh. {SIGN_IN_AGAIN}")));
    };
    let client = crate::net::http_client()
        .map_err(|e| RefreshError::Transient(format!("could not start the HTTP client: {e}")))?;
    let resp = form_post(
        &client,
        &ep.token,
        &[
            ("grant_type", "refresh_token"),
            ("client_id", account.client_id.as_str()),
            ("refresh_token", old.refresh_token.as_str()),
            ("resource", API_BASE),
        ],
    )
    .send()
    .await
    .map_err(|e| {
        RefreshError::Transient(format!("could not reach ChatGPT to refresh the sign-in: {}", e.without_url()))
    })?;
    let status = resp.status();
    let body = resp.text().await.map_err(|e| {
        RefreshError::Transient(format!("could not read ChatGPT's refresh response: {}", e.without_url()))
    })?;
    if status.is_success() {
        let body: TokenBody = serde_json::from_str(&body)
            .map_err(|_| RefreshError::Transient("ChatGPT's refresh response is not JSON; mu will try again".into()))?;
        return refreshed(body, old, unix_now()).ok_or_else(|| {
            RefreshError::Transient("ChatGPT's refresh response is incomplete; mu will try again".into())
        });
    }
    Err(classify_refresh(status.as_u16(), &body))
}

/// The new token set. The refresh token and ID token are kept from `old` when the response omits them.
fn refreshed(body: TokenBody, old: &Tokens, now: i64) -> Option<Tokens> {
    let expires_in = body.expires_in.filter(|e| *e > 0)?;
    Some(Tokens {
        access_token: nonempty(body.access_token)?,
        refresh_token: nonempty(body.refresh_token).unwrap_or_else(|| old.refresh_token.clone()),
        id_token: nonempty(body.id_token).unwrap_or_else(|| old.id_token.clone()),
        refresh_at: now + expires_in - REFRESH_MARGIN,
        scopes: nonempty(body.scope).map_or_else(|| old.scopes.clone(), |s| split_scopes(&s)),
    })
}

fn classify_refresh(status: u16, body: &str) -> RefreshError {
    let code = error_code(body);
    match code.as_deref() {
        Some("invalid_client") => RefreshError::SignInAgain(format!(
            "ChatGPT rejected mu's client registration ({}). {SIGN_IN_AGAIN}",
            describe(status, code.as_deref())
        )),
        Some(c) if UNUSABLE_REFRESH.contains(&c) && matches!(status, 400 | 401 | 403) => RefreshError::SignInAgain(
            format!("The ChatGPT sign-in is no longer valid ({}). {SIGN_IN_AGAIN}", describe(status, Some(c))),
        ),
        _ => RefreshError::Transient(format!(
            "ChatGPT refresh failed ({}); mu will try again",
            describe(status, code.as_deref())
        )),
    }
}

/// Revokes the account's refresh token at the issuer, retrying network failures and 5xx a few times.
///
/// `Err` means the revocation is not confirmed. The caller still drops the local tokens and tells the
/// user to disconnect mu in ChatGPT Settings if it still appears there.
pub async fn revoke(ep: &Endpoints, account: &Account) -> Result<(), String> {
    let Some(tokens) = account.tokens.as_ref() else {
        return Ok(());
    };
    let client = crate::net::http_client().map_err(|_| "could not start the HTTP client".to_string())?;
    let mut last = String::new();
    for attempt in 0..=REVOKE_BACKOFF.len() {
        if attempt > 0 {
            tokio::time::sleep(REVOKE_BACKOFF[attempt - 1]).await;
        }
        let sent = form_post(
            &client,
            &ep.revoke,
            &[
                ("token", tokens.refresh_token.as_str()),
                ("token_type_hint", "refresh_token"),
                ("client_id", account.client_id.as_str()),
            ],
        )
        .send()
        .await;
        match sent {
            Ok(r) if r.status().is_success() => return Ok(()),
            Ok(r) if r.status().is_client_error() && r.status().as_u16() != 429 => {
                return Err(format!(
                    "ChatGPT did not confirm the revocation (HTTP {}); disconnect mu in ChatGPT Settings.",
                    r.status().as_u16()
                ));
            }
            Ok(r) => last = format!("HTTP {}", r.status().as_u16()),
            Err(e) => last = e.without_url().to_string(),
        }
    }
    Err(format!(
        "ChatGPT did not confirm the revocation ({last}); disconnect mu in ChatGPT Settings if it still appears there."
    ))
}

/// The authorization URL with `id_token_hint` and `login_hint` values replaced by `[REDACTED]`, for
/// logs and displays. The URL given to the browser stays complete.
pub fn redact_url(url: &str) -> String {
    let (rest, fragment) = match url.split_once('#') {
        Some((r, f)) => (r, Some(f)),
        None => (url, None),
    };
    let Some((base, query)) = rest.split_once('?') else {
        return url.to_string();
    };
    let query = query
        .split('&')
        .map(|kv| match kv.split_once('=') {
            Some((k, _)) if matches!(k, "id_token_hint" | "login_hint") => format!("{k}=[REDACTED]"),
            _ => kv.to_string(),
        })
        .collect::<Vec<_>>()
        .join("&");
    match fragment {
        Some(f) => format!("{base}?{query}#{f}"),
        None => format!("{base}?{query}"),
    }
}

fn form_post(client: &reqwest::Client, url: &str, pairs: &[(&str, &str)]) -> reqwest::RequestBuilder {
    client
        .post(url)
        .timeout(HTTP_TIMEOUT)
        .header(reqwest::header::ACCEPT, "application/json")
        .header(reqwest::header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(form_body(pairs))
}

/// `application/x-www-form-urlencoded` for `pairs`. The `url` crate encodes it; reqwest's `form` feature is off.
fn form_body(pairs: &[(&str, &str)]) -> String {
    Url::parse_with_params("http://localhost/", pairs)
        .ok()
        .and_then(|u| u.query().map(str::to_string))
        .unwrap_or_default()
}

/// `status` and the OAuth error code, the only parts of an error body that reach messages.
fn describe(status: u16, code: Option<&str>) -> String {
    match code {
        Some(c) => format!("HTTP {status}, {c}"),
        None => format!("HTTP {status}"),
    }
}

/// The `error` string of an OAuth error body, or its `error.code`. Only short identifiers pass, so
/// nothing else from a body can reach a message.
fn error_code(body: &str) -> Option<String> {
    let v: Value = serde_json::from_str(body).ok()?;
    let code = match &v["error"] {
        Value::String(s) => s.as_str(),
        Value::Object(o) => o.get("code")?.as_str()?,
        _ => return None,
    };
    let ok = !code.is_empty() && code.len() <= 64 && code.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    ok.then(|| code.to_string())
}

fn nonempty(v: Option<String>) -> Option<String> {
    v.filter(|s| !s.trim().is_empty())
}

fn split_scopes(s: &str) -> Vec<String> {
    s.split_whitespace().map(str::to_string).collect()
}

fn unix_now() -> i64 {
    jiff::Timestamp::now().as_second()
}
