use crate::config::Configs;
use anyhow::{Result, anyhow, bail};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Thinking {
    Off,
    Low,
    Medium,
    High,
}

impl Thinking {
    pub const NAMES: [&'static str; 4] = ["off", "low", "medium", "high"];

    pub fn as_str(self) -> &'static str {
        Self::NAMES[self as usize]
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "off" => Self::Off,
            "low" | "minimal" => Self::Low,
            "medium" => Self::Medium,
            "high" | "xhigh" => Self::High,
            _ => return None,
        })
    }

    pub fn cycle(self) -> Self {
        match self {
            Self::Off => Self::Low,
            Self::Low => Self::Medium,
            Self::Medium => Self::High,
            Self::High => Self::Off,
        }
    }

    pub fn budget(self) -> Option<u32> {
        match self {
            Self::Off => None,
            Self::Low => Some(4096),
            Self::Medium => Some(8192),
            Self::High => Some(16384),
        }
    }
}

pub const API: &str = "anthropic-messages";

#[derive(Debug, Clone)]
pub struct ModelSpec {
    /// `provider/model`
    pub id: String,
    pub provider: String,
    pub context_window: u32,
    pub max_output: u32,
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

pub fn resolve(
    sel: &str,
    cfg: &Configs,
    gateway_models: Option<&[GatewayModel]>,
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
    Ok((
        ModelSpec {
            id: base.clone(),
            provider: provider.into(),
            context_window: entry
                .and_then(|m| m.context_window)
                .or(ov.and_then(|o| o.context_window))
                .or(gw.and_then(|g| g.context_length))
                .unwrap_or(DEFAULT_WINDOW),
            max_output: entry
                .and_then(|m| m.max_tokens)
                .or(ov.and_then(|o| o.max_tokens))
                .or(gw.and_then(|g| g.max_output_tokens))
                .unwrap_or(DEFAULT_MAX_OUT),
        },
        level,
    ))
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
        assert_eq!(split_level("a/b:xhigh"), ("a/b".into(), Some(Thinking::High)));
        assert_eq!(split_level("a/b:auto"), ("a/b".into(), None));
        assert_eq!(split_level("a/b:c"), ("a/b:c".into(), None));
    }

    #[test]
    fn metadata_precedence() {
        let c = cfg();
        let (m, l) = resolve("@smol", &c, None).unwrap();
        assert_eq!(
            (l, m.id.as_str(), m.context_window, m.max_output),
            (Some(Thinking::High), "modal/a/b", 1000, 32000)
        );
        let gw = [GatewayModel {
            id: "anthropic/claude-sonnet-5-5".into(),
            context_length: Some(500),
            max_output_tokens: Some(9),
        }];
        let (m, l) = resolve("@default:low", &c, Some(&gw)).unwrap();
        assert_eq!((m.context_window, m.max_output, l), (500, 9, Some(Thinking::Low)));
        let (m, _) = resolve("openai-codex/g", &c, Some(&gw)).unwrap();
        assert_eq!(m.context_window, 272000);
        assert!(resolve("@nope", &c, None).is_err());
    }

    #[test]
    fn chains() {
        assert_eq!(role_chain("memory"), ["memory", "smol", "default"]);
        assert_eq!(role_selector("task", &cfg()).as_deref(), Some("@default"));
    }
}
