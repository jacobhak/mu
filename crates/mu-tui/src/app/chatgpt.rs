//! Sign in with ChatGPT in the TUI: the browser flow with its pasted-redirect fallback, the
//! sign-out, and the `/login` row. The protocol and the rules for saving live in
//! `mu_provider::auth::chatgpt`; this file drives them and shows the results.

use super::*;
use crate::widgets::overlay::{KeyOption, LoginTarget};
use crate::widgets::select::{Item, Select};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use mu_provider::auth::chatgpt::{self, Account, LABEL, PROVIDER_ID, USAGE_URL, oauth, signin};
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

/// A sign-in waiting for the browser's callback or for the redirect URL typed into the composer.
/// The composer is locked to that input until the flow ends.
pub(super) struct ChatgptFlow {
    /// Tells a late result from the current flow's.
    pub(super) id: u64,
    pub(super) cancel: CancellationToken,
    /// The typed redirect URL goes to the login task here.
    pub(super) paste: UnboundedSender<String>,
}

/// `openai-chatgpt`, or its `chatgpt` alias.
pub(super) fn is_chatgpt(id: &str) -> bool {
    matches!(id, PROVIDER_ID | "chatgpt")
}

/// The `/login` row for the ChatGPT sign-in and its status.
pub(super) fn login_item(account: Option<&Account>) -> (Item, LoginTarget) {
    let detail = match account {
        Some(a) if a.plan_usage() => format!("signed in as {} · chat on your plan", a.label()),
        Some(a) if a.signed_in() => "signed in · plan usage not enabled".into(),
        _ => "not signed in".into(),
    };
    (Item { label: "Continue with ChatGPT".into(), detail }, LoginTarget::ChatGpt)
}

/// The ChatGPT menu for a saved account: sign in (again), sign out when signed in, forget the
/// registration to use another account, or cancel.
pub(super) fn chatgpt_menu(signed_in: bool) -> (Vec<Item>, Vec<KeyOption>) {
    let mut items = vec![Item {
        label: if signed_in { "Sign in again" } else { "Sign in" }.into(),
        detail: "reauthorize this mu's registration".into(),
    }];
    let mut options = vec![KeyOption::SignIn];
    if signed_in {
        items.push(Item { label: "Sign out".into(), detail: "drop the tokens and revoke them".into() });
        options.push(KeyOption::SignOut);
    }
    items.push(Item {
        label: "Use a different ChatGPT account".into(),
        detail: "forget this registration, then sign in".into(),
    });
    options.push(KeyOption::Forget);
    items.push(Item { label: "Cancel".into(), detail: "nothing changes".into() });
    options.push(KeyOption::Cancel);
    (items, options)
}

/// The picker's marker for a model that runs natively: the ChatGPT plan, or the API key.
pub(super) fn native_label(provider: &str) -> &'static str {
    if provider == PROVIDER_ID { LABEL } else { "key" }
}

impl App {
    /// Whether a ChatGPT sign-in is waiting; the composer is locked to it meanwhile.
    pub(super) fn chatgpt_typing(&self) -> bool {
        self.flows.chatgpt.is_some()
    }

