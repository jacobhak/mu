//! Protocol tests against a fake issuer on 127.0.0.1. The test plays the browser: it reads the
//! authorization URL, then sends the callback over TCP or pastes it.

use super::*;
use crate::auth::chatgpt::DIRECT_SCOPE;
use base64::engine::general_purpose::STANDARD;
use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use serde_json::json;
use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::sync::Mutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc::UnboundedSender;

/// A 2048-bit RSA key (PKCS#8 DER, base64). Test-only; it signs nothing real.
const KEY_B64: &str = "MIIEvgIBADANBgkqhkiG9w0BAQEFAASCBKgwggSkAgEAAoIBAQCiK8OIKmrEG7tH5vkLo8hc5qbsPz6tRiWPjYLbFUEdKo9ntHutA/bMvxyerRyc7tWyFp7nmv87tCldUouvwhsEZV4lsQAMswMr6MQYPV5bMVbfSmwlteC7Y4j5qoh/6jQ8ReSue4UaVgksnedTwS4pzoOUgRnh80JK1Z7ECSNKn2mrh686ElVZ70c+ULqKb+Hegn2su9U1BIYLmSjdzMgrdNqmebbhDcKCivYwIw4XbEu6Kg8mii25y2eyvwI3U71WE+NYL442OSOKyn+DsiBIwy6xtHQ1hBwQMdpbEFvyJkhMpKCKZdWrhj0PMkhJG/SBeJfJPC56Ryv/I//bCNcrAgMBAAECggEABV+U2YCqS/gtQuskyl1xaJ6d3ksULRKbcP+7271/DFPaL5H73RY6Cbu1W46Z2ZtSSlXUixEILva/vh9bkIXKdmFTdsVAHquZyc0rHjB+XWArpDdnkPJPUXYxVjBv2GkutOaN9rgSB3bdU/ShcxV+DZWRp4yFioaQZkFgwtcYq8LB8CNTR+y5R0fxRHVbEfkQEl4vYeCOWzOgHEGVS9QmYutmijqZX6YCseBu0X1YeEls3w9RU2yjgassVjj6+esY8U8cI0ARmX5KM8fAcvaxGBiGjzET59Ba433kMBCCcWUe3fX1FJSZ80gTiLn88WJyhYQEqKTSmBFnybCuKojrLQKBgQDRYp2AIHElaM8fO3oT1AELAzRp4GuTThFU8YRUkaplYFP1IbN00KUmzmE9Cy4wzG0+wFwYRiK3s0JAh70Twv4rAR6uYvcDNxybUD59B+25pvNeSgh41xqBblFDU6fmSgQhkB40Ia2xdPeMClDGfIXbQnLv/r2fpgoRCJeg3CA/BwKBgQDGRkpGS45NAF3OuG9RBrNyTc5BD5PXHyCLAfMqTyWaQL/vpFpRH/vfU9TChpwY7hxT8qZd02GiMshXg/pN1w8l8TdKfU0xCdpBIdqswFzx4t+X2cYkaFf0meL8TWl19nbiSoubFuGxwUC6eTyOyDy/mDOr1tDvgzBUxvutTxB5vQKBgQCScGDs+kOHwVySYFuJwI0WMaCXp+A0vrW6bct6BgJhMlSlrXM/8KiAknhJZCdi86Y36/q881hWl+VvEj/3sTlnxDm8BvTw57t3GFELyt7FJ/EvcaGpOe+MtCXba4eStRNckdm95gv/f2meJJzHpNFNPpT6KnReUpbZwOOjGjDnQwKBgGIWAcZ2uEXliYKrgP4fh4Rjc0aCU0vXrTy3UTk8AEkoAQunTbrWTL71dMpq1ywKF198Kcs+uA0KfEu3EYuSeX72rrid9iLtUdhozAor+476AkzXwgrex6xMuJrCHQrrS3K1GA04K5zJEqFtOoUUD+FkhSNcWO40AyPdcEITW/4tAoGBAKdTuSnWpwLzLbttCqMvBbhoaaJuDhh/b6XPH46RDGRwhInjNTi45sT5+Fx2P4tZkPgzO+jg+APqRzcYCxq43IHw8OZoVMZntr+5cHXm9X0Sd4xXHWA5Jcjtqn8l7C2CBRzGun0XJipaliXsf7M04yfCiiUBm7iKzUkjkpXayXp8";
const KID: &str = "test-kid";
const CLIENT: &str = "oaiapp_test";
const HOST: &str = "urn:uuid:6f1c2b7e-0d3a-4f5b-9a8e-1c2d3e4f5a6b";
const SCOPE_ALL: &str = "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
const SCOPE_NO_DIRECT: &str = "openid profile email offline_access resource.invoke";

