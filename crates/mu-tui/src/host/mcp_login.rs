//! Signing in to an OAuth MCP server (table row "MCP"): the flow runs on the host, in the `mu-mcp-http`
//! bridge; the sign-in URL and the pasted redirect cross the wire, the tokens never do.

use super::local::{Local, Registry};
use super::{Answer, Pushed, encode, ready};
use mu_tools::plugin::McpLoginEvent;
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
use tokio_util::sync::CancellationToken;

pub const MCP_LOGIN_START: &str = "mcp_login_start";
pub const MCP_LOGIN_PASTE: &str = "mcp_login_paste";
pub const MCP_LOGIN_CANCEL: &str = "mcp_login_cancel";

/// Pushed: the sign-in link of a waiting login (`Url`).
pub const URL: &str = "mcp_login_url";
/// Pushed: a line the waiting login reports (`Notice`).
pub const NOTICE: &str = "mcp_login_notice";
/// Pushed: the login ended (`Done`).
pub const DONE: &str = "mcp_login_done";

/// Starts a login for a server; the UI picks the id and tells a late result from the current flow's by it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Start {
    pub flow: u64,
    pub name: String,
}

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
    /// Why the login did not finish; none when it did.
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Paste {
    pub flow: u64,
    pub text: String,
}

struct Running {
    cancel: CancellationToken,
    paste: UnboundedSender<String>,
}

thread_local! {
    static RUNNING: RefCell<HashMap<u64, Running>> = RefCell::new(HashMap::new());
}

pub(super) fn register(r: &mut Registry) {
    r.add(MCP_LOGIN_START, start);
    r.add(MCP_LOGIN_PASTE, paste);
    r.add(MCP_LOGIN_CANCEL, cancel);
}

fn push<T: Serialize>(tx: &UnboundedSender<Pushed>, name: &str, data: &T) {
    let _ = tx.send(Pushed { name: name.into(), data: encode(data) });
}

fn start(l: &Rc<Local>, args: serde_json::Value) -> Answer {
    ready(args, |a: Start| {
        let Start { flow, name } = a;
        let cancel = CancellationToken::new();
        let (paste, paste_rx) = unbounded_channel();
        let (events, mut events_rx) = unbounded_channel();
        RUNNING.with(|r| r.borrow_mut().insert(flow, Running { cancel: cancel.clone(), paste }));
        let (plugins, tx) = (l.plugins.clone(), l.push.clone());
        tokio::task::spawn_local(async move {
            let forward = {
                let tx = tx.clone();
                tokio::task::spawn_local(async move {
                    while let Some(e) = events_rx.recv().await {
                        match e {
                            McpLoginEvent::Url(url) => push(&tx, URL, &Url { flow, url }),
                            McpLoginEvent::Notice(text) => push(&tx, NOTICE, &Notice { flow, text }),
                        }
                    }
                })
            };
            let r = plugins.mcp_login(&name, events, paste_rx, cancel).await;
            // The events end with the login; deliver them before its result.
            let _ = forward.await;
            RUNNING.with(|r| r.borrow_mut().remove(&flow));
            push(&tx, DONE, &Done { flow, error: r.err() });
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
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replies_and_pushed_values_survive_the_wire() {
        let done = Done { flow: 3, error: Some("timed out".into()) };
        assert_eq!(serde_json::from_value::<Done>(encode(&done)).unwrap(), done);
        let url = Url { flow: 1, url: "https://x/?a=b".into() };
        assert_eq!(serde_json::from_value::<Url>(encode(&url)).unwrap(), url);
        let start = Start { flow: 2, name: "srv".into() };
        assert_eq!(serde_json::from_value::<Start>(encode(&start)).unwrap(), start);
    }
}
