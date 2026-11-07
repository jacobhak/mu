//! The custom-endpoint wizard: `/login custom`, or the "Add custom OpenAI-compatible endpoint" row.
//! The app shows each step of `custom::wizard` and runs the one model check it asks for. Only Save
//! writes (the definition, the key, then the listing); every other way out writes nothing and gives
//! the composer back its text. Typed keys stay in the composer's masked input and in the wizard's
//! draft, and are registered for redaction as soon as they are checked.

use super::*;
use crate::custom::wizard::{Answer, Destinations, Fetch, Probe, Saved, Step, Wizard};
use crate::widgets::select::{Item, Select};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use mu_provider::auth::store::resolve_entry;
use mu_provider::model::discovery::{DiscoverError, custom_fetch};
use mu_provider::provider::custom::{self, CustomProvider, SaveError};

/// The wizard in progress, and the composer text it displaced (put back when it ends).
pub(super) struct CustomFlow {
    /// Tells a late check result from this wizard's.
    id: u64,
    wizard: Wizard,
    draft: String,
}

impl App {
    pub(super) fn custom_wizard_active(&self) -> bool {
        self.flows.custom_wizard.is_some()
    }

    /// Whether the composer is locked to the wizard: it waits for a line of text, or a check runs.
    pub(super) fn custom_typing(&self) -> bool {
        self.flows.custom_wizard.as_ref().is_some_and(|f| f.wizard.awaits_text() || f.wizard.busy())
    }

    /// Whether a check is running; nothing typed or pasted goes anywhere until it answers.
    pub(super) fn custom_busy(&self) -> bool {
        self.flows.custom_wizard.as_ref().is_some_and(|f| f.wizard.busy())
    }

    /// Opens the wizard at its first step. One that is already open is left alone.
    pub(super) fn start_custom_wizard(&mut self) {
        if self.custom_wizard_active() {
            return;
        }
        let registry = self.services.providers.custom_registry();
        let names: Vec<String> = match registry.snapshot() {
            Ok(defs) => defs.into_keys().collect(),
            Err(e) => return self.error(&format!("{e:#}")),
        };
        let store = self.services.providers.store().clone();
        let dests =
            Destinations { providers: registry.path().display().to_string(), auth: store.path().display().to_string() };
        let taken = move |id: &str| names.iter().any(|n| n == id) || store.has(id);
        self.flows.custom_wizard_seq += 1;
        let id = self.flows.custom_wizard_seq;
        let draft = self.prompt.composer.take_secret();
        self.prompt.composer.set_private(true);
        self.prompt.composer.set_closed(true);
        self.flows.custom_wizard = Some(CustomFlow { id, wizard: Wizard::new(dests, taken), draft });
        self.custom_show();
    }

    /// Shows the wizard's current step: a line of text, a picker, or the check it asks for.
    fn custom_show(&mut self) {
        let Some(f) = self.flows.custom_wizard.as_mut() else { return };
        let (flow, notes, step) = (f.id, f.wizard.take_notes(), f.wizard.step());
        if !notes.is_empty() {
            self.notice(&notes.join("\n"));
        }
        // The previous step's picker is replaced or dropped, never left on screen.
        if matches!(self.overlay, Some(Overlay::CustomWizard { .. })) {
            self.overlay = None;
        }
        match step {
            Step::Text { prompt, secret, .. } => {
                // Clears the composer: each line starts empty, masked when it is a key.
                self.prompt.composer.set_masked(secret);
                self.notice(&format!("{prompt}   (Esc cancels)"));
            }
            Step::Pick { title, context, items } => {
                self.prompt.composer.set_masked(false);
                let items: Vec<Item> = items.into_iter().map(|i| Item { label: i.label, detail: i.detail }).collect();
                self.overlay = Some(Overlay::CustomWizard { sel: Select::new(&title, items).context(&context) });
            }
            Step::Fetch(probe) => self.custom_check(flow, probe),
            Step::Save(saved) => self.custom_save(*saved),
            Step::Cancelled => {
                self.end_custom_wizard();
            }
        }
    }

