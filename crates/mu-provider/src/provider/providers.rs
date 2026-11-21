//! Providers mu talks to directly with an API key, and their table. Everything else goes through the gateway.

use super::client::Providers;
use super::custom::{AuthMode, CustomProvider, KeyState, Registry};
use crate::auth::chatgpt;
use crate::model::{catalog, chatgpt_models, discovery};
use mu_base::models::{self, Api, CompletionsCompat, ModelSpec, ThinkingMode};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthStyle {
    /// `x-api-key: <key>`
    XApiKey,
    /// `Authorization: Bearer <key>`
    Bearer,
}

/// How a provider's requests are authenticated: a key the user stores, or the ChatGPT sign-in (no
/// key of its own; the tokens live in the store's `chatgpt` section).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Credential {
    ApiKey,
    ChatgptPlan,
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
    pub kind: Credential,
}

/// Providers mu talks to directly with an API key. Everything else goes through the gateway.
pub const PROVIDERS: [ProviderDef; 4] = [
    ProviderDef {
        id: "anthropic",
        label: "Anthropic",
        api: Api::AnthropicMessages,
        base_url: "https://api.anthropic.com",
        auth: AuthStyle::XApiKey,
        validate: "/v1/models?limit=1",
        key_url: "https://console.anthropic.com/settings/keys",
        env_hint: "ANTHROPIC_API_KEY",
        kind: Credential::ApiKey,
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
        kind: Credential::ApiKey,
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
        kind: Credential::ApiKey,
    },
    ProviderDef {
        id: "openai-chatgpt",
        label: chatgpt::LABEL,
        api: Api::OpenAiResponses,
        base_url: chatgpt::API_BASE,
        auth: AuthStyle::Bearer,
        validate: "/models",
        key_url: chatgpt::USAGE_URL,
        env_hint: chatgpt::REDACT_LABEL,
        kind: Credential::ChatgptPlan,
    },
];

/// The providers that take an API key: the ones `/login` and `mu auth add` offer a key flow for.
pub fn api_key_defs() -> impl Iterator<Item = &'static ProviderDef> {
    PROVIDERS.iter().filter(|d| d.kind == Credential::ApiKey)
}

pub fn native_def(provider: &str) -> Option<&'static ProviderDef> {
    PROVIDERS.iter().find(|d| d.id == provider)
}

/// A key that only serves web search.
#[derive(Debug)]
pub struct SearchKeyDef {
    pub id: &'static str,
    pub label: &'static str,
    pub key_url: &'static str,
    pub env_hint: &'static str,
}

pub const SEARCH_KEYS: [SearchKeyDef; 2] = [
    SearchKeyDef {
        id: "parallel",
        label: "Parallel",
        key_url: "https://platform.parallel.ai",
        env_hint: "PARALLEL_API_KEY",
    },
    SearchKeyDef { id: "exa", label: "Exa", key_url: "https://dashboard.exa.ai/api-keys", env_hint: "EXA_API_KEY" },
];

/// Where a direct request goes: a provider mu knows, or a custom endpoint from `providers.toml`.
#[derive(Debug, Clone)]
pub enum DirectProvider {
    Builtin(&'static ProviderDef),
    Custom(CustomProvider),
}

impl DirectProvider {
    pub fn id(&self) -> &str {
        match self {
            Self::Builtin(d) => d.id,
            Self::Custom(c) => &c.id,
        }
    }