fn test_key() -> RsaKeyPair {
    RsaKeyPair::from_pkcs8(&STANDARD.decode(KEY_B64).unwrap()).unwrap()
}

fn now() -> i64 {
    jiff::Timestamp::now().as_second()
}

/// `(n, e)` from a PKCS#1 `RSAPublicKey`, `SEQUENCE { INTEGER n, INTEGER e }`.
fn rsa_components(der: &[u8]) -> (Vec<u8>, Vec<u8>) {
    fn read(d: &[u8]) -> (&[u8], &[u8]) {
        let first = d[1] as usize;
        let (len, start) = if first < 0x80 {
            (first, 2)
        } else {
            let n = first & 0x7f;
            (d[2..2 + n].iter().fold(0usize, |acc, b| (acc << 8) | *b as usize), 2 + n)
        };
        (&d[start..start + len], &d[start + len..])
    }
    let (seq, _) = read(der);
    let (n, rest) = read(seq);
    let (e, _) = read(rest);
    (n.iter().skip_while(|b| **b == 0).copied().collect(), e.to_vec())
}

fn jwks() -> String {
    let (n, e) = rsa_components(test_key().public().as_ref());
    json!({"keys": [{
        "kty": "RSA", "kid": KID, "alg": "RS256", "use": "sig",
        "n": URL_SAFE_NO_PAD.encode(n), "e": URL_SAFE_NO_PAD.encode(e),
    }]})
    .to_string()
}

/// A compact JWS signed with `test_key`.
fn sign_jwt(header: Value, claims: &Value) -> String {
    let input =
        format!("{}.{}", URL_SAFE_NO_PAD.encode(header.to_string()), URL_SAFE_NO_PAD.encode(claims.to_string()));
    let key = test_key();
    let mut sig = vec![0u8; key.public().modulus_len()];
    key.sign(&RSA_PKCS1_SHA256, &SystemRandom::new(), input.as_bytes(), &mut sig).unwrap();
    format!("{input}.{}", URL_SAFE_NO_PAD.encode(sig))
}

fn id_token(claims: &Value) -> String {
    sign_jwt(json!({"alg": "RS256", "kid": KID, "typ": "JWT"}), claims)
}

fn good_claims(issuer: &str, client: &str, nonce: &str) -> Value {
    json!({
        "iss": issuer, "aud": client, "sub": "user-123", "email": "Jane@Example.com",
        "nonce": nonce, "exp": now() + 3600, "iat": now(),
    })
}

fn token_body(id: &str, scope: &str) -> String {
    json!({
        "access_token": "acc-1", "refresh_token": "ref-1", "id_token": id,
        "token_type": "Bearer", "scope": scope, "expires_in": 3600,
    })
    .to_string()
}

/// The fake issuer: scripted token and revoke replies, a log of every POST, and the JWKS.
#[derive(Default)]
struct Scripts {
    token: VecDeque<(u16, String)>,
    revoke: VecDeque<(u16, String)>,
    /// (path, form body) of every POST, in order.
    posts: Vec<(String, String)>,
}

#[derive(Clone)]
struct Issuer {
    ep: Endpoints,
    scripts: Arc<Mutex<Scripts>>,
}

