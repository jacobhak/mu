//! One MCP `tools/call` over streamable HTTP, with no `initialize` handshake: the free search
//! endpoints answer a bare call with JSON or an event stream.

use serde_json::{Value, json};

pub enum Failure {
    Http(u16, String),
    Other(String),
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Failure::Http(s, b) => write!(f, "HTTP {s}: {}", super::trunc(b.trim(), 300)),
            Failure::Other(m) => f.write_str(m),
        }
    }
}

const ID: u64 = 1;

/// The `result` of the call. A JSON-RPC error or `isError` result is a failure carrying its text.
pub async fn tools_call(url: &str, headers: &[(&str, &str)], tool: &str, args: Value) -> Result<Value, Failure> {
    let mut h = vec![("Accept", "application/json, text/event-stream")];
    h.extend_from_slice(headers);
    let body = json!({"jsonrpc": "2.0", "id": ID, "method": "tools/call", "params": {"name": tool, "arguments": args}});
    let r = super::post(url, &h, &body).await.map_err(Failure::Other)?;
    if !(200..300).contains(&r.status) {
        return Err(Failure::Http(r.status, r.body));
    }
    let msg = reply_message(&r.content_type, &r.body)
        .ok_or_else(|| Failure::Other(format!("{} sent a reply that is not a tool result", host(url))))?;
    if let Some(e) = msg.get("error") {
        let m = e.get("message").and_then(Value::as_str).unwrap_or("unknown error");
        return Err(Failure::Other(format!("MCP error: {m}")));
    }
    let result = msg.get("result").cloned().unwrap_or(Value::Null);
    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        let text = first_text(&result).unwrap_or_else(|| "the search service returned an error".into());
        return Err(Failure::Other(text));
    }
    Ok(result)
}

fn host(url: &str) -> &str {
    url.split_once("://").map_or(url, |(_, r)| r).split(['/', '?']).next().unwrap_or(url)
}

pub fn first_text(result: &Value) -> Option<String> {
    result["content"]
        .as_array()?
        .iter()
        .find(|i| i["type"] == "text")
        .and_then(|i| i["text"].as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// The JSON-RPC message for our id, from a JSON body or an event stream (`data:` lines).
fn reply_message(content_type: &str, body: &str) -> Option<Value> {
    let mine = |v: &Value| v.get("id").and_then(Value::as_u64) == Some(ID);
    let stream = content_type.contains("event-stream")
        || body.trim_start().starts_with("event:")
        || body.trim_start().starts_with("data:");
    if !stream {
        return serde_json::from_str::<Value>(body).ok().filter(mine);
    }
    // An event's data may span several `data:` lines; events are separated by a blank line.
    let mut events: Vec<String> = Vec::new();
    let mut cur = String::new();
    for line in body.lines() {
        if let Some(d) = line.strip_prefix("data:") {
            if !cur.is_empty() {
                cur.push('\n');
            }
            cur.push_str(d.strip_prefix(' ').unwrap_or(d));
        } else if line.trim().is_empty() && !cur.is_empty() {
            events.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        events.push(cur);
    }
    events.iter().filter_map(|e| serde_json::from_str::<Value>(e).ok()).find(mine)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::fake;

    #[test]
    fn json_and_event_stream_replies_are_matched_on_id() {
        let json = r#"{"jsonrpc":"2.0","id":1,"result":{"a":1}}"#;
        assert_eq!(reply_message("application/json", json).unwrap()["result"]["a"], 1);
        // A notification first, then our reply split over two data lines.
        let sse = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/x\"}\n\nevent: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\ndata: \"result\":{\"a\":2}}\n\n";
        assert_eq!(reply_message("text/event-stream", sse).unwrap()["result"]["a"], 2);
        assert!(reply_message("application/json", r#"{"jsonrpc":"2.0","id":7,"result":{}}"#).is_none());
    }

    #[tokio::test]
    async fn a_call_posts_jsonrpc_and_surfaces_errors() {
        let ok = r#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"hi"}]}}"#;
        let (origin, seen) = fake::serve(vec![(200, "application/json", ok.into())]).await;
        let r =
            tools_call(&format!("{origin}/mcp"), &[("x-exa-source", "mu")], "web_search_exa", json!({"query": "q"}))
                .await
                .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(first_text(&r).as_deref(), Some("hi"));
        let raw = seen.lock().unwrap()[0].clone();
        let head = fake::head_of(&raw);
        assert!(head.starts_with("post /mcp ") && head.contains("x-exa-source: mu"), "{head}");
        assert!(head.contains("accept: application/json, text/event-stream"), "{head}");
        let b = fake::body_of(&raw);
        assert_eq!((b["method"].as_str(), b["params"]["name"].as_str()), (Some("tools/call"), Some("web_search_exa")));
        assert_eq!(b["params"]["arguments"]["query"], "q");

        let err =
            r#"{"jsonrpc":"2.0","id":1,"result":{"isError":true,"content":[{"type":"text","text":"quota used"}]}}"#;
        let (origin, _) = fake::serve(vec![(200, "application/json", err.into())]).await;
        let e = tools_call(&origin, &[], "t", json!({})).await.err().unwrap();
        assert_eq!(e.to_string(), "quota used");
        let (origin, _) = fake::serve(vec![(429, "text/plain", "slow down".into())]).await;
        assert!(matches!(tools_call(&origin, &[], "t", json!({})).await, Err(Failure::Http(429, _))));
    }
}
