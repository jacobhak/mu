//! Network access for fetch: the internal-address guard, per-hop pinned connections, user-agent
//! rotation and capped body reads. A port of omp's `loadPage` (`web/scrapers/types.ts`).

use encoding_rs::{Encoding, UTF_8};
use reqwest::header::{
    ACCEPT, ACCEPT_ENCODING, ACCEPT_LANGUAGE, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue,
    LOCATION, RETRY_AFTER, USER_AGENT,
};
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, SystemTime};
use url::Url;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_REDIRECTS: usize = 10;
const DEFAULT_ACCEPT: &str = "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8";
const USER_AGENTS: [&str; 3] = [
    "curl/8.0",
    "Mozilla/5.0 (compatible; TextBot/1.0)",
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36",
];
/// Statuses that a site uses to refuse a user agent; the next agent may be served.
const BLOCKED_STATUSES: [u16; 4] = [403, 429, 503, 999];
const RETRY_AFTER_CAP: Duration = Duration::from_secs(10);
pub const MAX_BYTES: usize = 50 * 1024 * 1024;

/// Installs rustls' process-default crypto provider (ring). reqwest is built with
/// `rustls-no-provider`, so a client built before this call panics. Idempotent.
pub fn install_crypto() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// Decides which resolved addresses a fetch may connect to.
#[derive(Clone, Copy)]
pub struct Guard {
    /// Never reachable: loopback, link-local (cloud metadata), multicast, unspecified.
    pub blocked: fn(IpAddr) -> bool,
    /// LAN addresses (RFC 1918, unique-local, CGNAT): reachable only when the user's own URL is
    /// on the LAN, never through a redirect or a link from a public page. See `Reach`.
    pub lan: fn(IpAddr) -> bool,
}

impl Default for Guard {
    fn default() -> Self {
        Self { blocked: is_internal, lan: is_lan }
    }
}

/// How a request came about, which decides whether it may reach the LAN.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Reach {
    /// The URL the user or model asked for: it may be on the LAN.
    #[default]
    Explicit,
    /// A URL taken from a fetched page (an alternate link, `llms.txt`, `.md`): on the LAN only
    /// when the page it came from was.
    Derived { lan_ok: bool },
}

impl Guard {
    pub fn allow_all() -> Self {
        Self { blocked: |_| false, lan: |_| false }
    }

    /// The default guard, lifted by `MU_WEB_ALLOW_PRIVATE=1`.
    pub fn from_env() -> Self {
        if std::env::var("MU_WEB_ALLOW_PRIVATE").is_ok_and(|v| v == "1") { Self::allow_all() } else { Self::default() }
    }
}

#[derive(Clone)]
pub struct LoadOpts {
    /// Replaces the default `Accept` header.
    pub accept: Option<&'static str>,
    /// Extra headers. A `User-Agent` here disables rotation; an `Authorization` is dropped on a
    /// redirect to another host.
    pub headers: Vec<(String, String)>,
    /// Bounds each request attempt, body included.
    pub timeout: Duration,
    pub max_bytes: usize,
    pub reach: Reach,
}

impl Default for LoadOpts {
    fn default() -> Self {
        Self {
            accept: None,
            headers: Vec::new(),
            timeout: Duration::from_secs(20),
            max_bytes: MAX_BYTES,
            reach: Reach::Explicit,
        }
    }
}

pub struct Page {
    pub ok: bool,
    /// 2xx is `ok`. 0 when no response was received.
    pub status: u16,
    pub final_url: String,
    /// Lowercased media type without parameters; empty when absent.
    pub content_type: String,
    pub body: Vec<u8>,
    /// The body was cut at `max_bytes`.
    pub truncated: bool,
    pub error: Option<String>,
    /// Every hop of the chain, the last included, was on the LAN, so links taken from this page
    /// may be too.
    pub lan: bool,
    charset: Option<String>,
}

impl Page {
    fn failed(url: &str, error: Option<String>) -> Self {
        Self {
            ok: false,
            status: 0,
            final_url: url.to_string(),
            content_type: String::new(),
            body: Vec::new(),
            truncated: false,
            error,
            lan: false,
            charset: None,
        }
    }

