//! The host that runs in the UI's own process: it answers from the provider registry, the context
//! and the plugin host, which are what the UI used to hold itself.

use super::{Answer, Host, HostInfo, Pushed};
use mu_common::paths;
use mu_provider::provider::client::Providers;
use mu_tools::context::Context;
use mu_tools::plugin::host::PluginHost;
use serde_json::Value;
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

/// What the handlers work on.
pub struct Local {
    pub providers: Rc<Providers>,
    pub ctx: Rc<Context>,
    pub plugins: Rc<PluginHost>,
    pub cwd: PathBuf,
    /// Where a handler's background task sends values the UI did not ask for.
    pub push: UnboundedSender<Pushed>,
}

pub type Handler = fn(&Rc<Local>, Value) -> Answer;

/// The calls the local host answers, by name. Each surface module adds its own in `register`.
#[derive(Default)]
pub struct Registry(HashMap<&'static str, Handler>);

impl Registry {
    pub fn add(&mut self, name: &'static str, handler: Handler) {
        let was = self.0.insert(name, handler);
        debug_assert!(was.is_none(), "host call {name} is registered twice");
    }
}

pub struct LocalHost {
    local: Rc<Local>,
    info: HostInfo,
    handlers: HashMap<&'static str, Handler>,
    pushed: RefCell<Option<UnboundedReceiver<Pushed>>>,
}

impl LocalHost {
    pub fn new(providers: Rc<Providers>, ctx: Rc<Context>, plugins: Rc<PluginHost>, cwd: PathBuf) -> Self {
        let (push, pushed) = unbounded_channel();
        let info = HostInfo {
            home: paths::home(),
            profile_home: paths::profile_home(),
            tmp: std::env::temp_dir(),
            cwd: cwd.clone(),
            profile: std::env::var("OMP_PROFILE")
                .or_else(|_| std::env::var("PI_PROFILE"))
                .ok()
                .filter(|p| !p.is_empty()),
        };
        let mut registry = Registry::default();
        super::startup::register(&mut registry);
        super::completion::register(&mut registry);
        super::models::register(&mut registry);
        super::keys::register(&mut registry);
        super::web::register(&mut registry);
        super::chatgpt::register(&mut registry);
        super::custom::register(&mut registry);
        super::mcp::register(&mut registry);
        super::sessions::register(&mut registry);
        super::plans::register(&mut registry);
        super::memory::register(&mut registry);
        super::program::register(&mut registry);
        Self {
            local: Rc::new(Local { providers, ctx, plugins, cwd, push }),
            info,
            handlers: registry.0,
            pushed: RefCell::new(Some(pushed)),
        }
    }
}

impl Host for LocalHost {
    fn info(&self) -> &HostInfo {
        &self.info
    }

    fn call(&self, name: &str, args: Value) -> Answer {
        match self.handlers.get(name) {
            Some(h) => h(&self.local, args),
            None => Answer::Ready(Err(format!("the host has no call {name}"))),
        }
    }

    fn pushed(&self) -> Option<UnboundedReceiver<Pushed>> {
        self.pushed.borrow_mut().take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::custom::panel::Saved;
    use crate::host::keys::KeyFacts;
    use mu_provider::auth::chatgpt::{Account, DIRECT_SCOPE, Tokens};
    use mu_provider::auth::store::Entry;
    use mu_provider::provider::custom::CustomProvider;
    use serde_json::json;

    const ANTHROPIC_KEY: &str = "sk-test-anthropic-0123456789abcdef";
    const CUSTOM_KEY: &str = "sk-test-custom-0123456789abcdef";
    const ID_TOKEN: &str = "test-id-token-0123456789abcdef";
    const ACCESS_TOKEN: &str = "test-access-token-0123456789abcdef";
    const REFRESH_TOKEN: &str = "test-refresh-token-0123456789abcdef";
    const SECRETS: [&str; 5] = [ANTHROPIC_KEY, CUSTOM_KEY, ID_TOKEN, ACCESS_TOKEN, REFRESH_TOKEN];

    fn host() -> LocalHost {
        use mu_base::config::Configs;
        let (ntx, _nrx) = unbounded_channel();
        let plugins = PluginHost::new(vec![], vec![], ntx, Default::default());
        let gateway = mu_provider::provider::endpoint::GatewaySource::new(plugins.gateway_start());
        let auth = mu_testutil::tmp().join("auth.json");
        let providers = Rc::new(Providers::for_tests_with_auth(Rc::new(Configs::default()), gateway, auth));
        LocalHost::new(providers, Rc::new(Context::default()), plugins, "/".into())
    }

    fn ready(h: &LocalHost, name: &str, args: Value) -> Result<Value, String> {
        match h.call(name, args) {
            Answer::Ready(r) => r,
            Answer::Pending(_) => panic!("{name} did not answer at once"),
        }
    }

    /// Stores a key for a chat provider, a custom endpoint with its own key, and a ChatGPT sign-in.
    fn seed(h: &LocalHost) {
        let store = h.local.providers.store();
        ready(h, "save_key", json!(["anthropic", Entry::literal(ANTHROPIC_KEY)])).unwrap();
        let saved = Saved {
            id: "lab".into(),
            provider: CustomProvider::new("https://lab.example/v1"),
            entry: Some(Entry::literal(CUSTOM_KEY)),
            editing: false,
            key_changed: true,
            listed: None,
        };
        ready(h, "custom_save", json!(saved)).unwrap();
        store
            .chatgpt_save(Account {
                client_id: "oaiapp_test".into(),
                subject: "user-test".into(),
                email: Some("me@example.com".into()),
                tokens: Some(Tokens {
                    id_token: ID_TOKEN.into(),
                    access_token: ACCESS_TOKEN.into(),
                    refresh_token: REFRESH_TOKEN.into(),
                    refresh_at: 0,
                    scopes: vec![DIRECT_SCOPE.into()],
                }),
                added: "2026-01-01T00:00:00Z".into(),
            })
            .unwrap();
    }

    #[test]
    fn no_host_reply_carries_a_stored_secret() {
        let h = host();
        seed(&h);
        // Calls that need arguments to say anything about a credential; every other call is made with `null`.
        let with_args = [
            ("key_target", json!("anthropic")),
            ("custom_get", json!("lab")),
            ("custom_get", json!("missing")),
            ("custom_env", json!("ANTHROPIC_API_KEY")),
            ("remove_key", json!("nothing-stored")),
        ];
        let mut names: Vec<&str> = h.handlers.keys().copied().collect();
        names.sort_unstable();
        for (name, _) in &with_args {
            assert!(names.contains(name), "the walk names a call that is not registered: {name}");
        }
        // These change what is stored, so they run after the rest have read it.
        let last = ["chatgpt_sign_out", "chatgpt_forget"];
        let mut walked = 0;
        for name in names.iter().filter(|n| !last.contains(n)).chain(&last) {
            let mut calls: Vec<Value> = vec![Value::Null];
            calls.extend(with_args.iter().filter(|(n, _)| n == name).map(|(_, a)| a.clone()));
            for args in calls {
                // A call that waits (the network, a child program) is not run; its future is dropped.
                if let Answer::Ready(reply) = h.call(name, args) {
                    let text = format!("{reply:?}");
                    for secret in SECRETS {
                        assert!(!text.contains(secret), "{name} replied with a stored secret: {text}");
                    }
                    walked += 1;
                }
            }
        }
        assert!(walked > 20, "only {walked} replies were read");
    }

    #[test]
    fn the_key_rows_and_the_custom_entry_carry_labels_and_a_marker_only() {
        let h = host();
        seed(&h);
        let facts: KeyFacts = serde_json::from_value(ready(&h, "key_rows", Value::Null).unwrap()).unwrap();
        let chatgpt = facts.chatgpt.expect("signed in");
        assert!(chatgpt.signed_in && chatgpt.plan_usage);
        assert_eq!(chatgpt.label, "me@example.com");

        let edit = ready(&h, "custom_get", json!("lab")).unwrap();
        assert_eq!(edit["entry"]["key"], json!(crate::host::custom::KEY_MARKER));
    }

    #[test]
    fn saving_an_entry_with_the_marker_keeps_the_stored_key() {
        let h = host();
        seed(&h);
        let edit = ready(&h, "custom_get", json!("lab")).unwrap();
        let entry: Entry = serde_json::from_value(edit["entry"].clone()).unwrap();
        let def: CustomProvider = serde_json::from_value(edit["def"].clone()).unwrap();
        let saved = |entry: Entry, key_changed| Saved {
            id: "lab".into(),
            provider: def.clone(),
            entry: Some(entry),
            editing: true,
            key_changed,
            listed: None,
        };
        let out = ready(&h, "custom_save", json!(saved(entry.clone(), false))).unwrap();
        assert_eq!(out, json!("Saved"));
        let store = h.local.providers.store();
        assert_eq!(store.get("lab").and_then(|e| e.key).as_deref(), Some(CUSTOM_KEY));

        // The marker is never written as a key.
        assert!(ready(&h, "custom_save", json!(saved(entry, true))).is_err());
        assert_eq!(store.get("lab").and_then(|e| e.key).as_deref(), Some(CUSTOM_KEY));
    }

    fn probe(url: &str, entry: Option<Entry>) -> crate::custom::panel::Probe {
        crate::custom::panel::Probe { id: "lab".into(), base_url: url.into(), credential: entry }
    }

    #[test]
    fn a_stored_key_is_checked_only_against_its_stored_url() {
        use crate::custom::panel::Fetch;
        use crate::host::custom::{KEY_MARKER, pin_probe};
        let h = host();
        seed(&h);
        let marked = || Some(Entry { key: Some(KEY_MARKER.into()), ..Entry::default() });

        let ok = pin_probe(&h.local, probe("https://lab.example/v1/", marked())).unwrap();
        assert_eq!(ok.credential.and_then(|e| e.key).as_deref(), Some(CUSTOM_KEY));

        let Err(Fetch::Failed(m)) = pin_probe(&h.local, probe("https://evil.example/v1", marked())) else {
            panic!("a stored key went to another URL")
        };
        assert!(m.contains("https://lab.example/v1") && !m.contains(CUSTOM_KEY), "{m}");
        assert!(pin_probe(&h.local, probe("https://evil.example/v1", Some(Entry::literal("typed")))).is_err());

        // An endpoint with no stored key takes the client's URL, and a marker has nothing to stand for.
        ready(&h, "remove_key", json!("lab")).unwrap();
        assert!(pin_probe(&h.local, probe("https://evil.example/v1", Some(Entry::literal("typed")))).is_ok());
        assert!(matches!(pin_probe(&h.local, probe("https://lab.example/v1", marked())), Err(Fetch::Credential(_))));
    }

    #[test]
    fn a_stored_provider_key_is_checked_against_its_stored_url_not_the_clients() {
        use crate::host::keys::pinned_base_url;
        let h = host();
        let store = h.local.providers.store();
        let theirs = Entry { base_url: Some("https://evil.example/v1".into()), ..Entry::literal("typed") };
        assert_eq!(
            pinned_base_url(store, "anthropic", theirs.clone()).base_url.as_deref(),
            Some("https://evil.example/v1"),
            "no key is stored, so the client's URL stands"
        );
        let stored = Entry { base_url: Some("https://proxy.example/v1".into()), ..Entry::literal(ANTHROPIC_KEY) };
        store.set("anthropic", stored).unwrap();
        assert_eq!(pinned_base_url(store, "anthropic", theirs).base_url.as_deref(), Some("https://proxy.example/v1"));
        let none = Entry::literal("typed");
        store.set("openai", Entry::literal("k")).unwrap();
        assert_eq!(
            pinned_base_url(store, "openai", Entry { base_url: Some("https://x/".into()), ..none }).base_url,
            None
        );
    }
}