    pub fn label(&self) -> &str {
        match self {
            Self::Builtin(d) => d.label,
            Self::Custom(c) => c.label(),
        }
    }
}

/// Something a key can be stored for: a chat provider, a search service or a custom endpoint.
#[derive(Debug, Clone)]
pub enum KeyTarget {
    Provider(&'static ProviderDef),
    Search(&'static SearchKeyDef),
    Custom(CustomProvider),
}

impl KeyTarget {
    /// Built-in and search targets resolve at once; any other id is looked up in `registry`,
    /// whose errors (an unreadable or invalid `providers.toml`) are passed on.
    pub fn find(id: &str, registry: &Registry) -> anyhow::Result<Option<Self>> {
        if native_def(id).is_some() || SEARCH_KEYS.iter().any(|d| d.id == id) {
            return Ok(Self::builtin(id));
        }
        Ok(registry.snapshot()?.remove(id).map(Self::Custom))
    }

    /// A built-in chat provider or search service by id; never consults any configuration.
    pub fn builtin(id: &str) -> Option<Self> {
        native_def(id)
            .filter(|d| d.kind == Credential::ApiKey)
            .map(Self::Provider)
            .or_else(|| SEARCH_KEYS.iter().find(|d| d.id == id).map(Self::Search))
    }

    pub fn id(&self) -> &str {
        match self {
            Self::Provider(d) => d.id,
            Self::Search(d) => d.id,
            Self::Custom(c) => &c.id,
        }
    }

    pub fn label(&self) -> &str {
        match self {
            Self::Provider(d) => d.label,
            Self::Search(d) => d.label,
            Self::Custom(c) => c.label(),
        }
    }

    /// Where the user creates a key; empty for a custom endpoint, which has no such page.
    pub fn key_url(&self) -> &str {
        match self {
            Self::Provider(d) => d.key_url,
            Self::Search(d) => d.key_url,
            Self::Custom(_) => "",
        }
    }

    pub fn env_hint(&self) -> &str {
        match self {
            Self::Provider(d) => d.env_hint,
            Self::Search(d) => d.env_hint,
            Self::Custom(c) => &c.env_hint,
        }
    }

    /// A custom endpoint that takes no key at all.
    pub fn is_keyless(&self) -> bool {
        matches!(self, Self::Custom(c) if c.auth == AuthMode::None)
    }

    /// anthropic and openai: the key can serve chat models or only web search, so the user must say which.
    pub fn asks_chat(&self) -> bool {
        matches!(self, Self::Provider(d) if d.id == "anthropic" || d.id == "openai")
    }
}

/// What the user is told after saving a provider's key: the routing and billing consequence.
pub fn saved_notice(def: &ProviderDef, chat: bool) -> String {
    if chat {
        format!("{}/* models now use your API key (billed per token) instead of the gateway.", def.id)
    } else {
        format!(
            "saved the {} key; {}/* chat models still go through the gateway. The key is used only where you choose it, such as /web.",
            def.label, def.id
        )
    }
}

/// A natively routed model: its catalog row, else a conservative spec for the provider's wire
/// (Anthropic gets a plain thinking budget; reasoning is never guessed for other unknown ids).
pub fn direct_spec(provider: &str, model: &str) -> Option<ModelSpec> {
    let def = native_def(provider)?;
    // A discovered listing is fresher than the snapshot's numbers; the snapshot row still decides
    // thinking and request quirks.
    let found = discovery::find(provider, model);
    // The plan speaks the Responses wire only: a row of another api is not that model there.
    if let Some(mut spec) = catalog::row(provider, model)
        .and_then(|r| r.spec(provider, models::DEFAULT_WINDOW, models::DEFAULT_MAX_OUT))
        .filter(|s| def.kind == Credential::ApiKey || s.api == def.api)
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
    /// A custom endpoint's credential state, for its rows; `None` for the built-in providers.
    pub key: Option<KeyState>,
}

impl Providers {
    /// Whether `provider`'s models go to its own API with the user's key: the provider is one
    /// mu speaks natively and the key store has an entry for it.
    pub fn is_direct(&self, provider: &str) -> bool {
        if provider == chatgpt::PROVIDER_ID {
            return self.store.chatgpt_account().is_some_and(|a| a.plan_usage());
        }
        if let Some(d) = native_def(provider) {
            return d.kind == Credential::ApiKey && self.store.get(provider).is_some_and(|e| e.routes_chat());
        }
        // A configured custom namespace is native whatever its key state: a request without a
        // usable key fails, it never falls through to the gateway.
        self.registry.get(provider).is_ok_and(|c| c.is_some())
    }

    /// Models of the direct providers that have a key: the snapshot's rows (and, for Baseten,
    /// the last discovered ones), then every configured custom endpoint's manual and cached rows,
    /// whatever its key state. Never touches the network. Built-ins sort by provider, then id.
    /// An invalid `providers.toml` is an error.
    pub fn direct_models(&self) -> anyhow::Result<Vec<DirectModel>> {
        let custom = self.registry.snapshot()?;
        let mut out = Vec::new();
        for def in PROVIDERS.iter().filter(|d| self.is_direct(d.id)) {
            out.extend(self.provider_models(def));
        }
        for c in custom.values() {
            out.extend(self.custom_models(c));
        }
        Ok(out)
    }

    /// A custom endpoint's picker models: the configured ones and, when it discovers, its own
    /// cached listing, sorted by id.
    fn custom_models(&self, c: &CustomProvider) -> Vec<DirectModel> {
        let key = Some(c.key_state(&self.store));
        let mut rows: std::collections::BTreeMap<String, String> =
            c.models.iter().map(|(id, m)| (id.clone(), m.name.clone().unwrap_or_else(|| id.clone()))).collect();
        if c.discover {
            for r in discovery::custom_cached(c) {
                rows.entry(r.id.clone()).or_insert(r.id);
            }
        }
        rows.into_iter()
            .map(|(id, name)| DirectModel { id: format!("{}/{id}", c.id), provider: c.id.clone(), name, key })
            .collect()
    }

    /// One direct provider's picker models: the ChatGPT account's own list once cached, else the catalogue.
    fn provider_models(&self, def: &ProviderDef) -> Vec<DirectModel> {
        if def.kind == Credential::ChatgptPlan {
            let listed = self.store.chatgpt_account().map(|a| chatgpt_models::cached(&a.subject)).unwrap_or_default();
            if !listed.is_empty() {
                return listed
                    .into_iter()
                    .map(|m| DirectModel {
                        id: format!("{}/{}", def.id, m.slug),
                        provider: def.id.to_string(),
                        name: m.name,
                        key: None,
                    })
                    .collect();
            }
        }
        let mut out: Vec<DirectModel> = catalog::rows(def.id)
            .iter()
            .filter(|r| Api::parse(&r.api).is_some_and(|a| def.kind == Credential::ApiKey || a == def.api))
            .map(|r| DirectModel {
                id: format!("{}/{}", def.id, r.id),
                provider: def.id.to_string(),
                name: r.name.clone().unwrap_or_else(|| r.id.clone()),
                key: None,
            })
            .collect();
        if discovery::discovers(def) {
            out.extend(
                discovery::cached(def.id).into_iter().filter(|d| d.tools && catalog::row(def.id, &d.id).is_none()).map(
                    |d| DirectModel {
                        id: format!("{}/{}", def.id, d.id),
                        provider: def.id.to_string(),
                        name: d.name.clone().unwrap_or_else(|| d.id.clone()),
                        key: None,
                    },
                ),
            );
        }
        out
    }

    /// Refreshes the discovered model lists of the providers that have a key and list their
    /// models, at most daily unless `force`. Failures keep the old cache. Returns how many
    /// models were fetched.
    pub async fn refresh_direct_models(&self, force: bool) -> usize {
        if self.is_relayed() {
            return 0;
        }
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
        if let Ok(custom) = self.registry.snapshot() {
            for c in custom.values().filter(|c| c.discover && (force || discovery::custom_stale(c))) {
                if let Ok(k) = c.refresh_models(&self.store).await {
                    n += k;
                }
            }
        }
        let due = self
            .store
            .chatgpt_account()
            .is_some_and(|a| a.plan_usage() && (force || chatgpt_models::stale(&a.subject)));
        if due && let Ok(c) = chatgpt_models::refresh(&self.store).await {
            n += c;
        }
        n
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mu_base::config::Configs;
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

    #[test]
    fn a_search_only_key_does_not_route_chat() {
        use crate::auth::store::{Entry, Store};
        use std::rc::Rc;
        let d = std::env::temp_dir().join(format!("mu-direct-{}", uuid::Uuid::new_v4()));
        let store = Store::at(d.join("auth.json"));
        let p =
            Providers::with_store(Rc::new(Configs::default()), crate::provider::client::no_gateway(), store.clone());
        assert!(!p.is_direct("openai"));
        store.set("openai", Entry::literal("sk-test-0123456789")).unwrap();
        assert!(p.is_direct("openai"), "a stored key routes chat unless told otherwise");
        assert!(store.set_chat("openai", false).unwrap());
        assert!(!p.is_direct("openai") && p.direct_models().unwrap().is_empty());
        assert!(!store.set_chat("baseten", true).unwrap());
        // Parallel and Exa keys never route anything.
        store.set("parallel", Entry::literal("par-test-0123456789")).unwrap();
        assert!(!p.is_direct("parallel"));
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn the_chatgpt_plan_takes_no_key_and_routes_only_with_plan_usage() {
        use crate::auth::chatgpt::{Account, DIRECT_SCOPE, Tokens};
        use crate::auth::store::Store;
        use std::rc::Rc;
        assert!(api_key_defs().all(|d| d.id != "openai-chatgpt"));
        let d = std::env::temp_dir().join(format!("mu-chatgpt-route-{}", uuid::Uuid::new_v4()));
        let store = Store::at(d.join("auth.json"));
        let p =
            Providers::with_store(Rc::new(Configs::default()), crate::provider::client::no_gateway(), store.clone());
        assert!(KeyTarget::find("openai-chatgpt", p.custom_registry()).unwrap().is_none());
        assert!(
            !p.is_direct("openai-chatgpt") && p.direct_models().unwrap().iter().all(|m| m.provider != "openai-chatgpt")
        );
        let tokens = |scopes: Vec<String>| Tokens {
            id_token: "id-0123456789".into(),
            access_token: "acc-0123456789".into(),
            refresh_token: "ref-0123456789".into(),
            refresh_at: i64::MAX / 2,
            scopes,
        };
        let mut account = Account {
            client_id: "oaiapp_test".into(),
            subject: "sub-test-1".into(),
            email: None,
            tokens: Some(tokens(vec!["openid".into()])),
            added: "2026-11-06T00:00:00Z".into(),
        };
        store.chatgpt_save(account.clone()).unwrap();
        assert!(!p.is_direct("openai-chatgpt"), "a sign-in without plan usage does not route");
        account.tokens = Some(tokens(vec![DIRECT_SCOPE.into()]));
        store.chatgpt_save(account).unwrap();
        assert!(p.is_direct("openai-chatgpt"));
        let (m, _) = p.resolve("openai-chatgpt/gpt-5.5").unwrap();
        assert_eq!(
            (m.native, m.provider.as_str(), m.wire_id.as_str(), m.api),
            (true, "openai-chatgpt", "gpt-5.5", Api::OpenAiResponses)
        );
        assert!(p.direct_models().unwrap().iter().any(|m| m.id == "openai-chatgpt/gpt-5.5"));
        let (u, _) = p.resolve("openai-chatgpt/gpt-future-9").unwrap();
        assert_eq!((u.native, u.thinking.mode), (true, ThinkingMode::None));
        store.chatgpt_sign_out().unwrap();
        let e = p.resolve("openai-chatgpt/gpt-5.5").unwrap_err().to_string();
        assert!(e.contains("mu auth login openai-chatgpt"), "{e}");
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn key_targets_cover_providers_and_search() {
        let reg =
            Registry::at(std::env::temp_dir().join(format!("mu-kt-{}", uuid::Uuid::new_v4())).join("providers.toml"));
        let find = |i: &str| KeyTarget::find(i, &reg).unwrap();
        let ids: Vec<_> =
            ["anthropic", "openai", "baseten", "parallel", "exa"].iter().map(|i| find(i).unwrap()).collect();
        assert_eq!(ids.iter().filter(|t| t.asks_chat()).count(), 2);
        assert!(find("nope").is_none());
        assert_eq!(find("exa").unwrap().env_hint(), "EXA_API_KEY");
        assert!(saved_notice(native_def("openai").unwrap(), false).contains("still go through the gateway"));
    }

    fn cfg() -> Configs {
        Configs::from_models_toml(
            "[sets.main]\ndefault = \"anthropic/claude-sonnet-5-5:medium\"\nsmol = \"modal/a/b:high\"\n\
             [models.\"modal/a/b\"]\ncontext_window = 1000\n[models.\"openai-codex/g\"]\ncontext_window = 272000\n",
        )
    }

    #[test]
    fn native_models_use_the_catalog_below_models_toml() {
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
        // An explicit models.toml entry beats the catalog.
        let c2 = Configs::from_models_toml("[models.\"anthropic/claude-sonnet-5-5\"]\ncontext_window = 123\n");
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
