//! The request body of a native Anthropic call (an API key against `api.anthropic.com`), as
//! omp builds it for API-key accounts. The gateway route keeps `anthropic::body`: the gateway
//! adapts thinking and adds prompt caching itself.

use super::{base_body, clamp_budget, output_cap, to_wire, tools_json};
use crate::provider::types::Request;
use mu_base::models::{ModelSpec, Thinking, ThinkingMode, clamp_effort};
use serde_json::{Value, json};

const EFFORT_BETA: &str = "effort-2025-11-24";
const CONTEXT_MANAGEMENT_BETA: &str = "context-management-2025-06-27";
const FINE_GRAINED_TOOLS_BETA: &str = "fine-grained-tool-streaming-2025-05-14";
const INTERLEAVED_BETA: &str = "interleaved-thinking-2025-05-14";

/// What the reasoning level turns into on the wire.
#[derive(Debug, Default, PartialEq)]
pub struct ThinkingParams {
    pub thinking: Option<Value>,
    pub effort: Option<String>,
    /// Replayed thinking must be kept: `context_management` with `clear_thinking keep: all`.
    pub keep_thinking: bool,
    pub interleaved: bool,
}

fn adaptive(m: &ModelSpec) -> bool {
    m.thinking.mode == ThinkingMode::AnthropicAdaptive && !m.anthropic.disable_adaptive_thinking
}

/// Thinking by the model's mode (omp's `buildParams`): adaptive models take `thinking: adaptive`
/// plus `output_config.effort`; budget models take a token budget, and budget-effort ones an
/// effort too. "Off" is never `disabled` on a model that rejects it. A model that requires thinking
/// is never switched off: it thinks with no effort set.
pub fn thinking_params(m: &ModelSpec, level: Thinking, max_tokens: u32) -> ThinkingParams {
    let q = &m.anthropic;
    match m.thinking.mode {
        ThinkingMode::None => return ThinkingParams::default(),
        ThinkingMode::Gateway | ThinkingMode::Effort => return ThinkingParams::default(),
        _ => {}
    }
    let forced = level == Thinking::Off && q.requires_thinking_enabled && q.disabled_thinking.is_none();
    if level == Thinking::Off && !forced {
        return off(m);
    }
    let effort = if forced { None } else { clamp_effort(level, &m.thinking.efforts).map(String::from) };
    if adaptive(m) {
        let mut t = json!({"type":"adaptive"});
        // Opus 4.7+ and the 5-series omit thinking content unless asked; older models reject `display`.
        if q.supports_display == Some(true) {
            t["display"] = "summarized".into();
        }
        return ThinkingParams { thinking: Some(t), effort, keep_thinking: true, interleaved: false };
    }
    // A forced budget is omp's default when no budget is given.
    let budget = if forced { Some(1024) } else { clamp_budget(level.budget(), max_tokens) };
    match budget {
        Some(b) => {
            let mut t = json!({"type":"enabled","budget_tokens":b});
            if q.supports_display != Some(false) {
                t["display"] = "summarized".into();
            }
            ThinkingParams {
                thinking: Some(t),
                effort: (m.thinking.mode == ThinkingMode::BudgetEffort).then_some(effort).flatten(),
                keep_thinking: true,
                interleaved: q.supports_display != Some(true),
            }
        }
        // No room for a budget: send the request without thinking.
        None => off(m),
    }
}

/// "Off" as the model's quirks say. A `between_tools` request at `xhigh` or `max` effort is a 400,
/// so thinking is dropped and the effort kept.
fn off(m: &ModelSpec) -> ThinkingParams {
    let mut p = off_params(m);
    let at_max = matches!(p.effort.as_deref(), Some("xhigh" | "max"));
    if at_max && p.thinking.as_ref().is_some_and(|t| t["type"] == "between_tools") {
        p.thinking = None;
    }
    p
}