impl Issuer {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let scripts = Arc::new(Mutex::new(Scripts::default()));
        let (st, keys) = (scripts.clone(), jwks());
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(serve_fake(stream, st.clone(), keys.clone()));
            }
        });
        Self { ep: Endpoints::at(&base), scripts }
    }

    fn token_reply(&self, status: u16, body: &str) {
        self.scripts.lock().unwrap().token.push_back((status, body.to_string()));
    }

    fn revoke_reply(&self, status: u16, body: &str) {
        self.scripts.lock().unwrap().revoke.push_back((status, body.to_string()));
    }

    /// The form bodies posted to `path`, decoded.
    fn posted(&self, path: &str) -> Vec<BTreeMap<String, String>> {
        self.scripts
            .lock()
            .unwrap()
            .posts
            .iter()
            .filter(|(p, _)| p == path)
            .map(|(_, body)| {
                let u = Url::parse(&format!("http://x/?{body}")).unwrap();
                u.query_pairs().into_owned().collect()
            })
            .collect()
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

async fn serve_fake(mut stream: TcpStream, scripts: Arc<Mutex<Scripts>>, keys: String) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    let head_end = loop {
        if let Some(p) = find(&buf, b"\r\n\r\n") {
            break p + 4;
        }
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let len = head
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case("content-length").then(|| v.trim().parse::<usize>().ok()).flatten()
        })
        .unwrap_or(0);
    while buf.len() < head_end + len {
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
    let body = String::from_utf8_lossy(&buf[head_end..head_end + len]).into_owned();
    let mut words = head.split_whitespace();
    let (method, path) = (words.next().unwrap_or_default(), words.next().unwrap_or_default());
    let (status, reply) = match (method, path) {
        ("GET", "/.well-known/jwks.json") => (200, keys),
        ("POST", "/api/accounts/oauth/token") => {
            let mut s = scripts.lock().unwrap();
            s.posts.push((path.to_string(), body));
            s.token.pop_front().unwrap_or((500, "{}".to_string()))
        }
        ("POST", "/api/accounts/oauth/revoke") => {
            let mut s = scripts.lock().unwrap();
            s.posts.push((path.to_string(), body));
            s.revoke.pop_front().unwrap_or((200, String::new()))
        }
        _ => (404, String::new()),
    };
    let resp = format!(
        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
        reply.len()
    );
    let _ = stream.write_all(resp.as_bytes()).await;
    let _ = stream.shutdown().await;
}

/// What the test's browser learns from the authorization URL.
struct Authz {
    url: String,
    state: String,
    nonce: String,
    port: u16,
}

fn param(u: &Url, name: &str) -> Option<String> {
    u.query_pairs().find(|(k, _)| k == name).map(|(_, v)| v.into_owned())
}

async fn authz(rx: &mut UnboundedReceiver<String>) -> Authz {
    let url = rx.recv().await.expect("the authorization URL");
    let u = Url::parse(&url).unwrap();
    let redirect = Url::parse(&param(&u, "redirect_uri").unwrap()).unwrap();
    Authz {
        state: param(&u, "state").unwrap(),
        nonce: param(&u, "nonce").unwrap(),
        port: redirect.port().unwrap(),
        url,
    }
}

/// The browser's plain GET of the callback; returns the whole response.
async fn get(port: u16, path: &str) -> String {
    let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    s.write_all(format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").as_bytes()).await.unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).await.unwrap();
    out
}

fn request(ep: &Endpoints, known: Option<Known>) -> LoginRequest<'_> {
    LoginRequest { endpoints: ep, host_id: HOST, known, cancel: CancellationToken::new() }
}

fn saved() -> Known {
    Known {
        client_id: "oaiapp_saved".into(),
        id_token: Some("old-id-token".into()),
        email: Some("jane@example.com".into()),
    }
}

/// What one login attempt produced: the result, and the notices shown while it waited.
struct Attempt {
    result: Result<Account>,
    notes: Vec<String>,
}

/// Runs `sign_in` with the browser step `browser`, which receives the authorization details.
async fn attempt<F, Fut>(issuer: &Issuer, known: Option<Known>, browser: F) -> Attempt
where
    F: FnOnce(UnboundedReceiver<String>, UnboundedSender<String>, Issuer) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let (url_tx, url_rx) = mpsc::unbounded_channel::<String>();
    let (paste_tx, mut paste_rx) = mpsc::unbounded_channel::<String>();
    let notes = RefCell::new(Vec::new());
    let mut on_url = |u: &str| {
        let _ = url_tx.send(u.to_string());
    };
    let mut notice = |m: &str| notes.borrow_mut().push(m.to_string());
    let fut = browser(url_rx, paste_tx, issuer.clone());
    let (result, ()) =
        tokio::join!(sign_in(request(&issuer.ep, known), Some(0), &mut on_url, &mut notice, &mut paste_rx), fut);
    Attempt { result, notes: notes.into_inner() }
}

#[test]
fn pkce_challenge_matches_the_rfc_7636_vector() {
    assert_eq!(
        pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
        "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
    );
}