    /// Starts the browser sign-in, as `/login chatgpt` and the `/login` row do.
    pub(super) fn start_chatgpt(&mut self) {
        if self.chatgpt_typing() {
            return self.notice("a ChatGPT sign-in is already waiting: paste its redirect URL, or press Esc");
        }
        let store = self.services.providers.store().clone();
        let host = match store.chatgpt_host_id() {
            Ok(h) => h,
            Err(e) => return self.error(&format!("could not start the ChatGPT sign-in: {e}")),
        };
        let endpoints = store.chatgpt_endpoints();
        let known = store.chatgpt_known();
        self.flows.chatgpt_seq += 1;
        let id = self.flows.chatgpt_seq;
        let cancel = CancellationToken::new();
        let (paste, mut paste_rx) = unbounded_channel();
        self.flows.chatgpt = Some(ChatgptFlow { id, cancel: cancel.clone(), paste });
        self.prompt.composer.set_private(true);
        self.prompt.composer.set_closed(true);
        self.notice("signing in with ChatGPT…");
        let tx = self.ui_tx.clone();
        tokio::task::spawn_local(async move {
            let url_tx = tx.clone();
            let req = oauth::LoginRequest { endpoints: &endpoints, host_id: &host, known, cancel };
            let notice_tx = tx.clone();
            let mut on_url = |url: &str| {
                let _ = url_tx.send(UiMsg::ChatgptUrl { flow: id, url: url.to_string() });
                let _ = chatgpt::browser::open_browser(url);
            };
            let mut notice = |text: &str| {
                let _ = notice_tx.send(UiMsg::ChatgptNotice { flow: id, text: text.to_string() });
            };
            let r = oauth::login(req, &mut on_url, &mut notice, &mut paste_rx).await.map_err(|e| e.to_string());
            let _ = tx.send(UiMsg::ChatgptDone { flow: id, r });
        });
    }

    /// A line the sign-in reports while it waits: a callback or pasted URL that did not match it.
    pub(super) fn on_chatgpt_notice(&mut self, flow: u64, text: &str) {
        if self.flows.chatgpt.as_ref().is_some_and(|f| f.id == flow) {
            self.notice(text);
        }
    }

    /// Shows the authorization link the browser was asked to open, and how to finish by hand.
    pub(super) fn on_chatgpt_url(&mut self, flow: u64, url: &str) {
        if self.flows.chatgpt.as_ref().is_some_and(|f| f.id == flow) {
            self.notice(&format!(
                "A browser should open to sign in with ChatGPT. If it does not, open this link:\n{url}\n\
                 If the browser can't reach this machine, paste the final redirect URL here and press Enter (Esc cancels)."
            ));
        }
    }

