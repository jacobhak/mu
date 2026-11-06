//! OpenAI Responses API wire (`POST {base}/responses`).
//!
//! Replay follows omp so sessions move between the two tools: a reasoning item is kept as the
//! thinking signature (its JSON, verbatim), a tool call id is `call_id|item_id`.

use super::convert::{normalize_id, paired, same_origin, tool_list, unavailable};
use super::usage;
use super::{SseFrames, WireFormat};
use crate::auth::chatgpt;
use crate::provider::retry;
use crate::provider::sse::SseDecoder;
use crate::provider::types::{Decoder, ProviderError, Request, StreamEvent};
use mu_base::message::{Block, Message, StopReason, Usage, now_ms};
use mu_base::models::{ModelSpec, Thinking, ThinkingMode, clamp_effort};
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub struct Responses;

impl WireFormat for Responses {
    fn url(&self, base: &str) -> String {
        format!("{base}/responses")
    }

    fn body(&self, req: &Request) -> Value {
        body(req)
    }

    fn decoder(&self, req: &Request) -> Box<dyn Decoder> {
        Box::new(Assembler::new(req.model))
    }
}

/// Only reasoning models take `reasoning`. `off` sends none, as omp does.
fn reasoning_effort(model: &ModelSpec, level: Thinking) -> Option<&str> {
    if model.thinking.mode != ThinkingMode::Effort {
        return None;
    }
    clamp_effort(level, &model.thinking.efforts)
}

pub fn body(req: &Request) -> Value {
    let tools = tool_list(
        req.tools,
        |name, description, schema| json!({"type":"function","name":name,"description":description,"parameters":schema,"strict":false}),
    );
    let mut body = json!({
        "model": req.model.wire_id,
        "input": to_wire(req.messages, req.model),
        "stream": true,
        "store": false,
    });
    // The ChatGPT plan refuses `max_output_tokens`, as it refuses the other limit and sampling fields.
    if req.model.provider != chatgpt::PROVIDER_ID {
        body["max_output_tokens"] = json!(req.max_tokens.unwrap_or(req.model.max_output));
    }
    if !req.system.is_empty() {
        body["instructions"] = json!(req.system);
    }
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools);
    }
    // Encrypted reasoning only exists for reasoning models; the key is omitted when there is no session.
    if req.model.thinking.mode == ThinkingMode::Effort {
        body["include"] = json!(["reasoning.encrypted_content"]);
    }
    if let Some(key) = req.session_id.filter(|k| !k.is_empty()) {
        body["prompt_cache_key"] = json!(key);
    }
    if let Some(effort) = reasoning_effort(req.model, req.thinking.get()) {
        body["reasoning"] = json!({"effort": effort, "summary": "auto"});
    }
    body
}

pub fn to_wire(messages: &[Message], model: &ModelSpec) -> Vec<Value> {
    let messages = paired(messages);
    let mut out = Vec::new();
    for (idx, m) in messages.iter().enumerate() {
        match m {
            Message::User { content, .. } => {
                let parts: Vec<Value> = content
                    .iter()
                    .filter_map(|b| match b {
                        Block::Text { text } if !text.is_empty() => Some(json!({"type":"input_text","text":text})),
                        Block::Image(i) => Some(input_image(i)),
                        _ => None,
                    })
                    .collect();
                if !parts.is_empty() {
                    out.push(json!({"role":"user","content":parts}));
                }
            }
            Message::Assistant { content, api, provider, model: from, stop_reason, .. } => {
                // Reasoning replays only into the model that produced it, and never from an errored turn.
                let same_model = same_origin(api, provider, model) && from == &model.id;
                assistant_items(idx, content, same_model && *stop_reason != StopReason::Error, &mut out);
            }
            Message::ToolResult { tool_call_id, content, .. } => out.push(json!({
                "type":"function_call_output",
                "call_id": split_call_id(tool_call_id).0,
                "output": tool_output(content, model),
            })),
        }
    }
    out
}

/// `call_id|item_id` as mu and omp store a Responses tool call id.
fn split_call_id(id: &str) -> (String, Option<String>) {
    match id.split_once('|') {
        Some((call, item)) => (normalize_id(call), Some(normalize_id(item)).filter(|s| !s.is_empty())),
        None => (normalize_id(id), None),
    }
}

/// A reasoning item saved as a thinking signature. Anthropic signatures and Chat Completions field
/// names are not JSON items, so they never parse here.
fn reasoning_item(sig: &str) -> Option<Value> {
    let v: Value = serde_json::from_str(sig).ok()?;
    (v["type"] == "reasoning" && v["id"].is_string()).then_some(v)
}

/// `replayable`: the turn came from this model and is not an errored one.
fn assistant_items(idx: usize, content: &[Block], replayable: bool, out: &mut Vec<Value>) {
    let replay = replayable
        && content
            .iter()
            .any(|b| matches!(b, Block::Thinking { thinking_signature: Some(s), .. } if reasoning_item(s).is_some()));
    let mut items = Vec::new();
    let mut texts = 0;
    for blk in content {
        match blk {
            Block::Thinking { thinking_signature: Some(sig), .. } if replayable => items.extend(reasoning_item(sig)),
            Block::Text { text } if !text.is_empty() => {
                let mut msg = json!({
                    "type":"message","role":"assistant",
                    "content":[{"type":"output_text","text":text,"annotations":[]}],
                    "status":"completed",
                });
                // Item ids are only sent next to replayed reasoning, as omp does.
                if replay {
                    msg["id"] = json!(if texts == 0 { format!("msg_{idx}") } else { format!("msg_{idx}_{texts}") });
                    texts += 1;
                }
                items.push(msg);
            }
            Block::ToolCall { id, name, arguments } => {
                let (call_id, item_id) = split_call_id(id);
                let mut call =
                    json!({"type":"function_call","call_id":call_id,"name":name,"arguments":arguments_json(arguments)});
                if let Some(item_id) = item_id.filter(|_| replay) {
                    call["id"] = json!(item_id);
                }
                items.push(call);
            }
            _ => {}
        }
    }
    // A reasoning item is only valid followed by its output in the same turn.
    while items.last().is_some_and(|v| v["type"] == "reasoning") {
        items.pop();
    }
    out.extend(items);
}

