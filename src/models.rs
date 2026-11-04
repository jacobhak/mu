use crate::config::Configs;
use anyhow::{Result, anyhow, bail};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Thinking {
    Off,
    Low,
    Medium,
    High,
    XHigh,
    Max,
}

impl Thinking {
    pub const NAMES: [&'static str; 6] = ["off", "low", "medium", "high", "xhigh", "max"];

    pub fn as_str(self) -> &'static str {
        Self::NAMES[self as usize]
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "off" => Self::Off,
            "low" | "minimal" => Self::Low,
            "medium" => Self::Medium,
            "high" => Self::High,
            "xhigh" => Self::XHigh,
            "max" => Self::Max,
            _ => return None,
        })
    }

    pub fn cycle(self) -> Self {
        match self {
            Self::Off => Self::Low,
            Self::Low => Self::Medium,
            Self::Medium => Self::High,
            Self::High => Self::XHigh,
            Self::XHigh => Self::Max,
            Self::Max => Self::Off,
        }
    }

    pub fn budget(self) -> Option<u32> {
        match self {
            Self::Off => None,
            Self::Low => Some(4096),
            Self::Medium => Some(8192),
            Self::High => Some(16384),
            Self::XHigh => Some(32768),
            Self::Max => Some(64000),
        }
    }
}

/// Wire protocol of a request. The strings are omp's (`Message::Assistant.api`), so sessions stay
/// readable by omp in both directions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Api {
    AnthropicMessages,
    OpenAiResponses,
    OpenAiCompletions,
}

impl Api {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AnthropicMessages => "anthropic-messages",
            Self::OpenAiResponses => "openai-responses",
            Self::OpenAiCompletions => "openai-completions",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "anthropic-messages" => Self::AnthropicMessages,
            "openai-responses" => Self::OpenAiResponses,
            "openai-completions" => Self::OpenAiCompletions,
            _ => return None,
        })
    }
}

/// How a model takes its reasoning level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThinkingMode {
    /// Gateway-routed: mu sends a budget and `output_config.effort`, the gateway adapts them.
    #[default]
    Gateway,
    /// Not a reasoning model: no thinking parameters.
    None,
    /// Anthropic `thinking: {type: enabled, budget_tokens}`.
    Budget,
    /// Anthropic budget plus `output_config.effort`.
    BudgetEffort,
    /// Anthropic `thinking: {type: adaptive}` plus `output_config.effort`.
    AnthropicAdaptive,
    /// OpenAI-style `reasoning.effort` / `reasoning_effort`.
    Effort,
}

