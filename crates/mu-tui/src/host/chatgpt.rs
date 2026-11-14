//! Sign in with ChatGPT (table row "ChatGPT sign-in"): the flow runs on the host; the URL and the
//! pasted redirect cross the wire. The tokens never do: a finished sign-in waits on the host until
//! `chatgpt_save`, and a signed-out account until `chatgpt_revoke`.

use super::local::{Local, Registry};
use super::{Answer, Pushed, encode, pending, ready};
use mu_provider::auth::chatgpt::{Account, oauth, signin};
use serde::{Deserialize, Serialize};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
use tokio_util::sync::CancellationToken;

/// Whether an account is saved, and whether it is signed in: `Option<bool>`.
pub const CHATGPT_STATUS: &str = "chatgpt_status";
pub const CHATGPT_START: &str = "chatgpt_start";
pub const CHATGPT_PASTE: &str = "chatgpt_paste";
pub const CHATGPT_CANCEL: &str = "chatgpt_cancel";
pub const CHATGPT_SAVE: &str = "chatgpt_save";
pub const CHATGPT_SIGN_OUT: &str = "chatgpt_sign_out";
pub const CHATGPT_FORGET: &str = "chatgpt_forget";
pub const CHATGPT_REVOKE: &str = "chatgpt_revoke";

/// Pushed: the authorization URL of a waiting sign-in (`Url`).
pub const URL: &str = "chatgpt_url";
/// Pushed: a line the waiting sign-in reports (`Notice`).
pub const NOTICE: &str = "chatgpt_notice";
/// Pushed: the sign-in ended (`Done`).
pub const DONE: &str = "chatgpt_done";

