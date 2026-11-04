//! `/login` for API keys and `/logout`: the key picker, the masked input a key is typed into, the
//! check against the provider, and the stored entry. Typed keys never reach the agent, so they stay
//! out of the transcript, the prompt history, the session file and predict's corpus.

use super::*;
use crate::tui::overlay::KeyOption;
use mu_base::models::{self, ProviderDef};
use mu_provider::auth::store::Entry;
use mu_provider::provider::native::{self, Check};

/// What the composer holds while a key flow waits for text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Typing {
    /// A key, drawn as bullets.
    Key,
    /// A `!command` that prints the key, drawn as typed (no history, no prediction).
    Command,
}

/// An API-key flow from `/login` to the saved entry.
pub(super) struct KeyFlow {
    /// Tells a late check result from the current flow's.
    pub(super) id: u64,
    pub(super) def: &'static ProviderDef,
    pub(super) typing: Option<Typing>,
    /// The entry a check could not confirm, waiting for "save anyway".
    pub(super) pending: Option<Entry>,
    /// A check is in flight.
    pub(super) checking: bool,
}

fn env_is_set(name: &str) -> bool {
    std::env::var(name).is_ok_and(|v| !v.trim().is_empty())
}

/// The status of a provider's key in `/login`.
fn key_status(stored: bool, env_set: bool, env_name: &str) -> String {
    if stored {
        "key saved".into()
    } else if env_set {
        format!("${env_name} is set")
    } else {
        "not set".into()
    }
}

/// The API-key rows of `/login`, one per provider, with the target each opens.
fn api_key_items(stored: impl Fn(&str) -> bool, env_set: impl Fn(&str) -> bool) -> (Vec<Item>, Vec<LoginTarget>) {
    models::PROVIDERS
        .iter()
        .map(|d| {
            let detail = key_status(stored(d.id), env_set(d.env_hint), d.env_hint);
            (Item { label: format!("API key: {}", d.label), detail }, LoginTarget::ApiKey(d.id.into()))
        })
        .unzip()
}

impl App {
    /// The API-key rows that open `/login`.
    pub(super) fn api_key_rows(&self) -> (Vec<Item>, Vec<LoginTarget>) {
        let store = self.providers.store();
        api_key_items(|p| store.has(p), env_is_set)
    }

    /// Says where to get a key, then offers the ways to give one.
    pub(super) fn start_key_flow(&mut self, def: &'static ProviderDef) {
        self.key_flow_seq += 1;
        self.key_flow = Some(KeyFlow { id: self.key_flow_seq, def, typing: None, pending: None, checking: false });
        self.notice(&format!("{} API keys: create one at {}", def.label, def.key_url));
        let mut items = vec![Item { label: "Paste a key".into(), detail: "shown as bullets".into() }];
        let mut options = vec![KeyOption::Paste];
        if env_is_set(def.env_hint) {
            items.push(Item { label: format!("Use ${}", def.env_hint), detail: "already set".into() });
            options.push(KeyOption::Env(def.env_hint.into()));
        }
        items.push(Item { label: "Use a command".into(), detail: "a command that prints the key".into() });
        options.push(KeyOption::Command);
        self.overlay = Some(Overlay::KeyChoice {
            sel: Select::new(&format!("{} API key", def.label), items),
            provider: def.id.into(),
            options,
        });
    }

