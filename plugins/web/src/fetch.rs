//! `fetch`: one URL to Markdown. A port of omp's `renderUrl` (`tools/fetch.ts`), limited to the
//! classes of URL in use: GitHub handlers, JSON, text, and HTML through the markdown
//! alternates, `.md` suffix, content negotiation, native conversion and llms.txt.

use crate::github;
use crate::html;
use crate::net::{Guard, LoadOpts, MAX_BYTES, Page, Reach, load_page};
use std::time::Duration;
use url::Url;

const OVERALL: Duration = Duration::from_secs(30);
const LLMS_TIMEOUT: Duration = Duration::from_secs(5);
const OUTPUT_CAP: usize = 100 * 1024;
const NEGOTIATE_ACCEPT: &str = "text/markdown, text/plain;q=0.9, text/html;q=0.8";
const JS_NOTE: &str = "Page appears to require JavaScript or is mostly navigation";
const BINARY_SNIFF: usize = 8 * 1024;

pub struct Rendered {
    pub final_url: String,
    pub content_type: String,
    pub method: &'static str,
    pub notes: Vec<String>,
    pub content: String,
}

impl Rendered {
    pub fn new(final_url: &str, content_type: &str, method: &'static str, notes: Vec<String>, content: String) -> Self {
        Self {
            final_url: final_url.into(),
            content_type: content_type.into(),
            method,
            notes,
            content: finalize(&content),
        }
    }
}

/// Renders `url` as the tool's output: an omp-style header, then the content, capped at 100 KB.
/// `Err` is for input the tool rejects, and for the 30 s overall budget running out.
pub async fn fetch(g: &Guard, url: &str, raw: bool) -> Result<String, String> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err("fetch only supports http(s) URLs".into());
    }
    let rendered = tokio::time::timeout(OVERALL, render(g, url, raw))
        .await
        .map_err(|_| format!("fetch timed out after {}s", OVERALL.as_secs()))?;
    // A failed load is the tool's error, with the same header so the cause is in the notes.
    if rendered.method == "failed" { Err(output(&rendered)) } else { Ok(output(&rendered)) }
}

fn output(r: &Rendered) -> String {
    let mut out = format!("URL: {}\nContent-Type: {}\nMethod: {}\n", r.final_url, r.content_type, r.method);
    if !r.notes.is_empty() {
        out.push_str(&format!("Notes: {}\n", r.notes.join("; ")));
    }
    out.push_str("\n---\n\n");
    out.push_str(&r.content);
    truncate(out)
}

async fn render(g: &Guard, url: &str, raw: bool) -> Rendered {
    if !raw && let Some(r) = github::handle(g, &github::Endpoints::production(), url).await {
        return r;
    }

    let page = load_page(g, url, LoadOpts::default()).await;
    let final_url = page.final_url.clone();
    let mime = if page.content_type.is_empty() { "unknown".to_string() } else { page.content_type.clone() };
    if !page.ok {
        let mut notes = vec![match page.status {
            0 => "Failed to fetch URL".to_string(),
            s => format!("Failed to fetch URL (HTTP {s})"),
        }];
        if let Some(e) = &page.error {
            notes.push(format!("Cause: {e}"));
        }
        return Rendered::new(&final_url, &mime, "failed", notes, String::new());
    }
    if page.status == 204 {
        return Rendered::new(&final_url, &mime, "no-content", vec!["HTTP 204 No Content".into()], String::new());
    }

    let mut notes = Vec::new();
    if page.truncated {
        notes.push(format!("Response truncated at {} MB", MAX_BYTES / (1024 * 1024)));
    }
    let text = page.text();
    if raw {
        return Rendered::new(&final_url, &page.content_type, "raw", notes, text);
    }

    if page.content_type.contains("json") {
        let pretty = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| serde_json::to_string_pretty(&v).ok())
            .unwrap_or(text);
        return Rendered::new(&final_url, &page.content_type, "json", notes, pretty);
    }

    let is_text = page.content_type.contains("text/plain") || page.content_type.contains("text/markdown");
    if is_text && !html::looks_like_html(&text) {
        return Rendered::new(&final_url, &page.content_type, "text", notes, text);
    }

    if page.content_type.contains("html") {
        return html_flow(g, url, &page, text, notes).await;
    }

    if page.body[..page.body.len().min(BINARY_SNIFF)].contains(&0) {
        let notice = format!("Binary content ({}, {} bytes) not shown.", page.content_type, page.body.len());
        return Rendered::new(&final_url, &page.content_type, "binary", notes, notice);
    }
    Rendered::new(&final_url, &page.content_type, "raw", notes, text)
}

