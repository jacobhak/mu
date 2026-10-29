use super::sse::SseDecoder;
use super::{ProviderError, Request, StreamEvent, normalize_id, same_origin, stream_error};
use crate::message::{Block, Message, StopReason, Usage, now_ms};
use crate::models::{ModelSpec, Thinking};
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub fn url(base: &str) -> String {
    format!("{base}/v1/messages")
}

/// The thinking budget must leave room for the answer: at most `max_tokens - 1024`, and
/// Anthropic needs at least 1024. Below that, thinking is turned off for the request.
pub fn clamp_budget(budget: Option<u32>, max_tokens: u32) -> Option<u32> {
    let b = budget?.min(max_tokens.saturating_sub(1024));
    (b >= 1024).then_some(b)
}

/// The gateway takes the full `provider/model` id and adds prompt caching itself.
pub fn body(req: &Request) -> Value {
    let max_tokens = req.max_tokens.unwrap_or(req.model.max_output);
    let level = req.thinking.get();
    let budget = clamp_budget(level.budget(), max_tokens);
    let th = match budget {
        Some(b) => json!({"type":"enabled","budget_tokens":b}),
        None => json!({"type":"disabled"}),
    };
    let tools: Vec<Value> =
        req.tools.iter().map(|t| json!({"name":t.name,"description":t.description,"input_schema":t.schema})).collect();
    let mut body = json!({
        "model": req.model.id,
        "max_tokens": max_tokens,
        "stream": true,
        "system": [{"type":"text","text":req.system}],
        "thinking": th,
        "messages": to_wire(req.messages, req.model),
    });
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools);
    }
    if level != Thinking::Off && budget.is_some() {
        body["output_config"] = json!({"effort": level.as_str()});
    }
    body
}

pub fn to_wire(messages: &[Message], model: &ModelSpec) -> Vec<Value> {
    // Backstop: never send unpaired tool calls/results, whatever the history holds.
    let mut repaired;
    let messages = if crate::message::check_tool_pairs(messages).is_err() {
        repaired = messages.to_vec();
        crate::message::repair_tool_pairs(&mut repaired);
        &repaired[..]
    } else {
        messages
    };
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
                        Block::Text { text } if !text.is_empty() => Some(json!({"type":"text","text":text})),
                        Block::Image(i) => Some(image_json(i)),
                        _ => None,
                    })
                    .collect();
                if !b.is_empty() {
                    push("user", b);
                }
            }
            Message::Assistant { content, api, provider, .. } => {
                let origin = same_origin(api, provider, model);
                let mut b = Vec::new();
                for blk in content {
                    match blk {
                        Block::Thinking { thinking, thinking_signature: Some(sig) } if origin => {
                            b.push(json!({"type":"thinking","thinking":thinking,"signature":sig}))
                        }
                        Block::RedactedThinking { data } if origin => {
                            b.push(json!({"type":"redacted_thinking","data":data}))
                        }
                        Block::Text { text } if !text.is_empty() => b.push(json!({"type":"text","text":text})),
                        Block::ToolCall { id, name, arguments } => {
                            b.push(json!({"type":"tool_use","id":normalize_id(id),"name":name,"input":arguments}))
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
                        Block::Image(i) => Some(image_json(i)),
                        _ => None,
                    })
                    .collect();
                push(
                    "user",
                    vec![
                        json!({"type":"tool_result","tool_use_id":normalize_id(tool_call_id),"content":c,"is_error":is_error}),
                    ],
                );
            }
        }
    }
    out.into_iter().map(|(role, content)| json!({"role":role,"content":content})).collect()
}

/// An image block, its bytes read back from the blob store; a text note if they are gone.
fn image_json(i: &crate::message::Image) -> Value {
    match crate::image::resolve(&i.data) {
        Some(data) => json!({"type":"image","source":{"type":"base64","media_type":i.mime_type,"data":data}}),
        None => json!({"type":"text","text":format!("[image unavailable: {}]", i.mime_type)}),
    }
}

enum Open {
    Text(String),
    Thinking(String, Option<String>),
    Redacted(String),
    Tool { id: String, name: String, json: String },
    Ignored,
}

pub struct Assembler {
    model: String,
    provider: String,
    sse: SseDecoder,
    blocks: BTreeMap<u64, Open>,
    done: Vec<Block>,
    usage: Usage,
    stop: StopReason,
    response_id: Option<String>,
}

impl Assembler {
    pub fn new(model: &ModelSpec) -> Self {
        Self {
            model: model.id.clone(),
            provider: model.provider.clone(),
            sse: SseDecoder::default(),
            blocks: BTreeMap::new(),
            done: Vec::new(),
            usage: Usage::default(),
            stop: StopReason::Stop,
            response_id: None,
        }
    }