/// The arguments as the model streamed them: an unparsable call goes back as its raw text.
fn arguments_json(arguments: &Value) -> String {
    match arguments.get("__invalid_json").and_then(Value::as_str) {
        Some(raw) => raw.to_string(),
        None => arguments.to_string(),
    }
}

fn input_image(i: &mu_base::message::Image) -> Value {
    match mu_base::image::resolve(&i.data) {
        Some(data) => {
            json!({"type":"input_image","detail":"auto","image_url":format!("data:{};base64,{data}", i.mime_type)})
        }
        None => json!({"type":"input_text","text":unavailable(i)}),
    }
}

fn tool_output(content: &[Block], model: &ModelSpec) -> Value {
    let has_images = content.iter().any(|b| matches!(b, Block::Image(_)));
    if has_images && model.images {
        return Value::Array(
            content
                .iter()
                .filter_map(|b| match b {
                    Block::Text { text } => Some(json!({"type":"input_text","text":text})),
                    Block::Image(i) => Some(input_image(i)),
                    _ => None,
                })
                .collect(),
        );
    }
    let text: Vec<String> = content
        .iter()
        .filter_map(|b| match b {
            Block::Text { text } => Some(text.clone()),
            Block::Image(i) => Some(format!("[image omitted: {}]", i.mime_type)),
            _ => None,
        })
        .collect();
    json!(text.join("\n"))
}

enum Open {
    Text(String),
    Reasoning(String),
    Tool { id: String, name: String, json: String },
}

pub struct Assembler {
    model: String,
    provider: String,
    api: &'static str,
    sse: SseDecoder,
    /// Items still streaming, keyed by `output_index`.
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
            api: model.api.as_str(),
            sse: SseDecoder::default(),
            blocks: BTreeMap::new(),
            done: Vec::new(),
            usage: Usage::default(),
            stop: StopReason::Stop,
            response_id: None,
        }
    }

    pub fn feed(&mut self, chunk: &[u8], ev: &mut dyn FnMut(StreamEvent)) -> Result<Option<Message>, ProviderError> {
        super::feed(self, chunk, ev)
    }

    fn frame(&mut self, data: &str, ev: &mut dyn FnMut(StreamEvent)) -> Result<Option<Message>, ProviderError> {
        let v: Value = serde_json::from_str(data).map_err(|e| ProviderError::Fatal(format!("bad stream json: {e}")))?;
        let idx = v["output_index"].as_u64().unwrap_or(0);
        match v["type"].as_str().unwrap_or("") {
            "response.created" => self.response_id = v["response"]["id"].as_str().map(String::from),
            "response.output_item.added" => {
                let item = &v["item"];
                let open = match item["type"].as_str() {
                    Some("message") => Some(Open::Text(String::new())),
                    Some("reasoning") => Some(Open::Reasoning(String::new())),
                    Some("function_call") => {
                        let id = tool_id(item);
                        let name = item["name"].as_str().unwrap_or("").to_string();
                        ev(StreamEvent::ToolCallStart { id: &id, name: &name });
                        Some(Open::Tool { id, name, json: String::new() })
                    }
                    _ => None,
                };
                if let Some(open) = open {
                    self.blocks.insert(idx, open);
                }
            }
            "response.output_text.delta" | "response.refusal.delta" => {
                let d = v["delta"].as_str().unwrap_or("");
                if let Open::Text(t) = self.blocks.entry(idx).or_insert_with(|| Open::Text(String::new())) {
                    t.push_str(d);
                    ev(StreamEvent::Text(d));
                }
            }
            "response.reasoning_summary_text.delta" => {
                let d = v["delta"].as_str().unwrap_or("");
                if let Open::Reasoning(t) = self.blocks.entry(idx).or_insert_with(|| Open::Reasoning(String::new())) {
                    t.push_str(d);
                    ev(StreamEvent::Thinking(d));
                }
            }
            // Summary parts are joined by a blank line, as the finished item's summary is.
            "response.reasoning_summary_part.added" => {
                if let Some(Open::Reasoning(t)) = self.blocks.get_mut(&idx)
                    && !t.is_empty()
                {
                    t.push_str("\n\n");
                    ev(StreamEvent::Thinking("\n\n"));
                }
            }
            "response.function_call_arguments.delta" => {
                let d = v["delta"].as_str().unwrap_or("");
                if let Some(Open::Tool { id, json, .. }) = self.blocks.get_mut(&idx) {
                    json.push_str(d);
                    ev(StreamEvent::ToolCallDelta { id, json: d });
                }
            }
            // The finished arguments replace the streamed buffer; a blank one leaves it.
            "response.function_call_arguments.done" => {
                if let Some(Open::Tool { json, .. }) = self.blocks.get_mut(&idx)
                    && let Some(args) = v["arguments"].as_str().filter(|a| !a.is_empty())
                {
                    *json = args.to_string();
                }
            }
            "response.output_item.done" => {
                let open = self.blocks.remove(&idx);
                self.done.extend(finish(open, &v["item"]));
            }
            "response.completed" | "response.incomplete" | "response.done" => {
                return self.complete(&v["response"]).map(Some);
            }
            "response.failed" => return Err(failed_response(&v["response"])),
            "error" => {
                let e = if v["error"].is_object() { &v["error"] } else { &v };
                return Err(stream_failure(
                    e["code"].as_str().unwrap_or(""),
                    e["message"].as_str().unwrap_or("unknown stream error"),
                    e["param"].as_str().unwrap_or(""),
                ));
            }
            _ => {}
        }
        Ok(None)
    }

    fn complete(&mut self, r: &Value) -> Result<Message, ProviderError> {
        // Items the stream never closed (no `output_item.done`) are kept, in index order.
        for (_, open) in std::mem::take(&mut self.blocks) {
            self.done.extend(finish(Some(open), &Value::Null));
        }
        // Nothing streamed: the terminal event carries the whole output.
        if self.done.is_empty()
            && let Some(items) = r["output"].as_array()
        {
            self.done.extend(items.iter().filter_map(|item| finish(None, item)));
        }
        if let Some(id) = r["id"].as_str() {
            self.response_id = Some(id.into());
        }
        if !r["usage"].is_null() {
            self.usage = usage::responses_usage(&r["usage"]);
        }
        let has_calls = self.done.iter().any(|b| matches!(b, Block::ToolCall { .. }));
        let calls_parse = !self
            .done
            .iter()
            .any(|b| matches!(b, Block::ToolCall { arguments, .. } if arguments.get("__invalid_json").is_some()));
        self.stop = match r["status"].as_str().unwrap_or("completed") {
            "failed" | "cancelled" => return Err(failed_response(r)),
            // A truncated turn whose calls all parsed still runs them, as omp does.
            "incomplete" => match r["incomplete_details"]["reason"].as_str() {
                Some("max_output_tokens") if has_calls && calls_parse => StopReason::ToolUse,
                Some("max_output_tokens") => StopReason::Length,
                reason => return Err(ProviderError::Fatal(format!("incomplete: {}", reason.unwrap_or("unknown")))),
            },
            _ if has_calls => StopReason::ToolUse,
            _ => StopReason::Stop,
        };
        Ok(Message::Assistant {
            content: std::mem::take(&mut self.done),
            api: self.api.into(),
            provider: self.provider.clone(),
            model: self.model.clone(),
            usage: self.usage.clone(),
            stop_reason: self.stop,
            timestamp: now_ms(),
            response_id: self.response_id.clone(),
            error_message: None,
        })
    }
}

