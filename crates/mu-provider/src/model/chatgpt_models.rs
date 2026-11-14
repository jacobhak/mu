//! The models the signed-in ChatGPT account can use (`GET {api_base}/models`, the `list` ones in
//! server order). Cached in `chatgpt-models.json`, keyed by a hash of the account's subject so one
//! account never sees another's list. Refreshed at most daily, and an hour after a failure.

use crate::auth::store::Store;
use crate::net;
use crate::provider::retry;
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::time::Duration;

const FETCH_TIMEOUT: Duration = Duration::from_secs(20);

/// A model the account can use: its slug (the id sent on the wire) and the name to show.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Listed {
    pub slug: String,
    pub name: String,
}

#[derive(Default, Serialize, Deserialize)]
struct File {
    /// Unix milliseconds of the last successful fetch, per account key.
    #[serde(default)]
    fetched_at: BTreeMap<String, i64>,
    /// Unix milliseconds of the last failed fetch, per account key.
    #[serde(default)]
    failed_at: BTreeMap<String, i64>,
    #[serde(default)]
    models: BTreeMap<String, Vec<Listed>>,
}

fn cache() -> mu_base::jsoncache::JsonCache {
    mu_base::jsoncache::JsonCache::new(mu_common::paths::profile_home().join("chatgpt-models.json"))
}

fn load() -> File {
    cache().load(|t| serde_json::from_str::<File>(t).ok()).unwrap_or_default()
}

/// The cache key of an account: a hash, so the file never names the account.
fn key(subject: &str) -> String {
    Sha256::digest(subject.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

/// The models cached for the account with `subject`; empty when never fetched.
pub fn cached(subject: &str) -> Vec<Listed> {
    load().models.get(&key(subject)).cloned().unwrap_or_default()
}

/// No fetch yet, or the last one is over a day old. A failed fetch is due again after an hour.
pub fn stale(subject: &str) -> bool {
    let f = load();
    let k = key(subject);
    mu_base::jsoncache::due(f.failed_at.get(&k).copied(), f.fetched_at.get(&k).copied())
}

/// Maps a `/models` body to its `visibility == "list"` models, in server order.
pub fn parse(body: &str) -> Vec<Listed> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else { return Vec::new() };
    v["models"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter(|m| m["visibility"] == "list")
                .filter_map(|m| {
                    let slug = m["slug"].as_str().filter(|s| !s.is_empty())?.to_string();
                    let name = m["display_name"]
                        .as_str()
                        .filter(|s| !s.is_empty())
                        .map_or_else(|| slug.clone(), str::to_string);
                    Some(Listed { slug, name })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Asks the account's API for its models. Errors are redacted and name what the API answered.
pub async fn fetch(store: &Store) -> Result<Vec<Listed>, String> {
    let access = store.chatgpt_access(false).await.map_err(|e| mu_base::secrets::redact_keys(&e.to_string()))?;
    let base = store.chatgpt_api_base();
    let client = net::http_client().map_err(|e| e.to_string())?;
    let resp = client
        .get(format!("{base}/models"))
        .bearer_auth(&access.token)
        .timeout(FETCH_TIMEOUT)
        .send()
        .await
        .map_err(|e| mu_base::secrets::redact_keys(&format!("cannot reach {}: {e}", net::host_of(&base))))?;
    let status = resp.status().as_u16();
    let request_id = resp.headers().get("x-request-id").and_then(|v| v.to_str().ok()).map(str::to_owned);
    let text = resp.text().await.unwrap_or_default();
    if !(200..300).contains(&status) {
        let e = retry::chatgpt_error(status, None, &text, request_id.as_deref());
        return Err(mu_base::secrets::redact_keys(&e.to_string()));
    }
    Ok(parse(&text))
}

fn update(change: impl FnOnce(&mut File)) -> bool {
    let mut file = load();
    change(&mut file);
    serde_json::to_string(&file).map(|t| cache().save_private(&t)).unwrap_or(false)
}

/// Fetches the signed-in account's models and caches them. A failure, or an empty list, keeps the
/// old cache and is due again after an hour. Returns how many models were cached.
pub async fn refresh(store: &Store) -> Result<usize> {
    let Some(account) = store.chatgpt_account() else { bail!("not signed in with ChatGPT") };
    let k = key(&account.subject);
    match fetch(store).await {
        Ok(rows) if !rows.is_empty() => {
            let n = rows.len();
            update(|f| {
                f.fetched_at.insert(k.clone(), mu_base::message::now_ms());
                f.failed_at.remove(&k);
                f.models.insert(k, rows);
            });
            Ok(n)
        }
        Ok(_) => {
            update(|f| _ = f.failed_at.insert(k, mu_base::message::now_ms()));
            bail!("ChatGPT lists no models for this account")
        }
        Err(e) => {
            update(|f| _ = f.failed_at.insert(k, mu_base::message::now_ms()));
            bail!("{e}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BODY: &str = r#"{"models":[
      {"slug":"gpt-6.1-sol","display_name":"GPT-6.1 Sol","visibility":"list","context_window":400000},
      {"slug":"internal-eval","display_name":"Eval","visibility":"hide"},
      {"slug":"gpt-6-mini","visibility":"list"},
      {"display_name":"no slug","visibility":"list"}]}"#;

    #[test]
    fn only_listed_models_in_server_order() {
        let v = parse(BODY);
        assert_eq!(
            v,
            vec![
                Listed { slug: "gpt-6.1-sol".into(), name: "GPT-6.1 Sol".into() },
                Listed { slug: "gpt-6-mini".into(), name: "gpt-6-mini".into() },
            ]
        );
        assert!(parse("not json").is_empty() && parse("{}").is_empty());
    }

    #[test]
    fn the_cache_is_per_account_and_names_no_account() {
        mu_testutil::home();
        let (a, b) = ("sub-alpha-0001", "sub-beta-0002");
        assert!(stale(a));
        let mut f = File::default();
        f.fetched_at.insert(key(a), mu_base::message::now_ms());
        f.models.insert(key(a), parse(BODY));
        assert!(update(|x| *x = f));
        assert_eq!(cached(a).len(), 2);
        assert!(cached(b).is_empty(), "another account sees nothing");
        assert!(!stale(a) && stale(b));
        let text = std::fs::read_to_string(mu_common::paths::profile_home().join("chatgpt-models.json")).unwrap();
        assert!(!text.contains(a), "the file names no account");
    }
}
