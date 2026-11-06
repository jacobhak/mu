//! OpenAI's web search tool, run through the Responses API on the user's key.

use super::{Citation, Config, Params, SearchResponse, Source, dedupe_push};
use serde_json::{Value, json};

const BASE: &str = "https://api.openai.com/v1";

pub async fn search(cfg: &Config, p: &Params<'_>, origin: Option<&str>) -> Result<SearchResponse, String> {
    let key = cfg.key.as_deref().ok_or("no OpenAI key was provided")?;
    let base = cfg.base_url.as_deref().or(origin).unwrap_or(BASE);
    let model = cfg.model.as_deref().unwrap_or("gpt-5.5");
    let body = json!({
        "model": model,
        "input": p.query,
        "tools": [{"type": "web_search"}],
        "tool_choice": "required",
        "include": ["web_search_call.action.sources"],
        "store": false,
    });
    let bearer = format!("Bearer {key}");
    let r =
        super::post(&format!("{}/responses", base.trim_end_matches('/')), &[("Authorization", &bearer)], &body).await?;
    if !(200..300).contains(&r.status) {
        return Err(http_error(r.status, &r.body));
    }
    let payload: Value = serde_json::from_str(&r.body).map_err(|e| format!("OpenAI returned invalid JSON: {e}"))?;
    let mut out = parse(&payload)?;
    out.sources.truncate(p.limit);
    out.citations.truncate(p.limit);
    Ok(out)
}

/// The status, and the structured error fields. The free-form message is dropped: OpenAI echoes
/// (masked) API keys in 401 messages.
fn http_error(status: u16, body: &str) -> String {
    let base = match status {
        400 => "OpenAI Responses API rejected the request (400)".to_string(),
        401 => "OpenAI API key was rejected (401)".to_string(),
        402 => "OpenAI API billing limit reached (402)".to_string(),
        403 => "OpenAI API request forbidden (403)".to_string(),
        429 => "OpenAI API rate limit reached (429)".to_string(),
        s => format!("OpenAI Responses API request failed ({s})"),
    };
    let fields: Vec<String> = serde_json::from_str::<Value>(body)
        .ok()
        .map(|v| {
            ["type", "code", "param"]
                .iter()
                .filter_map(|f| {
                    v["error"][*f].as_str().map(str::trim).filter(|s| !s.is_empty()).map(|s| format!("{f}={s}"))
                })
                .collect()
        })
        .unwrap_or_default();
    if fields.is_empty() { base } else { format!("{base}: {}", fields.join(", ")) }
}