    /// What the picker chose for `provider`, or `Cancel` when it was dismissed.
    pub(super) fn on_key_choice(&mut self, provider: &str, choice: KeyOption) {
        let Some(def) = models::native_def(provider) else { return };
        match choice {
            KeyOption::Paste => {
                self.set_typing(Some(Typing::Key));
                self.composer.set_masked(true);
                self.notice(&format!("paste the {} key and press Enter (Esc cancels)", def.label));
            }
            KeyOption::Command => {
                self.set_typing(Some(Typing::Command));
                self.composer.set_private(true);
                self.composer.set_closed(true);
                self.notice("type the command that prints the key and press Enter (Esc cancels)");
            }
            KeyOption::Env(name) => match Entry::reference(&format!("${name}")) {
                Ok(entry) => self.check_key(def, entry),
                Err(e) => self.error(&e.to_string()),
            },
            KeyOption::SaveAnyway => match self.key_flow.as_mut().and_then(|f| f.pending.take()) {
                Some(entry) => self.save_key(def, entry),
                None => self.cancel_key_flow(),
            },
            KeyOption::Cancel => self.cancel_key_flow(),
        }
    }

    /// Keys the masked or command input takes before the composer. True when the key was handled.
    pub(super) fn key_flow_key(&mut self, k: KeyEvent) -> bool {
        let ctrl_c = k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL);
        if k.code == KeyCode::Esc || ctrl_c {
            self.cancel_key_flow();
            return true;
        }
        if k.code == KeyCode::Enter && !k.modifiers.intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) {
            self.finish_typing();
            return true;
        }
        false
    }

    /// Enter in the masked or command input: the text becomes an entry and is checked.
    fn finish_typing(&mut self) {
        let Some(f) = self.key_flow.as_ref() else { return };
        let (def, typing) = (f.def, f.typing);
        let text = self.composer.secret_text().trim().to_string();
        if text.is_empty() {
            return;
        }
        let entry = match typing {
            Some(Typing::Command) => Entry::reference(&if text.starts_with('!') { text } else { format!("!{text}") }),
            _ => Ok(Entry::literal(&text)),
        };
        match entry {
            Ok(entry) => {
                self.end_typing();
                self.check_key(def, entry);
            }
            Err(e) => self.error(&e.to_string()),
        }
    }

    /// Clears the typed text, gives the composer its normal behaviour back, and restores what was
    /// held back while the text was typed.
    fn end_typing(&mut self) {
        self.composer.take_secret();
        self.composer.set_masked(false);
        self.composer.set_private(false);
        self.composer.set_closed(false);
        self.set_typing(None);
        let held = std::mem::take(&mut self.held_back);
        self.restore_queued(held);
    }

    /// Puts queued inputs back into the composer, or holds them until the typed text is done.
    pub(super) fn restore_queued(&mut self, items: Vec<UserInput>) {
        if items.is_empty() {
            return;
        }
        if let Err(back) = self.composer.restore(items) {
            self.held_back.extend(back);
        }
    }

    fn set_typing(&mut self, t: Option<Typing>) {
        if let Some(f) = self.key_flow.as_mut() {
            f.typing = t;
        }
    }

    /// Whether a key or command is being typed: the composer is locked to it.
    pub(super) fn typing(&self) -> bool {
        self.key_flow.as_ref().is_some_and(|f| f.typing.is_some())
    }

    /// A key is being typed or a choice waits for the user: no prediction is asked for.
    pub(super) fn key_flow_pending(&self) -> bool {
        self.key_flow.as_ref().is_some_and(|f| f.typing.is_some() || f.pending.is_some())
    }

    fn cancel_key_flow(&mut self) {
        if self.key_flow.take().is_some_and(|f| f.typing.is_some()) {
            self.end_typing();
        }
        self.notice("cancelled; no key was saved");
    }

    /// Drops a flow whose overlay was replaced by another one, so it cannot hold the ghost off or
    /// take a late result. A flow that is typing or checking is left alone.
    pub(super) fn sync_key_flow(&mut self) {
        let owned = matches!(self.overlay, Some(Overlay::KeyChoice { .. }));
        if self.key_flow.as_ref().is_some_and(|f| f.typing.is_none() && !f.checking) && !owned {
            self.key_flow = None;
        }
    }

    /// Resolves the entry and asks the provider whether it works. Nothing is saved yet.
    fn check_key(&mut self, def: &'static ProviderDef, entry: Entry) {
        let Some(f) = self.key_flow.as_mut() else { return };
        f.checking = true;
        let flow = f.id;
        self.notice(&format!("checking the key with {}…", def.label));
        let tx = self.ui_tx.clone();
        tokio::task::spawn_local(async move {
            let r = match mu_provider::auth::store::resolve_entry(def.id, &entry).await {
                Ok(resolved) => Ok(native::validate(def, &resolved.key, None).await),
                Err(e) => Err(e.to_string()),
            };
            let _ = tx.send(UiMsg::KeyChecked { flow, def, entry, r });
        });
    }

    pub(super) fn on_key_checked(
        &mut self,
        flow: u64,
        def: &'static ProviderDef,
        entry: Entry,
        r: Result<Check, String>,
    ) {
        match self.key_flow.as_mut() {
            Some(f) if f.id == flow => f.checking = false,
            // The flow was cancelled or replaced while the check ran.
            _ => return,
        }
        match r {
            Ok(Check::Valid) => self.save_key(def, entry),
            Ok(Check::Rejected(m)) => {
                self.key_flow = None;
                self.error(&format!("{m}. Nothing was saved; /login to try another key"));
            }
            Ok(Check::Unreachable(m)) => {
                self.notice(&format!("{m}; the key is not confirmed"));
                if let Some(f) = self.key_flow.as_mut() {
                    f.pending = Some(entry);
                }
                let items = vec![
                    Item { label: "Save anyway".into(), detail: "use the key without checking it".into() },
                    Item { label: "Cancel".into(), detail: "nothing is saved".into() },
                ];
                self.overlay = Some(Overlay::KeyChoice {
                    sel: Select::new(&format!("{} did not answer", def.label), items),
                    provider: def.id.into(),
                    options: vec![KeyOption::SaveAnyway, KeyOption::Cancel],
                });
            }
            Err(e) => {
                self.key_flow = None;
                self.error(&e);
            }
        }
    }

    fn save_key(&mut self, def: &'static ProviderDef, entry: Entry) {
        self.key_flow = None;
        match self.providers.store().set(def.id, entry) {
            Ok(()) => {
                self.notice(&native::saved_notice(def));
                self.send(Command::RefreshModel);
            }
            Err(e) => self.error(&format!("could not save the {} key: {e}", def.label)),
        }
    }

    /// `/logout [provider]`: removes the given key, or picks one of the stored keys.
    pub(super) fn logout_cmd(&mut self, arg: &str) {
        if !arg.is_empty() {
            return self.logout(arg);
        }
        let providers: Vec<String> = self
            .providers
            .store()
            .list()
            .into_iter()
            .map(|(p, _)| p)
            .filter(|p| models::native_def(p).is_some())
            .collect();
        if providers.is_empty() {
            return self.notice("no API keys are stored; /login adds one");
        }
        let items = providers
            .iter()
            .map(|p| Item {
                label: models::native_def(p).map_or_else(|| p.clone(), |d| d.label.to_string()),
                detail: "remove this key".into(),
            })
            .collect();
        self.overlay = Some(Overlay::Logout { sel: Select::new("Remove an API key", items), providers });
    }

    pub(super) fn logout(&mut self, provider: &str) {
        let Some(def) = models::native_def(provider) else {
            return self.error(&format!("no API key support for `{provider}`"));
        };
        match self.providers.store().remove(def.id) {
            Ok(true) => {
                self.notice(&format!("removed the {} key; {}/* models go through omp again", def.label, def.id));
                self.send(Command::RefreshModel);
            }
            Ok(false) => self.notice(&format!("no {} key is stored", def.label)),
            Err(e) => self.error(&format!("could not remove the {} key: {e}", def.label)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_rows_show_each_status() {
        let (items, targets) = api_key_items(|p| p == "anthropic", |n| n == "OPENAI_API_KEY");
        let details: Vec<&str> = items.iter().map(|i| i.detail.as_str()).collect();
        assert_eq!(details, ["key saved", "$OPENAI_API_KEY is set", "not set"]);
        assert_eq!(targets[2], LoginTarget::ApiKey("baseten".into()));
    }

    #[test]
    fn escape_in_the_key_picker_cancels() {
        let items = vec![Item { label: "Paste a key".into(), detail: String::new() }];
        let ov = Overlay::KeyChoice {
            sel: Select::new("t", items),
            provider: "anthropic".into(),
            options: vec![KeyOption::Paste],
        };
        let (rest, ev) = ov.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(rest.is_none());
        assert!(matches!(ev, Some(OverlayEvent::Key { choice: KeyOption::Cancel, .. })));
    }

    fn test_app(history: std::path::PathBuf) -> App {
        use mu_base::config::Configs;
        let (cmd_tx, _cmd_rx) = unbounded_channel();
        let (ui_tx, _ui_rx) = unbounded_channel();
        let (ntx, _nrx) = unbounded_channel();
        let plugins = mu_tools::plugin::PluginHost::new(vec![], vec![], ntx, Default::default());
        let gateway = mu_provider::provider::endpoint::GatewaySource::new(plugins.gateway_start());
        App {
            tr: Transcript::new(true, false),
            cmd_tx,
            inbox: mu_base::events::new_inbox(),
            providers: Rc::new(Providers::new(Rc::new(Configs::default()), gateway)),
            ctx: Rc::new(Context::default()),
            plugins,
            ui_tx,
            cwd: PathBuf::from("/"),
            status_cfg: Default::default(),
            bell: Default::default(),
            notify: false,
            turn_started: None,
            pick_assign: false,
            pick_deliver: false,
            renderer: Renderer::new(),
            composer: Composer::new(Some(history)),
            commands: vec![],
            tools: vec![],
            jobs: vec![],
            jobs_committed: HashSet::new(),
            job_out: HashMap::new(),
            ghost_seq: 0,
            ghost_asked: String::new(),
            bash_cmds: vec![],
            compacting: false,
            todo: vec![],
            status: Default::default(),
            overlay: None,
            pty: None,
            running: false,
            quitting: false,
            calls: vec![],
            expanded: false,
            dirty: false,
            child_busy: false,
            last_draw: Instant::now(),
            started: Instant::now(),
            mcp_wizard: None,
            key_flow: None,
            key_flow_seq: 0,
            held_back: vec![],
        }
    }

    fn press(code: KeyCode, mods: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new(code, mods))
    }

    fn transcript_text(app: &App) -> String {
        app.tr.pending.iter().map(crate::tui::line::line_text).collect::<Vec<_>>().join("\n")
    }

    /// Runs `f` on a current-thread runtime inside a `LocalSet`, as `spawn_local` needs.
    fn in_local(f: impl std::future::Future<Output = ()>) {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        tokio::task::LocalSet::new().block_on(&rt, f);
    }

    #[test]
    fn typed_key_reaches_no_overlay_history_transcript_or_prediction() {
        let hist = std::env::temp_dir().join(format!("mu-keyflow-{}", uuid::Uuid::new_v4())).join("history.jsonl");
        let mut app = test_app(hist.clone());
        in_local(async {
            app.start_key_flow(models::native_def("anthropic").unwrap());
            app.on_key_choice("anthropic", KeyOption::Paste);
            app.overlay = None;
            for c in "sk-ab".chars() {
                app.on_terminal(press(KeyCode::Char(c), KeyModifiers::NONE));
            }
            app.on_terminal(press(KeyCode::Char('j'), KeyModifiers::ALT));
            app.on_terminal(press(KeyCode::Char('m'), KeyModifiers::ALT));
            app.on_terminal(press(KeyCode::Char('r'), KeyModifiers::CONTROL));
            app.on_terminal(press(KeyCode::Char('g'), KeyModifiers::CONTROL));
            app.on_terminal(press(KeyCode::Char('v'), KeyModifiers::CONTROL));
            app.on_terminal(press(KeyCode::Up, KeyModifiers::ALT));
            app.on_terminal(press(KeyCode::Char('q'), KeyModifiers::CONTROL));
            app.on_terminal(press(KeyCode::Enter, KeyModifiers::ALT));
            assert!(app.overlay.is_none(), "no overlay opens while a key is typed");
            assert_eq!(app.composer.text(), "sk-ab");
            assert_eq!(app.ghost_seq, 0, "no prediction is asked for");

            // A paste reaches the masked input even with an overlay open.
            app.overlay = Some(Overlay::Logout { sel: Select::new("t", vec![]), providers: vec![] });
            app.on_terminal(Event::Paste("PASTED".into()));
            assert!(app.overlay.is_some());
            assert_eq!(app.composer.text(), "sk-abPASTED");
            assert_eq!(app.ghost_seq, 0);

            // Text held back while typing comes back once the key flow ends.
            app.restore_queued(vec![UserInput::from("queued")]);
            assert_eq!(app.composer.text(), "sk-abPASTED", "the key is untouched");
            app.on_terminal(press(KeyCode::Esc, KeyModifiers::NONE));
            assert!(app.key_flow.is_none());
            assert_eq!(app.composer.text(), "queued");
        });
        let shown = transcript_text(&app);
        assert!(!shown.contains("sk-ab") && !shown.contains("PASTED"), "{shown}");
        assert!(!hist.exists(), "nothing typed is written to history");
    }

    #[test]
    fn a_late_check_result_of_an_old_flow_is_ignored() {
        let mut app = test_app(std::env::temp_dir().join("mu-unused-history.jsonl"));
        let def = models::native_def("anthropic").unwrap();
        app.start_key_flow(def);
        app.key_flow = None;
        app.start_key_flow(def);
        app.on_key_checked(1, def, Entry::literal("sk-x"), Ok(Check::Valid));
        assert_eq!(app.key_flow.as_ref().map(|f| f.id), Some(2), "the current flow survives");
    }

    #[test]
    fn a_flow_whose_overlay_was_replaced_is_dropped() {
        let mut app = test_app(std::env::temp_dir().join("mu-unused-history.jsonl"));
        app.start_key_flow(models::native_def("anthropic").unwrap());
        app.sync_key_flow();
        assert!(app.key_flow.is_some(), "the key choice is still open");
        app.overlay = Some(Overlay::Logout { sel: Select::new("t", vec![]), providers: vec![] });
        app.sync_key_flow();
        assert!(app.key_flow.is_none());
    }

    #[test]
    fn login_with_a_native_provider_opens_its_key_flow() {
        let mut app = test_app(std::env::temp_dir().join("mu-unused-history.jsonl"));
        app.login("anthropic");
        assert!(matches!(&app.overlay, Some(Overlay::KeyChoice { provider, .. }) if provider == "anthropic"));
        assert!(app.key_flow.is_some());
    }

    #[test]
    fn logout_of_an_unknown_provider_says_so() {
        let mut app = test_app(std::env::temp_dir().join("mu-unused-history.jsonl"));
        app.logout_cmd("nope");
        assert!(transcript_text(&app).contains("no API key support for `nope`"));
        assert!(app.overlay.is_none());
    }

    #[test]
    fn logout_without_argument_lists_only_stored_keys() {
        let mut app = test_app(std::env::temp_dir().join("mu-unused-history.jsonl"));
        app.logout_cmd("");
        let stored: Vec<String> = app.providers.store().list().into_iter().map(|(p, _)| p).collect();
        if stored.is_empty() {
            assert!(transcript_text(&app).contains("no API keys are stored"));
        } else {
            assert!(matches!(&app.overlay, Some(Overlay::Logout { providers, .. }) if !providers.is_empty()));
        }
    }
}
