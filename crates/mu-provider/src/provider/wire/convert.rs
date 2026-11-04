//! What the wires share: the tool-pair backstop, provider-safe tool-call ids and their replay
//! origin, and the shapes of tool lists and unavailable images.

use mu_base::message::{Image, Message};
use mu_base::models::ModelSpec;
use mu_base::tool::ToolDef;
use serde_json::Value;
use std::borrow::Cow;

/// `messages` with unpaired tool calls and results repaired: the backstop, so no wire sends an
/// unpaired one whatever the history holds. Borrowed when the history already pairs.
pub fn paired(messages: &[Message]) -> Cow<'_, [Message]> {
    if mu_base::message::check_tool_pairs(messages).is_ok() {
        return Cow::Borrowed(messages);
    }
    let mut repaired = messages.to_vec();
    mu_base::message::repair_tool_pairs(&mut repaired);
    Cow::Owned(repaired)
}

/// The tool list of a wire: `entry` shapes each tool's name, description and schema.
pub fn tool_list(tools: &[ToolDef], entry: impl Fn(&str, &str, &Value) -> Value) -> Vec<Value> {
    tools.iter().map(|t| entry(&t.name, &t.description, &t.schema)).collect()
}

/// The text that stands in for an image whose bytes the blob store no longer has.
pub fn unavailable(i: &Image) -> String {
    format!("[image unavailable: {}]", i.mime_type)
}

/// Replace characters outside `[A-Za-z0-9_-]` with `_`, then cap at 64 characters (the strictest
/// provider limit). A longer id keeps its first 47 characters plus a hash of the whole original, so
/// the same input always gives the same output and a tool call and its result still pair up.
pub fn normalize_id(id: &str) -> String {
    let clean: String =
        id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect();
    if clean.len() <= 64 {
        return clean;
    }
    format!("{}_{:016x}", &clean[..47], fnv1a(id.as_bytes()))
}

/// FNV-1a: unlike `DefaultHasher`, its output is fixed across Rust versions, so ids stay stable.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, b| (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3))
}

/// Thinking blocks are only replayable to the same api + provider that produced them.
pub fn same_origin(api: &str, provider: &str, model: &ModelSpec) -> bool {
    api == model.api.as_str() && provider == model.provider
}
