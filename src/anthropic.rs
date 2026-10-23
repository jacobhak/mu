use crate::message::{Block, Message, StopReason, Usage, now_ms};
use crate::paths;
use anyhow::{Result, anyhow, bail};
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub trait Events {
    fn text(&mut self, s: &str);
    fn thinking(&mut self, s: &str);
}

pub struct Gateway {
    base: String,
    token: String,
    client: Option<reqwest::Client>,
}

impl Gateway {
    pub fn from_env(spawned: Option<&str>) -> Result<Self> {
        let base = match spawned {
            Some(u) => u.to_string(),
            None => std::env::var("MU_GATEWAY_URL")?,
        };
        let token = match std::env::var("MU_GATEWAY_TOKEN") {
            Ok(t) if !t.trim().is_empty() => t.trim().to_string(),
            _ => {
                let p = paths::omp_config_root().join("auth-gateway.token");
                std::fs::read_to_string(&p).map(|s| s.trim().to_string()).map_err(|_| {
                    anyhow!("gateway token not found at {}; run `omp auth-gateway token`", p.display())
                })?
            }
        };
        Ok(Self { base: base.trim_end_matches('/').into(), token, client: None })
    }

    pub fn thinking_budget(level: &str) -> Option<u32> {
        match level {
            "low" => Some(4096),
            "medium" => Some(8192),
            "high" => Some(16384),
            _ => None,
        }
    }

    pub async fn stream(
        &mut self,
        model: &str,
        thinking: &str,
        system: &str,
        tools: &[Value],
        messages: &[Message],
        ev: &mut dyn Events,
    ) -> Result<Message> {
        let client = self.client.get_or_insert_with(reqwest::Client::new);
        let th = match Self::thinking_budget(thinking) {
            Some(b) => json!({"type":"enabled","budget_tokens":b}),
            None => json!({"type":"disabled"}),
        };
        let body = json!({
            "model": model, "max_tokens": 32000, "stream": true,
            "system": [{"type":"text","text":system}],
            "thinking": th, "tools": tools, "messages": to_wire(messages),
        });
        let resp = client
            .post(format!("{}/v1/messages", self.base))
            .bearer_auth(&self.token)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .map_err(|e| anyhow!("cannot reach gateway at {}: {e}; is the gateway running?", self.base))?;
        let status = resp.status();
        if !status.is_success() {
            let t = resp.text().await.unwrap_or_default();
            bail!("HTTP {}: {}", status.as_u16(), t.chars().take(2000).collect::<String>());
        }
        let mut asm = Assembler::new(model);
        let mut bytes = resp.bytes_stream();
        while let Some(chunk) = bytes.next().await {
            let chunk = chunk.map_err(|e| anyhow!("stream error: {e}"))?;
            if let Some(m) = asm.feed(&chunk, ev)? {
                return Ok(m);
            }
        }
        bail!("stream ended early")
    }
}

pub fn to_wire(messages: &[Message]) -> Vec<Value> {
    let mut out: Vec<(String, Vec<Value>)> = Vec::new();
    let mut push = |role: &str, blocks: Vec<Value>| match out.last_mut() {
        Some((r, c)) if r == role => c.extend(blocks),
        _ => out.push((role.to_string(), blocks)),
    };
    for m in messages {
        match m {
            Message::User { content, .. } => {
                let b: Vec<Value> = content
                    .iter()
                    .filter_map(|b| match b {
                        Block::Text { text } => Some(json!({"type":"text","text":text})),
                        _ => None,
                    })
                    .collect();
                if !b.is_empty() {
                    push("user", b);
                }
            }
            Message::Assistant { content, .. } => {
                let mut b = Vec::new();
                for blk in content {
                    match blk {
                        Block::Thinking { thinking, thinking_signature: Some(sig) } => {
                            b.push(json!({"type":"thinking","thinking":thinking,"signature":sig}))
                        }
                        Block::Text { text } if !text.is_empty() => b.push(json!({"type":"text","text":text})),
                        Block::ToolCall { id, name, arguments } => {
                            b.push(json!({"type":"tool_use","id":id,"name":name,"input":arguments}))
                        }
                        _ => {}
                    }
                }
                if !b.is_empty() {
                    push("assistant", b);
                }
            }
            Message::ToolResult { tool_call_id, content, is_error, .. } => {
                let c: Vec<Value> = content
                    .iter()
                    .filter_map(|b| match b {
                        Block::Text { text } => Some(json!({"type":"text","text":text})),
                        _ => None,
                    })
                    .collect();
                push(
                    "user",
                    vec![json!({"type":"tool_result","tool_use_id":tool_call_id,"content":c,"is_error":is_error})],
                );
            }
        }
    }
    out.into_iter().map(|(role, content)| json!({"role":role,"content":content})).collect()
}