/// Markdown for an HTML page, trying the cheapest source of clean text first.
async fn html_flow(g: &Guard, url: &str, page: &Page, html: String, mut notes: Vec<String>) -> Rendered {
    let final_url = page.final_url.as_str();
    let mime = page.content_type.as_str();
    let Ok(page_url) = Url::parse(final_url) else {
        return Rendered::new(final_url, mime, "raw-html", notes, html);
    };

    if let Some(alt) = html::markdown_alternate(&html, &page_url)
        && let Some(body) = accepted(g, alt.as_str(), derived(page, LoadOpts::default())).await
    {
        notes.push(format!("Used markdown alternate: {alt}"));
        return Rendered::new(final_url, "text/markdown", "alternate-markdown", notes, body);
    }

    if let Some(md_url) = md_suffix_url(&page_url)
        && let Some(body) = accepted(g, md_url.as_str(), derived(page, LoadOpts::default())).await
    {
        notes.push("Found .md suffix version".into());
        return Rendered::new(final_url, "text/markdown", "md-suffix", notes, body);
    }

    let negotiated = load_page(g, url, LoadOpts { accept: Some(NEGOTIATE_ACCEPT), ..Default::default() }).await;
    if negotiated.ok {
        let body = negotiated.text();
        let ntype = negotiated.content_type.as_str();
        if (ntype.contains("markdown") || ntype == "text/plain") && !html::looks_like_html(&body) {
            notes.push(format!("Content negotiation returned {ntype}"));
            return Rendered::new(final_url, ntype, "content-negotiation", notes, body);
        }
    }

    let native = html::to_markdown(&html).ok().map(|md| html::strip_data_uri_images(&md));
    let native_len = native.as_deref().map_or(0, |md| md.trim().chars().count());
    if let Some(md) = &native
        && native_len > 100
        && !html::is_low_quality(md)
    {
        return Rendered::new(final_url, mime, "native", notes, md.clone());
    }

    for candidate in llms_candidates(&page_url) {
        let opts = derived(page, LoadOpts { timeout: LLMS_TIMEOUT, ..Default::default() });
        if let Some(body) = accepted(g, &candidate, opts).await {
            notes.push(format!("Used llms.txt fallback: {candidate}"));
            return Rendered::new(final_url, "text/plain", "llms-txt", notes, body);
        }
    }

    match native {
        Some(md) if native_len > 100 => {
            notes.push(JS_NOTE.into());
            Rendered::new(final_url, mime, "native", notes, md)
        }
        _ => Rendered::new(final_url, mime, "raw-html", notes, html),
    }
}

/// `opts` for a URL taken from `page`'s content: it reaches the LAN only if `page` came from there.
fn derived(page: &Page, opts: LoadOpts) -> LoadOpts {
    LoadOpts { reach: Reach::Derived { lan_ok: page.lan }, ..opts }
}

/// The body of a successful, substantial, non-HTML response.
async fn accepted(g: &Guard, url: &str, opts: LoadOpts) -> Option<String> {
    let page = load_page(g, url, opts).await;
    if !page.ok {
        return None;
    }
    let body = page.text();
    (body.trim().chars().count() > 100 && !html::looks_like_html(&body)).then_some(body)
}

/// `/a/b/` gives `/a/b/index.html.md`; `/a/b` and `/a/b.html` give `/a/b.md` and `/a/b.html.md`.
fn md_suffix_url(page: &Url) -> Option<Url> {
    let path = page.path();
    let suffixed = if path.ends_with('/') { format!("{path}index.html.md") } else { format!("{path}.md") };
    Url::parse(&format!("{}{suffixed}", page.origin().ascii_serialization())).ok()
}

/// llms.txt and llms.md at each directory from the page's own scope up to the root. Port of omp's
/// `buildLlmEndpointCandidates`, including its scope rule for a file path.
fn llms_candidates(page: &Url) -> Vec<String> {
    let origin = page.origin().ascii_serialization();
    let path = page.path();
    if path == "/" {
        return vec![
            format!("{origin}/.well-known/llms.txt"),
            format!("{origin}/llms.txt"),
            format!("{origin}/llms.md"),
        ];
    }
    let segments: Vec<&str> = path.trim_end_matches('/').split('/').filter(|s| !s.is_empty()).collect();
    if segments.is_empty() {
        return Vec::new();
    }
    let scope_depth = if path.ends_with('/') { segments.len() } else { segments.len().saturating_sub(1).max(1) };
    let mut out = Vec::new();
    for depth in (1..=scope_depth).rev() {
        let scope = format!("/{}/", segments[..depth].join("/"));
        out.push(format!("{origin}{scope}llms.txt"));
        out.push(format!("{origin}{scope}llms.md"));
    }
    out
}

