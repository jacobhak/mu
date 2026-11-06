//! Input routing and API keys. A key goes to the open pty, the overlay, the MCP wizard or the
//! composer (`on_terminal`); `/login` and `/logout` handle API keys. Typed keys never reach the
//! agent, so they stay out of the transcript, the prompt history, the session file and predict's
//! corpus.

use super::*;
use crate::chat::composer::Action;
use crate::term::clipboard;
use crate::widgets::overlay::{KeyOption, LoginTarget};
use crate::widgets::select::{Item, Select};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use mu_base::models::Thinking;
use mu_provider::auth::check::{self, Check};
use mu_provider::auth::store::Entry;
use mu_provider::provider::providers::{self, KeyTarget, PROVIDERS, SEARCH_KEYS};

/// What the composer holds while a key flow waits for text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Typing {
    /// A key, drawn as bullets.
    Key,
    /// A `!command` that prints the key, drawn as typed (no history, no prediction).
    Command,
}

/// What a key flow resumes when it ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum KeyThen {
    WebWizard,
}

/// An API-key flow from `/login` to the saved entry.
pub(super) struct KeyFlow {
    /// Tells a late check result from the current flow's.
    pub(super) id: u64,
    pub(super) target: KeyTarget,
    /// Whether the provider's chat models use the key, when already decided (anthropic and openai ask).
    pub(super) chat: Option<bool>,
    pub(super) then: Option<KeyThen>,
    pub(super) typing: Option<Typing>,
    /// The entry a check could not confirm, waiting for "save anyway".
    pub(super) pending: Option<Entry>,
    /// A check is in flight.
    pub(super) checking: bool,
}

/// Whether `name` is set to a value other than blanks.
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

/// The API-key rows of `/login`: the chat providers, then the search services, with the target each opens.
fn api_key_items(
    stored: impl Fn(&str) -> bool,
    env_set: impl Fn(&str) -> bool,
    routes_chat: impl Fn(&str) -> bool,
) -> (Vec<Item>, Vec<LoginTarget>) {
    let providers = PROVIDERS.iter().map(|d| {
        let mut detail = key_status(stored(d.id), env_set(d.env_hint), d.env_hint);
        if stored(d.id) {
            detail.push_str(if routes_chat(d.id) { " · chat" } else { " · not for chat" });
        }
        (Item { label: format!("API key: {}", d.label), detail }, LoginTarget::ApiKey(d.id.into()))
    });
    let search = SEARCH_KEYS.iter().map(|d| {
        let detail = key_status(stored(d.id), env_set(d.env_hint), d.env_hint);
        (Item { label: format!("API key: {} (web search)", d.label), detail }, LoginTarget::ApiKey(d.id.into()))
    });
    providers.chain(search).unzip()
}

/// The two answers to "does chat use this key?"; `current` marks the stored choice.
fn chat_items(target: KeyTarget, current: Option<bool>) -> Vec<Item> {
    let id = target.id();
    let row = |label: String, detail: &str, on: bool| Item {
        label,
        detail: if current == Some(on) { "current".into() } else { detail.into() },
    };
    vec![
        row(format!("Yes: {id}/* chat models use this key"), "billed per token instead of your omp login", true),
        row("No: web search and other chosen uses only".into(), &format!("{id}/* chat keeps going through omp"), false),
    ]
}

impl App {
    /// The events of the terminal: the pty, paste, resize and keys.
    pub(super) fn on_terminal(&mut self, e: Event) {
        self.screen.dirty = true;
        match e {
            Event::Paste(s) if self.agent.pty.is_some() => {
                if let Some(p) = &self.agent.pty {
                    p.paste(&s);
                }
            }
            Event::Resize(..) if self.agent.pty.is_some() => {
                let (w, h) = self.screen.renderer.size();
                if let Some(p) = self.agent.pty.as_mut() {
                    p.resize(w, h);
                }
            }
            Event::Key(k) => self.on_key(k),
            Event::Paste(s) => {
                // A key being typed takes the paste even when an overlay is open.
                let typing = self.typing();
                match self.overlay.as_mut() {
                    Some(ov) if !typing => ov.on_paste(&s),
                    _ => {
                        if let Err(e) = self.prompt.composer.on_paste(&s, &self.prompt.commands) {
                            self.error(&format!("image: {e}"));
                        }
                        self.request_ghost();
                    }
                }
            }
            _ => {}
        }
    }