    /// A NUL byte in the first 8 KiB: not text, so not worth decoding.
    pub fn looks_binary(&self) -> bool {
        self.body[..self.body.len().min(8 * 1024)].contains(&0)
    }

    /// Decodes the body with the header's charset, else a `<meta charset>` in the first 2 KiB
    /// when the body is HTML or untyped, else UTF-8. Invalid sequences become U+FFFD.
    pub fn text(&self) -> String {
        let label = self.charset.clone().or_else(|| {
            let html_or_untyped = self.content_type.is_empty() || self.content_type.contains("html");
            html_or_untyped.then(|| meta_charset(&self.body)).flatten()
        });
        let enc = label.and_then(|l| Encoding::for_label(l.as_bytes())).unwrap_or(UTF_8);
        enc.decode(&self.body).0.into_owned()
    }
}

/// Fetches `url`, following up to 10 redirects by hand. Each hop resolves its host, refuses any
/// blocked address and connects only to the addresses it checked. Blocked or refused statuses
/// rotate the user agent. Failures are reported in `Page::error`, never as a panic.
pub async fn load_page(g: &Guard, url: &str, opts: LoadOpts) -> Page {
    install_crypto();
    let own_ua = opts.headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("user-agent"));
    let mut attempt = 0;
    let mut retried_429 = false;
    loop {
        let ua = (!own_ua).then(|| USER_AGENTS[attempt]);
        let last_agent = own_ua || attempt == USER_AGENTS.len() - 1;
        let hop = match fetch_chain(g, url, ua, &opts).await {
            Ok(hop) => hop,
            Err(Failure::Refused(e)) => return Page::failed(url, Some(e)),
            Err(Failure::Transport(e)) => {
                if last_agent {
                    return Page::failed(url, Some(e));
                }
                attempt += 1;
                continue;
            }
        };
        if !last_agent && BLOCKED_STATUSES.contains(&hop.status) {
            attempt += 1;
            continue;
        }
        if hop.status == 429 && !retried_429 {
            retried_429 = true;
            tokio::time::sleep(retry_after_delay(hop.retry_after.as_deref())).await;
            continue;
        }
        return hop.into_page();
    }
}

enum Failure {
    /// Never retried with another user agent: the guard, a bad redirect, or the redirect limit.
    Refused(String),
    Transport(String),
}

struct Hop {
    status: u16,
    final_url: String,
    content_type: String,
    charset: Option<String>,
    retry_after: Option<String>,
    body: Vec<u8>,
    truncated: bool,
    lan: bool,
}

impl Hop {
    fn into_page(self) -> Page {
        Page {
            ok: (200..300).contains(&self.status),
            status: self.status,
            final_url: self.final_url,
            content_type: self.content_type,
            body: self.body,
            truncated: self.truncated,
            error: None,
            lan: self.lan,
            charset: self.charset,
        }
    }
}

