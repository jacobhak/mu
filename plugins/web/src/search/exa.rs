//! Exa: the free MCP endpoint (no key) or the REST API on the user's key.

use super::mcp::{self, Failure};
use super::{Params, SearchResponse, Source, trunc};
use crate::search::Config;
use serde_json::{Value, json};

const KEYLESS: &str = "https://mcp.exa.ai";
const KEYED: &str = "https://api.exa.ai";

pub async fn search(cfg: &Config, p: &Params<'_>, origin: Option<&str>) -> Result<SearchResponse, String> {
    let mut sources = if cfg.keyless { keyless(p, origin).await? } else { keyed(cfg, p, origin).await? };
    sources.truncate(p.limit);
    Ok(SearchResponse { sources, ..Default::default() })
}

async fn keyless(p: &Params<'_>, origin: Option<&str>) -> Result<Vec<Source>, String> {
    let url = format!("{}/mcp?tools=web_search_exa", origin.unwrap_or(KEYLESS));
    let args = json!({"query": p.dated_query(), "numResults": p.limit});
    let result =
        mcp::tools_call(&url, &[("x-exa-source", "mu")], "web_search_exa", args).await.map_err(|e| match e {
            Failure::Http(429, _) => {
                "Exa's free endpoint is rate-limited (429); add a key with `mu auth add exa` and `mu web use exa`."
                    .to_string()
            }
            Failure::Http(s, b) => format!("Exa MCP request failed ({s}): {}", trunc(b.trim(), 300)),
            Failure::Other(m) => format!("Exa: {m}"),
        })?;
    if let Some(r) = result.get("structuredContent").and_then(parse_json_results) {
        return Ok(r);
    }
    let blocks: Vec<String> = result["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|i| i["text"].as_str())
        .map(|t| t.replace("\r\n", "\n").trim().to_string())
        .filter(|t| !t.is_empty())
        .collect();
    for b in &blocks {
        if let Some(r) = serde_json::from_str::<Value>(b).ok().as_ref().and_then(parse_json_results) {
            return Ok(r);
        }
    }
    Ok(parse_sections(&blocks.join("\n\n")))
}

async fn keyed(cfg: &Config, p: &Params<'_>, origin: Option<&str>) -> Result<Vec<Source>, String> {
    let key = cfg.key.as_deref().ok_or("no Exa key was provided")?;
    let base = cfg.base_url.as_deref().or(origin).unwrap_or(KEYED);
    let mut body = json!({
        "query": p.query,
        "numResults": p.limit,
        "type": "auto",
        "contents": {"summary": {"query": p.query}},
    });
    if let Some(d) = p.after() {
        body["startPublishedDate"] = json!(format!("{d}T00:00:00.000Z"));
    }
    let r = super::post(&format!("{base}/search"), &[("x-api-key", key)], &body).await?;
    if !(200..300).contains(&r.status) {
        let detail = trunc(r.body.trim(), 300);
        return Err(format!(
            "Exa API error ({}){}",
            r.status,
            if detail.is_empty() { String::new() } else { format!(": {detail}") }
        ));
    }
    let payload: Value = serde_json::from_str(&r.body).map_err(|e| format!("Exa returned invalid JSON: {e}"))?;
    parse_json_results(&payload).ok_or_else(|| "Exa returned an unexpected response".to_string())
}

fn parse_json_results(v: &Value) -> Option<Vec<Source>> {
    let results = v.get("results")?.as_array()?;
    Some(
        results
            .iter()
            .filter_map(|r| {
                let url = r["url"].as_str().filter(|u| !u.is_empty())?;
                let highlights = r["highlights"]
                    .as_array()
                    .map(|h| h.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" "))
                    .filter(|s| !s.is_empty());
                let snippet =
                    [r["summary"].as_str().map(String::from), r["text"].as_str().map(String::from), highlights]
                        .into_iter()
                        .flatten()
                        .map(|s| s.trim().to_string())
                        .find(|s| !s.is_empty())
                        .map(|s| trunc(&s, 500));
                Some(Source {
                    title: r["title"].as_str().filter(|t| !t.is_empty()).unwrap_or(url).to_string(),
                    url: url.to_string(),
                    snippet,
                    published: r["publishedDate"].as_str().filter(|d| !d.is_empty()).map(String::from),
                })
            })
            .collect(),
    )
}