#[test]
fn random_tokens_are_43_url_safe_characters() {
    let (a, b) = (random_token().unwrap(), random_token().unwrap());
    assert_eq!(a.len(), 43);
    assert!(a.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    assert_ne!(a, b);
}

fn query_of(u: &Url) -> BTreeMap<String, String> {
    u.query_pairs().into_owned().collect()
}

#[test]
fn first_registration_url_names_the_agent_and_no_hints() {
    let ep = Endpoints::at("http://127.0.0.1:9");
    let u = authorize_url(&ep, HOST, None, "http://127.0.0.1:1455/auth/callback", "chal", "st", "no").unwrap();
    let q = query_of(&u);
    assert_eq!(u.as_str().split('?').next().unwrap(), "http://127.0.0.1:9/api/accounts/authorize");
    assert_eq!(q["client_id"], "dynamic_agent_client");
    assert_eq!(q["agent_name_hint"], "mu");
    assert_eq!(q["ext_agent_host_id"], HOST);
    assert_eq!(q["redirect_uri"], "http://127.0.0.1:1455/auth/callback");
    assert_eq!(q["resource"], "https://api.openai.com/v1");
    assert_eq!(q["scope"], SCOPE);
    assert_eq!(q["code_challenge_method"], "S256");
    assert_eq!(q["code_challenge"], "chal");
    assert_eq!(q["state"], "st");
    assert_eq!(q["nonce"], "no");
    assert_eq!(q["response_type"], "code");
    assert!(!q.contains_key("id_token_hint") && !q.contains_key("login_hint"));
}

#[test]
fn reauthorization_url_reuses_the_registration_and_its_hints() {
    let ep = Endpoints::at("http://127.0.0.1:9");
    let known = saved();
    let u = authorize_url(&ep, HOST, Some(&known), "http://127.0.0.1:2/auth/callback", "c", "s", "n").unwrap();
    let q = query_of(&u);
    assert_eq!(q["client_id"], "oaiapp_saved");
    assert_eq!(q["id_token_hint"], "old-id-token");
    assert_eq!(q["login_hint"], "jane@example.com");
    assert!(!q.contains_key("agent_name_hint"));
    assert_eq!(q["ext_agent_host_id"], HOST);
}

#[test]
fn redact_url_hides_the_hints_and_keeps_the_rest() {
    let url = "https://auth.openai.com/api/accounts/authorize?client_id=oaiapp_x&id_token_hint=eyJhbGci.abc.def\
               &login_hint=jane%40example.com&state=st&code_challenge=c#frag";
    let red = redact_url(url);
    assert_eq!(
        red,
        "https://auth.openai.com/api/accounts/authorize?client_id=oaiapp_x&id_token_hint=[REDACTED]\
         &login_hint=[REDACTED]&state=st&code_challenge=c#frag"
    );
    assert_eq!(redact_url("not a url"), "not a url");
}

#[test]
fn paste_must_be_this_listeners_callback() {
    let expect = Expect { state: "st".into(), known: None };
    let ok = "http://127.0.0.1:4242/auth/callback?code=c1&state=st&client_id=oaiapp_a";
    assert!(
        matches!(callback::read_paste(ok, 4242, &expect), callback::Step::Done(i) if i.code == "c1" && i.client_id == "oaiapp_a")
    );
    for bad in [
        "http://localhost:4242/auth/callback?code=c1&state=st&client_id=oaiapp_a",
        "http://127.0.0.1:4243/auth/callback?code=c1&state=st&client_id=oaiapp_a",
        "https://127.0.0.1:4242/auth/callback?code=c1&state=st&client_id=oaiapp_a",
        "http://127.0.0.1:4242/other?code=c1&state=st&client_id=oaiapp_a",
        "   ",
        "not a url",
    ] {
        assert!(matches!(callback::read_paste(bad, 4242, &expect), callback::Step::Retry(_)), "{bad}");
    }
    let err = "http://127.0.0.1:4242/auth/callback?error=access_denied&state=st";
    assert!(matches!(callback::read_paste(err, 4242, &expect), callback::Step::Fail(_)));
    let stateless = "http://127.0.0.1:4242/auth/callback?error=access_denied";
    assert!(matches!(callback::read_paste(stateless, 4242, &expect), callback::Step::Retry(_)));
}

#[test]
fn an_error_callback_needs_this_sign_ins_state_and_is_not_echoed() {
    let expect = Expect { state: "st".into(), known: None };
    let q = |pairs: &[(&str, &str)]| -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    };
    for params in [q(&[("error", "<b>x</b>")]), q(&[("error", "x"), ("state", "other")])] {
        assert!(matches!(callback::read_callback(&params, &expect), callback::Step::Retry(_)));
    }
    let callback::Step::Fail(msg) = callback::read_callback(&q(&[("error", "<b>evil</b>"), ("state", "st")]), &expect)
    else {
        panic!("an error with the right state ends the attempt");
    };
    assert!(!msg.contains("evil"), "{msg}");
}