async fn fetch_chain(g: &Guard, url: &str, ua: Option<&str>, opts: &LoadOpts) -> Result<Hop, Failure> {
    let mut headers = HeaderMap::new();
    if let Some(ua) = ua {
        headers.insert(USER_AGENT, header_value(ua)?);
    }
    headers.insert(ACCEPT, header_value(opts.accept.unwrap_or(DEFAULT_ACCEPT))?);
    headers.insert(ACCEPT_LANGUAGE, header_value("en-US,en;q=0.5")?);
    headers.insert(ACCEPT_ENCODING, header_value("identity")?);
    for (k, v) in &opts.headers {
        let name = HeaderName::from_bytes(k.as_bytes()).map_err(|e| Failure::Refused(format!("header {k}: {e}")))?;
        headers.insert(name, header_value(v)?);
    }

    // The checked and the requested URL must be the same parse.
    let mut current = Url::parse(url).map_err(|e| Failure::Refused(format!("invalid URL {url}: {e}")))?.to_string();
    let mut redirects = 0;
    let mut prev_host: Option<String> = None;
    // Once a hop leaves the LAN, no later hop may come back to it.
    let mut lan_allowed = match opts.reach {
        Reach::Explicit => true,
        Reach::Derived { lan_ok } => lan_ok,
    };
    loop {
        let (host, addrs) = resolve_checked(g, &current, lan_allowed).await.map_err(Failure::Refused)?;
        lan_allowed = lan_allowed && addrs.iter().all(|a| (g.lan)(a.ip()));
        if prev_host.as_ref().is_some_and(|h| *h != host) {
            headers.remove(AUTHORIZATION);
        }
        prev_host = Some(host.clone());

        let client = pinned_client(&host, &addrs).map_err(|e| Failure::Transport(describe(&e)))?;
        let resp = client
            .get(current.as_str())
            .headers(headers.clone())
            .timeout(opts.timeout)
            .send()
            .await
            .map_err(|e| Failure::Transport(describe(&e)))?;

        if matches!(resp.status().as_u16(), 301 | 302 | 303 | 307 | 308)
            && let Some(loc) = resp.headers().get(LOCATION).and_then(|v| v.to_str().ok())
        {
            if redirects == MAX_REDIRECTS {
                return Err(Failure::Refused(format!("too many redirects fetching {url}")));
            }
            let next = Url::parse(&current)
                .and_then(|base| base.join(loc))
                .map_err(|e| Failure::Refused(format!("bad redirect to {loc}: {e}")))?;
            if !matches!(next.scheme(), "http" | "https") {
                return Err(Failure::Refused(format!("refusing redirect to {next}: unsupported scheme")));
            }
            current = next.to_string();
            redirects += 1;
            continue;
        }

        let status = resp.status().as_u16();
        let raw_type = resp.headers().get(CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
        let retry_after = resp.headers().get(RETRY_AFTER).and_then(|v| v.to_str().ok()).map(str::to_string);
        let (body, truncated) = read_capped(resp, opts.max_bytes).await.map_err(Failure::Transport)?;
        return Ok(Hop {
            status,
            final_url: current,
            content_type: raw_type.split(';').next().unwrap_or_default().trim().to_ascii_lowercase(),
            charset: charset_param(&raw_type),
            retry_after,
            body,
            truncated,
            lan: lan_allowed,
        });
    }
}

fn header_value(v: &str) -> Result<HeaderValue, Failure> {
    HeaderValue::from_str(v).map_err(|e| Failure::Refused(format!("invalid header value {v:?}: {e}")))
}

/// Reads the body up to `max`. The flag is set when bytes beyond the cap were left unread.
async fn read_capped(mut resp: reqwest::Response, max: usize) -> Result<(Vec<u8>, bool), String> {
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| describe(&e))? {
        let room = max - body.len();
        if chunk.len() > room {
            body.extend_from_slice(&chunk[..room]);
            return Ok((body, true));
        }
        body.extend_from_slice(&chunk);
    }
    Ok((body, false))
}

/// `Retry-After` as seconds or an HTTP date, bounded to 10 s; 1 s when absent or unparseable.
fn retry_after_delay(value: Option<&str>) -> Duration {
    let Some(v) = value.map(str::trim) else {
        return Duration::from_secs(1);
    };
    let delay = match v.parse::<f64>() {
        Ok(secs) if secs.is_finite() => Duration::try_from_secs_f64(secs.max(0.0)).unwrap_or(RETRY_AFTER_CAP),
        _ => match httpdate::parse_http_date(v) {
            Ok(at) => at.duration_since(SystemTime::now()).unwrap_or_default(),
            Err(_) => return Duration::from_secs(1),
        },
    };
    delay.min(RETRY_AFTER_CAP)
}

/// Resolves the URL's host and refuses it if any address is blocked. Returns the host and the
/// checked addresses, which the connection is pinned to.
async fn resolve_checked(g: &Guard, url: &str, lan_allowed: bool) -> Result<(String, Vec<SocketAddr>), String> {
    let (host, port) = host_port(url).ok_or_else(|| format!("invalid URL: {url}"))?;
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), port))
        .await
        .map_err(|e| format!("cannot resolve {host}: {e}"))?
        .collect();
    if addrs.is_empty() {
        return Err(format!("cannot resolve {host}: no addresses"));
    }
    if let Some(a) = addrs.iter().find(|a| (g.blocked)(a.ip())) {
        return Err(format!(
            "refusing to fetch {host}: it resolves to an internal address ({}). Set MU_WEB_ALLOW_PRIVATE=1 to allow.",
            a.ip()
        ));
    }
    if !lan_allowed && let Some(a) = addrs.iter().find(|a| (g.lan)(a.ip())) {
        return Err(format!(
            "refusing to fetch {host}: it resolves to a LAN address ({}), and LAN addresses are only reachable when the URL you ask for is itself on the LAN, not through redirects or links from other pages. Set MU_WEB_ALLOW_PRIVATE=1 to allow.",
            a.ip()
        ));
    }
    Ok((host, addrs))
}

