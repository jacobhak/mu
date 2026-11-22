//! Sign in with ChatGPT in the TUI: the browser flow with its pasted-redirect fallback, the
//! sign-out, and the `/login` row. The protocol and the rules for saving live in
//! `mu_provider::auth::chatgpt`; this file drives them and shows the results.

use super::*;
use crate::host::chatgpt::{self as host, Done, Flow, Gone, Notice, Paste, Revoke, Saved, Url};
use crate::host::keys::ChatgptLabel;
use crate::keymap;
use crate::widgets::overlay::{KeyOption, LoginTarget};
use crate::widgets::picker::Picker;
use crate::widgets::select::{Item, Select};
use crossterm::event::KeyEvent;
use mu_provider::auth::chatgpt::{self, AUTH_BASE, LABEL, PROVIDER_ID, USAGE_URL};

/// A sign-in waiting for the browser's callback or for the redirect URL typed into the composer.
/// The composer is locked to that input until the flow ends. The sign-in itself runs on the host.
pub(super) struct ChatgptFlow {
    /// Tells a late result from the current flow's.
    pub(super) id: u64,
}

/// `openai-chatgpt`, or its `chatgpt` alias.
pub(super) fn is_chatgpt(id: &str) -> bool {
    matches!(id, PROVIDER_ID | "chatgpt")
}

/// The `/login` row for the ChatGPT sign-in and its status.
pub(super) fn login_item(account: Option<&ChatgptLabel>) -> (Item, LoginTarget) {
    let detail = match account {
        Some(a) if a.plan_usage => format!("signed in as {} · chat on your plan", a.label),
        Some(a) if a.signed_in => "signed in · plan usage not enabled".into(),
        _ => "not signed in".into(),
    };
    (Item { label: "Continue with ChatGPT".into(), detail }, LoginTarget::ChatGpt)
}

/// The ChatGPT menu for a saved account: sign in (again), sign out when signed in, forget the
/// registration to use another account, or cancel.
pub(super) fn chatgpt_menu(signed_in: bool) -> Vec<(Item, KeyOption)> {
    let mut rows = vec![(
        Item {
            label: if signed_in { "Sign in again" } else { "Sign in" }.into(),
            detail: "reauthorize this mu's registration".into(),
        },
        KeyOption::SignIn,
    )];
    if signed_in {
        rows.push((
            Item { label: "Sign out".into(), detail: "drop the tokens and revoke them".into() },
            KeyOption::SignOut,
        ));
    }
    rows.push((
        Item {
            label: "Use a different ChatGPT account".into(),
            detail: "forget this registration, then sign in".into(),
        },
        KeyOption::Forget,
    ));
    rows.push((Item { label: "Cancel".into(), detail: "nothing changes".into() }, KeyOption::Cancel));
    rows
}

/// Whether the client may open `url`: `https` on the sign-in's own host, with no userinfo or port.
/// The URL comes from the host, which may be a remote agent.
fn auth_url_openable(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else { return false };
    if url.chars().any(|c| c.is_control() || c.is_whitespace() || c == '\\') {
        return false;
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    AUTH_BASE.strip_prefix("https://").is_some_and(|h| h.eq_ignore_ascii_case(authority))
}

/// The picker's marker for a model that runs natively: the ChatGPT plan, or the API key.
pub(super) fn native_label(provider: &str) -> &'static str {
    if provider == PROVIDER_ID { LABEL } else { "key" }
}

impl App {
    /// Whether a ChatGPT sign-in is waiting; the composer is locked to it meanwhile.
    pub(super) fn chatgpt_typing(&self) -> bool {
        self.flows.chatgpt().is_some()
    }

    /// Starts the browser sign-in, as `/login chatgpt` and the `/login` row do.
    pub(super) fn start_chatgpt(&mut self) {
        self.leave_custom_wizard();
        if self.chatgpt_typing() {
            return self.notice(&format!(
                "a ChatGPT sign-in is already waiting: paste its redirect URL, or press {}",
                keymap::WIZARD_CANCEL.label()
            ));
        }
        self.flows.chatgpt_seq += 1;
        let id = self.flows.chatgpt_seq;
        // The flow is live before the host answers: its URL can arrive ahead of the reply.
        self.flows.begin(Active::Chatgpt(ChatgptFlow { id }));
        self.prompt.composer.set_private(true);
        self.prompt.composer.set_closed(true);
        self.host_call(host::CHATGPT_START, &Flow { flow: id }, move |app, r: Result<(), String>| match r {
            Ok(()) => app.notice("signing in with ChatGPT…"),
            Err(e) => {
                if app.flows.chatgpt().is_some_and(|f| f.id == id) {
                    app.flows.end_chatgpt();
                    app.unlock_composer();
                }
                app.error(&format!("could not start the ChatGPT sign-in: {e}"));
            }
        });
    }

