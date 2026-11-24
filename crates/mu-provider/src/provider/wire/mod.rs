//! The provider wire formats. Each API builds its own request and decodes its own stream; the
//! request loop in `client` drives them through `WireFormat` and does not branch on the api.

pub mod anthropic;
pub mod convert;
pub mod openai_completions;
pub mod openai_responses;
pub mod usage;

use super::client::Route;
use super::sse::SseDecoder;
use super::types::{Decoder, ProviderError, Request, StreamEvent};
use mu_base::message::Message;
use mu_base::models::{Api, ModelSpec};
use serde_json::Value;

/// One API's request shape and stream decoding.
pub trait WireFormat {
    /// The complete URL for a request to `base`.
    fn url(&self, base: &str) -> String;
    /// Protocol headers beyond the content type and auth.
    fn headers(&self, _req: &Request) -> Vec<(&'static str, String)> {
        Vec::new()
    }
    fn body(&self, req: &Request) -> Value;
    /// The stream decoder. It maps in-stream error frames to their retry class itself.
    fn decoder(&self, req: &Request) -> Box<dyn Decoder>;
}

/// The wire for `model` over `route`. Only anthropic depends on the route: a direct request sends
/// its own body and betas.
pub fn select(model: &ModelSpec, route: &Route) -> Box<dyn WireFormat> {
    match model.api {
        Api::AnthropicMessages => Box::new(anthropic::Anthropic { direct: route.is_direct() }),
        Api::OpenAiResponses => Box::new(openai_responses::Responses),
        Api::OpenAiCompletions => Box::new(openai_completions::Completions),
    }
}

/// What the shared SSE loop needs from a wire: its frame splitter and its per-frame handler.
pub trait SseFrames {
    fn sse(&mut self) -> &mut SseDecoder;
    fn frame(&mut self, data: &str, ev: &mut dyn FnMut(StreamEvent)) -> Result<Option<Message>, ProviderError>;
}

