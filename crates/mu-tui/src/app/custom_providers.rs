//! The custom-endpoint panel: `/login custom`, or the `/login` row that adds one.
//! One screen of fields reads like the `providers.toml` table it writes; the app shows each step of
//! `custom::panel`, takes the key in the masked composer, runs the one `/models` check it asks for
//! and carries out its Save. Only Save writes (the definition, the key, then the listing); every
//! other way out writes nothing and gives the composer back its text. A typed key stays in the
//! composer's masked input and in the panel's draft, and is registered for redaction as soon as
//! it is checked.

use super::*;
use crate::custom::panel::{self, Action, Answer, Fetch, Probe, Saved, Step};
use crate::host::custom::{CUSTOM_ENV, CUSTOM_FETCH, CUSTOM_GET, CUSTOM_OPEN, CUSTOM_SAVE, Edit, Open, Outcome};
use crate::widgets::overlay::PanelPick;
use crossterm::event::{KeyCode, KeyEvent};
use mu_provider::provider::custom;

/// The panel in progress, and the composer text it displaced (put back when it ends).
pub(super) struct CustomFlow {
    /// Tells a late check result from this panel's.
    id: u64,
    panel: panel::Panel,
    draft: String,
    /// A `/models` read or a host question is running; keys wait for its answer.
    checking: bool,
}

impl App {
    pub(super) fn custom_wizard_active(&self) -> bool {
        self.flows.custom().is_some()
    }

    /// Whether the composer is locked to the panel: it takes the key, or a check runs.
    pub(super) fn custom_typing(&self) -> bool {
        self.flows.custom().is_some_and(|f| f.panel.awaits_key() || f.checking)
    }

    /// Whether a check is running; nothing typed or pasted goes anywhere until it answers.
    pub(super) fn custom_busy(&self) -> bool {
        self.flows.custom().is_some_and(|f| f.checking)
    }

    /// Opens the panel at its fields. One that is already open is left alone.
    pub(super) fn start_custom_wizard(&mut self) {
        if self.custom_wizard_active() {
            return;
        }
        self.host_call(CUSTOM_OPEN, &(), |app, r: Result<Open, String>| match r {
            Ok(Open { names, keys, dests }) => {
                let taken = move |id: &str| names.iter().chain(&keys).any(|n| n == id);
                app.open_custom_panel(panel::Panel::new(dests, taken));
            }
            Err(e) => app.error(&e),
        });
    }

    /// Opens the panel on an existing endpoint: its definition, models and reasoning as saved, and
    /// the key it takes now. Save replaces the definition; the key is written only when it is chosen.
    pub(super) fn start_custom_edit(&mut self, id: &str) {
        let id = id.to_string();
        self.host_call(CUSTOM_GET, &id.clone(), move |app, r: Result<Edit, String>| match r {
            Ok(Edit { def: Some(mut def), entry, dests }) => {
                // The id is the table's key, not a field, so it does not travel with the definition.
                def.id = id;
                app.open_custom_panel(panel::Panel::edited(dests, &def, entry));
            }
            Ok(Edit { def: None, dests, .. }) => app.error(&format!(
                "no custom endpoint `{id}` in {}; `+ Add a custom endpoint…` starts a new one",
                dests.providers
            )),
            Err(e) => app.error(&e),
        });
    }

    /// Shows `panel` as the flow on screen, taking the composer's draft aside until it ends.
    fn open_custom_panel(&mut self, panel: panel::Panel) {
        self.leave_custom_wizard();
        self.flows.custom_wizard_seq += 1;
        let id = self.flows.custom_wizard_seq;
        let draft = self.prompt.composer.take_secret();
        self.prompt.composer.set_private(true);
        self.prompt.composer.set_closed(true);
        self.flows.begin(Active::Custom(Box::new(CustomFlow { id, panel, draft, checking: false })));
        self.custom_show();
    }

    /// Shows the panel's current step.
    fn custom_show(&mut self) {
        if let Some(name) = self.flows.custom().and_then(|f| f.panel.env_wanted()) {
            return self.custom_ask_env(name);
        }
        self.custom_notes();
        // The previous step's screen is replaced or dropped, never left on display.
        if matches!(self.overlay, Some(Overlay::Custom { .. })) {
            self.overlay = None;
        }
        let Some(f) = self.flows.custom() else { return };
        let step = f.panel.step();
        match step {
            Step::Pick(sel) => {
                self.prompt.composer.set_masked(false);
                self.open_overlay(Overlay::Custom { sel: *sel });
            }
            Step::Key => {
                self.prompt.composer.set_masked(true);
                self.notice("paste the key and press Enter (shown as bullets; Esc goes back)");
            }
        }
    }

    /// Asks the host whether the variable the auth screen names is set, then shows the screen.
    fn custom_ask_env(&mut self, name: String) {
        let Some(flow) = self.flows.custom_mut().map(|f| {
            f.checking = true;
            f.id
        }) else {
            return;
        };
        self.host_call(CUSTOM_ENV, &name.clone(), move |app, r: Result<bool, String>| {
            let Some(f) = app.flows.custom_mut().filter(|f| f.id == flow) else { return };
            f.checking = false;
            f.panel.env_known(&name, r.unwrap_or(false));
            app.custom_show();
        });
    }