    /// Asks the predict plugin for a ghost when the composer wants one. Failures are silent.
    fn request_ghost(&mut self) {
        let cfg = &self.services.providers.cfg.mu.predict;
        if !cfg.enabled
            || self.prompt.composer.has_ghost()
            || self.flows.mcp_wizard.is_some()
            || self.key_flow_pending()
        {
            return;
        }
        let Some(q) = self.prompt.composer.predict_query() else { return };
        if q == self.prompt.ghost_asked {
            return;
        }
        self.prompt.ghost_seq += 1;
        self.prompt.ghost_asked = q.clone();
        let seq = self.prompt.ghost_seq;
        let assistant: String = {
            let r = self.agent.tr.replies.last().map(String::as_str).unwrap_or_default();
            let skip = r.chars().count().saturating_sub(4000);
            r.chars().skip(skip).collect()
        };
        let params = json!({ "text": q, "cwd": self.cwd, "assistant": assistant, "threshold": cfg.threshold });
        let (plugins, tx) = (self.plugins.clone(), self.ui_tx.clone());
        tokio::task::spawn_local(async move {
            let ghost = plugins
                .request("predict", "predict/complete", params, Duration::from_millis(300))
                .await
                .ok()
                .and_then(|v| v["ghost"].as_str().map(String::from))
                .unwrap_or_default();
            let _ = tx.send(UiMsg::Ghost { seq, text: q, ghost });
        });
    }

    fn on_key(&mut self, k: KeyEvent) {
        if let Some(p) = &self.agent.pty {
            p.key(k);
            return;
        }
        self.on_key_inner(k);
        if self.overlay.is_none() {
            self.request_ghost();
        }
    }

    fn on_key_inner(&mut self, k: KeyEvent) {
        // While a key is typed, every key goes to the masked composer, even past an open overlay.
        let typing = self.typing();
        if typing {
            if self.key_flow_key(k) {
                return;
            }
        } else {
            if self.overlay.is_some() {
                return self.overlay_key(k);
            }
            if self.flows.mcp_wizard.is_some() && self.mcp_wizard_key(k) {
                return;
            }
        }
        let action = self.prompt.composer.on_key(k, self.agent.running, &self.prompt.commands);
        // Only actions that stay in the composer may run while a key or command is typed.
        if typing
            && !matches!(action, Action::None | Action::Redraw | Action::Abort | Action::Exit | Action::ToggleExpanded)
        {
            return;
        }
        match action {
            Action::None | Action::Redraw => {}
            Action::Reply => self.open_reply(),
            Action::Jobs => self.open_jobs(),
            Action::Submit(t) => self.submit(t, false),
            Action::QueueFollowUp(t) => self.submit(t, true),
            Action::Abort => self.send(Command::Cancel),
            Action::Exit => self.quit(),
            Action::CycleThinking => {
                let cur = Thinking::parse(&self.agent.status.thinking).unwrap_or(Thinking::Medium);
                self.send(Command::SetThinking(cur.cycle()));
            }
            Action::ToggleThinking => {
                let w = self.width();
                self.agent.tr.toggle_thinking(w);
            }
            Action::ToggleExpanded => self.agent.expanded = !self.agent.expanded,
            Action::ModelPicker => self.request_models(),
            Action::EditExternal => self.edit_external(),
            Action::PasteImage => {
                let tx = self.ui_tx.clone();
                tokio::task::spawn_local(async move {
                    let _ = tx.send(UiMsg::ClipboardImage(clipboard::read_image().await));
                });
            }
            Action::PopQueue => self.send(Command::PopQueue),
        }
    }

    /// `/login`: with a provider id run `omp login <id>`, with `mcp:<name>` log in to that MCP
    /// server; otherwise list omp's providers and the MCP servers that need auth in a picker.
    pub(super) fn login(&mut self, provider: &str) {
        if let Some(target) = KeyTarget::find(provider) {
            return self.begin_key(target);
        }
        if let Some(name) = provider.strip_prefix("mcp:") {
            return self.run_mcp_login(name);
        }
        if !provider.is_empty() {
            return self.run_login(&[provider], None);
        }
        self.notice("listing login providers…");
        let tx = self.ui_tx.clone();
        tokio::task::spawn_local(async move {
            let _ = tx.send(UiMsg::LoginProviders(list_login_providers().await));
        });
    }