    /// Keys the wizard takes before the composer and the overlay. True when the key was used up here.
    pub(super) fn custom_key(&mut self, k: KeyEvent) -> bool {
        let ctrl_c = k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL);
        if k.code == KeyCode::Esc || ctrl_c {
            self.custom_cancel();
            return true;
        }
        let Some(f) = self.flows.custom_wizard.as_ref() else { return false };
        if f.wizard.busy() {
            return true;
        }
        if f.wizard.awaits_text() {
            let enter = k.code == KeyCode::Enter && !k.modifiers.intersects(KeyModifiers::SHIFT | KeyModifiers::ALT);
            if enter {
                let text = self.prompt.composer.secret_text();
                self.custom_answer(Answer::Text(text), true);
                return true;
            }
            return false;
        }
        // A picker step: its overlay takes the keys. Another flow that replaced it ends this one,
        // so the keys reach that flow and never the composer.
        if !matches!(self.overlay, Some(Overlay::CustomWizard { .. })) {
            self.custom_cancel();
        }
        false
    }

    /// Answers the current step. `from_text`: the answer came from the composer, which is cleared
    /// once accepted. A refused answer keeps what was typed, so it can be corrected.
    fn custom_answer(&mut self, a: Answer, from_text: bool) {
        let Some(f) = self.flows.custom_wizard.as_mut() else { return };
        match f.wizard.answer(a) {
            Ok(()) => {
                if from_text {
                    self.prompt.composer.take_secret();
                }
                self.custom_show();
            }
            Err(e) => self.error(&e),
        }
    }

    /// The overlay's choice: the row chosen, or `None` when it was dismissed.
    pub(super) fn on_custom_pick(&mut self, row: Option<usize>) {
        match row {
            Some(i) => self.custom_answer(Answer::Pick(i), false),
            None => self.custom_cancel(),
        }
    }

    /// A model check answered. Only the wizard that started it hears the answer.
    pub(super) fn on_custom_fetched(&mut self, flow: u64, r: Fetch) {
        if !self.flows.custom_wizard.as_ref().is_some_and(|f| f.id == flow) {
            return;
        }
        // Another flow has the screen now: this wizard yields to it rather than replace its picker.
        if self.overlay.is_some() {
            return self.custom_cancel();
        }
        let Some(f) = self.flows.custom_wizard.as_mut() else { return };
        match f.wizard.answer(Answer::Fetched(r)) {
            Ok(()) => self.custom_show(),
            Err(e) => self.error(&e),
        }
    }

    /// Esc or Ctrl-C: nothing is written.
    pub(super) fn custom_cancel(&mut self) {
        if self.end_custom_wizard() {
            self.notice("cancelled; nothing was saved");
        }
    }

    /// Ends the wizard without a notice, for a flow that opens over it.
    pub(super) fn leave_custom_wizard(&mut self) {
        self.end_custom_wizard();
    }

    /// Ends the wizard: its picker goes, and the composer gets its modes and its text back. Writes
    /// nothing. False when no wizard was open.
    fn end_custom_wizard(&mut self) -> bool {
        let Some(flow) = self.flows.custom_wizard.take() else { return false };
        if matches!(self.overlay, Some(Overlay::CustomWizard { .. })) {
            self.overlay = None;
        }
        self.unlock_composer();
        let rest = self.prompt.composer.text();
        let text = match (flow.draft.is_empty(), rest.trim().is_empty()) {
            (true, _) => rest,
            (false, true) => flow.draft,
            (false, false) => format!("{}\n\n{rest}", flow.draft),
        };
        self.prompt.composer.set_text(&text);
        true
    }

    /// Starts the check: the draft key is registered for redaction, then the endpoint's `/models` is
    /// read in the background. The answer comes back as `UiMsg::CustomFetched`.
    fn custom_check(&mut self, flow: u64, probe: Probe) {
        if let Some(key) = probe.credential.as_ref().and_then(|e| e.key.as_deref()) {
            mu_base::secrets::register_stored(&custom::env_hint(&probe.id), key);
        }
        self.notice(&format!("checking {}/models…", probe.base_url));
        let tx = self.ui_tx.clone();
        tokio::task::spawn_local(async move {
            let r = read_listing(&probe).await;
            let _ = tx.send(UiMsg::CustomFetched { flow, r });
        });
    }

    /// Only Save reaches here. The definition and key are written, then the listing is cached.
    fn custom_save(&mut self, saved: Saved) {
        self.end_custom_wizard();
        let Saved { id, provider, entry, listed } = saved;
        match self.services.save_custom(&id, provider, entry) {
            Ok(p) => {
                if let Some(rows) = listed {
                    self.services.commit_custom(&p, rows);
                }
                self.notice("Provider configured; use /switch to try it or /model to save a role.");
                self.custom_written();
            }
            Err(e) => {
                // A definition saved without its key still counts as configured.
                let written = matches!(e, SaveError::KeyNotSaved { .. });
                self.error(&e.to_string());
                if written {
                    self.custom_written();
                }
            }
        }
    }

    fn custom_written(&mut self) {
        self.refresh_commands();
        self.send(Command::RefreshModel);
    }
}