fn pinned_client(host: &str, addrs: &[SocketAddr]) -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .resolve_to_addrs(host, addrs)
        .build()
}

/// reqwest errors keep their cause in `source()`, which `Display` omits.
fn describe(e: &reqwest::Error) -> String {
    let mut msg = e.to_string();
    let mut src = std::error::Error::source(e);
    while let Some(s) = src {
        msg.push_str(": ");
        msg.push_str(&s.to_string());
        src = s.source();
    }
    msg
}

/// The label after `charset=` (quotes and spaces allowed), lowercased.
fn charset_param(s: &str) -> Option<String> {
    let lower = s.to_ascii_lowercase();
    let rest = &lower[lower.find("charset")? + "charset".len()..];
    let rest = rest.trim_start().strip_prefix('=')?.trim_start().trim_start_matches(['"', '\'']);
    let label: String = rest.chars().take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_')).collect();
    (!label.is_empty()).then_some(label)
}

fn meta_charset(body: &[u8]) -> Option<String> {
    let head = &body[..body.len().min(2048)];
    let lower = String::from_utf8_lossy(head).to_ascii_lowercase();
    let mut rest = lower.as_str();
    while let Some(i) = rest.find("<meta") {
        let tag = &rest[i..];
        let end = tag.find('>').unwrap_or(tag.len());
        if let Some(label) = charset_param(&tag[..end]) {
            return Some(label);
        }
        rest = &tag[end..];
    }
    None
}

/// Host and port as the HTTP client will read them: both come from the same `Url` parse.
fn host_port(url: &str) -> Option<(String, u16)> {
    let u = Url::parse(url).ok()?;
    if !matches!(u.scheme(), "http" | "https") {
        return None;
    }
    let host = match u.host()? {
        url::Host::Domain(d) => d.to_string(),
        url::Host::Ipv4(a) => a.to_string(),
        url::Host::Ipv6(a) => a.to_string(),
    };
    Some((host, u.port_or_known_default()?))
}

/// LAN addresses: RFC 1918, IPv6 unique-local and CGNAT (Tailscale).
fn is_lan(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => {
            let o = v.octets();
            v.is_private() || (o[0] == 100 && (64..128).contains(&o[1]))
        }
        IpAddr::V6(v) => match v.to_ipv4_mapped() {
            Some(m) => is_lan(IpAddr::V4(m)),
            None => (v.segments()[0] & 0xfe00) == 0xfc00,
        },
    }
}

/// Addresses a fetch never reaches: loopback, link-local (which holds cloud metadata services),
/// "this network" (`0.0.0.0/8`, which some stacks route to the host), broadcast, multicast and
/// Alibaba Cloud's metadata address, which sits inside the CGNAT range the LAN rule allows.
fn is_internal(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => {
            v.is_loopback()
                || v.is_link_local()
                || v.octets()[0] == 0
                || v.is_broadcast()
                || v.is_multicast()
                || v.octets() == [100, 100, 100, 200]
        }
        IpAddr::V6(v) => {
            if let Some(m) = v.to_ipv4_mapped() {
                return is_internal(IpAddr::V4(m));
            }
            let s = v.segments();
            v.is_loopback() || v.is_unspecified() || v.is_multicast() || (s[0] & 0xffc0) == 0xfe80 // link local
        }
    }
}

/// A loopback HTTP/1.1 responder for tests. Each connection serves one request and closes.
#[cfg(test)]
pub(crate) mod test_server {
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    pub struct Req {
        pub path: String,
        pub headers: Vec<(String, String)>,
    }

    impl Req {
        pub fn header(&self, name: &str) -> Option<&str> {
            self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
        }
    }

    pub struct Resp {
        pub status: u16,
        pub headers: Vec<(String, String)>,
        pub body: Vec<u8>,
    }