impl Decoder for Assembler {
    fn feed(&mut self, chunk: &[u8], ev: &mut dyn FnMut(StreamEvent)) -> Result<Option<Message>, ProviderError> {
        Assembler::feed(self, chunk, ev)
    }

    fn usage(&self) -> Usage {
        self.usage.clone()
    }
}

impl SseFrames for Assembler {
    fn sse(&mut self) -> &mut SseDecoder {
        &mut self.sse
    }

    fn frame(&mut self, data: &str, ev: &mut dyn FnMut(StreamEvent)) -> Result<Option<Message>, ProviderError> {
        Assembler::frame(self, data, ev)
    }
}

/// The id mu stores for a function call: `call_id|item_id`.
fn tool_id(item: &Value) -> String {
    let call = item["call_id"].as_str().unwrap_or("");
    match item["id"].as_str() {
        Some(item_id) if !item_id.is_empty() => format!("{call}|{item_id}"),
        _ => call.into(),
    }
}

/// The block for one finished item. `open` is what streamed; `item` is the final item, `Null` when
/// the stream ended without one.
fn finish(open: Option<Open>, item: &Value) -> Option<Block> {
    match open {
        Some(Open::Reasoning(streamed)) => Some(reasoning_block(streamed, item)),
        Some(Open::Text(streamed)) => text_block(streamed, item),
        Some(Open::Tool { id, name, json }) => Some(tool_block(id, name, json, item)),
        None => match item["type"].as_str()? {
            "reasoning" => Some(reasoning_block(String::new(), item)),
            "message" => text_block(String::new(), item),
            "function_call" => Some(tool_block(tool_id(item), String::new(), String::new(), item)),
            _ => None,
        },
    }
}

/// Thinking shows the summary; the signature keeps the whole item for replay.
fn reasoning_block(streamed: String, item: &Value) -> Block {
    let thinking = [texts_of(&item["summary"], "\n\n"), texts_of(&item["content"], "\n\n")]
        .into_iter()
        .find(|s| !s.is_empty())
        .unwrap_or(streamed);
    Block::Thinking { thinking, thinking_signature: (!item.is_null()).then(|| item.to_string()) }
}

fn text_block(streamed: String, item: &Value) -> Option<Block> {
    let text = if streamed.is_empty() { texts_of(&item["content"], "") } else { streamed };
    (!text.is_empty()).then(|| Block::text(text))
}

fn tool_block(id: String, name: String, streamed: String, item: &Value) -> Block {
    let name = if name.is_empty() { item["name"].as_str().unwrap_or("").to_string() } else { name };
    let args = item["arguments"].as_str().filter(|s| !s.is_empty()).map_or(streamed, str::to_owned);
    let arguments = if args.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str(&args).unwrap_or_else(|_| json!({"__invalid_json": args}))
    };
    Block::ToolCall { id, name, arguments }
}

/// The non-empty `text` fields of an array of parts, joined by `sep`.
fn texts_of(parts: &Value, sep: &str) -> String {
    parts
        .as_array()
        .map(|a| {
            // A refusal part carries its text under `refusal`; it is shown as text.
            a.iter()
                .filter_map(|p| p["text"].as_str().or(p["refusal"].as_str()))
                .filter(|t| !t.is_empty())
                .collect::<Vec<_>>()
                .join(sep)
        })
        .unwrap_or_default()
}

/// Maps an in-stream error by OpenAI's error code.
fn stream_failure(code: &str, msg: &str, param: &str) -> ProviderError {
    if let Some((retryable, text)) = retry::chatgpt_text(code, msg, param) {
        return if retryable {
            ProviderError::Retryable { msg: text, retry_after: None }
        } else {
            ProviderError::Fatal(text)
        };
    }
    match code {
        "rate_limit_exceeded" | "server_error" => ProviderError::Retryable { msg: msg.into(), retry_after: None },
        "context_length_exceeded" => ProviderError::Overflow(msg.into()),
        _ => ProviderError::Fatal(format!("{}: {msg}", if code.is_empty() { "unknown" } else { code })),
    }
}

