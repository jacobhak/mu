//! `/login` and `/logout` (table rows "/login, /logout, keys"): the stored API keys, which key a
//! provider id stands for, checking a key against its provider, and the MCP servers that need a login.

use super::local::{Local, Registry};
use super::{Answer, pending, ready};
use mu_provider::auth::chatgpt::{Account, PROVIDER_ID};
use mu_provider::auth::check;
use mu_provider::auth::store::{Entry, Store};
use mu_provider::provider::custom::{AuthMode, KeyState};
use mu_provider::provider::providers::{self, KeyTarget, SEARCH_KEYS};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::rc::Rc;

pub const KEY_ROWS: &str = "key_rows";
pub const KEY_TARGET: &str = "key_target";
pub const CHECK_KEY: &str = "check_key";
pub const SAVE_KEY: &str = "save_key";
pub const SET_CHAT: &str = "set_chat";
pub const REMOVE_KEY: &str = "remove_key";
pub const LOGOUT_LIST: &str = "logout_list";
pub const LOGIN_PROVIDERS: &str = "login_providers";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TargetKind {
    /// A chat provider.
    Provider,
    /// A search-only service.
    Search,
    /// A custom endpoint.
    Custom,
}

/// What the UI needs of a `KeyTarget`: the target itself holds `'static` definitions and cannot
/// cross the wire, so the host resolves it by id and sends this.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct KeyInfo {
    pub id: String,
    pub label: String,
    pub env_hint: String,
    /// Empty for a custom endpoint.
    pub key_url: String,
    pub kind: TargetKind,
    /// A custom endpoint that takes no key at all.
    pub keyless: bool,
    /// anthropic and openai: the key can serve chat or only web search, so the user must say which.
    pub asks_chat: bool,
    /// `env_hint` is set to a value other than blanks on the host.
    pub env_set: bool,
    /// Whether the stored key's chat models use it; `None` when no key is stored.
    pub stored_chat: Option<bool>,
    /// The custom-endpoint file on the host, named when a keyless endpoint is asked for a key.
    pub providers_path: PathBuf,
}

impl KeyInfo {
    fn of(t: &KeyTarget, env_set: bool, stored_chat: Option<bool>, providers_path: PathBuf) -> Self {
        Self {
            id: t.id().into(),
            label: t.label().into(),
            env_hint: t.env_hint().into(),
            key_url: t.key_url().into(),
            kind: match t {
                KeyTarget::Provider(_) => TargetKind::Provider,
                KeyTarget::Search(_) => TargetKind::Search,
                KeyTarget::Custom(_) => TargetKind::Custom,
            },
            keyless: t.is_keyless(),
            asks_chat: t.asks_chat(),
            env_set,
            stored_chat,
            providers_path,
        }
    }

    /// A built-in target as a fixture: nothing stored, the environment as it is.
    #[cfg(test)]
    pub(crate) fn builtin(id: &str) -> Option<Self> {
        let t = KeyTarget::builtin(id)?;
        let set = env_is_set(t.env_hint());
        Some(Self::of(&t, set, None, PathBuf::new()))
    }
}

/// A custom endpoint's row of the key list.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CustomRow {
    pub id: String,
    pub label: String,
    /// Takes no key (`auth = "none"`).
    pub keyless: bool,
    pub state: KeyState,
}

/// The ChatGPT sign-in as a row shows it. The account's tokens never leave the host.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChatgptLabel {
    pub signed_in: bool,
    /// The grant includes plan usage.
    pub plan_usage: bool,
    /// The email, or the account's subject when there is none.
    pub label: String,
    pub added: String,
}

impl ChatgptLabel {
    fn of(a: &Account) -> Self {
        Self { signed_in: a.signed_in(), plan_usage: a.plan_usage(), label: a.label(), added: a.added.clone() }
    }
}

