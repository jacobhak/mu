//! Providers mu talks to directly with an API key. Everything else goes through the gateway.

use super::{catalog, discovery};
use mu_base::models::{self, Api, CompletionsCompat, ModelSpec, ProviderDef, ThinkingMode};

/// The outcome of asking a provider whether a key works.
#[derive(Debug, PartialEq)]
pub enum Check {
    Valid,
    /// 401/403: the provider refused the key.
    Rejected(String),
    /// Anything else (no network, a 5xx): the key is neither confirmed nor refuted.
    Unreachable(String),
}

const VALIDATE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// One cheap authenticated GET against the provider (`def.validate`), 15 s at most.
pub async fn validate(def: &ProviderDef, key: &str, base_url: Option<&str>) -> Check {
    super::install_crypto();
    let base = match base_url.map(crate::auth_store::check_base_url).transpose() {
        Ok(b) => b.unwrap_or_else(|| def.base_url.to_string()),
        Err(e) => return Check::Rejected(e.to_string()),
    };
    let Ok(client) =
        reqwest::Client::builder().timeout(VALIDATE_TIMEOUT).redirect(reqwest::redirect::Policy::none()).build()
    else {
        return Check::Unreachable("cannot build an HTTP client".into());
    };
    let rb = client.get(format!("{base}{}", def.validate)).header("anthropic-version", "2023-06-01");
    let rb = match super::Auth::for_def(def, key).apply(rb) {
        Ok(rb) => rb,
        Err(e) => return Check::Rejected(e.to_string()),
    };
    match rb.send().await {
        Ok(r) if r.status().is_success() => Check::Valid,
        Ok(r) if matches!(r.status().as_u16(), 401 | 403) => {
            Check::Rejected(format!("{} rejected the key (HTTP {})", def.label, r.status().as_u16()))
        }
        Ok(r) => Check::Unreachable(format!("{} answered HTTP {}", def.label, r.status().as_u16())),
        Err(e) => {
            Check::Unreachable(mu_base::secrets::redact_keys(&format!("cannot reach {}: {e}", super::host_of(&base))))
        }
    }
}

/// What the user is told after saving a key: the routing and billing change.
pub fn saved_notice(def: &ProviderDef) -> String {
    format!("{}/* models now use your API key (billed per token) instead of omp.", def.id)
}

/// A natively routed model: its catalog row, else a conservative spec for the provider's wire
/// (Anthropic gets a plain thinking budget; reasoning is never guessed for other unknown ids).
pub fn native_spec(provider: &str, model: &str) -> Option<ModelSpec> {
    let def = models::native_def(provider)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use mu_base::config::{Configs, ModelsYml, MuConfig, OmpConfig};
    use mu_base::models::{GatewayModel, resolve};

    #[test]
    fn an_unsafe_base_url_is_refused_before_any_request() {
        let p = models::native_def("openai").unwrap();
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        for base in ["http://example.com", "https://user:pw@example.com"] {
            let c = rt.block_on(validate(p, "k-0123456789", Some(base)));
            assert!(matches!(c, Check::Rejected(_)), "{c:?}");
        }
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
        let native = |p: &str, m: &str| (p == "anthropic" || p == "baseten").then(|| native_spec(p, m)).flatten();
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
