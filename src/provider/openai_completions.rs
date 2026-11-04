//! OpenAI-compatible Chat Completions wire (`POST {base}/chat/completions`), used by Baseten.
//! Conventions follow omp's `openai-completions.ts` so sessions move between the two.

use super::sse::SseDecoder;
use super::{Decoder, ProviderError, Request, Route, StreamEvent, Wire, normalize_id};
use crate::message::{Block, Image, Message, StopReason, Usage, now_ms};
use crate::models::{self, CompletionsCompat, ModelSpec, Thinking, ThinkingMode};
use serde_json::{Value, json};
use std::collections::HashMap;

/// Field names a reasoning delta may arrive under, in the order they are tried.
const REASONING_FIELDS: [&str; 3] = ["reasoning_content", "reasoning", "reasoning_text"];

pub fn url(base: &str) -> String {
    format!("{base}/chat/completions")
}

pub fn wire(req: &Request, route: &Route) -> Result<Wire, ProviderError> {
    Ok(Wire {
        url: url(&route.base),
        headers: Vec::new(),
        body: body(req),
        decoder: Box::new(Assembler::new(req.model)),
    })
}

impl Decoder for Assembler {
    fn feed(&mut self, chunk: &[u8], ev: &mut dyn FnMut(StreamEvent)) -> Result<Option<Message>, ProviderError> {
        Assembler::feed(self, chunk, ev)
    }

    fn finish(&mut self) -> Option<Message> {
        Assembler::finish(self)
    }
}

pub fn body(req: &Request) -> Value {
    let model = req.model;
    let compat = model.compat.clone().unwrap_or_default();
    let max_tokens = req.max_tokens.unwrap_or(model.max_output).min(model.max_output);
    let mut body = json!({
        "model": model.wire_id,
        "messages": to_wire(req.system, req.messages, model),
        "stream": true,
    });
    body[compat.max_tokens_field.as_str()] = json!(max_tokens);
    if compat.supports_usage_in_streaming {
        body["stream_options"] = json!({"include_usage": true});
    }
    if !req.tools.is_empty() {
        body["tools"] = Value::Array(
            req.tools
                .iter()
                .map(|t| json!({"type":"function","function":{"name":t.name,"description":t.description,"parameters":t.schema}}))
                .collect(),
        );
    }
    if model.thinking.mode == ThinkingMode::Effort
        && let Some(e) = reasoning_effort(req.thinking.get(), &model.thinking.efforts, &compat)
    {
        body["reasoning_effort"] = e.into();
    }
    body
}

/// The `reasoning_effort` to send. Off is disabled per the model's disable mode: `none-effort`
/// sends `none`, `lowest-effort` the bottom of the ladder, `omit` (or no mode) sends nothing.
fn reasoning_effort(level: Thinking, efforts: &[String], compat: &CompletionsCompat) -> Option<String> {
    if level != Thinking::Off {
        return models::clamp_effort(level, efforts).map(String::from);
    }
    match compat.reasoning_disable_mode.as_deref() {
        Some("none-effort") => Some("none".into()),
        Some("lowest-effort") => efforts.first().cloned(),
        _ => None,
    }
}

