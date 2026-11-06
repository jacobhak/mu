//! Key validation: whether a provider accepts a key. `mu auth test` and the TUI key flow share it.

use super::store::{Entry, check_base_url, resolve_entry};
use crate::net;
use crate::provider::providers::{Credential, KeyTarget, ProviderDef};
use crate::provider::types::Auth;
use std::time::Duration;

/// The outcome of asking a provider whether a key works.
#[derive(Debug, PartialEq)]
pub enum Check {
    Valid,
    /// 401/403: the provider refused the key.
    Rejected(String),
    /// Anything else (no network, a 5xx): the key is neither confirmed nor refuted.
    Unreachable(String),
    /// The service has no free way to validate a key; the entry resolved and nothing was sent.
    Unchecked,
}

const VALIDATE_TIMEOUT: Duration = Duration::from_secs(15);

/// One cheap authenticated GET against the provider (`def.validate`), 15 s at most.
pub async fn validate(def: &ProviderDef, key: &str, base_url: Option<&str>) -> Check {
    let base = match base_url.map(check_base_url).transpose() {
        Ok(b) => b.unwrap_or_else(|| def.base_url.to_string()),
        Err(e) => return Check::Rejected(e.to_string()),
    };
    // Not `net::http_client`: validation has only its overall timeout, no separate connect timeout.
    net::install_crypto();
    let Ok(client) =
        reqwest::Client::builder().timeout(VALIDATE_TIMEOUT).redirect(reqwest::redirect::Policy::none()).build()
    else {
        return Check::Unreachable("cannot build an HTTP client".into());
    };
    let rb = client.get(format!("{base}{}", def.validate)).header("anthropic-version", "2023-06-01");
    let rb = match Auth::for_def(def, key).apply(rb) {
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
            Check::Unreachable(mu_base::secrets::redact_keys(&format!("cannot reach {}: {e}", net::host_of(&base))))
        }
    }
}

/// Resolves `entry` (running its `!command` if it has one), then validates the key it yields.
/// `Err` when the entry cannot be resolved; nothing was sent.
pub async fn check_entry(def: &ProviderDef, entry: &Entry) -> Result<Check, String> {
    if def.kind != Credential::ApiKey {
        return Err(format!("{} takes no key; sign in with /login instead", def.label));
    }
    let resolved = resolve_entry(def.id, entry).await.map_err(|e| e.to_string())?;
    Ok(validate(def, &resolved.key, entry.base_url.as_deref()).await)
}

/// `check_entry` for a provider; for a search service the entry is only resolved, since a test
/// search would cost money.
pub async fn check_target(t: KeyTarget, entry: &Entry) -> Result<Check, String> {
    match t {
        KeyTarget::Provider(def) => check_entry(def, entry).await,
        KeyTarget::Search(_) => {
            resolve_entry(t.id(), entry).await.map_err(|e| e.to_string())?;
            Ok(Check::Unchecked)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unsafe_base_url_is_refused_before_any_request() {
        let p = crate::provider::providers::native_def("openai").unwrap();
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        for base in ["http://example.com", "https://user:pw@example.com"] {
            let c = rt.block_on(validate(p, "k-0123456789", Some(base)));
            assert!(matches!(c, Check::Rejected(_)), "{c:?}");
        }
    }
}
