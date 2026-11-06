//! Parallel: the free MCP endpoint (no key) or the REST API on the user's key.

use super::mcp::{self, Failure};
use super::{Config, Params, SearchResponse, Source, trunc};
use serde_json::{Value, json};

const KEYLESS: &str = "https://search.parallel.ai";
const KEYED: &str = "https://api.parallel.ai";
const BETA: &str = "search-extract-2025-10-10";
const RATE_LIMITED: &str =
    "Parallel's free endpoint is rate-limited (429); add a key with `mu auth add parallel` and `mu web use parallel`.";

pub async fn search(cfg: &Config, p: &Params<'_>, origin: Option<&str>) -> Result<SearchResponse, String> {
    let mut sources = if cfg.keyless { keyless(p, origin).await? } else { keyed(cfg, p, origin).await? };
    sources.truncate(p.limit);
    Ok(SearchResponse { sources, ..Default::default() })
}

async fn keyless(p: &Params<'_>, origin: Option<&str>) -> Result<Vec<Source>, String> {
    let url = format!("{}/mcp", origin.unwrap_or(KEYLESS));
    let mut args = json!({"objective": p.query, "search_queries": [p.dated_query()]});
    if let Some(s) = super::SESSION.get().filter(|s| !s.is_empty()) {
        args["session_id"] = json!(s);
    }
    let result = mcp::tools_call(&url, &[("User-Agent", "mu-web")], "web_search", args).await.map_err(|e| match e {
        Failure::Http(429, _) => RATE_LIMITED.to_string(),
        Failure::Http(s, b) => format!("Parallel MCP request failed ({s}): {}", trunc(b.trim(), 300)),
        Failure::Other(m) => format!("Parallel: {m}"),
    })?;
    if !result["structuredContent"].is_null() {
        return parse(&result["structuredContent"]);
    }
    for item in result["content"].as_array().into_iter().flatten() {
        if item["type"] != "text" {
            continue;
        }
        if let Some(payload) = item["text"].as_str().and_then(|t| serde_json::from_str::<Value>(t).ok()) {
            return parse(&payload);
        }
    }
    Ok(Vec::new())
}

async fn keyed(cfg: &Config, p: &Params<'_>, origin: Option<&str>) -> Result<Vec<Source>, String> {
    let key = cfg.key.as_deref().ok_or("no Parallel key was provided")?;
    let base = cfg.base_url.as_deref().or(origin).unwrap_or(KEYED);
    let mut body = json!({
        "objective": p.query,
        "search_queries": [p.query],
        "mode": "fast",
        "excerpts": {"max_chars_per_result": 10000},
    });
    if let Some(d) = p.after() {
        body["source_policy"] = json!({"after_date": d});
    }
    let headers = [("x-api-key", key), ("parallel-beta", BETA)];
    let r = super::post(&format!("{base}/v1beta/search"), &headers, &body).await?;
    if !(200..300).contains(&r.status) {
        return Err(api_error(r.status, &r.body));
    }
    let payload: Value =
        serde_json::from_str(&r.body).map_err(|e| format!("Parallel search returned invalid JSON: {e}"))?;
    parse(&payload)
}

fn api_error(status: u16, body: &str) -> String {
    let detail = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| {
            [&v["error"]["message"], &v["message"], &v["detail"], &v["error"]]
                .iter()
                .find_map(|x| x.as_str().map(String::from))
        })
        .unwrap_or_else(|| body.trim().to_string());
    let detail = trunc(&detail, 300);
    if detail.is_empty() {
        format!("Parallel API error ({status})")
    } else {
        format!("Parallel API error ({status}): {detail}")
    }
}