    /// Keys the masked or command input takes before the composer. True when the key was handled.
    fn key_flow_key(&mut self, k: KeyEvent) -> bool {
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
        let Some(f) = self.flows.key_flow.as_ref() else { return };
        let (target, typing) = (f.target, f.typing);
        let text = self.prompt.composer.secret_text().trim().to_string();
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
                self.check_key(target, entry);
            }
            Err(e) => self.error(&e.to_string()),
        }
    }

    /// Clears the typed text, gives the composer its normal behaviour back, and restores what was
    /// held back while the text was typed.
    fn end_typing(&mut self) {
        self.prompt.composer.take_secret();
        self.prompt.composer.set_masked(false);
        self.prompt.composer.set_private(false);
        self.prompt.composer.set_closed(false);
        self.set_typing(None);
        let held = std::mem::take(&mut self.prompt.held_back);
        self.restore_queued(held);
    }

    /// Puts queued inputs back into the composer, or holds them until the typed text is done.
    pub(super) fn restore_queued(&mut self, items: Vec<UserInput>) {
        if items.is_empty() {
            return;
        }
        if let Err(back) = self.prompt.composer.restore(items) {
            self.prompt.held_back.extend(back);
        }
    }

    fn set_typing(&mut self, t: Option<Typing>) {
        if let Some(f) = self.flows.key_flow.as_mut() {
            f.typing = t;
        }
    }

    /// Whether a key or command is being typed: the composer is locked to it.
    pub(super) fn typing(&self) -> bool {
        self.flows.key_flow.as_ref().is_some_and(|f| f.typing.is_some())
    }

    /// A key is being typed or a choice waits for the user: no prediction is asked for.
    fn key_flow_pending(&self) -> bool {
        self.flows.key_flow.as_ref().is_some_and(|f| f.typing.is_some() || f.pending.is_some())
    }

    /// Ends the flow without saving. A flow that began elsewhere (the wizard) resumes there.
    fn cancel_key_flow(&mut self) {
        let flow = self.flows.key_flow.take();
        if flow.as_ref().is_some_and(|f| f.typing.is_some()) {
            self.end_typing();
        }
        self.notice(if flow.is_some() { "cancelled; no key was saved" } else { "cancelled" });
        self.key_flow_ended(flow.and_then(|f| f.then), false);
    }

    fn key_flow_ended(&mut self, then: Option<KeyThen>, saved: bool) {
        match then {
            Some(KeyThen::WebWizard) => self.web_wizard_key_done(saved),
            None => {}
        }
    }

    /// Drops a flow whose overlay was replaced by another one, so it cannot hold the ghost off or
    /// take a late result. A flow that is typing or checking is left alone.
    pub(super) fn sync_key_flow(&mut self) {
        let owned = matches!(self.overlay, Some(Overlay::KeyChoice { .. }));
        if self.flows.key_flow.as_ref().is_some_and(|f| f.typing.is_none() && !f.checking) && !owned {
            let then = self.flows.key_flow.take().and_then(|f| f.then);
            self.key_flow_ended(then, false);
        }
    }

    /// The API-key rows that open `/login`.
    pub(super) fn api_key_rows(&self) -> (Vec<Item>, Vec<LoginTarget>) {
        let store = self.services.providers.store();
        api_key_items(|p| store.has(p), env_is_set, |p| store.get(p).is_some_and(|e| e.routes_chat()))
    }

    /// `/login <id>` or a `/login` row: a key already stored for anthropic or openai opens a menu
    /// (chat use, replace); anything else starts the key flow.
    pub(super) fn begin_key(&mut self, target: KeyTarget) {
        let stored = self.services.providers.store().get(target.id());
        let Some(entry) = stored.filter(|_| target.asks_chat()) else {
            return self.start_key_flow(target, None, None);
        };
        let mut items = chat_items(target, Some(entry.routes_chat()));
        items.push(Item { label: "Replace the key".into(), detail: "paste, use a variable or a command".into() });
        items.push(Item { label: "Cancel".into(), detail: "nothing changes".into() });
        self.overlay = Some(Overlay::KeyChoice {
            sel: Select::new(&format!("{} API key", target.label()), items),
            provider: target.id().into(),
            options: vec![KeyOption::Chat(true), KeyOption::Chat(false), KeyOption::Replace, KeyOption::Cancel],
        });
    }

    /// Says where to get a key, then offers the ways to give one.
    /// `chat` presets the answer to "does chat use this key?", `then` resumes another flow at the end.
    pub(super) fn start_key_flow(&mut self, target: KeyTarget, chat: Option<bool>, then: Option<KeyThen>) {
        self.flows.key_flow_seq += 1;
        self.flows.key_flow = Some(KeyFlow {
            id: self.flows.key_flow_seq,
            target,
            chat,
            then,
            typing: None,
            pending: None,
            checking: false,
        });
        self.notice(&format!("{} API keys: create one at {}", target.label(), target.key_url()));
        let mut items = vec![Item { label: "Paste a key".into(), detail: "shown as bullets".into() }];
        let mut options = vec![KeyOption::Paste];
        if env_is_set(target.env_hint()) {
            items.push(Item { label: format!("Use ${}", target.env_hint()), detail: "already set".into() });
            options.push(KeyOption::Env(target.env_hint().into()));
        }
        items.push(Item { label: "Use a command".into(), detail: "a command that prints the key".into() });
        options.push(KeyOption::Command);
        self.overlay = Some(Overlay::KeyChoice {
            sel: Select::new(&format!("{} API key", target.label()), items),
            provider: target.id().into(),
            options,
        });
    }

    /// What the picker chose for `provider`, or `Cancel` when it was dismissed.
    pub(super) fn on_key_choice(&mut self, provider: &str, choice: KeyOption) {
        let Some(target) = KeyTarget::find(provider) else { return };
        match choice {
            KeyOption::Paste => {
                self.set_typing(Some(Typing::Key));
                self.prompt.composer.set_masked(true);
                self.notice(&format!("paste the {} key and press Enter (Esc cancels)", target.label()));
            }
            KeyOption::Command => {
                self.set_typing(Some(Typing::Command));
                self.prompt.composer.set_private(true);
                self.prompt.composer.set_closed(true);
                self.notice("type the command that prints the key and press Enter (Esc cancels)");
            }
            KeyOption::Env(name) => match Entry::reference(&format!("${name}")) {
                Ok(entry) => self.check_key(target, entry),
                Err(e) => self.error(&e.to_string()),
            },
            KeyOption::SaveAnyway => match self.flows.key_flow.as_mut().and_then(|f| f.pending.take()) {
                Some(entry) => self.confirm_key(target, entry),
                None => self.cancel_key_flow(),
            },
            KeyOption::Chat(on) => match self.flows.key_flow.as_mut().and_then(|f| f.pending.take()) {
                Some(entry) => self.save_key(target, Entry { chat: Some(on), ..entry }),
                None => match self.services.set_chat(target.id(), on) {
                    Ok(true) => {
                        let msg = if on {
                            format!(
                                "{id}/* models now use your API key (billed per token) instead of omp.",
                                id = target.id()
                            )
                        } else {
                            format!(
                                "{id}/* chat models go through omp again; the key stays for web search",
                                id = target.id()
                            )
                        };
                        self.notice(&msg);
                        self.send(Command::RefreshModel);
                    }
                    Ok(false) => self.notice(&format!("no {} key is stored", target.label())),
                    Err(e) => self.error(&format!("could not update the {} key: {e}", target.label())),
                },
            },
            KeyOption::Replace => self.start_key_flow(target, None, None),
            KeyOption::Cancel => self.cancel_key_flow(),
        }
    }

    /// Resolves the entry and asks the provider whether it works. Nothing is saved yet.
    fn check_key(&mut self, target: KeyTarget, entry: Entry) {
        let Some(f) = self.flows.key_flow.as_mut() else { return };
        f.checking = true;
        let flow = f.id;
        self.notice(&format!("checking the key with {}…", target.label()));
        let tx = self.ui_tx.clone();
        tokio::task::spawn_local(async move {
            let r = check::check_target(target, &entry).await;
            let _ = tx.send(UiMsg::KeyChecked { flow, target, entry, r });
        });
    }

    pub(super) fn on_key_checked(&mut self, flow: u64, target: KeyTarget, entry: Entry, r: Result<Check, String>) {
        match self.flows.key_flow.as_mut() {
            Some(f) if f.id == flow => f.checking = false,
            // The flow was cancelled or replaced while the check ran.
            _ => return,
        }
        match r {
            Ok(Check::Valid) => self.confirm_key(target, entry),
            Ok(Check::Unchecked) => {
                self.notice(&format!(
                    "saved without checking: {} has no free way to test a key; a bad key shows on the first search",
                    target.label()
                ));
                self.confirm_key(target, entry);
            }
            Ok(Check::Rejected(m)) => {
                self.end_key_flow(false);
                self.error(&format!("{m}. Nothing was saved; /login to try another key"));
            }
            Ok(Check::Unreachable(m)) => {
                self.notice(&format!("{m}; the key is not confirmed"));
                if let Some(f) = self.flows.key_flow.as_mut() {
                    f.pending = Some(entry);
                }
                let items = vec![
                    Item { label: "Save anyway".into(), detail: "use the key without checking it".into() },
                    Item { label: "Cancel".into(), detail: "nothing is saved".into() },
                ];
                self.overlay = Some(Overlay::KeyChoice {
                    sel: Select::new(&format!("{} did not answer", target.label()), items),
                    provider: target.id().into(),
                    options: vec![KeyOption::SaveAnyway, KeyOption::Cancel],
                });
            }
            Err(e) => {
                self.end_key_flow(false);
                self.error(&e);
            }
        }
    }

    fn end_key_flow(&mut self, saved: bool) {
        let then = self.flows.key_flow.take().and_then(|f| f.then);
        self.key_flow_ended(then, saved);
    }

    /// A key that works is saved, after asking whether chat uses it when the user has not said.
    fn confirm_key(&mut self, target: KeyTarget, entry: Entry) {
        let undecided = self.flows.key_flow.as_ref().is_some_and(|f| f.chat.is_none());
        if !(target.asks_chat() && undecided) {
            return self.save_key(target, entry);
        }
        if let Some(f) = self.flows.key_flow.as_mut() {
            f.pending = Some(entry);
        }
        self.overlay = Some(Overlay::KeyChoice {
            sel: Select::new(&format!("Use this {} key for chat models?", target.label()), chat_items(target, None)),
            provider: target.id().into(),
            options: vec![KeyOption::Chat(true), KeyOption::Chat(false)],
        });
    }

    fn save_key(&mut self, target: KeyTarget, mut entry: Entry) {
        let flow = self.flows.key_flow.take();
        let then = flow.as_ref().and_then(|f| f.then);
        entry.chat = match target {
            // A search-only key has no chat flag; Baseten's only use is chat.
            KeyTarget::Search(_) => None,
            KeyTarget::Provider(_) if !target.asks_chat() => Some(true),
            // Never left unset: an unset flag would route chat.
            KeyTarget::Provider(_) => Some(entry.chat.or(flow.and_then(|f| f.chat)).unwrap_or(false)),
        };
        let chat = entry.chat.unwrap_or(false);
        match self.services.save_key(target.id(), entry) {
            Ok(()) => {
                match target {
                    KeyTarget::Provider(def) => {
                        self.notice(&providers::saved_notice(def, chat));
                        self.send(Command::RefreshModel);
                    }
                    KeyTarget::Search(d) => self.notice(&format!("saved the {} key", d.label)),
                }
                self.key_flow_ended(then, true);
            }
            Err(e) => {
                self.error(&format!("could not save the {} key: {e}", target.label()));
                self.key_flow_ended(then, false);
            }
        }
    }

    /// `/logout [provider]`: removes the given key, or picks one of the stored keys.
    pub(super) fn logout_cmd(&mut self, arg: &str) {
        if !arg.is_empty() {
            return self.logout(arg);
        }
        let providers: Vec<String> = self
            .services
            .providers
            .store()
            .list()
            .into_iter()
            .map(|(p, _)| p)
            .filter(|p| KeyTarget::find(p).is_some())
            .collect();
        if providers.is_empty() {
            return self.notice("no API keys are stored; /login adds one");
        }
        let items = providers
            .iter()
            .map(|p| Item {
                label: KeyTarget::find(p).map_or_else(|| p.clone(), |t| t.label().to_string()),
                detail: "remove this key".into(),
            })
            .collect();
        self.overlay = Some(Overlay::Logout { sel: Select::new("Remove an API key", items), providers });
    }

    pub(super) fn logout(&mut self, provider: &str) {
        let Some(target) = KeyTarget::find(provider) else {
            return self.error(&format!("no API key support for `{provider}`"));
        };
        let was_chat = matches!(target, KeyTarget::Provider(_))
            && self.services.providers.store().get(target.id()).is_some_and(|e| e.routes_chat());
        match self.services.remove_key(target.id()) {
            Ok(true) if was_chat => {
                self.notice(&format!(
                    "removed the {} key; {}/* models go through omp again",
                    target.label(),
                    target.id()
                ));
                self.send(Command::RefreshModel);
            }
            Ok(true) => self.notice(&format!("removed the {} key", target.label())),
            Ok(false) => self.notice(&format!("no {} key is stored", target.label())),
            Err(e) => self.error(&format!("could not remove the {} key: {e}", target.label())),
        }
    }
}