    impl Resp {
        pub fn new(status: u16, content_type: &str, body: impl Into<Vec<u8>>) -> Self {
            Self { status, headers: vec![("Content-Type".into(), content_type.into())], body: body.into() }
        }

        pub fn with(mut self, name: &str, value: &str) -> Self {
            self.headers.push((name.into(), value.into()));
            self
        }
    }

    /// Serves `handler` on a fresh loopback port, returned. The server lives as long as the runtime.
    pub async fn serve<F>(handler: F) -> u16
    where
        F: Fn(&Req) -> Resp + Send + Sync + 'static,
    {
        serve_at("127.0.0.1", handler).await
    }

    /// The address that plays a LAN host in tests: `::1`, the only loopback address besides
    /// `127.0.0.1` that macOS has assigned, so binding it works on both platforms. Any other
    /// 127/8 address fails with `EADDRNOTAVAIL` on macOS, as a real LAN address would.
    pub const LAN: &str = "::1";

    /// The origin of a server on `LAN`; IPv6, so the host is bracketed.
    pub fn lan_origin(port: u16) -> String {
        format!("http://[{LAN}]:{port}")
    }

    /// `serve` on another address.
    pub async fn serve_at<F>(ip: &str, handler: F) -> u16
    where
        F: Fn(&Req) -> Resp + Send + Sync + 'static,
    {
        let listener = TcpListener::bind((ip, 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let handler = Arc::new(handler);
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let handler = handler.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 1024];
                    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        match sock.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        }
                    }
                    let head = String::from_utf8_lossy(&buf).into_owned();
                    let mut lines = head.split("\r\n");
                    let path = lines.next().and_then(|l| l.split_whitespace().nth(1)).unwrap_or("/").to_string();
                    let headers = lines
                        .take_while(|l| !l.is_empty())
                        .filter_map(|l| l.split_once(':'))
                        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
                        .collect();
                    let resp = (*handler)(&Req { path, headers });
                    let mut out = format!(
                        "HTTP/1.1 {} X\r\nContent-Length: {}\r\nConnection: close\r\n",
                        resp.status,
                        resp.body.len()
                    );
                    for (k, v) in &resp.headers {
                        out.push_str(&format!("{k}: {v}\r\n"));
                    }
                    out.push_str("\r\n");
                    let _ = sock.write_all(out.as_bytes()).await;
                    let _ = sock.write_all(&resp.body).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        port
    }
}

#[cfg(test)]
mod tests {
    use super::test_server::{Resp, serve};
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn page(content_type: &str, charset: Option<&str>, body: &[u8]) -> Page {
        Page {
            ok: true,
            status: 200,
            final_url: String::new(),
            content_type: content_type.into(),
            body: body.to_vec(),
            truncated: false,
            error: None,
            lan: false,
            charset: charset.map(str::to_string),
        }
    }

    #[test]
    fn charset_from_header_or_meta() {
        assert_eq!(page("text/plain", Some("iso-8859-1"), b"caf\xe9").text(), "café");
        let html = b"<html><head><meta http-equiv=x content='text/html; charset=windows-1252'></head>caf\xe9";
        assert!(page("text/html", None, html).text().ends_with("</head>café"));
        assert_eq!(page("application/json", None, "ü".as_bytes()).text(), "ü");
        assert_eq!(page("text/plain", Some("no-such-charset"), "ok".as_bytes()).text(), "ok");
    }

    #[test]
    fn charset_param_parsing() {
        assert_eq!(charset_param("text/html; Charset=\"UTF-8\""), Some("utf-8".into()));
        assert_eq!(charset_param("text/html"), None);
    }

    #[tokio::test]
    async fn ua_rotates_after_a_refusal() {
        let port = serve(|req| {
            let ua = req.header("user-agent").unwrap_or_default();
            if ua.starts_with("curl/") {
                Resp::new(403, "text/plain", "no")
            } else {
                Resp::new(200, "text/plain", "hello")
            }
        })
        .await;
        let p = load_page(&Guard::allow_all(), &format!("http://127.0.0.1:{port}/"), LoadOpts::default()).await;
        assert!(p.ok, "{:?}", p.error);
        assert_eq!(p.text(), "hello");
    }

