//! The ChatGPT section of the key store, and the access-token lifecycle over it. A refresh rotates
//! the refresh token, so refreshes are serialized twice: an in-process mutex, then the store's lock
//! file, which also covers other mu processes. Under both the file is re-read, so a token another
//! session just refreshed is used as it is.

use super::oauth::{self, RefreshError};
use super::{
    API_BASE, Access, Account, ChatgptData, Endpoints, Known, REDACT_ID_LABEL, REDACT_LABEL, REDACT_REFRESH_LABEL,
    Tokens,
};
use crate::auth::store::{Store, check_base_url};
use anyhow::{Context, Result, anyhow, bail};
use mu_common::fs::FileLock;
use std::time::{SystemTime, UNIX_EPOCH};

/// Serializes refreshes within this process; the lock file does it across processes.
static REFRESH: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const SIGN_IN_HINT: &str = "run /login (or `mu auth login openai-chatgpt`) to sign in again";

/// Registers every stored token for redaction, under its own label.
pub(crate) fn register_tokens(c: &ChatgptData) {
    for t in c.accounts.iter().filter_map(|a| a.tokens.as_ref()) {
        register_token_set(t);
    }
}

fn register_token_set(t: &Tokens) {
    mu_base::secrets::register_stored(REDACT_LABEL, &t.access_token);
    mu_base::secrets::register_stored(REDACT_REFRESH_LABEL, &t.refresh_token);
    mu_base::secrets::register_stored(REDACT_ID_LABEL, &t.id_token);
}

fn endpoints_of(c: &ChatgptData) -> Endpoints {
    match c.auth_base.as_deref().and_then(|u| check_base_url(u).ok()) {
        Some(u) => Endpoints::at(&u),
        None => Endpoints::production(),
    }
}

/// A refused override falls back to production: a token never goes to a host the user did not pick.
pub(crate) fn api_base_of(c: &ChatgptData) -> String {
    c.api_base.as_deref().and_then(|u| check_base_url(u).ok()).unwrap_or_else(|| API_BASE.to_string())
}

fn access_of(t: &Tokens, a: &Account) -> Access {
    Access { token: t.access_token.clone(), label: a.label() }
}

fn now_secs() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

fn not_signed_in() -> anyhow::Error {
    anyhow!("not signed in with ChatGPT; {SIGN_IN_HINT}")
}

impl Store {
    /// A snapshot of the `chatgpt` section.
    pub fn chatgpt(&self) -> ChatgptData {
        self.chatgpt_cached()
    }

    /// The account in use, signed in or not.
    pub fn chatgpt_account(&self) -> Option<Account> {
        self.chatgpt_cached().active_account().cloned()
    }

    /// The endpoints: production unless the section overrides `authBase`.
    pub fn chatgpt_endpoints(&self) -> Endpoints {
        endpoints_of(&self.chatgpt_cached())
    }

    /// The Responses API base: `https://api.openai.com/v1` unless the section overrides `apiBase`.
    pub fn chatgpt_api_base(&self) -> String {
        api_base_of(&self.chatgpt_cached())
    }

    /// This store's `urn:uuid:…` host id; generated and saved on first use.
    pub fn chatgpt_host_id(&self) -> Result<String> {
        let mut id = None;
        self.update_chatgpt(|c| {
            let changed = c.host_id.is_none();
            let h = c.host_id.get_or_insert_with(|| format!("urn:uuid:{}", uuid::Uuid::new_v4()));
            id = Some(h.clone());
            changed
        })?;
        id.ok_or_else(|| anyhow!("no ChatGPT host id"))
    }

    /// The registration a new sign-in repeats: the active account's, signed in or not.
    pub fn chatgpt_known(&self) -> Option<Known> {
        self.chatgpt_account().map(|a| a.known())
    }

    /// Saves `account` (replacing the one with the same client id) and makes it the active one.
    pub fn chatgpt_save(&self, account: Account) -> Result<()> {
        if let Some(t) = &account.tokens {
            register_token_set(t);
        }
        self.update_chatgpt(|c| {
            let id = account.client_id.clone();
            match c.accounts.iter().position(|a| a.client_id == id) {
                Some(i) => c.accounts[i] = account,
                None => c.accounts.push(account),
            }
            c.active = Some(id);
            true
        })?;
        Ok(())
    }