/// omp's login menu names Baseten too; mu owns that provider's key, so the omp row is dropped.
pub(super) fn is_baseten(name: &str) -> bool {
    name.split(|c: char| !c.is_alphanumeric())
        .find(|w| !w.is_empty())
        .is_some_and(|w| w.eq_ignore_ascii_case("baseten"))
}

/// Provider menu of `omp login`, scraped from its numbered prompt (omp has no machine-readable list).
async fn list_login_providers() -> Result<Vec<(usize, String)>, String> {
    let out = tokio::process::Command::new(mu_common::paths::omp_bin())
        .arg("login")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .await
        .map_err(|e| e.to_string())?;
    Ok(parse_login_menu(&String::from_utf8_lossy(&out.stdout)))
}

fn parse_login_menu(text: &str) -> Vec<(usize, String)> {
    let mut plain = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for n in chars.by_ref() {
                if n.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            plain.push(c);
        }
    }
    plain
        .lines()
        .filter_map(|l| {
            let (n, name) = l.trim().split_once(". ")?;
            Some((n.parse().ok()?, name.trim().to_string())).filter(|(_, name): &(usize, String)| !name.is_empty())
        })
        .collect()
}

#[cfg(test)]
mod login_tests {
    use super::parse_login_menu;

    #[test]
    fn menu_numbers_and_names_survive_ansi_and_noise() {
        let t = "Select a provider:\n\n  1. ChatGPT Plus/Pro (Codex)\n\x1b[2m  12. Z.AI (GLM · Sign in)\x1b[0m\nEnter number: ";
        assert_eq!(
            parse_login_menu(t),
            vec![(1, "ChatGPT Plus/Pro (Codex)".into()), (12, "Z.AI (GLM · Sign in)".into())]
        );
    }

