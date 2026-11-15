use crate::ask_nudge;
use crate::events::AgentEvent;
use crate::models::{Api, ModelSpec, same_origin};
use crate::tool::ToolOutput;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Block {
    Text {
        text: String,
    },
    #[serde(rename_all = "camelCase")]
    Thinking {
        thinking: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thinking_signature: Option<String>,
    },
    ToolCall {
        id: String,
        name: String,
        arguments: Value,
    },
    /// Encrypted thinking the API returns instead of readable thinking; it must be sent back unchanged.
    RedactedThinking {
        data: String,
    },
    Image(Image),
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Cost {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    pub total: f64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub total_tokens: u64,
    pub cost: Cost,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
/// Why a response ended. `Error` and `Aborted` are never produced by mu, but omp writes them and
/// its sessions must still load, so they stay readable.
pub enum StopReason {
    #[serde(rename = "stop")]
    Stop,
    #[serde(rename = "length")]
    Length,
    #[serde(rename = "toolUse")]
    ToolUse,
    #[serde(rename = "error")]
    Error,
    #[serde(rename = "aborted")]
    Aborted,
}

fn blocks_or_string<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<Block>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum C {
        S(String),
        B(Vec<Block>),
    }
    Ok(match C::deserialize(d)? {
        C::S(text) => vec![Block::text(text)],
        C::B(b) => b,
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "camelCase")]
pub enum Message {
    #[serde(rename = "user")]
    User {
        #[serde(deserialize_with = "blocks_or_string")]
        content: Vec<Block>,
        timestamp: i64,
        /// Sent by mu itself (an approved plan), not typed by the user; the UI does not echo it.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        synthetic: bool,
        /// What the user typed (`/ship --keep`) when `content` is a slash command's or skill's expansion,
        /// so readers of the session can tell the template's words from the user's.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        command: Option<String>,
    },
    #[serde(rename = "assistant", rename_all = "camelCase")]
    Assistant {
        content: Vec<Block>,
        api: String,
        provider: String,
        model: String,
        usage: Usage,
        stop_reason: StopReason,
        timestamp: i64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        response_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error_message: Option<String>,
    },
    #[serde(rename = "toolResult", rename_all = "camelCase")]
    ToolResult {
        tool_call_id: String,
        tool_name: String,
        content: Vec<Block>,
        is_error: bool,
        timestamp: i64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<Value>,
    },
}

pub fn now_ms() -> i64 {
    jiff::Timestamp::now().as_millisecond()
}

/// Base64 image data with its MIME type; serializes like omp's `ImageContent`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Image {
    pub data: String,
    pub mime_type: String,
}

/// A user prompt: text plus attached images.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UserInput {
    pub text: String,
    pub images: Vec<Image>,
    /// The slash command `text` was expanded from, as typed; see `Message::User::command`.
    pub command: Option<String>,
}

impl From<String> for UserInput {
    fn from(text: String) -> Self {
        Self { text, ..Default::default() }
    }
}

impl From<&str> for UserInput {
    fn from(text: &str) -> Self {
        Self { text: text.into(), ..Default::default() }
    }
}

/// User-role text that mu injected rather than the user typed: job and inbox notices, reminders,
/// `!cmd` output and the compaction summary.
pub fn is_injected(text: &str) -> bool {
    const PREFIXES: [&str; 3] = ["<system-notice>", "<system-reminder>", "Prior model work/tool state available."];
    let t = text.trim_start();
    PREFIXES.iter().any(|p| t.starts_with(p)) || (t.starts_with("I ran `") && t.contains("`:\n```"))
}