    /// A value the host pushed for the sign-in: its URL, a line it reports, or its end.
    pub(super) fn on_chatgpt_pushed(&mut self, p: crate::host::Pushed) {
        match p.name.as_str() {
            host::URL => {
                if let Ok(u) = serde_json::from_value::<Url>(p.data) {
                    if self.flows.chatgpt().is_some_and(|f| f.id == u.flow) && !cfg!(test) {
                        if auth_url_openable(&u.url) {
                            let _ = chatgpt::browser::open_browser(&u.url);
                        } else {
                            self.notice(
                                "the sign-in link is not on the ChatGPT sign-in host, so no browser was opened",
                            );
                        }
                    }
                    self.on_chatgpt_url(u.flow, &u.url);
                }
            }
            host::NOTICE => {
                if let Ok(n) = serde_json::from_value::<Notice>(p.data) {
                    self.on_chatgpt_notice(n.flow, &n.text);
                }
            }
            host::DONE => {
                if let Ok(d) = serde_json::from_value::<Done>(p.data) {
                    self.on_chatgpt_done(d.flow, d.error.map_or(Ok(()), Err));
                }
            }
            _ => {}
        }
    }

    /// A line the sign-in reports while it waits: a callback or pasted URL that did not match it.
    pub(super) fn on_chatgpt_notice(&mut self, flow: u64, text: &str) {
        if self.flows.chatgpt().is_some_and(|f| f.id == flow) {
            self.notice(text);
        }
    }

    /// Shows the authorization link the browser was asked to open, and how to finish by hand.
    pub(super) fn on_chatgpt_url(&mut self, flow: u64, url: &str) {
        if self.flows.chatgpt().is_some_and(|f| f.id == flow) {
            self.notice(&format!(
                "A browser should open to sign in with ChatGPT. If it does not, open this link:\n{url}\n\
                 If the browser can't reach this machine, paste the final redirect URL here and press {} ({} cancels).",
                keymap::SUBMIT.label(),
                keymap::WIZARD_CANCEL.label()
            ));
        }
    }

    /// Keys the redirect-URL input takes before the composer. True when handled.
    pub(super) fn chatgpt_key(&mut self, k: KeyEvent) -> bool {
        if keymap::WIZARD_CANCEL.hit(&k) {
            self.cancel_chatgpt();
            return true;
        }
        if keymap::SUBMIT.hit(&k) {
            self.send_redirect();
            return true;
        }
        false
    }

    /// Enter in the redirect-URL input: the login task takes the text. The composer is cleared and
    /// stays locked until the sign-in ends.
    fn send_redirect(&mut self) {
        let text = self.prompt.composer.secret_text().trim().to_string();
        if text.is_empty() {
            return;
        }
        self.prompt.composer.take_secret();
        if let Some(flow) = self.flows.chatgpt().map(|f| f.id) {
            self.host_call(host::CHATGPT_PASTE, &Paste { flow, text }, |app, r: Result<(), String>| {
                if let Err(e) = r {
                    app.error(&e);
                }
            });
        }
        self.notice("checking the redirect URL…");
    }

    /// Esc while a sign-in waits: the flow ends, and its late result is ignored.
    fn cancel_chatgpt(&mut self) {
        let Some(f) = self.flows.end_chatgpt() else { return };
        self.host_call(host::CHATGPT_CANCEL, &Flow { flow: f.id }, |_, _: Result<(), String>| {});
        self.unlock_composer();
        self.notice("sign-in cancelled; nothing changed");
    }

    pub(super) fn on_chatgpt_done(&mut self, flow: u64, r: Result<(), String>) {
        if !self.flows.chatgpt().is_some_and(|f| f.id == flow) {
            return;
        }
        self.flows.end_chatgpt();
        self.unlock_composer();
        match r {
            Ok(()) => self.save_chatgpt(flow),
            Err(e) => {
                self.error(&format!("ChatGPT sign-in did not finish: {e}. Nothing was saved; /login to try again"))
            }
        }
    }