impl ThinkingMode {
    /// Parses omp's catalog `thinking.mode`; an unknown mode is treated as no reasoning.
    pub fn parse(s: &str) -> Self {
        match s {
            "budget" => Self::Budget,
            "anthropic-budget-effort" => Self::BudgetEffort,
            "anthropic-adaptive" => Self::AnthropicAdaptive,
            "effort" => Self::Effort,
            _ => Self::None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ThinkingSpec {
    pub mode: ThinkingMode,
    /// The efforts the model accepts, lowest first, as omp names them (`minimal`..`max`).
    pub efforts: Vec<String>,
}

fn effort_rank(name: &str) -> Option<i32> {
    Some(match name {
        "minimal" => 0,
        "low" => 1,
        "medium" => 2,
        "high" => 3,
        "xhigh" => 4,
        "max" => 5,
        _ => return None,
    })
}

/// The effort in `efforts` nearest to mu's `level` (ties go to the lower one). `None` for
/// `Off`, or when the ladder is empty.
pub fn clamp_effort(level: Thinking, efforts: &[String]) -> Option<&str> {
    if level == Thinking::Off {
        return None;
    }
    let want = level as i32;
    efforts
        .iter()
        .filter_map(|e| Some((e.as_str(), effort_rank(e)?)))
        .min_by_key(|(_, r)| ((r - want).abs(), *r))
        .map(|(e, _)| e)
}

/// Per-model flags for Chat Completions servers, as omp resolves them in its catalog.
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CompletionsCompat {
    pub max_tokens_field: String,
    pub supports_developer_role: bool,
    pub supports_multiple_system_messages: bool,
    pub supports_usage_in_streaming: bool,
    pub reasoning_content_field: Option<String>,
    pub requires_reasoning_content_for_tool_calls: bool,
    pub requires_reasoning_content_for_all_assistant_turns: bool,
    pub requires_assistant_content_for_tool_calls: bool,
    /// Whether reasoning replayed on tool turns may be a synthetic `"."` when none was captured.
    pub allows_synthetic_reasoning_content_for_tool_calls: bool,
    /// The value sent for missing reasoning where synthetic placeholders are refused.
    pub synthetic_reasoning_content_fallback: Option<String>,
    /// `omit`, `lowest-effort` or `none-effort`.
    pub reasoning_disable_mode: Option<String>,
    pub thinking_format: Option<String>,
    pub strip_image_input: bool,
}

impl Default for CompletionsCompat {
    fn default() -> Self {
        Self {
            max_tokens_field: "max_tokens".into(),
            supports_developer_role: false,
            supports_multiple_system_messages: true,
            supports_usage_in_streaming: true,
            reasoning_content_field: None,
            requires_reasoning_content_for_tool_calls: false,
            requires_reasoning_content_for_all_assistant_turns: false,
            requires_assistant_content_for_tool_calls: false,
            allows_synthetic_reasoning_content_for_tool_calls: true,
            synthetic_reasoning_content_fallback: None,
            reasoning_disable_mode: None,
            thinking_format: None,
            strip_image_input: false,
        }
    }
}

/// Per-model flags for the native Anthropic wire, from omp's catalog.
#[derive(Debug, Clone, Default, PartialEq, serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AnthropicQuirks {
    /// `thinking.supportsDisplay`: adaptive thinking may ask for `display: "summarized"`.
    /// `None` means unknown, which budget thinking treats as supported.
    #[serde(skip)]
    pub supports_display: Option<bool>,
    /// The model's lowest thinking setting is `between_tools`; it rejects `disabled`.
    pub supports_between_tools_thinking: bool,
    pub between_tools_effort: Option<String>,
    /// What "thinking off" sends: `between-tools`, `omit`, `adaptive`, anything else is `disabled`.
    pub disabled_thinking: Option<String>,
    pub requires_thinking_enabled: bool,
    pub disable_adaptive_thinking: bool,
    /// `None` means supported.
    pub supports_context_management: Option<bool>,
}

#[derive(Debug, Clone)]
pub struct ModelSpec {
    /// `provider/model`
    pub id: String,
    pub provider: String,
    pub context_window: u32,
    pub max_output: u32,
    pub api: Api,
    /// Routed to the provider's own API with the user's key rather than through the gateway.
    pub native: bool,
    /// The id without the `provider/` prefix, which a native request sends; the gateway takes `id`.
    pub wire_id: String,
    pub thinking: ThinkingSpec,
    pub images: bool,
    pub compat: Option<CompletionsCompat>,
    pub anthropic: AnthropicQuirks,
}

impl ModelSpec {
    /// A gateway-routed Anthropic Messages model.
    pub fn gateway(id: &str, context_window: u32, max_output: u32) -> Self {
        let (provider, wire_id) = id.split_once('/').unwrap_or(("", id));
        Self {
            id: id.into(),
            provider: provider.into(),
            context_window,
            max_output,
            api: Api::AnthropicMessages,
            native: false,
            wire_id: wire_id.into(),
            thinking: ThinkingSpec::default(),
            images: true,
            compat: None,
            anthropic: AnthropicQuirks::default(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct GatewayModel {
    pub id: String,
    pub context_length: Option<u32>,
    pub max_output_tokens: Option<u32>,
}

const DEFAULT_WINDOW: u32 = 200_000;
const DEFAULT_MAX_OUT: u32 = 32_000;

/// Splits a trailing `:level`. `auto`/`inherit` mean "no level".
pub fn split_level(v: &str) -> (String, Option<Thinking>) {
    if let Some((m, l)) = v.rsplit_once(':') {
        return match l {
            "auto" | "inherit" => (m.into(), None),
            _ => match Thinking::parse(l) {
                Some(t) => (m.into(), Some(t)),
                None => (v.into(), None),
            },
        };
    }
    (v.into(), None)
}

/// Roles tried in order for an internal call.
pub fn role_chain(role: &str) -> Vec<String> {
    let rest: &[&str] = match role {
        "memory" => &["smol", "default"],
        "smol" | "task" | "plan" => &["default"],
        "review" => &["plan", "default"],
        _ => &[],
    };
    std::iter::once(role).chain(rest.iter().copied()).map(String::from).collect()
}

/// First configured role in the chain, as a selector string (`@role`).
pub fn role_selector(role: &str, cfg: &Configs) -> Option<String> {
    role_chain(role).into_iter().find(|r| cfg.role(r).is_some()).map(|r| format!("@{r}"))
}

/// `native(provider)` says whether the provider is routed natively (the user has a key for it).
pub fn resolve(
    sel: &str,
    cfg: &Configs,
    gateway_models: Option<&[GatewayModel]>,
    native: &dyn Fn(&str) -> bool,
) -> Result<(ModelSpec, Option<Thinking>)> {
    let (base, mut level) = split_level(sel);
    let base = match base.strip_prefix('@') {
        Some(role) => {
            let v = cfg.role(role).ok_or_else(|| anyhow!("model role @{role} is not configured"))?;
            let (m, l) = split_level(&v);
            level = level.or(l);
            m
        }
        None => base,
    };
    let Some((provider, model)) = base.split_once('/') else {
        bail!("model must be provider/model or @role, got `{base}`");
    };
    let pc = cfg.models.providers.get(provider);
    let entry = pc.and_then(|p| p.models.iter().find(|m| m.id == model));
    let ov = pc.and_then(|p| p.model_overrides.get(model));
    let gw = gateway_models.and_then(|g| g.iter().find(|m| m.id == base));
    let mut spec = match native(provider).then(|| native_spec(provider, model)).flatten() {
        Some(s) => s,
        None => ModelSpec::gateway(&base, DEFAULT_WINDOW, DEFAULT_MAX_OUT),
    };
    // Explicit models.yml entries win, then overrides, then what the catalog or gateway reports.
    let known = spec.native.then_some((spec.context_window, spec.max_output));
    spec.context_window = entry
        .and_then(|m| m.context_window)
        .or(ov.and_then(|o| o.context_window))
        .or(known.map(|k| k.0))
        .or(gw.and_then(|g| g.context_length))
        .unwrap_or(DEFAULT_WINDOW);
    spec.max_output = entry
        .and_then(|m| m.max_tokens)
        .or(ov.and_then(|o| o.max_tokens))
        .or(known.map(|k| k.1))
        .or(gw.and_then(|g| g.max_output_tokens))
        .unwrap_or(DEFAULT_MAX_OUT);
    Ok((spec, level))
}

/// A natively routed model: its catalog row, else a conservative spec for the provider's wire
/// (Anthropic gets a plain thinking budget; reasoning is never guessed for other unknown ids).
fn native_spec(provider: &str, model: &str) -> Option<ModelSpec> {
    let def = crate::provider::native::def(provider)?;
    // A discovered listing is fresher than the snapshot's numbers; the snapshot row still decides
    // thinking and request quirks.
    let found = crate::provider::discovery::find(provider, model);
    if let Some(mut spec) =
        crate::provider::catalog::row(provider, model).and_then(|r| r.spec(provider, DEFAULT_WINDOW, DEFAULT_MAX_OUT))
    {
        if let Some(d) = &found {
            spec.context_window = d.context_length.unwrap_or(spec.context_window);
            spec.max_output = d.max_completion_tokens.unwrap_or(spec.max_output);
        }
        return Some(spec);
    }
    if let Some(d) = &found
        && let Some(spec) = crate::provider::discovery::spec(provider, d, DEFAULT_WINDOW, DEFAULT_MAX_OUT)
    {
        return Some(spec);
    }
    let mut spec = ModelSpec::gateway(&format!("{provider}/{model}"), DEFAULT_WINDOW, DEFAULT_MAX_OUT);
    spec.api = def.api;
    spec.native = true;
    spec.images = def.api != Api::OpenAiCompletions;
    spec.compat = (def.api == Api::OpenAiCompletions).then(CompletionsCompat::default);
    spec.thinking.mode = if def.api == Api::AnthropicMessages { ThinkingMode::Budget } else { ThinkingMode::None };
    Some(spec)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::*;

    fn cfg() -> Configs {
        let omp: OmpConfig = serde_yaml_ng::from_str(
            "modelRoles:\n  default: anthropic/claude-sonnet-5-5:medium\n  smol: modal/a/b:high\n",
        )
        .unwrap();
        let models: ModelsYml = serde_yaml_ng::from_str(
            "providers:\n  modal:\n    models:\n      - id: a/b\n        contextWindow: 1000\n  openai-codex:\n    modelOverrides:\n      g:\n        contextWindow: 272000\n",
        )
        .unwrap();
        Configs { omp, models, mu: MuConfig::default(), ..Default::default() }
    }

    #[test]
    fn level_split() {
        assert_eq!(split_level("a/b:xhigh"), ("a/b".into(), Some(Thinking::XHigh)));
        assert_eq!(split_level("a/b:max"), ("a/b".into(), Some(Thinking::Max)));
        assert_eq!(split_level("a/b:minimal"), ("a/b".into(), Some(Thinking::Low)));
        assert_eq!(split_level("a/b:auto"), ("a/b".into(), None));
        assert_eq!(split_level("a/b:c"), ("a/b:c".into(), None));
    }

    #[test]
    fn levels_parse_cycle_and_budget() {
        for t in [Thinking::Off, Thinking::Low, Thinking::Medium, Thinking::High, Thinking::XHigh, Thinking::Max] {
            assert_eq!(Thinking::parse(t.as_str()), Some(t));
        }
        assert_eq!(Thinking::parse("ultra"), None);
        let mut t = Thinking::Off;
        let order: Vec<&str> = (0..7)
            .map(|_| {
                t = t.cycle();
                t.as_str()
            })
            .collect();
        assert_eq!(order, ["low", "medium", "high", "xhigh", "max", "off", "low"]);
        assert_eq!((Thinking::XHigh.budget(), Thinking::Max.budget()), (Some(32768), Some(64000)));
    }

    fn no_native(_: &str) -> bool {
        false
    }

    #[test]
    fn metadata_precedence() {
        let c = cfg();
        let (m, l) = resolve("@smol", &c, None, &no_native).unwrap();
        assert_eq!(
            (l, m.id.as_str(), m.context_window, m.max_output),
            (Some(Thinking::High), "modal/a/b", 1000, 32000)
        );
        assert_eq!((m.api, m.native, m.wire_id.as_str()), (Api::AnthropicMessages, false, "a/b"));
        let gw = [GatewayModel {
            id: "anthropic/claude-sonnet-5-5".into(),
            context_length: Some(500),
            max_output_tokens: Some(9),
        }];
        let (m, l) = resolve("@default:low", &c, Some(&gw), &no_native).unwrap();
        assert_eq!((m.context_window, m.max_output, l), (500, 9, Some(Thinking::Low)));
        let (m, _) = resolve("openai-codex/g", &c, Some(&gw), &no_native).unwrap();
        assert_eq!(m.context_window, 272000);
        assert!(resolve("@nope", &c, None, &no_native).is_err());
    }

    #[test]
    fn native_models_use_the_catalog_below_models_yml() {
        let native = |p: &str| p == "anthropic" || p == "baseten";
        let c = cfg();
        // The catalog row beats the gateway's figures.
        let gw = [GatewayModel {
            id: "anthropic/claude-sonnet-5-5".into(),
            context_length: Some(500),
            max_output_tokens: Some(9),
        }];
        let (m, _) = resolve("anthropic/claude-sonnet-5-5", &c, Some(&gw), &native).unwrap();
        assert!(m.native && m.context_window > 500 && m.max_output > 9, "{m:?}");
        assert_eq!((m.wire_id.as_str(), m.thinking.mode), ("claude-sonnet-5-5", ThinkingMode::AnthropicAdaptive));
        // An explicit models.yml entry beats the catalog.
        let models: ModelsYml = serde_yaml_ng::from_str(
            "providers:\n  anthropic:\n    modelOverrides:\n      claude-sonnet-5-5:\n        contextWindow: 123\n",
        )
        .unwrap();
        let c2 = Configs { models, ..Default::default() };
        let (m, _) = resolve("anthropic/claude-sonnet-5-5", &c2, None, &native).unwrap();
        assert_eq!(m.context_window, 123);
        // A provider without a key stays on the gateway; an unknown native id gets a safe spec.
        let (m, _) = resolve("openai/gpt-5.5", &c, None, &native).unwrap();
        assert!(!m.native && m.api == Api::AnthropicMessages);
        let (m, _) = resolve("anthropic/claude-future-9", &c, None, &native).unwrap();
        assert_eq!((m.native, m.thinking.mode), (true, ThinkingMode::Budget));
        let (m, _) = resolve("baseten/acme/New-1", &c, None, &native).unwrap();
        assert_eq!(
            (m.api, m.wire_id.as_str(), m.thinking.mode),
            (Api::OpenAiCompletions, "acme/New-1", ThinkingMode::None)
        );
    }

    #[test]
    fn efforts_clamp_to_the_ladder() {
        let l = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let ladder = l(&["low", "high", "max"]);
        assert_eq!(clamp_effort(Thinking::Off, &ladder), None);
        assert_eq!(clamp_effort(Thinking::Low, &ladder), Some("low"));
        assert_eq!(clamp_effort(Thinking::Medium, &ladder), Some("low")); // a tie goes down
        assert_eq!(clamp_effort(Thinking::XHigh, &ladder), Some("high"));
        assert_eq!(clamp_effort(Thinking::Max, &ladder), Some("max"));
        assert_eq!(clamp_effort(Thinking::Low, &l(&["minimal", "low"])), Some("low"));
        assert_eq!(clamp_effort(Thinking::Low, &[]), None);
    }

    #[test]
    fn chains() {
        assert_eq!(role_chain("memory"), ["memory", "smol", "default"]);
        assert_eq!(role_selector("task", &cfg()).as_deref(), Some("@default"));
    }
}
