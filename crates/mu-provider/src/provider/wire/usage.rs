//! Stop reasons and token usage as each wire reports them. Each wire keeps its own `total_tokens`
//! formula: they are not comparable across wires, so none is normalised here.

use mu_base::message::{StopReason, Usage};
use serde_json::Value;

/// Anthropic's `message_delta.stop_reason`.
pub fn anthropic_stop(reason: &str) -> StopReason {
    match reason {
        "tool_use" => StopReason::ToolUse,
        "max_tokens" | "model_context_window_exceeded" => StopReason::Length,
        _ => StopReason::Stop,
    }
}

/// Anthropic's `message_delta.usage`. A `message_delta` may carry only some fields: those present
/// replace the running values.
pub fn anthropic_usage(usage: &mut Usage, u: &Value) {
    let g = |k: &str| u[k].as_u64();
    if let Some(x) = g("input_tokens") {
        usage.input = x;
    }
    if let Some(x) = g("output_tokens") {
        usage.output = x;
    }
    if let Some(x) = g("cache_read_input_tokens") {
        usage.cache_read = x;
    }
    if let Some(x) = g("cache_creation_input_tokens") {
        usage.cache_write = x;
    }
    usage.total_tokens = usage.input + usage.output + usage.cache_read + usage.cache_write;
}

/// `input_tokens` includes the cached part, which is reported apart as `cache_read`. Reasoning
/// tokens are already inside `output_tokens`.
pub fn responses_usage(u: &Value) -> Usage {
    let input = u["input_tokens"].as_u64().unwrap_or(0);
    let cache_read = u["input_tokens_details"]["cached_tokens"].as_u64().unwrap_or(0).min(input);
    let mut usage = Usage {
        input: input - cache_read,
        output: u["output_tokens"].as_u64().unwrap_or(0),
        cache_read,
        ..Usage::default()
    };
    usage.total_tokens = usage.input + usage.output + usage.cache_read;
    usage
}

/// `prompt_tokens` includes the cached ones, which are reported apart as `cache_read`.
pub fn completions_usage(u: &Value) -> Usage {
    let prompt = u["prompt_tokens"].as_u64().unwrap_or(0);
    let output = u["completion_tokens"].as_u64().unwrap_or(0);
    let cached = u["prompt_tokens_details"]["cached_tokens"].as_u64().unwrap_or(0).min(prompt);
    Usage { input: prompt - cached, output, cache_read: cached, total_tokens: prompt + output, ..Usage::default() }
}

/// A `finish_reason` as mu's stop reason. A stop that carries tool calls is a tool turn.
pub fn chat_stop(reason: Option<&str>, has_calls: bool) -> (StopReason, Option<String>) {
    match reason {
        None | Some("stop" | "end") => (if has_calls { StopReason::ToolUse } else { StopReason::Stop }, None),
        Some("length" | "max_tokens") => (StopReason::Length, None),
        Some("tool_calls" | "function_call") => (if has_calls { StopReason::ToolUse } else { StopReason::Stop }, None),
        Some(other) => (StopReason::Error, Some(format!("Provider finish_reason: {other}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anthropic_stop_reasons() {
        assert_eq!(anthropic_stop("model_context_window_exceeded"), StopReason::Length);
        assert_eq!(anthropic_stop("max_tokens"), StopReason::Length);
        assert_eq!(anthropic_stop("tool_use"), StopReason::ToolUse);
        assert_eq!(anthropic_stop("end_turn"), StopReason::Stop);
    }
}