/// What the `/login` key rows show.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct KeyFacts {
    /// The stored keys: provider id and whether its chat models use it.
    pub stored: Vec<(String, bool)>,
    /// The key variables (`ANTHROPIC_API_KEY`, …) that are set on the host.
    pub env_set: Vec<String>,
    pub chatgpt: Option<ChatgptLabel>,
    pub customs: Vec<CustomRow>,
    /// `providers.toml` did not read: the custom endpoints are missing from the list.
    pub error: Option<String>,
}

/// A stored key `/logout` can remove.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LogoutRow {
    /// The provider id; the ChatGPT sign-in is `openai-chatgpt`.
    pub id: String,
    pub label: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct LogoutList {
    pub rows: Vec<LogoutRow>,
    pub error: Option<String>,
}

/// The MCP servers that need a login.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct LoginList {
    /// MCP server name and its URL.
    pub mcp: Vec<(String, String)>,
}

pub(super) fn register(r: &mut Registry) {
    r.add(KEY_ROWS, key_rows);
    r.add(KEY_TARGET, key_target);
    r.add(CHECK_KEY, check_key);
    r.add(SAVE_KEY, save_key);
    r.add(SET_CHAT, set_chat);
    r.add(REMOVE_KEY, remove_key);
    r.add(LOGOUT_LIST, logout_list);
    r.add(LOGIN_PROVIDERS, login_providers);
}

/// Whether `name` is set to a value other than blanks.
pub(crate) fn env_is_set(name: &str) -> bool {
    std::env::var(name).is_ok_and(|v| !v.trim().is_empty())
}

fn key_rows(l: &Rc<Local>, args: serde_json::Value) -> Answer {
    ready(args, |(): ()| {
        let (customs, error) = match l.providers.custom_registry().snapshot() {
            Ok(c) => (c, None),
            Err(e) => (Default::default(), Some(format!("{e:#}"))),
        };
        let store = l.providers.store();
        let env_set = providers::api_key_defs()
            .map(|d| d.env_hint)
            .chain(SEARCH_KEYS.iter().map(|d| d.env_hint))
            .filter(|n| env_is_set(n))
            .map(String::from)
            .collect();
        Ok(KeyFacts {
            stored: store.list().into_iter().map(|(p, e)| (p, e.routes_chat())).collect(),
            env_set,
            chatgpt: store.chatgpt_account().as_ref().map(ChatgptLabel::of),
            customs: customs
                .values()
                .map(|c| CustomRow {
                    id: c.id.clone(),
                    label: c.label().into(),
                    keyless: c.auth == AuthMode::None,
                    state: c.key_state(store),
                })
                .collect(),
            error,
        })
    })
}

/// `id`: the key target it stands for, `None` when it is none (`KeyTarget::find`).
fn key_target(l: &Rc<Local>, args: serde_json::Value) -> Answer {
    ready(args, |id: String| {
        let found = KeyTarget::find(&id, l.providers.custom_registry()).map_err(|e| format!("{e:#}"))?;
        Ok(found.map(|t| {
            let stored_chat = l.providers.store().get(t.id()).map(|e| e.routes_chat());
            let path = l.providers.custom_registry().path().to_path_buf();
            KeyInfo::of(&t, env_is_set(t.env_hint()), stored_chat, path)
        }))
    })
}

/// `{id, entry}`: resolves the entry (reads `$NAME` or runs the `!command`) and asks the provider
/// whether the key works. Nothing is saved.
fn check_key(l: &Rc<Local>, args: serde_json::Value) -> Answer {
    pending(args, |(id, entry): (String, Entry)| {
        let target = KeyTarget::find(&id, l.providers.custom_registry())
            .map_err(|e| format!("{e:#}"))
            .and_then(|t| t.ok_or_else(|| format!("no API key support for `{id}`")));
        let entry = pinned_base_url(l.providers.store(), &id, entry);
        async move { check::check_target(&target?, &entry).await }
    })
}

