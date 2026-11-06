//! `web_search`: one backend, chosen by the user and handed over by the host (`core/webSearch`).
//! Nothing here picks a backend or falls back to another one.

mod anthropic;
mod exa;
mod mcp;
mod openai;
mod parallel;

use serde_json::{Value, json};
use std::sync::OnceLock;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(60);
const MAX_REPLY: usize = 8 * 1024 * 1024;

/// What the web gate says when no backend is set up; the host normally refuses first.
pub const NOT_SET_UP: &str = "Web search is not set up, so nothing was searched. In interactive mu run /web to choose a search backend, or run `mu web use <backend>` (see `mu web --help`).";

static SESSION: OnceLock<String> = OnceLock::new();

/// The session id from `initialize`, sent to Parallel's free endpoint.
pub fn set_session(id: &str) {
    let _ = SESSION.set(id.chars().take(100).collect());
}

#[derive(Debug, Clone, PartialEq)]
pub struct Source {
    pub title: String,
    pub url: String,
    pub snippet: Option<String>,
    pub published: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Citation {
    pub url: String,
    pub title: Option<String>,
    pub cited: Option<String>,
}

#[derive(Debug, Default, PartialEq)]
pub struct SearchResponse {
    pub answer: Option<String>,
    pub sources: Vec<Source>,
    pub citations: Vec<Citation>,
    pub queries: Vec<String>,
    pub notes: Vec<String>,
}

/// The host's answer to `core/webSearch`.
#[derive(Debug, Clone)]
pub struct Config {
    pub backend: String,
    pub keyless: bool,
    pub key: Option<String>,
    pub base_url: Option<String>,
    pub model: Option<String>,
}

impl Config {
    fn from_value(v: &Value) -> Option<Self> {
        let s = |k: &str| v.get(k).and_then(Value::as_str).map(String::from);
        Some(Self {
            backend: s("backend")?,
            keyless: v.get("keyless").and_then(Value::as_bool).unwrap_or(false),
            key: s("key"),
            base_url: s("baseUrl"),
            model: s("model"),
        })
    }
}

pub struct Params<'a> {
    pub query: &'a str,
    pub recency: Option<&'a str>,
    pub limit: usize,
}

impl Params<'_> {
    /// `YYYY-MM-DD` of the start of the recency window.
    fn after(&self) -> Option<String> {
        let days = match self.recency? {
            "day" => 1,
            "week" => 7,
            "month" => 30,
            "year" => 365,
            _ => return None,
        };
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok()?.as_secs() as i64;
        Some(civil(now.div_euclid(86_400) - days))
    }

    /// `query` with the recency bound in the operator form the free endpoints take.
    fn dated_query(&self) -> String {
        match self.after() {
            Some(d) => format!("{} after:{d}", self.query),
            None => self.query.to_string(),
        }
    }
}

/// Days since 1970-01-01 as `YYYY-MM-DD` (proleptic Gregorian).
fn civil(days: i64) -> String {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

pub async fn search(args: &Value) -> (String, bool) {
    let Some(query) = args.get("query").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()) else {
        return ("query is required".into(), true);
    };
    let recency = match args.get("recency").and_then(Value::as_str).filter(|s| !s.trim().is_empty()) {
        None => None,
        Some(r @ ("day" | "week" | "month" | "year")) => Some(r),
        Some(other) => return (format!("recency must be day, week, month or year, not `{other}`"), true),
    };
    let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(5).clamp(1, 20) as usize;
    let cfg = match mu_common::rpc::request("core/webSearch", json!({})).await {
        Ok(v) => match Config::from_value(&v) {
            Some(c) => c,
            None => return (NOT_SET_UP.into(), true),
        },
        Err(e) => return (format!("could not read the web search settings: {e}"), true),
    };
    run(&cfg, &Params { query, recency, limit }, None).await
}