    #[test]
    fn baseten_is_the_only_omp_row_mu_drops() {
        use super::is_baseten;
        assert!(is_baseten("Baseten"));
        assert!(is_baseten(" Baseten (API key)"));
        assert!(!is_baseten("Basetenx"));
        assert!(!is_baseten("Z.AI (GLM · Sign in)"));
    }

    #[test]
    #[ignore]
    fn contract_omp_login_menu() {
        let out = mu_common::paths::run_omp_blocking(&["login"], &[]);
        assert!(!parse_login_menu(&out).is_empty(), "no providers parsed from: {out}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::Renderer;
    use crate::text::line::line_text;
    use crate::widgets::overlay::OverlayEvent;
    use mu_provider::provider::client::Providers;
    use mu_tools::context::Context;

    fn transcript_text(app: &App) -> String {
        app.agent.tr.pending.iter().map(line_text).collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn login_rows_show_each_status() {
        let (items, targets) = api_key_items(|p| p == "anthropic" || p == "exa", |n| n == "OPENAI_API_KEY", |_| false);
        let details: Vec<&str> = items.iter().map(|i| i.detail.as_str()).collect();
        assert_eq!(details, ["key saved · not for chat", "$OPENAI_API_KEY is set", "not set", "not set", "key saved"]);
        assert_eq!(targets[2], LoginTarget::ApiKey("baseten".into()));
        assert_eq!(targets[4], LoginTarget::ApiKey("exa".into()));
        assert_eq!(items[3].label, "API key: Parallel (web search)");
        let (items, _) = api_key_items(|p| p == "openai", |_| false, |_| true);
        assert_eq!(items[1].detail, "key saved · chat");
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
        let plugins = mu_tools::plugin::host::PluginHost::new(vec![], vec![], ntx, Default::default());
        let gateway = mu_provider::provider::endpoint::GatewaySource::new(plugins.gateway_start());
        App {
            agent: AgentView::new(true, false),
            services: Services { providers: Rc::new(Providers::for_tests(Rc::new(Configs::default()), gateway)) },
            link: Link { cmd_tx },
            ctx: Rc::new(Context::default()),
            plugins,
            ui_tx,
            cwd: PathBuf::from("/"),
            screen: Screen {
                renderer: Renderer::new(),
                last_draw: Instant::now(),
                dirty: false,
                child_busy: false,
                started: Instant::now(),
                status_cfg: Default::default(),
                notify: false,
            },
            prompt: Prompt {
                composer: Composer::new(Some(history)),
                commands: vec![],
                ghost_seq: 0,
                ghost_asked: String::new(),
                held_back: vec![],
            },
            overlay: None,
            flows: Flows {
                pick_assign: false,
                assign_role: None,
                pick_deliver: false,
                mcp_wizard: None,
                web_wizard: None,
                key_flow: None,
                key_flow_seq: 0,
            },
            quitting: false,
        }
    }

    fn press(code: KeyCode, mods: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new(code, mods))
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
            app.start_key_flow(KeyTarget::find("anthropic").unwrap(), None, None);
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
            assert_eq!(app.prompt.composer.text(), "sk-ab");
            assert_eq!(app.prompt.ghost_seq, 0, "no prediction is asked for");

            // A paste reaches the masked input even with an overlay open.
            app.overlay = Some(Overlay::Logout { sel: Select::new("t", vec![]), providers: vec![] });
            app.on_terminal(Event::Paste("PASTED".into()));
            assert!(app.overlay.is_some());
            assert_eq!(app.prompt.composer.text(), "sk-abPASTED");
            assert_eq!(app.prompt.ghost_seq, 0);

            // Text held back while typing comes back once the key flow ends.
            app.restore_queued(vec![UserInput::from("queued")]);
            assert_eq!(app.prompt.composer.text(), "sk-abPASTED", "the key is untouched");
            app.on_terminal(press(KeyCode::Esc, KeyModifiers::NONE));
            assert!(app.flows.key_flow.is_none());
            assert_eq!(app.prompt.composer.text(), "queued");
        });
        let shown = transcript_text(&app);
        assert!(!shown.contains("sk-ab") && !shown.contains("PASTED"), "{shown}");
        assert!(!hist.exists(), "nothing typed is written to history");
    }