/// A stored key is sent only to its stored `base_url`: the client's `base_url` counts only for a
/// provider with no stored key.
pub(super) fn pinned_base_url(store: &Store, id: &str, mut entry: Entry) -> Entry {
    if let Some(stored) = store.get(id) {
        entry.base_url = stored.base_url;
    }
    entry
}

fn save_key(l: &Rc<Local>, args: serde_json::Value) -> Answer {
    ready(args, |(id, entry): (String, Entry)| l.providers.store().set(&id, entry).map_err(|e| e.to_string()))
}

/// `{id, chat}`: whether the stored key was found.
fn set_chat(l: &Rc<Local>, args: serde_json::Value) -> Answer {
    ready(args, |(id, chat): (String, bool)| l.providers.store().set_chat(&id, chat).map_err(|e| e.to_string()))
}

/// `id`: whether a key was stored.
fn remove_key(l: &Rc<Local>, args: serde_json::Value) -> Answer {
    ready(args, |id: String| l.providers.store().remove(&id).map_err(|e| e.to_string()))
}

fn logout_list(l: &Rc<Local>, args: serde_json::Value) -> Answer {
    ready(args, |(): ()| {
        let (custom, error) = match l.providers.custom_registry().snapshot() {
            Ok(c) => (c, None),
            Err(e) => (Default::default(), Some(format!("{e:#}"))),
        };
        let store = l.providers.store();
        let mut rows: Vec<LogoutRow> = store
            .list()
            .into_iter()
            .map(|(p, _)| p)
            .filter(|p| KeyTarget::builtin(p).is_some() || custom.contains_key(p))
            .map(|p| {
                let label = KeyTarget::builtin(&p)
                    .map(|t| t.label().to_string())
                    .or_else(|| custom.get(&p).map(|c| c.label().to_string()))
                    .unwrap_or_else(|| p.clone());
                LogoutRow { id: p, label }
            })
            .collect();
        if store.chatgpt_account().is_some_and(|a| a.signed_in()) {
            rows.push(LogoutRow { id: PROVIDER_ID.into(), label: String::new() });
        }
        Ok(LogoutList { rows, error })
    })
}

fn login_providers(l: &Rc<Local>, args: serde_json::Value) -> Answer {
    ready(args, |(): ()| Ok(LoginList { mcp: l.plugins.needs_login() }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use mu_provider::auth::check::Check;

    #[test]
    fn the_replies_survive_the_wire() {
        let info = KeyInfo::builtin("openai").unwrap();
        let back: KeyInfo = serde_json::from_value(serde_json::to_value(&info).unwrap()).unwrap();
        assert_eq!(back, info);
        assert_eq!((back.kind, back.asks_chat, back.keyless), (TargetKind::Provider, true, false));

        let facts = KeyFacts {
            stored: vec![("anthropic".into(), false)],
            env_set: vec!["EXA_API_KEY".into()],
            chatgpt: None,
            customs: vec![CustomRow {
                id: "lab".into(),
                label: "Lab".into(),
                keyless: true,
                state: KeyState::NotNeeded,
            }],
            error: Some("bad file".into()),
        };
        let back: KeyFacts = serde_json::from_value(serde_json::to_value(&facts).unwrap()).unwrap();
        assert_eq!(back, facts);

        let logout = LogoutList { rows: vec![LogoutRow { id: "exa".into(), label: "Exa".into() }], error: None };
        let back: LogoutList = serde_json::from_value(serde_json::to_value(&logout).unwrap()).unwrap();
        assert_eq!(back, logout);

        let login = LoginList { mcp: vec![("docs".into(), "https://x".into())] };
        let back: LoginList = serde_json::from_value(serde_json::to_value(&login).unwrap()).unwrap();
        assert_eq!(back, login);

        for c in [Check::Valid, Check::Rejected("no".into()), Check::Unreachable("down".into()), Check::Unchecked] {
            let back: Check = serde_json::from_value(serde_json::to_value(&c).unwrap()).unwrap();
            assert_eq!(back, c);
        }
    }
}
