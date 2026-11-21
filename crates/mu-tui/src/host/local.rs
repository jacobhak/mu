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
    pub profile_home: PathBuf,
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
        super::results::register(&mut registry);
        super::state::register(&mut registry);
        Self {
            local: Rc::new(Local { providers, ctx, plugins, cwd, profile_home: info.profile_home.clone(), push }),
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

    /// A host whose temp directories are removed when it drops.
    struct TestHost {
        host: LocalHost,
        _dirs: [mu_testutil::TempDir; 2],
    }

    impl std::ops::Deref for TestHost {
        type Target = LocalHost;
        fn deref(&self) -> &LocalHost {
            &self.host
        }
    }

    fn host() -> TestHost {
        use mu_base::config::Configs;
        let (ntx, _nrx) = unbounded_channel();
        let plugins = PluginHost::new(vec![], vec![], ntx, Default::default());
        let gateway = mu_provider::provider::endpoint::GatewaySource::new(plugins.gateway_start());
        let auth_dir = mu_testutil::tmp();
        let auth = auth_dir.join("auth.json");
        let providers = Rc::new(Providers::for_tests_with_auth(Rc::new(Configs::default()), gateway, auth));
        let profile_dir = mu_testutil::tmp();
        let mut h = LocalHost::new(providers, Rc::new(Context::default()), plugins, "/".into());
        let l = &h.local;
        // Its own profile, so the state calls never touch the test process's shared one.
        h.local = Rc::new(Local {
            providers: l.providers.clone(),
            ctx: l.ctx.clone(),
            plugins: l.plugins.clone(),
            cwd: l.cwd.clone(),
            profile_home: profile_dir.to_path_buf(),
            push: l.push.clone(),
        });
        TestHost { host: h, _dirs: [auth_dir, profile_dir] }
    }

    /// An SSH ed25519 identity from a seed: its private key text and its public key line.
    fn identity(seed: u8) -> (String, String) {
        use ssh_key::private::Ed25519Keypair;
        let key = ssh_key::PrivateKey::from(Ed25519Keypair::from_seed(&[seed; 32]));
        (key.to_openssh(ssh_key::LineEnding::LF).unwrap().to_string(), key.public_key().to_openssh().unwrap())
    }

    /// State calls share the process's unlocked state and the wanted flag: one test at a time.
    fn state_serial() -> std::sync::MutexGuard<'static, ()> {
        static S: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let g = S.lock().unwrap_or_else(|e| e.into_inner());
        mu_common::sealed::lock();
        crate::host::state::set_wanted(false);
        crate::host::state::reset_failures();
        g
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
        use base64::Engine;
        let _g = state_serial();
        let h = host();
        seed(&h);
        crate::host::state::set_wanted(true);
        let (private, public) = identity(7);
        let dk_like = base64::engine::general_purpose::STANDARD.encode([9u8; 32]);
        // Calls that need arguments to say anything about a credential; every other call is made with `null`.
        let with_args = [
            ("key_target", json!("anthropic")),
            ("custom_get", json!("lab")),
            ("custom_get", json!("missing")),
            ("custom_env", json!("ANTHROPIC_API_KEY")),
            ("remove_key", json!("nothing-stored")),
            ("result_diff", json!({"cwd": "/", "ref": "refs/mu/result/none", "base": "0".repeat(40)})),
            ("result_apply", json!({"cwd": "/", "ref": "refs/mu/result/none", "base": "0".repeat(40)})),
            ("state_init", json!({"recipients": [public]})),
            ("state_unlock", json!({"dk": dk_like})),
            ("state_add_recipient", json!({"recipient": public})),
            ("state_remove_recipient", json!({"fingerprint": "SHA256:none"})),
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
                    for secret in SECRETS.iter().chain(&[dk_like.as_str(), private.as_str()]) {
                        assert!(!text.contains(secret), "{name} replied with a stored secret: {text}");
                    }
                    walked += 1;
                }
            }
        }
        assert!(walked > 20, "only {walked} replies were read");
        mu_common::sealed::lock();
        crate::host::state::set_wanted(false);
    }

    fn status(h: &LocalHost) -> crate::host::state::StateStatus {
        serde_json::from_value(ready(h, "state_status", Value::Null).unwrap()).unwrap()
    }

    #[test]
    fn a_state_key_is_set_up_locked_and_unlocked_through_the_host_without_a_reply_carrying_a_key() {
        use base64::Engine;
        use mu_common::sealed::{self, Kind, Wrapped};
        let _g = state_serial();
        let h = host();
        let home = h.local.profile_home.clone();
        let (private, public) = identity(1);
        let s = status(&h);
        assert!(!s.enabled && !s.initialised && !s.locked && s.kind.is_none());
        assert!(ready(&h, "state_init", json!({"recipients": [public]})).is_err(), "not turned on");

        crate::host::state::set_wanted(true);
        assert!(status(&h).needs_init());
        assert!(ready(&h, "state_wrapped", Value::Null).is_err(), "nothing to unwrap yet");
        let mut replies = vec![ready(&h, "state_init", json!({"recipients": [public]})).unwrap()];
        assert_eq!(
            status(&h),
            crate::host::state::StateStatus {
                enabled: true,
                initialised: true,
                locked: false,
                kind: Some(Kind::Recipients),
            }
        );
        assert!(ready(&h, "state_init", json!({"recipients": [public]})).is_err(), "only once");
        assert!(sealed::is_initialised(&home));

        sealed::lock();
        let s = status(&h);
        assert!(s.locked && !s.needs_init());
        let wrapped = ready(&h, "state_wrapped", Value::Null).unwrap();
        replies.push(wrapped.clone());
        let w: Wrapped = serde_json::from_value(wrapped).unwrap();
        let dk = sealed::unwrap_with_identity(&w, &private, None).unwrap();
        let dk_text = base64::engine::general_purpose::STANDARD.encode(dk.as_bytes());
        replies.push(ready(&h, "state_unlock", json!({"dk": dk_text})).unwrap());
        assert!(!status(&h).locked && sealed::is_unlocked());
        assert!(ready(&h, "state_wrapped", Value::Null).is_err(), "only while locked");

        let (_, other) = identity(2);
        replies.push(ready(&h, "state_add_recipient", json!({"recipient": other})).unwrap());
        assert_eq!(replies.last().unwrap().as_array().unwrap().len(), 2);
        let fp = replies.last().unwrap()[1]["fingerprint"].as_str().unwrap().to_string();
        replies.push(ready(&h, "state_remove_recipient", json!({"fingerprint": fp})).unwrap());
        assert_eq!(replies.last().unwrap().as_array().unwrap().len(), 1);
        replies.push(status_value(&h));
        let text = format!("{replies:?}");
        assert!(!text.contains(&dk_text) && !text.contains(&private), "a reply carried a key: {text}");

        sealed::lock();
        assert!(
            ready(&h, "state_add_recipient", json!({"recipient": other})).is_err(),
            "a locked agent changes nothing"
        );
        crate::host::state::set_wanted(false);
    }

    fn status_value(h: &LocalHost) -> Value {
        ready(h, "state_status", Value::Null).unwrap()
    }

    #[test]
    fn a_wrong_key_is_refused_and_only_a_few_are_tried_a_minute() {
        use base64::Engine;
        let _g = state_serial();
        let h = host();
        let (_, public) = identity(3);
        crate::host::state::set_wanted(true);
        ready(&h, "state_init", json!({"recipients": [public]})).unwrap();
        mu_common::sealed::lock();
        let wrong = json!({"dk": base64::engine::general_purpose::STANDARD.encode([5u8; 32])});
        for _ in 0..5 {
            let e = ready(&h, "state_unlock", wrong.clone()).unwrap_err();
            assert!(e.contains("wrong key"), "{e}");
        }
        let e = ready(&h, "state_unlock", wrong).unwrap_err();
        assert!(e.contains("too many"), "{e}");
        assert!(ready(&h, "state_unlock", json!({"dk": "not base64!"})).is_err());
        assert!(status(&h).locked);
        crate::host::state::set_wanted(false);
    }

    #[test]
    fn a_result_is_diffed_and_applied_through_the_host_and_only_by_its_ref() {
        use crate::host::results::{Applied, RESULT_APPLY, RESULT_DIFF, ResultArgs};
        use mu_testutil::{repo, sh};
        let root = repo();
        let base = sh(&root, &["rev-parse", "HEAD"]).trim().to_string();
        std::fs::write(root.join("a.txt"), "two\n").unwrap();
        sh(&root, &["commit", "-qam", "work"]);
        sh(&root, &["update-ref", "refs/mu/result/h-1", "HEAD"]);
        sh(&root, &["reset", "-q", "--hard", &base]);
        let h = host();
        let args = |r: &str| json!(ResultArgs { cwd: root.to_path_buf(), r#ref: r.into(), base: base.clone() });
        let wait = |a: Answer| match a {
            Answer::Pending(f) => mu_testutil::block_on(f),
            Answer::Ready(r) => r,
        };
        let diff = wait(h.call(RESULT_DIFF, args("refs/mu/result/h-1"))).unwrap();
        assert!(diff.as_str().is_some_and(|d| d.contains("+two")), "{diff}");
        assert!(wait(h.call(RESULT_DIFF, args("refs/heads/master"))).is_err(), "only a result ref is served");
        let done = wait(h.call(RESULT_APPLY, args("refs/mu/result/h-1"))).unwrap();
        let done: Applied = serde_json::from_value(done).unwrap();
        assert_eq!(done, Applied { files: 1, conflicts: Vec::new() });
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "two\n");
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