/// The endpoint's `/models` with the draft's credential. Nothing is written.
async fn read_listing(probe: &Probe) -> Fetch {
    let provider = match CustomProvider::new(&probe.base_url).validated(&probe.id) {
        Ok(p) => p,
        Err(e) => return Fetch::Failed(mu_base::secrets::redact_keys(&format!("{e:#}"))),
    };
    let key = match &probe.credential {
        Some(entry) => match resolve_entry(&probe.id, entry).await {
            Ok(r) => Some(r.key),
            Err(e) => return Fetch::Credential(mu_base::secrets::redact_keys(&format!("{e:#}"))),
        },
        None => None,
    };
    match custom_fetch(&provider, key.as_deref()).await {
        Ok(rows) => Fetch::Listed(rows),
        Err(DiscoverError::Rejected(m)) => Fetch::Rejected(m),
        Err(DiscoverError::Failed(m)) => Fetch::Failed(m),
    }
}

#[cfg(test)]
mod tests {
    use super::super::keys::tests::{in_local, press, test_providers, transcript_text};
    use super::super::services::Services;
    use super::*;
    use crate::text::line::line_text;
    use crate::widgets::overlay::{LoginTarget, OverlayEvent};
    use crossterm::event::KeyModifiers;
    use mu_base::config::Configs;
    use mu_base::events::Command;
    use mu_provider::provider::custom::{AuthMode, CustomModel, CustomProvider};
    use mu_provider::provider::endpoint::{GatewaySource, start_fn};
    use mu_provider::provider::providers::KeyTarget;
    use mu_testutil::Reply;
    use std::collections::BTreeMap;
    use std::rc::Rc;
    use tokio::sync::mpsc::UnboundedReceiver;

    /// A gateway that never starts: the picker must work without one.
    fn no_gateway() -> GatewaySource {
        GatewaySource::new(start_fn(|| async { Err::<(String, String), String>("no gateway in tests".into()) }))
    }

    fn history() -> PathBuf {
        std::env::temp_dir().join(format!("mu-custom-hist-{}", uuid::Uuid::new_v4())).join("history.jsonl")
    }

    /// An app with its own providers (and no gateway), a command receiver and a UI channel it reads.
    fn app_with(cfg: &str) -> (App, UnboundedReceiver<Command>, UnboundedReceiver<UiMsg>) {
        let (mut app, cmds) = test_app_with_commands(history());
        let cfg = Rc::new(Configs::from_models_toml(cfg));
        app.services = Services { providers: Rc::new(test_providers(cfg, no_gateway())) };
        let (ui_tx, ui_rx) = unbounded_channel();
        app.ui_tx = ui_tx;
        (app, cmds, ui_rx)
    }

    fn test_app_with_commands(h: PathBuf) -> (App, UnboundedReceiver<Command>) {
        super::super::keys::tests::test_app_with_commands(h)
    }

    /// The picker's text as drawn.
    fn screen(app: &App) -> String {
        let Some(sel) = app.overlay.as_ref().and_then(|o| o.select()) else { return String::new() };
        sel.render(120, 60).0.iter().map(line_text).collect::<Vec<_>>().join("\n")
    }

    fn type_text(app: &mut App, s: &str) {
        for c in s.chars() {
            app.on_terminal(press(KeyCode::Char(c), KeyModifiers::NONE));
        }
    }

    /// Types `s` and presses Enter.
    fn line(app: &mut App, s: &str) {
        type_text(app, s);
        app.on_terminal(press(KeyCode::Enter, KeyModifiers::NONE));
    }

    /// Id, label and base URL, to the credential picker.
    fn to_credentials(app: &mut App, base: &str) {
        app.login("custom");
        line(app, "modal");
        line(app, "");
        line(app, base);
    }