/// The `Title:` / `URL:` / `Published Date:` / `Text:` sections the free endpoint prints.
fn parse_sections(text: &str) -> Vec<Source> {
    let field = |section: &str, label: &str| -> Option<String> {
        section
            .lines()
            .find_map(|l| l.strip_prefix(label).and_then(|r| r.strip_prefix(':')))
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    let mut sections: Vec<String> = Vec::new();
    for line in text.lines() {
        if line.starts_with("Title:") {
            sections.push(String::new());
        }
        if let Some(cur) = sections.last_mut() {
            cur.push_str(line);
            cur.push('\n');
        }
    }
    sections
        .iter()
        .filter_map(|s| {
            let url = field(s, "URL")?;
            let body = s.split_once("\nText:").map(|(_, t)| t.trim().to_string()).filter(|t| !t.is_empty());
            Some(Source {
                title: field(s, "Title").unwrap_or_else(|| url.clone()),
                url,
                snippet: body.map(|t| trunc(&t, 500)),
                published: field(s, "Published Date"),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::fake;

    fn cfg(keyless: bool) -> Config {
        Config {
            backend: "exa".into(),
            keyless,
            key: (!keyless).then(|| "exa-test-key".into()),
            base_url: None,
            model: None,
        }
    }

    #[tokio::test]
    async fn the_free_endpoint_text_sections_are_parsed() {
        let text = "Title: First\nURL: https://a.example\nPublished Date: 2025-03-01\nText: line one\nline two\n\nTitle: Second\nURL: https://b.example\nText: second text";
        let msg = json!({"jsonrpc": "2.0", "id": 1, "result": {"content": [{"type": "text", "text": text}]}});
        let (origin, seen) = fake::serve(vec![(200, "application/json", msg.to_string())]).await;
        let p = Params { query: "q", recency: None, limit: 5 };
        let r = search(&cfg(true), &p, Some(&origin)).await.unwrap();
        assert_eq!(r.sources.len(), 2);
        assert_eq!(r.sources[0].title, "First");
        assert_eq!(r.sources[0].published.as_deref(), Some("2025-03-01"));
        assert_eq!(r.sources[0].snippet.as_deref(), Some("line one\nline two"));
        assert_eq!(r.sources[1].published, None);
        let raw = seen.lock().unwrap()[0].clone();
        let head = fake::head_of(&raw);
        assert!(head.starts_with("post /mcp?tools=web_search_exa ") && head.contains("x-exa-source: mu"), "{head}");
        let b = fake::body_of(&raw);
        assert_eq!(
            (b["params"]["name"].as_str(), b["params"]["arguments"]["numResults"].as_u64()),
            (Some("web_search_exa"), Some(5))
        );
    }

    #[tokio::test]
    async fn the_free_endpoint_may_also_answer_with_json_results() {
        let payload = json!({"results": [{"url": "https://a.example", "title": "A", "text": "t"}]}).to_string();
        let msg = json!({"jsonrpc": "2.0", "id": 1, "result": {"content": [{"type": "text", "text": payload}]}});
        let (origin, _) = fake::serve(vec![(200, "application/json", msg.to_string())]).await;
        let p = Params { query: "q", recency: None, limit: 5 };
        let r = search(&cfg(true), &p, Some(&origin)).await.unwrap();
        assert_eq!((r.sources[0].title.as_str(), r.sources[0].snippet.as_deref()), ("A", Some("t")));
    }

    #[tokio::test]
    async fn the_keyed_api_asks_for_summaries_and_prefers_them() {
        let body = json!({"results": [
            {"url": "https://a.example", "title": "A", "summary": "short", "text": "long text", "publishedDate": "2025-05-05"},
            {"url": "https://b.example", "highlights": ["h1", "h2"]}
        ]});
        let (origin, seen) = fake::serve(vec![(200, "application/json", body.to_string())]).await;
        let p = Params { query: "q", recency: Some("month"), limit: 3 };
        let r = search(&cfg(false), &p, Some(&origin)).await.unwrap();
        assert_eq!(r.sources[0].snippet.as_deref(), Some("short"));
        assert_eq!(r.sources[1].snippet.as_deref(), Some("h1 h2"));
        let raw = seen.lock().unwrap()[0].clone();
        let head = fake::head_of(&raw);
        assert!(head.starts_with("post /search ") && head.contains("x-api-key: exa-test-key"), "{head}");
        let b = fake::body_of(&raw);
        assert_eq!((b["type"].as_str(), b["numResults"].as_u64()), (Some("auto"), Some(3)));
        assert_eq!(b["contents"]["summary"]["query"], "q");
        assert!(b["startPublishedDate"].as_str().unwrap().ends_with("T00:00:00.000Z"));
    }
}