fn off_params(m: &ModelSpec) -> ThinkingParams {
    let q = &m.anthropic;
    let tag = |t: &str| Some(json!({"type": t}));
    match q.disabled_thinking.as_deref() {
        Some("between-tools") => {
            return ThinkingParams {
                thinking: tag("between_tools"),
                effort: q.between_tools_effort.clone(),
                ..Default::default()
            };
        }
        Some("omit") => return ThinkingParams::default(),
        Some("adaptive") => {
            let mut t = json!({"type":"adaptive"});
            if q.supports_display == Some(true) {
                t["display"] = "summarized".into();
            }
            return ThinkingParams { thinking: Some(t), ..Default::default() };
        }
        Some(_) => return ThinkingParams { thinking: tag("disabled"), ..Default::default() },
        None => {}
    }
    if q.supports_between_tools_thinking {
        return ThinkingParams {
            thinking: tag("between_tools"),
            effort: q.between_tools_effort.clone(),
            ..Default::default()
        };
    }
    if adaptive(m) {
        // Adaptive-only models reject `disabled` and default to thinking on: omit the field and
        // pin the lowest effort so "off" stays cheap.
        return ThinkingParams { effort: Some("low".into()), ..Default::default() };
    }
    ThinkingParams { thinking: tag("disabled"), ..Default::default() }
}

/// Marks the last block of the last message a breakpoint; never a thinking block.
fn mark_last_message(messages: &mut [Value]) {
    let Some(last) = messages.last_mut() else { return };
    let Some(blocks) = last["content"].as_array_mut() else { return };
    for b in blocks.iter_mut().rev() {
        if matches!(b["type"].as_str(), Some("text" | "image" | "tool_use" | "tool_result")) {
            b["cache_control"] = json!({"type":"ephemeral"});
            return;
        }
    }
}

/// The `anthropic-beta` values a request needs, without building its body.
pub fn betas(req: &Request) -> Vec<&'static str> {
    betas_for(req, &thinking_params(req.model, req.thinking.get(), output_cap(req)))
}

fn betas_for(req: &Request, tp: &ThinkingParams) -> Vec<&'static str> {
    let mut betas = Vec::new();
    if !req.tools.is_empty() {
        betas.push(FINE_GRAINED_TOOLS_BETA);
    }
    if tp.effort.is_some() {
        betas.push(EFFORT_BETA);
    }
    if tp.keep_thinking && req.model.anthropic.supports_context_management != Some(false) {
        betas.push(CONTEXT_MANAGEMENT_BETA);
    }
    if tp.interleaved {
        betas.push(INTERLEAVED_BETA);
    }
    betas
}