    /// Carries the background answers to the app until `done` holds on the screen.
    async fn settle(app: &mut App, ui: &mut UnboundedReceiver<UiMsg>, done: impl Fn(&App) -> bool) {
        for _ in 0..200 {
            if done(app) {
                return;
            }
            if let Ok(Some(m)) = tokio::time::timeout(Duration::from_millis(50), ui.recv()).await {
                app.on_ui(m);
            }
        }
        panic!("the wizard did not reach its step; screen:\n{}", screen(app));
    }

    /// A listing that says `/models` is up; the wizard's check is answered by the fake server.
    fn listing(body: &str) -> Reply {
        Reply::new(200, body)
    }

    const ONE_MODEL: &str = r#"{"data":[{"id":"org/model","max_model_len":65536,"max_completion_tokens":8192}]}"#;

    fn env_free_name() -> String {
        format!("MU_TEST_UNSET_{}", uuid::Uuid::new_v4().simple())
    }

    #[test]
    fn a_wizard_add_through_save_lists_its_models_and_changes_no_assignment() {
        let (mut app, mut cmds, mut ui) = app_with("[sets.main]\ndefault = \"a/old\"\n");
        in_local(async {
            let (base, _seen) = mu_testutil::fake_server(vec![listing(ONE_MODEL)]).await;
            to_credentials(&mut app, &base);
            assert!(app.custom_wizard_active());
            app.on_custom_pick(Some(3));
            settle(&mut app, &mut ui, |a| screen(a).contains("Choose a model")).await;
            app.on_custom_pick(Some(0));
            line(&mut app, "");
            line(&mut app, "");
            app.on_custom_pick(Some(0));
            app.on_custom_pick(Some(0));
            assert!(!app.custom_wizard_active() && app.overlay.is_none(), "saved, and the wizard is done");

            let saved = app.services.providers.custom_registry().get("modal").unwrap().expect("saved");
            assert_eq!(saved.auth, AuthMode::None);
            assert_eq!(saved.models["org/model"].context_window, Some(65536), "the listed limits are the default");
            assert!(
                transcript_text(&app).contains("Provider configured; use /switch to try it or /model to save a role.")
            );
            assert!(matches!(cmds.try_recv(), Ok(Command::RefreshModel)));
            assert!(cmds.try_recv().is_err(), "nothing selects a model or edits a role");
            assert_eq!(app.agent.status.model_id, "");
            assert_eq!(app.services.providers.cfg.role("default").as_deref(), Some("a/old"));

            // The discovered listing is cached, so both pickers show it.
            app.request_models();
            assert!(screen(&app).contains("modal/org/model"), "{}", screen(&app));
            app.overlay = None;
            app.open_model_roles(Some("default".into()));
            app.on_overlay_event(OverlayEvent::LoadRoleModels);
            assert!(screen(&app).contains("modal/org/model"), "{}", screen(&app));
        });
    }

