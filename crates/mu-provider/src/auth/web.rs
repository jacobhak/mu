//! The user's web search choice, stored next to the keys in `auth.json`. Exactly one backend is
//! used at a time and nothing is contacted until the user has chosen one.

use super::store::Store;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SearchBackend {
    Parallel,
    Exa,
    Anthropic,
    Openai,
    Off,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WebSearch {
    pub backend: SearchBackend,
    /// Parallel or Exa through their free public endpoint instead of a key.
    #[serde(default)]
    pub keyless: bool,
    /// When the user approved a keyless third party (RFC 3339). Required for `keyless`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approved: Option<String>,
    /// Model for Anthropic/OpenAI search; `default_model` when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

impl WebSearch {
    pub fn keyed(backend: SearchBackend) -> Self {
        Self { backend, keyless: false, approved: None, model: None }
    }

    pub fn keyless(backend: SearchBackend, approved: String) -> Self {
        Self { backend, keyless: true, approved: Some(approved), model: None }
    }

    pub fn off() -> Self {
        Self::keyed(SearchBackend::Off)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum WebStatus {
    Ready(WebSearch),
    Off,
    NotSetUp,
}

pub fn status(store: &Store) -> WebStatus {
    let Some(w) = store.web() else { return WebStatus::NotSetUp };
    match w.backend {
        SearchBackend::Off => WebStatus::Off,
        b if w.keyless => {
            if w.approved.is_some() && matches!(b, SearchBackend::Parallel | SearchBackend::Exa) {
                WebStatus::Ready(w)
            } else {
                WebStatus::NotSetUp
            }
        }
        b => match key_id(b) {
            Some(id) if store.has(id) => WebStatus::Ready(w),
            _ => WebStatus::NotSetUp,
        },
    }
}

/// The key-store id of the key a backend uses.
pub fn key_id(b: SearchBackend) -> Option<&'static str> {
    match b {
        SearchBackend::Parallel => Some("parallel"),
        SearchBackend::Exa => Some("exa"),
        SearchBackend::Anthropic => Some("anthropic"),
        SearchBackend::Openai => Some("openai"),
        SearchBackend::Off => None,
    }
}

pub fn default_model(b: SearchBackend) -> Option<&'static str> {
    match b {
        SearchBackend::Anthropic => Some("claude-haiku-4-5"),
        SearchBackend::Openai => Some("gpt-5.5"),
        _ => None,
    }
}

/// What the web plugin needs to search, as `core/webSearch` answers it: the backend, whether it
/// is keyless, the resolved key and base URL, and the model. `{"backend": null}` when web search
/// is off or not set up. `Err` is a message already free of key text.
pub async fn plugin_config(store: &Store) -> Result<serde_json::Value, String> {
    use serde_json::json;
    let WebStatus::Ready(w) = status(store) else { return Ok(json!({ "backend": null })) };
    let (mut key, mut base_url) = (None, None);
    if !w.keyless {
        let id = key_id(w.backend).ok_or("web search has no key to resolve")?;
        let r = store.resolve(id).await.map_err(|e| e.to_string())?;
        (key, base_url) = (Some(r.key), r.base_url);
    }
    let backend = serde_json::to_value(w.backend).map_err(|e| e.to_string())?;
    let model = w.model.clone().or_else(|| default_model(w.backend).map(String::from));
    Ok(json!({ "backend": backend, "keyless": w.keyless, "key": key, "baseUrl": base_url, "model": model }))
}

pub fn parse_backend(s: &str) -> Option<SearchBackend> {
    match s {
        "parallel" => Some(SearchBackend::Parallel),
        "exa" => Some(SearchBackend::Exa),
        "anthropic" => Some(SearchBackend::Anthropic),
        "openai" => Some(SearchBackend::Openai),
        "off" => Some(SearchBackend::Off),
        _ => None,
    }
}

/// What the user is told, with the privacy and billing terms, before choosing a backend.
pub fn about(b: SearchBackend, keyless: bool) -> &'static str {
    match (b, keyless) {
        (SearchBackend::Parallel, true) => {
            "Parallel Web Systems, Inc. (Palo Alto, US) runs this free endpoint (search.parallel.ai): no account, lower rate limit. Each search sends the query the model writes, not your files or conversation. Parallel's customer terms let it use queries to train its models, and it publishes no retention terms for the free endpoint."
        }
        (SearchBackend::Parallel, false) => {
            "Searches go to Parallel's API on your key, billed per request by Parallel. Parallel's customer terms let it use queries to train its models unless you have a zero-data-retention plan."
        }
        (SearchBackend::Exa, true) => {
            "Exa Labs, Inc. (San Francisco, US) runs this free endpoint (mcp.exa.ai) at an unpublished rate limit. Each search sends the query the model writes, not your files or conversation. Exa's privacy policy says query data is used to train its models; zero data retention is only on its Enterprise plan."
        }
        (SearchBackend::Exa, false) => {
            "Searches go to Exa's API on your key: $10 of free credits a month, then $7 per 1,000 searches plus $1 per 1,000 page summaries. Exa's privacy policy says query data is used to train its models."
        }
        (SearchBackend::Anthropic, _) => {
            "Searches run Anthropic's web search tool with claude-haiku-4-5 on your Anthropic API key, billed per search and per token by Anthropic. The key is not used for chat models unless you turn that on in /login."
        }
        (SearchBackend::Openai, _) => {
            "Searches run OpenAI's web search tool with gpt-5.5 on your OpenAI API key, billed per search call and per token by OpenAI. The key is not used for chat models unless you turn that on in /login."
        }
        (SearchBackend::Off, _) => "web_search returns an error and nothing is contacted.",
    }
}

pub fn label(b: SearchBackend, keyless: bool) -> &'static str {
    match (b, keyless) {
        (SearchBackend::Parallel, true) => "Parallel (no key)",
        (SearchBackend::Parallel, false) => "Parallel (API key)",
        (SearchBackend::Exa, true) => "Exa (no key)",
        (SearchBackend::Exa, false) => "Exa (API key)",
        (SearchBackend::Anthropic, _) => "Anthropic web search (API key)",
        (SearchBackend::Openai, _) => "OpenAI web search (API key)",
        (SearchBackend::Off, _) => "Off",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::store::Entry;
    use std::path::PathBuf;

    fn store(tag: &str) -> (Store, PathBuf) {
        let d = std::env::temp_dir().join(format!("mu-web-{tag}-{}", uuid::Uuid::new_v4()));
        (Store::at(d.join("auth.json")), d)
    }

    #[test]
    fn status_covers_every_case() {
        let (s, d) = store("status");
        assert_eq!(status(&s), WebStatus::NotSetUp);
        s.set_web(Some(WebSearch::off())).unwrap();
        assert_eq!(status(&s), WebStatus::Off);
        // A hand-edited keyless entry without approval is not set up.
        let unapproved = WebSearch { approved: None, ..WebSearch::keyless(SearchBackend::Exa, String::new()) };
        s.set_web(Some(unapproved)).unwrap();
        assert_eq!(status(&s), WebStatus::NotSetUp);
        // Keyless is only for Parallel and Exa.
        s.set_web(Some(WebSearch::keyless(SearchBackend::Openai, "2026-01-01T00:00:00Z".into()))).unwrap();
        assert_eq!(status(&s), WebStatus::NotSetUp);
        let ok = WebSearch::keyless(SearchBackend::Parallel, "2026-01-01T00:00:00Z".into());
        s.set_web(Some(ok.clone())).unwrap();
        assert_eq!(status(&s), WebStatus::Ready(ok));
        let keyed = WebSearch::keyed(SearchBackend::Openai);
        s.set_web(Some(keyed.clone())).unwrap();
        assert_eq!(status(&s), WebStatus::NotSetUp);
        s.set("openai", Entry::literal("sk-test-0123456789")).unwrap();
        assert_eq!(status(&s), WebStatus::Ready(keyed));
        s.remove("openai").unwrap();
        assert_eq!(status(&s), WebStatus::NotSetUp);
        s.set_web(None).unwrap();
        assert_eq!(s.web(), None);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn settings_round_trip_and_survive_key_writes() {
        let (s, d) = store("roundtrip");
        let w = WebSearch::keyless(SearchBackend::Exa, "2026-01-01T00:00:00Z".into());
        s.set_web(Some(w.clone())).unwrap();
        s.set("openai", Entry::literal("sk-test-0123456789")).unwrap();
        assert_eq!(s.web(), Some(w));
        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(s.path()).unwrap()).unwrap();
        assert_eq!(v["web"]["backend"], "exa");
        assert_eq!(v["web"]["keyless"], true);
        assert_eq!(v["web"]["approved"], "2026-01-01T00:00:00Z");
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn plugin_config_carries_the_resolved_key_and_model() {
        use mu_testutil::block_on as rt;
        let (s, d) = store("config");
        assert_eq!(rt(plugin_config(&s)).unwrap()["backend"], serde_json::Value::Null);
        s.set_web(Some(WebSearch::keyless(SearchBackend::Exa, "2026-01-01T00:00:00Z".into()))).unwrap();
        let c = rt(plugin_config(&s)).unwrap();
        assert_eq!(
            (c["backend"].as_str(), c["keyless"].as_bool(), c["key"].is_null()),
            (Some("exa"), Some(true), true)
        );
        s.set_web(Some(WebSearch::keyed(SearchBackend::Openai))).unwrap();
        s.set("openai", Entry::literal("sk-test-0123456789")).unwrap();
        let c = rt(plugin_config(&s)).unwrap();
        assert_eq!(c["key"], "sk-test-0123456789");
        assert_eq!(c["model"], "gpt-5.5");
        s.set("openai", Entry::reference("$MU_TEST_SURELY_UNSET_VAR").unwrap()).unwrap();
        assert!(rt(plugin_config(&s)).unwrap_err().contains("not set"));
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn every_backend_has_text_and_a_key_id() {
        for b in [SearchBackend::Parallel, SearchBackend::Exa, SearchBackend::Anthropic, SearchBackend::Openai] {
            assert!(key_id(b).is_some() && !about(b, false).is_empty() && !label(b, false).is_empty());
            assert_eq!(parse_backend(key_id(b).unwrap()), Some(b));
        }
        assert_eq!(key_id(SearchBackend::Off), None);
        assert_eq!(parse_backend("off"), Some(SearchBackend::Off));
        assert_eq!(default_model(SearchBackend::Openai), Some("gpt-5.5"));
    }
}
