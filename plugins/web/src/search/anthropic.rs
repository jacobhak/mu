//! Anthropic's server-side web search tool, run through the Messages API on the user's key.

use super::{Citation, Config, Params, SearchResponse, Source, dedupe_push, trunc};
use serde_json::{Value, json};

const BASE: &str = "https://api.anthropic.com";

pub async fn search(cfg: &Config, p: &Params<'_>, origin: Option<&str>) -> Result<SearchResponse, String> {
    let key = cfg.key.as_deref().ok_or("no Anthropic key was provided")?;
    let base = cfg.base_url.as_deref().or(origin).unwrap_or(BASE);
    let model = cfg.model.as_deref().unwrap_or("claude-haiku-4-5");
    let body = json!({
        "model": model,
        "max_tokens": 2048,
        "messages": [{"role": "user", "content": p.query}],
        "tools": [{"type": "web_search_20250305", "name": "web_search", "max_uses": 3}],
    });
    let headers = [("x-api-key", key), ("anthropic-version", "2023-06-01")];
    let r = super::post(&format!("{base}/v1/messages"), &headers, &body).await?;
    if !(200..300).contains(&r.status) {
        return Err(format!("Anthropic API error ({}): {}", r.status, trunc(r.body.trim(), 300)));
    }
    let payload: Value = serde_json::from_str(&r.body).map_err(|e| format!("Anthropic returned invalid JSON: {e}"))?;
    let mut out = parse(&payload)?;
    out.sources.truncate(p.limit);
    out.citations.truncate(p.limit);
    Ok(out)
}

fn parse(payload: &Value) -> Result<SearchResponse, String> {
    let blocks = payload["content"].as_array().ok_or("Anthropic returned an unexpected response")?;
    let mut out = SearchResponse::default();
    let mut answer: Vec<String> = Vec::new();
    for b in blocks {
        match b["type"].as_str() {
            Some("server_tool_use") if b["name"] == "web_search" => {
                if let Some(q) = b["input"]["query"].as_str().map(str::trim).filter(|q| !q.is_empty()) {
                    out.queries.push(q.to_string());
                }
            }
            Some("web_search_tool_result") => match &b["content"] {
                Value::Array(results) => {
                    for r in results.iter().filter(|r| r["type"] == "web_search_result") {
                        let Some(url) = r["url"].as_str().filter(|u| !u.is_empty()) else { continue };
                        dedupe_push(
                            &mut out.sources,
                            Source {
                                title: r["title"].as_str().filter(|t| !t.is_empty()).unwrap_or(url).to_string(),
                                url: url.to_string(),
                                snippet: None,
                                published: r["page_age"].as_str().map(String::from),
                            },
                        );
                    }
                }
                Value::Object(e) => {
                    let code = e.get("error_code").and_then(Value::as_str).unwrap_or("unknown");
                    out.notes.push(format!("the search tool reported an error: {code}"));
                }
                _ => {}
            },
            Some("text") => {
                if let Some(t) = b["text"].as_str().map(str::trim).filter(|t| !t.is_empty()) {
                    answer.push(t.to_string());
                }
                for c in b["citations"].as_array().into_iter().flatten() {
                    let Some(url) = c["url"].as_str().filter(|u| !u.is_empty()) else { continue };
                    let title = c["title"].as_str().filter(|t| !t.is_empty()).map(String::from);
                    let cited = c["cited_text"].as_str().map(str::trim).filter(|t| !t.is_empty()).map(String::from);
                    match out.citations.iter_mut().find(|x| x.url == url) {
                        Some(x) => {
                            x.title = x.title.take().or(title.clone());
                            x.cited = x.cited.take().or(cited.clone());
                        }
                        None => out.citations.push(Citation {
                            url: url.to_string(),
                            title: title.clone(),
                            cited: cited.clone(),
                        }),
                    }
                    dedupe_push(
                        &mut out.sources,
                        Source {
                            title: title.unwrap_or_else(|| url.to_string()),
                            url: url.to_string(),
                            snippet: cited,
                            published: None,
                        },
                    );
                }
            }
            _ => {}
        }
    }
    if !answer.is_empty() {
        out.answer = Some(answer.join("\n\n"));
    }
    if out.answer.is_none() && out.sources.is_empty() {
        return Err("Anthropic web search returned no answer or sources".into());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::fake;

    fn cfg() -> Config {
        Config {
            backend: "anthropic".into(),
            keyless: false,
            key: Some("ant-test-key".into()),
            base_url: None,
            model: Some("claude-haiku-4-5".into()),
        }
    }

    const REPLY: &str = r#"{"content":[
      {"type":"text","text":"Let me search."},
      {"type":"server_tool_use","id":"s1","name":"web_search","input":{"query":"rust html to markdown"}},
      {"type":"web_search_tool_result","tool_use_id":"s1","content":[
        {"type":"web_search_result","title":"html-to-markdown-rs","url":"https://crates.io/crates/html-to-markdown-rs","page_age":"April 30, 2025","encrypted_content":"x"},
        {"type":"web_search_result","title":"Other","url":"https://other.example"}]},
      {"type":"text","text":"Use html-to-markdown-rs.","citations":[
        {"type":"web_search_result_location","url":"https://crates.io/crates/html-to-markdown-rs","title":"html-to-markdown-rs","cited_text":"A fast converter"}]}
    ]}"#;

    #[tokio::test]
    async fn sources_citations_queries_and_answer_are_read() {
        let (origin, seen) = fake::serve(vec![(200, "application/json", REPLY.into())]).await;
        let p = Params { query: "rust html to markdown", recency: None, limit: 5 };
        let r = search(&cfg(), &p, Some(&origin)).await.unwrap();
        assert_eq!(r.answer.as_deref(), Some("Let me search.\n\nUse html-to-markdown-rs."));
        assert_eq!(r.queries, ["rust html to markdown"]);
        assert_eq!(r.sources.len(), 2);
        assert_eq!(r.sources[0].published.as_deref(), Some("April 30, 2025"));
        assert_eq!(r.sources[0].snippet.as_deref(), Some("A fast converter"));
        assert_eq!(r.citations.len(), 1);
        assert_eq!(r.citations[0].cited.as_deref(), Some("A fast converter"));

        let raw = seen.lock().unwrap()[0].clone();
        let head = fake::head_of(&raw);
        assert!(head.starts_with("post /v1/messages "), "{head}");
        assert!(head.contains("x-api-key: ant-test-key") && head.contains("anthropic-version: 2023-06-01"), "{head}");
        let b = fake::body_of(&raw);
        assert_eq!(b["model"], "claude-haiku-4-5");
        assert_eq!(b["tools"][0]["type"], "web_search_20250305");
        assert_eq!(b["tools"][0]["max_uses"], 3);
        assert_eq!(b["messages"][0]["content"], "rust html to markdown");
    }

    #[tokio::test]
    async fn errors_carry_the_status_and_a_capped_body() {
        let (origin, _) = fake::serve(vec![(400, "application/json", "x".repeat(1000))]).await;
        let p = Params { query: "q", recency: None, limit: 5 };
        let e = search(&cfg(), &p, Some(&origin)).await.unwrap_err();
        assert!(e.starts_with("Anthropic API error (400): xxx") && e.chars().count() < 340, "{e}");
        let (origin, _) = fake::serve(vec![(200, "application/json", r#"{"content":[]}"#.into())]).await;
        assert!(search(&cfg(), &p, Some(&origin)).await.unwrap_err().contains("no answer or sources"));
    }
}