    #[test]
    fn escape_at_every_step_writes_nothing_and_gives_the_composer_back() {
        // Each stage is reached from a fresh `/login custom`, then escape is pressed.
        type Stage = (&'static str, fn(&mut App));
        let stages: [Stage; 7] = [
            ("id", |_| {}),
            ("label", |a| line(a, "modal")),
            ("base", |a| {
                line(a, "modal");
                line(a, "")
            }),
            ("credential picker", |a| {
                line(a, "modal");
                line(a, "");
                line(a, "http://127.0.0.1:9")
            }),
            ("key", |a| {
                line(a, "modal");
                line(a, "");
                line(a, "http://127.0.0.1:9");
                a.on_custom_pick(Some(0));
                type_text(a, "typed-SECRET-01")
            }),
            ("command", |a| {
                line(a, "modal");
                line(a, "");
                line(a, "http://127.0.0.1:9");
                a.on_custom_pick(Some(2));
                type_text(a, "pass show modal")
            }),
            ("checking", |a| {
                line(a, "modal");
                line(a, "");
                line(a, "http://127.0.0.1:9");
                a.on_custom_pick(Some(3))
            }),
        ];
        in_local(async {
            for (name, reach) in stages {
                let (mut app, _cmds, _ui) = app_with("");
                app.prompt.composer.set_text("my draft");
                app.login("custom");
                assert_eq!(app.prompt.composer.text(), "", "{name}: the draft is taken aside while the wizard runs");
                reach(&mut app);
                app.on_terminal(press(KeyCode::Esc, KeyModifiers::NONE));
                assert!(!app.custom_wizard_active() && app.overlay.is_none(), "{name}");
                assert!(!app.typing(), "{name}: the composer is unlocked");
                assert_eq!(app.prompt.composer.text(), "my draft", "{name}: the draft is back");
                assert!(!app.services.providers.custom_registry().path().exists(), "{name}: no definition");
                assert!(app.services.providers.store().get("modal").is_none(), "{name}: no key");
                assert!(!app.prompt.composer.is_empty() && !transcript_text(&app).contains("SECRET"), "{name}");
            }
        });
    }

    #[test]
    fn escape_while_the_check_runs_writes_nothing_and_its_late_answer_is_ignored() {
        let (mut app, _cmds, mut ui) = app_with("");
        in_local(async {
            app.prompt.composer.set_text("my draft");
            to_credentials(&mut app, "http://127.0.0.1:9");
            app.on_custom_pick(Some(3));
            assert!(app.custom_busy(), "the check is running");
            app.on_terminal(press(KeyCode::Esc, KeyModifiers::NONE));
            assert!(!app.custom_wizard_active());
            assert_eq!(app.prompt.composer.text(), "my draft");

            // The refused connection's answer arrives after the escape: nothing happens.
            if let Ok(Some(m)) = tokio::time::timeout(Duration::from_secs(5), ui.recv()).await {
                app.on_ui(m);
            }
            assert!(app.overlay.is_none() && !app.custom_wizard_active());
            assert!(!app.services.providers.custom_registry().path().exists());
        });
    }

    #[test]
    fn stale_answers_cannot_save_or_reopen_a_wizard() {
        let (mut app, _cmds, _ui) = app_with("");
        in_local(async {
            to_credentials(&mut app, "http://127.0.0.1:9");
            app.on_custom_pick(Some(3));
            let old = app.flows.custom_wizard_seq;
            app.on_terminal(press(KeyCode::Esc, KeyModifiers::NONE));

            // A new wizard is at its first step when the old check's answer arrives.
            app.login("custom");
            let new = app.flows.custom_wizard_seq;
            assert_ne!(old, new);
            app.on_ui(UiMsg::CustomFetched { flow: old, r: Fetch::Listed(vec![]) });
            app.on_ui(UiMsg::CustomFetched { flow: old, r: Fetch::Rejected("late".into()) });
            assert_eq!(app.flows.custom_wizard.as_ref().map(|f| f.id), Some(new), "the new wizard survives");
            assert!(app.custom_typing() && !app.custom_busy(), "still at its id line");
            assert!(!app.services.providers.custom_registry().path().exists());
            assert!(!transcript_text(&app).contains("late"));
        });
    }

    #[test]
    fn a_refused_key_or_a_missing_variable_returns_to_the_credential_choice_and_saves_nothing() {
        let (mut app, _cmds, mut ui) = app_with("");
        in_local(async {
            let (base, _) = mu_testutil::fake_server(vec![Reply::new(401, "{}")]).await;
            to_credentials(&mut app, &base);
            app.on_custom_pick(Some(0));
            type_text(&mut app, "refused-0123456789");
            app.on_terminal(press(KeyCode::Enter, KeyModifiers::NONE));
            settle(&mut app, &mut ui, |a| screen(a).contains("How does the endpoint take its key?")).await;
            assert!(transcript_text(&app).contains("Nothing was saved"));
            assert!(!app.services.providers.custom_registry().path().exists());
            app.on_custom_pick(None);

            let (mut app, _cmds, mut ui) = app_with("");
            let unset = env_free_name();
            to_credentials(&mut app, "http://127.0.0.1:9");
            app.on_custom_pick(Some(1));
            line(&mut app, &unset);
            settle(&mut app, &mut ui, |a| screen(a).contains("How does the endpoint take its key?")).await;
            assert!(transcript_text(&app).contains(&unset), "the variable is named");
            assert!(app.services.providers.store().get("modal").is_none());
        });
    }

    #[test]
    fn a_listing_that_does_not_read_is_a_discovery_failure_with_a_manual_model() {
        let (mut app, _cmds, mut ui) = app_with("");
        in_local(async {
            let (base, _) = mu_testutil::fake_server(vec![listing(r#"{"data":[]}"#)]).await;
            to_credentials(&mut app, &base);
            app.on_custom_pick(Some(3));
            settle(&mut app, &mut ui, |a| screen(a).contains("did not list its models")).await;
            app.on_custom_pick(Some(2));
            line(&mut app, "org/typed");
            line(&mut app, "");
            line(&mut app, "");
            app.on_custom_pick(Some(0));
            app.on_custom_pick(Some(0));
            let saved = app.services.providers.custom_registry().get("modal").unwrap().expect("saved");
            assert!(saved.models.contains_key("org/typed"));
            assert!(app.services.providers.direct_models().unwrap().iter().any(|m| m.id == "modal/org/typed"));
        });
    }

    #[test]
    fn literal_keys_stay_out_of_the_transcript_history_prediction_and_previews() {
        let (mut app, _cmds, mut ui) = app_with("");
        let key = "modalkey-SECRET-0123456789";
        in_local(async {
            let (base, _) = mu_testutil::fake_server(vec![listing(ONE_MODEL)]).await;
            to_credentials(&mut app, &base);
            app.on_custom_pick(Some(0));
            type_text(&mut app, key);
            app.on_terminal(Event::Paste("-PASTED-SECRET-99".into()));
            assert_eq!(app.prompt.composer.text(), format!("{key}-PASTED-SECRET-99"), "the masked input holds it");
            assert_eq!(app.prompt.ghost_seq, 0, "no prediction is asked for while a key is typed");
            app.on_terminal(press(KeyCode::Enter, KeyModifiers::NONE));
            assert!(app.prompt.composer.is_empty(), "the typed key leaves the composer");
            let full = format!("{key}-PASTED-SECRET-99");
            assert!(!mu_base::secrets::redact_keys(&full).contains(&full), "the key is registered for redaction");
            settle(&mut app, &mut ui, |a| screen(a).contains("Choose a model")).await;
            let shown = transcript_text(&app);
            assert!(!shown.contains("SECRET"), "{shown}");
        });
    }

    #[test]
    fn a_command_is_previewed_by_its_program_only() {
        let (mut app, _cmds, mut ui) = app_with("");
        in_local(async {
            let (base, _) = mu_testutil::fake_server(vec![listing(ONE_MODEL)]).await;
            to_credentials(&mut app, &base);
            app.on_custom_pick(Some(2));
            line(&mut app, "echo modalkey-ARG-0123456789");
            settle(&mut app, &mut ui, |a| screen(a).contains("Choose a model")).await;
            app.on_custom_pick(Some(0));
            line(&mut app, "");
            line(&mut app, "");
            app.on_custom_pick(Some(0));
            let review = screen(&app);
            assert!(review.contains("Auth: !echo …"), "{review}");
            assert!(!review.contains("modalkey-ARG") && !review.contains("0123456789"), "{review}");
            assert!(!transcript_text(&app).contains("modalkey-ARG"));
        });
    }

    #[test]
    fn the_picker_opens_at_once_with_a_configured_model_and_no_gateway() {
        let (mut app, _cmds, mut ui) = app_with("");
        let modal = CustomProvider {
            models: BTreeMap::from([("x/b".to_string(), CustomModel::default())]),
            discover: false,
            ..CustomProvider::new("http://127.0.0.1:9")
        };
        app.services.providers.custom_registry().add("modal", modal.validated("modal").unwrap()).unwrap();
        in_local(async {
            app.request_models();
            // Before any network answer: the row is already listed.
            assert!(matches!(&app.overlay, Some(Overlay::Model { ids, .. }) if ids.contains(&"modal/x/b".to_string())));
            settle(&mut app, &mut ui, |a| transcript_text(a).contains("gateway models unavailable")).await;
            assert!(screen(&app).contains("modal/x/b"), "a failed refresh keeps the rows");
            assert!(matches!(&app.overlay, Some(Overlay::Model { .. })));
        });
    }

    #[test]
    fn a_malformed_providers_file_is_an_error_in_the_picker_not_an_empty_list() {
        let (mut app, _cmds, _ui) = app_with("");
        let path = app.services.providers.custom_registry().path().to_path_buf();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "[providers.modal\nbase_url = ").unwrap();
        in_local(async { app.request_models() });
        let shown = screen(&app);
        assert!(shown.contains("providers.toml"), "{shown}");
        assert!(shown.contains("fix"), "{shown}");
        assert!(matches!(&app.overlay, Some(Overlay::Model { .. })), "the picker still opens");
        let (items, targets) = app.api_key_rows();
        assert!(targets.contains(&LoginTarget::CustomProvider), "the add row needs no valid file");
        assert!(items.len() >= 2);
        assert!(transcript_text(&app).contains("providers.toml"));
    }

    #[test]
    fn the_login_list_shows_custom_endpoints_with_their_key_state() {
        let (mut app, _cmds, _ui) = app_with("");
        let registry = app.services.providers.custom_registry().clone();
        registry.add("modal", CustomProvider::new("http://127.0.0.1:9").validated("modal").unwrap()).unwrap();
        let lab = CustomProvider { auth: AuthMode::None, ..CustomProvider::new("http://127.0.0.1:8") };
        registry.add("lab", lab.validated("lab").unwrap()).unwrap();
        let (items, targets) = app.api_key_rows();
        let row = |t: &LoginTarget| items.iter().zip(&targets).find(|(_, x)| *x == t).map(|(i, _)| i.detail.clone());
        assert_eq!(row(&LoginTarget::ApiKey("modal".into())).as_deref(), Some("key missing"));
        assert_eq!(row(&LoginTarget::ApiKey("lab".into())).as_deref(), Some("no auth"));
        assert!(targets.last() == Some(&LoginTarget::CustomProvider), "the add row comes last");
        assert_eq!(items.last().map(|i| i.label.as_str()), Some("Add custom OpenAI-compatible endpoint"));

        // A keyless endpoint takes no key: its row says so instead of opening a key flow.
        app.login("lab");
        assert!(transcript_text(&app).contains("takes no key"));
        assert!(app.overlay.is_none());
        app.login("modal");
        assert!(matches!(&app.overlay, Some(Overlay::KeyChoice { provider, .. }) if provider == "modal"));
    }

    #[test]
    fn completion_names_custom_ids_and_the_custom_shortcut() {
        let (mut app, _cmds, _ui) = app_with("");
        app.services
            .providers
            .custom_registry()
            .add("modal", CustomProvider::new("http://127.0.0.1:9").validated("modal").unwrap())
            .unwrap();
        app.refresh_commands();
        let arg_list = |name: &str| -> Vec<String> {
            app.prompt.commands.iter().find(|e| e.name == name).map(|e| e.args.clone()).unwrap_or_default()
        };
        let login = arg_list("login");
        assert!(login.contains(&"custom".to_string()) && login.contains(&"modal".to_string()), "{login:?}");
        let logout = arg_list("logout");
        assert!(logout.contains(&"modal".to_string()) && !logout.contains(&"custom".to_string()), "{logout:?}");
    }

    #[test]
    fn a_picker_replaced_by_another_flow_ends_the_wizard_and_keeps_its_keys() {
        let (mut app, mut cmds, _ui) = app_with("");
        to_credentials(&mut app, "http://127.0.0.1:9");
        assert!(matches!(app.overlay, Some(Overlay::CustomWizard { .. })));
        app.overlay = Some(Overlay::Logout { sel: Select::new("t", vec![]), providers: vec![] });
        app.on_terminal(press(KeyCode::Enter, KeyModifiers::NONE));
        assert!(!app.custom_wizard_active(), "the wizard ends");
        assert!(transcript_text(&app).contains("cancelled; nothing was saved"));
        assert!(cmds.try_recv().is_err(), "Enter never submits the composer's text");
    }

    #[test]
    fn a_check_answering_under_another_picker_ends_the_wizard_without_replacing_it() {
        let (mut app, _cmds, _ui) = app_with("");
        in_local(async {
            to_credentials(&mut app, "http://127.0.0.1:9");
            app.on_custom_pick(Some(3));
            app.overlay = Some(Overlay::Logout { sel: Select::new("t", vec![]), providers: vec![] });
            let flow = app.flows.custom_wizard_seq;
            app.on_ui(UiMsg::CustomFetched { flow, r: Fetch::Listed(vec![]) });
            assert!(!app.custom_wizard_active());
            assert!(matches!(app.overlay, Some(Overlay::Logout { .. })), "the other picker stays");
        });
    }

    #[test]
    fn another_flow_takes_the_wizards_place() {
        let (mut app, _cmds, _ui) = app_with("");
        app.prompt.composer.set_text("kept");
        app.login("custom");
        assert!(app.custom_wizard_active());
        app.start_key_flow(KeyTarget::builtin("anthropic").unwrap(), None, None);
        assert!(!app.custom_wizard_active(), "the key flow replaces it");
        assert_eq!(app.prompt.composer.text(), "kept", "the draft comes back");
    }
}