    #[test]
    fn a_late_check_result_of_an_old_flow_is_ignored() {
        let mut app = test_app(std::env::temp_dir().join("mu-unused-history.jsonl"));
        let t = KeyTarget::find("anthropic").unwrap();
        app.start_key_flow(t, None, None);
        app.flows.key_flow = None;
        app.start_key_flow(t, None, None);
        app.on_key_checked(1, t, Entry::literal("sk-x"), Ok(Check::Valid));
        assert_eq!(app.flows.key_flow.as_ref().map(|f| f.id), Some(2), "the current flow survives");
    }

    #[test]
    fn a_valid_openai_key_asks_about_chat_and_saves_the_answer() {
        let mut app = test_app(std::env::temp_dir().join("mu-unused-history.jsonl"));
        let t = KeyTarget::find("openai").unwrap();
        app.start_key_flow(t, None, None);
        app.on_key_checked(1, t, Entry::literal("sk-test-0123456789"), Ok(Check::Valid));
        let Some(Overlay::KeyChoice { options, .. }) = &app.overlay else { panic!("no chat question") };
        assert_eq!(options, &[KeyOption::Chat(true), KeyOption::Chat(false)]);
        assert!(app.services.providers.store().get("openai").is_none(), "nothing is saved before the answer");
        app.on_key_choice("openai", KeyOption::Chat(false));
        let store = app.services.providers.store().clone();
        let saved = store.get("openai").unwrap();
        assert_eq!(saved.chat, Some(false));
        assert!(!app.services.providers.is_direct("openai"));
        // The existing-key menu changes the answer without a new key.
        app.begin_key(t);
        let Some(Overlay::KeyChoice { options, .. }) = &app.overlay else { panic!("no menu") };
        assert_eq!(options.len(), 4);
        app.on_key_choice("openai", KeyOption::Chat(true));
        assert!(app.services.providers.is_direct("openai"));
        store.remove("openai").unwrap();
    }