    /// Drops the active account's tokens and keeps its registration and the host id. Returns the
    /// account as it was, tokens included, so the caller can revoke the refresh token.
    pub fn chatgpt_sign_out(&self) -> Result<Option<Account>> {
        let mut prev = None;
        self.update_chatgpt(|c| {
            let Some(id) = c.active.clone() else { return false };
            let Some(a) = c.accounts.iter_mut().find(|a| a.client_id == id) else { return false };
            if a.tokens.is_none() {
                return false;
            }
            prev = Some(a.clone());
            a.tokens = None;
            true
        })?;
        Ok(prev)
    }

    /// Removes the active account, its registration and tokens both, so the next sign-in registers a
    /// new client and may be another identity. Keeps the host id and the welcome flag. Returns the
    /// account as it was, tokens included, so the caller can revoke the refresh token.
    pub fn chatgpt_forget(&self) -> Result<Option<Account>> {
        let mut prev = None;
        self.update_chatgpt(|c| {
            let Some(id) = c.active.clone() else { return false };
            let Some(i) = c.accounts.iter().position(|a| a.client_id == id) else { return false };
            prev = Some(c.accounts.remove(i));
            c.active = None;
            true
        })?;
        Ok(prev)
    }

    pub fn chatgpt_welcomed(&self) -> bool {
        self.chatgpt_cached().welcomed
    }

    pub fn chatgpt_set_welcomed(&self) -> Result<()> {
        self.update_chatgpt(|c| !std::mem::replace(&mut c.welcomed, true))?;
        Ok(())
    }

    /// A usable access token. Refreshes first when it is at or past `refreshAt`, or when
    /// `force_refresh`; refreshes are serialized across threads and processes, and a token another
    /// session just refreshed is used as it is. Errors say to sign in again when the refresh token is unusable.
    pub async fn chatgpt_access(&self, force_refresh: bool) -> Result<Access> {
        // The token this call started with: a different one on disk after the lock means another session refreshed.
        let seen = self.chatgpt_account().and_then(|a| a.tokens.map(|t| t.access_token));
        let _turn = REFRESH.lock().await;
        let path = self.lock_path();
        let lock = tokio::task::spawn_blocking(move || FileLock::exclusive(&path)).await?;
        let _file = lock.context("locking the key store")?;

        let data = self.chatgpt_fresh()?;
        register_tokens(&data);
        let account = data.active_account().cloned().ok_or_else(not_signed_in)?;
        let tokens = account.tokens.clone().ok_or_else(not_signed_in)?;
        if !account.plan_usage() {
            bail!("the ChatGPT sign-in does not include plan usage; {SIGN_IN_HINT}");
        }
        let replaced = seen.as_deref().is_some_and(|s| s != tokens.access_token);
        let due = now_secs() >= tokens.refresh_at;
        if replaced || (!force_refresh && !due) {
            return Ok(access_of(&tokens, &account));
        }

        let fresh = match oauth::refresh(&endpoints_of(&data), &account).await {
            Ok(t) => t,
            Err(RefreshError::SignInAgain(m)) => {
                bail!("the ChatGPT sign-in has expired or was revoked ({m}); {SIGN_IN_HINT}")
            }
            Err(e @ RefreshError::Transient(_)) => return Err(anyhow::Error::new(e)),
        };
        let access = access_of(&fresh, &account);
        register_token_set(&fresh);
        // The old refresh token is spent by now: a failed save loses the rotated one, so it is an error.
        let id = account.client_id.clone();
        self.update_chatgpt_locked(|c| {
            if let Some(a) = c.accounts.iter_mut().find(|a| a.client_id == id) {
                a.tokens = Some(fresh);
            }
            true
        })
        .context("saving the refreshed ChatGPT sign-in")?;
        Ok(access)
    }
}