#[tokio::test]
async fn loopback_login_registers_and_returns_a_verified_account() {
    let issuer = Issuer::start().await;
    let out = attempt(&issuer, None, |mut rx, _ptx, iss| async move {
        let a = authz(&mut rx).await;
        iss.token_reply(200, &token_body(&id_token(&good_claims(&iss.ep.issuer, CLIENT, &a.nonce)), SCOPE_ALL));
        let resp = get(a.port, &format!("/auth/callback?code=abc&state={}&client_id={CLIENT}", a.state)).await;
        assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
    })
    .await;
    let account = out.result.unwrap();
    assert_eq!(account.client_id, CLIENT);
    assert_eq!(account.subject, "user-123");
    assert_eq!(account.email.as_deref(), Some("jane@example.com"));
    assert!(account.plan_usage());
    let tokens = account.tokens.unwrap();
    assert_eq!(tokens.access_token, "acc-1");
    assert_eq!(tokens.refresh_token, "ref-1");
    assert!((tokens.refresh_at - (now() + 3600 - 180)).abs() <= 5);
    assert!(out.notes.is_empty());

    let posts = issuer.posted("/api/accounts/oauth/token");
    assert_eq!(posts.len(), 1);
    let form = &posts[0];
    assert_eq!(form["grant_type"], "authorization_code");
    assert_eq!(form["code"], "abc");
    assert_eq!(form["client_id"], CLIENT);
    assert_eq!(form["resource"], "https://api.openai.com/v1");
    assert!(form["redirect_uri"].starts_with("http://127.0.0.1:") && form["redirect_uri"].ends_with("/auth/callback"));
    assert_eq!(form["code_verifier"].len(), 43);
}

#[tokio::test]
async fn pasted_redirect_completes_the_login() {
    let issuer = Issuer::start().await;
    let out = attempt(&issuer, None, |mut rx, ptx, iss| async move {
        let a = authz(&mut rx).await;
        iss.token_reply(200, &token_body(&id_token(&good_claims(&iss.ep.issuer, CLIENT, &a.nonce)), SCOPE_ALL));
        let _ = ptx.send(format!("http://localhost:{}/auth/callback?code=x&state={}", a.port, a.state));
        let _ = ptx.send("nonsense".into());
        let _ = ptx
            .send(format!("http://127.0.0.1:{}/auth/callback?code=abc&state={}&client_id={CLIENT}", a.port, a.state));
    })
    .await;
    assert_eq!(out.result.unwrap().client_id, CLIENT);
    assert_eq!(out.notes.len(), 2, "{:?}", out.notes);
    assert!(out.notes[0].contains("starting with http://127.0.0.1:"));
}

#[tokio::test]
async fn stray_callbacks_are_reported_and_the_wait_goes_on() {
    let issuer = Issuer::start().await;
    let out = attempt(&issuer, None, |mut rx, _ptx, iss| async move {
        let a = authz(&mut rx).await;
        iss.token_reply(200, &token_body(&id_token(&good_claims(&iss.ep.issuer, CLIENT, &a.nonce)), SCOPE_ALL));
        let wrong = get(a.port, &format!("/auth/callback?code=abc&state=other&client_id={CLIENT}")).await;
        assert!(wrong.starts_with("HTTP/1.1 400"), "{wrong}");
        let missing = get(a.port, &format!("/auth/callback?state={}&client_id={CLIENT}", a.state)).await;
        assert!(missing.starts_with("HTTP/1.1 400"), "{missing}");
        let other = get(a.port, "/favicon.ico").await;
        assert!(other.starts_with("HTTP/1.1 404"), "{other}");
        let ok = get(a.port, &format!("/auth/callback?code=abc&state={}&client_id={CLIENT}", a.state)).await;
        assert!(ok.starts_with("HTTP/1.1 200"), "{ok}");
    })
    .await;
    assert_eq!(out.result.unwrap().client_id, CLIENT);
    assert_eq!(out.notes.len(), 2, "{:?}", out.notes);
}

