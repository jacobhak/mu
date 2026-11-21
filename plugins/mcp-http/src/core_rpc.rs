//! Calls from the bridge to the core. A `CoreRpc` handle queues them; whoever owns stdout drains the
//! `Pump`, writes each line, and hands the core's replies back to it.

use std::collections::HashMap;

use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};

/// The bridge's own request ids start here, so they do not collide with the core's small ids.
pub const TOKEN_ID_BASE: u64 = 1 << 40;

type Reply = oneshot::Sender<Result<Value, String>>;

enum Call {
    Request { method: String, params: Value, reply: Reply },
    Notify { method: String, params: Value },
}

#[derive(Clone)]
pub struct CoreRpc {
    tx: mpsc::UnboundedSender<Call>,
}

pub struct Pump {
    rx: mpsc::UnboundedReceiver<Call>,
    waiting: HashMap<u64, Reply>,
    next: u64,
}

pub fn channel() -> (CoreRpc, Pump) {
    let (tx, rx) = mpsc::unbounded_channel();
    (CoreRpc { tx }, Pump { rx, waiting: HashMap::new(), next: 0 })
}

impl CoreRpc {
    /// Calls a core method and waits for its answer; `Err` carries the core's message.
    pub async fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        let (reply, rx) = oneshot::channel();
        let call = Call::Request { method: method.into(), params, reply };
        self.tx.send(call).map_err(|_| "the core closed the connection".to_string())?;
        rx.await.map_err(|_| "the core closed the connection".to_string())?
    }

    pub fn notify(&self, method: &str, params: Value) {
        let _ = self.tx.send(Call::Notify { method: method.into(), params });
    }
}

impl Pump {
    /// The next line to write to the core. Cancel-safe.
    pub async fn next_line(&mut self) -> Option<Value> {
        let call = self.rx.recv().await?;
        Some(self.line(call))
    }

    /// Lines queued but not yet taken.
    pub fn drain(&mut self) -> Vec<Value> {
        let mut out = Vec::new();
        while let Ok(call) = self.rx.try_recv() {
            out.push(self.line(call));
        }
        out
    }

    fn line(&mut self, call: Call) -> Value {
        match call {
            Call::Notify { method, params } => json!({"jsonrpc": "2.0", "method": method, "params": params}),
            Call::Request { method, params, reply } => {
                let id = TOKEN_ID_BASE + self.next;
                self.next += 1;
                self.waiting.insert(id, reply);
                json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
            }
        }
    }

    /// Completes the call a response from the core answers. False when it answers none.
    pub fn on_reply(&mut self, v: &Value) -> bool {
        if v.get("method").is_some() {
            return false;
        }
        let Some(reply) = v.get("id").and_then(Value::as_u64).and_then(|id| self.waiting.remove(&id)) else {
            return false;
        };
        let result = match v.pointer("/error/message").and_then(Value::as_str) {
            Some(m) => Err(m.to_string()),
            None => Ok(v.get("result").cloned().unwrap_or(Value::Null)),
        };
        let _ = reply.send(result);
        true
    }
}