/// Feeds one chunk through the wire's frames, stopping at the frame that completes the message.
pub fn feed<F: SseFrames>(
    wire: &mut F,
    chunk: &[u8],
    ev: &mut dyn FnMut(StreamEvent),
) -> Result<Option<Message>, ProviderError> {
    let frames = wire.sse().push(chunk);
    if wire.sse().overflow {
        return Err(ProviderError::Fatal("stream frame larger than 16 MB".into()));
    }
    for (_, data) in frames {
        if let Some(m) = wire.frame(&data, ev)? {
            return Ok(Some(m));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod wire_contract_tests {
    use super::convert::normalize_id;
    use super::*;
    use mu_base::message::{Block, StopReason, Usage};
    use mu_base::models::Thinking;
    use mu_base::models::{Api, ThinkingMode, ThinkingSpec};
    use serde_json::json;
    use std::cell::Cell;
    use std::rc::Rc;
    const LONG_ID: &str = "call_0123456789abcdefghijklmn|fc_0123456789abcdef0123456789abcdef";

    fn spec(id: &str, api: Api, mode: ThinkingMode) -> ModelSpec {
        let (provider, wire_id) = id.split_once('/').unwrap();
        ModelSpec {
            id: id.into(),
            provider: provider.into(),
            context_window: 200_000,
            max_output: 8000,
            api,
            native: true,
            wire_id: wire_id.into(),
            thinking: ThinkingSpec { mode, efforts: vec!["low".into(), "high".into()] },
            images: true,
            compat: None,
            anthropic: Default::default(),
        }
    }

    fn assistant(content: Vec<Block>, api: &str, provider: &str, model: &str) -> Message {
        Message::Assistant {
            content,
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

    fn result(id: &str) -> Message {
        Message::ToolResult {
            tool_call_id: id.into(),
            tool_name: "read".into(),
            content: vec![Block::text("ok")],
            is_error: false,
            timestamp: 0,
            details: None,
        }
    }

    fn request<'a>(model: &'a ModelSpec, messages: &'a [Message]) -> Request<'a> {
        Request {
            model,
            thinking: Rc::new(Cell::new(Thinking::High)),
            system: "sys",
            tools: &[],
            messages,
            max_tokens: None,
            session_id: Some("s"),
        }
    }

    #[test]
    fn normalize_id_is_length_safe_and_stable() {
        assert_eq!(LONG_ID.len(), 65);
        let n = normalize_id(LONG_ID);
        assert_eq!(n.len(), 64);
        assert_eq!(n, normalize_id(LONG_ID));
        assert_ne!(n, normalize_id(&LONG_ID.replace("fc_0123", "fc_9999")));
        assert_eq!(normalize_id("toolu_01A"), "toolu_01A");
        assert_eq!(normalize_id("a b.c"), "a_b_c");

        // A tool_use and its tool_result map to the same id on both wires that strip `|`.
        let msgs = [
            assistant(
                vec![Block::ToolCall { id: LONG_ID.into(), name: "read".into(), arguments: json!({}) }],
                "anthropic-messages",
                "anthropic",
                "anthropic/claude-sonnet-5-5",
            ),
            result(LONG_ID),
        ];
        let anth = spec("anthropic/claude-sonnet-5-5", Api::AnthropicMessages, ThinkingMode::AnthropicAdaptive);
        let wire = Value::Array(anthropic::to_wire(&msgs, &anth)).to_string();
        assert!(!wire.contains(LONG_ID));
        assert_eq!(wire.matches(&format!("\"{n}\"")).count(), 2, "{wire}");

        // Chat Completions keeps a same-model id verbatim (omp does) and normalises another model's.
        let bt = spec("baseten/moonshotai/Kimi-K3", Api::OpenAiCompletions, ThinkingMode::Effort);
        let call = |api: &str, provider: &str, model: &str| {
            [
                assistant(
                    vec![Block::ToolCall { id: LONG_ID.into(), name: "read".into(), arguments: json!({}) }],
                    api,
                    provider,
                    model,
                ),
                result(LONG_ID),
            ]
        };
        let msgs = call("openai-responses", "openai", "openai/gpt-5.5");
        let wire = Value::Array(openai_completions::to_wire("", &msgs, &bt)).to_string();
        assert!(!wire.contains(LONG_ID));
        assert_eq!(wire.matches(&format!("\"{n}\"")).count(), 2, "{wire}");
        let msgs = call("openai-completions", "baseten", "baseten/moonshotai/Kimi-K3");
        let wire = Value::Array(openai_completions::to_wire("", &msgs, &bt)).to_string();
        assert_eq!(wire.matches(&format!("\"{LONG_ID}\"")).count(), 2, "{wire}");
    }

    /// pi's own sessions are v3 JSONL of the same shape; opening one by path (`--resume <file>`) must keep working,
    /// including its string user content, system messages, usage and label entries.
    #[test]
    fn a_pi_session_loads_and_replays_to_the_wire() {
        let dir = std::env::temp_dir().join(format!("mu-pi-session-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.jsonl");
        std::fs::write(&path, include_str!("testdata/pi_session.jsonl")).unwrap();
        let loaded = mu_session::session::format::load(&path).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
        assert_eq!(loaded.model.as_deref(), Some("anthropic/claude-sonnet-5-5"));
        assert_eq!(loaded.thinking.as_deref(), Some("medium"));
        let msgs = loaded.messages;
        // user, assistant, tool result, assistant, the extension's message as a reminder, user, assistant
        assert_eq!(msgs.len(), 7);
        let anth = spec("anthropic/claude-sonnet-5-5", Api::AnthropicMessages, ThinkingMode::AnthropicAdaptive);
        let body = anthropic::native::body(&request(&anth, &msgs)).0.to_string();
        assert!(body.contains("Read a.txt and tell me what it says.") && body.contains("It says hello."), "{body}");
        assert!(body.contains("EqQBCkYIARgCIiQ3Zm9vYmFyYmF6MTIzNDU2"), "its own signature replays: {body}");
        assert!(!body.contains("You are an expert coding assistant"), "pi's system message is not mu's: {body}");
    }

    #[test]
    fn an_omp_session_loads_and_replays_only_to_its_own_wire() {
        let dir = std::env::temp_dir().join(format!("mu-omp-session-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.jsonl");
        std::fs::write(&path, include_str!("testdata/omp_session.jsonl")).unwrap();
        let loaded = mu_session::session::format::load(&path).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
        let msgs = loaded.messages;
        assert_eq!(msgs.len(), 11);

        let anth = spec("anthropic/claude-sonnet-5-5", Api::AnthropicMessages, ThinkingMode::AnthropicAdaptive);
        let resp = spec("openai/gpt-5.5", Api::OpenAiResponses, ThinkingMode::Effort);
        let bt = spec("baseten/moonshotai/Kimi-K3", Api::OpenAiCompletions, ThinkingMode::Effort);

        // Anthropic: its own signed thinking replays; no OpenAI reasoning item leaks, native or gateway.
        let r = request(&anth, &msgs);
        let native = anthropic::native::body(&r).0.to_string();
        let gateway = anthropic::body(&r).to_string();
        for body in [&native, &gateway] {
            assert!(body.contains("EqQBCkYIARgCIiQ3Zm9vYmFyYmF6MTIzNDU2"), "own signature replays");
            assert!(!body.contains("rs_0a1b2c3d4e5f") && !body.contains("gAAAAABfakeEncrypted"), "{body}");
            assert!(!body.contains("Reading the file first") && !body.contains("Two files"), "{body}");
        }

        // Responses: its own reasoning item and the long call id replay; Anthropic and Chat
        // Completions thinking never do.
        let r = openai_responses::body(&request(&resp, &msgs)).to_string();
        assert!(r.contains("rs_0a1b2c3d4e5f") && r.contains("gAAAAABfakeEncrypted"), "{r}");
        assert!(r.contains("call_0123456789abcdefghijklmnopqr") && r.contains("fc_0123456789abcdef0123456789abcdef"));
        assert!(!r.contains("EqQBCk") && !r.contains("The user wants") && !r.contains("Two files"), "{r}");

        // Chat Completions: no reasoning item or signature; other models' thinking is demoted to
        // plain text (as omp does), never sent as `reasoning_content`.
        let c = Value::Array(openai_completions::to_wire("sys", &msgs, &bt)).to_string();
        assert!(!c.contains("rs_0a1b2c3d4e5f") && !c.contains("EqQBCk") && !c.contains("gAAAAABfake"), "{c}");
        assert!(c.contains("The user wants") && !c.contains("\"reasoning_content\":\"The user wants"), "{c}");
    }
}