    #[test]
    fn a_search_key_is_saved_without_a_chat_question() {
        let mut app = test_app(std::env::temp_dir().join("mu-unused-history.jsonl"));
        let t = KeyTarget::find("exa").unwrap();
        app.start_key_flow(t, None, None);
        app.overlay = None;
        app.on_key_checked(1, t, Entry::literal("exa-test-0123456789"), Ok(Check::Unchecked));
        assert!(app.overlay.is_none(), "no chat question for a search key");
        let store = app.services.providers.store().clone();
        assert_eq!(store.get("exa").unwrap().chat, None);
        assert!(!app.services.providers.is_direct("exa"));
        store.remove("exa").unwrap();
    }

    #[test]
    fn the_web_wizard_answers_a_waiting_search_and_saves_the_choice() {
        let mut app = test_app(std::env::temp_dir().join("mu-unused-history.jsonl"));
        let store = app.services.providers.store().clone();
        // Choosing Off tells the waiting call "no" and stores the choice.
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        app.start_web_wizard(Some(tx));
        assert!(matches!(app.overlay, Some(Overlay::Web { .. })));
        app.on_web_pick(Some(6));
        assert_eq!(rx.try_recv(), Ok(false));
        assert_eq!(store.web(), Some(mu_provider::auth::web::WebSearch::off()));
        assert!(app.flows.web_wizard.is_none());
        store.set_web(None).unwrap();

        // Esc cancels, and a second wizard releases the first one's call.
        let (tx1, mut rx1) = tokio::sync::oneshot::channel();
        app.start_web_wizard(Some(tx1));
        let (tx2, mut rx2) = tokio::sync::oneshot::channel();
        app.start_web_wizard(Some(tx2));
        assert_eq!(rx1.try_recv(), Ok(false));
        app.on_web_pick(None);
        assert_eq!(rx2.try_recv(), Ok(false));
        assert_eq!(store.web(), None);
        assert!(transcript_text(&app).contains("web search was not set up"));
    }