#[tokio::test]
async fn idle_connections_do_not_stall_the_callback() {
    let issuer = Issuer::start().await;
    let out = attempt(&issuer, None, |mut rx, _ptx, iss| async move {
        let a = authz(&mut rx).await;
        iss.token_reply(200, &token_body(&id_token(&good_claims(&iss.ep.issuer, CLIENT, &a.nonce)), SCOPE_ALL));
        let _idle = TcpStream::connect(("127.0.0.1", a.port)).await.unwrap();
        let ok = get(a.port, &format!("/auth/callback?code=abc&state={}&client_id={CLIENT}", a.state)).await;
        assert!(ok.starts_with("HTTP/1.1 200"), "{ok}");
    })
    .await;
    assert!(out.result.is_ok());
}

#[tokio::test]
async fn access_denied_ends_the_attempt() {
    let issuer = Issuer::start().await;
    let out = attempt(&issuer, None, |mut rx, _ptx, _iss| async move {
        let a = authz(&mut rx).await;
        let stray = get(a.port, "/auth/callback?error=access_denied").await;
        assert!(stray.starts_with("HTTP/1.1 400"), "{stray}");
        let resp = get(a.port, &format!("/auth/callback?error=access_denied&state={}", a.state)).await;
        assert!(resp.starts_with("HTTP/1.1 400"), "{resp}");
    })
    .await;
    let msg = out.result.unwrap_err().to_string();
    assert!(msg.contains("not approved"), "{msg}");
}

#[tokio::test]
async fn pasted_error_ends_the_attempt() {
    let issuer = Issuer::start().await;
    let out = attempt(&issuer, None, |mut rx, ptx, _iss| async move {
        let a = authz(&mut rx).await;
        let _ = ptx.send(format!("http://127.0.0.1:{}/auth/callback?error=access_denied&state={}", a.port, a.state));
    })
    .await;
    assert!(out.result.unwrap_err().to_string().contains("not approved"));
}

#[tokio::test]
async fn first_registration_without_an_issued_client_id_fails() {
    let issuer = Issuer::start().await;
    let out = attempt(&issuer, None, |mut rx, _ptx, _iss| async move {
        let a = authz(&mut rx).await;
        let _ = get(a.port, &format!("/auth/callback?code=abc&state={}", a.state)).await;
    })
    .await;
    assert!(out.result.unwrap_err().to_string().contains("client id"));
}

#[tokio::test]
async fn reauthorization_with_another_client_id_fails() {
    let issuer = Issuer::start().await;
    let out = attempt(&issuer, Some(saved()), |mut rx, _ptx, _iss| async move {
        let a = authz(&mut rx).await;
        assert!(a.url.contains("id_token_hint=old-id-token"));
        let _ = get(a.port, &format!("/auth/callback?code=abc&state={}&client_id=oaiapp_other", a.state)).await;
    })
    .await;
    assert!(out.result.unwrap_err().to_string().contains("different client id"));
}

#[tokio::test]
async fn reauthorization_reuses_the_saved_client_when_the_callback_omits_it() {
    let issuer = Issuer::start().await;
    let out = attempt(&issuer, Some(saved()), |mut rx, _ptx, iss| async move {
        let a = authz(&mut rx).await;
        iss.token_reply(200, &token_body(&id_token(&good_claims(&iss.ep.issuer, "oaiapp_saved", &a.nonce)), SCOPE_ALL));
        let _ = get(a.port, &format!("/auth/callback?code=abc&state={}", a.state)).await;
    })
    .await;
    assert_eq!(out.result.unwrap().client_id, "oaiapp_saved");
}

#[tokio::test]
async fn grant_without_direct_scope_is_an_account_without_plan_usage() {
    let issuer = Issuer::start().await;
    let out = attempt(&issuer, None, |mut rx, _ptx, iss| async move {
        let a = authz(&mut rx).await;
        iss.token_reply(200, &token_body(&id_token(&good_claims(&iss.ep.issuer, CLIENT, &a.nonce)), SCOPE_NO_DIRECT));
        let _ = get(a.port, &format!("/auth/callback?code=abc&state={}&client_id={CLIENT}", a.state)).await;
    })
    .await;
    let account = out.result.unwrap();
    assert!(account.signed_in());
    assert!(!account.plan_usage());
}

