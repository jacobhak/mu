//! Sign in with ChatGPT: OpenAI's way for an open-source tool to run Responses requests on a
//! user's ChatGPT plan. `oauth` is the protocol (dynamic client registration, PKCE login, refresh,
//! revoke, ID-token checks), `store` keeps the credentials in `auth.json`, and the `openai-chatgpt`
//! provider (see `provider::providers`) sends the access token to `api.openai.com/v1/responses`.
//! Nothing here touches omp or chatgpt.com/backend-api.

pub mod browser;
pub mod oauth;
pub mod signin;
pub mod store;

use serde::{Deserialize, Serialize};
use std::fmt;

/// The provider id of models that run on the plan: `openai-chatgpt/gpt-6.1-sol`. A separate id from
/// `openai` (API key) so the model picker, not a precedence rule, chooses the credential.
pub const PROVIDER_ID: &str = "openai-chatgpt";
/// Shown after `· ` in the status line and next to models in pickers.
pub const LABEL: &str = "ChatGPT plan";
pub const USAGE_URL: &str = "https://chatgpt.com/settings/usage";
/// The name mu registers with (`agent_name_hint`), used for every installation.
pub const AGENT_NAME: &str = "mu";
/// Scope that authorizes ChatGPT plan usage; sign-in alone does not.
pub const DIRECT_SCOPE: &str = "chatgpt.tokens.use.direct";
pub const API_BASE: &str = "https://api.openai.com/v1";
pub const AUTH_BASE: &str = "https://auth.openai.com";
/// The labels the access, refresh and ID tokens are redacted under.
pub const REDACT_LABEL: &str = "OPENAI_CHATGPT_TOKEN";
pub const REDACT_REFRESH_LABEL: &str = "OPENAI_CHATGPT_REFRESH_TOKEN";
pub const REDACT_ID_LABEL: &str = "OPENAI_CHATGPT_ID_TOKEN";

/// The OAuth endpoints, from `https://auth.openai.com/.well-known/openid-configuration`.
#[derive(Debug, Clone, PartialEq)]
pub struct Endpoints {
    pub issuer: String,
    pub authorize: String,
    pub token: String,
    pub revoke: String,
    pub jwks: String,
}

impl Endpoints {
    pub fn production() -> Self {
        Self::at(AUTH_BASE)
    }

    /// The endpoints under `base` (no trailing slash): production, or a test server.
    pub fn at(base: &str) -> Self {
        let base = base.trim_end_matches('/');
        Self {
            issuer: base.to_string(),
            authorize: format!("{base}/api/accounts/authorize"),
            token: format!("{base}/api/accounts/oauth/token"),
            revoke: format!("{base}/api/accounts/oauth/revoke"),
            jwks: format!("{base}/.well-known/jwks.json"),
        }
    }
}

/// The token set of a signed-in account. Replaced as a whole on every refresh.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tokens {
    /// Kept for `id_token_hint` on the next sign-in.
    pub id_token: String,
    pub access_token: String,
    pub refresh_token: String,
    /// Unix seconds from which the access token is refreshed (its expiry minus a margin).
    pub refresh_at: i64,
    pub scopes: Vec<String>,
}

impl fmt::Debug for Tokens {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tokens").field("refresh_at", &self.refresh_at).field("scopes", &self.scopes).finish()
    }
}

/// One registration (an issued client id bound to a verified ChatGPT identity). Signing out drops
/// `tokens` and keeps the rest, so the next sign-in reuses the client instead of registering again.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Account {
    /// The issued `oaiapp_…` id, never `dynamic_agent_client`.
    pub client_id: String,
    /// The verified ID token's `sub`.
    pub subject: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<Tokens>,
    pub added: String,
}

impl Account {
    pub fn signed_in(&self) -> bool {
        self.tokens.is_some()
    }

    /// Whether the grant includes `chatgpt.tokens.use.direct`.
    pub fn plan_usage(&self) -> bool {
        self.tokens.as_ref().is_some_and(|t| t.scopes.iter().any(|s| s == DIRECT_SCOPE))
    }

    /// What a user recognises the account by.
    pub fn label(&self) -> String {
        self.email.clone().unwrap_or_else(|| self.subject.clone())
    }

    /// What a later sign-in reuses.
    pub fn known(&self) -> Known {
        Known {
            client_id: self.client_id.clone(),
            id_token: self.tokens.as_ref().map(|t| t.id_token.clone()),
            email: self.email.clone(),
        }
    }
}

/// A saved registration a sign-in repeats instead of registering a new client.
#[derive(Clone, PartialEq)]
pub struct Known {
    pub client_id: String,
    /// Sent as `id_token_hint` (it may be expired); absent after a sign-out.
    pub id_token: Option<String>,
    /// Sent as `login_hint`.
    pub email: Option<String>,
}

impl fmt::Debug for Known {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Known")
            .field("client_id", &self.client_id)
            .field("id_token", &self.id_token.as_ref().map(|_| "…"))
            .field("email", &self.email)
            .finish()
    }
}

/// The `chatgpt` section of `auth.json` (store version 3).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatgptData {
    /// `urn:uuid:…`, generated once for this store and kept through sign-outs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_id: Option<String>,
    /// The `client_id` of the account in use. One account today; the list leaves room for more.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub accounts: Vec<Account>,
    /// The first-sign-in "you're using your ChatGPT plan" message was shown.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub welcomed: bool,
    /// Test seams: replace `https://auth.openai.com` and `https://api.openai.com/v1` (https, or http to loopback).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_base: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_base: Option<String>,
}

impl ChatgptData {
    pub fn active_account(&self) -> Option<&Account> {
        let id = self.active.as_deref()?;
        self.accounts.iter().find(|a| a.client_id == id)
    }
}

/// A valid access token for a request.
pub struct Access {
    pub token: String,
    pub label: String,
}

/// The token is never printed.
impl fmt::Debug for Access {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Access").field("label", &self.label).finish()
    }
}
