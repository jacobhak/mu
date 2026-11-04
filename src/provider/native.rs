//! Providers mu talks to directly with an API key. Everything else goes through the gateway.

use crate::models::Api;

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

pub fn def(provider: &str) -> Option<&'static ProviderDef> {
    PROVIDERS.iter().find(|d| d.id == provider)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_is_consistent() {
        for d in &PROVIDERS {
            assert!(d.base_url.starts_with("https://") && !d.base_url.ends_with('/'), "{}", d.id);
            assert!(d.validate.starts_with('/'), "{}", d.id);
            assert_eq!(def(d.id).map(|x| x.id), Some(d.id));
        }
        assert!(def("nope").is_none());
    }

    #[test]
    fn an_unsafe_base_url_is_refused_before_any_request() {
        let p = def("openai").unwrap();
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        for base in ["http://example.com", "https://user:pw@example.com"] {
            let c = rt.block_on(validate(p, "k-0123456789", Some(base)));
            assert!(matches!(c, Check::Rejected(_)), "{c:?}");
        }
    }
}