    fn frame(&mut self, data: &str, ev: &mut dyn FnMut(StreamEvent)) -> Result<Option<Message>, ProviderError> {
        let v: Value = serde_json::from_str(data).map_err(|e| ProviderError::Fatal(format!("bad stream json: {e}")))?;
        let idx = v["index"].as_u64().unwrap_or(0);
        match v["type"].as_str().unwrap_or("") {
            "message_start" => self.response_id = v["message"]["id"].as_str().map(String::from),
            "content_block_start" => {
                let cb = &v["content_block"];
                let open = match cb["type"].as_str() {
                    Some("text") => Open::Text(String::new()),
                    Some("thinking") => Open::Thinking(String::new(), None),
                    Some("redacted_thinking") => Open::Redacted(cb["data"].as_str().unwrap_or_default().into()),
                    Some("tool_use") => {
                        let id: String = cb["id"].as_str().unwrap_or("").into();
                        let name: String = cb["name"].as_str().unwrap_or("").into();
                        ev(StreamEvent::ToolCallStart { id: &id, name: &name });
                        Open::Tool { id, name, json: String::new() }
                    }
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
                        ev(StreamEvent::Text(s("text")));
                    }
                    (Some(Open::Thinking(t, _)), Some("thinking_delta")) => {
                        t.push_str(s("thinking"));
                        ev(StreamEvent::Thinking(s("thinking")));
                    }
                    (Some(Open::Thinking(_, sig)), Some("signature_delta")) => *sig = Some(s("signature").into()),
                    (Some(Open::Tool { id, json, .. }), Some("input_json_delta")) => {
                        json.push_str(s("partial_json"));
                        ev(StreamEvent::ToolCallDelta { id, json: s("partial_json") });
                    }
                    _ => {}
                }
            }
            "content_block_stop" => match self.blocks.remove(&idx) {
                Some(Open::Text(text)) => self.done.push(Block::text(text)),
                Some(Open::Redacted(data)) => self.done.push(Block::RedactedThinking { data }),
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
                    api: crate::models::API.into(),
                    provider: self.provider.clone(),
                    model: self.model.clone(),
                    usage: self.usage.clone(),
                    stop_reason: self.stop,
                    timestamp: now_ms(),
                    response_id: self.response_id.clone(),
                    error_message: None,
                }));
            }
            "error" => {
                return Err(stream_error(
                    v["error"]["type"].as_str().unwrap_or(""),
                    v["error"]["message"].as_str().unwrap_or("unknown stream error"),
                ));
            }
            _ => {}
        }
        Ok(None)
    }
}