    #[tokio::test]
    async fn caller_user_agent_is_not_rotated() {
        let port = serve(|_| Resp::new(403, "text/plain", "no")).await;
        let opts = LoadOpts { headers: vec![("User-Agent".into(), "mu-web".into())], ..Default::default() };
        let p = load_page(&Guard::allow_all(), &format!("http://127.0.0.1:{port}/"), opts).await;
        assert!(!p.ok);
        assert_eq!(p.status, 403);
    }

    #[tokio::test]
    async fn rate_limited_last_agent_is_retried_once() {
        let hits = AtomicUsize::new(0);
        let port = serve(move |_| {
            if hits.fetch_add(1, Ordering::SeqCst) == 0 {
                Resp::new(429, "text/plain", "slow").with("Retry-After", "0")
            } else {
                Resp::new(200, "text/plain", "ok")
            }
        })
        .await;
        let opts = LoadOpts { headers: vec![("User-Agent".into(), "mu-web".into())], ..Default::default() };
        let p = load_page(&Guard::allow_all(), &format!("http://127.0.0.1:{port}/"), opts).await;
        assert!(p.ok, "{:?}", p.error);
        assert_eq!(p.text(), "ok");
    }

    #[tokio::test]
    async fn body_is_capped() {
        let port = serve(|_| Resp::new(200, "text/plain", vec![b'a'; 1_000_000])).await;
        let opts = LoadOpts { max_bytes: 1000, ..Default::default() };
        let p = load_page(&Guard::allow_all(), &format!("http://127.0.0.1:{port}/"), opts).await;
        assert!(p.ok);
        assert_eq!(p.body.len(), 1000);
        assert!(p.truncated);
    }

    #[tokio::test]
    async fn redirect_into_blocked_address_is_refused_before_connecting() {
        let port = serve(|_| Resp::new(302, "text/plain", "").with("Location", "http://127.0.0.2:9/")).await;
        let g = Guard { blocked: |ip| ip == IpAddr::from([127, 0, 0, 2]), lan: |_| false };
        let p = load_page(&g, &format!("http://127.0.0.1:{port}/"), LoadOpts::default()).await;
        assert!(!p.ok);
        let err = p.error.unwrap_or_default();
        assert!(err.contains("internal address"), "{err}");
    }

    /// `::1` plays the LAN and 127.0.0.1 the public internet.
    fn lan_guard() -> Guard {
        Guard { blocked: |_| false, lan: |ip| ip == IpAddr::V6(std::net::Ipv6Addr::LOCALHOST) }
    }

    #[tokio::test]
    async fn a_lan_url_asked_for_directly_is_fetched() {
        use super::test_server::{LAN, lan_origin, serve_at};
        let port = serve_at(LAN, |_| Resp::new(200, "text/plain", "router page")).await;
        let p = load_page(&lan_guard(), &format!("{}/", lan_origin(port)), LoadOpts::default()).await;
        assert!(p.ok && p.lan, "{:?}", p.error);
    }

    #[tokio::test]
    async fn a_public_page_cannot_redirect_into_the_lan() {
        use super::test_server::{LAN, lan_origin, serve_at};
        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();
        let lan = serve_at(LAN, move |_| {
            h.fetch_add(1, Ordering::SeqCst);
            Resp::new(200, "text/plain", "admin")
        })
        .await;
        let loc = format!("{}/", lan_origin(lan));
        let public = serve(move |_| Resp::new(302, "text/plain", "").with("Location", &loc)).await;
        let p = load_page(&lan_guard(), &format!("http://127.0.0.1:{public}/"), LoadOpts::default()).await;
        assert!(!p.ok);
        assert!(p.error.unwrap_or_default().contains("LAN address"));
        assert_eq!(hits.load(Ordering::SeqCst), 0, "the LAN host was contacted");
    }