pub fn to_wire(system: &str, messages: &[Message], model: &ModelSpec) -> Vec<Value> {
    let compat = model.compat.clone().unwrap_or_default();
    // Backstop: never send unpaired tool calls/results, whatever the history holds.
    let mut repaired;
    let messages = if crate::message::check_tool_pairs(messages).is_err() {
        repaired = messages.to_vec();
        crate::message::repair_tool_pairs(&mut repaired);
        &repaired[..]
    } else {
        messages
    };
    let images = model.images && !compat.strip_image_input;
    let mut out = Vec::new();
    if !system.trim().is_empty() {
        let developer = compat.supports_developer_role && model.thinking.mode != ThinkingMode::None;
        out.push(json!({"role": if developer { "developer" } else { "system" }, "content": system}));
    }
    // Images from tool results go in one user message after the whole batch of results: the
    // tool messages must stay contiguous after the assistant turn that called them.
    // Tool-call ids the assistant turns sent, so the results that follow use the same ids.
    let mut ids: HashMap<String, String> = HashMap::new();
    let mut pending: Vec<Value> = Vec::new();
    for m in messages {
        if !matches!(m, Message::ToolResult { .. }) {
            flush_images(&mut out, &mut pending);
        }
        match m {
            Message::User { content, .. } => {
                let parts: Vec<Value> = content
                    .iter()
                    .filter_map(|b| match b {
                        Block::Text { text } if !text.is_empty() => Some(json!({"type":"text","text":text})),
                        Block::Image(i) => Some(image_part(i, images)),
                        _ => None,
                    })
                    .collect();
                if !parts.is_empty() {
                    out.push(json!({"role":"user","content":parts}));
                }
            }
            Message::Assistant { content, api, provider, model: turn, .. } => {
                let same = api == model.api.as_str() && provider == &model.provider && turn == &model.id;
                out.extend(assistant(content, same, model, &compat, &mut ids));
            }
            Message::ToolResult { tool_call_id, content, .. } => {
                let texts: Vec<&str> = content
                    .iter()
                    .filter_map(|b| match b {
                        Block::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect();
                let imgs: Vec<&Image> = content
                    .iter()
                    .filter_map(|b| match b {
                        Block::Image(i) => Some(i),
                        _ => None,
                    })
                    .collect();
                let mut text = texts.join("\n");
                if !imgs.is_empty() {
                    if images {
                        if text.is_empty() {
                            text = "(see attached image)".into();
                        }
                        pending.extend(imgs.into_iter().map(|i| image_part(i, true)));
                    } else {
                        let note = format!("[{} image(s) omitted: this model takes no image input]", imgs.len());
                        text = if text.is_empty() { note } else { format!("{text}\n{note}") };
                    }
                }
                let tool_call_id = ids.get(tool_call_id).cloned().unwrap_or_else(|| normalize_id(tool_call_id));
                out.push(json!({"role":"tool","tool_call_id":tool_call_id,"content":text}));
            }
        }
    }
    flush_images(&mut out, &mut pending);
    out
}

fn flush_images(out: &mut Vec<Value>, pending: &mut Vec<Value>) {
    if pending.is_empty() {
        return;
    }
    let mut parts = vec![json!({"type":"text","text":"Attached image(s) from tool result:"})];
    parts.append(pending);
    out.push(json!({"role":"user","content":parts}));
}

/// An assistant turn. Reasoning goes back under the field it streamed in (or the compat field),
/// and only where the model requires it: on every turn, or on turns that call tools. Thinking from
/// another model is demoted to text in a `<think>` block (omp's demotion for the open-weight
/// dialects Baseten serves). Tool-call ids stay verbatim on the same model and are normalised
/// otherwise; `ids` records them for the results that follow.
fn assistant(
    content: &[Block],
    same_model: bool,
    model: &ModelSpec,
    compat: &CompletionsCompat,
    ids: &mut HashMap<String, String>,
) -> Option<Value> {
    // Z.AI reads another model's reasoning as a continuation hint, so it is replayed natively.
    let zai = compat.thinking_format.as_deref() == Some("zai") && model.thinking.mode != ThinkingMode::None;
    let mut parts: Vec<(std::borrow::Cow<str>, bool)> = Vec::new();
    let mut thinking: Vec<&str> = Vec::new();
    let mut signature: Option<&str> = None;
    for b in content {
        match b {
            Block::Text { text } if !text.trim().is_empty() => parts.push((text.as_str().into(), false)),
            Block::Thinking { thinking: t, thinking_signature } if !t.trim().is_empty() => {
                if same_model || zai {
                    if thinking.is_empty() {
                        signature = thinking_signature.as_deref();
                    }
                    thinking.push(t.as_str());
                } else {
                    parts.push((format!("<think>\n{t}\n</think>").into(), true));
                }
            }
            _ => {}
        }
    }
    let mut text = String::new();
    for (i, (t, demoted)) in parts.iter().enumerate() {
        text.push_str(t);
        // A demoted thinking block must not run into the text after it.
        if *demoted && i + 1 < parts.len() {
            text.push('\n');
        }
    }
    let mut calls = Vec::new();
    for b in content {
        if let Block::ToolCall { id, name, arguments } = b {
            let wire_id = if same_model { id.clone() } else { normalize_id(id) };
            ids.insert(id.clone(), wire_id.clone());
            calls.push(json!({
                "id": wire_id,
                "type": "function",
                "function": {"name": name, "arguments": wire_arguments(arguments)},
            }));
        }
    }
    let has_calls = !calls.is_empty();
    let configured = compat.reasoning_content_field.as_deref().unwrap_or("reasoning_content");
    let tool_path = compat.requires_reasoning_content_for_tool_calls;
    let all_turns = compat.requires_reasoning_content_for_all_assistant_turns;
    // The streamed field name is replayed when it is a recognised one the model accepts; on tool
    // turns that needs synthetic reasoning to be allowed, else the configured field is used.
    let field = signature
        .filter(|s| {
            REASONING_FIELDS.contains(s) && (compat.allows_synthetic_reasoning_content_for_tool_calls || !tool_path)
        })
        .unwrap_or(configured);
    let mut reasoning: Option<(&str, String)> = None;
    if !thinking.is_empty() && (tool_path || all_turns || zai) {
        reasoning = Some((field, thinking.join("\n")));
    }
    // No reasoning captured where the model needs the field: a placeholder or the fallback (omp's
    // tiers 2 and 3).
    if reasoning.is_none() && tool_path && (all_turns || has_calls) {
        let allows = compat.allows_synthetic_reasoning_content_for_tool_calls;
        if has_calls && allows && matches!(compat.thinking_format.as_deref(), Some("openai" | "openrouter" | "zai")) {
            reasoning = Some((configured, ".".into()));
        } else if !allows {
            reasoning = Some((configured, compat.synthetic_reasoning_content_fallback.clone().unwrap_or_default()));
        }
    }
    if text.is_empty() && has_calls && compat.requires_assistant_content_for_tool_calls {
        text = ".".into();
    }
    if text.is_empty() && !has_calls && reasoning.is_none() {
        return None;
    }
    let mut m = json!({"role": "assistant", "content": text});
    if has_calls {
        m["tool_calls"] = Value::Array(calls);
    }
    if let Some((f, r)) = reasoning {
        m[f] = r.into();
    }
    Some(m)
}

/// Arguments as the wire takes them. An unparsable call is stored as `{"__invalid_json": raw}`;
/// its raw text goes back unchanged.
fn wire_arguments(args: &Value) -> String {
    match args.get("__invalid_json").and_then(Value::as_str) {
        Some(raw) if args.as_object().is_some_and(|o| o.len() == 1) => raw.to_string(),
        _ => args.to_string(),
    }
}

fn image_part(i: &Image, supported: bool) -> Value {
    if !supported {
        return json!({"type":"text","text":"[image omitted: this model takes no image input]"});
    }
    match crate::image::resolve(&i.data) {
        Some(data) => json!({"type":"image_url","image_url":{"url":format!("data:{};base64,{data}", i.mime_type)}}),
        None => json!({"type":"text","text":format!("[image unavailable: {}]", i.mime_type)}),
    }
}

#[derive(Default)]
struct Call {
    /// The `index` the server gave the call, when it sends one.
    index: Option<u64>,
    id: String,
    name: String,
    /// Every argument fragment so far, kept so a call whose id arrives late can still replay them.
    args: String,
    started: bool,
}

pub struct Assembler {
    model: String,
    provider: String,
    api: &'static str,
    sse: SseDecoder,
    text: String,
    reasoning: String,
    /// The field the reasoning arrived under; stored as the block's signature for replay.
    reasoning_field: Option<&'static str>,
    /// In stream order; a fragment finds its call through `slot`.
    calls: Vec<Call>,
    finish_reason: Option<String>,
    usage: Usage,
    response_id: Option<String>,
}

impl Assembler {
    pub fn new(model: &ModelSpec) -> Self {
        Self {
            model: model.id.clone(),
            provider: model.provider.clone(),
            api: model.api.as_str(),
            sse: SseDecoder::default(),
            text: String::new(),
            reasoning: String::new(),
            reasoning_field: None,
            calls: Vec::new(),
            finish_reason: None,
            usage: Usage::default(),
            response_id: None,
        }
    }

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

    fn frame(&mut self, data: &str, ev: &mut dyn FnMut(StreamEvent)) -> Result<Option<Message>, ProviderError> {
        // The message ends at `[DONE]`, not at `finish_reason`: servers send the usage chunk after it.
        if data.trim() == "[DONE]" {
            return Ok(Some(self.complete()));
        }
        let v: Value = serde_json::from_str(data).map_err(|e| ProviderError::Fatal(format!("bad stream json: {e}")))?;
        if !v["error"].is_null() {
            return Err(stream_failure(&v["error"]));
        }
        if self.response_id.is_none()
            && let Some(id) = v["id"].as_str()
        {
            self.response_id = Some(id.into());
        }
        if v["usage"].is_object() {
            self.usage = usage_of(&v["usage"]);
        }
        let choice = &v["choices"][0];
        if let Some(r) = choice["finish_reason"].as_str() {
            self.finish_reason = Some(r.into());
        }
        let delta = &choice["delta"];
        if let Some(t) = delta["content"].as_str().filter(|t| !t.is_empty()) {
            self.text.push_str(t);
            ev(StreamEvent::Text(t));
        }
        for f in REASONING_FIELDS {
            if let Some(t) = delta[f].as_str().filter(|t| !t.is_empty()) {
                self.reasoning.push_str(t);
                self.reasoning_field.get_or_insert(f);
                ev(StreamEvent::Thinking(t));
                break;
            }
        }
        if let Some(calls) = delta["tool_calls"].as_array() {
            for c in calls {
                self.tool_fragment(c, ev);
            }
        }
        Ok(None)
    }

    /// The call a `tool_calls` entry extends: by `index` when the server sends one, else by id,
    /// else the latest call. Returns its position in `calls`.
    fn slot(&mut self, c: &Value) -> usize {
        let id = c["id"].as_str().filter(|s| !s.is_empty());
        let index = c["index"].as_u64();
        let found = match index {
            Some(ix) => {
                self.calls.iter().position(|k| k.index == Some(ix) && id.is_none_or(|i| k.id.is_empty() || k.id == i))
            }
            None => match id {
                Some(i) => self.calls.iter().position(|k| k.id == i),
                None => self.calls.len().checked_sub(1),
            },
        };
        found.unwrap_or_else(|| {
            self.calls.push(Call { index, ..Call::default() });
            self.calls.len() - 1
        })
    }

    /// One `tool_calls` entry of a delta. Id and name come in the first fragment of a call, but
    /// arguments may precede them, so the call starts once its name is known. A call that never
    /// sends an id gets a stable synthetic one.
    fn tool_fragment(&mut self, c: &Value, ev: &mut dyn FnMut(StreamEvent)) {
        let pos = self.slot(c);
        let call = &mut self.calls[pos];
        if call.id.is_empty()
            && let Some(id) = c["id"].as_str().filter(|s| !s.is_empty())
        {
            call.id = id.into();
        }
        if call.name.is_empty()
            && let Some(n) = c["function"]["name"].as_str().filter(|s| !s.is_empty())
        {
            call.name = n.into();
        }
        if !call.started && !call.name.is_empty() {
            if call.id.is_empty() {
                call.id = format!("call_{pos}");
            }
            call.started = true;
            ev(StreamEvent::ToolCallStart { id: &call.id, name: &call.name });
            if !call.args.is_empty() {
                ev(StreamEvent::ToolCallDelta { id: &call.id, json: &call.args });
            }
        }
        if let Some(a) = c["function"]["arguments"].as_str().filter(|s| !s.is_empty()) {
            call.args.push_str(a);
            if call.started {
                ev(StreamEvent::ToolCallDelta { id: &call.id, json: a });
            }
        }
    }

    /// The message for a stream that ended without `[DONE]`: kept once a `finish_reason` arrived.
    pub fn finish(&mut self) -> Option<Message> {
        self.finish_reason.is_some().then(|| self.complete())
    }

    fn complete(&mut self) -> Message {
        let mut content = Vec::new();
        if !self.reasoning.is_empty() {
            content.push(Block::Thinking {
                thinking: std::mem::take(&mut self.reasoning),
                thinking_signature: self.reasoning_field.map(String::from),
            });
        }
        if !self.text.is_empty() {
            content.push(Block::text(std::mem::take(&mut self.text)));
        }
        let has_calls = !self.calls.is_empty();
        for (idx, c) in std::mem::take(&mut self.calls).into_iter().enumerate() {
            let id = if c.id.is_empty() { format!("call_{idx}") } else { c.id };
            let arguments = if c.args.trim().is_empty() {
                json!({})
            } else {
                serde_json::from_str(&c.args).unwrap_or_else(|_| json!({"__invalid_json": c.args}))
            };
            content.push(Block::ToolCall { id, name: c.name, arguments });
        }
        let (stop_reason, error_message) = stop_of(self.finish_reason.as_deref(), has_calls);
        Message::Assistant {
            content,
            api: self.api.into(),
            provider: self.provider.clone(),
            model: self.model.clone(),
            usage: self.usage.clone(),
            stop_reason,
            timestamp: now_ms(),
            response_id: self.response_id.clone(),
            error_message,
        }
    }
}

/// A `finish_reason` as mu's stop reason. A stop that carries tool calls is a tool turn.
fn stop_of(reason: Option<&str>, has_calls: bool) -> (StopReason, Option<String>) {
    match reason {
        None | Some("stop" | "end") => (if has_calls { StopReason::ToolUse } else { StopReason::Stop }, None),
        Some("length" | "max_tokens") => (StopReason::Length, None),
        Some("tool_calls" | "function_call") => (if has_calls { StopReason::ToolUse } else { StopReason::Stop }, None),
        Some(other) => (StopReason::Error, Some(format!("Provider finish_reason: {other}"))),
    }
}

/// `prompt_tokens` includes the cached ones, which are reported apart as `cache_read`.
fn usage_of(u: &Value) -> Usage {
    let prompt = u["prompt_tokens"].as_u64().unwrap_or(0);
    let output = u["completion_tokens"].as_u64().unwrap_or(0);
    let cached = u["prompt_tokens_details"]["cached_tokens"].as_u64().unwrap_or(0).min(prompt);
    Usage { input: prompt - cached, output, cache_read: cached, total_tokens: prompt + output, ..Usage::default() }
}

/// An in-stream `error` object: rate limits, overload and server faults retry; a context overflow
/// is its own class; anything else is final.
fn stream_failure(e: &Value) -> ProviderError {
    const RETRYABLE: [&str; 9] = [
        "rate_limit",
        "overload",
        "server_error",
        "api_error",
        "upstream",
        "unavailable",
        "internal",
        "timeout",
        "too_many",
    ];
    let msg = e.as_str().or_else(|| e["message"].as_str()).unwrap_or("unknown stream error");
    let kind = format!("{} {}", e["code"].as_str().unwrap_or(""), e["type"].as_str().unwrap_or("")).to_lowercase();
    let status = e["code"].as_u64().or_else(|| e["code"].as_str().and_then(|s| s.parse().ok())).unwrap_or(0);
    let lower = msg.to_lowercase();
    if kind.contains("context_length") || lower.contains("context length") || lower.contains("maximum context") {
        ProviderError::Overflow(msg.into())
    } else if status == 429 || (500..600).contains(&status) || RETRYABLE.iter().any(|k| kind.contains(k)) {
        ProviderError::Retryable { msg: msg.into(), retry_after: None }
    } else {
        ProviderError::Fatal(msg.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{Auth, ToolDef};
    use std::cell::Cell;
    use std::rc::Rc;

    const INTERLEAVED: &[u8] = include_bytes!("testdata/completions/interleaved.sse");
    const LENGTH: &[u8] = include_bytes!("testdata/completions/length.sse");
    const ERROR: &[u8] = include_bytes!("testdata/completions/error.sse");

    fn spec(id: &str) -> ModelSpec {
        crate::provider::catalog::row("baseten", id).unwrap().spec("baseten", 1_000_000, 262_144).unwrap()
    }

    fn kimi() -> ModelSpec {
        spec("moonshotai/Kimi-K3")
    }

    fn tool() -> ToolDef {
        ToolDef {
            name: "read".into(),
            description: "Read a file.".into(),
            schema: json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}),
        }
    }

    fn request<'a>(m: &'a ModelSpec, level: Thinking, tools: &'a [ToolDef], msgs: &'a [Message]) -> Request<'a> {
        Request {
            model: m,
            thinking: Rc::new(Cell::new(level)),
            system: "Be brief.",
            tools,
            messages: msgs,
            max_tokens: Some(1000),
            session_id: None,
        }
    }

    fn effort(m: &ModelSpec, level: Thinking) -> Option<String> {
        body(&request(m, level, &[], &[])).get("reasoning_effort").and_then(Value::as_str).map(String::from)
    }

    /// An assistant turn from `m` itself: same api, provider and model id.
    fn own(m: &ModelSpec, content: Vec<Block>) -> Message {
        Message::Assistant {
            content,
            api: m.api.as_str().into(),
            provider: m.provider.clone(),
            model: m.id.clone(),
            usage: Usage::default(),
            stop_reason: StopReason::ToolUse,
            timestamp: 0,
            response_id: None,
            error_message: None,
        }
    }

    /// An assistant turn that calls `id` and thinks `secret plan`, from another api/provider/model.
    fn foreign(api: &str, provider: &str, model: &str, id: &str) -> Message {
        Message::Assistant {
            content: vec![
                Block::Thinking {
                    thinking: "secret plan".into(),
                    thinking_signature: Some("reasoning_content".into()),
                },
                Block::text("ok"),
                Block::ToolCall { id: id.into(), name: "read".into(), arguments: json!({}) },
            ],
            api: api.into(),
            provider: provider.into(),
            model: model.into(),
            usage: Usage::default(),
            stop_reason: StopReason::ToolUse,
            timestamp: 0,
            response_id: None,
            error_message: None,
        }
    }

    fn tool_result(id: &str, text: &str) -> Message {
        Message::ToolResult {
            tool_call_id: id.into(),
            tool_name: "read".into(),
            content: vec![Block::text(text)],
            is_error: false,
            timestamp: 0,
            details: None,
        }
    }

    #[test]
    fn kimi_golden_body_off_and_max() {
        let m = kimi();
        let msgs = [Message::user("hi")];
        let tools = [tool()];
        let req = request(&m, Thinking::Off, &tools, &msgs);
        assert_eq!(
            body(&req),
            json!({
                "model": "moonshotai/Kimi-K3",
                "messages": [
                    {"role":"system","content":"Be brief."},
                    {"role":"user","content":[{"type":"text","text":"hi"}]},
                ],
                "stream": true,
                "stream_options": {"include_usage": true},
                "max_completion_tokens": 1000,
                "tools": [{"type":"function","function":{
                    "name":"read","description":"Read a file.",
                    "parameters":{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}}}],
                "reasoning_effort": "low",
            })
        );
        req.thinking.set(Thinking::Max);
        assert_eq!(body(&req)["reasoning_effort"], "max");
        assert_eq!(body(&req)["max_completion_tokens"], 1000);
    }

    #[test]
    fn reasoning_effort_per_level_across_three_rows() {
        let cases = [
            ("moonshotai/Kimi-K3", [Some("low"), Some("low"), Some("high"), Some("max")]),
            ("zai-org/GLM-5.2", [Some("high"), Some("high"), Some("high"), Some("max")]),
            ("openai/gpt-oss-120b", [Some("high"), Some("high"), Some("high"), Some("max")]),
        ];
        for (id, want) in cases {
            let m = spec(id);
            let got = [Thinking::Off, Thinking::Low, Thinking::High, Thinking::Max].map(|l| effort(&m, l));
            let got: Vec<Option<&str>> = got.iter().map(|e| e.as_deref()).collect();
            assert_eq!(got, want, "{id}");
        }
    }

    #[test]
    fn disable_modes_and_non_reasoning_models() {
        let mut m = kimi();
        m.compat.as_mut().unwrap().reasoning_disable_mode = Some("none-effort".into());
        assert_eq!(effort(&m, Thinking::Off).as_deref(), Some("none"));
        m.compat.as_mut().unwrap().reasoning_disable_mode = Some("omit".into());
        assert_eq!(effort(&m, Thinking::Off), None);
        assert_eq!(effort(&m, Thinking::High).as_deref(), Some("high"));

        m.thinking.mode = ThinkingMode::None;
        for l in [Thinking::Off, Thinking::High, Thinking::Max] {
            assert_eq!(effort(&m, l), None);
        }
        assert!(body(&request(&m, Thinking::Max, &[], &[])).get("reasoning_effort").is_none());
    }

    #[test]
    fn compat_flags_shape_the_body() {
        let mut m = kimi();
        {
            let c = m.compat.as_mut().unwrap();
            c.max_tokens_field = "max_tokens".into();
            c.supports_usage_in_streaming = false;
            c.supports_developer_role = true;
        }
        let b = body(&request(&m, Thinking::High, &[], &[]));
        assert_eq!(b["max_tokens"], 1000);
        assert!(b.get("max_completion_tokens").is_none());
        assert!(b.get("stream_options").is_none());
        assert_eq!(b["messages"][0]["role"], "developer");
    }

    #[test]
    fn images_are_inlined_or_noted() {
        let msgs = [Message::User {
            content: vec![
                Block::text("look"),
                Block::Image(Image { data: "QUJD".into(), mime_type: "image/png".into() }),
            ],
            timestamp: 0,
            synthetic: false,
        }];
        let m = kimi();
        let b = body(&request(&m, Thinking::Off, &[], &msgs));
        assert_eq!(
            b["messages"][1]["content"],
            json!([
                {"type":"text","text":"look"},
                {"type":"image_url","image_url":{"url":"data:image/png;base64,QUJD"}},
            ])
        );
        let mut stripped = kimi();
        stripped.images = false;
        stripped.compat.as_mut().unwrap().strip_image_input = true;
        let b = body(&request(&stripped, Thinking::Off, &[], &msgs));
        assert_eq!(b["messages"][1]["content"][1]["type"], "text");
        assert!(b.to_string().contains("[image omitted"));
    }

    #[test]
    fn multi_turn_replay_with_tool_calls() {
        let m = kimi();
        let msgs = [
            Message::user("first"),
            own(
                &m,
                vec![
                    Block::Thinking { thinking: "plan A".into(), thinking_signature: Some("reasoning_content".into()) },
                    Block::text("Checking."),
                    Block::ToolCall { id: "call|x 1".into(), name: "read".into(), arguments: json!({"path":"a"}) },
                ],
            ),
            tool_result("call|x 1", "contents"),
            Message::user("next"),
            // No reasoning captured on a tool turn: Kimi still needs the field, as a placeholder.
            own(
                &m,
                vec![Block::ToolCall { id: "call_2".into(), name: "grep".into(), arguments: json!({"pattern":"fn"}) }],
            ),
            tool_result("call_2", "none"),
            Message::user("done"),
        ];
        let b = body(&request(&m, Thinking::High, &[], &msgs));
        assert_eq!(
            b["messages"],
            json!([
                {"role":"system","content":"Be brief."},
                {"role":"user","content":[{"type":"text","text":"first"}]},
                {"role":"assistant","content":"Checking.",
                 "tool_calls":[{"id":"call|x 1","type":"function","function":{"name":"read","arguments":"{\"path\":\"a\"}"}}],
                 "reasoning_content":"plan A"},
                {"role":"tool","tool_call_id":"call|x 1","content":"contents"},
                {"role":"user","content":[{"type":"text","text":"next"}]},
                // Kimi wants non-empty content and reasoning on tool turns.
                {"role":"assistant","content":".",
                 "tool_calls":[{"id":"call_2","type":"function","function":{"name":"grep","arguments":"{\"pattern\":\"fn\"}"}}],
                 "reasoning_content":"."},
                {"role":"tool","tool_call_id":"call_2","content":"none"},
                {"role":"user","content":[{"type":"text","text":"done"}]},
            ])
        );
    }

    #[test]
    fn reasoning_replayed_on_every_turn_when_the_row_requires_it() {
        let mut m = spec("zai-org/GLM-5.2");
        let msgs = [
            Message::user("q"),
            own(
                &m,
                vec![
                    Block::Thinking { thinking: "t".into(), thinking_signature: Some("reasoning_content".into()) },
                    Block::text("a"),
                ],
            ),
        ];
        assert!(body(&request(&m, Thinking::High, &[], &msgs))["messages"][2].get("reasoning_content").is_none());
        m.compat.as_mut().unwrap().requires_reasoning_content_for_all_assistant_turns = true;
        assert_eq!(body(&request(&m, Thinking::High, &[], &msgs))["messages"][2]["reasoning_content"], "t");
    }

    #[test]
    fn foreign_thinking_is_demoted_and_ids_normalised() {
        let m = kimi();
        let msgs = [
            Message::user("q"),
            foreign("anthropic-messages", "anthropic", "claude", "a1"),
            tool_result("a1", "r"),
            foreign("openai-responses", "baseten", &m.id, "b|2"),
            tool_result("b|2", "r"),
            foreign("openai-completions", "openrouter", &m.id, "c 3"),
            tool_result("c 3", "r"),
            own(
                &m,
                vec![
                    Block::Thinking {
                        thinking: "secret plan".into(),
                        thinking_signature: Some("reasoning_content".into()),
                    },
                    Block::text("ok"),
                    Block::ToolCall { id: "d4".into(), name: "read".into(), arguments: json!({}) },
                ],
            ),
            tool_result("d4", "r"),
        ];
        // Another model's thinking is text, with Kimi's placeholder; its own replays as reasoning.
        // Only the same api, provider and model keep ids verbatim.
        assert_eq!(
            body(&request(&m, Thinking::High, &[], &msgs))["messages"],
            json!([
                {"role":"system","content":"Be brief."},
                {"role":"user","content":[{"type":"text","text":"q"}]},
                {"role":"assistant","content":"<think>\nsecret plan\n</think>\nok","tool_calls":[{"id":"a1","type":"function","function":{"name":"read","arguments":"{}"}}],"reasoning_content":"."},
                {"role":"tool","tool_call_id":"a1","content":"r"},
                {"role":"assistant","content":"<think>\nsecret plan\n</think>\nok","tool_calls":[{"id":"b_2","type":"function","function":{"name":"read","arguments":"{}"}}],"reasoning_content":"."},
                {"role":"tool","tool_call_id":"b_2","content":"r"},
                {"role":"assistant","content":"<think>\nsecret plan\n</think>\nok","tool_calls":[{"id":"c_3","type":"function","function":{"name":"read","arguments":"{}"}}],"reasoning_content":"."},
                {"role":"tool","tool_call_id":"c_3","content":"r"},
                {"role":"assistant","content":"ok","tool_calls":[{"id":"d4","type":"function","function":{"name":"read","arguments":"{}"}}],"reasoning_content":"secret plan"},
                {"role":"tool","tool_call_id":"d4","content":"r"},
            ])
        );
    }

    #[test]
    fn zai_replays_foreign_reasoning_natively() {
        let mut m = spec("zai-org/GLM-5.2");
        m.compat.as_mut().unwrap().thinking_format = Some("zai".into());
        let msgs =
            [Message::user("q"), foreign("anthropic-messages", "anthropic", "claude", "z1"), tool_result("z1", "r")];
        let b = body(&request(&m, Thinking::High, &[], &msgs));
        assert_eq!(b["messages"][2]["content"], "ok");
        assert_eq!(b["messages"][2]["reasoning_content"], "secret plan");
    }

    #[test]
    fn deepseek_replays_reasoning_on_every_turn() {
        let m = spec("deepseek-ai/DeepSeek-V4-Pro-0813");
        let msgs = [
            Message::user("q"),
            own(&m, vec![Block::text("a")]),
            Message::user("q2"),
            own(
                &m,
                vec![
                    Block::Thinking { thinking: "t".into(), thinking_signature: Some("reasoning_content".into()) },
                    Block::ToolCall { id: "d1".into(), name: "read".into(), arguments: json!({}) },
                ],
            ),
            tool_result("d1", "r"),
            own(&m, vec![Block::ToolCall { id: "d2".into(), name: "read".into(), arguments: json!({}) }]),
            tool_result("d2", "r"),
        ];
        // DeepSeek rejects placeholders: turns without reasoning get the (empty) fallback.
        assert_eq!(
            body(&request(&m, Thinking::High, &[], &msgs))["messages"],
            json!([
                {"role":"system","content":"Be brief."},
                {"role":"user","content":[{"type":"text","text":"q"}]},
                {"role":"assistant","content":"a","reasoning_content":""},
                {"role":"user","content":[{"type":"text","text":"q2"}]},
                {"role":"assistant","content":"","tool_calls":[{"id":"d1","type":"function","function":{"name":"read","arguments":"{}"}}],"reasoning_content":"t"},
                {"role":"tool","tool_call_id":"d1","content":"r"},
                {"role":"assistant","content":"","tool_calls":[{"id":"d2","type":"function","function":{"name":"read","arguments":"{}"}}],"reasoning_content":""},
                {"role":"tool","tool_call_id":"d2","content":"r"},
            ])
        );
    }

    #[test]
    fn unparsable_arguments_replay_as_the_raw_string() {
        let m = kimi();
        let msgs = [
            Message::user("q"),
            own(
                &m,
                vec![Block::ToolCall {
                    id: "e1".into(),
                    name: "read".into(),
                    arguments: json!({"__invalid_json": "{\"path\": "}),
                }],
            ),
            tool_result("e1", "err"),
        ];
        let b = body(&request(&m, Thinking::Off, &[], &msgs));
        assert_eq!(b["messages"][2]["tool_calls"][0]["function"]["arguments"], "{\"path\": ");
    }

    #[test]
    fn glm_golden_body_with_tools() {
        let m = spec("zai-org/GLM-5.2");
        let msgs = [Message::user("hi")];
        let tools = [tool()];
        assert_eq!(
            body(&request(&m, Thinking::High, &tools, &msgs)),
            json!({
                "model": "zai-org/GLM-5.2",
                "messages": [
                    {"role":"system","content":"Be brief."},
                    {"role":"user","content":[{"type":"text","text":"hi"}]},
                ],
                "stream": true,
                "stream_options": {"include_usage": true},
                "max_completion_tokens": 1000,
                "tools": [{"type":"function","function":{
                    "name":"read","description":"Read a file.",
                    "parameters":{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}}}],
                "reasoning_effort": "high",
            })
        );
    }

    #[test]
    fn gpt_oss_golden_body_replays_a_tool_turn_without_reasoning() {
        let m = spec("openai/gpt-oss-120b");
        let msgs = [
            Message::user("q"),
            own(&m, vec![Block::ToolCall { id: "g1".into(), name: "grep".into(), arguments: json!({"pattern":"fn"}) }]),
            tool_result("g1", "none"),
            Message::user("next"),
        ];
        assert_eq!(
            body(&request(&m, Thinking::Max, &[], &msgs)),
            json!({
                "model": "openai/gpt-oss-120b",
                "messages": [
                    {"role":"system","content":"Be brief."},
                    {"role":"user","content":[{"type":"text","text":"q"}]},
                    {"role":"assistant","content":"","tool_calls":[{"id":"g1","type":"function","function":{"name":"grep","arguments":"{\"pattern\":\"fn\"}"}}]},
                    {"role":"tool","tool_call_id":"g1","content":"none"},
                    {"role":"user","content":[{"type":"text","text":"next"}]},
                ],
                "stream": true,
                "stream_options": {"include_usage": true},
                "max_completion_tokens": 1000,
                "reasoning_effort": "max",
            })
        );
    }

    fn drive(m: &ModelSpec, data: &[u8], step: usize) -> (Vec<Seen>, Result<Message, ProviderError>) {
        let mut a = Assembler::new(m);
        let mut seen = Vec::new();
        for c in data.chunks(step) {
            let r = a.feed(c, &mut |e| {
                if let Some(s) = seen_of(e) {
                    seen.push(s);
                }
            });
            match r {
                Ok(Some(msg)) => return (seen, Ok(msg)),
                Err(e) => return (seen, Err(e)),
                Ok(None) => {}
            }
        }
        match a.finish() {
            Some(msg) => (seen, Ok(msg)),
            None => (seen, Err(ProviderError::Fatal("stream ended without a message".into()))),
        }
    }

    #[derive(Debug, PartialEq)]
    enum Seen {
        Text(String),
        Thinking(String),
        Start(String, String),
        Delta(String, String),
    }

    fn seen_of(e: StreamEvent) -> Option<Seen> {
        Some(match e {
            StreamEvent::Text(t) => Seen::Text(t.into()),
            StreamEvent::Thinking(t) => Seen::Thinking(t.into()),
            StreamEvent::ToolCallStart { id, name } => Seen::Start(id.into(), name.into()),
            StreamEvent::ToolCallDelta { id, json } => Seen::Delta(id.into(), json.into()),
            StreamEvent::Retry { .. } => return None,
        })
    }

    fn content_json(m: &Message) -> Value {
        let Message::Assistant { content, .. } = m else { panic!("not an assistant message") };
        serde_json::to_value(content).unwrap()
    }

    #[test]
    fn interleaved_fixture_assembles_the_same_at_every_split() {
        let m = kimi();
        let (whole_seen, whole) = drive(&m, INTERLEAVED, usize::MAX);
        let whole = whole.unwrap();
        for step in [1, 2, 3, 5, 7, 64] {
            let (seen, msg) = drive(&m, INTERLEAVED, step);
            assert_eq!(seen, whole_seen, "step {step}");
            assert_eq!(content_json(&msg.unwrap()), content_json(&whole), "step {step}");
        }
        assert_eq!(
            content_json(&whole),
            json!([
                {"type":"thinking","thinking":"Let me check the file. Two files.","thinkingSignature":"reasoning_content"},
                {"type":"text","text":"Reading both."},
                {"type":"toolCall","id":"call_a1","name":"read","arguments":{"path":"a.rs"}},
                {"type":"toolCall","id":"call_b2","name":"grep","arguments":{"pattern":"fn"}},
            ])
        );
        assert!(whole_seen.contains(&Seen::Start("call_a1".into(), "read".into())));
        assert!(whole_seen.contains(&Seen::Start("call_b2".into(), "grep".into())));
        let Message::Assistant { stop_reason, usage, .. } = &whole else { panic!() };
        assert_eq!(*stop_reason, StopReason::ToolUse);
        // The usage chunk arrives after finish_reason and still counts.
        assert_eq!((usage.input, usage.cache_read, usage.output, usage.total_tokens), (20, 100, 34, 154));
    }

    #[test]
    fn length_fixture_keeps_partial_text_and_reasoning_alias() {
        let (_, msg) = drive(&kimi(), LENGTH, 3);
        let msg = msg.unwrap();
        let Message::Assistant { stop_reason, usage, .. } = &msg else { panic!() };
        assert_eq!(*stop_reason, StopReason::Length);
        assert_eq!(usage.output, 1000);
        assert_eq!(
            content_json(&msg),
            json!([
                {"type":"thinking","thinking":"Thinking hard about it","thinkingSignature":"reasoning"},
                {"type":"text","text":"The answer is"},
            ])
        );
    }

    #[test]
    fn error_chunk_is_retryable_after_partial_text() {
        let (seen, res) = drive(&kimi(), ERROR, 4);
        assert!(matches!(res, Err(ProviderError::Retryable { .. })), "{res:?}");
        assert_eq!(seen, [Seen::Text("Partial ".into()), Seen::Text("answer".into())]);
    }

    #[test]
    fn stream_without_done_is_a_message_once_finished() {
        let s = br#"data: {"id":"x","choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":"stop"}]}

data: {"choices":[],"usage":{"prompt_tokens":3,"completion_tokens":1}}

"#;
        let (seen, res) = drive(&kimi(), s, 1);
        let msg = res.unwrap();
        let Message::Assistant { stop_reason, usage, .. } = &msg else { panic!() };
        assert_eq!(*stop_reason, StopReason::Stop);
        assert_eq!(usage.output, 1);
        assert_eq!(seen, [Seen::Text("hi".into())]);
    }

    #[test]
    fn stream_without_finish_reason_is_not_a_message() {
        let s = br#"data: {"id":"x","choices":[{"index":0,"delta":{"content":"hi"}}]}

"#;
        let mut a = Assembler::new(&kimi());
        assert!(a.feed(s, &mut |_| {}).unwrap().is_none());
        assert!(a.finish().is_none());
    }

    #[test]
    fn tool_fragments_without_index_match_by_id_then_position() {
        let frames = [
            json!({"choices":[{"index":0,"delta":{"tool_calls":[{"id":"a1","function":{"name":"read","arguments":"{\"path\":"}}]}}]}),
            json!({"choices":[{"index":0,"delta":{"tool_calls":[{"function":{"arguments":"\"x\"}"}}]}}]}),
            json!({"choices":[{"index":0,"delta":{"tool_calls":[{"id":"a2","function":{"name":"grep","arguments":"{}"}}]}}]}),
            json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}),
        ];
        let mut data = String::new();
        for f in frames {
            data.push_str(&format!("data: {f}\n\n"));
        }
        data.push_str("data: [DONE]\n\n");
        let (seen, msg) = drive(&kimi(), data.as_bytes(), 1);
        assert_eq!(
            seen,
            [
                Seen::Start("a1".into(), "read".into()),
                Seen::Delta("a1".into(), "{\"path\":".into()),
                Seen::Delta("a1".into(), "\"x\"}".into()),
                Seen::Start("a2".into(), "grep".into()),
                Seen::Delta("a2".into(), "{}".into()),
            ]
        );
        assert_eq!(
            content_json(&msg.unwrap()),
            json!([
                {"type":"toolCall","id":"a1","name":"read","arguments":{"path":"x"}},
                {"type":"toolCall","id":"a2","name":"grep","arguments":{}},
            ])
        );
    }

    #[test]
    fn named_call_without_id_gets_a_stable_one() {
        let frames = [
            json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"name":"read","arguments":"{}"}}]}}]}),
            json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}),
        ];
        let mut data = String::new();
        for f in frames {
            data.push_str(&format!("data: {f}\n\n"));
        }
        data.push_str("data: [DONE]\n\n");
        let (seen, msg) = drive(&kimi(), data.as_bytes(), 3);
        assert_eq!(seen, [Seen::Start("call_0".into(), "read".into()), Seen::Delta("call_0".into(), "{}".into())]);
        assert_eq!(
            content_json(&msg.unwrap()),
            json!([{"type":"toolCall","id":"call_0","name":"read","arguments":{}}])
        );
    }

    #[test]
    fn tool_calls_finish_without_calls_is_a_stop() {
        let data = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"tool_calls\"}]}\n\n\
                    data: [DONE]\n\n";
        let (_, msg) = drive(&kimi(), data.as_bytes(), 5);
        let Message::Assistant { stop_reason, .. } = msg.unwrap() else { panic!() };
        assert_eq!(stop_reason, StopReason::Stop);
    }

    #[test]
    fn tool_arguments_before_name_are_replayed_on_start() {
        let frames = [
            json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"a\":"}}]}}]}),
            json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"c9","function":{"name":"x","arguments":"1}"}}]}}]}),
            json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}),
        ];
        let mut data = String::new();
        for f in frames {
            data.push_str(&format!("data: {f}\n\n"));
        }
        data.push_str("data: [DONE]\n\n");
        let (seen, msg) = drive(&kimi(), data.as_bytes(), 1);
        assert_eq!(
            seen,
            [
                Seen::Start("c9".into(), "x".into()),
                Seen::Delta("c9".into(), "{\"a\":".into()),
                Seen::Delta("c9".into(), "1}".into()),
            ]
        );
        assert_eq!(content_json(&msg.unwrap()), json!([{"type":"toolCall","id":"c9","name":"x","arguments":{"a":1}}]));
    }

    #[test]
    fn stream_failures_are_classed() {
        let class = |e: Value| match stream_failure(&e) {
            ProviderError::Retryable { .. } => "retry",
            ProviderError::Overflow(_) => "overflow",
            ProviderError::Fatal(_) => "fatal",
            ProviderError::Aborted => "aborted",
        };
        assert_eq!(class(json!({"message":"slow down","code":"rate_limit_exceeded"})), "retry");
        assert_eq!(class(json!({"message":"busy","code":503})), "retry");
        assert_eq!(class(json!({"message":"x","type":"server_error"})), "retry");
        assert_eq!(
            class(
                json!({"message":"This model's maximum context length is 8192 tokens","code":"context_length_exceeded"})
            ),
            "overflow"
        );
        assert_eq!(class(json!({"message":"invalid api key","code":"invalid_api_key"})), "fatal");
        assert_eq!(class(json!("plain text error")), "fatal");
    }

    /// One short real turn. Needs `BASETEN_API_KEY`; skipped (passes) without it.
    #[tokio::test(flavor = "current_thread")]
    #[ignore]
    async fn contract_native_baseten() {
        use futures_util::StreamExt;
        let Ok(key) = std::env::var("BASETEN_API_KEY") else {
            eprintln!("skipped: BASETEN_API_KEY is not set");
            return;
        };
        crate::provider::install_crypto();
        let m = spec("moonshotai/Kimi-K3");
        let msgs = [Message::user("Reply with the single word: pong")];
        let req = Request { max_tokens: Some(256), ..request(&m, Thinking::Low, &[], &msgs) };
        let route = Route {
            base: "https://inference.baseten.co/v1".into(),
            auth: Auth::Bearer(key.clone()),
            def: crate::provider::native::def("baseten"),
        };
        let mut w = wire(&req, &route).unwrap();
        let resp = reqwest::Client::new()
            .post(&w.url)
            .bearer_auth(&key)
            .header("content-type", "application/json")
            .body(w.body.to_string())
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success(), "status {}", resp.status());
        let mut stream = resp.bytes_stream();
        let mut done = None;
        while let Some(chunk) = stream.next().await {
            if let Some(msg) = w.decoder.feed(&chunk.unwrap(), &mut |_| {}).unwrap() {
                done = Some(msg);
                break;
            }
        }
        let msg = done.or_else(|| w.decoder.finish()).expect("the stream ended without a message");
        let Message::Assistant { content, stop_reason, .. } = &msg else { panic!() };
        assert!(!content.is_empty());
        assert!(matches!(stop_reason, StopReason::Stop | StopReason::Length), "{stop_reason:?}");
    }
}