    /// Prints what the panel has to say; it is drained whenever the panel moves on.
    fn custom_notes(&mut self) {
        let Some(f) = self.flows.custom_mut() else { return };
        let notes = f.panel.take_notes();
        if !notes.is_empty() {
            self.notice(&notes.join("\n"));
        }
    }

    /// Keys the panel takes before the composer and the overlay. True when the key was used up here.
    pub(super) fn custom_key(&mut self, k: KeyEvent) -> bool {
        let Some(f) = self.flows.custom() else { return false };
        // A check is running: Esc gives up on it, and its late answer is ignored.
        if f.checking {
            if k.code == KeyCode::Esc {
                self.custom_cancel();
            }
            return true;
        }
        if f.panel.awaits_key() {
            if k.code == KeyCode::Esc {
                self.custom_key_cancelled();
                return true;
            }
            let enter = k.code == KeyCode::Enter && !crate::widgets::editor::adds_newline(k.modifiers);
            if enter {
                let text = self.prompt.composer.secret_text();
                self.custom_key_typed(&text);
                return true;
            }
            return false;
        }
        // Esc in a field's editor leaves the field, rather than a list of one empty row.
        if k.code == KeyCode::Esc
            && let Some(Overlay::Custom { sel }) = &self.overlay
            && sel.is_editing()
        {
            self.overlay = None;
            self.custom_answer(Answer::Back);
            return true;
        }
        // A picker step: its overlay takes the keys. Another flow that replaced it ends this one,
        // so the keys reach that flow and never the composer.
        if !matches!(self.overlay, Some(Overlay::Custom { .. })) {
            self.custom_cancel();
        }
        false
    }

    /// The key or command the panel asked for: Enter in the masked composer.
    fn custom_key_typed(&mut self, text: &str) {
        let Some(f) = self.flows.custom_mut() else { return };
        match f.panel.key_typed(text) {
            Ok(()) => {
                self.prompt.composer.take_secret();
                self.prompt.composer.set_masked(false);
                self.custom_show();
            }
            // The key stays in the composer, so it can be corrected.
            Err(e) => self.error(&e),
        }
    }

    /// Esc while the key is typed: back to the choice of how the endpoint takes one.
    fn custom_key_cancelled(&mut self) {
        self.prompt.composer.take_secret();
        self.prompt.composer.set_masked(false);
        self.custom_answer(Answer::Back);
    }

    /// The overlay's answer: the row chosen, the text typed in a field, or neither when dismissed.
    pub(super) fn on_custom_pick(&mut self, pick: PanelPick) {
        let event = match pick {
            PanelPick::Row(Some(i)) => Answer::Pick(i),
            PanelPick::Text(text) => Answer::Text(text),
            PanelPick::Row(None) => Answer::Back,
        };
        self.custom_answer(event);
    }

    /// Feeds one answer to the panel and carries out what it asks for.
    fn custom_answer(&mut self, a: Answer) {
        let Some(f) = self.flows.custom_mut() else { return };
        let action = f.panel.answer(a);
        match action {
            Action::Pending => self.custom_show(),
            Action::Close => self.custom_cancel(),
            Action::Refused(m) => {
                // The panel keeps the field open on what was typed.
                self.custom_show();
                self.error(&m);
            }
            Action::Fetch(probe) => {
                self.custom_show();
                self.custom_check(probe);
            }
            Action::Save(saved) => self.custom_save(*saved),
        }
    }

    /// A `/models` check answered. Only the panel that started it hears the answer.
    pub(super) fn on_custom_fetched(&mut self, flow: u64, r: Fetch) {
        let Some(f) = self.flows.custom_mut().filter(|f| f.id == flow) else { return };
        f.checking = false;
        let action = f.panel.fetched(r);
        match action {
            Action::Fetch(probe) => self.custom_check(probe),
            _ => self.custom_show(),
        }
    }

    /// Esc: nothing is written.
    pub(super) fn custom_cancel(&mut self) {
        if self.end_custom_wizard() {
            self.notice("cancelled; nothing was saved");
        }
    }

    /// Ends the panel without a notice, for a flow that opens over it.
    pub(super) fn leave_custom_wizard(&mut self) {
        self.end_custom_wizard();
    }