    #[tokio::test]
    async fn a_lan_page_may_redirect_within_the_lan_but_not_out_and_back() {
        use super::test_server::{LAN, lan_origin, serve_at};
        let b = serve_at(LAN, |_| Resp::new(200, "text/plain", "b")).await;
        let to_b = format!("{}/", lan_origin(b));
        let a = serve_at(LAN, move |_| Resp::new(302, "text/plain", "").with("Location", &to_b)).await;
        let p = load_page(&lan_guard(), &format!("{}/", lan_origin(a)), LoadOpts::default()).await;
        assert!(p.ok && p.lan, "{:?}", p.error);

        // LAN, then public, then LAN again: once out, the chain cannot return.
        let back = format!("{}/", lan_origin(b));
        let public = serve(move |_| Resp::new(302, "text/plain", "").with("Location", &back)).await;
        let out = format!("http://127.0.0.1:{public}/");
        let a2 = serve_at(LAN, move |_| Resp::new(302, "text/plain", "").with("Location", &out)).await;
        let p = load_page(&lan_guard(), &format!("{}/", lan_origin(a2)), LoadOpts::default()).await;
        assert!(!p.ok && p.error.unwrap_or_default().contains("LAN address"));
    }

    #[tokio::test]
    async fn a_derived_request_reaches_the_lan_only_from_a_lan_page() {
        use super::test_server::{LAN, lan_origin, serve_at};
        let lan = serve_at(LAN, |_| Resp::new(200, "text/plain", "x")).await;
        let url = format!("{}/", lan_origin(lan));
        let from_public = LoadOpts { reach: Reach::Derived { lan_ok: false }, ..Default::default() };
        let p = load_page(&lan_guard(), &url, from_public).await;
        assert!(!p.ok && p.error.unwrap_or_default().contains("LAN address"));
        let from_lan = LoadOpts { reach: Reach::Derived { lan_ok: true }, ..Default::default() };
        assert!(load_page(&lan_guard(), &url, from_lan).await.ok);
    }

    #[tokio::test]
    async fn default_guard_refuses_loopback() {
        let port = serve(|_| Resp::new(200, "text/plain", "x")).await;
        let p = load_page(&Guard::default(), &format!("http://127.0.0.1:{port}/"), LoadOpts::default()).await;
        assert!(!p.ok);
        assert!(p.error.unwrap_or_default().contains("internal address"));
    }

    #[test]
    fn internal_addresses_are_refused_and_lan_ones_allowed() {
        for ip in [
            "127.0.0.1",
            "169.254.169.254",
            "0.0.0.0",
            "0.1.2.3",
            "0.255.255.255",
            "100.100.100.200",
            "::ffff:100.100.100.200",
            "::1",
            "fe80::1",
            "::ffff:127.0.0.1",
            "::",
        ] {
            assert!(is_internal(ip.parse().unwrap()), "{ip}");
        }
        for ip in [
            "8.8.8.8",
            "1.1.1.1",
            "2606:4700::1111",
            "10.0.0.1",
            "172.16.0.1",
            "192.168.1.80",
            "fd00::1",
            "100.64.0.1",
            "100.100.100.201",
        ] {
            assert!(!is_internal(ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn host_port_parsing() {
        assert_eq!(host_port("http://a.com/x"), Some(("a.com".into(), 80)));
        assert_eq!(host_port("https://u:p@a.com:8443/x?y"), Some(("a.com".into(), 8443)));
        assert_eq!(host_port("http://[::1]:9000/"), Some(("::1".into(), 9000)));
        assert_eq!(host_port("http://"), None);
        assert_eq!(host_port("ftp://a.com/"), None);
    }

    #[test]
    fn host_port_agrees_with_the_client_on_backslash() {
        assert_eq!(host_port("http://127.0.0.1:9\\@8.8.8.8/"), Some(("127.0.0.1".into(), 9)));
    }

    #[test]
    fn huge_retry_after_is_capped() {
        assert_eq!(retry_after_delay(Some("1e30")), RETRY_AFTER_CAP);
    }

    #[tokio::test]
    async fn localhost_fetch_is_blocked() {
        for lan_allowed in [true, false] {
            assert!(resolve_checked(&Guard::default(), "http://127.0.0.1:1/", lan_allowed).await.is_err());
            assert!(resolve_checked(&Guard::default(), "http://169.254.169.254/latest", lan_allowed).await.is_err());
        }
        assert!(resolve_checked(&Guard::default(), "http://192.168.1.80/", true).await.is_ok());
        assert!(resolve_checked(&Guard::default(), "http://192.168.1.80/", false).await.is_err());
    }
}
