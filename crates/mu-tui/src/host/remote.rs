//! The host of an attached client: every call is a line to the agent, answered by a line back.
//!
//! The connection itself (a command's stdin and stdout) belongs to the driver in `src/run/attach.rs`;
//! it hands this host the sender for the current connection (`connect`), the replies as they come
//! (`reply`) and the values the agent pushes (`push`). While there is no connection a call fails at once,
//! and a call that was waiting when the connection dropped fails with it.

use super::{Answer, Host, HostInfo, Pushed, Reply, Tunnel, TunnelIn};
use mu_base::wire::ClientLine;
use serde_json::Value;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::sync::oneshot;

const LOST: &str = "the connection to the agent was lost";

pub struct RemoteHost {
    info: HostInfo,
    out: RefCell<Option<UnboundedSender<ClientLine>>>,
    next: Cell<u64>,
    waiting: RefCell<HashMap<u64, oneshot::Sender<Reply>>>,
    push_tx: UnboundedSender<Pushed>,
    push_rx: RefCell<Option<UnboundedReceiver<Pushed>>>,
    tunnels: RefCell<HashMap<u64, UnboundedSender<TunnelIn>>>,
}

impl RemoteHost {
    pub fn new(info: HostInfo) -> Rc<Self> {
        let (push_tx, push_rx) = unbounded_channel();
        Rc::new(Self {
            info,
            out: RefCell::new(None),
            next: Cell::new(0),
            waiting: RefCell::new(HashMap::new()),
            push_tx,
            push_rx: RefCell::new(Some(push_rx)),
            tunnels: RefCell::new(HashMap::new()),
        })
    }

    /// Calls go out on `out` from now on.
    pub fn connect(&self, out: UnboundedSender<ClientLine>) {
        *self.out.borrow_mut() = Some(out);
    }

    /// The connection is gone: calls that wait for it fail, and new ones fail until `connect`.
    pub fn disconnect(&self) {
        *self.out.borrow_mut() = None;
        for (_, tx) in self.tunnels.borrow_mut().drain() {
            let _ = tx.send(TunnelIn::Closed(Some(LOST.into())));
        }
        for (_, tx) in self.waiting.borrow_mut().drain() {
            let _ = tx.send(Err(LOST.into()));
        }
    }

    pub fn reply(&self, id: u64, result: Reply) {
        if let Some(tx) = self.waiting.borrow_mut().remove(&id) {
            let _ = tx.send(result);
        }
    }

    pub fn push(&self, name: String, data: Value) {
        let _ = self.push_tx.send(Pushed { name, data });
    }

    pub fn tunnel_line(&self, id: u64, line: String) {
        if let Some(tx) = self.tunnels.borrow().get(&id) {
            let _ = tx.send(TunnelIn::Line(line));
        }
    }

    pub fn tunnel_closed(&self, id: u64, error: Option<String>) {
        if let Some(tx) = self.tunnels.borrow_mut().remove(&id) {
            let _ = tx.send(TunnelIn::Closed(error));
        }
    }
}

impl Host for RemoteHost {
    fn info(&self) -> &HostInfo {
        &self.info
    }

    fn is_remote(&self) -> bool {
        true
    }

    fn tunnel(&self, session: &str) -> Result<Tunnel, String> {
        let Some(out) = self.out.borrow().clone() else { return Err("not connected to the agent".into()) };
        let id = self.next.get() + 1;
        self.next.set(id);
        let (tx, rx) = unbounded_channel();
        self.tunnels.borrow_mut().insert(id, tx);
        if out.send(ClientLine::TunnelOpen { id, session: session.into() }).is_err() {
            self.tunnels.borrow_mut().remove(&id);
            return Err(LOST.into());
        }
        Ok(Tunnel { id, out, rx })
    }

    fn call(&self, name: &str, args: Value) -> Answer {
        if name == super::startup::DETACH {
            // Leaves the agent running: the driver sees the line pass and ends the attach.
            return Answer::Ready(match &*self.out.borrow() {
                Some(out) if out.send(ClientLine::Detach).is_ok() => Ok(Value::Null),
                _ => Err("not connected to the agent".into()),
            });
        }
        let Some(out) = self.out.borrow().clone() else {
            return Answer::Ready(Err("not connected to the agent".into()));
        };
        let id = self.next.get() + 1;
        self.next.set(id);
        let (tx, rx) = oneshot::channel();
        self.waiting.borrow_mut().insert(id, tx);
        if out.send(ClientLine::Host { id, call: name.into(), args }).is_err() {
            self.waiting.borrow_mut().remove(&id);
            return Answer::Ready(Err(LOST.into()));
        }
        Answer::Pending(Box::pin(async move { rx.await.unwrap_or_else(|_| Err(LOST.into())) }))
    }

    fn pushed(&self) -> Option<UnboundedReceiver<Pushed>> {
        self.push_rx.borrow_mut().take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn a_call_is_a_line_and_its_reply_completes_it() {
        let host = RemoteHost::new(HostInfo::default());
        let (tx, mut lines) = unbounded_channel();
        host.connect(tx);
        let Answer::Pending(fut) = host.call("skills", json!([1])) else { panic!("not pending") };
        let Some(ClientLine::Host { id, call, args }) = lines.recv().await else { panic!("no host line") };
        assert_eq!((call.as_str(), args), ("skills", json!([1])));
        host.reply(id, Ok(json!(["a"])));
        assert_eq!(fut.await, Ok(json!(["a"])));
    }

    #[tokio::test]
    async fn calls_fail_without_a_connection_and_waiting_ones_fail_when_it_drops() {
        let host = RemoteHost::new(HostInfo::default());
        let Answer::Ready(Err(e)) = host.call("x", Value::Null) else { panic!("not an immediate error") };
        assert!(e.contains("not connected"));
        let (tx, _lines) = unbounded_channel();
        host.connect(tx);
        let Answer::Pending(fut) = host.call("x", Value::Null) else { panic!("not pending") };
        host.disconnect();
        assert_eq!(fut.await, Err(LOST.to_string()));
    }

    #[tokio::test]
    async fn detach_is_a_line_of_its_own() {
        let host = RemoteHost::new(HostInfo::default());
        let (tx, mut lines) = unbounded_channel();
        host.connect(tx);
        assert!(matches!(host.call(crate::host::startup::DETACH, Value::Null), Answer::Ready(Ok(_))));
        assert!(matches!(lines.recv().await, Some(ClientLine::Detach)));
    }
}
