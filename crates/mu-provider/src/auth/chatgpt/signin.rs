//! What a finished sign-in and a sign-out do to the store. The CLI and the TUI both call these, so
//! the identity and plan-usage rules are the same in both.

use super::{Account, oauth};
use crate::auth::store::Store;

/// A sign-in that was saved.
pub struct Saved {
    pub label: String,
    /// Whether the grant includes plan usage. When false the sign-in is kept but cannot run models.
    pub plan_usage: bool,
    /// The first plan sign-in of this store: show the one-time "you're using your plan" message.
    pub welcome: bool,
}

/// Saves a finished sign-in as the active account. A sign-in as a different identity than the saved
/// registration's is refused and changes nothing.
pub fn save(store: &Store, account: Account) -> Result<Saved, String> {
    if let Some(prev) = store.chatgpt_account().filter(|p| p.subject != account.subject) {
        return Err(format!(
            "this ChatGPT sign-in is for {} but mu is registered to {}; nothing changed",
            account.label(),
            prev.label()
        ));
    }
    let label = account.label();
    let plan_usage = account.plan_usage();
    store.chatgpt_save(account).map_err(|e| e.to_string())?;
    let welcome = plan_usage && !store.chatgpt_welcomed();
    if welcome {
        store.chatgpt_set_welcomed().map_err(|e| e.to_string())?;
    }
    Ok(Saved { label, plan_usage, welcome })
}

/// Drops the active account's tokens and returns the account that held them, for `revoke`. `None`
/// when nothing was signed in.
pub fn sign_out(store: &Store) -> Result<Option<Account>, String> {
    let account = store.chatgpt_sign_out().map_err(|e| e.to_string())?;
    Ok(account.filter(Account::signed_in))
}

/// Removes the registration and its tokens, so the next sign-in registers a new client and may be
/// another identity. Returns the account as it was, for `revoke` when it was signed in; `None` when
/// no registration was saved.
pub fn forget(store: &Store) -> Result<Option<Account>, String> {
    store.chatgpt_forget().map_err(|e| e.to_string())
}

/// Revokes the refresh token at the issuer. An error means the sign-out is local only; the caller
/// says so and points at ChatGPT settings.
pub async fn revoke(store: &Store, account: &Account) -> Result<(), String> {
    oauth::revoke(&store.chatgpt_endpoints(), account).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::chatgpt::{DIRECT_SCOPE, Tokens};

    fn account(subject: &str, client_id: &str) -> Account {
        Account {
            client_id: client_id.into(),
            subject: subject.into(),
            email: None,
            tokens: Some(Tokens {
                id_token: "id-test-0123456789".into(),
                access_token: "acc-test-0123456789".into(),
                refresh_token: "ref-test-0123456789".into(),
                refresh_at: i64::MAX / 2,
                scopes: vec![DIRECT_SCOPE.into()],
            }),
            added: "2026-01-01T00:00:00Z".into(),
        }
    }

    #[test]
    fn after_forget_a_sign_in_as_another_identity_is_saved() {
        let d = std::env::temp_dir().join(format!("mu-signin-forget-{}", uuid::Uuid::new_v4()));
        let store = Store::at(d.join("auth.json"));
        save(&store, account("user-a", "oaiapp_a")).unwrap();
        let Err(refused) = save(&store, account("user-b", "oaiapp_b")) else {
            panic!("a different identity is refused while a registration is saved");
        };
        assert!(refused.contains("nothing changed"), "{refused}");
        let prev = forget(&store).unwrap().expect("the registration was saved");
        assert_eq!(prev.client_id, "oaiapp_a");
        assert!(forget(&store).unwrap().is_none(), "a second forget has nothing to return");
        assert!(save(&store, account("user-b", "oaiapp_b")).is_ok());
        let _ = std::fs::remove_dir_all(d);
    }
}