/// The JSON body and the `anthropic-beta` values it needs. Prompt-cache breakpoints (5-minute
/// ephemeral) sit on the last tool, the last system block and the last message: three of the four allowed.
pub fn body(req: &Request) -> (Value, Vec<&'static str>) {
    let m = req.model;
    let max_tokens = output_cap(req);
    let tp = thinking_params(m, req.thinking.get(), max_tokens);
    let betas = betas_for(req, &tp);
    let mut messages = to_wire(req.messages, m);
    mark_last_message(&mut messages);
    let mut body = base_body(&m.wire_id, max_tokens, messages);
    if !req.system.is_empty() {
        body["system"] = json!([{"type":"text","text":req.system,"cache_control":{"type":"ephemeral"}}]);
    }
    if !req.tools.is_empty() {
        let mut tools = tools_json(req.tools);
        if let Some(last) = tools.last_mut() {
            last["cache_control"] = json!({"type":"ephemeral"});
        }
        body["tools"] = json!(tools);
    }
    if let Some(t) = tp.thinking {
        body["thinking"] = t;
    }
    if let Some(e) = tp.effort {
        body["output_config"] = json!({"effort": e});
    }
    if tp.keep_thinking && m.anthropic.supports_context_management != Some(false) {
        body["context_management"] = json!({"edits":[{"type":"clear_thinking_20251015","keep":"all"}]});
    }
    (body, betas)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mu_base::message::{Block, Message};
    use mu_base::tool::ToolDef;
    use std::cell::Cell;
    use std::rc::Rc;

    fn spec(id: &str) -> ModelSpec {
        crate::model::catalog::row("anthropic", id).unwrap().spec("anthropic", 1, 1).unwrap()
    }

    fn build(m: &ModelSpec, level: Thinking, messages: &[Message], tools: &[ToolDef]) -> (Value, Vec<&'static str>) {
        let req = Request {
            model: m,
            thinking: Rc::new(Cell::new(level)),
            system: "be brief",
            tools,
            messages,
            max_tokens: Some(20_000),
            session_id: None,
        };
        let built = body(&req);
        assert_eq!(betas(&req), built.1, "the header betas match the body's");
        built
    }

    fn tool() -> ToolDef {
        ToolDef { name: "t".into(), description: "d".into(), schema: json!({"type":"object"}) }
    }

    fn conversation() -> Vec<Message> {
        vec![Message::user("hi")]
    }

    #[test]
    fn adaptive_models_send_adaptive_and_an_effort() {
        let m = spec("claude-sonnet-5-5");
        let (b, betas) = build(&m, Thinking::High, &conversation(), &[]);
        assert_eq!(b["model"], "claude-sonnet-5-5", "the wire id, not provider/model");
        assert_eq!(b["thinking"], json!({"type":"adaptive","display":"summarized"}));
        assert_eq!(b["output_config"], json!({"effort":"high"}));
        assert_eq!(b["context_management"], json!({"edits":[{"type":"clear_thinking_20251015","keep":"all"}]}));
        assert_eq!(betas, [EFFORT_BETA, CONTEXT_MANAGEMENT_BETA]);
        assert!(b.get("temperature").is_none());
        // mu's levels map onto the model's ladder
        let (b, _) = build(&m, Thinking::Max, &conversation(), &[]);
        assert_eq!(b["output_config"]["effort"], "max");
        let sonnet46 = spec("claude-sonnet-4-6"); // low, medium, high
        let (b, _) = build(&sonnet46, Thinking::XHigh, &conversation(), &[]);
        assert_eq!(b["output_config"]["effort"], "high");
        assert_eq!(b["thinking"], json!({"type":"adaptive"}), "no display on models that reject it");
    }

    #[test]
    fn off_never_sends_disabled_to_a_model_that_rejects_it() {
        // Sonnet 5.5: its lowest setting is between_tools.
        let (b, betas) = build(&spec("claude-sonnet-5-5"), Thinking::Off, &conversation(), &[]);
        assert_eq!(b["thinking"], json!({"type":"between_tools"}));
        assert!(b.get("output_config").is_none() && b.get("context_management").is_none());
        assert!(betas.is_empty(), "{betas:?}");
        // Opus 4.6: adaptive-only. Omit thinking, pin the lowest effort.
        let (b, betas) = build(&spec("claude-opus-4-6"), Thinking::Off, &conversation(), &[]);
        assert!(b.get("thinking").is_none(), "{b}");
        assert_eq!(b["output_config"], json!({"effort":"low"}));
        assert_eq!(betas, [EFFORT_BETA]);
        // Budget models can be switched off.
        let (b, _) = build(&spec("claude-haiku-4-5"), Thinking::Off, &conversation(), &[]);
        assert_eq!(b["thinking"], json!({"type":"disabled"}));
    }

    #[test]
    fn budget_models_send_a_budget_and_budget_effort_models_an_effort() {
        let (b, betas) = build(&spec("claude-haiku-4-5"), Thinking::Medium, &conversation(), &[]);
        assert_eq!(b["thinking"], json!({"type":"enabled","budget_tokens":8192,"display":"summarized"}));
        assert!(b.get("output_config").is_none());
        assert_eq!(betas, [CONTEXT_MANAGEMENT_BETA, INTERLEAVED_BETA]);
        let (b, betas) = build(&spec("claude-opus-4-5"), Thinking::High, &conversation(), &[]);
        assert_eq!(b["thinking"]["budget_tokens"], 16384);
        assert_eq!(b["output_config"], json!({"effort":"high"}));
        assert!(betas.contains(&EFFORT_BETA));
        // The budget leaves room for the answer; with none left, thinking is off.
        let m = spec("claude-haiku-4-5");
        let req = Request {
            model: &m,
            thinking: Rc::new(Cell::new(Thinking::Max)),
            system: "",
            tools: &[],
            messages: &[],
            max_tokens: Some(2000),
            session_id: None,
        };
        let (b, _) = body(&req);
        assert_eq!(b["thinking"], json!({"type":"disabled"}));
        assert!(b.get("system").is_none(), "an empty system prompt is not sent");
    }

    #[test]
    fn tools_add_the_streaming_beta() {
        let (b, betas) = build(&spec("claude-sonnet-5-5"), Thinking::Off, &conversation(), &[tool()]);
        assert_eq!(b["tools"][0]["name"], "t");
        assert_eq!(betas, [FINE_GRAINED_TOOLS_BETA]);
    }

    #[test]
    fn golden_tool_round_trip_with_three_breakpoints() {
        let m = spec("claude-sonnet-5-5");
        let msgs = vec![
            Message::user("hi"),
            Message::Assistant {
                content: vec![
                    Block::Thinking { thinking: "hm".into(), thinking_signature: Some("sig".into()) },
                    Block::text("answer"),
                    Block::ToolCall { id: "c1".into(), name: "t".into(), arguments: json!({}) },
                ],
                api: "anthropic-messages".into(),
                provider: "anthropic".into(),
                model: "anthropic/claude-sonnet-5-5".into(),
                usage: Default::default(),
                stop_reason: mu_base::message::StopReason::ToolUse,
                timestamp: 0,
                response_id: None,
                error_message: None,
            },
            Message::ToolResult {
                tool_call_id: "c1".into(),
                tool_name: "t".into(),
                content: vec![Block::text("ok")],
                is_error: false,
                timestamp: 0,
                details: None,
            },
        ];
        let (b, betas) = build(&m, Thinking::High, &msgs, &[tool()]);
        // Three breakpoints: the last tool, the system block and the last message. The replayed
        // thinking block carries none.
        assert_eq!(
            b,
            json!({
                "model": "claude-sonnet-5-5",
                "max_tokens": 20000,
                "stream": true,
                "system": [{"type":"text","text":"be brief","cache_control":{"type":"ephemeral"}}],
                "tools": [{"name":"t","description":"d","input_schema":{"type":"object"},"cache_control":{"type":"ephemeral"}}],
                "messages": [
                    {"role":"user","content":[{"type":"text","text":"hi"}]},
                    {"role":"assistant","content":[
                        {"type":"thinking","thinking":"hm","signature":"sig"},
                        {"type":"text","text":"answer"},
                        {"type":"tool_use","id":"c1","name":"t","input":{}}
                    ]},
                    {"role":"user","content":[
                        {"type":"tool_result","tool_use_id":"c1","content":[{"type":"text","text":"ok"}],"is_error":false,"cache_control":{"type":"ephemeral"}}
                    ]}
                ],
                "thinking": {"type":"adaptive","display":"summarized"},
                "output_config": {"effort":"high"},
                "context_management": {"edits":[{"type":"clear_thinking_20251015","keep":"all"}]}
            })
        );
        assert_eq!(betas, [FINE_GRAINED_TOOLS_BETA, EFFORT_BETA, CONTEXT_MANAGEMENT_BETA]);
    }

    #[test]
    fn golden_adaptive_body() {
        let (b, betas) = build(&spec("claude-sonnet-5-5"), Thinking::High, &conversation(), &[]);
        assert_eq!(
            b,
            json!({
                "model": "claude-sonnet-5-5",
                "max_tokens": 20000,
                "stream": true,
                "system": [{"type":"text","text":"be brief","cache_control":{"type":"ephemeral"}}],
                "messages": [{"role":"user","content":[{"type":"text","text":"hi","cache_control":{"type":"ephemeral"}}]}],
                "thinking": {"type":"adaptive","display":"summarized"},
                "output_config": {"effort":"high"},
                "context_management": {"edits":[{"type":"clear_thinking_20251015","keep":"all"}]}
            })
        );
        assert_eq!(betas, [EFFORT_BETA, CONTEXT_MANAGEMENT_BETA]);
    }

    #[test]
    fn golden_budget_body() {
        let (b, betas) = build(&spec("claude-haiku-4-5"), Thinking::Medium, &conversation(), &[]);
        assert_eq!(
            b,
            json!({
                "model": "claude-haiku-4-5",
                "max_tokens": 20000,
                "stream": true,
                "system": [{"type":"text","text":"be brief","cache_control":{"type":"ephemeral"}}],
                "messages": [{"role":"user","content":[{"type":"text","text":"hi","cache_control":{"type":"ephemeral"}}]}],
                "thinking": {"type":"enabled","budget_tokens":8192,"display":"summarized"},
                "context_management": {"edits":[{"type":"clear_thinking_20251015","keep":"all"}]}
            })
        );
        assert_eq!(betas, [CONTEXT_MANAGEMENT_BETA, INTERLEAVED_BETA]);
    }

    #[test]
    fn golden_off_on_an_adaptive_only_model() {
        let (b, betas) = build(&spec("claude-opus-4-6"), Thinking::Off, &conversation(), &[]);
        assert_eq!(
            b,
            json!({
                "model": "claude-opus-4-6",
                "max_tokens": 20000,
                "stream": true,
                "system": [{"type":"text","text":"be brief","cache_control":{"type":"ephemeral"}}],
                "messages": [{"role":"user","content":[{"type":"text","text":"hi","cache_control":{"type":"ephemeral"}}]}],
                "output_config": {"effort":"low"}
            })
        );
        assert_eq!(betas, [EFFORT_BETA]);
    }

    #[test]
    fn between_tools_is_dropped_at_xhigh_and_max_effort() {
        let mut m = spec("claude-sonnet-5-5");
        m.anthropic.between_tools_effort = Some("xhigh".into());
        let (b, _) = build(&m, Thinking::Off, &conversation(), &[]);
        assert!(b.get("thinking").is_none(), "{b}");
        assert_eq!(b["output_config"], json!({"effort":"xhigh"}));
        m.anthropic.between_tools_effort = Some("high".into());
        let (b, _) = build(&m, Thinking::Off, &conversation(), &[]);
        assert_eq!(b["thinking"], json!({"type":"between_tools"}));
        assert_eq!(b["output_config"], json!({"effort":"high"}));
    }

    #[test]
    fn adaptive_off_asks_for_summaries_only_where_display_is_accepted() {
        let mut m = spec("claude-sonnet-5-5");
        m.anthropic.disabled_thinking = Some("adaptive".into());
        m.anthropic.supports_display = Some(true);
        let (b, _) = build(&m, Thinking::Off, &conversation(), &[]);
        assert_eq!(b["thinking"], json!({"type":"adaptive","display":"summarized"}));
        m.anthropic.supports_display = None;
        let (b, _) = build(&m, Thinking::Off, &conversation(), &[]);
        assert_eq!(b["thinking"], json!({"type":"adaptive"}));
    }

    #[test]
    fn a_model_that_requires_thinking_thinks_without_an_effort_when_off() {
        let mut m = spec("claude-opus-4-6");
        m.anthropic.requires_thinking_enabled = true;
        let (b, betas) = build(&m, Thinking::Off, &conversation(), &[]);
        assert_eq!(b["thinking"], json!({"type":"adaptive"}));
        assert!(b.get("output_config").is_none(), "{b}");
        assert_eq!(betas, [CONTEXT_MANAGEMENT_BETA], "replayed thinking is kept");
        let mut h = spec("claude-haiku-4-5");
        h.anthropic.requires_thinking_enabled = true;
        let (b, _) = build(&h, Thinking::Off, &conversation(), &[]);
        assert_eq!(b["thinking"], json!({"type":"enabled","budget_tokens":1024,"display":"summarized"}));
    }

    #[test]
    fn an_assistant_tail_is_marked_on_its_last_cacheable_block() {
        let mut msgs = vec![json!({"role":"assistant","content":[
            {"type":"text","text":"a"},
            {"type":"thinking","thinking":"t","signature":"s"},
        ]})];
        mark_last_message(&mut msgs);
        assert_eq!(msgs[0]["content"][0]["cache_control"], json!({"type":"ephemeral"}));
        assert!(msgs[0]["content"][1].get("cache_control").is_none());
        let mut none: Vec<Value> = vec![];
        mark_last_message(&mut none);
    }

    #[test]
    fn unknown_ids_get_a_plain_budget() {
        let m = mu_base::models::resolve(
            "anthropic/claude-future-9",
            &Default::default(),
            None,
            &crate::provider::providers::direct_spec,
        )
        .unwrap()
        .0;
        let (b, _) = build(&m, Thinking::High, &conversation(), &[]);
        assert_eq!(b["model"], "claude-future-9");
        assert_eq!(b["thinking"]["type"], "enabled");
        assert!(b["thinking"].get("output_config").is_none());
    }

    #[test]
    fn the_gateway_body_is_unchanged() {
        let m = ModelSpec::gateway("anthropic/claude-sonnet-5-5", 1, 1);
        let req = Request {
            model: &m,
            thinking: Rc::new(Cell::new(Thinking::High)),
            system: "s",
            tools: &[],
            messages: &[],
            max_tokens: Some(20_000),
            session_id: None,
        };
        let b = super::super::body(&req);
        assert_eq!(b["model"], "anthropic/claude-sonnet-5-5");
        assert_eq!(b["thinking"], json!({"type":"enabled","budget_tokens":16384}));
        assert_eq!(b["output_config"], json!({"effort":"high"}));
        assert!(!b.to_string().contains("cache_control"));
    }
}