enum Open {
    Text(String),
    Thinking(String, Option<String>),
    Tool { id: String, name: String, json: String },
    Ignored,
}

pub struct Assembler {
    model: String,
    buf: Vec<u8>,
    blocks: BTreeMap<u64, Open>,
    done: Vec<Block>,
    usage: Usage,
    stop: StopReason,
    response_id: Option<String>,
}

impl Assembler {
    pub fn new(model: &str) -> Self {
        Self {
            model: model.into(),
            buf: Vec::new(),
            blocks: BTreeMap::new(),
            done: Vec::new(),
            usage: Usage::default(),
            stop: StopReason::Stop,
            response_id: None,
        }
    }

    pub fn feed(&mut self, chunk: &[u8], ev: &mut dyn Events) -> Result<Option<Message>> {
        self.buf.extend_from_slice(chunk);
        while let Some(pos) = self.buf.windows(2).position(|w| w == b"\n\n") {
            let frame: Vec<u8> = self.buf.drain(..pos + 2).collect();
            if let Some(m) = self.frame(&frame, ev)? {
                return Ok(Some(m));
            }
        }
        Ok(None)
    }

    fn frame(&mut self, frame: &[u8], ev: &mut dyn Events) -> Result<Option<Message>> {
        let text = String::from_utf8_lossy(frame);
        let data: Vec<&str> = text
            .lines()
            .filter_map(|l| l.strip_prefix("data:"))
            .map(|d| d.strip_prefix(' ').unwrap_or(d))
            .collect();
        if data.is_empty() {
            return Ok(None);
        }
        let v: Value = serde_json::from_str(&data.join("\n"))?;
        let idx = v["index"].as_u64().unwrap_or(0);
        match v["type"].as_str().unwrap_or("") {
            "message_start" => self.response_id = v["message"]["id"].as_str().map(String::from),
            "content_block_start" => {
                let cb = &v["content_block"];
                let open = match cb["type"].as_str() {
                    Some("text") => Open::Text(String::new()),
                    Some("thinking") => Open::Thinking(String::new(), None),
                    Some("tool_use") => Open::Tool {
                        id: cb["id"].as_str().unwrap_or("").into(),
                        name: cb["name"].as_str().unwrap_or("").into(),
                        json: String::new(),
                    },
                    _ => Open::Ignored,
                };
                self.blocks.insert(idx, open);
            }
            "content_block_delta" => {
                let d = &v["delta"];
                let s = |k: &str| d[k].as_str().unwrap_or("");
                match (self.blocks.get_mut(&idx), d["type"].as_str()) {
                    (Some(Open::Text(t)), Some("text_delta")) => {
                        t.push_str(s("text"));
                        ev.text(s("text"));
                    }
                    (Some(Open::Thinking(t, _)), Some("thinking_delta")) => {
                        t.push_str(s("thinking"));
                        ev.thinking(s("thinking"));
                    }
                    (Some(Open::Thinking(_, sig)), Some("signature_delta")) => *sig = Some(s("signature").into()),
                    (Some(Open::Tool { json, .. }), Some("input_json_delta")) => json.push_str(s("partial_json")),
                    _ => {}
                }
            }
            "content_block_stop" => match self.blocks.remove(&idx) {
                Some(Open::Text(text)) => self.done.push(Block::Text { text }),
                Some(Open::Thinking(thinking, sig)) => {
                    self.done.push(Block::Thinking { thinking, thinking_signature: sig })
                }
                Some(Open::Tool { id, name, json }) => {
                    let arguments = if json.trim().is_empty() {
                        json!({})
                    } else {
                        serde_json::from_str(&json).unwrap_or_else(|_| json!({"__invalid_json": json}))
                    };
                    self.done.push(Block::ToolCall { id, name, arguments });
                }
                _ => {}
            },
            "message_delta" => {
                if let Some(r) = v["delta"]["stop_reason"].as_str() {
                    self.stop = match r {
                        "tool_use" => StopReason::ToolUse,
                        "max_tokens" => StopReason::Length,
                        _ => StopReason::Stop,
                    };
                }
                let u = &v["usage"];
                let g = |k: &str| u[k].as_u64();
                if let Some(x) = g("input_tokens") {
                    self.usage.input = x;
                }
                if let Some(x) = g("output_tokens") {
                    self.usage.output = x;
                }
                if let Some(x) = g("cache_read_input_tokens") {
                    self.usage.cache_read = x;
                }
                if let Some(x) = g("cache_creation_input_tokens") {
                    self.usage.cache_write = x;
                }
                let u = &mut self.usage;
                u.total_tokens = u.input + u.output + u.cache_read + u.cache_write;
            }
            "message_stop" => {
                return Ok(Some(Message::Assistant {
                    content: std::mem::take(&mut self.done),
                    api: "anthropic-messages".into(),
                    provider: self.model.split('/').next().unwrap_or("anthropic").into(),
                    model: self.model.clone(),
                    usage: self.usage.clone(),
                    stop_reason: self.stop,
                    timestamp: now_ms(),
                    response_id: self.response_id.clone(),
                    error_message: None,
                }));
            }
            "error" => bail!("{}", v["error"]["message"].as_str().unwrap_or("unknown stream error")),
            _ => {}
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Null;
    impl Events for Null {
        fn text(&mut self, _: &str) {}
        fn thinking(&mut self, _: &str) {}
    }

    fn sse() -> Vec<u8> {
        let ev = [
            json!({"type":"message_start","message":{"id":"m1"}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"héllo ✓"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"t1","name":"read","input":{}}}),
            json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"pa"}}),
            json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"th\":\"a\"}"}}),
            json!({"type":"content_block_stop","index":1}),
            json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"input_tokens":5,"output_tokens":7}}),
            json!({"type":"message_stop"}),
        ];
        ev.iter().map(|e| format!("event: x\ndata: {e}\n\n")).collect::<String>().into_bytes()
    }

    fn run(chunks: &[&[u8]]) -> Message {
        let mut a = Assembler::new("m");
        let mut out = None;
        for c in chunks {
            if let Some(m) = a.feed(c, &mut Null).unwrap() {
                out = Some(m);
            }
        }
        out.unwrap()
    }

    #[test]
    fn split_stream_equals_whole() {
        let all = sse();
        let whole = run(&[&all]);
        // split at every byte boundary, including mid-UTF-8 and mid-frame
        let singles: Vec<&[u8]> = all.chunks(1).collect();
        let split = run(&singles);
        let (Message::Assistant { content: a, stop_reason: sa, usage: ua, .. }, Message::Assistant { content: b, stop_reason: sb, .. }) =
            (&whole, &split)
        else {
            panic!()
        };
        assert_eq!(serde_json::to_string(a).unwrap(), serde_json::to_string(b).unwrap());
        assert_eq!(sa, sb);
        assert_eq!(*sa, StopReason::ToolUse);
        assert_eq!(ua.total_tokens, 12);
        let Block::ToolCall { arguments, .. } = &a[1] else { panic!() };
        assert_eq!(arguments, &json!({"path":"a"}));
        let Block::Text { text } = &a[0] else { panic!() };
        assert_eq!(text, "héllo ✓");
    }

    fn asst(blocks: Vec<Block>) -> Message {
        Message::Assistant {
            content: blocks,
            api: "anthropic-messages".into(),
            provider: "anthropic".into(),
            model: "m".into(),
            usage: Usage::default(),
            stop_reason: StopReason::ToolUse,
            timestamp: 0,
            response_id: None,
            error_message: None,
        }
    }

    #[test]
    fn wire_merges_results_and_drops_unsigned_thinking() {
        let tr = |id: &str| Message::ToolResult {
            tool_call_id: id.into(),
            tool_name: "read".into(),
            content: vec![Block::Text { text: "ok".into() }],
            is_error: false,
            timestamp: 0,
        };
        let msgs = vec![
            asst(vec![
                Block::Thinking { thinking: "t".into(), thinking_signature: None },
                Block::ToolCall { id: "a".into(), name: "read".into(), arguments: json!({}) },
                Block::ToolCall { id: "b".into(), name: "read".into(), arguments: json!({}) },
            ]),
            tr("a"),
            tr("b"),
            Message::user("next"),
        ];
        let w = to_wire(&msgs);
        assert_eq!(w.len(), 2);
        assert_eq!(w[0]["content"].as_array().unwrap().len(), 2);
        let u = w[1]["content"].as_array().unwrap();
        assert_eq!(u.len(), 3);
        assert_eq!(u[0]["type"], "tool_result");
        assert_eq!(u[2]["type"], "text");
    }
}