/// Names one sign-in; the UI picks the id and tells a late result from the current flow's by it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Flow {
    pub flow: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Url {
    pub flow: u64,
    pub url: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Notice {
    pub flow: u64,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Done {
    pub flow: u64,
    /// Why the sign-in did not finish; none when it did (and waits for `chatgpt_save`).
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Paste {
    pub flow: u64,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Saved {
    pub label: String,
    pub plan_usage: bool,
    pub welcome: bool,
}

/// The account a sign-out or a forget dropped.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Gone {
    pub label: String,
    /// Names the tokens the host keeps for `chatgpt_revoke`; none when the account was not signed in.
    pub revoke: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Revoke {
    pub revoke: u64,
}

struct Running {
    cancel: CancellationToken,
    paste: UnboundedSender<String>,
}

thread_local! {
    static RUNNING: RefCell<HashMap<u64, Running>> = RefCell::new(HashMap::new());
    static FINISHED: RefCell<HashMap<u64, Account>> = RefCell::new(HashMap::new());
    static TO_REVOKE: RefCell<HashMap<u64, Account>> = RefCell::new(HashMap::new());
    static NEXT_REVOKE: Cell<u64> = const { Cell::new(0) };
}

pub(super) fn register(r: &mut Registry) {
    r.add(CHATGPT_STATUS, status);
    r.add(CHATGPT_START, start);
    r.add(CHATGPT_PASTE, paste);
    r.add(CHATGPT_CANCEL, cancel);
    r.add(CHATGPT_SAVE, save);
    r.add(CHATGPT_SIGN_OUT, sign_out);
    r.add(CHATGPT_FORGET, forget);
    r.add(CHATGPT_REVOKE, revoke);
}

fn push<T: Serialize>(tx: &UnboundedSender<Pushed>, name: &str, data: &T) {
    let _ = tx.send(Pushed { name: name.into(), data: encode(data) });
}

fn status(l: &Rc<Local>, args: serde_json::Value) -> Answer {
    ready(args, |(): ()| Ok(l.providers.store().chatgpt_account().map(|a| a.signed_in())))
}

/// Starts the browser sign-in; the callback listener binds in this process.
fn start(l: &Rc<Local>, args: serde_json::Value) -> Answer {
    ready(args, |a: Flow| {
        let store = l.providers.store().clone();
        let host = store.chatgpt_host_id().map_err(|e| e.to_string())?;
        let endpoints = store.chatgpt_endpoints();
        let known = store.chatgpt_known();
        let flow = a.flow;
        let cancel = CancellationToken::new();
        let (paste, mut paste_rx) = unbounded_channel();
        RUNNING.with(|r| r.borrow_mut().insert(flow, Running { cancel: cancel.clone(), paste }));
        let tx = l.push.clone();
        tokio::task::spawn_local(async move {
            let req = oauth::LoginRequest { endpoints: &endpoints, host_id: &host, known, cancel };
            let (url_tx, notice_tx) = (tx.clone(), tx.clone());
            let mut on_url = |url: &str| push(&url_tx, URL, &Url { flow, url: url.to_string() });
            let mut notice = |text: &str| push(&notice_tx, NOTICE, &Notice { flow, text: text.to_string() });
            let r = oauth::login(req, &mut on_url, &mut notice, &mut paste_rx).await;
            RUNNING.with(|r| r.borrow_mut().remove(&flow));
            let error = match r {
                Ok(account) => {
                    FINISHED.with(|f| f.borrow_mut().insert(flow, account));
                    None
                }
                Err(e) => Some(e.to_string()),
            };
            push(&tx, DONE, &Done { flow, error });
        });
        Ok(())
    })
}

/// The redirect URL typed by the user, for a browser that cannot reach this machine.
fn paste(_: &Rc<Local>, args: serde_json::Value) -> Answer {
    ready(args, |a: Paste| {
        RUNNING.with(|r| {
            if let Some(f) = r.borrow().get(&a.flow) {
                let _ = f.paste.send(a.text);
            }
        });
        Ok(())
    })
}

fn cancel(_: &Rc<Local>, args: serde_json::Value) -> Answer {
    ready(args, |a: Flow| {
        if let Some(f) = RUNNING.with(|r| r.borrow_mut().remove(&a.flow)) {
            f.cancel.cancel();
        }
        FINISHED.with(|f| f.borrow_mut().remove(&a.flow));
        Ok(())
    })
}

/// Saves a finished sign-in by the same rules as `mu auth login`.
fn save(l: &Rc<Local>, args: serde_json::Value) -> Answer {
    ready(args, |a: Flow| {
        let account = FINISHED
            .with(|f| f.borrow_mut().remove(&a.flow))
            .ok_or_else(|| "no finished ChatGPT sign-in to save".to_string())?;
        let saved = signin::save(l.providers.store(), account)?;
        Ok(Saved { label: saved.label, plan_usage: saved.plan_usage, welcome: saved.welcome })
    })
}

fn gone(account: Account) -> Gone {
    let label = account.label();
    let revoke = account.signed_in().then(|| {
        let id = NEXT_REVOKE.with(|n| {
            n.set(n.get() + 1);
            n.get()
        });
        TO_REVOKE.with(|t| t.borrow_mut().insert(id, account));
        id
    });
    Gone { label, revoke }
}

fn sign_out(l: &Rc<Local>, args: serde_json::Value) -> Answer {
    ready(args, |(): ()| Ok(signin::sign_out(l.providers.store())?.map(gone)))
}

fn forget(l: &Rc<Local>, args: serde_json::Value) -> Answer {
    ready(args, |(): ()| Ok(signin::forget(l.providers.store())?.map(gone)))
}

/// Revokes a dropped account's refresh token at the issuer.
fn revoke(l: &Rc<Local>, args: serde_json::Value) -> Answer {
    let store = l.providers.store().clone();
    pending(args, move |a: Revoke| {
        let account = TO_REVOKE.with(|t| t.borrow_mut().remove(&a.revoke));
        async move {
            match account {
                Some(account) => signin::revoke(&store, &account).await,
                None => Err("no ChatGPT sign-in to revoke".to_string()),
            }
        }
    })
}

/// Puts a finished sign-in where `chatgpt_save` finds it, as a completed `chatgpt_start` does.
#[cfg(test)]
pub(crate) fn finish_for_test(flow: u64, account: Account) {
    FINISHED.with(|f| f.borrow_mut().insert(flow, account));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replies_and_pushed_values_survive_the_wire() {
        let done = Done { flow: 3, error: Some("timed out".into()) };
        assert_eq!(serde_json::from_value::<Done>(encode(&done)).unwrap(), done);
        let saved = Saved { label: "me@example.com".into(), plan_usage: true, welcome: false };
        assert_eq!(serde_json::from_value::<Saved>(encode(&saved)).unwrap(), saved);
        let gone = Some(Gone { label: "me".into(), revoke: Some(2) });
        assert_eq!(serde_json::from_value::<Option<Gone>>(encode(&gone)).unwrap(), gone);
        let none: Option<Gone> = None;
        assert_eq!(serde_json::from_value::<Option<Gone>>(encode(&none)).unwrap(), none);
        let url = Url { flow: 1, url: "https://x/?a=b".into() };
        assert_eq!(serde_json::from_value::<Url>(encode(&url)).unwrap(), url);
    }
}