    /// Keys the redirect-URL input takes before the composer. True when handled.
    pub(super) fn chatgpt_key(&mut self, k: KeyEvent) -> bool {
        let ctrl_c = k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL);
        if k.code == KeyCode::Esc || ctrl_c {
            self.cancel_chatgpt();
            return true;
        }
        if k.code == KeyCode::Enter && !k.modifiers.intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) {
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
        if let Some(f) = self.flows.chatgpt.as_ref() {
            let _ = f.paste.send(text);
        }
        self.notice("checking the redirect URL…");
    }

    /// Esc or Ctrl-C while a sign-in waits: the flow ends, and its late result is ignored.
    fn cancel_chatgpt(&mut self) {
        let Some(f) = self.flows.chatgpt.take() else { return };
        f.cancel.cancel();
        self.unlock_composer();
        self.notice("sign-in cancelled; nothing changed");
    }

    pub(super) fn on_chatgpt_done(&mut self, flow: u64, r: Result<Account, String>) {
        if !self.flows.chatgpt.as_ref().is_some_and(|f| f.id == flow) {
            return;
        }
        self.flows.chatgpt = None;
        self.unlock_composer();
        match r {
            Ok(account) => self.save_chatgpt(account),
            Err(e) => {
                self.error(&format!("ChatGPT sign-in did not finish: {e}. Nothing was saved; /login to try again"))
            }
        }
    }

    /// Saves a finished sign-in by the same rules as `mu auth login`, then says what it means.
    fn save_chatgpt(&mut self, account: Account) {
        let store = self.services.providers.store().clone();
        let saved = match signin::save(&store, account) {
            Ok(s) => s,
            Err(m) => return self.error(&m),
        };
        self.send(Command::RefreshModel);
        if !saved.plan_usage {
            return self.error(&format!(
                "signed in as {}, but plan usage is not enabled for this sign-in, so {PROVIDER_ID}/* models cannot run yet. /login again and approve the plan permission",
                saved.label
            ));
        }
        self.notice(&format!("signed in as {}; {PROVIDER_ID}/* models now run on your ChatGPT plan", saved.label));
        if saved.welcome {
            self.notice(&format!("You're using your ChatGPT plan. Manage usage: {USAGE_URL}"));
        }
    }

    /// The `/login` row: signs in when no account is saved, else opens the ChatGPT menu (signed in or not).
    pub(super) fn chatgpt_row(&mut self) {
        let Some(signed_in) = self.services.providers.store().chatgpt_account().map(|a| a.signed_in()) else {
            return self.start_chatgpt();
        };
        let (items, options) = chatgpt_menu(signed_in);
        self.overlay = Some(Overlay::KeyChoice {
            sel: Select::new("ChatGPT sign-in", items),
            provider: PROVIDER_ID.into(),
            options,
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
        let store = self.services.providers.store().clone();
        let account = match signin::sign_out(&store) {
            Ok(Some(a)) => a,
            Ok(None) => return self.notice("not signed in with ChatGPT"),
            Err(e) => return self.error(&format!("could not sign out of ChatGPT: {e}")),
        };
        self.notice(&format!(
            "signed out of ChatGPT ({}); {PROVIDER_ID}/* models stop running until you sign in again",
            account.label()
        ));
        self.send(Command::RefreshModel);
        let tx = self.ui_tx.clone();
        tokio::task::spawn_local(async move {
            let r = signin::revoke(&store, &account).await;
            let _ = tx.send(UiMsg::ChatgptRevoked(r));
        });
    }

    /// Forgets the registration, so the sign-in that follows may be another ChatGPT account. The
    /// sign-in is revoked in the background, as a sign-out is, and the browser sign-in starts now.
    pub(super) fn chatgpt_forget(&mut self) {
        if self.chatgpt_typing() {
            return self.start_chatgpt();
        }
        let store = self.services.providers.store().clone();
        let account = match signin::forget(&store) {
            Ok(a) => a,
            Err(e) => return self.error(&format!("could not forget the ChatGPT registration: {e}")),
        };
        if let Some(account) = account {
            self.notice(&format!(
                "forgot the ChatGPT registration ({}); the next sign-in may use another ChatGPT account. The old app stays listed under your ChatGPT settings, apps, until you disconnect it there",
                account.label()
            ));
            self.send(Command::RefreshModel);
            if account.signed_in() {
                let tx = self.ui_tx.clone();
                tokio::task::spawn_local(async move {
                    let r = signin::revoke(&store, &account).await;
                    let _ = tx.send(UiMsg::ChatgptRevoked(r));
                });
            }
        }
        self.start_chatgpt();
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
    use mu_provider::auth::chatgpt::{DIRECT_SCOPE, Tokens};

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

    #[test]
    fn the_login_row_shows_each_status() {
        let detail = |a: Option<Account>| login_item(a.as_ref()).0.detail;
        assert_eq!(detail(None), "not signed in");
        assert_eq!(detail(Some(account(false, false))), "not signed in");
        assert_eq!(detail(Some(account(true, true))), "signed in as me@example.com · chat on your plan");
        assert_eq!(detail(Some(account(true, false))), "signed in · plan usage not enabled");
        let (item, target) = login_item(None);
        assert_eq!(item.label, "Continue with ChatGPT");
        assert_eq!(target, LoginTarget::ChatGpt);
    }

    #[test]
    fn the_chatgpt_menu_offers_a_different_account_with_or_without_a_sign_in() {
        let (items, options) = chatgpt_menu(true);
        assert_eq!(items.len(), options.len());
        assert_eq!(options, [KeyOption::SignIn, KeyOption::SignOut, KeyOption::Forget, KeyOption::Cancel]);
        assert_eq!(items[2].label, "Use a different ChatGPT account");
        let (items, options) = chatgpt_menu(false);
        assert_eq!(items.len(), options.len());
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