    /// Ends the panel: its screen goes, and the composer gets its modes and its text back. Writes
    /// nothing. False when no panel was open.
    fn end_custom_wizard(&mut self) -> bool {
        let Some(flow) = self.flows.end_custom() else { return false };
        if matches!(self.overlay, Some(Overlay::Custom { .. })) {
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

    /// Starts the check: the draft key is registered for redaction, then the endpoint's `/models`
    /// is read in the background. The answer comes back as `UiMsg::CustomFetched`.
    fn custom_check(&mut self, probe: Probe) {
        let Some(flow) = self.flows.custom_mut().map(|f| {
            f.checking = true;
            f.id
        }) else {
            return;
        };
        if let Some(key) = probe.credential.as_ref().and_then(|e| e.key.as_deref()) {
            mu_base::secrets::register_stored(&custom::env_hint(&probe.id), key);
        }
        self.notice(&format!("checking {}/models…", probe.base_url));
        self.host_call(CUSTOM_FETCH, &probe, move |app, r: Result<Fetch, String>| {
            app.on_custom_fetched(flow, r.unwrap_or_else(Fetch::Failed));
        });
    }

    /// Only Save reaches here. The definition and key are written, then the listing is cached.
    fn custom_save(&mut self, saved: Saved) {
        self.end_custom_wizard();
        let editing = saved.editing;
        self.host_call(CUSTOM_SAVE, &saved, move |app, r: Result<Outcome, String>| match r {
            Ok(Outcome::Saved) => {
                app.notice(if editing {
                    "Endpoint updated; use /switch to try it or /model to save a role."
                } else {
                    "Provider configured; use /switch to try it or /model to save a role."
                });
                app.custom_written();
            }
            Ok(Outcome::Failed { message, written }) => {
                // A definition saved without its key still counts as configured.
                app.error(&message);
                if written {
                    app.custom_written();
                }
            }
            Err(e) => app.error(&e),
        });
    }

    fn custom_written(&mut self) {
        self.refresh_commands();
        self.send(Command::RefreshModel);
    }
}

#[cfg(test)]
mod tests {
    use super::super::keys::tests::{in_local, press, test_providers, transcript_text};
    use super::*;
    use crate::text::line::line_text;
    use crate::widgets::overlay::{LoginTarget, OverlayEvent};
    use crossterm::event::KeyModifiers;
    use mu_base::config::Configs;
    use mu_base::events::Command;
    use mu_provider::provider::custom::{AuthMode, CustomModel, CustomProvider};
    use mu_provider::provider::endpoint::{GatewaySource, start_fn};
    use mu_testutil::Reply;
    use std::collections::BTreeMap;
    use std::rc::Rc;
    use tokio::sync::mpsc::UnboundedReceiver;

    /// A gateway that never starts: the panel must work without one.
    fn no_gateway() -> GatewaySource {
        GatewaySource::new(start_fn(|| async { Err::<(String, String), String>("no gateway in tests".into()) }))
    }

    fn history() -> PathBuf {
        std::env::temp_dir().join(format!("mu-custom-hist-{}", uuid::Uuid::new_v4())).join("history.jsonl")
    }

    /// An app with its own providers (and no gateway), a command receiver and a UI channel it reads.
    fn app_with(cfg: &str) -> (App, UnboundedReceiver<Command>, UnboundedReceiver<UiMsg>) {
        let (mut app, cmds) = super::super::keys::tests::test_app_with_commands(history());
        let cfg = Rc::new(Configs::from_models_toml(cfg));
        app.use_providers(Rc::new(test_providers(cfg, no_gateway())));
        let (ui_tx, ui_rx) = unbounded_channel();
        app.ui_tx = ui_tx;
        (app, cmds, ui_rx)
    }

    /// The picker's text as drawn.
    fn screen(app: &App) -> String {
        let Some(sel) = app.overlay.as_ref().and_then(|o| o.select()) else { return String::new() };
        sel.render(120, 60).0.iter().map(line_text).collect::<Vec<_>>().join("\n")
    }

    /// The row at `index` of the screen on show, chosen with the arrow keys. A screen may open with
    /// another row highlighted, so the arrows move until this one is.
    fn choose(app: &mut App, index: usize) {
        for _ in 0..40 {
            let at = app.overlay.as_ref().and_then(|o| o.select()).and_then(|s| s.highlighted());
            if at == Some(index) {
                break;
            }
            app.on_terminal(press(KeyCode::Down, KeyModifiers::NONE));
        }
        app.on_terminal(press(KeyCode::Enter, KeyModifiers::NONE));
    }

    fn type_text(app: &mut App, s: &str) {
        for c in s.chars() {
            app.on_terminal(press(KeyCode::Char(c), KeyModifiers::NONE));
        }
    }

    /// Types `s` into the open field and presses Enter.
    fn line(app: &mut App, s: &str) {
        type_text(app, s);
        app.on_terminal(press(KeyCode::Enter, KeyModifiers::NONE));
    }

    fn esc(app: &mut App) {
        app.on_terminal(press(KeyCode::Esc, KeyModifiers::NONE));
    }

    /// An endpoint with an id, a base URL and no key, at the panel's first screen.
    fn filled(app: &mut App, base: &str) {
        app.login("custom");
        choose(app, 0);
        line(app, "modal");
        choose(app, 2);
        line(app, base);
        choose(app, 3);
        choose(app, 3); // No authentication
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
        panic!("the panel did not reach its screen; screen:\n{}", screen(app));
    }

    const ONE_MODEL: &str = r#"{"data":[{"id":"org/model","max_model_len":65536,"max_completion_tokens":8192}]}"#;

    fn listing(body: &str) -> Reply {
        Reply::new(200, body)
    }

    #[test]
    fn a_panel_add_through_save_lists_its_models_and_changes_no_assignment() {
        let (mut app, mut cmds, mut ui) = app_with("[sets.main]\ndefault = \"a/old\"\n");
        in_local(async {
            let (base, _seen) = mu_testutil::fake_server(vec![listing(ONE_MODEL)]).await;
            filled(&mut app, &base);
            choose(&mut app, 4); // Models: reads the endpoint
            settle(&mut app, &mut ui, |a| screen(a).contains("org/model")).await;
            choose(&mut app, 0); // the model, with the limits the listing reported
            choose(&mut app, 4); // Add it (preselected on the limits screen)
            esc(&mut app); // back to the fields
            choose(&mut app, 5); // Save
            assert!(
                !app.custom_wizard_active() && app.overlay.is_none(),
                "saved, and the panel is done; screen:\n{}\n{}",
                screen(&app),
                transcript_text(&app)
            );

            let saved = app.services.providers.custom_registry().get("modal").unwrap().expect("saved");
            assert_eq!(saved.auth, AuthMode::None);
            assert_eq!(saved.models["org/model"].context_window, Some(65536), "the listed limits");
            assert_eq!(
                saved.models["org/model"].efforts,
                ["low", "medium", "high"],
                "an added model gets the effort ladder"
            );
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

    /// Esc `exits` times: each goes back a screen, and the last one closes the panel.
    fn escape_from(app: &mut App, exits: usize) {
        for _ in 0..exits {
            esc(app);
        }
    }

    /// Nothing was written, and the composer is as it was.
    fn assert_nothing_written(app: &App, draft: &str, name: &str) {
        assert!(!app.custom_wizard_active() && app.overlay.is_none(), "{name}");
        assert!(!app.typing(), "{name}: the composer is unlocked");
        assert_eq!(app.prompt.composer.text(), draft, "{name}: the draft is back");
        assert!(!app.services.providers.custom_registry().path().exists(), "{name}: no definition");
        assert!(app.services.providers.store().get("modal").is_none(), "{name}: no key");
        assert!(!transcript_text(app).contains("SECRET"), "{name}");
    }

    #[test]
    fn escape_at_every_screen_writes_nothing_and_gives_the_composer_back() {
        // Each stage is reached from a fresh `/login custom`, and leaves it with Esc.
        type Stage = (&'static str, fn(&mut App), usize);
        let stages: [Stage; 4] = [
            ("fields", |_| {}, 1),
            ("a field", |a| choose(a, 0), 2),
            ("the auth screen", |a| choose(a, 3), 2),
            (
                "the key",
                |a| {
                    choose(a, 3);
                    choose(a, 0);
                    type_text(a, "typed-SECRET-01")
                },
                2,
            ),
        ];
        in_local(async {
            for (name, reach, exits) in stages {
                let (mut app, _cmds, _ui) = app_with("");
                app.prompt.composer.set_text("my draft");
                app.login("custom");
                assert_eq!(app.prompt.composer.text(), "", "{name}: the draft is taken aside");
                reach(&mut app);
                escape_from(&mut app, exits);
                assert_nothing_written(&app, "my draft", name);
            }
        });
    }

    #[test]
    fn escape_after_the_listing_leaves_the_models_and_limits_screens_without_writing() {
        in_local(async {
            // The models screen, once the endpoint's listing is on it.
            let (mut app, _cmds, mut ui) = app_with("");
            let (base, _) = mu_testutil::fake_server(vec![listing(ONE_MODEL)]).await;
            app.prompt.composer.set_text("my draft");
            filled(&mut app, &base);
            choose(&mut app, 4); // Models reads the endpoint
            settle(&mut app, &mut ui, |a| screen(a).contains("org/model")).await;
            escape_from(&mut app, 2); // models → fields → out
            assert_nothing_written(&app, "my draft", "the models");

            // A model's limits, one screen further in.
            let (mut app, _cmds, mut ui) = app_with("");
            let (base, _) = mu_testutil::fake_server(vec![listing(ONE_MODEL)]).await;
            app.prompt.composer.set_text("my draft");
            filled(&mut app, &base);
            choose(&mut app, 4);
            settle(&mut app, &mut ui, |a| screen(a).contains("org/model")).await;
            choose(&mut app, 0); // the model, with its limits
            assert!(screen(&app).contains("Limits of org/model"), "{}", screen(&app));
            escape_from(&mut app, 3); // limits → models → fields → out
            assert_nothing_written(&app, "my draft", "a model's limits");
        });
    }

    #[test]
    fn ctrl_c_no_longer_leaves_the_panel_but_arms_the_exit() {
        // Esc is what leaves the panel; Ctrl-C belongs to the session's exit.
        in_local(async {
            let (mut app, _cmds, _ui) = app_with("");
            app.prompt.composer.set_text("my draft");
            app.login("custom");
            choose(&mut app, 0);
            app.on_terminal(press(KeyCode::Char('c'), KeyModifiers::CONTROL));
            assert!(app.custom_wizard_active(), "the panel stays open");
            assert!(app.ctrl_c_armed, "the exit gesture is armed");
            assert!(!app.services.providers.custom_registry().path().exists(), "no definition is written");
            assert!(app.services.providers.store().get("modal").is_none(), "no key is stored");
        });
    }

    #[test]
    fn escape_while_the_check_runs_writes_nothing_and_its_late_answer_is_ignored() {
        let (mut app, _cmds, mut ui) = app_with("");
        in_local(async {
            app.prompt.composer.set_text("my draft");
            filled(&mut app, "http://127.0.0.1:9");
            choose(&mut app, 4);
            assert!(app.custom_busy(), "the check is running");
            esc(&mut app);
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
    fn stale_answers_cannot_save_or_reopen_a_panel() {
        let (mut app, _cmds, _ui) = app_with("");
        in_local(async {
            filled(&mut app, "http://127.0.0.1:9");
            choose(&mut app, 4);
            let old = app.flows.custom_wizard_seq;
            esc(&mut app);

            // A new panel is at its first screen when the old check's answer arrives.
            app.login("custom");
            let new = app.flows.custom_wizard_seq;
            assert_ne!(old, new);
            app.on_custom_fetched(old, Fetch::Listed(vec![]));
            app.on_custom_fetched(old, Fetch::Rejected("late".into()));
            assert_eq!(app.flows.custom().map(|f| f.id), Some(new), "the new panel survives");
            assert!(app.overlay.is_some() && !app.custom_typing(), "still at its fields");
            assert!(!app.services.providers.custom_registry().path().exists());
            assert!(!transcript_text(&app).contains("late"));
        });
    }

    #[test]
    fn a_refused_key_or_a_missing_variable_returns_to_the_choice_and_saves_nothing() {
        let (mut app, _cmds, mut ui) = app_with("");
        in_local(async {
            let (base, _) = mu_testutil::fake_server(vec![Reply::new(401, "{}")]).await;
            app.login("custom");
            choose(&mut app, 0);
            line(&mut app, "modal");
            choose(&mut app, 2);
            line(&mut app, &base);
            choose(&mut app, 3);
            choose(&mut app, 0); // Paste a key
            line(&mut app, "refused-0123456789");
            choose(&mut app, 4); // Models: the check is refused
            settle(&mut app, &mut ui, |a| screen(a).contains("How does the endpoint take its key?")).await;
            assert!(transcript_text(&app).contains("Nothing was saved"));
            assert!(screen(&app).contains("HTTP 401"), "{}", screen(&app));
            assert!(!app.services.providers.custom_registry().path().exists());
            esc(&mut app);
            esc(&mut app);

            let (mut app, _cmds, mut ui) = app_with("");
            let unset = format!("MU_TEST_UNSET_{}", uuid::Uuid::new_v4().simple());
            filled(&mut app, "http://127.0.0.1:9");
            choose(&mut app, 3); // Key: another way to give one
            choose(&mut app, 1); // an environment variable
            line(&mut app, &unset);
            choose(&mut app, 4);
            settle(&mut app, &mut ui, |a| screen(a).contains("How does the endpoint take its key?")).await;
            assert!(transcript_text(&app).contains(&unset), "the variable is named");
            assert!(app.services.providers.store().get("modal").is_none());
        });
    }

    #[test]
    fn a_listing_that_does_not_read_is_shown_and_a_typed_model_still_saves() {
        let (mut app, _cmds, mut ui) = app_with("");
        in_local(async {
            let (base, _) = mu_testutil::fake_server(vec![listing(r#"{"data":[]}"#)]).await;
            filled(&mut app, &base);
            choose(&mut app, 4);
            settle(&mut app, &mut ui, |a| screen(a).contains("listed no usable models")).await;
            assert!(screen(&app).contains("Type a model id"), "{}", screen(&app));
            choose(&mut app, 0); // Type a model id…
            line(&mut app, "org/typed");
            choose(&mut app, 4); // Add it with the defaults
            esc(&mut app);
            choose(&mut app, 5); // Save
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
            app.login("custom");
            choose(&mut app, 0);
            line(&mut app, "modal");
            choose(&mut app, 2);
            line(&mut app, &base);
            choose(&mut app, 3);
            choose(&mut app, 0); // Paste a key
            type_text(&mut app, key);
            app.on_terminal(crossterm::event::Event::Paste("-PASTED-SECRET-99".into()));
            assert_eq!(app.prompt.composer.text(), format!("{key}-PASTED-SECRET-99"), "the masked input holds it");
            assert_eq!(app.prompt.ghost_seq, 0, "no prediction is asked for while a key is typed");
            app.on_terminal(press(KeyCode::Enter, KeyModifiers::NONE));
            assert!(app.prompt.composer.is_empty(), "the typed key leaves the composer");
            choose(&mut app, 4);
            settle(&mut app, &mut ui, |a| screen(a).contains("org/model")).await;
            let full = format!("{key}-PASTED-SECRET-99");
            assert!(!mu_base::secrets::redact_keys(&full).contains(&full), "the key is registered for redaction");
            let shown = transcript_text(&app);
            assert!(!shown.contains("SECRET"), "{shown}");
            assert!(!screen(&app).contains("SECRET"), "{}", screen(&app));
        });
    }

    #[test]
    fn a_command_is_previewed_by_its_program_only() {
        let (mut app, _cmds, _ui) = app_with("");
        in_local(async {
            filled(&mut app, "https://modal.example/v1");
            choose(&mut app, 3);
            choose(&mut app, 2); // Use a command
            line(&mut app, "echo modalkey-ARG-0123456789");
            assert!(screen(&app).contains("!echo …"), "{}", screen(&app));
            assert!(!transcript_text(&app).contains("modalkey-ARG"));
            choose(&mut app, 5); // Save
            let entry = app.services.providers.store().get("modal").expect("a stored command");
            assert_eq!(entry.reference.as_deref(), Some("!echo modalkey-ARG-0123456789"));
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
            assert!(
                matches!(&app.overlay, Some(Overlay::Model { pick, .. }) if pick.values().contains(&"modal/x/b".to_string()))
            );
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
        let rows = app.api_key_rows();
        assert!(rows.iter().any(|(_, t)| *t == LoginTarget::CustomProvider(None)), "the add row needs no valid file");
        assert!(rows.len() >= 2);
        assert!(transcript_text(&app).contains("providers.toml"));
    }

    #[test]
    fn the_login_list_shows_custom_endpoints_with_their_key_state() {
        let (mut app, _cmds, _ui) = app_with("");
        let registry = app.services.providers.custom_registry().clone();
        registry.add("modal", CustomProvider::new("http://127.0.0.1:9").validated("modal").unwrap()).unwrap();
        let lab = CustomProvider { auth: AuthMode::None, ..CustomProvider::new("http://127.0.0.1:8") };
        registry.add("lab", lab.validated("lab").unwrap()).unwrap();
        let rows = app.api_key_rows();
        let row = |t: &LoginTarget| rows.iter().find(|(_, x)| x == t).map(|(i, _)| i.detail.clone());
        assert_eq!(row(&LoginTarget::ApiKey("modal".into())).as_deref(), Some("key missing"));
        assert_eq!(row(&LoginTarget::ApiKey("lab".into())).as_deref(), Some("no auth"));
        assert!(rows.last().map(|r| &r.1) == Some(&LoginTarget::CustomProvider(None)), "the add row comes last");
        assert_eq!(rows.last().map(|r| r.0.label.as_str()), Some("+ Add a custom endpoint…"));

        // A keyless endpoint takes no key: its row says so instead of opening a key flow.
        app.login("lab");
        assert!(transcript_text(&app).contains("takes no key"));
        assert!(app.overlay.is_none());
        app.login("modal");
        assert!(matches!(&app.overlay, Some(Overlay::KeyChoice { provider, .. }) if provider == "modal"));
    }

    #[test]
    fn editing_an_endpoint_from_login_replaces_it_and_keeps_its_stored_key() {
        let (mut app, mut cmds, mut ui) = app_with("");
        in_local(async {
            let (base, _seen) = mu_testutil::fake_server(vec![listing(ONE_MODEL)]).await;
            let def = CustomProvider {
                models: BTreeMap::from([(
                    "org/model".to_string(),
                    CustomModel { context_window: Some(65_536), max_tokens: Some(8_192), ..Default::default() },
                )]),
                ..CustomProvider::new(&base)
            };
            let registry = app.services.providers.custom_registry().clone();
            registry.add("modal", def.validated("modal").unwrap()).unwrap();
            let store = app.services.providers.store().clone();
            store.set("modal", mu_provider::auth::store::Entry::literal("stored-key-0123456789")).unwrap();

            app.login("custom modal");
            let text = screen(&app);
            assert!(text.contains("Edit custom endpoint modal"), "{text}");
            assert!(text.contains("the stored key (not shown)"), "{text}");
            assert!(!text.contains("Provider id"), "the id is fixed:\n{text}");

            // Its model gains the effort ladder, keeping the limits the file set.
            choose(&mut app, 3); // Models
            settle(&mut app, &mut ui, |a| screen(a).contains("org/model")).await;
            choose(&mut app, 0); // the model
            assert!(screen(&app).contains("none; /thinking has nothing to set"), "{}", screen(&app));
            choose(&mut app, 3); // Reasoning
            choose(&mut app, 1); // Effort levels
            esc(&mut app); // back to the limits
            assert!(screen(&app).contains("effort: low, medium, high"), "{}", screen(&app));
            choose(&mut app, 4); // Save org/model
            esc(&mut app); // models → fields
            choose(&mut app, 4); // Save

            assert!(!app.custom_wizard_active() && app.overlay.is_none(), "{}", transcript_text(&app));
            assert!(transcript_text(&app).contains("Endpoint updated"), "{}", transcript_text(&app));
            let def = registry.get("modal").unwrap().expect("still there");
            assert_eq!(def.base_url, base, "the address is kept");
            assert_eq!(def.models["org/model"].context_window, Some(65_536));
            assert_eq!(def.models["org/model"].efforts, ["low", "medium", "high"]);
            assert_eq!(
                store.get("modal").and_then(|e| e.key).as_deref(),
                Some("stored-key-0123456789"),
                "an edit that chooses no key leaves the stored one alone"
            );
            assert!(matches!(cmds.try_recv(), Ok(Command::RefreshModel)));
        });
    }

    /// Answers every host call later, as a remote host does.
    fn deferred(app: &mut App) {
        app.host = Rc::new(crate::host::Deferred(crate::host::local::LocalHost::new(
            app.services.providers.clone(),
            app.ctx.clone(),
            app.plugins.clone(),
            app.cwd.clone(),
        )));
    }

    #[test]
    fn with_a_deferred_host_add_check_and_save_come_out_the_same() {
        let (mut app, mut cmds, mut ui) = app_with("[sets.main]\ndefault = \"a/old\"\n");
        deferred(&mut app);
        in_local(async {
            let (base, _seen) = mu_testutil::fake_server(vec![listing(ONE_MODEL)]).await;
            app.login("custom");
            assert!(!app.custom_wizard_active(), "the panel waits for the host");
            settle(&mut app, &mut ui, |a| screen(a).contains("Provider id")).await;
            choose(&mut app, 0);
            line(&mut app, "modal");
            choose(&mut app, 2);
            line(&mut app, &base);
            choose(&mut app, 3);
            settle(&mut app, &mut ui, |a| screen(a).contains("How does the endpoint take its key?")).await;
            choose(&mut app, 3); // No authentication
            choose(&mut app, 4); // Models: reads the endpoint
            settle(&mut app, &mut ui, |a| screen(a).contains("org/model")).await;
            choose(&mut app, 0);
            choose(&mut app, 4); // Add it
            esc(&mut app);
            choose(&mut app, 5); // Save
            settle(&mut app, &mut ui, |a| transcript_text(a).contains("Provider configured")).await;

            let saved = app.services.providers.custom_registry().get("modal").unwrap().expect("saved");
            assert_eq!(saved.models["org/model"].context_window, Some(65536), "the listed limits");
            assert!(matches!(cmds.try_recv(), Ok(Command::RefreshModel)));
            assert_eq!(app.services.providers.cfg.role("default").as_deref(), Some("a/old"));
            app.request_models();
            assert!(screen(&app).contains("modal/org/model"), "{}", screen(&app));
        });
    }

    #[test]
    fn with_a_deferred_host_a_taken_id_is_refused() {
        let (mut app, _cmds, mut ui) = app_with("");
        let registry = app.services.providers.custom_registry().clone();
        registry.add("modal", CustomProvider::new("http://127.0.0.1:9").validated("modal").unwrap()).unwrap();
        deferred(&mut app);
        in_local(async {
            app.login("custom");
            settle(&mut app, &mut ui, |a| screen(a).contains("Provider id")).await;
            choose(&mut app, 0);
            line(&mut app, "modal");
            assert!(transcript_text(&app).contains("`modal` is already in use"), "{}", transcript_text(&app));
        });
    }

    #[test]
    fn with_a_deferred_host_the_auth_screen_waits_for_the_variable_lookup() {
        let (mut app, _cmds, mut ui) = app_with("");
        deferred(&mut app);
        in_local(async {
            app.login("custom");
            settle(&mut app, &mut ui, |a| screen(a).contains("Provider id")).await;
            choose(&mut app, 0);
            line(&mut app, "lab");
            choose(&mut app, 3); // Key
            assert!(app.custom_busy(), "the host is asked, and keys wait");
            settle(&mut app, &mut ui, |a| screen(a).contains("How does the endpoint take its key?")).await;
            assert!(screen(&app).contains("$LAB_API_KEY is not set"), "{}", screen(&app));
            assert!(!app.custom_busy());
        });
    }

    #[test]
    fn with_a_deferred_host_an_edit_opens_and_saves_and_an_unknown_id_is_an_error() {
        let (mut app, _cmds, mut ui) = app_with("");
        in_local(async {
            let (base, _seen) = mu_testutil::fake_server(vec![listing(ONE_MODEL)]).await;
            let registry = app.services.providers.custom_registry().clone();
            registry.add("modal", CustomProvider::new(&base).validated("modal").unwrap()).unwrap();
            let store = app.services.providers.store().clone();
            store.set("modal", mu_provider::auth::store::Entry::literal("stored-key-0123456789")).unwrap();
            deferred(&mut app);

            app.login("custom modal");
            settle(&mut app, &mut ui, |a| screen(a).contains("Edit custom endpoint modal")).await;
            assert!(screen(&app).contains("the stored key (not shown)"), "{}", screen(&app));
            choose(&mut app, 3); // Models
            settle(&mut app, &mut ui, |a| screen(a).contains("org/model")).await;
            choose(&mut app, 0);
            choose(&mut app, 4); // Save org/model
            esc(&mut app);
            choose(&mut app, 4); // Save
            settle(&mut app, &mut ui, |a| transcript_text(a).contains("Endpoint updated")).await;
            assert!(registry.get("modal").unwrap().expect("there").models.contains_key("org/model"));
            assert_eq!(store.get("modal").and_then(|e| e.key).as_deref(), Some("stored-key-0123456789"));

            app.login("custom nosuch");
            settle(&mut app, &mut ui, |a| transcript_text(a).contains("no custom endpoint `nosuch`")).await;
            assert!(app.overlay.is_none());
        });
    }

    #[test]
    fn the_host_reports_an_environment_variable_as_set_only_when_it_is_not_blank() {
        let (app, _cmds, _ui) = app_with("");
        in_local(async {
            let ask = |name: &str| {
                let r = app.host.call(CUSTOM_ENV, serde_json::json!(name));
                match r {
                    crate::host::Answer::Ready(Ok(v)) => v.as_bool(),
                    _ => None,
                }
            };
            assert_eq!(ask("PATH"), Some(true));
            assert_eq!(ask(&format!("MU_TEST_UNSET_{}", uuid::Uuid::new_v4().simple())), Some(false));
        });
    }

    #[test]
    fn the_panel_types_survive_the_wire() {
        use mu_provider::auth::store::Entry;
        use mu_provider::model::discovery::CustomRow;
        fn round<T: serde::Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug>(v: T) {
            let back: T = serde_json::from_value(serde_json::to_value(&v).unwrap()).unwrap();
            assert_eq!(serde_json::to_value(&back).unwrap(), serde_json::to_value(&v).unwrap());
        }
        let row = CustomRow { id: "m".into(), context_length: Some(8), max_completion_tokens: None };
        let mut provider = CustomProvider::new("http://127.0.0.1:9");
        let mut model = CustomModel { context_window: Some(8192), ..Default::default() };
        model.extra_body = serde_json::from_value(serde_json::json!({"a": {"b": true, "n": 3, "x": 1.5}})).unwrap();
        provider.models.insert("org/m".into(), model.clone());
        let provider = provider.validated("modal").unwrap();
        let wire: CustomProvider = serde_json::from_value(serde_json::to_value(&provider).unwrap()).unwrap();
        assert_eq!(wire.models["org/m"], model, "a definition's nested switches survive JSON");
        round(Probe {
            id: "modal".into(),
            base_url: "http://x".into(),
            credential: Some(Entry::literal("k-1234567890")),
        });
        round(Saved {
            id: "modal".into(),
            provider,
            entry: Some(Entry::reference("$MODAL_KEY").unwrap()),
            editing: true,
            key_changed: false,
            listed: Some(vec![row.clone()]),
        });
        round(Fetch::Listed(vec![row]));
        round(Fetch::Credential("not set".into()));
        round(Outcome::Failed { message: "m".into(), written: true });
        round(Outcome::Saved);
    }

    #[test]
    fn the_login_list_offers_an_edit_row_and_an_unknown_id_is_an_error() {
        let (mut app, _cmds, _ui) = app_with("");
        let registry = app.services.providers.custom_registry().clone();
        registry.add("modal", CustomProvider::new("http://127.0.0.1:9").validated("modal").unwrap()).unwrap();
        let rows = app.api_key_rows();
        let edit = LoginTarget::CustomProvider(Some("modal".into()));
        assert!(rows.iter().any(|(_, t)| *t == edit), "each endpoint has a row that edits it");
        let label = rows.iter().find(|(_, t)| *t == edit).map(|(i, _)| i.label.clone());
        assert_eq!(label.as_deref(), Some("Edit custom endpoint: modal"));

        app.login("custom modal");
        assert!(app.custom_wizard_active());
        assert!(screen(&app).contains("Edit custom endpoint modal"), "{}", screen(&app));
        app.custom_cancel();
        app.login("custom nope");
        assert!(!app.custom_wizard_active());
        assert!(transcript_text(&app).contains("no custom endpoint `nope`"), "{}", transcript_text(&app));
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
    fn another_flow_takes_the_panels_place() {
        let (mut app, _cmds, _ui) = app_with("");
        app.prompt.composer.set_text("kept");
        app.login("custom");
        assert!(app.custom_wizard_active());
        app.start_key_flow(mu_provider::provider::providers::KeyTarget::builtin("anthropic").unwrap(), None, None);
        assert!(!app.custom_wizard_active(), "the key flow replaces it");
        assert_eq!(app.prompt.composer.text(), "kept", "the draft comes back");
    }
}