    /// Saves a finished sign-in by the same rules as `mu auth login`, then says what it means.
    fn save_chatgpt(&mut self, flow: u64) {
        self.host_call(host::CHATGPT_SAVE, &Flow { flow }, |app, r: Result<Saved, String>| {
            let saved = match r {
                Ok(s) => s,
                Err(m) => return app.error(&m),
            };
            app.send(Command::RefreshModel);
            if !saved.plan_usage {
                return app.error(&format!(
                    "signed in as {}, but plan usage is not enabled for this sign-in, so {PROVIDER_ID}/* models cannot run yet. /login again and approve the plan permission",
                    saved.label
                ));
            }
            app.notice(&format!("signed in as {}; {PROVIDER_ID}/* models now run on your ChatGPT plan", saved.label));
            if saved.welcome {
                app.notice(&format!("You're using your ChatGPT plan. Manage usage: {USAGE_URL}"));
            }
        });
    }

    /// The `/login` row: signs in when no account is saved, else opens the ChatGPT menu (signed in or not).
    pub(super) fn chatgpt_row(&mut self) {
        self.host_call(host::CHATGPT_STATUS, &(), |app, r: Result<Option<bool>, String>| match r {
            Ok(Some(signed_in)) => {
                let pick = Picker::from_rows(chatgpt_menu(signed_in), |items| Select::new("ChatGPT sign-in", items));
                app.open_overlay(Overlay::KeyChoice { pick, provider: PROVIDER_ID.into() });
            }
            Ok(None) => app.start_chatgpt(),
            Err(e) => app.error(&e),
        });
    }

    /// What the ChatGPT menu chose.
    pub(super) fn on_chatgpt_choice(&mut self, choice: KeyOption) {
        match choice {
            KeyOption::SignIn => self.start_chatgpt(),
            KeyOption::SignOut => self.chatgpt_sign_out(),
            KeyOption::Forget => self.chatgpt_forget(),
            _ => {}
        }
    }

    /// Drops the tokens now and revokes the refresh token in the background.
    pub(super) fn chatgpt_sign_out(&mut self) {
        self.host_call(host::CHATGPT_SIGN_OUT, &(), |app, r: Result<Option<Gone>, String>| {
            let account = match r {
                Ok(Some(a)) => a,
                Ok(None) => return app.notice("not signed in with ChatGPT"),
                Err(e) => return app.error(&format!("could not sign out of ChatGPT: {e}")),
            };
            app.notice(&format!(
                "signed out of ChatGPT ({}); {PROVIDER_ID}/* models stop running until you sign in again",
                account.label
            ));
            app.send(Command::RefreshModel);
            app.revoke_chatgpt(account.revoke);
        });
    }

    fn revoke_chatgpt(&mut self, revoke: Option<u64>) {
        if let Some(revoke) = revoke {
            self.host_call(host::CHATGPT_REVOKE, &Revoke { revoke }, |app, r: Result<(), String>| {
                app.on_chatgpt_revoked(r)
            });
        }
    }

    /// Forgets the registration, so the sign-in that follows may be another ChatGPT account. The
    /// sign-in is revoked in the background, as a sign-out is, and the browser sign-in starts now.
    pub(super) fn chatgpt_forget(&mut self) {
        if self.chatgpt_typing() {
            return self.start_chatgpt();
        }
        self.host_call(host::CHATGPT_FORGET, &(), |app, r: Result<Option<Gone>, String>| {
            let account = match r {
                Ok(a) => a,
                Err(e) => return app.error(&format!("could not forget the ChatGPT registration: {e}")),
            };
            if let Some(account) = account {
                app.notice(&format!(
                    "forgot the ChatGPT registration ({}); the next sign-in may use another ChatGPT account. The old app stays listed under your ChatGPT settings, apps, until you disconnect it there",
                    account.label
                ));
                app.send(Command::RefreshModel);
                app.revoke_chatgpt(account.revoke);
            }
            app.start_chatgpt();
        });
    }