fn parse(payload: &Value) -> Result<Vec<Source>, String> {
    let results = payload
        .get("results")
        .and_then(Value::as_array)
        .ok_or("Parallel search returned an invalid response payload.")?;
    Ok(results
        .iter()
        .filter_map(|r| {
            let url = r["url"].as_str().filter(|u| !u.is_empty())?;
            let excerpts: Vec<&str> =
                r["excerpts"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
            Some(Source {
                title: r["title"].as_str().filter(|t| !t.is_empty()).unwrap_or(url).to_string(),
                url: url.to_string(),
                snippet: (!excerpts.is_empty()).then(|| excerpts.join("\n\n")),
                published: r["publish_date"].as_str().map(String::from),
            })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::fake;

    fn cfg(keyless: bool) -> Config {
        Config {
            backend: "parallel".into(),
            keyless,
            key: (!keyless).then(|| "par-test-key".into()),
            base_url: None,
            model: None,
        }
    }

    const PAYLOAD: &str = r#"{"results":[{"url":"https://a.example/x","title":"A","publish_date":"2025-01-02","excerpts":["one","two"]},{"title":"no url"},{"url":"https://b.example"}]}"#;

    #[tokio::test]
    async fn the_free_endpoint_answers_over_an_event_stream() {
        let msg = json!({"jsonrpc": "2.0", "id": 1, "result": {"content": [{"type": "text", "text": PAYLOAD}]}});
        let sse = format!("event: message\ndata: {msg}\n\n");
        let (origin, seen) = fake::serve(vec![(200, "text/event-stream", sse)]).await;
        let p = Params { query: "rust crate", recency: Some("week"), limit: 5 };
        let r = search(&cfg(true), &p, Some(&origin)).await.unwrap();
        assert_eq!(r.sources.len(), 2);
        assert_eq!(r.sources[0].snippet.as_deref(), Some("one\n\ntwo"));
        assert_eq!(r.sources[0].published.as_deref(), Some("2025-01-02"));
        assert_eq!(r.sources[1].title, "https://b.example");
        let raw = seen.lock().unwrap()[0].clone();
        let b = fake::body_of(&raw);
        assert_eq!(b["params"]["name"], "web_search");
        assert_eq!(b["params"]["arguments"]["objective"], "rust crate");
        let q = b["params"]["arguments"]["search_queries"][0].as_str().unwrap();
        assert!(q.starts_with("rust crate after:20"), "{q}");
        assert!(!fake::head_of(&raw).contains("x-api-key"));
    }

    #[tokio::test]
    async fn the_free_endpoint_rate_limit_points_at_a_key() {
        let (origin, _) = fake::serve(vec![(429, "text/plain", "slow".into())]).await;
        let p = Params { query: "q", recency: None, limit: 5 };
        let e = search(&cfg(true), &p, Some(&origin)).await.unwrap_err();
        assert_eq!(e, RATE_LIMITED);
    }

    #[tokio::test]
    async fn the_keyed_api_sends_the_key_and_the_beta_header() {
        let (origin, seen) = fake::serve(vec![(200, "application/json", PAYLOAD.into())]).await;
        let p = Params { query: "q", recency: Some("day"), limit: 1 };
        let r = search(&cfg(false), &p, Some(&origin)).await.unwrap();
        assert_eq!(r.sources.len(), 1, "the limit applies");
        let raw = seen.lock().unwrap()[0].clone();
        let head = fake::head_of(&raw);
        assert!(head.starts_with("post /v1beta/search "), "{head}");
        assert!(
            head.contains("x-api-key: par-test-key") && head.contains("parallel-beta: search-extract-2025-10-10"),
            "{head}"
        );
        let b = fake::body_of(&raw);
        assert_eq!((b["mode"].as_str(), b["excerpts"]["max_chars_per_result"].as_u64()), (Some("fast"), Some(10000)));
        assert!(b["source_policy"]["after_date"].as_str().unwrap().starts_with("20"));

        let (origin, _) =
            fake::serve(vec![(401, "application/json", r#"{"error":{"message":"bad key"}}"#.into())]).await;
        let e = search(&cfg(false), &p, Some(&origin)).await.unwrap_err();
        assert_eq!(e, "Parallel API error (401): bad key");
    }
}