impl Assembler {
    pub fn feed(&mut self, chunk: &[u8], ev: &mut dyn FnMut(StreamEvent)) -> Result<Option<Message>, ProviderError> {
        let frames = self.sse.push(chunk);
        if self.sse.overflow {
            return Err(ProviderError::Fatal("stream frame larger than 16 MB".into()));
        }
        for (_, data) in frames {
            if let Some(m) = self.frame(&data, ev)? {
                return Ok(Some(m));
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(provider: &str) -> ModelSpec {
        ModelSpec { id: format!("{provider}/m"), provider: provider.into(), context_window: 1000, max_output: 100 }
    }

    fn gw() -> ModelSpec {
        spec("anthropic")
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
        let mut a = Assembler::new(&gw());
        let mut out = None;
        for c in chunks {
            if let Some(m) = a.feed(c, &mut |_| {}).unwrap() {
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
        let (
            Message::Assistant { content: a, stop_reason: sa, usage: ua, .. },
            Message::Assistant { content: b, stop_reason: sb, .. },
        ) = (&whole, &split)
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

    #[test]
    fn tool_call_events_stream_args() {
        let mut a = Assembler::new(&gw());
        let mut log: Vec<String> = vec![];
        for c in sse().chunks(1) {
            a.feed(c, &mut |e| match e {
                StreamEvent::ToolCallStart { id, name } => log.push(format!("start:{id}:{name}")),
                StreamEvent::ToolCallDelta { id, json } => log.push(format!("delta:{id}:{json}")),
                _ => {}
            })
            .unwrap();
        }
        assert_eq!(log.iter().filter(|l| l.starts_with("start:")).count(), 1);
        assert_eq!(log[0], "start:t1:read");
        let joined: String = log.iter().filter_map(|l| l.strip_prefix("delta:t1:")).collect();
        assert_eq!(joined, r#"{"path":"a"}"#);
    }

    fn asst(blocks: Vec<Block>) -> Message {
        Message::Assistant {
            content: blocks,
            api: crate::models::API.into(),
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
            content: vec![Block::text("ok")],
            is_error: false,
            timestamp: 0,
            details: None,
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
        let w = to_wire(&msgs, &gw());
        assert_eq!(w.len(), 2);
        assert_eq!(w[0]["content"].as_array().unwrap().len(), 2);
        let u = w[1]["content"].as_array().unwrap();
        assert_eq!(u.len(), 3);
        assert_eq!(u[0]["type"], "tool_result");
        assert_eq!(u[2]["type"], "text");
    }

    #[test]
    fn user_image_follows_text_block() {
        use crate::message::UserInput;
        let input = UserInput {
            text: "look".into(),
            images: vec![crate::message::Image { data: "QUJD".into(), mime_type: "image/png".into() }],
        };
        let w = to_wire(&[Message::user_input(&input)], &gw());
        let c = w[0]["content"].as_array().unwrap();
        assert_eq!(c.len(), 2);
        assert_eq!(c[0]["type"], "text");
        assert_eq!(c[1], json!({"type":"image","source":{"type":"base64","media_type":"image/png","data":"QUJD"}}));
    }

    #[test]
    fn tool_result_carries_its_images() {
        let img = crate::message::Image { data: "QUJD".into(), mime_type: "image/png".into() };
        let msgs = vec![
            asst(vec![Block::ToolCall { id: "a".into(), name: "read".into(), arguments: json!({}) }]),
            Message::ToolResult {
                tool_call_id: "a".into(),
                tool_name: "read".into(),
                content: vec![Block::text("Image: x.png"), Block::Image(img)],
                is_error: false,
                timestamp: 0,
                details: None,
            },
        ];
        let w = to_wire(&msgs, &gw());
        let c = w[1]["content"][0]["content"].as_array().unwrap();
        assert_eq!(c[0]["type"], "text");
        assert_eq!(c[1], json!({"type":"image","source":{"type":"base64","media_type":"image/png","data":"QUJD"}}));
    }
}

#[cfg(test)]
mod budget_tests {
    use super::clamp_budget;

    #[test]
    fn budget_leaves_room_for_the_answer() {
        assert_eq!(clamp_budget(Some(16000), 64000), Some(16000));
        assert_eq!(clamp_budget(Some(16000), 8000), Some(6976));
        assert_eq!(clamp_budget(Some(16000), 2000), None);
        assert_eq!(clamp_budget(None, 64000), None);
    }
}

#[cfg(test)]
mod effort_tests {
    use super::*;
    use crate::provider::Request;
    use std::cell::Cell;
    use std::rc::Rc;

    fn wire(level: Thinking, max_tokens: u32) -> Value {
        let model =
            ModelSpec { id: "anthropic/m".into(), provider: "anthropic".into(), context_window: 1000, max_output: 100 };
        let req = Request {
            model: &model,
            thinking: Rc::new(Cell::new(level)),
            system: "",
            tools: &[],
            messages: &[],
            max_tokens: Some(max_tokens),
        };
        body(&req)
    }

    #[test]
    fn new_levels_reach_the_wire_as_effort() {
        let x = wire(Thinking::XHigh, 100_000);
        assert_eq!(x["output_config"], json!({"effort": "xhigh"}));
        assert_eq!(x["thinking"], json!({"type":"enabled","budget_tokens":32768}));
        let m = wire(Thinking::Max, 100_000);
        assert_eq!(m["output_config"], json!({"effort": "max"}));
        assert_eq!(m["thinking"], json!({"type":"enabled","budget_tokens":64000}));
        assert_eq!(wire(Thinking::Off, 100_000)["thinking"], json!({"type":"disabled"}));
        assert!(wire(Thinking::Off, 100_000).get("output_config").is_none());
    }

    #[test]
    fn max_budget_is_clamped_by_max_tokens() {
        let m = wire(Thinking::Max, 8000);
        assert_eq!(m["thinking"], json!({"type":"enabled","budget_tokens":6976}));
        assert_eq!(m["output_config"], json!({"effort": "max"}));
    }
}

#[cfg(test)]
mod redacted_tests {
    use super::*;

    #[test]
    fn redacted_thinking_round_trips_and_image_only_prompts_have_no_empty_text() {
        let model =
            ModelSpec { id: "anthropic/m".into(), provider: "anthropic".into(), context_window: 1, max_output: 1 };
        let mut asm = Assembler::new(&model);
        let mut ev = |_: StreamEvent| {};
        let sse = "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"redacted_thinking\",\"data\":\"abc\"}}\n\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\ndata: {\"type\":\"message_stop\"}\n\n";
        let msg = asm.feed(sse.as_bytes(), &mut ev).unwrap().expect("message");
        let Message::Assistant { content, .. } = &msg else { panic!() };
        assert!(matches!(&content[0], Block::RedactedThinking { data } if data == "abc"));
        let w = to_wire(&[Message::user("hi"), msg], &model);
        assert_eq!(w[1]["content"][0], json!({"type":"redacted_thinking","data":"abc"}));
        let img = crate::message::UserInput {
            text: String::new(),
            images: vec![crate::message::Image { data: "x".into(), mime_type: "image/png".into() }],
        };
        let w = to_wire(&[Message::user_input(&img)], &model);
        assert_eq!(w[0]["content"].as_array().unwrap().len(), 1);
        assert_eq!(w[0]["content"][0]["type"], "image");
    }
}