    pub(super) fn on_chatgpt_revoked(&mut self, r: Result<(), String>) {
        if let Err(m) = r {
            self.notice(&format!(
                "could not confirm that ChatGPT revoked the sign-in ({m}); disconnect mu under your ChatGPT settings, apps"
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::keys::tests::{in_local, press, test_app_with_commands, transcript_text};
    use crate::host::{Deferred, Pushed, local::LocalHost};
    use crossterm::event::KeyCode;
    use mu_provider::auth::chatgpt::{Account, DIRECT_SCOPE, Tokens};
    use std::time::Duration;
    use tokio::sync::mpsc::UnboundedReceiver;

    struct Rig {
        app: App,
        ui: UnboundedReceiver<UiMsg>,
        pushed: UnboundedReceiver<Pushed>,
        _cmds: UnboundedReceiver<Command>,
    }

    /// An app whose host is the local one, or the local one wrapped in `Deferred`.
    fn rig(deferred: bool) -> Rig {
        let (mut app, _cmds) = test_app_with_commands(std::env::temp_dir().join("mu-chatgpt-ui-history"));
        let (ui_tx, ui) = tokio::sync::mpsc::unbounded_channel();
        app.ui_tx = ui_tx;
        if deferred {
            app.host = Rc::new(Deferred(LocalHost::new(
                app.services.providers.clone(),
                app.ctx.clone(),
                app.plugins.clone(),
                app.cwd.clone(),
            )));
        }
        let pushed = app.host.pushed().unwrap();
        Rig { app, ui, pushed, _cmds }
    }

    impl Rig {
        async fn pump(&mut self, done: impl Fn(&App) -> bool) {
            let Rig { app, ui, pushed, .. } = self;
            let waited = tokio::time::timeout(Duration::from_secs(10), async {
                while !done(app) {
                    tokio::select! {
                        Some(m) = ui.recv() => app.on_ui(m),
                        Some(p) = pushed.recv() => app.on_pushed(p),
                    }
                }
            })
            .await;
            assert!(waited.is_ok(), "timed out; the transcript says:\n{}", transcript_text(app));
        }
    }

    /// Runs `check` once with the local host and once with a deferred one.
    fn on_both_hosts<F: std::future::Future<Output = ()>>(check: impl Fn(Rig) -> F) {
        for deferred in [false, true] {
            in_local(check(rig(deferred)));
        }
    }

    fn save_account(app: &App, a: Account) {
        app.services.providers.store().chatgpt_save(a).unwrap();
    }

    #[test]
    fn the_login_row_opens_the_menu_of_a_saved_account() {
        on_both_hosts(|mut r| async move {
            save_account(&r.app, account(false, false));
            r.app.chatgpt_row();
            r.pump(|app| app.overlay.is_some()).await;
            assert!(matches!(r.app.overlay, Some(Overlay::KeyChoice { .. })));
        });
    }

    #[test]
    fn a_sign_out_without_an_account_says_so() {
        on_both_hosts(|mut r| async move {
            r.app.chatgpt_sign_out();
            r.pump(|app| transcript_text(app).contains("not signed in with ChatGPT")).await;
        });
    }

    #[test]
    fn forgetting_a_registration_names_it_and_starts_a_sign_in() {
        on_both_hosts(|mut r| async move {
            save_account(&r.app, account(false, false));
            r.app.chatgpt_forget();
            r.pump(|app| app.flows.chatgpt().is_some()).await;
            let t = transcript_text(&r.app);
            assert!(t.contains("forgot the ChatGPT registration (me@example.com)"), "{t}");
            assert!(r.app.services.providers.store().chatgpt_account().is_none());
        });
    }

    #[test]
    fn revoking_an_unknown_sign_in_is_reported() {
        on_both_hosts(|mut r| async move {
            r.app.revoke_chatgpt(Some(u64::MAX));
            r.pump(|app| transcript_text(app).contains("could not confirm")).await;
            assert!(transcript_text(&r.app).contains("no ChatGPT sign-in to revoke"));
        });
    }

    #[test]
    fn a_finished_sign_in_is_saved_by_the_host() {
        on_both_hosts(|mut r| async move {
            let mut a = account(true, true);
            if let Some(t) = a.tokens.as_mut() {
                t.access_token = "acc-test-0123456789".into();
                t.refresh_token = "ref-test-0123456789".into();
                t.id_token = "id-test-0123456789".into();
            }
            r.app.flows.active = Active::Chatgpt(ChatgptFlow { id: 4 });
            crate::host::chatgpt::finish_for_test(4, a);
            r.app.on_chatgpt_done(4, Ok(()));
            r.pump(|app| transcript_text(app).contains("Manage usage")).await;
            let t = transcript_text(&r.app);
            assert!(t.contains(&format!("signed in as me@example.com; {PROVIDER_ID}/* models now run")), "{t}");
            assert!(r.app.services.providers.store().chatgpt_account().is_some_and(|a| a.signed_in()));
            // A second save has nothing to save.
            r.app.flows.active = Active::Chatgpt(ChatgptFlow { id: 4 });
            r.app.on_chatgpt_done(4, Ok(()));
            r.pump(|app| transcript_text(app).contains("no finished ChatGPT sign-in")).await;
        });
    }

    #[test]
    fn a_sign_in_shows_its_url_takes_a_paste_and_is_cancelled_on_the_host() {
        on_both_hosts(|mut r| async move {
            // A saved registration lets the callback listener use any free port.
            save_account(&r.app, account(false, false));
            r.app.start_chatgpt();
            r.pump(|app| transcript_text(app).contains("A browser should open")).await;
            assert!(r.app.typing(), "the composer is locked to the redirect URL");
            for c in "nope".chars() {
                r.app.on_terminal(press(KeyCode::Char(c), crossterm::event::KeyModifiers::NONE));
            }
            r.app.on_terminal(press(KeyCode::Enter, crossterm::event::KeyModifiers::NONE));
            r.pump(|app| transcript_text(app).contains("Paste the full address from the browser")).await;
            r.app.on_terminal(press(KeyCode::Esc, crossterm::event::KeyModifiers::NONE));
            assert!(r.app.flows.chatgpt().is_none());
            // The host ends the cancelled flow; its late result is ignored.
            let p = tokio::time::timeout(Duration::from_secs(10), r.pushed.recv()).await.unwrap().unwrap();
            assert_eq!(p.name, crate::host::chatgpt::DONE);
            r.app.on_pushed(p);
            let t = transcript_text(&r.app);
            assert!(t.contains("sign-in cancelled; nothing changed") && !t.contains("did not finish"), "{t}");
        });
    }

    fn account(signed_in: bool, plan: bool) -> Account {
        let tokens = signed_in.then(|| Tokens {
            id_token: String::new(),
            access_token: String::new(),
            refresh_token: String::new(),
            refresh_at: 0,
            scopes: if plan { vec![DIRECT_SCOPE.into()] } else { vec![] },
        });
        Account {
            client_id: "oaiapp_test".into(),
            subject: "user-test".into(),
            email: Some("me@example.com".into()),
            tokens,
            added: "2026-01-01T00:00:00Z".into(),
        }
    }

    fn label(signed_in: bool, plan: bool) -> ChatgptLabel {
        ChatgptLabel {
            signed_in,
            plan_usage: signed_in && plan,
            label: "me@example.com".into(),
            added: "2026-01-01T00:00:00Z".into(),
        }
    }

    #[test]
    fn only_https_urls_on_the_auth_host_open() {
        assert!(auth_url_openable("https://auth.openai.com/api/accounts/authorize?client_id=x&state=y"));
        for bad in [
            "http://auth.openai.com/a",
            "file:///etc/passwd",
            "https://auth.openai.com.evil.example/a",
            "https://auth.openai.com@evil.example/a",
            "https://evil.example/https://auth.openai.com/",
            "https://auth.openai.com:8443/a",
            "https://auth.openai.com/a b",
            "https://auth.openai.com/a\n--flag",
            "-https://auth.openai.com/",
            "",
        ] {
            assert!(!auth_url_openable(bad), "{bad}");
        }
    }

    #[test]
    fn the_login_row_shows_each_status() {
        let detail = |a: Option<ChatgptLabel>| login_item(a.as_ref()).0.detail;
        assert_eq!(detail(None), "not signed in");
        assert_eq!(detail(Some(label(false, false))), "not signed in");
        assert_eq!(detail(Some(label(true, true))), "signed in as me@example.com · chat on your plan");
        assert_eq!(detail(Some(label(true, false))), "signed in · plan usage not enabled");
        let (item, target) = login_item(None);
        assert_eq!(item.label, "Continue with ChatGPT");
        assert_eq!(target, LoginTarget::ChatGpt);
    }

    #[test]
    fn the_chatgpt_menu_offers_a_different_account_with_or_without_a_sign_in() {
        let (items, options): (Vec<_>, Vec<_>) = chatgpt_menu(true).into_iter().unzip();
        assert_eq!(options, [KeyOption::SignIn, KeyOption::SignOut, KeyOption::Forget, KeyOption::Cancel]);
        assert_eq!(items[2].label, "Use a different ChatGPT account");
        let (items, options): (Vec<_>, Vec<_>) = chatgpt_menu(false).into_iter().unzip();
        assert_eq!(options, [KeyOption::SignIn, KeyOption::Forget, KeyOption::Cancel]);
        assert_eq!(items[0].label, "Sign in");
        assert_eq!(items[1].label, "Use a different ChatGPT account");
    }

    #[test]
    fn the_plan_and_the_key_markers_differ() {
        assert_eq!(native_label(PROVIDER_ID), "ChatGPT plan");
        assert_eq!(native_label("openai"), "key");
        assert!(is_chatgpt("chatgpt") && is_chatgpt(PROVIDER_ID) && !is_chatgpt("openai"));
    }
}