/// Runs the search on `cfg`'s backend. `origin` replaces the backend's host (tests).
pub async fn run(cfg: &Config, p: &Params<'_>, origin: Option<&str>) -> (String, bool) {
    let work = async {
        match cfg.backend.as_str() {
            "parallel" => parallel::search(cfg, p, origin).await,
            "exa" => exa::search(cfg, p, origin).await,
            "anthropic" => anthropic::search(cfg, p, origin).await,
            "openai" => openai::search(cfg, p, origin).await,
            other => Err(format!("unknown web search backend `{other}`")),
        }
    };
    let result = match tokio::time::timeout(TIMEOUT, work).await {
        Ok(r) => r,
        Err(_) => Err(format!("the search timed out after {}s", TIMEOUT.as_secs())),
    };
    match result {
        Ok(mut r) => {
            let ignores = matches!(cfg.backend.as_str(), "anthropic" | "openai");
            if ignores && p.recency.is_some() {
                let who = if cfg.backend == "anthropic" { "Anthropic web search" } else { "OpenAI web search" };
                r.notes.push(format!("recency is not applied by {who}"));
            }
            (format_for_llm(&r), false)
        }
        Err(e) => (e, true),
    }
}

fn trunc(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n - 1).collect::<String>().trim_end())
    }
}

fn count(word: &str, n: usize) -> String {
    format!("{n} {word}{}", if n == 1 { "" } else { "s" })
}

/// The text the model sees.
pub fn format_for_llm(r: &SearchResponse) -> String {
    if r.sources.is_empty() && r.answer.is_none() && r.citations.is_empty() {
        let mut out: Vec<String> = r.notes.iter().map(|n| format!("Note: {n}")).collect();
        out.push("No results.".into());
        return out.join("\n");
    }
    let mut parts: Vec<String> = r.notes.iter().map(|n| format!("Note: {n}")).collect();
    if let Some(a) = &r.answer {
        parts.push(a.clone());
        if !r.sources.is_empty() {
            parts.push("\n## Sources".into());
            parts.push(count("source", r.sources.len()));
        }
    }
    for (i, s) in r.sources.iter().enumerate() {
        let age = s.published.as_deref().map(|a| format!(" ({a})")).unwrap_or_default();
        parts.push(format!("[{}] {}{age}\n    {}", i + 1, s.title, s.url));
        if let Some(sn) = s.snippet.as_deref().filter(|s| !s.is_empty()) {
            parts.push(format!("    {}", trunc(sn, 240)));
        }
    }
    if !r.citations.is_empty() {
        parts.push("\n## Citations".into());
        parts.push(count("citation", r.citations.len()));
        for (i, c) in r.citations.iter().enumerate() {
            let title = c.title.as_deref().filter(|t| !t.is_empty()).unwrap_or(&c.url);
            parts.push(format!("[{}] {title}\n    {}", i + 1, c.url));
            if let Some(t) = c.cited.as_deref().filter(|t| !t.is_empty()) {
                parts.push(format!("    {}", trunc(t, 240)));
            }
        }
    }
    if !r.queries.is_empty() {
        parts.push(format!("Search queries: {}", r.queries.len()));
        for q in r.queries.iter().take(3) {
            parts.push(format!("- {}", trunc(q, 120)));
        }
    }
    parts.join("\n")
}

/// One HTTP reply: status, content type and body (capped).
pub(crate) struct Reply {
    pub status: u16,
    pub content_type: String,
    pub body: String,
}

/// POSTs JSON. Redirects are never followed, so a key in a header cannot leave for another host.
pub(crate) async fn post(url: &str, headers: &[(&str, &str)], body: &Value) -> Result<Reply, String> {
    crate::net::install_crypto();
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| format!("cannot build an HTTP client: {e}"))?;
    let mut rb = client
        .post(url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .body(body.to_string());
    for (k, v) in headers {
        rb = rb.header(*k, *v);
    }
    let mut resp = rb.send().await.map_err(|e| format!("cannot reach {}: {}", host_of(url), e.without_url()))?;
    let status = resp.status().as_u16();
    let content_type = resp.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
    let mut buf = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| format!("reading the reply failed: {}", e.without_url()))? {
        buf.extend_from_slice(&chunk);
        if buf.len() > MAX_REPLY {
            return Err("the reply was too large".into());
        }
    }
    Ok(Reply { status, content_type, body: String::from_utf8_lossy(&buf).into_owned() })
}

fn host_of(url: &str) -> &str {
    url.split_once("://").map_or(url, |(_, r)| r).split(['/', '?']).next().unwrap_or(url)
}