/// `mu auth test` and the `/login` check: asks the API for the account's model list.
pub async fn check(store: &Store) -> Result<Vec<String>, String> {
    crate::model::chatgpt_models::fetch(store).await.map(|ms| ms.into_iter().map(|m| m.slug).collect())
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::chatgpt::DIRECT_SCOPE;
    use crate::auth::store::Entry;
    use mu_testutil::{Reply, block_on, fake_server};
    use std::path::PathBuf;

    const ACCESS: &str = "acc-old-0123456789";
    const REFRESH_TOKEN: &str = "ref-old-0123456789";
    const NEW_ACCESS: &str = "acc-new-0123456789";
    /// `refresh_at` far away: the token is not due, so only a forced refresh reaches the issuer.
    const FAR: i64 = i64::MAX / 2;
    const DUE: i64 = 0;

    fn store(tag: &str) -> (Store, PathBuf) {
        let d = std::env::temp_dir().join(format!("mu-chatgpt-{tag}-{}", uuid::Uuid::new_v4()));
        (Store::at(d.join("auth.json")), d)
    }

    fn account(access: &str, refresh_at: i64, scopes: &[&str]) -> Account {
        Account {
            client_id: "oaiapp_test_client".into(),
            subject: "user-test-subject".into(),
            email: Some("me@example.com".into()),
            tokens: Some(Tokens {
                id_token: "id-old-0123456789".into(),
                access_token: access.into(),
                refresh_token: REFRESH_TOKEN.into(),
                refresh_at,
                scopes: scopes.iter().map(|s| s.to_string()).collect(),
            }),
            added: "2026-11-06T00:00:00Z".into(),
        }
    }

    /// A signed-in store whose issuer and API are the given fake servers.
    fn signed_in(s: &Store, auth: &str, api: &str, refresh_at: i64) {
        s.update_chatgpt(|c| {
            c.auth_base = Some(auth.into());
            c.api_base = Some(api.into());
            true
        })
        .unwrap();
        s.chatgpt_save(account(ACCESS, refresh_at, &["openid", DIRECT_SCOPE])).unwrap();
    }

    /// A refresh answer. It omits `id_token`, so the stored one is kept.
    fn token_reply(access: &str) -> Reply {
        let body = format!(
            r#"{{"access_token":"{access}","refresh_token":"ref-new-0123456789","token_type":"Bearer","expires_in":3600,"scope":"openid profile email offline_access resource.invoke chatgpt.tokens.use.direct"}}"#
        );
        Reply::new(200, &body).header("content-type", "application/json")
    }

    fn file_json(s: &Store) -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(s.path()).unwrap()).unwrap()
    }

    #[test]
    fn the_section_is_version_3_only_when_it_holds_something() {
        let (s, d) = store("ver");
        s.set("anthropic", Entry::literal("sk-ant-test-0123456789")).unwrap();
        assert_eq!(file_json(&s)["version"], 2);
        assert!(file_json(&s).get("chatgpt").is_none());
        let host = s.chatgpt_host_id().unwrap();
        assert_eq!(file_json(&s)["version"], 3);
        assert_eq!(file_json(&s)["chatgpt"]["hostId"], host.as_str());
        assert!(host.starts_with("urn:uuid:") && s.chatgpt_host_id().unwrap() == host, "one host id per store");
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn a_chatgpt_less_file_reads_and_is_not_rewritten() {
        let (s, d) = store("old");
        std::fs::create_dir_all(d.clone()).unwrap();
        let text = r#"{"version":1,"providers":{"openai":{"key":"sk-test-0123456789"}}}"#;
        std::fs::write(s.path(), text).unwrap();
        assert!(s.get("openai").is_some() && s.chatgpt().accounts.is_empty());
        assert!(s.check().is_ok());
        assert_eq!(std::fs::read_to_string(s.path()).unwrap(), text);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn sign_out_keeps_the_registration_and_host_id_and_returns_the_tokens() {
        let (s, d) = store("out");
        let host = s.chatgpt_host_id().unwrap();
        s.chatgpt_save(account(ACCESS, FAR, &[DIRECT_SCOPE])).unwrap();
        let prev = s.chatgpt_sign_out().unwrap().expect("the account was signed in");
        assert_eq!(prev.tokens.as_ref().map(|t| t.refresh_token.as_str()), Some(REFRESH_TOKEN));
        let now = s.chatgpt_account().unwrap();
        assert!(!now.signed_in() && now.client_id == "oaiapp_test_client");
        assert_eq!(s.chatgpt_known().map(|k| k.client_id).as_deref(), Some("oaiapp_test_client"));
        assert_eq!(s.chatgpt().host_id.as_deref(), Some(host.as_str()));
        assert!(s.chatgpt_sign_out().unwrap().is_none(), "a second sign-out has nothing to return");
        assert!(!s.chatgpt_welcomed());
        s.chatgpt_set_welcomed().unwrap();
        assert!(s.chatgpt_welcomed());
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn forget_drops_the_registration_and_keeps_the_host_id() {
        let (s, d) = store("forget");
        let host = s.chatgpt_host_id().unwrap();
        s.chatgpt_save(account(ACCESS, FAR, &[DIRECT_SCOPE])).unwrap();
        let prev = s.chatgpt_forget().unwrap().expect("the account was saved");
        assert_eq!(prev.tokens.as_ref().map(|t| t.refresh_token.as_str()), Some(REFRESH_TOKEN));
        assert!(s.chatgpt_account().is_none() && s.chatgpt_known().is_none());
        assert!(s.chatgpt().accounts.is_empty());
        assert_eq!(s.chatgpt().host_id.as_deref(), Some(host.as_str()));
        assert!(s.chatgpt_forget().unwrap().is_none(), "a second forget has nothing to return");
        let mut other = account(ACCESS, FAR, &[DIRECT_SCOPE]);
        other.client_id = "oaiapp_other_client".into();
        other.subject = "user-other-subject".into();
        s.chatgpt_save(other).unwrap();
        assert_eq!(s.chatgpt_account().unwrap().subject, "user-other-subject");
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn stored_tokens_are_redacted_under_their_own_labels() {
        let (s, d) = store("redact");
        s.chatgpt_save(account("acc-redact-0123456789", FAR, &[DIRECT_SCOPE])).unwrap();
        let r = mu_base::secrets::redact_keys(&format!("a=acc-redact-0123456789 r={REFRESH_TOKEN}"));
        assert!(!r.contains("acc-redact") && !r.contains(REFRESH_TOKEN), "{r}");
        assert!(r.contains("[REDACTED:OPENAI_CHATGPT_TOKEN]"), "{r}");
        assert!(r.contains("[REDACTED:OPENAI_CHATGPT_REFRESH_TOKEN]"), "{r}");
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn a_refused_base_override_is_reported_and_not_used() {
        let (s, d) = store("base");
        s.update_chatgpt(|c| {
            c.api_base = Some("http://example.com/v1".into());
            true
        })
        .unwrap();
        let e = s.check().unwrap_err().to_string();
        assert!(e.contains("base URL override"), "{e}");
        assert_eq!(s.chatgpt_api_base(), API_BASE, "a token never goes to a host the user did not pick");
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn concurrent_callers_refresh_once_and_share_the_token() {
        block_on(async {
            let (auth, hits) = fake_server(vec![token_reply(NEW_ACCESS)]).await;
            let (api, _) = fake_server(vec![Reply::new(200, "{}")]).await;
            let (s, d) = store("once");
            signed_in(&s, &auth, &api, DUE);
            // A second handle on the same file stands in for another process.
            let other = Store::at(s.path().to_path_buf());
            let (a, b, c, e) = tokio::join!(
                s.chatgpt_access(false),
                other.chatgpt_access(false),
                s.chatgpt_access(false),
                other.chatgpt_access(false)
            );
            for r in [a, b, c, e] {
                assert_eq!(r.unwrap().token, NEW_ACCESS);
            }
            assert_eq!(hits.lock().unwrap().len(), 1, "one refresh for every caller");
            let saved = s.chatgpt_account().unwrap().tokens.unwrap();
            assert_eq!((saved.access_token.as_str(), saved.refresh_token.as_str()), (NEW_ACCESS, "ref-new-0123456789"));
            let _ = std::fs::remove_dir_all(d);
        });
    }

    #[test]
    fn a_forced_refresh_happens_even_when_the_token_is_not_due() {
        block_on(async {
            let (auth, hits) = fake_server(vec![token_reply(NEW_ACCESS)]).await;
            let (api, _) = fake_server(vec![Reply::new(200, "{}")]).await;
            let (s, d) = store("force");
            signed_in(&s, &auth, &api, FAR);
            assert_eq!(s.chatgpt_access(false).await.unwrap().token, ACCESS, "not due: no refresh");
            assert!(hits.lock().unwrap().is_empty());
            assert_eq!(s.chatgpt_access(true).await.unwrap().token, NEW_ACCESS);
            assert_eq!(hits.lock().unwrap().len(), 1);
            let _ = std::fs::remove_dir_all(d);
        });
    }

    #[test]
    fn an_unusable_refresh_token_keeps_the_tokens_and_says_to_sign_in() {
        block_on(async {
            let (auth, _) = fake_server(vec![Reply::new(400, r#"{"error":"invalid_grant"}"#)]).await;
            let (api, _) = fake_server(vec![Reply::new(200, "{}")]).await;
            let (s, d) = store("again");
            signed_in(&s, &auth, &api, DUE);
            let e = s.chatgpt_access(false).await.unwrap_err().to_string();
            assert!(e.contains("mu auth login openai-chatgpt"), "{e}");
            assert_eq!(s.chatgpt_account().unwrap().tokens.unwrap().access_token, ACCESS, "the caller decides");
            let _ = std::fs::remove_dir_all(d);
        });
    }

    #[test]
    fn a_transient_refresh_failure_keeps_the_tokens() {
        block_on(async {
            let (auth, _) = fake_server(vec![Reply::new(503, "busy")]).await;
            let (api, _) = fake_server(vec![Reply::new(200, "{}")]).await;
            let (s, d) = store("transient");
            signed_in(&s, &auth, &api, DUE);
            let e = s.chatgpt_access(false).await.unwrap_err();
            assert!(matches!(e.downcast_ref::<RefreshError>(), Some(RefreshError::Transient(_))), "{e}");
            assert_eq!(s.chatgpt_account().unwrap().tokens.unwrap().access_token, ACCESS);
            let _ = std::fs::remove_dir_all(d);
        });
    }

    #[test]
    fn no_sign_in_or_no_plan_usage_is_a_clear_error() {
        block_on(async {
            let (s, d) = store("none");
            let e = s.chatgpt_access(false).await.unwrap_err().to_string();
            assert!(e.contains("not signed in with ChatGPT"), "{e}");
            s.chatgpt_save(account(ACCESS, FAR, &["openid"])).unwrap();
            let e = s.chatgpt_access(false).await.unwrap_err().to_string();
            assert!(e.contains("does not include plan usage"), "{e}");
            let _ = std::fs::remove_dir_all(d);
        });
    }

    #[test]
    fn check_lists_the_models_the_account_can_use() {
        block_on(async {
            let body =
                r#"{"models":[{"slug":"gpt-6.1-sol","visibility":"list"},{"slug":"hidden-1","visibility":"hide"}]}"#;
            let (api, seen) = fake_server(vec![Reply::new(200, body)]).await;
            let (auth, _) = fake_server(vec![Reply::new(500, "")]).await;
            let (s, d) = store("check");
            signed_in(&s, &auth, &api, FAR);
            assert_eq!(check(&s).await.unwrap(), ["gpt-6.1-sol"]);
            let req = seen.lock().unwrap()[0].to_lowercase();
            assert!(
                req.starts_with("get /models ") && req.contains(&format!("authorization: bearer {ACCESS}")),
                "{req}"
            );
            let _ = std::fs::remove_dir_all(d);
        });
    }
}
