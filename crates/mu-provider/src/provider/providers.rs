//! Providers mu talks to directly with an API key, and their table. Everything else goes through the gateway.

use super::client::Providers;
use crate::model::{catalog, discovery};
use mu_base::models::{self, Api, CompletionsCompat, ModelSpec, ThinkingMode};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthStyle {
    /// `x-api-key: <key>`
    XApiKey,
    /// `Authorization: Bearer <key>`
    Bearer,
}

#[derive(Debug)]
pub struct ProviderDef {
    pub id: &'static str,
    pub label: &'static str,
    pub api: Api,
    /// Without a trailing slash; each wire appends its own path.
    pub base_url: &'static str,
    pub auth: AuthStyle,
    /// Path (after `base_url`) of a cheap authenticated GET that checks a key.
    pub validate: &'static str,
    /// Where the user creates a key.
    pub key_url: &'static str,
    /// The conventional environment variable; also the redaction label of a stored key.
    pub env_hint: &'static str,
    /// Model id (without the provider prefix) used when nothing else is configured.
    pub default_model: &'static str,
}

/// Providers mu talks to directly with an API key. Everything else goes through the gateway.
pub const PROVIDERS: [ProviderDef; 3] = [
    ProviderDef {
        id: "anthropic",
        label: "Anthropic",
        api: Api::AnthropicMessages,
        base_url: "https://api.anthropic.com",
        auth: AuthStyle::XApiKey,
        validate: "/v1/models?limit=1",
        key_url: "https://console.anthropic.com/settings/keys",
        env_hint: "ANTHROPIC_API_KEY",
        default_model: "claude-sonnet-5-5",
    },
    ProviderDef {
        id: "openai",
        label: "OpenAI",
        api: Api::OpenAiResponses,
        base_url: "https://api.openai.com/v1",
        auth: AuthStyle::Bearer,
        validate: "/models",
        key_url: "https://platform.openai.com/api-keys",
        env_hint: "OPENAI_API_KEY",
        default_model: "gpt-5.5",
    },
    ProviderDef {
        id: "baseten",
        label: "Baseten",
        api: Api::OpenAiCompletions,
        base_url: "https://inference.baseten.co/v1",
        auth: AuthStyle::Bearer,
        validate: "/models",
        key_url: "https://app.baseten.co/settings/api_keys",
        env_hint: "BASETEN_API_KEY",
        default_model: "moonshotai/Kimi-K3",
    },
];

pub fn native_def(provider: &str) -> Option<&'static ProviderDef> {
    PROVIDERS.iter().find(|d| d.id == provider)
}

/// What the user is told after saving a key: the routing and billing change.
pub fn saved_notice(def: &ProviderDef) -> String {
    format!("{}/* models now use your API key (billed per token) instead of omp.", def.id)
}

/// A natively routed model: its catalog row, else a conservative spec for the provider's wire
/// (Anthropic gets a plain thinking budget; reasoning is never guessed for other unknown ids).
pub fn direct_spec(provider: &str, model: &str) -> Option<ModelSpec> {
    let def = native_def(provider)?;
    // A discovered listing is fresher than the snapshot's numbers; the snapshot row still decides
    // thinking and request quirks.
    let found = discovery::find(provider, model);
    if let Some(mut spec) =
        catalog::row(provider, model).and_then(|r| r.spec(provider, models::DEFAULT_WINDOW, models::DEFAULT_MAX_OUT))
    {
        if let Some(d) = &found {
            spec.context_window = d.context_length.unwrap_or(spec.context_window);
            spec.max_output = d.max_completion_tokens.unwrap_or(spec.max_output);
        }
        return Some(spec);
    }
    if let Some(d) = &found
        && let Some(spec) = discovery::spec(provider, d, models::DEFAULT_WINDOW, models::DEFAULT_MAX_OUT)
    {
        return Some(spec);
    }
    let mut spec = ModelSpec::gateway(&format!("{provider}/{model}"), models::DEFAULT_WINDOW, models::DEFAULT_MAX_OUT);
    spec.api = def.api;
    spec.native = true;
    spec.images = def.api != Api::OpenAiCompletions;
    spec.compat = (def.api == Api::OpenAiCompletions).then(CompletionsCompat::default);
    spec.thinking.mode = if def.api == Api::AnthropicMessages { ThinkingMode::Budget } else { ThinkingMode::None };
    Some(spec)
}

/// A model a direct provider offers, for pickers.
#[derive(Debug, Clone, PartialEq)]
pub struct DirectModel {
    /// `provider/model`, as a selector.
    pub id: String,
    pub provider: String,
    pub name: String,
}

impl Providers {
    /// Whether `provider`'s models go to its own API with the user's key: the provider is one
    /// mu speaks natively and the key store has an entry for it.
    pub fn is_direct(&self, provider: &str) -> bool {
        native_def(provider).is_some() && self.store.has(provider)
    }

    /// Models of the direct providers that have a key: the snapshot's rows (and, for Baseten,
    /// the last discovered ones). Never touches the network. Sorted by provider, then id.
    pub fn direct_models(&self) -> Vec<DirectModel> {
        let mut out = Vec::new();
        for def in PROVIDERS.iter().filter(|d| self.is_direct(d.id)) {
            for r in catalog::rows(def.id).iter().filter(|r| Api::parse(&r.api).is_some()) {
                out.push(DirectModel {
                    id: format!("{}/{}", def.id, r.id),
                    provider: def.id.to_string(),
                    name: r.name.clone().unwrap_or_else(|| r.id.clone()),
                });
            }
            if discovery::discovers(def) {
                for d in
                    discovery::cached(def.id).into_iter().filter(|d| d.tools && catalog::row(def.id, &d.id).is_none())
                {
                    out.push(DirectModel {
                        id: format!("{}/{}", def.id, d.id),
                        provider: def.id.to_string(),
                        name: d.name.clone().unwrap_or_else(|| d.id.clone()),
                    });
                }
            }
        }
        out
    }

    /// Refreshes the discovered model lists of the providers that have a key and list their
    /// models, at most daily unless `force`. Failures keep the old cache. Returns how many
    /// models were fetched.
    pub async fn refresh_direct_models(&self, force: bool) -> usize {
        let mut n = 0;
        for def in PROVIDERS.iter().filter(|d| discovery::discovers(d) && self.is_direct(d.id)) {
            if !force && !discovery::stale(def.id) {
                continue;
            }
            let Ok(r) = self.store.resolve(def.id).await else { continue };
            if let Ok(c) = discovery::refresh(def, &r.key, r.base_url.as_deref()).await {
                n += c;
            }
        }
        n
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mu_base::config::{Configs, ModelsYml, MuConfig, OmpConfig};
    use mu_base::models::{GatewayModel, resolve};

    #[test]
    fn table_is_consistent() {
        for d in &PROVIDERS {
            assert!(d.base_url.starts_with("https://") && !d.base_url.ends_with('/'), "{}", d.id);
            assert!(d.validate.starts_with('/'), "{}", d.id);
            assert_eq!(native_def(d.id).map(|x| x.id), Some(d.id));
        }
        assert!(native_def("nope").is_none());
    }

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
    fn native_models_use_the_catalog_below_models_yml() {
        let native = |p: &str, m: &str| (p == "anthropic" || p == "baseten").then(|| direct_spec(p, m)).flatten();
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
}
