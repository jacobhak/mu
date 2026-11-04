//! Providers mu talks to directly with an API key. Everything else goes through the gateway.

use super::{catalog, discovery};
use crate::models::{self, Api, CompletionsCompat, ModelSpec, ProviderDef, ThinkingMode};

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
            Check::Unreachable(crate::secrets::redact_keys(&format!("cannot reach {}: {e}", super::host_of(&base))))
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

    #[test]
    fn an_unsafe_base_url_is_refused_before_any_request() {
        let p = models::native_def("openai").unwrap();
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        for base in ["http://example.com", "https://user:pw@example.com"] {
            let c = rt.block_on(validate(p, "k-0123456789", Some(base)));
            assert!(matches!(c, Check::Rejected(_)), "{c:?}");
        }
    }
}