    #[test]
    fn the_web_wizard_asks_for_a_missing_key_and_resumes_after_it() {
        let mut app = test_app(std::env::temp_dir().join("mu-unused-history.jsonl"));
        app.start_web_wizard(None);
        app.on_web_pick(Some(2));
        assert!(matches!(&app.overlay, Some(Overlay::Web { .. })), "keyless Exa asks for approval first");
        app.on_web_pick(Some(1));
        // Exa (API key) has no key in the isolated home: the key flow opens, for search only.
        app.on_web_pick(Some(3));
        let f = app.flows.key_flow.as_ref().expect("a key flow");
        assert_eq!((f.target.id(), f.chat, f.then), ("exa", Some(false), Some(KeyThen::WebWizard)));
        assert!(matches!(&app.overlay, Some(Overlay::KeyChoice { provider, .. }) if provider == "exa"));
        // Cancelling the key flow returns to the backend choice.
        app.on_key_choice("exa", KeyOption::Cancel);
        assert!(app.flows.key_flow.is_none());
        assert!(matches!(&app.overlay, Some(Overlay::Web { .. })));
    }

    #[test]
    fn a_flow_whose_overlay_was_replaced_is_dropped() {
        let mut app = test_app(std::env::temp_dir().join("mu-unused-history.jsonl"));
        app.start_key_flow(KeyTarget::find("anthropic").unwrap(), None, None);
        app.sync_key_flow();
        assert!(app.flows.key_flow.is_some(), "the key choice is still open");
        app.overlay = Some(Overlay::Logout { sel: Select::new("t", vec![]), providers: vec![] });
        app.sync_key_flow();
        assert!(app.flows.key_flow.is_none());
    }

    #[test]
    fn login_with_a_native_provider_opens_its_key_flow() {
        let mut app = test_app(std::env::temp_dir().join("mu-unused-history.jsonl"));
        app.login("anthropic");
        assert!(matches!(&app.overlay, Some(Overlay::KeyChoice { provider, .. }) if provider == "anthropic"));
        assert!(app.flows.key_flow.is_some());
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
        let stored: Vec<String> = app.services.providers.store().list().into_iter().map(|(p, _)| p).collect();
        if stored.is_empty() {
            assert!(transcript_text(&app).contains("no API keys are stored"));
        } else {
            assert!(matches!(&app.overlay, Some(Overlay::Logout { providers, .. }) if !providers.is_empty()));
        }
    }

    #[test]
    fn model_lists_every_role_and_a_role_without_a_model_gets_one_picked() {
        use mu_base::config::Configs;
        let mut app = test_app(std::env::temp_dir().join("mu-unused-history.jsonl"));
        let cfg = Configs {
            omp: serde_yaml_ng::from_str("modelRoles:\n  default: a/b\n  commit: c/d\n").unwrap(),
            ..Default::default()
        };
        let gateway = mu_provider::provider::endpoint::GatewaySource::new(app.plugins.gateway_start());
        app.services = Services { providers: Rc::new(Providers::for_tests(Rc::new(cfg), gateway)) };

        app.flows.pick_assign = true;
        app.open_model_picker(vec![], None);
        let Some(Overlay::Model { ids, target: None, .. }) = &app.overlay else { panic!("no model picker") };
        let roles: Vec<&str> = ids.iter().map(String::as_str).take_while(|i| i.starts_with('@')).collect();
        assert_eq!(roles, ["@default", "@plan", "@task", "@scout", "@review", "@smol", "@memory", "@slow"]);

        // /switch lists only the roles that have a model.
        app.flows.pick_assign = false;
        app.open_model_picker(vec![], None);
        let Some(Overlay::Model { ids, .. }) = &app.overlay else { panic!("no model picker") };
        assert_eq!(ids, &["@default"]);

        // Enter on an unset role asks for a model for it, and the picked model goes to that role.
        app.flows.pick_assign = true;
        mu_testutil::block_on_local(async { app.on_overlay_event(OverlayEvent::SetModel("@review".into())) });
        assert_eq!(app.flows.assign_role.as_deref(), Some("review"));
        let target = app.flows.assign_role.take();
        app.open_model_picker(vec![], target);
        let Some(Overlay::Model { ids, assign: true, target: Some(role), .. }) = &app.overlay else {
            panic!("no picker for the role")
        };
        assert_eq!(role, "review");
        assert!(ids.iter().all(|i| !i.starts_with('@')), "no roles to pick from: {ids:?}");
    }
}