#[tokio::test]
async fn id_token_with_a_bad_signature_is_rejected() {
    let issuer = Issuer::start().await;
    let out = attempt(&issuer, None, |mut rx, _ptx, iss| async move {
        let a = authz(&mut rx).await;
        let genuine = id_token(&good_claims(&iss.ep.issuer, CLIENT, &a.nonce));
        let mut parts: Vec<String> = genuine.split('.').map(str::to_string).collect();
        let mut forged = good_claims(&iss.ep.issuer, CLIENT, &a.nonce);
        forged["sub"] = json!("mallory");
        parts[1] = URL_SAFE_NO_PAD.encode(forged.to_string());
        iss.token_reply(200, &token_body(&parts.join("."), SCOPE_ALL));
        let _ = get(a.port, &format!("/auth/callback?code=abc&state={}&client_id={CLIENT}", a.state)).await;
    })
    .await;
    assert!(out.result.unwrap_err().to_string().contains("signature"));
}

#[tokio::test]
async fn id_token_with_the_wrong_issuer_audience_expiry_or_nonce_is_rejected() {
    type Claims = fn(&str, &str) -> Value;
    let cases: Vec<(&str, Claims, &str)> = vec![
        (
            "issuer",
            |n, iss| {
                let mut c = good_claims(iss, CLIENT, n);
                c["iss"] = json!("https://evil.example.com");
                c
            },
            "another issuer",
        ),
        ("audience", |n, iss| good_claims(iss, "oaiapp_other", n), "another client"),
        (
            "expiry",
            |n, iss| {
                let mut c = good_claims(iss, CLIENT, n);
                c["exp"] = json!(now() - 3600);
                c
            },
            "expired",
        ),
        ("nonce", |_n, iss| good_claims(iss, CLIENT, "stale-nonce"), "nonce"),
    ];
    for (what, make, expect) in cases {
        let issuer = Issuer::start().await;
        let out = attempt(&issuer, None, |mut rx, _ptx, iss| async move {
            let a = authz(&mut rx).await;
            iss.token_reply(200, &token_body(&id_token(&make(&a.nonce, &iss.ep.issuer)), SCOPE_ALL));
            let _ = get(a.port, &format!("/auth/callback?code=abc&state={}&client_id={CLIENT}", a.state)).await;
        })
        .await;
        let msg = out.result.unwrap_err().to_string();
        assert!(msg.contains(expect), "{what}: {msg}");
    }
}

#[tokio::test]
async fn id_token_audience_may_be_a_list_that_includes_the_client() {
    let issuer = Issuer::start().await;
    let out = attempt(&issuer, None, |mut rx, _ptx, iss| async move {
        let a = authz(&mut rx).await;
        let mut c = good_claims(&iss.ep.issuer, CLIENT, &a.nonce);
        c["aud"] = json!(["oaiapp_other", CLIENT]);
        iss.token_reply(200, &token_body(&id_token(&c), SCOPE_ALL));
        let _ = get(a.port, &format!("/auth/callback?code=abc&state={}&client_id={CLIENT}", a.state)).await;
    })
    .await;
    assert_eq!(out.result.unwrap().subject, "user-123");
}

fn account_with_tokens() -> Account {
    Account {
        client_id: CLIENT.into(),
        subject: "user-123".into(),
        email: None,
        tokens: Some(Tokens {
            id_token: "old-id".into(),
            access_token: "old-access".into(),
            refresh_token: "old-refresh".into(),
            refresh_at: 0,
            scopes: vec![DIRECT_SCOPE.into()],
        }),
        added: "2026-01-01T00:00:00Z".into(),
    }
}