/// The text blocks of `blocks`, one per line.
pub fn text_of(blocks: &[Block]) -> String {
    blocks
        .iter()
        .filter_map(|b| if let Block::Text { text } = b { Some(text.as_str()) } else { None })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Bytes `v` takes as JSON, counted without building the string.
pub(crate) fn json_len(v: &Value) -> usize {
    struct Count(usize);
    impl std::io::Write for Count {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0 += b.len();
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut c = Count(0);
    let _ = serde_json::to_writer(&mut c, v);
    c.0
}

/// Tokens a flat image is counted as.
const IMAGE_TOKENS: u64 = 1600;
/// Rough bytes of framing each message adds on the wire (role, ids, block wrappers).
const MESSAGE_OVERHEAD: usize = 24;

impl Block {
    fn est_bytes(&self) -> usize {
        match self {
            Block::Text { text } => text.len(),
            Block::Thinking { thinking, thinking_signature } => {
                thinking.len() + thinking_signature.as_ref().map_or(0, String::len)
            }
            Block::ToolCall { id, name, arguments } => id.len() + name.len() + json_len(arguments),
            Block::RedactedThinking { data } => data.len(),
            Block::Image(_) | Block::Unknown => 0,
        }
    }
}

/// Bytes an assistant block adds to a request to `target`: only what that wire actually sends.
fn assistant_bytes(
    block: &Block,
    api: &str,
    provider: &str,
    model: &str,
    stop: StopReason,
    target: &ModelSpec,
) -> usize {
    match block {
        Block::Text { text } => text.len(),
        Block::ToolCall { id, name, arguments } => id.len() + name.len() + json_len(arguments),
        Block::Image(_) | Block::Unknown => 0,
        Block::Thinking { thinking, thinking_signature } => {
            let origin = same_origin(api, provider, target);
            match target.api {
                // Signed same-origin thinking replays verbatim; unsigned and foreign thinking is dropped.
                Api::AnthropicMessages => match thinking_signature {
                    Some(sig) if origin => thinking.len() + sig.len(),
                    _ => 0,
                },
                // Only the signature (a reasoning item) is replayed, and the text never is.
                Api::OpenAiResponses => {
                    let replayable = origin && model == target.id && stop != StopReason::Error;
                    match thinking_signature {
                        Some(sig) if replayable => sig.len(),
                        _ => 0,
                    }
                }
                // Same-origin thinking goes as reasoning; foreign thinking is demoted to `<think>` text.
                Api::OpenAiCompletions => thinking.len(),
            }
        }
        Block::RedactedThinking { data } => match target.api {
            Api::AnthropicMessages if same_origin(api, provider, target) => data.len(),
            _ => 0,
        },
    }
}

impl Block {
    pub fn text(text: impl Into<String>) -> Self {
        Block::Text { text: text.into() }
    }
}

impl Message {
    pub fn user(text: &str) -> Self {
        Message::User { content: vec![Block::text(text)], timestamp: now_ms(), synthetic: false, command: None }
    }

    /// A user-role message the user did not type and the transcript does not echo.
    pub fn synthetic_user(text: &str) -> Self {
        Message::User { content: vec![Block::text(text)], timestamp: now_ms(), synthetic: true, command: None }
    }

    /// A user message: the text, then one image block per attachment.
    pub fn user_input(i: &UserInput) -> Self {
        // An image-only prompt has no text block: the API rejects empty ones.
        let text = (!i.text.is_empty() || i.images.is_empty()).then(|| Block::text(i.text.as_str()));
        let content = text.into_iter().chain(i.images.iter().cloned().map(Block::Image)).collect();
        Message::User { content, timestamp: now_ms(), synthetic: false, command: i.command.clone() }
    }

    /// Token estimate of what this message adds to a request to `target`: about 4 bytes per token
    /// over the text, thinking and tool arguments that wire sends, a flat amount per image (its
    /// bytes are not counted), and a small framing allowance. Counts lengths only; nothing is
    /// serialized into a string.
    pub fn estimate_tokens(&self, target: &ModelSpec) -> u64 {
        let content = self.content();
        let bytes: usize = MESSAGE_OVERHEAD
            + match self {
                Message::Assistant { api, provider, model, stop_reason, .. } => content
                    .iter()
                    .map(|b| assistant_bytes(b, api, provider, model, *stop_reason, target))
                    .sum::<usize>(),
                _ => content.iter().map(Block::est_bytes).sum(),
            }
            + match self {
                Message::ToolResult { tool_call_id, tool_name, .. } => tool_call_id.len() + tool_name.len(),
                _ => 0,
            };
        // An assistant image is dropped by every wire; only user and tool-result images are sent.
        let images = match self {
            Message::Assistant { .. } => 0,
            _ => content.iter().filter(|b| matches!(b, Block::Image(_))).count() as u64,
        };
        bytes as u64 / 4 + IMAGE_TOKENS * images
    }

    pub fn timestamp(&self) -> i64 {
        match self {
            Message::User { timestamp, .. }
            | Message::Assistant { timestamp, .. }
            | Message::ToolResult { timestamp, .. } => *timestamp,
        }
    }

    fn content(&self) -> &[Block] {
        match self {
            Message::User { content, .. }
            | Message::Assistant { content, .. }
            | Message::ToolResult { content, .. } => content,
        }
    }

    /// `(image count, total base64 bytes)`.
    pub fn image_stats(&self) -> (u64, usize) {
        self.content().iter().fold((0, 0), |(n, b), blk| match blk {
            Block::Image(i) => (n + 1, b + i.data.len()),
            _ => (n, b),
        })
    }

    /// A copy with every image replaced by a `[image]` text block, for payloads that must stay small.
    pub fn without_images(&self) -> Message {
        let mut m = self.clone();
        let (Message::User { content, .. } | Message::Assistant { content, .. } | Message::ToolResult { content, .. }) =
            &mut m;
        for b in content.iter_mut() {
            if matches!(b, Block::Image(_)) {
                *b = Block::text("[image]");
            }
        }
        m
    }
}

pub fn tool_calls(m: &Message) -> Vec<(String, String, Value)> {
    match m {
        Message::Assistant { content, .. } => content
            .iter()
            .filter_map(|b| match b {
                Block::ToolCall { id, name, arguments } => Some((id.clone(), name.clone(), arguments.clone())),
                _ => None,
            })
            .collect(),
        _ => vec![],
    }
}

/// A reply that is only `.` (declining a reminder): kept in the session, never rendered.
pub fn is_dismissal(m: &Message) -> bool {
    matches!(m, Message::Assistant { content, .. } if tool_calls(m).is_empty() && text_of(content).trim() == ".")
}

/// The UI events that show a stored conversation: what a resumed session replays and what the jobs
/// panel renders for a subagent's session.
pub fn history_events(messages: &[Message]) -> Vec<AgentEvent> {
    let mut out = Vec::new();
    let mut after_reminder = false;
    for m in messages {
        let reminded = std::mem::replace(
            &mut after_reminder,
            matches!(m, Message::User { content, .. } if ask_nudge::is_reminder(&text_of(content))),
        );
        match m {
            Message::User { synthetic: true, .. } => out.push(AgentEvent::Notice("▸ Plan approved".into())),
            Message::User { content, .. } if ask_nudge::is_reminder(&text_of(content)) => {}
            Message::User { content, .. } => out.push(AgentEvent::UserPrompt(text_of(content))),
            Message::Assistant { .. } if reminded && is_dismissal(m) => {}
            Message::Assistant { content, .. } => {
                for b in content {
                    match b {
                        Block::Thinking { thinking, .. } if !thinking.trim().is_empty() => {
                            out.push(AgentEvent::Thinking(thinking.clone()));
                        }
                        Block::Text { text } => {
                            out.push(AgentEvent::Text(text.clone()));
                            out.push(AgentEvent::BlockEnd);
                        }
                        Block::ToolCall { id, name, arguments } => {
                            out.push(AgentEvent::ToolStart {
                                id: id.clone(),
                                name: name.clone(),
                                args: arguments.clone(),
                            });
                        }
                        _ => {}
                    }
                }
            }
            Message::ToolResult { tool_call_id, tool_name, content, is_error, details, .. } => {
                let output = ToolOutput::from_stored(text_of(content), *is_error, details.clone());
                out.push(AgentEvent::ToolEnd { id: tool_call_id.clone(), name: tool_name.clone(), output });
            }
        }
    }
    out
}

fn synthetic_result(id: String, name: String) -> Message {
    Message::ToolResult {
        tool_call_id: id,
        tool_name: name,
        content: vec![Block::text("Tool call was not completed (missing result in session history).")],
        is_error: true,
        timestamp: now_ms(),
        details: None,
    }
}

/// Repairs tool pairing: drops `tool_result`s with no preceding `tool_use` and adds an error
/// `tool_result` for every `tool_use` left unanswered. `ids` (parallel to `messages`) is kept
/// aligned; synthetic messages get an empty id. Returns one description per repair.
pub fn repair_tool_pairs_with_ids(messages: &mut Vec<Message>, mut ids: Option<&mut Vec<String>>) -> Vec<String> {
    let mut notes = Vec::new();
    let old = std::mem::take(messages);
    let old_ids = ids.as_deref_mut().map(std::mem::take);
    let mut pending: Vec<(String, String)> = Vec::new();
    let mut out_ids: Vec<String> = Vec::new();
    for (i, m) in old.into_iter().enumerate() {
        let eid = old_ids.as_ref().and_then(|v| v.get(i).cloned()).unwrap_or_default();
        let flush = |pending: &mut Vec<(String, String)>,
                     messages: &mut Vec<Message>,
                     out_ids: &mut Vec<String>,
                     notes: &mut Vec<String>| {
            for (id, name) in pending.drain(..) {
                notes.push(format!("added error result for unanswered tool call {id}"));
                messages.push(synthetic_result(id, name));
                out_ids.push(String::new());
            }
        };
        match &m {
            Message::ToolResult { tool_call_id, .. } => {
                if let Some(p) = pending.iter().position(|(id, _)| id == tool_call_id) {
                    pending.remove(p);
                } else {
                    notes.push(format!("dropped orphaned tool result {tool_call_id} (entry {eid})"));
                    continue;
                }
            }
            _ => {
                flush(&mut pending, messages, &mut out_ids, &mut notes);
                pending = tool_calls(&m).into_iter().map(|(id, name, _)| (id, name)).collect();
            }
        }
        messages.push(m);
        out_ids.push(eid);
    }
    for (id, name) in pending.drain(..) {
        notes.push(format!("added error result for unanswered tool call {id}"));
        messages.push(synthetic_result(id, name));
        out_ids.push(String::new());
    }
    if let Some(ids) = ids {
        *ids = out_ids;
    }
    notes
}

pub fn repair_tool_pairs(messages: &mut Vec<Message>) -> Vec<String> {
    repair_tool_pairs_with_ids(messages, None)
}

/// Every `tool_use` has exactly one later `tool_result` and every `tool_result` has a `tool_use`.
pub fn check_tool_pairs(messages: &[Message]) -> Result<(), String> {
    let mut notes = Vec::new();
    let mut pending: Vec<&str> = Vec::new();
    for m in messages {
        match m {
            Message::ToolResult { tool_call_id, .. } => match pending.iter().position(|id| *id == tool_call_id) {
                Some(p) => {
                    pending.remove(p);
                }
                None => notes.push(format!("dropped orphaned tool result {tool_call_id}")),
            },
            _ => {
                notes.extend(pending.drain(..).map(|id| format!("added error result for unanswered tool call {id}")));
                if let Message::Assistant { content, .. } = m {
                    pending = content
                        .iter()
                        .filter_map(|b| match b {
                            Block::ToolCall { id, .. } => Some(id.as_str()),
                            _ => None,
                        })
                        .collect();
                }
            }
        }
    }
    notes.extend(pending.into_iter().map(|id| format!("added error result for unanswered tool call {id}")));
    if notes.is_empty() { Ok(()) } else { Err(notes.join("; ")) }
}

#[cfg(test)]
mod pair_tests {
    use super::*;

    fn call(id: &str) -> Message {
        Message::Assistant {
            content: vec![Block::ToolCall { id: id.into(), name: "bash".into(), arguments: Value::Null }],
            api: String::new(),
            provider: String::new(),
            model: String::new(),
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
            tool_name: "bash".into(),
            content: vec![],
            is_error: false,
            timestamp: 0,
            details: None,
        }
    }

    #[test]
    fn valid_pairs_pass() {
        assert!(check_tool_pairs(&[Message::user("a"), call("1"), result("1"), Message::user("b")]).is_ok());
    }

    #[test]
    fn repairs_orphans_both_ways() {
        let mut m = vec![Message::user("a"), result("x"), call("1"), Message::user("b")];
        let mut ids: Vec<String> = ["a", "x", "c", "b"].map(String::from).to_vec();
        let notes = repair_tool_pairs_with_ids(&mut m, Some(&mut ids));
        assert_eq!(notes.len(), 2);
        assert!(check_tool_pairs(&m).is_ok());
        assert_eq!(m.len(), ids.len());
        assert_eq!(m.len(), 4);
        assert!(matches!(&m[2], Message::ToolResult { is_error: true, .. }));
        assert_eq!(ids, ["a", "c", "", "b"]);
    }

    #[test]
    fn trailing_call_gets_result() {
        let mut m = vec![call("1")];
        assert_eq!(repair_tool_pairs(&mut m).len(), 1);
        assert_eq!(m.len(), 2);
    }
}

#[cfg(test)]
mod estimate_tests {
    use super::*;

    fn target() -> ModelSpec {
        ModelSpec::gateway("anthropic/x", 200_000, 32_000)
    }

    fn assistant(content: Vec<Block>, api: &str, provider: &str, model: &str) -> Message {
        Message::Assistant {
            content,
            api: api.into(),
            provider: provider.into(),
            model: model.into(),
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            timestamp: 0,
            response_id: None,
            error_message: None,
        }
    }

    #[test]
    fn counts_lengths_not_image_bytes_and_text_of_joins_lines() {
        let target = target();
        let short = Message::user("x");
        let long = Message::user(&"y".repeat(4000));
        assert!(long.estimate_tokens(&target) >= short.estimate_tokens(&target) + 999);
        let with_image = Message::User {
            content: vec![
                Block::text("hi"),
                Block::Image(Image { data: "A".repeat(1_000_000), mime_type: "image/png".into() }),
            ],
            timestamp: 0,
            synthetic: false,
            command: None,
        };
        let est = with_image.estimate_tokens(&target);
        assert!((1600..1700).contains(&est), "a flat 1600 for the image, its bytes not counted: {est}");
        let call = Message::Assistant {
            content: vec![Block::ToolCall {
                id: "t".into(),
                name: "bash".into(),
                arguments: serde_json::json!({"command": "x".repeat(400)}),
            }],
            api: String::new(),
            provider: String::new(),
            model: String::new(),
            usage: Usage::default(),
            stop_reason: StopReason::ToolUse,
            timestamp: 0,
            response_id: None,
            error_message: None,
        };
        assert!(call.estimate_tokens(&target) >= 100, "tool arguments count");
        assert_eq!(text_of(&[Block::text("a"), Block::Unknown, Block::text("b")]), "a\nb");
    }

    #[test]
    fn anthropic_counts_foreign_thinking_as_nothing_on_the_wire() {
        let target = target();
        let signed = |provider: &str| {
            assistant(
                vec![
                    Block::Thinking { thinking: "t".repeat(400), thinking_signature: Some("s".repeat(40_000)) },
                    Block::RedactedThinking { data: "r".repeat(40_000) },
                ],
                "anthropic-messages",
                provider,
                "anthropic/x",
            )
        };
        assert!(signed("openai").estimate_tokens(&target) < 200, "foreign thinking is dropped");
        assert!(signed("anthropic").estimate_tokens(&target) > 20_000, "same-origin thinking replays");
    }

    #[test]
    fn responses_counts_the_signature_but_not_the_thinking_text() {
        let mut target = ModelSpec::gateway("openai/gpt-5", 200_000, 32_000);
        target.api = Api::OpenAiResponses;
        let sig = "s".repeat(4000);
        let with_text = assistant(
            vec![Block::Thinking { thinking: "t".repeat(400), thinking_signature: Some(sig.clone()) }],
            "openai-responses",
            "openai",
            "openai/gpt-5",
        );
        let bare = assistant(
            vec![Block::Thinking { thinking: String::new(), thinking_signature: Some(sig) }],
            "openai-responses",
            "openai",
            "openai/gpt-5",
        );
        let est = with_text.estimate_tokens(&target);
        assert_eq!(est, bare.estimate_tokens(&target), "the thinking text is never sent");
        assert!((950..1100).contains(&est), "the signature is sent: {est}");
    }

    #[test]
    fn an_assistant_image_costs_nothing() {
        let image = assistant(
            vec![Block::Image(Image { data: "A".repeat(1_000_000), mime_type: "image/png".into() })],
            "anthropic-messages",
            "anthropic",
            "anthropic/x",
        );
        assert!(image.estimate_tokens(&target()) < 10, "every wire drops assistant images");
    }

    #[test]
    fn a_command_prompt_keeps_what_was_typed_and_plain_ones_store_nothing_extra() {
        let input = UserInput { text: "Ship it".into(), command: Some("/ship --keep".into()), ..Default::default() };
        let v = serde_json::to_value(Message::user_input(&input)).unwrap();
        assert_eq!(v["command"], "/ship --keep");
        let back: Message = serde_json::from_value(v).unwrap();
        assert!(matches!(back, Message::User { command: Some(c), .. } if c == "/ship --keep"));
        let plain = serde_json::to_value(Message::user("hi")).unwrap();
        assert!(plain.get("command").is_none(), "{plain}");
        let old: Message = serde_json::from_str(r#"{"role":"user","content":"hi","timestamp":1}"#).unwrap();
        assert!(matches!(old, Message::User { command: None, .. }));
    }

    #[test]
    fn injected_user_text_is_told_apart_from_typed_text() {
        for t in [
            "<system-notice>\njob done",
            "<system-reminder>\nx",
            "I ran `ls`:\n```\n```",
            "Prior model work/tool state available.\n",
        ] {
            assert!(is_injected(t), "{t}");
        }
        for t in ["please fix the bug", "what is <system-notice>?", "I ran `cargo test` and it failed"] {
            assert!(!is_injected(t), "{t}");
        }
    }
}