fn failed_response(r: &Value) -> ProviderError {
    let e = &r["error"];
    if e.is_object() {
        return stream_failure(
            e["code"].as_str().unwrap_or(""),
            e["message"].as_str().unwrap_or("no message"),
            e["param"].as_str().unwrap_or(""),
        );
    }
    match r["incomplete_details"]["reason"].as_str() {
        Some(reason) => ProviderError::Fatal(format!("incomplete: {reason}")),
        None => ProviderError::Fatal("response failed without error details".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::client::Route;
    use crate::provider::types::Auth;
    use mu_base::models::{Api, Thinking, ThinkingSpec};
    use mu_base::tool::ToolDef;
    use std::cell::Cell;
    use std::rc::Rc;

    /// The parts a request is built from, as the tests read them.
    struct Built {
        url: String,
        headers: Vec<(&'static str, String)>,
        body: Value,
        decoder: Box<dyn Decoder>,
    }

    fn wire(req: &Request, route: &Route) -> Result<Built, ProviderError> {
        Ok(Built {
            url: Responses.url(&route.base),
            headers: Responses.headers(req),
            body: Responses.body(req),
            decoder: Responses.decoder(req),
        })
    }

    const REASONING: &str = r#"{"type":"reasoning","id":"rs_0a1b2c","summary":[{"type":"summary_text","text":"Need to read the file."}],"encrypted_content":"gAAAAABfakeEncryptedReasoning","status":"completed"}"#;

    fn model(reasoning: bool) -> ModelSpec {
        ModelSpec {
            id: "openai/gpt-5.5".into(),
            provider: "openai".into(),
            context_window: 400_000,
            max_output: 1000,
            api: Api::OpenAiResponses,
            native: true,
            wire_id: "gpt-5.5".into(),
            thinking: ThinkingSpec {
                mode: if reasoning { ThinkingMode::Effort } else { ThinkingMode::None },
                efforts: ["minimal", "low", "medium", "high", "xhigh"].map(String::from).to_vec(),
            },
            images: true,
            compat: None,
            anthropic: Default::default(),
        }
    }

    fn request<'a>(
        model: &'a ModelSpec,
        level: Thinking,
        messages: &'a [Message],
        tools: &'a [ToolDef],
    ) -> Request<'a> {
        Request {
            model,
            thinking: Rc::new(Cell::new(level)),
            system: "sys",
            tools,
            messages,
            max_tokens: None,
            session_id: Some("sess-1"),
        }
    }

    fn assistant(content: Vec<Block>, api: &str, provider: &str) -> Message {
        Message::Assistant {
            content,
            api: api.into(),
            provider: provider.into(),
            model: "openai/gpt-5.5".into(),
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            timestamp: 0,
            response_id: None,
            error_message: None,
        }
    }

    fn tool_result(id: &str, content: Vec<Block>) -> Message {
        Message::ToolResult {
            tool_call_id: id.into(),
            tool_name: "read".into(),
            content,
            is_error: false,
            timestamp: 0,
            details: None,
        }
    }

    fn read_tool() -> ToolDef {
        ToolDef {
            name: "read".into(),
            description: "Read a file".into(),
            schema: json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}),
        }
    }

    #[test]
    fn golden_body_reasoning_with_tools_and_session() {
        let m = model(true);
        let msgs = [Message::user("hi")];
        let tools = [read_tool()];
        let body = body(&request(&m, Thinking::High, &msgs, &tools));
        assert_eq!(
            body,
            json!({
                "model": "gpt-5.5",
                "instructions": "sys",
                "input": [{"role":"user","content":[{"type":"input_text","text":"hi"}]}],
                "stream": true,
                "store": false,
                "include": ["reasoning.encrypted_content"],
                "max_output_tokens": 1000,
                "tools": [{
                    "type":"function","name":"read","description":"Read a file",
                    "parameters": read_tool().schema,"strict":false
                }],
                "prompt_cache_key": "sess-1",
                "reasoning": {"effort":"high","summary":"auto"},
            })
        );
    }

    #[test]
    fn golden_body_off_or_non_reasoning_sends_no_reasoning() {
        let msgs = [Message::user("hi")];
        let m = model(true);
        let mut req = request(&m, Thinking::Off, &msgs, &[]);
        req.session_id = None;
        let out = body(&req);
        assert!(out.get("reasoning").is_none());
        assert!(out.get("tools").is_none());
        assert!(out.get("prompt_cache_key").is_none());
        let plain = model(false);
        assert!(body(&request(&plain, Thinking::High, &msgs, &[])).get("reasoning").is_none());
    }

    #[test]
    fn the_plan_body_drops_what_it_refuses_and_its_errors_are_named() {
        let mut m = model(true);
        m.id = "openai-chatgpt/gpt-5.5".into();
        m.provider = "openai-chatgpt".into();
        let msgs = [Message::user("hi")];
        let tools = [read_tool()];
        let b = body(&request(&m, Thinking::High, &msgs, &tools));
        assert!(b.get("max_output_tokens").is_none(), "{b}");
        assert_eq!((&b["store"], &b["stream"]), (&json!(false), &json!(true)));
        assert_eq!(
            (&b["tools"][0]["strict"], &b["include"][0]),
            (&json!(false), &json!("reasoning.encrypted_content"))
        );
        assert!(!b.to_string().contains("\"role\":\"system\""), "{b}");
        // The usage limit arrives as a failed response mid-stream.
        let failed = "data: {\"type\":\"response.failed\",\"response\":{\"status\":\"failed\",\"error\":{\"code\":\"subscription_sharing_usage_limit_exceeded\",\"message\":\"limit\"}}}\n\n";
        let err = Assembler::new(&m).feed(failed.as_bytes(), &mut |_: StreamEvent| {}).unwrap_err().to_string();
        assert!(err.contains("https://chatgpt.com/settings/usage"), "{err}");
        let unsupported = "data: {\"type\":\"error\",\"error\":{\"code\":\"subscription_sharing_unsupported_capability\",\"param\":\"tools[0].type\",\"message\":\"no\"}}\n\n";
        let err = Assembler::new(&m).feed(unsupported.as_bytes(), &mut |_: StreamEvent| {}).unwrap_err().to_string();
        assert!(err.contains("tools[0].type"), "{err}");
        // An API-key route keeps its limit field.
        assert!(body(&request(&model(true), Thinking::High, &msgs, &[])).get("max_output_tokens").is_some());
    }

    #[test]
    fn golden_body_effort_clamps_to_the_model_ladder() {
        let mut m = model(true);
        m.thinking.efforts = vec!["low".into(), "medium".into()];
        let msgs = [Message::user("hi")];
        let effort = |level| body(&request(&m, level, &msgs, &[]))["reasoning"]["effort"].clone();
        assert_eq!(effort(Thinking::XHigh), json!("medium"));
        assert_eq!(effort(Thinking::Low), json!("low"));
    }

    #[test]
    fn golden_body_images_in_user_and_tool_output() {
        mu_testutil::home();
        let img = mu_base::message::Image { data: "iVBORw0KGgo=".into(), mime_type: "image/png".into() };
        let msgs = [
            Message::user_input(&mu_base::message::UserInput {
                text: "look".into(),
                images: vec![img.clone()],
                command: None,
            }),
            assistant(
                vec![Block::ToolCall { id: "call_1|fc_1".into(), name: "read".into(), arguments: json!({"path":"a"}) }],
                "openai-responses",
                "openai",
            ),
            tool_result("call_1|fc_1", vec![Block::text("5 cm"), Block::Image(img.clone())]),
        ];
        let m = model(true);
        let input = body(&request(&m, Thinking::Off, &msgs, &[]))["input"].clone();
        let url = "data:image/png;base64,iVBORw0KGgo=";
        assert_eq!(input[0]["content"][1], json!({"type":"input_image","detail":"auto","image_url":url}));
        assert_eq!(
            input[2]["output"],
            json!([{"type":"input_text","text":"5 cm"},{"type":"input_image","detail":"auto","image_url":url}])
        );

        let mut text_only = model(true);
        text_only.images = false;
        let input = body(&request(&text_only, Thinking::Off, &msgs, &[]))["input"].clone();
        assert_eq!(input[2]["output"], json!("5 cm\n[image omitted: image/png]"));
    }

    #[test]
    fn replay_reasoning_and_pairs_across_turns() {
        let m = model(true);
        let msgs = vec![
            Message::user("What's in a.txt?"),
            assistant(
                vec![
                    Block::Thinking {
                        thinking: "Need to read the file.".into(),
                        thinking_signature: Some(REASONING.into()),
                    },
                    Block::ToolCall {
                        id: "call_Xy12|fc_0a1b2c".into(),
                        name: "read".into(),
                        arguments: json!({"path":"a.txt"}),
                    },
                ],
                "openai-responses",
                "openai",
            ),
            tool_result("call_Xy12|fc_0a1b2c", vec![Block::text("hello")]),
            assistant(vec![Block::text("It says hello.")], "openai-responses", "openai"),
            Message::user("Thanks"),
        ];
        let input = Value::Array(to_wire(&msgs, &m));
        assert_eq!(
            input,
            json!([
                {"role":"user","content":[{"type":"input_text","text":"What's in a.txt?"}]},
                serde_json::from_str::<Value>(REASONING).unwrap(),
                {"type":"function_call","id":"fc_0a1b2c","call_id":"call_Xy12","name":"read","arguments":"{\"path\":\"a.txt\"}"},
                {"type":"function_call_output","call_id":"call_Xy12","output":"hello"},
                {"type":"message","role":"assistant","content":[{"type":"output_text","text":"It says hello.","annotations":[]}],"status":"completed"},
                {"role":"user","content":[{"type":"input_text","text":"Thanks"}]},
            ])
        );
    }

    #[test]
    fn foreign_reasoning_is_not_replayed() {
        let m = model(true);
        let msgs = vec![
            assistant(
                vec![
                    Block::Thinking { thinking: "x".into(), thinking_signature: Some("EqQBCkYIARgCIiQ=".into()) },
                    Block::Thinking { thinking: "y".into(), thinking_signature: Some(REASONING.into()) },
                    Block::text("answer"),
                    Block::ToolCall { id: "toolu_01A".into(), name: "read".into(), arguments: json!({}) },
                ],
                "anthropic-messages",
                "anthropic",
            ),
            tool_result("toolu_01A", vec![Block::text("ok")]),
            // Same provider, other wire: its reasoning JSON is not an item for this API either.
            assistant(
                vec![
                    Block::Thinking { thinking: "y".into(), thinking_signature: Some(REASONING.into()) },
                    Block::ToolCall { id: "call_1|fc_1".into(), name: "read".into(), arguments: json!({}) },
                ],
                "openai-completions",
                "openai",
            ),
            tool_result("call_1|fc_1", vec![Block::text("ok")]),
            // A reasoning item with no output after it in its turn is dropped.
            assistant(
                vec![Block::Thinking { thinking: "z".into(), thinking_signature: Some(REASONING.into()) }],
                "openai-responses",
                "openai",
            ),
            Message::user("next"),
        ];
        assert_eq!(
            Value::Array(to_wire(&msgs, &m)),
            json!([
                {"type":"message","role":"assistant","content":[{"type":"output_text","text":"answer","annotations":[]}],"status":"completed"},
                {"type":"function_call","call_id":"toolu_01A","name":"read","arguments":"{}"},
                {"type":"function_call_output","call_id":"toolu_01A","output":"ok"},
                {"type":"function_call","call_id":"call_1","name":"read","arguments":"{}"},
                {"type":"function_call_output","call_id":"call_1","output":"ok"},
                {"role":"user","content":[{"type":"input_text","text":"next"}]},
            ])
        );
    }

    #[test]
    fn unanswered_call_gets_an_output() {
        let m = model(true);
        let msgs = [
            assistant(
                vec![Block::ToolCall { id: "call_9|fc_9".into(), name: "read".into(), arguments: json!({}) }],
                "openai-responses",
                "openai",
            ),
            Message::user("again"),
        ];
        let input = to_wire(&msgs, &m);
        assert_eq!(input[1]["type"], "function_call_output");
        assert_eq!(input[1]["call_id"], "call_9");
        assert!(input[1]["output"].as_str().unwrap().contains("not completed"));
    }

    #[test]
    fn url_is_under_the_base() {
        let m = model(true);
        let msgs = [Message::user("hi")];
        let route = Route {
            base: "https://api.openai.com/v1".into(),
            auth: Auth::Bearer("k".into()),
            def: crate::provider::providers::native_def("openai"),
        };
        let w = wire(&request(&m, Thinking::Off, &msgs, &[]), &route).unwrap();
        assert_eq!(w.url, "https://api.openai.com/v1/responses");
        assert!(w.headers.is_empty());
    }

    type Log = Vec<String>;

    fn describe(e: StreamEvent) -> String {
        match e {
            StreamEvent::Text(t) => format!("text:{t}"),
            StreamEvent::Thinking(t) => format!("thinking:{t}"),
            StreamEvent::ToolCallStart { id, name } => format!("start:{id}:{name}"),
            StreamEvent::ToolCallDelta { id, json } => format!("delta:{id}:{json}"),
            StreamEvent::Retry { .. } => "retry".into(),
            StreamEvent::Notice(_) | StreamEvent::FailedAttemptUsage(_) => "other".into(),
        }
    }

    fn run(bytes: &[u8], step: usize) -> (Log, Result<Message, ProviderError>) {
        let mut a = Assembler::new(&model(true));
        let mut log = Vec::new();
        for c in bytes.chunks(step) {
            match a.feed(c, &mut |e| log.push(describe(e))) {
                Ok(Some(m)) => return (log, Ok(m)),
                Ok(None) => {}
                Err(e) => return (log, Err(e)),
            }
        }
        (log, Err(ProviderError::Fatal("stream ended without a terminal event".into())))
    }

    /// Feeds `bytes` at several chunk sizes; each must give the same events and message as one chunk.
    fn run_split(bytes: &[u8]) -> (Log, Result<Message, ProviderError>) {
        let whole = run(bytes, bytes.len().max(1));
        for step in [1, 2, 3, 7] {
            let split = run(bytes, step);
            assert_eq!(split.0, whole.0, "events at step {step}");
            match (&split.1, &whole.1) {
                (Ok(a), Ok(b)) => assert_eq!(shape(a), shape(b), "message at step {step}"),
                (Err(a), Err(b)) => assert_eq!(a.to_string(), b.to_string(), "error at step {step}"),
                _ => panic!("outcome differs at step {step}"),
            }
        }
        whole
    }

    /// Content and stop reason; the timestamp differs between runs.
    fn shape(m: &Message) -> (Value, StopReason) {
        let Message::Assistant { content, stop_reason, .. } = m else { panic!("not an assistant message") };
        (json!(content), *stop_reason)
    }

    fn usage(m: &Message) -> Usage {
        let Message::Assistant { usage, .. } = m else { panic!("not an assistant message") };
        usage.clone()
    }

    #[test]
    fn text_only_stream_with_cached_usage() {
        let (log, res) = run_split(include_bytes!("testdata/responses/text.sse"));
        let m = res.unwrap();
        assert_eq!(log, ["text:Hel", "text:lo ✓"]);
        assert_eq!(shape(&m).1, StopReason::Stop);
        assert_eq!(shape(&m).0, json!([{"type":"text","text":"Hello ✓"}]));
        let u = usage(&m);
        assert_eq!((u.input, u.output, u.cache_read, u.total_tokens), (12, 5, 8, 25));
        let Message::Assistant { response_id, .. } = &m else { panic!() };
        assert_eq!(response_id.as_deref(), Some("resp_0a1b2c"));
    }

    #[test]
    fn reasoning_then_tool_call_stream() {
        let (log, res) = run_split(include_bytes!("testdata/responses/reasoning_tool.sse"));
        let m = res.unwrap();
        assert_eq!(shape(&m).1, StopReason::ToolUse);
        assert_eq!(
            log,
            [
                "thinking:Need to ",
                "thinking:read the file.",
                "start:call_Xy12|fc_0a1b2c:read",
                "delta:call_Xy12|fc_0a1b2c:{\"pa",
                "delta:call_Xy12|fc_0a1b2c:th\":\"a.txt\"}",
            ]
        );
        let Message::Assistant { content, .. } = &m else { panic!() };
        let Block::Thinking { thinking, thinking_signature } = &content[0] else { panic!("reasoning first") };
        assert_eq!(thinking, "Need to read the file.");
        assert_eq!(
            serde_json::from_str::<Value>(thinking_signature.as_deref().unwrap()).unwrap(),
            serde_json::from_str::<Value>(REASONING).unwrap()
        );
        let Block::ToolCall { id, name, arguments } = &content[1] else { panic!("call second") };
        assert_eq!((id.as_str(), name.as_str()), ("call_Xy12|fc_0a1b2c", "read"));
        assert_eq!(arguments, &json!({"path":"a.txt"}));
        let u = usage(&m);
        assert_eq!((u.input, u.output, u.cache_read, u.total_tokens), (24, 30, 16, 70));
    }

    #[test]
    fn incomplete_at_max_output_tokens_is_length() {
        let (_, res) = run_split(include_bytes!("testdata/responses/max_tokens.sse"));
        let m = res.unwrap();
        assert_eq!(shape(&m).1, StopReason::Length);
        assert_eq!(shape(&m).0, json!([{"type":"text","text":"The answer is"}]));
    }

    #[test]
    fn failed_response_is_retryable_when_server_error() {
        let (_, res) = run_split(include_bytes!("testdata/responses/failed.sse"));
        assert!(matches!(res, Err(ProviderError::Retryable { .. })), "{res:?}");
    }

    #[test]
    fn error_event_context_overflow() {
        let (_, res) = run_split(include_bytes!("testdata/responses/error.sse"));
        assert!(matches!(res, Err(ProviderError::Overflow(_))), "{res:?}");
    }

    #[test]
    fn in_stream_error_codes_map_to_retry_classes() {
        let frame = |code: &str| {
            format!("event: error\ndata: {{\"type\":\"error\",\"code\":\"{code}\",\"message\":\"m\"}}\n\n")
        };
        let class = |code: &str| {
            let (_, res) = run(frame(code).as_bytes(), 1);
            res.unwrap_err()
        };
        assert!(matches!(class("rate_limit_exceeded"), ProviderError::Retryable { .. }));
        assert!(matches!(class("server_error"), ProviderError::Retryable { .. }));
        assert!(matches!(class("context_length_exceeded"), ProviderError::Overflow(_)));
        let fatal = class("invalid_prompt");
        assert!(matches!(&fatal, ProviderError::Fatal(m) if m.as_str() == "invalid_prompt: m"), "{fatal:?}");
    }

    #[test]
    fn stream_without_terminal_event_is_an_error() {
        let (_, res) =
            run(b"event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"r\"}}\n\n", 5);
        assert!(res.is_err());
    }

    #[test]
    fn stream_reasoning_only_is_stored_as_item() {
        let frames = [
            json!({"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs_1","summary":[]}}),
            json!({"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"rs_1","summary":[],"encrypted_content":"enc"}}),
            json!({"type":"response.completed","response":{"id":"r","status":"completed"}}),
        ];
        let bytes: String = frames.iter().map(|f| format!("data: {f}\n\n")).collect();
        let (_, res) = run(bytes.as_bytes(), bytes.len());
        let m = res.unwrap();
        let Message::Assistant { content, .. } = &m else { panic!() };
        let Block::Thinking { thinking, thinking_signature } = &content[0] else { panic!() };
        assert!(thinking.is_empty());
        assert!(thinking_signature.as_deref().unwrap().contains("enc"));
    }

    fn frames_bytes(frames: &[Value]) -> Vec<u8> {
        frames.iter().map(|f| format!("data: {f}\n\n")).collect::<String>().into_bytes()
    }

    #[test]
    fn arguments_done_replaces_the_streamed_buffer() {
        let item = json!({"type":"function_call","id":"fc_1","call_id":"call_1","name":"read","arguments":""});
        let bytes = frames_bytes(&[
            json!({"type":"response.output_item.added","output_index":0,"item":item}),
            json!({"type":"response.function_call_arguments.delta","output_index":0,"delta":"{\"pa"}),
            json!({"type":"response.function_call_arguments.done","output_index":0,"arguments":"{\"path\":\"b.txt\"}"}),
            json!({"type":"response.output_item.done","output_index":0,"item":item}),
            json!({"type":"response.completed","response":{"id":"r","status":"completed"}}),
        ]);
        let (_, res) = run_split(&bytes);
        let m = res.unwrap();
        let Message::Assistant { content, .. } = &m else { panic!() };
        assert!(matches!(&content[0], Block::ToolCall { arguments, .. } if *arguments == json!({"path":"b.txt"})));
    }

    #[test]
    fn terminal_output_is_used_when_no_item_streamed() {
        let bytes = frames_bytes(&[json!({"type":"response.completed","response":{
        "id":"resp_t","status":"completed",
        "output":[
            {"type":"message","id":"msg_t","role":"assistant","content":[{"type":"output_text","text":"hi","annotations":[]}]},
            {"type":"function_call","id":"fc_t","call_id":"call_t","name":"read","arguments":"{\"path\":\"c\"}"}
        ]}})]);
        let (_, res) = run_split(&bytes);
        let m = res.unwrap();
        assert_eq!(shape(&m).1, StopReason::ToolUse);
        assert_eq!(
            shape(&m).0,
            json!([
                {"type":"text","text":"hi"},
                {"type":"toolCall","id":"call_t|fc_t","name":"read","arguments":{"path":"c"}},
            ])
        );
    }

    // Fixture events follow omp's openai-responses-stream-terminal.test.ts shapes; written by hand, keys absent.
    #[test]
    fn truncated_turn_with_executable_calls_runs_them() {
        let (log, res) = run_split(include_bytes!("testdata/responses/incomplete_tool.sse"));
        let m = res.unwrap();
        assert_eq!(shape(&m).1, StopReason::ToolUse);
        assert_eq!(log.len(), 2, "{log:?}");
        let Message::Assistant { content, .. } = &m else { panic!() };
        assert!(
            matches!(&content[0], Block::ToolCall { arguments, .. } if *arguments == json!({"path":"a.txt","content":"hi"}))
        );
    }

    #[test]
    fn truncated_turn_with_a_cut_off_call_is_length() {
        let bytes = frames_bytes(&[
            json!({"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc_2","call_id":"call_2","name":"write","arguments":""}}),
            json!({"type":"response.function_call_arguments.delta","output_index":0,"delta":"{\"path\":\"a"}),
            json!({"type":"response.incomplete","response":{"id":"r","status":"incomplete","incomplete_details":{"reason":"max_output_tokens"}}}),
        ]);
        let (_, res) = run_split(&bytes);
        let m = res.unwrap();
        assert_eq!(shape(&m).1, StopReason::Length);
        let Message::Assistant { content, .. } = &m else { panic!() };
        assert!(matches!(&content[0], Block::ToolCall { arguments, .. } if arguments.get("__invalid_json").is_some()));
    }

    #[test]
    fn refusal_streams_and_finishes_as_text() {
        let (log, res) = run_split(include_bytes!("testdata/responses/refusal.sse"));
        let m = res.unwrap();
        assert_eq!(log, ["text:I cannot ", "text:help with that."]);
        assert_eq!(shape(&m).0, json!([{"type":"text","text":"I cannot help with that."}]));
        assert_eq!(shape(&m).1, StopReason::Stop);
    }

    #[test]
    fn failed_and_cancelled_terminal_responses_are_errors() {
        let failed = frames_bytes(&[json!({"type":"response.completed","response":{"id":"r","status":"failed",
            "error":{"code":"invalid_prompt","message":"bad"}}})]);
        let (_, res) = run(&failed, failed.len());
        assert_eq!(res.unwrap_err().to_string(), "invalid_prompt: bad");
        let cancelled = frames_bytes(&[json!({"type":"response.done","response":{"id":"r","status":"cancelled"}})]);
        let (_, res) = run(&cancelled, cancelled.len());
        assert!(res.is_err());
    }

    #[test]
    fn reasoning_include_and_cache_key_follow_the_model() {
        let msgs = [Message::user("hi")];
        let plain = model(false);
        let out = body(&request(&plain, Thinking::Off, &msgs, &[]));
        assert!(out.get("include").is_none(), "{out}");
        let reasoning = model(true);
        let mut req = request(&reasoning, Thinking::Off, &msgs, &[]);
        req.session_id = Some("");
        assert!(body(&req).get("prompt_cache_key").is_none());
    }

    #[test]
    fn unparsable_arguments_are_replayed_as_the_raw_text() {
        let m = model(true);
        let msgs = [assistant(
            vec![Block::ToolCall {
                id: "call_r|fc_r".into(),
                name: "write".into(),
                arguments: json!({"__invalid_json": "{\"path\":"}),
            }],
            "openai-responses",
            "openai",
        )];
        let input = to_wire(&msgs, &m);
        assert_eq!(input[0]["arguments"], "{\"path\":");
    }

    #[test]
    fn reasoning_and_item_ids_replay_only_into_the_same_model() {
        let m = model(true);
        let turn = |model_id: &str, stop: StopReason| Message::Assistant {
            content: vec![
                Block::Thinking { thinking: "t".into(), thinking_signature: Some(REASONING.into()) },
                Block::ToolCall { id: "call_z|fc_z".into(), name: "read".into(), arguments: json!({}) },
            ],
            api: "openai-responses".into(),
            provider: "openai".into(),
            model: model_id.into(),
            usage: Usage::default(),
            stop_reason: stop,
            timestamp: 0,
            response_id: None,
            error_message: None,
        };
        let other = to_wire(&[turn("openai/gpt-4.1", StopReason::ToolUse)], &m);
        assert_eq!(other[0], json!({"type":"function_call","call_id":"call_z","name":"read","arguments":"{}"}));
        let errored = to_wire(&[turn("openai/gpt-5.5", StopReason::Error)], &m);
        assert_eq!(errored[0], json!({"type":"function_call","call_id":"call_z","name":"read","arguments":"{}"}));
        let same = to_wire(&[turn("openai/gpt-5.5", StopReason::ToolUse)], &m);
        assert_eq!(same[0]["type"], "reasoning");
        assert_eq!(same[1]["id"], "fc_z");
    }

    #[test]
    #[ignore = "contract: calls the real OpenAI API; set OPENAI_API_KEY (and optionally OPENAI_CONTRACT_MODEL)"]
    fn contract_native_openai() {
        let Ok(key) = std::env::var("OPENAI_API_KEY") else {
            eprintln!("skipped contract_native_openai: OPENAI_API_KEY is not set");
            return;
        };
        crate::net::install_crypto();
        let wire_id = std::env::var("OPENAI_CONTRACT_MODEL").unwrap_or_else(|_| "gpt-5.5".into());
        let mut spec = model(true);
        spec.id = format!("openai/{wire_id}");
        spec.wire_id = wire_id;
        let route = Route {
            base: "https://api.openai.com/v1".into(),
            auth: Auth::Bearer(key.clone()),
            def: crate::provider::providers::native_def("openai"),
        };
        let tools = [read_tool()];
        let mut msgs = vec![Message::user("Call read on a.txt, then tell me the word it returned.")];
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            use futures_util::StreamExt;
            let client = reqwest::Client::new();
            let mut last = None;
            // Turn 0 calls the tool; turn 1 must replay the reasoning item and the call.
            for turn in 0..3 {
                let mut w = {
                    let req = request(&spec, Thinking::Low, &msgs, &tools);
                    wire(&req, &route).unwrap()
                };
                let resp = client
                    .post(&w.url)
                    .bearer_auth(&key)
                    .header("content-type", "application/json")
                    .body(w.body.to_string())
                    .send()
                    .await
                    .expect("request");
                if !resp.status().is_success() {
                    panic!("turn {turn}: {}: {}", resp.status(), resp.text().await.unwrap_or_default());
                }
                let mut stream = resp.bytes_stream();
                let mut done = None;
                while let Some(chunk) = stream.next().await {
                    let chunk = chunk.expect("stream");
                    if let Some(m) = w.decoder.feed(&chunk, &mut |_| {}).expect("decode") {
                        done = Some(m);
                        break;
                    }
                }
                let m = done.unwrap_or_else(|| panic!("turn {turn}: stream ended without a response"));
                let Message::Assistant { content, .. } = &m else { panic!() };
                let calls: Vec<(String, String)> = content
                    .iter()
                    .filter_map(|b| match b {
                        Block::ToolCall { id, name, .. } => Some((id.clone(), name.clone())),
                        _ => None,
                    })
                    .collect();
                msgs.push(m.clone());
                if calls.is_empty() {
                    last = Some(m);
                    break;
                }
                for (id, name) in calls {
                    msgs.push(Message::ToolResult {
                        tool_call_id: id,
                        tool_name: name,
                        content: vec![Block::text("plover")],
                        is_error: false,
                        timestamp: 0,
                        details: None,
                    });
                }
            }
            let m = last.expect("no final answer");
            let (content, stop) = shape(&m);
            assert_eq!(stop, StopReason::Stop);
            assert!(content.to_string().to_lowercase().contains("plover"), "{content}");
        });
    }
}