pub(crate) fn dedupe_push(sources: &mut Vec<Source>, s: Source) {
    match sources.iter_mut().find(|x| x.url == s.url) {
        Some(x) => {
            if x.title == x.url && s.title != s.url {
                x.title = s.title;
            }
            if x.snippet.is_none() {
                x.snippet = s.snippet;
            }
        }
        None => sources.push(s),
    }
}

#[cfg(test)]
pub(crate) mod fake {
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A loopback HTTP server: the n-th request gets the n-th reply (the last repeats). Returns its
    /// origin and the raw requests seen.
    pub async fn serve(replies: Vec<(u16, &'static str, String)>) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        tokio::spawn(async move {
            let mut n = 0;
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { return };
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                let (head_end, want) = loop {
                    let Ok(k) = sock.read(&mut tmp).await else { break (0, 0) };
                    if k == 0 {
                        break (0, 0);
                    }
                    buf.extend_from_slice(&tmp[..k]);
                    if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..p]).to_lowercase();
                        let len = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        break (p + 4, len);
                    }
                };
                while buf.len() < head_end + want {
                    let Ok(k) = sock.read(&mut tmp).await else { break };
                    if k == 0 {
                        break;
                    }
                    buf.extend_from_slice(&tmp[..k]);
                }
                log.lock().unwrap().push(String::from_utf8_lossy(&buf).into_owned());
                let (status, ct, body) = replies[n.min(replies.len() - 1)].clone();
                n += 1;
                let head = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: {ct}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(body.as_bytes()).await;
                let _ = sock.shutdown().await;
            }
        });
        (origin, seen)
    }

    pub fn body_of(raw: &str) -> serde_json::Value {
        serde_json::from_str(raw.split("\r\n\r\n").nth(1).unwrap()).unwrap()
    }

    pub fn head_of(raw: &str) -> String {
        raw.split("\r\n\r\n").next().unwrap().to_lowercase()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_are_civil() {
        assert_eq!(civil(0), "1970-01-01");
        assert_eq!(civil(19_723), "2024-01-01");
        assert_eq!(civil(19_782), "2024-02-29");
        assert_eq!(civil(-1), "1969-12-31");
    }

    #[test]
    fn the_golden_output() {
        let r = SearchResponse {
            answer: Some("Rust is a language.".into()),
            sources: vec![
                Source {
                    title: "Rust".into(),
                    url: "https://rust-lang.org".into(),
                    snippet: Some("x".repeat(300)),
                    published: Some("2025-04-30".into()),
                },
                Source {
                    title: "https://b.example".into(),
                    url: "https://b.example".into(),
                    snippet: None,
                    published: None,
                },
            ],
            citations: vec![Citation {
                url: "https://rust-lang.org".into(),
                title: Some("Rust".into()),
                cited: Some("a language".into()),
            }],
            queries: vec!["rust".into()],
            notes: vec!["recency is not applied by OpenAI web search".into()],
        };
        let want = format!(
            "Note: recency is not applied by OpenAI web search\nRust is a language.\n\n## Sources\n2 sources\n[1] Rust (2025-04-30)\n    https://rust-lang.org\n    {}…\n[2] https://b.example\n    https://b.example\n\n## Citations\n1 citation\n[1] Rust\n    https://rust-lang.org\n    a language\nSearch queries: 1\n- rust",
            "x".repeat(239)
        );
        assert_eq!(format_for_llm(&r), want);
    }

    #[test]
    fn nothing_found_says_so() {
        assert_eq!(format_for_llm(&SearchResponse::default()), "No results.");
    }

    #[tokio::test]
    async fn bad_arguments_are_refused_before_anything_is_asked() {
        assert_eq!(search(&json!({})).await, ("query is required".into(), true));
        let (m, err) = search(&json!({"query": "x", "recency": "decade"})).await;
        assert!(err && m.contains("recency must be"), "{m}");
        // Outside serve_stdio there is no host to ask.
        let (m, err) = search(&json!({"query": "x"})).await;
        assert!(err && m.contains("could not read the web search settings"), "{m}");
    }

    #[tokio::test]
    async fn an_ignored_recency_is_noted() {
        let cfg = Config { backend: "off".into(), keyless: false, key: None, base_url: None, model: None };
        let (m, err) = run(&cfg, &Params { query: "q", recency: None, limit: 5 }, None).await;
        assert!(err && m.contains("unknown web search backend"), "{m}");
    }
}