fn parse(payload: &Value) -> Result<SearchResponse, String> {
    let output = payload["output"].as_array().ok_or("OpenAI Responses API returned an invalid response")?;
    let calls: Vec<&Value> =
        output.iter().filter(|i| i["type"] == "web_search_call" && i["status"] != "failed").collect();
    if calls.is_empty() {
        return Err("OpenAI did not run a web search".into());
    }
    let mut out = SearchResponse::default();
    let mut consulted: Vec<(String, Option<String>)> = Vec::new();
    for c in &calls {
        if let Some(q) = c["action"]["query"].as_str().map(str::trim).filter(|q| !q.is_empty())
            && !out.queries.iter().any(|x| x == q)
        {
            out.queries.push(q.to_string());
        }
        for s in c["action"]["sources"].as_array().into_iter().flatten() {
            if let Some(u) = s["url"].as_str().map(str::trim).filter(|u| !u.is_empty()) {
                consulted.push((u.to_string(), s["title"].as_str().map(String::from)));
            }
        }
    }
    let add = |url: &str, title: Option<&str>, cited: Option<String>, out: &mut SearchResponse| {
        let title = title.map(str::trim).filter(|t| !t.is_empty()).unwrap_or(url).to_string();
        dedupe_push(
            &mut out.sources,
            Source { title: title.clone(), url: url.to_string(), snippet: cited.clone(), published: None },
        );
        match out.citations.iter_mut().find(|c| c.url == url) {
            Some(c) => {
                if c.title.as_deref() == Some(url) && title != url {
                    c.title = Some(title);
                }
                c.cited = c.cited.take().or(cited);
            }
            None => out.citations.push(Citation { url: url.to_string(), title: Some(title), cited }),
        }
    };
    let mut answer: Vec<String> = Vec::new();
    for item in output.iter().filter(|i| i["type"] == "message") {
        for part in item["content"].as_array().into_iter().flatten() {
            let text = part["text"].as_str().unwrap_or_default();
            if part["type"] == "output_text" && !text.trim().is_empty() {
                answer.push(text.trim().to_string());
            }
            for a in part["annotations"].as_array().into_iter().flatten().filter(|a| a["type"] == "url_citation") {
                let Some(url) = a["url"].as_str().map(str::trim).filter(|u| !u.is_empty()) else { continue };
                let cited = match (a["start_index"].as_u64(), a["end_index"].as_u64()) {
                    (Some(s), Some(e)) if e > s => {
                        let t: String = text.chars().skip(s as usize).take((e - s) as usize).collect();
                        Some(t.trim().to_string()).filter(|t| !t.is_empty())
                    }
                    _ => None,
                };
                add(url, a["title"].as_str(), cited, &mut out);
            }
        }
    }
    for (url, title) in &consulted {
        add(url, title.as_deref(), None, &mut out);
    }
    if !answer.is_empty() {
        out.answer = Some(answer.join("\n\n"));
    }
    if out.answer.is_none() && out.sources.is_empty() {
        return Err("OpenAI web search returned no answer or sources".into());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::fake;

    fn cfg() -> Config {
        Config {
            backend: "openai".into(),
            keyless: false,
            key: Some("sk-test-0123456789".into()),
            base_url: None,
            model: None,
        }
    }

    const REPLY: &str = r#"{"output":[
      {"type":"web_search_call","status":"completed","action":{"type":"search","query":"rust pdf crate","sources":[{"type":"url","url":"https://docs.rs/pdf-inspector"},{"type":"url","url":"https://crates.io/crates/pdf-inspector"}]}},
      {"type":"message","content":[{"type":"output_text","text":"Use pdf-inspector for text.","annotations":[
        {"type":"url_citation","url":"https://docs.rs/pdf-inspector","title":"pdf-inspector docs","start_index":4,"end_index":17}]}]}
    ]}"#;

    #[tokio::test]
    async fn the_search_call_message_and_citations_are_read() {
        let (origin, seen) = fake::serve(vec![(200, "application/json", REPLY.into())]).await;
        let p = Params { query: "rust pdf crate", recency: None, limit: 5 };
        let r = search(&cfg(), &p, Some(&origin)).await.unwrap();
        assert_eq!(r.answer.as_deref(), Some("Use pdf-inspector for text."));
        assert_eq!(r.queries, ["rust pdf crate"]);
        assert_eq!(r.sources.len(), 2);
        assert_eq!(r.sources[0].title, "pdf-inspector docs");
        assert_eq!(r.citations[0].cited.as_deref(), Some("pdf-inspector"));
        let raw = seen.lock().unwrap()[0].clone();
        let head = fake::head_of(&raw);
        assert!(
            head.starts_with("post /responses ") && head.contains("authorization: bearer sk-test-0123456789"),
            "{head}"
        );
        let b = fake::body_of(&raw);
        assert_eq!(
            (b["model"].as_str(), b["tool_choice"].as_str(), b["store"].as_bool()),
            (Some("gpt-5.5"), Some("required"), Some(false))
        );
        assert_eq!(b["tools"][0]["type"], "web_search");
        assert_eq!(b["include"][0], "web_search_call.action.sources");
    }

    #[tokio::test]
    async fn a_reply_without_a_search_call_is_an_error() {
        let body = r#"{"output":[{"type":"message","content":[{"type":"output_text","text":"from memory"}]}]}"#;
        let (origin, _) = fake::serve(vec![(200, "application/json", body.into())]).await;
        let p = Params { query: "q", recency: None, limit: 5 };
        assert_eq!(search(&cfg(), &p, Some(&origin)).await.unwrap_err(), "OpenAI did not run a web search");
    }

    #[tokio::test]
    async fn http_errors_never_echo_the_body_message() {
        let body = r#"{"error":{"message":"Incorrect API key provided: sk-test-****6789","type":"invalid_request_error","code":"invalid_api_key"}}"#;
        let (origin, _) = fake::serve(vec![(401, "application/json", body.into())]).await;
        let p = Params { query: "q", recency: None, limit: 5 };
        let e = search(&cfg(), &p, Some(&origin)).await.unwrap_err();
        assert_eq!(e, "OpenAI API key was rejected (401): type=invalid_request_error, code=invalid_api_key");
        assert!(!e.contains("sk-test"));
    }
}
