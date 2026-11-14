//! Input routing and API keys. A key goes to the open pty, the overlay, the MCP wizard or the
//! composer (`on_terminal`); `/login` and `/logout` handle API keys. Typed keys never reach the
//! agent, so they stay out of the transcript, the prompt history, the session file and predict's
//! corpus.

use super::*;
use crate::chat::composer::Action;
use crate::host::keys::{KeyFacts, KeyInfo, LoginList, LogoutList, TargetKind};
use crate::term::clipboard;
use crate::widgets::overlay::{KeyOption, LoginTarget};
use crate::widgets::picker::Picker;
use crate::widgets::select::{Item, Select};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use mu_provider::auth::chatgpt::PROVIDER_ID;
use mu_provider::auth::check::Check;
use mu_provider::auth::store::Entry;
use mu_provider::provider::providers::{self, SEARCH_KEYS};

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
    pub(super) target: KeyInfo,
    /// Whether the provider's chat models use the key, when already decided (anthropic and openai ask).
    pub(super) chat: Option<bool>,
    pub(super) then: Option<KeyThen>,
    /// The `/web` wizard that asked for this key; it is active again when the flow ends.
    pub(super) web: Option<Box<WebFlow>>,
    pub(super) typing: Option<Typing>,
    /// The entry a check could not confirm, waiting for "save anyway".
    pub(super) pending: Option<Entry>,
    /// A check is in flight.
    pub(super) checking: bool,
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
) -> Vec<(Item, LoginTarget)> {
    let providers = providers::api_key_defs().map(|d| {
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
    providers.chain(search).collect()
}

/// The two answers to "does chat use this key?"; `current` marks the stored choice.
fn chat_rows(target: &KeyInfo, current: Option<bool>) -> Vec<(Item, KeyOption)> {
    let id = &target.id;
    let row = |label: String, detail: &str, on: bool| {
        let item = Item { label, detail: if current == Some(on) { "current".into() } else { detail.into() } };
        (item, KeyOption::Chat(on))
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
                self.ctrl_c_armed = false;
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
                self.ctrl_c_armed = false;
                // A check is running: nothing typed or pasted goes anywhere until it answers.
                if self.custom_busy() {
                    return;
                }
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
        if !self.prompt.predict
            || self.prompt.composer.has_ghost()
            || self.flows.mcp().is_some()
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
        self.host_call(host::completion::GHOST, &(q.clone(), assistant), move |app, r: Result<String, String>| {
            let ghost = r.unwrap_or_default();
            app.on_ui(UiMsg::Ghost { seq, text: q, ghost });
        });
    }

    fn on_key(&mut self, k: KeyEvent) {
        if self.ctrl_c(&k) {
            return;
        }
        if let Some(p) = &self.agent.pty {
            p.key(k);
            return;
        }
        self.on_key_inner(k);
        if self.overlay.is_none() {
            self.request_ghost();
        }
    }

    /// Ctrl-C before anything else sees the key: the first press arms the exit and does nothing
    /// else, any other key disarms it, and the next press exits from whatever state the app is in
    /// (a turn, a pty tool, an overlay, a wizard). True when the key was used up here.
    fn ctrl_c(&mut self, k: &KeyEvent) -> bool {
        if !matches!(k.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return false;
        }
        if !(k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL)) {
            self.ctrl_c_armed = false;
            return false;
        }
        if std::mem::take(&mut self.ctrl_c_armed) {
            self.quit();
        } else {
            self.ctrl_c_armed = true;
            self.screen.dirty = true;
        }
        true
    }

    fn on_key_inner(&mut self, k: KeyEvent) {
        // While a key is typed, every key goes to the masked composer, even past an open overlay.
        let typing = self.typing();
        if self.custom_wizard_active() && self.custom_key(k) {
            return;
        }
        if self.chatgpt_typing() {
            if self.chatgpt_key(k) {
                return;
            }
        } else if typing {
            if self.key_flow_key(k) {
                return;
            }
        } else {
            if self.overlay.is_some() {
                return self.overlay_key(k);
            }
            if self.flows.mcp().is_some() && self.mcp_wizard_key(k) {
                return;
            }
        }
        if !typing && self.marked_job_key(&k) {
            return;
        }
        let action = self.prompt.composer.on_key(k, self.agent.running, &self.prompt.commands);
        // Only actions that stay in the composer may run while a key or command is typed.
        if typing && !matches!(action, Action::None | Action::Redraw | Action::Abort | Action::ToggleExpanded) {
            return;
        }
        match action {
            Action::None | Action::Redraw => {}
            Action::Reply => self.open_reply(),
            Action::Jobs => self.open_jobs(),
            Action::Submit(t) => self.submit(t, false),
            Action::QueueFollowUp(t) => self.submit(t, true),
            Action::Abort => self.send(Command::Cancel),
            Action::CycleThinking => self.send(Command::CycleThinking),
            Action::ToggleThinking => {
                let w = self.width();
                self.agent.tr.toggle_thinking(w);
            }
            Action::ToggleExpanded => self.agent.expanded = !self.agent.expanded,
            Action::ModelPicker => self.model_cmd(""),
            Action::EditExternal => self.edit_external(),
            Action::PasteImage => {
                let tx = self.ui_tx.clone();
                tokio::task::spawn_local(async move {
                    let _ = tx.send(UiMsg::ClipboardImage(clipboard::read_image().await));
                });
            }
            Action::JobFocus(by) => self.agent.focus_job(by),
            Action::JobCancel => self.cancel_job(),
            Action::PopQueue => self.send(Command::PopQueue),
        }
    }

    fn cancel_job(&mut self) {
        match self.agent.cancel_target() {
            Ok(id) => self.send(Command::KillJobs(Some(id))),
            Err(why) => self.notice(why),
        }
    }

    /// Keys while a job is marked: `k` or `x` cancels it, Esc takes the mark off, and any other
    /// key takes it off and then does what it normally does. Ctrl/Alt chords (and a bare modifier)
    /// leave the mark alone. True when the key was used up here.
    fn marked_job_key(&mut self, k: &KeyEvent) -> bool {
        if self.agent.job_focus.is_none()
            || !matches!(k.kind, KeyEventKind::Press | KeyEventKind::Repeat)
            || k.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
            || matches!(k.code, KeyCode::Modifier(_))
        {
            return false;
        }
        match k.code {
            KeyCode::Char('k' | 'x') if k.modifiers.is_empty() => {
                self.cancel_job();
                true
            }
            KeyCode::Esc => {
                self.agent.job_focus = None;
                true
            }
            _ => {
                self.agent.job_focus = None;
                false
            }
        }
    }

    /// `/login`: with a provider id run `omp login <id>`, with `mcp:<name>` log in to that MCP
    /// server; otherwise list omp's providers and the MCP servers that need auth in a picker.
    pub(super) fn login(&mut self, provider: &str) {
        if chatgpt::is_chatgpt(provider) {
            return self.start_chatgpt();
        }
        if provider == "custom" {
            return self.start_custom_wizard();
        }
        // `/login custom <id>` edits an endpoint that is already defined.
        if let Some(id) = provider.strip_prefix("custom ").map(str::trim)
            && !id.is_empty()
        {
            return self.start_custom_edit(id);
        }
        if let Some(name) = provider.strip_prefix("mcp:") {
            return self.run_mcp_login(name);
        }
        let name = provider.to_string();
        self.host_call(host::keys::KEY_TARGET, &provider, move |app, r: Result<Option<KeyInfo>, String>| match r {
            Ok(Some(target)) => app.begin_key(target),
            Ok(None) if !name.is_empty() => app.run_login(&[name.as_str()], None),
            Ok(None) => {
                app.notice("listing login providers…");
                app.host_call(host::keys::LOGIN_PROVIDERS, &(), |app, r: Result<LoginList, String>| app.show_login(r));
            }
            Err(e) => app.error(&e),
        });
    }

    /// The `/login` picker: the key rows, then omp's providers and the MCP servers that need auth.
    fn show_login(&mut self, r: Result<LoginList, String>) {
        self.leave_custom_wizard();
        let list = r.unwrap_or_else(|e| LoginList { error: Some(e), ..Default::default() });
        self.with_api_key_rows(move |app, mut rows| {
            for (n, name) in list.providers.into_iter().filter(|(_, name)| !is_baseten(name)) {
                rows.push((Item { label: name, detail: String::new() }, LoginTarget::Provider(n)));
            }
            // The API-key rows and MCP servers stay usable without omp.
            if let Some(e) = list.error {
                app.error(&format!("cannot list omp's login providers: {e}"));
            }
            for (name, url) in list.mcp {
                rows.push((Item { label: format!("mcp:{name}"), detail: url }, LoginTarget::Mcp(name)));
            }
            app.overlay = Some(Overlay::Login { pick: Picker::from_rows(rows, |items| Select::new("Log in", items)) });
        });
    }

    /// Keys the masked or command input takes before the composer. True when the key was handled.
    fn key_flow_key(&mut self, k: KeyEvent) -> bool {
        if k.code == KeyCode::Esc {
            self.cancel_key_flow();
            return true;
        }
        if k.code == KeyCode::Enter && !crate::widgets::editor::adds_newline(k.modifiers) {
            self.finish_typing();
            return true;
        }
        false
    }

    /// Enter in the masked or command input: the text becomes an entry and is checked.
    fn finish_typing(&mut self) {
        let Some(f) = self.flows.key() else { return };
        let (target, typing) = (f.target.clone(), f.typing);
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
        self.set_typing(None);
        self.unlock_composer();
    }

    /// Clears the typed text, gives the composer its normal behaviour back, and restores what was
    /// held back while the text was typed.
    pub(super) fn unlock_composer(&mut self) {
        self.prompt.composer.take_secret();
        self.prompt.composer.set_masked(false);
        self.prompt.composer.set_private(false);
        self.prompt.composer.set_closed(false);
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
        if let Some(f) = self.flows.key_mut() {
            f.typing = t;
        }
    }

    /// Whether a key or command is being typed (or a custom endpoint's text or check is waiting):
    /// the composer is locked to it.
    pub(super) fn typing(&self) -> bool {
        self.flows.key().is_some_and(|f| f.typing.is_some()) || self.chatgpt_typing() || self.custom_typing()
    }

    /// A key is being typed or a choice waits for the user: no prediction is asked for.
    pub(super) fn key_flow_pending(&self) -> bool {
        self.flows.key().is_some_and(|f| f.typing.is_some() || f.pending.is_some())
            || self.chatgpt_typing()
            || self.custom_typing()
    }

    /// Ends the key flow without saving. A flow that began elsewhere (the wizard) resumes there.
    fn cancel_key_flow(&mut self) {
        let flow = self.flows.end_key();
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
        let owned = matches!(self.overlay, Some(Overlay::KeyChoice { .. }))
            || self.parked.iter().any(|o| matches!(o, Overlay::KeyChoice { .. }));
        if self.flows.key().is_some_and(|f| f.typing.is_none() && !f.checking) && !owned {
            let then = self.flows.end_key().and_then(|f| f.then);
            self.key_flow_ended(then, false);
        }
    }

    /// The API-key rows that open `/login`: the chat providers, ChatGPT and the search services, then
    /// each custom endpoint (its key state, or that it takes none), then the row that adds one. A
    /// `providers.toml` that does not read is reported, and its endpoints are left out.
    pub(super) fn with_api_key_rows(&mut self, then: impl FnOnce(&mut App, Vec<(Item, LoginTarget)>) + 'static) {
        self.host_call(host::keys::KEY_ROWS, &(), move |app, r: Result<KeyFacts, String>| {
            let facts = r.unwrap_or_else(|e| KeyFacts { error: Some(e), ..Default::default() });
            if let Some(e) = &facts.error {
                app.error(e);
            }
            let stored = |p: &str| facts.stored.iter().any(|(id, _)| id == p);
            let routes_chat = |p: &str| facts.stored.iter().any(|(id, chat)| id == p && *chat);
            let env_set = |n: &str| facts.env_set.iter().any(|e| e == n);
            let mut rows = api_key_items(stored, env_set, routes_chat);
            rows.insert(0, chatgpt::login_item(facts.chatgpt.as_ref()));
            for c in &facts.customs {
                let label = if c.keyless {
                    format!("Custom endpoint: {}", c.label)
                } else {
                    format!("API key: {} (custom)", c.label)
                };
                rows.push((
                    Item { label, detail: crate::labels::key_state_word(c.state).into() },
                    LoginTarget::ApiKey(c.id.clone()),
                ));
                let edit = Item {
                    label: format!("Edit custom endpoint: {}", c.label),
                    detail: "address, key, models, reasoning".into(),
                };
                rows.push((edit, LoginTarget::CustomProvider(Some(c.id.clone()))));
            }
            let add =
                Item { label: "+ Add a custom endpoint…".into(), detail: "guided: address, key, models".into() };
            rows.push((add, LoginTarget::CustomProvider(None)));
            then(app, rows);
        });
    }

    /// `/login <id>` or a `/login` row: a key already stored for anthropic or openai opens a menu
    /// (chat use, replace); anything else starts the key flow.
    pub(super) fn begin_key(&mut self, target: KeyInfo) {
        if target.keyless {
            return self.notice(&format!(
                "{} takes no key (auth = \"none\" in {}); `Edit custom endpoint: {}` changes that",
                target.label,
                target.providers_path.display(),
                target.label
            ));
        }
        let Some(chat) = target.stored_chat.filter(|_| target.asks_chat) else {
            return self.start_key_flow(target, None, None);
        };
        let mut rows = chat_rows(&target, Some(chat));
        let replace = Item { label: "Replace the key".into(), detail: "paste, use a variable or a command".into() };
        rows.push((replace, KeyOption::Replace));
        rows.push((Item { label: "Cancel".into(), detail: "nothing changes".into() }, KeyOption::Cancel));
        let pick = Picker::from_rows(rows, |items| Select::new(&format!("{} API key", target.label), items));
        self.open_overlay(Overlay::KeyChoice { pick, provider: target.id });
    }

    /// Says where to get a key, then offers the ways to give one.
    /// `chat` presets the answer to "does chat use this key?", `then` resumes another flow at the end.
    pub(super) fn start_key_flow(&mut self, target: KeyInfo, chat: Option<bool>, then: Option<KeyThen>) {
        self.leave_custom_wizard();
        self.flows.key_flow_seq += 1;
        let (label, id, hint, url) =
            (target.label.clone(), target.id.clone(), target.env_hint.clone(), target.key_url.clone());
        let env_set = target.env_set;
        let web = if then == Some(KeyThen::WebWizard) { self.flows.end_web().map(Box::new) } else { None };
        self.flows.begin(Active::Key(Box::new(KeyFlow {
            id: self.flows.key_flow_seq,
            target,
            chat,
            then,
            web,
            typing: None,
            pending: None,
            checking: false,
        })));
        if !url.is_empty() {
            self.notice(&format!("{label} API keys: create one at {url}"));
        }
        let mut rows =
            vec![(Item { label: "Paste a key".into(), detail: "shown as bullets".into() }, KeyOption::Paste)];
        if env_set {
            rows.push((Item { label: format!("Use ${hint}"), detail: "already set".into() }, KeyOption::Env(hint)));
        }
        let command = Item { label: "Use a command".into(), detail: "a command that prints the key".into() };
        rows.push((command, KeyOption::Command));
        let pick = Picker::from_rows(rows, |items| Select::new(&format!("{label} API key"), items));
        self.open_overlay(Overlay::KeyChoice { pick, provider: id });
    }

    /// What the picker chose for `provider`, or `Cancel` when it was dismissed.
    pub(super) fn on_key_choice(&mut self, provider: &str, choice: KeyOption) {
        if provider == PROVIDER_ID {
            return self.on_chatgpt_choice(choice);
        }
        self.host_call(host::keys::KEY_TARGET, &provider, move |app, r: Result<Option<KeyInfo>, String>| match r {
            Ok(Some(target)) => app.key_chosen(target, choice),
            Ok(None) => {}
            Err(e) => app.error(&e),
        });
    }

    fn key_chosen(&mut self, target: KeyInfo, choice: KeyOption) {
        match choice {
            KeyOption::Paste => {
                self.set_typing(Some(Typing::Key));
                self.prompt.composer.set_masked(true);
                self.notice(&format!("paste the {} key and press Enter (Esc cancels)", target.label));
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
            KeyOption::SaveAnyway => match self.flows.key_mut().and_then(|f| f.pending.take()) {
                Some(entry) => self.confirm_key(target, entry),
                None => self.cancel_key_flow(),
            },
            KeyOption::Chat(on) => match self.flows.key_mut().and_then(|f| f.pending.take()) {
                Some(entry) => self.save_key(target, Entry { chat: Some(on), ..entry }),
                None => {
                    let args = (target.id.clone(), on);
                    self.host_call(host::keys::SET_CHAT, &args, move |app, r: Result<bool, String>| match r {
                        Ok(true) => {
                            let msg = if on {
                                format!(
                                    "{id}/* models now use your API key (billed per token) instead of omp.",
                                    id = target.id
                                )
                            } else {
                                format!(
                                    "{id}/* chat models go through omp again; the key stays for web search",
                                    id = target.id
                                )
                            };
                            app.notice(&msg);
                            app.send(Command::RefreshModel);
                        }
                        Ok(false) => app.notice(&format!("no {} key is stored", target.label)),
                        Err(e) => app.error(&format!("could not update the {} key: {e}", target.label)),
                    });
                }
            },
            KeyOption::Replace => self.start_key_flow(target, None, None),
            KeyOption::SignIn => self.start_chatgpt(),
            KeyOption::SignOut => self.chatgpt_sign_out(),
            KeyOption::Forget => self.chatgpt_forget(),
            KeyOption::Cancel => self.cancel_key_flow(),
        }
    }

    /// Resolves the entry and asks the provider whether it works. Nothing is saved yet.
    fn check_key(&mut self, target: KeyInfo, entry: Entry) {
        let Some(f) = self.flows.key_mut() else { return };
        f.checking = true;
        let flow = f.id;
        self.notice(&format!("checking the key with {}…", target.label));
        let args = (target.id.clone(), entry.clone());
        self.host_call(host::keys::CHECK_KEY, &args, move |app, r: Result<Check, String>| {
            app.on_key_checked(flow, target, entry, r)
        });
    }

    pub(super) fn on_key_checked(&mut self, flow: u64, target: KeyInfo, entry: Entry, r: Result<Check, String>) {
        match self.flows.key_mut() {
            Some(f) if f.id == flow => f.checking = false,
            // The flow was cancelled or replaced while the check ran.
            _ => return,
        }
        match r {
            Ok(Check::Valid) => self.confirm_key(target, entry),
            Ok(Check::Unchecked) => {
                self.notice(&format!(
                    "saved without checking: {} has no free way to test a key; a bad key shows on the first search",
                    target.label
                ));
                self.confirm_key(target, entry);
            }
            Ok(Check::Rejected(m)) => {
                self.end_key_flow(false);
                self.error(&format!("{m}. Nothing was saved; /login to try another key"));
            }
            Ok(Check::Unreachable(m)) => {
                self.notice(&format!("{m}; the key is not confirmed"));
                if let Some(f) = self.flows.key_mut() {
                    f.pending = Some(entry);
                }
                let rows = vec![
                    (
                        Item { label: "Save anyway".into(), detail: "use the key without checking it".into() },
                        KeyOption::SaveAnyway,
                    ),
                    (Item { label: "Cancel".into(), detail: "nothing is saved".into() }, KeyOption::Cancel),
                ];
                let pick =
                    Picker::from_rows(rows, |items| Select::new(&format!("{} did not answer", target.label), items));
                self.open_overlay(Overlay::KeyChoice { pick, provider: target.id });
            }
            Err(e) => {
                self.end_key_flow(false);
                self.error(&e);
            }
        }
    }

    fn end_key_flow(&mut self, saved: bool) {
        let then = self.flows.end_key().and_then(|f| f.then);
        self.key_flow_ended(then, saved);
    }

    /// A key that works is saved, after asking whether chat uses it when the user has not said.
    fn confirm_key(&mut self, target: KeyInfo, entry: Entry) {
        let undecided = self.flows.key().is_some_and(|f| f.chat.is_none());
        if !(target.asks_chat && undecided) {
            return self.save_key(target, entry);
        }
        if let Some(f) = self.flows.key_mut() {
            f.pending = Some(entry);
        }
        let title = format!("Use this {} key for chat models?", target.label);
        let pick = Picker::from_rows(chat_rows(&target, None), |items| Select::new(&title, items));
        self.open_overlay(Overlay::KeyChoice { pick, provider: target.id });
    }

    fn save_key(&mut self, target: KeyInfo, mut entry: Entry) {
        let flow = self.flows.end_key();
        let then = flow.as_ref().and_then(|f| f.then);
        entry.chat = match target.kind {
            // A search-only key has no chat flag; Baseten's only use is chat.
            TargetKind::Search => None,
            // A custom endpoint's key is for chat and nothing else; it never carries a base URL.
            TargetKind::Custom => Some(true),
            TargetKind::Provider if !target.asks_chat => Some(true),
            // Never left unset: an unset flag would route chat.
            TargetKind::Provider => Some(entry.chat.or(flow.and_then(|f| f.chat)).unwrap_or(false)),
        };
        if target.kind == TargetKind::Custom {
            entry.base_url = None;
        }
        let chat = entry.chat.unwrap_or(false);
        let args = (target.id.clone(), entry);
        self.host_call(host::keys::SAVE_KEY, &args, move |app, r: Result<(), String>| match r {
            Ok(()) => {
                match target.kind {
                    TargetKind::Provider => {
                        if let Some(def) = providers::native_def(&target.id) {
                            app.notice(&providers::saved_notice(def, chat));
                        }
                        app.send(Command::RefreshModel);
                    }
                    TargetKind::Search => app.notice(&format!("saved the {} key", target.label)),
                    TargetKind::Custom => {
                        app.notice(&format!("saved the {} key; {}/* models use it.", target.label, target.id));
                        app.send(Command::RefreshModel);
                    }
                }
                app.key_flow_ended(then, true);
            }
            Err(e) => {
                app.error(&format!("could not save the {} key: {e}", target.label));
                app.key_flow_ended(then, false);
            }
        });
    }

    /// `/logout [provider]`: removes the given key, or picks one of the stored keys.
    pub(super) fn logout_cmd(&mut self, arg: &str) {
        if !arg.is_empty() {
            return self.logout(arg);
        }
        self.host_call(host::keys::LOGOUT_LIST, &(), |app, r: Result<LogoutList, String>| {
            let list = match r {
                Ok(l) => l,
                Err(e) => return app.error(&e),
            };
            if let Some(e) = &list.error {
                app.error(e);
            }
            if list.rows.is_empty() {
                return app.notice("no API keys are stored; /login adds one");
            }
            let rows = list
                .rows
                .into_iter()
                .map(|r| {
                    if chatgpt::is_chatgpt(&r.id) {
                        return (Item { label: "ChatGPT sign-in".into(), detail: "sign out and revoke".into() }, r.id);
                    }
                    (Item { label: r.label, detail: "remove this key".into() }, r.id)
                })
                .collect();
            let pick = Picker::from_rows(rows, |items| Select::new("Remove a key or sign out", items));
            app.open_overlay(Overlay::Logout { pick });
        });
    }

    pub(super) fn logout(&mut self, provider: &str) {
        if chatgpt::is_chatgpt(provider) {
            return self.chatgpt_sign_out();
        }
        let name = provider.to_string();
        self.host_call(host::keys::KEY_TARGET, &provider, move |app, r: Result<Option<KeyInfo>, String>| match r {
            Ok(Some(target)) => app.remove_key(target),
            Ok(None) => app.error(&format!("no API key support for `{name}`")),
            Err(e) => app.error(&e),
        });
    }

    fn remove_key(&mut self, target: KeyInfo) {
        if target.keyless {
            return self.notice(&format!(
                "{} takes no key, so there is none to remove; remove its definition with `mu providers rm {}`",
                target.label, target.id
            ));
        }
        let was_chat = target.kind == TargetKind::Provider && target.stored_chat == Some(true);
        let id = target.id.clone();
        self.host_call(host::keys::REMOVE_KEY, &id, move |app, r: Result<bool, String>| match r {
            Ok(true) if target.kind == TargetKind::Custom => {
                app.notice(&format!(
                    "removed the {} key; {}/* models now need a key (/login {})",
                    target.label, target.id, target.id
                ));
                app.send(Command::RefreshModel);
            }
            Ok(true) if was_chat => {
                app.notice(&format!("removed the {} key; {}/* models go through omp again", target.label, target.id));
                app.send(Command::RefreshModel);
            }
            Ok(true) => app.notice(&format!("removed the {} key", target.label)),
            Ok(false) => app.notice(&format!("no {} key is stored", target.label)),
            Err(e) => app.error(&format!("could not remove the {} key: {e}", target.label)),
        });
    }
}

/// omp's login menu names Baseten too; mu owns that provider's key, so the omp row is dropped.
pub(super) fn is_baseten(name: &str) -> bool {
    name.split(|c: char| !c.is_alphanumeric())
        .find(|w| !w.is_empty())
        .is_some_and(|w| w.eq_ignore_ascii_case("baseten"))
}

#[cfg(test)]
mod login_tests {
    #[test]
    fn baseten_is_the_only_omp_row_mu_drops() {
        use super::is_baseten;
        assert!(is_baseten("Baseten"));
        assert!(is_baseten(" Baseten (API key)"));
        assert!(!is_baseten("Basetenx"));
        assert!(!is_baseten("Z.AI (GLM · Sign in)"));
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::render::Renderer;
    use crate::text::line::line_text;
    use crate::widgets::overlay::OverlayEvent;
    use mu_provider::provider::client::Providers;
    use mu_tools::context::Context;

    static MODEL_CONFIG_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Providers over a key store of their own: tests that save or remove keys must not see each other's.
    pub(in crate::app) fn test_providers(
        cfg: Rc<mu_base::config::Configs>,
        gateway: mu_provider::provider::endpoint::GatewaySource,
    ) -> Providers {
        let auth = std::env::temp_dir().join(format!("mu-tui-auth-{}", uuid::Uuid::new_v4())).join("auth.json");
        Providers::for_tests_with_auth(cfg, gateway, auth)
    }

    pub(in crate::app) fn transcript_text(app: &App) -> String {
        app.agent.tr.pending.iter().map(line_text).collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn login_rows_show_each_status() {
        let (items, targets): (Vec<_>, Vec<_>) =
            api_key_items(|p| p == "anthropic" || p == "exa", |n| n == "OPENAI_API_KEY", |_| false).into_iter().unzip();
        let details: Vec<&str> = items.iter().map(|i| i.detail.as_str()).collect();
        assert_eq!(details, ["key saved · not for chat", "$OPENAI_API_KEY is set", "not set", "not set", "key saved"]);
        assert_eq!(targets[2], LoginTarget::ApiKey("baseten".into()));
        assert_eq!(targets[4], LoginTarget::ApiKey("exa".into()));
        assert_eq!(items[3].label, "API key: Parallel (web search)");
        let rows = api_key_items(|p| p == "openai", |_| false, |_| true);
        assert_eq!(rows[1].0.detail, "key saved · chat");
    }

    #[test]
    fn escape_in_the_key_picker_cancels() {
        let rows = vec![(Item { label: "Paste a key".into(), detail: String::new() }, KeyOption::Paste)];
        let ov = Overlay::KeyChoice {
            pick: Picker::from_rows(rows, |items| Select::new("t", items)),
            provider: "anthropic".into(),
        };
        let (rest, ev) = ov.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(rest.is_none());
        assert!(matches!(ev, Some(OverlayEvent::Key { choice: KeyOption::Cancel, .. })));
    }

    #[test]
    fn a_late_chatgpt_result_is_ignored_and_escape_ends_the_sign_in() {
        let mut app = test_app(std::env::temp_dir().join("mu-unused-history.jsonl"));
        app.flows.active = Active::Chatgpt(chatgpt::ChatgptFlow { id: 2 });
        app.on_chatgpt_done(1, Err("late".into()));
        assert_eq!(app.flows.chatgpt().map(|f| f.id), Some(2), "the current sign-in survives");
        assert!(app.typing(), "the composer is locked to the redirect URL");
        app.on_terminal(press(KeyCode::Char('x'), KeyModifiers::NONE));
        assert_eq!(app.prompt.composer.text(), "x");
        app.on_terminal(press(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.flows.chatgpt().is_none() && !app.typing());
        assert_eq!(app.prompt.composer.text(), "", "the typed text is cleared");
        assert!(transcript_text(&app).contains("sign-in cancelled"));
    }

    impl App {
        /// The `/login` key rows, asked of the host and read back; for a host that answers at once.
        pub(in crate::app) fn api_key_rows(&mut self) -> Vec<(Item, LoginTarget)> {
            let out = Rc::new(std::cell::RefCell::new(Vec::new()));
            let got = out.clone();
            self.with_api_key_rows(move |_, rows| *got.borrow_mut() = rows);
            out.take()
        }

        /// Swaps the provider registry, for the app and for the host that answers for it.
        pub(in crate::app) fn use_providers(&mut self, providers: Rc<mu_provider::provider::client::Providers>) {
            self.host = Rc::new(crate::host::local::LocalHost::new(
                providers.clone(),
                self.ctx.clone(),
                self.plugins.clone(),
                self.cwd.clone(),
            ));
            self.services = Services { providers };
        }
    }

    pub(in crate::app) fn test_app(history: std::path::PathBuf) -> App {
        test_app_with_commands(history).0
    }

    pub(in crate::app) fn test_app_with_commands(history: std::path::PathBuf) -> (App, UnboundedReceiver<Command>) {
        use mu_base::config::Configs;
        let (cmd_tx, cmd_rx) = unbounded_channel();
        let (ui_tx, _ui_rx) = unbounded_channel();
        let (ntx, _nrx) = unbounded_channel();
        let plugins = mu_tools::plugin::host::PluginHost::new(vec![], vec![], ntx, Default::default());
        let gateway = mu_provider::provider::endpoint::GatewaySource::new(plugins.gateway_start());
        let providers = Rc::new(test_providers(Rc::new(Configs::default()), gateway));
        let ctx = Rc::new(Context::default());
        let host =
            Rc::new(crate::host::local::LocalHost::new(providers.clone(), ctx.clone(), plugins.clone(), "/".into()));
        let app = App {
            agent: AgentView::new(true, false),
            host,
            services: Services { providers },
            link: Link { cmd_tx },
            ctx: ctx.clone(),
            plugins: plugins.clone(),
            ui_tx,
            cwd: PathBuf::from("/"),
            screen: Screen {
                renderer: Renderer::new(),
                last_draw: Instant::now(),
                dirty: false,
                child_busy: false,
                started: Instant::now(),
                status_cfg: Default::default(),
                status_levels: Default::default(),
                notify: false,
            },
            prompt: Prompt {
                composer: Composer::new(Some(history)),
                commands: vec![],
                predict: false,
                ghost_seq: 0,
                ghost_asked: String::new(),
                held_back: vec![],
            },
            overlay: None,
            parked: VecDeque::new(),
            schedule: Schedule::default(),
            approvals: VecDeque::new(),
            flows: Flows {
                model_request_seq: 0,
                pick_deliver: false,
                active: Active::None,
                key_flow_seq: 0,
                chatgpt_seq: 0,
                custom_wizard_seq: 0,
            },
            quitting: false,
            ctrl_c_armed: false,
        };
        (app, cmd_rx)
    }

    #[derive(Clone, Default)]
    struct Sink(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for Sink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_frame_is_drawn_to_the_injected_writer_at_the_injected_size() {
        let hist = std::env::temp_dir().join(format!("mu-draw-{}", uuid::Uuid::new_v4())).join("history.jsonl");
        let mut app = test_app(hist.clone());
        let sink = Sink::default();
        app.screen.renderer = Renderer::fixed(sink.clone(), (40, 10));
        in_local(async {
            for c in "hello".chars() {
                app.on_terminal(press(KeyCode::Char(c), KeyModifiers::NONE));
            }
        });
        app.draw().unwrap();
        let out = String::from_utf8(std::mem::take(&mut *sink.0.lock().unwrap())).unwrap();
        assert!(out.contains("hello"), "{out:?}");
        let rows = out.matches("\r\n").count() + 1;
        assert!(rows < 10, "the viewport leaves a row of the 10: {rows}");

        app.agent.tr.pending.push(crate::text::line::styled("scrolled up", |s| s));
        app.draw().unwrap();
        let out = String::from_utf8(std::mem::take(&mut *sink.0.lock().unwrap())).unwrap();
        assert!(out.contains("scrolled up") && out.contains("hello"), "{out:?}");
        let _ = std::fs::remove_dir_all(hist.parent().unwrap());
    }

    pub(in crate::app) fn press(code: KeyCode, mods: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new(code, mods))
    }

    /// Runs `f` on a current-thread runtime inside a `LocalSet`, as `spawn_local` needs.
    pub(in crate::app) fn in_local(f: impl std::future::Future<Output = ()>) {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        tokio::task::LocalSet::new().block_on(&rt, f);
    }

    #[test]
    fn typed_key_reaches_no_overlay_history_transcript_or_prediction() {
        let hist = std::env::temp_dir().join(format!("mu-keyflow-{}", uuid::Uuid::new_v4())).join("history.jsonl");
        let mut app = test_app(hist.clone());
        in_local(async {
            app.start_key_flow(KeyInfo::builtin("anthropic").unwrap(), None, None);
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
            app.overlay = Some(Overlay::Logout { pick: Picker::placeholder(vec![], |items| Select::new("t", items)) });
            app.on_terminal(Event::Paste("PASTED".into()));
            assert!(app.overlay.is_some());
            assert_eq!(app.prompt.composer.text(), "sk-abPASTED");
            assert_eq!(app.prompt.ghost_seq, 0);

            // Text held back while typing comes back once the key flow ends.
            app.restore_queued(vec![UserInput::from("queued")]);
            assert_eq!(app.prompt.composer.text(), "sk-abPASTED", "the key is untouched");
            app.on_terminal(press(KeyCode::Esc, KeyModifiers::NONE));
            assert!(app.flows.key().is_none());
            assert_eq!(app.prompt.composer.text(), "queued");
        });
        let shown = transcript_text(&app);
        assert!(!shown.contains("sk-ab") && !shown.contains("PASTED"), "{shown}");
        assert!(!hist.exists(), "nothing typed is written to history");
    }

    #[test]
    fn the_status_line_adds_what_the_subagents_spent() {
        use mu_base::events::{AgentEvent, JobKind, StatusSnapshot, SubagentState, SubagentStatus};
        let d = std::env::temp_dir().join(format!("mu-spend-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        let root = d.join("2026-01-01T00-00-00-000Z_root.jsonl");
        let kid = d.join("2026-01-01T00-00-01-000Z_kid.jsonl");
        std::fs::write(&root, "{\"type\":\"session\",\"id\":\"root\"}\n").unwrap();
        let reply = serde_json::json!({"role":"assistant","content":[],"usage":{"input":1,"output":1,"cacheRead":0,
            "cacheWrite":0,"totalTokens":2,"cost":{"total":0.25}},"stopReason":"stop","timestamp":1});
        let entry = serde_json::json!({"type":"message","id":"a","parentId":null,"message":reply});
        std::fs::write(&kid, format!("{{\"type\":\"session\",\"id\":\"kid\"}}\n{entry}\n")).unwrap();
        let job = SubagentStatus {
            name: "scout".into(),
            agent: "scout".into(),
            tools: 1,
            last: String::new(),
            state: SubagentState::Done,
            kind: JobKind::Subagent,
            cwd: d.clone(),
            task: String::new(),
            started: std::time::SystemTime::now(),
            ended: None,
            delivered: true,
            note: None,
            session: Some(kid),
            model: None,
        };
        let (ui_tx, mut ui_rx) = unbounded_channel();
        let mut app = test_app(std::env::temp_dir().join("mu-unused-history.jsonl"));
        app.ui_tx = ui_tx;
        in_local(async {
            let status =
                StatusSnapshot { session_path: Some(root.to_string_lossy().into_owned()), ..Default::default() };
            app.on_agent(AgentEvent::Status(status));
            app.on_agent(AgentEvent::Jobs(vec![job]));
            // Each event started a read; the first one's reader came back before the second began.
            while app.agent.subagent_cost == 0.0 {
                let Some(msg) = ui_rx.recv().await else { panic!("the read never reported") };
                app.on_ui(msg);
            }
        });
        assert!((app.agent.subagent_cost - 0.25).abs() < 1e-9, "{}", app.agent.subagent_cost);
        assert!(app.agent.spend.is_some(), "the reader is back for the next read");
    }

    #[test]
    fn ctrl_c_twice_exits_from_any_state_and_the_first_press_only_arms() {
        let (mut app, mut commands) = test_app_with_commands(std::env::temp_dir().join("mu-unused-history.jsonl"));
        in_local(async {
            app.agent.running = true;
            app.prompt.composer.set_text("half a question");
            app.on_terminal(press(KeyCode::Char('c'), KeyModifiers::CONTROL));
            assert!(app.ctrl_c_armed && !app.quitting);
            assert_eq!(app.prompt.composer.text(), "half a question", "Ctrl-C clears nothing");
            assert!(commands.try_recv().is_err(), "Ctrl-C aborts nothing");

            app.on_terminal(press(KeyCode::Left, KeyModifiers::NONE));
            assert!(!app.ctrl_c_armed, "any other key disarms");
            app.on_terminal(press(KeyCode::Char('c'), KeyModifiers::CONTROL));
            assert!(!app.quitting, "the first press only arms");

            // A pty tool takes the keys, but not this one: neither press reaches the command.
            let (tx, mut pty_in) = unbounded_channel();
            app.agent.pty = Some(crate::panels::ptyview::PtyView::new("t".into(), tx, (80, 24)));
            app.on_terminal(press(KeyCode::Char('c'), KeyModifiers::CONTROL));
            assert!(pty_in.try_recv().is_err(), "Ctrl-C is not a keystroke for the command");
            app.on_terminal(press(KeyCode::Char('c'), KeyModifiers::CONTROL));
            assert!(app.quitting, "the second press exits with a pty open");
            app.agent.pty = None;

            // Nor does an open panel swallow it.
            let (mut app, mut commands) = test_app_with_commands(std::env::temp_dir().join("mu-unused-history.jsonl"));
            app.on_terminal(press(KeyCode::Char('j'), KeyModifiers::ALT));
            assert!(matches!(app.overlay, Some(Overlay::Jobs { .. })));
            app.on_terminal(press(KeyCode::Char('c'), KeyModifiers::CONTROL));
            assert!(matches!(app.overlay, Some(Overlay::Jobs { .. })) && !app.quitting, "the panel stays");
            app.on_terminal(press(KeyCode::Char('c'), KeyModifiers::CONTROL));
            assert!(app.quitting, "the second press exits from inside the panel");
            let exited = std::iter::from_fn(|| commands.try_recv().ok()).any(|c| matches!(c, Command::Exit));
            assert!(exited, "the app asked the agent to exit");
        });
    }

    #[test]
    fn alt_keys_mark_and_cancel_running_jobs_without_the_panel() {
        use mu_base::events::{JobKind, SubagentState, SubagentStatus};
        let job = |name: &str| SubagentStatus {
            name: name.into(),
            agent: "bash".into(),
            tools: 0,
            last: String::new(),
            state: SubagentState::Running,
            kind: JobKind::Bash,
            cwd: std::path::PathBuf::from("/"),
            task: "sleep".into(),
            started: std::time::SystemTime::now(),
            ended: None,
            delivered: false,
            note: None,
            session: None,
            model: None,
        };
        let (mut app, mut commands) = test_app_with_commands(std::env::temp_dir().join("mu-unused-history.jsonl"));
        in_local(async {
            app.on_agent(AgentEvent::Jobs(vec![job("a"), job("b")]));
            app.on_terminal(press(KeyCode::Char('k'), KeyModifiers::ALT));
            assert!(commands.try_recv().is_err(), "two jobs and none marked: nothing is guessed");
            assert!(transcript_text(&app).contains("Alt+↓ marks one"));
            app.on_terminal(press(KeyCode::Down, KeyModifiers::ALT));
            app.on_terminal(press(KeyCode::Down, KeyModifiers::ALT));
            app.on_terminal(press(KeyCode::Char('k'), KeyModifiers::ALT));
            assert!(matches!(commands.try_recv(), Ok(Command::KillJobs(Some(id))) if id == "b"));
            app.on_terminal(press(KeyCode::Char('K'), KeyModifiers::ALT | KeyModifiers::SHIFT));
            assert!(commands.try_recv().is_err(), "no key cancels every job");
            // Alt+↑ marks upward; Shift+↑ is the queued-message key.
            app.agent.job_focus = None;
            app.on_terminal(press(KeyCode::Up, KeyModifiers::ALT));
            assert_eq!(app.agent.job_focus.as_deref(), Some("b"));
            assert!(commands.try_recv().is_err());
            app.on_terminal(press(KeyCode::Up, KeyModifiers::SHIFT));
            assert!(matches!(commands.try_recv(), Ok(Command::PopQueue)));
            assert_eq!(app.agent.job_focus, None, "a key that is not a job key takes the mark off");

            // While a job is marked a bare `k` or `x` cancels it, and Esc only unmarks.
            app.on_terminal(press(KeyCode::Up, KeyModifiers::ALT));
            app.on_terminal(press(KeyCode::Esc, KeyModifiers::NONE));
            assert_eq!(app.agent.job_focus, None);
            assert!(commands.try_recv().is_err(), "Esc is not an abort while a job is marked");
            app.on_terminal(press(KeyCode::Char('k'), KeyModifiers::NONE));
            assert_eq!(app.prompt.composer.text(), "k", "unmarked, `k` is a letter");
            app.prompt.composer.take_for_submit();
            app.on_terminal(press(KeyCode::Up, KeyModifiers::ALT));
            app.on_terminal(press(KeyCode::Char('o'), KeyModifiers::CONTROL));
            assert_eq!(app.agent.job_focus.as_deref(), Some("b"), "a chord leaves the mark");
            app.on_terminal(press(KeyCode::Char('x'), KeyModifiers::NONE));
            assert!(matches!(commands.try_recv(), Ok(Command::KillJobs(Some(id))) if id == "b"));
            assert_eq!(app.prompt.composer.text(), "", "the key was used up");
            app.on_terminal(press(KeyCode::Char('h'), KeyModifiers::NONE));
            assert_eq!(app.prompt.composer.text(), "h");
        });
    }

    #[test]
    fn a_late_check_result_of_an_old_flow_is_ignored() {
        let mut app = test_app(std::env::temp_dir().join("mu-unused-history.jsonl"));
        let t = KeyInfo::builtin("anthropic").unwrap();
        app.start_key_flow(t.clone(), None, None);
        app.flows.end_key();
        app.start_key_flow(t.clone(), None, None);
        app.on_key_checked(1, t, Entry::literal("sk-x"), Ok(Check::Valid));
        assert_eq!(app.flows.key().map(|f| f.id), Some(2), "the current flow survives");
    }

    #[test]
    fn a_valid_openai_key_asks_about_chat_and_saves_the_answer() {
        let mut app = test_app(std::env::temp_dir().join("mu-unused-history.jsonl"));
        let t = KeyInfo::builtin("openai").unwrap();
        app.start_key_flow(t.clone(), None, None);
        app.on_key_checked(1, t.clone(), Entry::literal("sk-test-0123456789"), Ok(Check::Valid));
        let Some(Overlay::KeyChoice { pick, .. }) = &app.overlay else { panic!("no chat question") };
        assert_eq!(pick.values(), &[KeyOption::Chat(true), KeyOption::Chat(false)]);
        assert!(app.services.providers.store().get("openai").is_none(), "nothing is saved before the answer");
        app.on_key_choice("openai", KeyOption::Chat(false));
        let store = app.services.providers.store().clone();
        let saved = store.get("openai").unwrap();
        assert_eq!(saved.chat, Some(false));
        assert!(!app.services.providers.is_direct("openai"));
        // The existing-key menu changes the answer without a new key.
        app.begin_key(host_target(&app, "openai"));
        let Some(Overlay::KeyChoice { pick, .. }) = &app.overlay else { panic!("no menu") };
        assert_eq!(pick.values().len(), 4);
        app.on_key_choice("openai", KeyOption::Chat(true));
        assert!(app.services.providers.is_direct("openai"));
        store.remove("openai").unwrap();
    }

    #[test]
    fn a_search_key_is_saved_without_a_chat_question() {
        let mut app = test_app(std::env::temp_dir().join("mu-unused-history.jsonl"));
        let t = KeyInfo::builtin("exa").unwrap();
        app.start_key_flow(t.clone(), None, None);
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
        assert!(app.flows.web().is_none());
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
        let f = app.flows.key().expect("a key flow");
        assert_eq!((f.target.id.as_str(), f.chat, f.then), ("exa", Some(false), Some(KeyThen::WebWizard)));
        assert!(matches!(&app.overlay, Some(Overlay::KeyChoice { provider, .. }) if provider == "exa"));
        // Cancelling the key flow returns to the backend choice.
        app.on_key_choice("exa", KeyOption::Cancel);
        assert!(app.flows.key().is_none());
        assert!(matches!(&app.overlay, Some(Overlay::Web { .. })));
    }

    #[test]
    fn a_flow_whose_overlay_was_replaced_is_dropped() {
        let mut app = test_app(std::env::temp_dir().join("mu-unused-history.jsonl"));
        app.start_key_flow(KeyInfo::builtin("anthropic").unwrap(), None, None);
        app.sync_key_flow();
        assert!(app.flows.key().is_some(), "the key choice is still open");
        app.overlay = Some(Overlay::Logout { pick: Picker::placeholder(vec![], |items| Select::new("t", items)) });
        app.sync_key_flow();
        assert!(app.flows.key().is_none());
    }

    #[test]
    fn login_with_a_native_provider_opens_its_key_flow() {
        let mut app = test_app(std::env::temp_dir().join("mu-unused-history.jsonl"));
        app.login("anthropic");
        assert!(matches!(&app.overlay, Some(Overlay::KeyChoice { provider, .. }) if provider == "anthropic"));
        assert!(app.flows.key().is_some());
    }

    /// What the host says a provider id stands for, from a host that answers at once.
    fn host_target(app: &App, id: &str) -> KeyInfo {
        let crate::host::Answer::Ready(Ok(v)) = app.host.call(host::keys::KEY_TARGET, serde_json::json!(id)) else {
            panic!("no immediate answer")
        };
        serde_json::from_value::<Option<KeyInfo>>(v).unwrap().unwrap()
    }

    /// An app whose host answers every call later, and the messages those answers arrive as.
    fn deferred_app() -> (App, UnboundedReceiver<UiMsg>) {
        let mut app = test_app(std::env::temp_dir().join("mu-unused-history.jsonl"));
        let (ui_tx, ui_rx) = unbounded_channel();
        app.ui_tx = ui_tx;
        app.host = Rc::new(crate::host::Deferred(crate::host::local::LocalHost::new(
            app.services.providers.clone(),
            app.ctx.clone(),
            app.plugins.clone(),
            app.cwd.clone(),
        )));
        (app, ui_rx)
    }

    /// Hands the next answer of the host to the app.
    async fn pump(app: &mut App, ui_rx: &mut UnboundedReceiver<UiMsg>) {
        let m = ui_rx.recv().await.expect("the host answers");
        app.on_ui(m);
    }

    #[test]
    fn a_deferred_host_opens_the_key_flow_after_its_answer() {
        let (mut app, mut rx) = deferred_app();
        in_local(async {
            app.login("anthropic");
            assert!(app.overlay.is_none() && app.flows.key().is_none(), "nothing before the answer");
            pump(&mut app, &mut rx).await;
            assert!(matches!(&app.overlay, Some(Overlay::KeyChoice { provider, .. }) if provider == "anthropic"));
            assert!(app.flows.key().is_some());

            // Choosing "paste" asks the host for the target again, then locks the composer to the key.
            app.on_key_choice("anthropic", KeyOption::Paste);
            assert!(!app.typing());
            pump(&mut app, &mut rx).await;
            assert!(app.typing());
            assert!(transcript_text(&app).contains("paste the Anthropic key"));

            // A keyless endpoint and an unknown id are told apart by the same call.
            app.on_key_choice("nope", KeyOption::Cancel);
            pump(&mut app, &mut rx).await;
            assert!(app.typing(), "an unknown id changes nothing");
        });
    }

    #[test]
    fn a_deferred_check_saves_a_search_key_and_reports_an_unreadable_one() {
        let (mut app, mut rx) = deferred_app();
        in_local(async {
            let store = app.services.providers.store().clone();
            let t = KeyInfo::builtin("exa").unwrap();
            app.start_key_flow(t.clone(), None, None);
            app.overlay = None;
            app.check_key(t.clone(), Entry::literal("exa-test-0123456789"));
            assert!(app.flows.key().is_some_and(|f| f.checking), "the check is in flight");
            assert!(store.get("exa").is_none());
            pump(&mut app, &mut rx).await; // the check
            assert!(app.flows.key().is_none(), "the flow ends when the save is asked for");
            pump(&mut app, &mut rx).await; // the save
            assert_eq!(store.get("exa").unwrap().chat, None);
            assert!(transcript_text(&app).contains("saved the Exa key"));
            store.remove("exa").unwrap();

            app.start_key_flow(t.clone(), None, None);
            app.overlay = None;
            app.check_key(t, Entry::reference("$MU_NO_SUCH_KEY_VARIABLE").unwrap());
            pump(&mut app, &mut rx).await;
            assert!(app.flows.key().is_none());
            assert!(transcript_text(&app).contains("MU_NO_SUCH_KEY_VARIABLE"), "{}", transcript_text(&app));
        });
    }

    #[test]
    fn a_deferred_chat_choice_and_logout_change_the_store() {
        let (mut app, mut rx) = deferred_app();
        in_local(async {
            let store = app.services.providers.store().clone();
            store.set("openai", Entry { chat: Some(false), ..Entry::literal("sk-test-0123456789") }).unwrap();
            app.login("openai");
            pump(&mut app, &mut rx).await;
            let Some(Overlay::KeyChoice { pick, .. }) = &app.overlay else { panic!("no menu") };
            assert_eq!(pick.values().len(), 4, "the existing-key menu");
            app.on_key_choice("openai", KeyOption::Chat(true));
            pump(&mut app, &mut rx).await; // the target
            pump(&mut app, &mut rx).await; // set_chat
            assert!(app.services.providers.is_direct("openai"));
            assert!(transcript_text(&app).contains("openai/* models now use your API key"));

            app.logout_cmd("");
            pump(&mut app, &mut rx).await;
            let Some(Overlay::Logout { pick }) = &app.overlay else { panic!("no logout list") };
            assert_eq!(pick.values(), &["openai"]);
            app.overlay = None;

            app.logout("openai");
            pump(&mut app, &mut rx).await; // the target
            pump(&mut app, &mut rx).await; // remove_key
            assert!(store.get("openai").is_none());
            assert!(transcript_text(&app).contains("removed the OpenAI key; openai/* models go through omp again"));
            app.logout("openai");
            pump(&mut app, &mut rx).await;
            pump(&mut app, &mut rx).await;
            assert!(transcript_text(&app).contains("no OpenAI key is stored"));
        });
    }

    #[test]
    fn a_deferred_login_list_has_the_key_rows_omp_providers_and_mcp_servers() {
        use crate::host::keys::LoginList;
        let (mut app, mut rx) = deferred_app();
        in_local(async {
            let list = LoginList {
                providers: vec![(1, "Z.AI".into()), (2, "Baseten".into())],
                error: Some("no omp".into()),
                mcp: vec![("docs".into(), "https://docs.example".into())],
            };
            app.show_login(Ok(list));
            assert!(app.overlay.is_none());
            pump(&mut app, &mut rx).await;
            let Some(Overlay::Login { pick }) = &app.overlay else { panic!("no login list") };
            let labels: Vec<&LoginTarget> = pick.values().iter().collect();
            assert!(labels.contains(&&LoginTarget::Provider(1)) && !labels.contains(&&LoginTarget::Provider(2)));
            assert!(labels.contains(&&LoginTarget::Mcp("docs".into())));
            assert!(labels.contains(&&LoginTarget::ChatGpt));
            assert!(labels.contains(&&LoginTarget::CustomProvider(None)));
            assert!(transcript_text(&app).contains("cannot list omp's login providers: no omp"));
        });
    }

    #[test]
    fn a_deferred_web_wizard_reads_and_saves_the_choice() {
        let (mut app, mut rx) = deferred_app();
        in_local(async {
            let store = app.services.providers.store().clone();
            let (tx, mut reply) = tokio::sync::oneshot::channel();
            app.start_web_wizard(Some(tx));
            assert!(app.overlay.is_none() && app.flows.web().is_none());
            pump(&mut app, &mut rx).await;
            assert!(matches!(app.overlay, Some(Overlay::Web { .. })));
            app.on_web_pick(Some(6)); // Off
            assert_eq!(reply.try_recv(), Err(tokio::sync::oneshot::error::TryRecvError::Empty));
            pump(&mut app, &mut rx).await;
            assert_eq!(reply.try_recv(), Ok(false));
            assert_eq!(store.web(), Some(mu_provider::auth::web::WebSearch::off()));
            store.set_web(None).unwrap();

            // A keyed backend without a key asks for it through the host too.
            app.start_web_wizard(None);
            pump(&mut app, &mut rx).await;
            app.on_web_pick(Some(3));
            assert!(app.flows.key().is_none());
            pump(&mut app, &mut rx).await;
            let f = app.flows.key().expect("a key flow");
            assert_eq!((f.target.id.as_str(), f.then), ("exa", Some(KeyThen::WebWizard)));
        });
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
            assert!(matches!(&app.overlay, Some(Overlay::Logout { pick }) if !pick.is_empty()));
        }
    }

    #[test]
    fn refresh_is_discoverable_and_queues_when_busy() {
        let (mut app, mut commands) = test_app_with_commands(std::env::temp_dir().join("mu-unused-history.jsonl"));
        assert!(super::super::commands::interactive_reference().contains("`/refresh`"));
        app.submit(UserInput::from("/refresh"), false);
        assert!(matches!(commands.try_recv(), Ok(Command::RefreshStatus)));
        assert!(!app.agent.running);
        app.agent.running = true;
        app.submit(UserInput::from("/refresh status"), false);
        assert!(matches!(commands.try_recv(), Ok(Command::RefreshStatus)));
        assert!(transcript_text(&app).contains("status refresh queued until the current turn finishes"));
        app.submit(UserInput::from("/refresh invalid"), false);
        assert!(commands.try_recv().is_err());
        assert!(transcript_text(&app).contains("usage: /refresh [status]"));
    }

    #[test]
    fn model_editor_is_independent_of_session_switching_and_discovery() {
        use mu_base::config::Configs;
        let (mut app, mut commands) = test_app_with_commands(std::env::temp_dir().join("mu-unused-history.jsonl"));
        let cfg = Rc::new(Configs::from_models_toml("[sets.main]\ndefault = \"a/old:high\"\n"));
        let gateway = mu_provider::provider::endpoint::GatewaySource::new(app.plugins.gateway_start());
        app.use_providers(Rc::new(test_providers(cfg.clone(), gateway)));

        app.model_cmd("");
        assert!(matches!(app.overlay, Some(Overlay::ModelRoles { .. })));
        assert_eq!(app.flows.model_request_seq, 0, "listing roles does not discover models");
        app.model_cmd("@task");
        assert!(matches!(app.overlay, Some(Overlay::ModelRoles { .. })));
        app.model_cmd("provider/model");
        app.model_cmd("@1bad");
        assert!(transcript_text(&app).contains("role names are a letter"));
        app.model_cmd("@task:high");
        assert!(transcript_text(&app).contains("usage: /model [@role]"));
        assert_eq!(cfg.role("default").as_deref(), Some("a/old:high"));
        assert!(commands.try_recv().is_err());

        app.overlay = None;
        app.on_terminal(press(KeyCode::Char('m'), KeyModifiers::ALT));
        assert!(matches!(app.overlay, Some(Overlay::ModelRoles { .. })), "Alt+M opens saved roles");
        app.submit(UserInput::from("/switch @default"), false);
        assert!(matches!(commands.try_recv(), Ok(Command::SetModel(id)) if id == "@default"));
        assert_eq!(cfg.role("default").as_deref(), Some("a/old:high"));
    }

    #[test]
    fn late_model_lists_only_fill_their_own_picker() {
        use mu_provider::provider::custom::{CustomModel, CustomProvider};
        let (mut app, _commands) = test_app_with_commands(std::env::temp_dir().join("mu-unused-history.jsonl"));
        // A configured model is a row the picker has before any refresh answers.
        let modal = CustomProvider {
            models: std::collections::BTreeMap::from([("x/b".to_string(), CustomModel::default())]),
            discover: false,
            ..CustomProvider::new("http://127.0.0.1:9")
        };
        app.services.providers.custom_registry().add("modal", modal.validated("modal").unwrap()).unwrap();
        in_local(async {
            app.request_models();
            let a = app.flows.model_request_seq;
            app.overlay = None;
            app.on_models_refreshed(a, None);
            assert!(app.overlay.is_none(), "a cancelled picker is not reopened");

            app.open_model_roles(Some("default".into()));
            app.overlay_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
            let b = app.flows.model_request_seq;
            assert_eq!(b, a + 1);
            app.on_models_refreshed(a, Some("late".into()));
            assert!(!transcript_text(&app).contains("late"), "a stale report is not shown");
            app.on_models_refreshed(b, None);
            let Some(Overlay::ModelRoles { st }) = &app.overlay else { panic!("editor gone") };
            assert!(st.select().highlighted().is_some(), "the matching list filled the sub-picker");
        });
    }

    #[test]
    fn saving_a_role_edits_config_without_sending_session_commands() {
        use mu_base::config::{Configs, models_path};
        let _guard = MODEL_CONFIG_TEST_LOCK.lock().unwrap();
        mu_testutil::home();
        let path = models_path();
        let text = "[sets.main]\ndefault = \"a/old:high\"\n";
        std::fs::write(&path, text).unwrap();
        let (mut app, mut commands) = test_app_with_commands(std::env::temp_dir().join("mu-unused-history.jsonl"));
        let cfg = Rc::new(Configs::from_models_toml(text));
        let gateway = mu_provider::provider::endpoint::GatewaySource::new(app.plugins.gateway_start());
        app.use_providers(Rc::new(test_providers(cfg.clone(), gateway)));
        app.open_model_roles(Some("default".into()));
        app.on_overlay_event(OverlayEvent::SaveRole {
            set: "main".into(),
            role: "default".into(),
            value: Some("a/new:low".into()),
            meta: None,
        });
        assert_eq!(cfg.role("default").as_deref(), Some("a/new:low"));
        assert!(std::fs::read_to_string(&path).unwrap().contains("default = \"a/new:low\""));
        assert_eq!(app.agent.status.model_id, "");
        assert!(commands.try_recv().is_err(), "role saves do not send commands to the session");
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn saving_and_deleting_a_custom_role_writes_its_settings() {
        use mu_base::config::{Configs, models_path};
        let _guard = MODEL_CONFIG_TEST_LOCK.lock().unwrap();
        mu_testutil::home();
        let path = models_path();
        let text = "[sets.main]\ndefault = \"a/old:high\"\nwriter = \"w/m\"\n";
        std::fs::write(&path, text).unwrap();
        let (mut app, mut commands) = test_app_with_commands(std::env::temp_dir().join("mu-unused-history.jsonl"));
        let cfg = Rc::new(Configs::from_models_toml(text));
        let gateway = mu_provider::provider::endpoint::GatewaySource::new(app.plugins.gateway_start());
        app.use_providers(Rc::new(test_providers(cfg.clone(), gateway)));
        app.open_model_roles(Some("writer".into()));
        let meta = mu_base::model_config::RoleMeta {
            description: Some("Long-form docs".into()),
            fallback: vec!["default".into()],
            delegate: true,
        };
        app.on_overlay_event(OverlayEvent::SaveRole {
            set: "main".into(),
            role: "writer".into(),
            value: Some("w/n".into()),
            meta: Some(meta.clone()),
        });
        assert_eq!(cfg.role("writer").as_deref(), Some("w/n"));
        assert_eq!(cfg.role_meta("writer"), Some(meta));
        let disk = std::fs::read_to_string(&path).unwrap();
        assert!(disk.contains("[roles.writer]") && disk.contains("delegate = true"), "{disk}");

        app.on_overlay_event(OverlayEvent::DeleteRole { set: "main".into(), role: "writer".into() });
        let disk = std::fs::read_to_string(&path).unwrap();
        assert!(!disk.contains("writer") && !disk.contains("[roles"), "{disk}");
        assert_eq!((cfg.role("writer"), cfg.role_meta("writer")), (None, None));
        assert!(commands.try_recv().is_err(), "role edits never reach the session");
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn modelset_switches_creates_renames_and_deletes() {
        use mu_base::config::{Configs, models_path};
        use mu_base::model_config;
        let _guard = MODEL_CONFIG_TEST_LOCK.lock().unwrap();
        mu_testutil::home();
        let path = models_path();
        let text = "[sets.main]\ndefault = \"a/b\"\n";
        std::fs::write(&path, text).unwrap();
        let mut app = test_app(std::env::temp_dir().join("mu-unused-history.jsonl"));
        let cfg = Rc::new(Configs::from_models_toml(text));
        let gateway = mu_provider::provider::endpoint::GatewaySource::new(app.plugins.gateway_start());
        app.use_providers(Rc::new(test_providers(cfg.clone(), gateway)));
        let on_disk = || model_config::parse(&std::fs::read_to_string(&path).unwrap()).0;

        app.modelset_cmd("new openai");
        assert_eq!(cfg.active_set().as_deref(), Some("openai"));
        assert_eq!(cfg.role("default").as_deref(), Some("a/b"));

        app.host_call(
            crate::host::models::EDIT_ROLE,
            &crate::host::models::EditRole { set: "openai".into(), role: "smol".into(), model: Some("c/d".into()) },
            |_, r: Result<(), String>| r.unwrap(),
        );
        let disk = on_disk();
        assert_eq!(disk.sets["openai"]["smol"], "c/d");
        assert!(!disk.sets["main"].contains_key("smol"));

        app.modelset_cmd("main");
        assert_eq!(cfg.active_set().as_deref(), Some("main"));
        assert_eq!(cfg.role("smol"), None);

        app.modelset_cmd("");
        let Some(Overlay::ModelSet { pick }) = &app.overlay else { panic!("no set picker") };
        assert_eq!(pick.values(), &["main", "openai"]);
        app.overlay = None;

        app.modelset_cmd("delete main");
        assert!(transcript_text(&app).contains("`main` is the active model set"), "{}", transcript_text(&app));
        assert_eq!(cfg.set_names(), ["main", "openai"]);

        app.modelset_cmd("rename openai oa");
        assert_eq!(cfg.set_names(), ["main", "oa"]);
        assert_eq!(cfg.active_set().as_deref(), Some("main"));
        app.modelset_cmd("delete oa");
        assert_eq!(on_disk().sets.keys().collect::<Vec<_>>(), ["main"]);
        let _ = std::fs::remove_file(&path);
    }

    /// Answers every host call later, as a remote host does; the UI messages the answers become arrive
    /// on the returned receiver.
    fn deferred(app: &mut App) -> UnboundedReceiver<UiMsg> {
        let (ui_tx, ui_rx) = unbounded_channel();
        app.ui_tx = ui_tx;
        app.host = Rc::new(crate::host::Deferred(crate::host::local::LocalHost::new(
            app.services.providers.clone(),
            app.ctx.clone(),
            app.plugins.clone(),
            app.cwd.clone(),
        )));
        ui_rx
    }

    /// Carries the answers to the app until none has arrived for a while.
    async fn pump_all(app: &mut App, ui: &mut UnboundedReceiver<UiMsg>) {
        while let Ok(Some(m)) = tokio::time::timeout(std::time::Duration::from_millis(50), ui.recv()).await {
            app.on_ui(m);
        }
    }

    #[test]
    fn the_model_pickers_fill_from_a_deferred_host() {
        use mu_base::config::Configs;
        use mu_provider::provider::custom::{CustomModel, CustomProvider};
        use mu_provider::provider::endpoint::{GatewaySource, start_fn};
        let (mut app, _commands) = test_app_with_commands(std::env::temp_dir().join("mu-unused-history.jsonl"));
        let cfg = Rc::new(Configs::from_models_toml("[sets.main]\ndefault = \"a/old\"\n"));
        let gateway = GatewaySource::new(start_fn(|| async { Err::<(String, String), String>("no gateway".into()) }));
        app.use_providers(Rc::new(test_providers(cfg, gateway)));
        let modal = CustomProvider {
            models: std::collections::BTreeMap::from([("x/b".to_string(), CustomModel::default())]),
            discover: false,
            ..CustomProvider::new("http://127.0.0.1:9")
        };
        app.services.providers.custom_registry().add("modal", modal.validated("modal").unwrap()).unwrap();
        let mut ui = deferred(&mut app);
        in_local(async {
            app.request_models();
            assert!(app.overlay.is_none(), "the rows are not here before the host answers");
            pump_all(&mut app, &mut ui).await;
            let Some(Overlay::Model { pick, .. }) = &app.overlay else { panic!("no model picker") };
            assert!(pick.values().contains(&"@default".to_string()));
            assert!(pick.values().contains(&"modal/x/b".to_string()));
            assert!(transcript_text(&app).contains("gateway models unavailable"), "{}", transcript_text(&app));

            app.overlay = None;
            app.open_model_roles(Some("default".into()));
            assert!(app.overlay.is_none());
            pump_all(&mut app, &mut ui).await;
            assert!(matches!(app.overlay, Some(Overlay::ModelRoles { .. })));
            app.on_overlay_event(OverlayEvent::LoadRoleModels);
            pump_all(&mut app, &mut ui).await;
            let Some(Overlay::ModelRoles { st }) = &app.overlay else { panic!("editor gone") };
            assert!(st.select().highlighted().is_some(), "the list filled the sub-picker");
        });
    }

    #[test]
    fn saving_a_role_through_a_deferred_host_reloads_the_editor() {
        use mu_base::config::{Configs, models_path};
        let _guard = MODEL_CONFIG_TEST_LOCK.lock().unwrap();
        mu_testutil::home();
        let path = models_path();
        let text = "[sets.main]\ndefault = \"a/old:high\"\n";
        std::fs::write(&path, text).unwrap();
        let (mut app, mut commands) = test_app_with_commands(std::env::temp_dir().join("mu-unused-history.jsonl"));
        let cfg = Rc::new(Configs::from_models_toml(text));
        let gateway = mu_provider::provider::endpoint::GatewaySource::new(app.plugins.gateway_start());
        app.use_providers(Rc::new(test_providers(cfg.clone(), gateway)));
        let mut ui = deferred(&mut app);
        in_local(async {
            app.open_model_roles(Some("default".into()));
            pump_all(&mut app, &mut ui).await;
            app.on_overlay_event(OverlayEvent::SaveRole {
                set: "main".into(),
                role: "default".into(),
                value: Some("a/new:low".into()),
                meta: None,
            });
            assert!(!transcript_text(&app).contains("saved @default"), "nothing is said before the answer");
            pump_all(&mut app, &mut ui).await;
            assert_eq!(cfg.role("default").as_deref(), Some("a/new:low"));
            assert!(transcript_text(&app).contains("saved @default in set main"));
            assert!(matches!(app.overlay, Some(Overlay::ModelRoles { .. })));

            app.on_overlay_event(OverlayEvent::DeleteRole { set: "main".into(), role: "writer".into() });
            pump_all(&mut app, &mut ui).await;
            assert!(transcript_text(&app).contains("deleted @writer from set main"));
        });
        assert!(std::fs::read_to_string(&path).unwrap().contains("default = \"a/new:low\""));
        assert!(commands.try_recv().is_err());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn modelset_commands_work_through_a_deferred_host() {
        use mu_base::config::{Configs, models_path};
        let _guard = MODEL_CONFIG_TEST_LOCK.lock().unwrap();
        mu_testutil::home();
        let path = models_path();
        let text = "[sets.main]\ndefault = \"a/b\"\n";
        std::fs::write(&path, text).unwrap();
        let (mut app, mut commands) = test_app_with_commands(std::env::temp_dir().join("mu-unused-history.jsonl"));
        let cfg = Rc::new(Configs::from_models_toml(text));
        let gateway = mu_provider::provider::endpoint::GatewaySource::new(app.plugins.gateway_start());
        app.use_providers(Rc::new(test_providers(cfg.clone(), gateway)));
        let mut ui = deferred(&mut app);
        in_local(async {
            app.modelset_cmd("new openai");
            assert!(!transcript_text(&app).contains("created"), "nothing is said before the answer");
            pump_all(&mut app, &mut ui).await;
            assert_eq!(cfg.active_set().as_deref(), Some("openai"));
            assert!(transcript_text(&app).contains("model set openai created from main and active"));

            app.modelset_cmd("");
            pump_all(&mut app, &mut ui).await;
            let Some(Overlay::ModelSet { pick }) = &app.overlay else { panic!("no set picker") };
            assert_eq!(pick.values(), &["main", "openai"]);
            app.overlay = None;

            app.modelset_cmd("main");
            pump_all(&mut app, &mut ui).await;
            assert_eq!(cfg.active_set().as_deref(), Some("main"));
            assert!(matches!(commands.try_recv(), Ok(Command::SetModel(id)) if id == "@default"));
            assert!(transcript_text(&app).contains("model set main: @default = a/b"));

            app.modelset_cmd("delete main");
            app.modelset_cmd("rename openai oa");
            pump_all(&mut app, &mut ui).await;
            assert!(transcript_text(&app).contains("`main` is the active model set"));
            assert_eq!(cfg.set_names(), ["main", "oa"]);
            assert!(transcript_text(&app).contains("model set openai renamed to oa"));
            app.modelset_cmd("delete oa");
            pump_all(&mut app, &mut ui).await;
            assert_eq!(cfg.set_names(), ["main"]);
        });
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn plan_tiers_come_from_a_deferred_host_as_from_a_local_one() {
        let (mut app, _commands) = test_app_with_commands(std::env::temp_dir().join("mu-unused-history.jsonl"));
        let seen = Rc::new(std::cell::RefCell::new(Vec::new()));
        let record = |seen: &Rc<std::cell::RefCell<Vec<(String, String)>>>| {
            let seen = seen.clone();
            move |_: &mut App, tiers: Vec<crate::panels::plan_review::Tier>| {
                *seen.borrow_mut() = tiers.into_iter().map(|t| (t.role, t.detail)).collect();
            }
        };
        app.with_plan_tiers(record(&seen));
        let local = seen.borrow().clone();
        assert!(!local.is_empty());
        seen.borrow_mut().clear();
        let mut ui = deferred(&mut app);
        in_local(async {
            app.with_plan_tiers(record(&seen));
            assert!(seen.borrow().is_empty());
            pump_all(&mut app, &mut ui).await;
        });
        assert_eq!(*seen.borrow(), local);
    }
}