/// omp's `finalizeOutput`: runs of three or more newlines collapse to two, then trim.
fn finalize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut run = 0;
    for c in s.chars() {
        if c == '\n' {
            run += 1;
            if run > 2 {
                continue;
            }
        } else {
            run = 0;
        }
        out.push(c);
    }
    out.trim().to_string()
}

fn truncate(mut s: String) -> String {
    if s.len() > OUTPUT_CAP {
        let mut cut = OUTPUT_CAP;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
        s.push_str("\n[truncated at 100KB]");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::test_server::{Resp, serve};

    const DOCS: &str = "<html><head><title>Guide</title></head><body><nav><a href='/'>Home</a></nav>\
        <h1>Getting started</h1><p>Install the crate with cargo, then read the guide below to learn the core concepts and the main API.</p>\
        <pre><code>cargo add foo</code></pre></body></html>";

    fn base(port: u16) -> String {
        format!("http://127.0.0.1:{port}")
    }

    async fn run(url: &str) -> String {
        fetch(&Guard::allow_all(), url, false).await.unwrap_or_else(|e| e)
    }

    fn method(out: &str) -> &str {
        out.lines().find_map(|l| l.strip_prefix("Method: ")).unwrap_or_default()
    }

    #[tokio::test]
    async fn native_html_for_a_docs_page() {
        let port = serve(|req| match req.path.as_str() {
            "/docs/guide" => Resp::new(200, "text/html; charset=utf-8", DOCS),
            _ => Resp::new(404, "text/plain", "nope"),
        })
        .await;
        let out = run(&format!("{}/docs/guide", base(port))).await;
        assert_eq!(method(&out), "native", "{out}");
        assert!(out.contains("# Getting started"), "{out}");
        assert!(out.contains("cargo add foo"), "{out}");
        assert!(
            out.starts_with(&format!(
                "URL: {}/docs/guide\nContent-Type: text/html\nMethod: native\n\n---\n\n",
                base(port)
            )),
            "{out}"
        );
    }

    #[tokio::test]
    async fn markdown_alternate_wins_when_linked() {
        let md = "# Intro\n\nThis markdown alternate is served by the page's own link tag. It describes the page in enough detail to be accepted as the answer.";
        let port = serve(move |req| match req.path.as_str() {
            "/docs/intro" => Resp::new(
                200,
                "text/html",
                r#"<html><head><link rel="alternate" type="text/markdown" href="/docs/intro.md"></head><body><h1>Intro</h1></body></html>"#,
            ),
            "/docs/intro.md" => Resp::new(200, "text/markdown", md),
            _ => Resp::new(404, "text/plain", ""),
        })
        .await;
        let out = run(&format!("{}/docs/intro", base(port))).await;
        assert_eq!(method(&out), "alternate-markdown", "{out}");
        assert!(out.contains("Notes: Used markdown alternate: "), "{out}");
        assert!(out.contains("# Intro"), "{out}");
    }

    /// `::1` plays the LAN and 127.0.0.1 the public internet.
    #[tokio::test]
    async fn a_link_on_a_public_page_cannot_pull_in_a_lan_url() {
        use crate::net::test_server::{LAN, lan_origin, serve_at};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let md = "# Secret\n\nThis markdown lives on the LAN and is long enough to be accepted as an alternate if it were ever fetched by the page's link.";
        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();
        let lan = serve_at(LAN, move |_| {
            h.fetch_add(1, Ordering::SeqCst);
            Resp::new(200, "text/markdown", md)
        })
        .await;
        let page = move |_: &crate::net::test_server::Req| {
            Resp::new(
                200,
                "text/html",
                format!(
                    r#"<html><head><link rel="alternate" type="text/markdown" href="{}/p.md"></head><body><h1>Public</h1></body></html>"#,
                    lan_origin(lan)
                ),
            )
        };
        let g = Guard { blocked: |_| false, lan: |ip| ip == std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST) };
        let public = serve(page).await;
        let out = fetch(&g, &format!("{}/p", base(public)), false).await.unwrap_or_else(|e| e);
        assert_ne!(method(&out), "alternate-markdown", "{out}");
        assert_eq!(hits.load(Ordering::SeqCst), 0, "the LAN host was contacted");

        // The same page served from the LAN may link to the LAN.
        let lan_page = serve_at(LAN, page).await;
        let out = fetch(&g, &format!("{}/p", lan_origin(lan_page)), false).await.unwrap();
        assert_eq!(method(&out), "alternate-markdown", "{out}");
    }

    #[tokio::test]
    async fn md_suffix_wins_when_served() {
        let md = "# Intro\n\nThe .md suffix version of this page is served by the site. It describes the page in enough detail to be accepted as the answer.";
        let port = serve(move |req| match req.path.as_str() {
            "/docs/intro" => Resp::new(200, "text/html", "<html><body><h1>Intro</h1></body></html>"),
            "/docs/intro.md" => Resp::new(200, "text/plain", md),
            _ => Resp::new(404, "text/plain", ""),
        })
        .await;
        let out = run(&format!("{}/docs/intro", base(port))).await;
        assert_eq!(method(&out), "md-suffix", "{out}");
        assert!(out.contains("Notes: Found .md suffix version"), "{out}");
    }

    #[tokio::test]
    async fn negotiation_wins_when_the_server_answers_markdown() {
        let md = "# Negotiated\n\nThe server returned markdown for the markdown Accept header, which is long enough.";
        let port = serve(move |req| {
            let wants_md = req.header("accept").is_some_and(|a| a.contains("text/markdown"));
            match (req.path.as_str(), wants_md) {
                ("/page", true) => Resp::new(200, "text/markdown; charset=utf-8", md),
                ("/page", false) => Resp::new(200, "text/html", "<html><body><h1>Page</h1></body></html>"),
                _ => Resp::new(404, "text/plain", ""),
            }
        })
        .await;
        let out = run(&format!("{}/page", base(port))).await;
        assert_eq!(method(&out), "content-negotiation", "{out}");
        assert!(out.contains("Content-Type: text/markdown\n"), "{out}");
        assert!(out.contains("Notes: Content negotiation returned text/markdown"), "{out}");
    }

    #[tokio::test]
    async fn json_is_pretty_printed() {
        let port = serve(|req| match req.path.as_str() {
            "/api" => Resp::new(200, "application/json", r#"{"a":1,"b":[1,2]}"#),
            _ => Resp::new(404, "text/plain", ""),
        })
        .await;
        let out = run(&format!("{}/api", base(port))).await;
        assert_eq!(method(&out), "json", "{out}");
        assert!(out.contains("{\n  \"a\": 1,\n  \"b\": [\n    1,\n    2\n  ]\n}"), "{out}");
    }

    #[tokio::test]
    async fn javascript_wall_falls_to_llms_txt() {
        let wall = "<html><body><h1>Example App</h1><p>Please enable JavaScript to use this application. \
            The single page app needs a modern browser to render the content of this page.</p></body></html>";
        let port = serve(move |req| match req.path.as_str() {
            "/docs/app" => Resp::new(200, "text/html", wall),
            "/docs/llms.txt" => Resp::new(200, "text/plain", "# Example App\n\nThe llms.txt summary of the app. It describes the routes, the data model and the build steps in enough detail to be accepted."),
            _ => Resp::new(404, "text/plain", ""),
        })
        .await;
        let out = run(&format!("{}/docs/app", base(port))).await;
        assert_eq!(method(&out), "llms-txt", "{out}");
        assert!(out.contains("Notes: Used llms.txt fallback: "), "{out}");
        assert!(out.contains("The llms.txt summary"), "{out}");
    }

    #[tokio::test]
    async fn javascript_wall_without_llms_keeps_native_with_note() {
        let wall = "<html><body><h1>Example App</h1><p>Please enable JavaScript to use this application. \
            The single page app needs a modern browser to render the content of this page.</p></body></html>";
        let port = serve(move |req| match req.path.as_str() {
            "/docs/app" => Resp::new(200, "text/html", wall),
            _ => Resp::new(404, "text/plain", ""),
        })
        .await;
        let out = run(&format!("{}/docs/app", base(port))).await;
        assert_eq!(method(&out), "native", "{out}");
        assert!(out.contains(JS_NOTE), "{out}");
    }

    #[tokio::test]
    async fn plain_text_passes_through_and_binary_is_summarised() {
        let port = serve(|req| match req.path.as_str() {
            "/notes.txt" => Resp::new(200, "text/plain", "line one\n\n\n\nline two\n"),
            "/blob.bin" => Resp::new(200, "application/x-foo", vec![0u8, 1, 2]),
            _ => Resp::new(404, "text/plain", ""),
        })
        .await;
        let text = run(&format!("{}/notes.txt", base(port))).await;
        assert_eq!(method(&text), "text");
        assert!(text.ends_with("---\n\nline one\n\nline two"), "{text}");
        let bin = run(&format!("{}/blob.bin", base(port))).await;
        assert_eq!(method(&bin), "binary");
        assert!(bin.contains("Binary content (application/x-foo, 3 bytes) not shown."), "{bin}");
    }

    #[tokio::test]
    async fn raw_returns_the_body_verbatim() {
        let port = serve(|_| Resp::new(200, "text/html", DOCS)).await;
        let out = fetch(&Guard::allow_all(), &format!("{}/x", base(port)), true).await.unwrap();
        assert_eq!(method(&out), "raw");
        assert!(out.ends_with(DOCS), "{out}");
    }

    #[tokio::test]
    async fn failures_and_no_content() {
        let port = serve(|req| match req.path.as_str() {
            "/nothing" => Resp::new(204, "text/plain", ""),
            _ => Resp::new(404, "text/plain", "gone"),
        })
        .await;
        let missing = run(&format!("{}/missing", base(port))).await;
        assert_eq!(method(&missing), "failed");
        assert!(missing.contains("Notes: Failed to fetch URL (HTTP 404)"), "{missing}");
        let empty = run(&format!("{}/nothing", base(port))).await;
        assert_eq!(method(&empty), "no-content");
    }

    #[tokio::test]
    async fn internal_address_is_refused_in_the_output() {
        let port = serve(|_| Resp::new(200, "text/plain", "secret")).await;
        let out = fetch(&Guard::default(), &format!("{}/", base(port)), false).await.unwrap_err();
        assert_eq!(method(&out), "failed");
        assert!(out.contains("refusing to fetch 127.0.0.1"), "{out}");
        assert!(!out.contains("secret"), "{out}");
    }

    #[tokio::test]
    async fn rejects_non_http_urls() {
        assert!(fetch(&Guard::allow_all(), "ftp://example.com/x", false).await.is_err());
        assert!(fetch(&Guard::allow_all(), "example.com", false).await.is_err());
    }

    #[test]
    fn llms_candidates_follow_the_scope() {
        let u = |s: &str| Url::parse(s).unwrap();
        assert_eq!(
            llms_candidates(&u("https://x.test/")),
            ["https://x.test/.well-known/llms.txt", "https://x.test/llms.txt", "https://x.test/llms.md"]
        );
        assert_eq!(
            llms_candidates(&u("https://x.test/docs/page")),
            ["https://x.test/docs/llms.txt", "https://x.test/docs/llms.md"]
        );
        assert_eq!(
            llms_candidates(&u("https://x.test/a/b/")),
            [
                "https://x.test/a/b/llms.txt",
                "https://x.test/a/b/llms.md",
                "https://x.test/a/llms.txt",
                "https://x.test/a/llms.md"
            ]
        );
    }

    #[test]
    fn md_suffix_candidates() {
        let u = |s: &str| Url::parse(s).unwrap();
        assert_eq!(md_suffix_url(&u("https://x.test/a/b/?q=1")).unwrap().as_str(), "https://x.test/a/b/index.html.md");
        assert_eq!(md_suffix_url(&u("https://x.test/a/b.html")).unwrap().as_str(), "https://x.test/a/b.html.md");
        assert_eq!(md_suffix_url(&u("https://x.test/a/b?x#y")).unwrap().as_str(), "https://x.test/a/b.md");
    }

    #[test]
    fn finalize_collapses_blank_runs() {
        assert_eq!(finalize("\n a\n\n\n\nb \n"), "a\n\nb");
    }

    #[test]
    fn truncates_on_char_boundary() {
        let s = "é".repeat(OUTPUT_CAP);
        let t = truncate(s);
        assert!(t.ends_with("\n[truncated at 100KB]"));
        assert!(t.len() <= OUTPUT_CAP + 32);
    }
}