#[tokio::test]
async fn refresh_rotates_the_refresh_token_and_keeps_the_id_token() {
    let issuer = Issuer::start().await;
    issuer.token_reply(
        200,
        &json!({"access_token": "new-access", "refresh_token": "new-refresh", "expires_in": 3600}).to_string(),
    );
    let tokens = refresh(&issuer.ep, &account_with_tokens()).await.unwrap();
    assert_eq!(tokens.access_token, "new-access");
    assert_eq!(tokens.refresh_token, "new-refresh");
    assert_eq!(tokens.id_token, "old-id");
    assert_eq!(tokens.scopes, vec![DIRECT_SCOPE.to_string()]);
    assert!(tokens.refresh_at > now());

    let form = &issuer.posted("/api/accounts/oauth/token")[0];
    assert_eq!(form["grant_type"], "refresh_token");
    assert_eq!(form["refresh_token"], "old-refresh");
    assert_eq!(form["client_id"], CLIENT);
    assert!(!form.contains_key("scope"));
}

#[tokio::test]
async fn refresh_errors_are_classified() {
    let cases = [
        (400, r#"{"error":"invalid_grant"}"#, "sign_in"),
        (401, r#"{"error":{"code":"refresh_token_reused"}}"#, "sign_in"),
        (403, r#"{"error":"refresh_token_invalidated"}"#, "sign_in"),
        (401, r#"{"error":"invalid_client"}"#, "sign_in"),
        (500, "oops", "transient"),
        (429, r#"{"error":"slow_down"}"#, "transient"),
        (400, "not json", "transient"),
    ];
    for (status, body, want) in cases {
        let issuer = Issuer::start().await;
        issuer.token_reply(status, body);
        let err = refresh(&issuer.ep, &account_with_tokens()).await.unwrap_err();
        match (want, &err) {
            ("sign_in", RefreshError::SignInAgain(m)) => assert!(m.contains("/login"), "{m}"),
            ("transient", RefreshError::Transient(_)) => {}
            _ => panic!("{status} {body}: {err:?}"),
        }
    }
}

#[tokio::test]
async fn refresh_messages_never_carry_a_token() {
    let issuer = Issuer::start().await;
    issuer.token_reply(400, r#"{"error":"invalid_request","error_description":"old-refresh was echoed"}"#);
    let err = refresh(&issuer.ep, &account_with_tokens()).await.unwrap_err();
    let text = err.to_string();
    assert!(!text.contains("old-refresh") && !text.contains("old-access"), "{text}");
    assert!(matches!(err, RefreshError::Transient(_)));
}

#[tokio::test]
async fn refresh_without_tokens_asks_for_sign_in() {
    let issuer = Issuer::start().await;
    let mut account = account_with_tokens();
    account.tokens = None;
    assert!(matches!(refresh(&issuer.ep, &account).await, Err(RefreshError::SignInAgain(_))));
}

#[tokio::test]
async fn revoke_posts_the_refresh_token_and_succeeds_on_2xx() {
    let issuer = Issuer::start().await;
    issuer.revoke_reply(200, "");
    revoke(&issuer.ep, &account_with_tokens()).await.unwrap();
    let form = &issuer.posted("/api/accounts/oauth/revoke")[0];
    assert_eq!(form["token"], "old-refresh");
    assert_eq!(form["token_type_hint"], "refresh_token");
    assert_eq!(form["client_id"], CLIENT);
}

#[tokio::test]
async fn revoke_retries_network_and_server_failures() {
    let issuer = Issuer::start().await;
    issuer.revoke_reply(500, "");
    issuer.revoke_reply(503, "");
    issuer.revoke_reply(200, "");
    revoke(&issuer.ep, &account_with_tokens()).await.unwrap();
    assert_eq!(issuer.posted("/api/accounts/oauth/revoke").len(), 3);
}

#[tokio::test]
async fn revoke_reports_an_unconfirmed_revocation_after_its_retries() {
    let issuer = Issuer::start().await;
    for _ in 0..3 {
        issuer.revoke_reply(500, "");
    }
    let err = revoke(&issuer.ep, &account_with_tokens()).await.unwrap_err();
    assert!(err.contains("did not confirm") && err.contains("ChatGPT Settings"), "{err}");
    assert!(!err.contains("old-refresh"));
    assert_eq!(issuer.posted("/api/accounts/oauth/revoke").len(), 3);
}

#[tokio::test]
async fn revoke_stops_at_a_client_error() {
    let issuer = Issuer::start().await;
    issuer.revoke_reply(400, "");
    let err = revoke(&issuer.ep, &account_with_tokens()).await.unwrap_err();
    assert!(err.contains("HTTP 400"), "{err}");
    assert_eq!(issuer.posted("/api/accounts/oauth/revoke").len(), 1);
}
