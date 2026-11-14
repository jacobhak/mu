//! What the terminal UI asks of the machine the agent runs on: model lists, stored keys, MCP
//! servers, the session directory, plugins. The UI never holds the provider registry, the context or
//! the plugin host; it holds a `Host` and calls it by name with JSON arguments, which is also the
//! wire form of a remote client.
//!
//! One implementation runs in this process (`local::LocalHost`, answering from the objects the UI used
//! to hold); the other sends the request over a byte stream (stage 4 of the UI and agent split).
//! A call to the local host usually answers at once (`Answer::Ready`); a slow one (a network call,
//! a program) or any call to a remote host is `Answer::Pending`, and the UI handles the reply as a
//! `UiMsg`, as it handles every result of background work.
//!
//! A surface (one row of the host-state inventory) is one module under this one: its argument and reply
//! types (plain serde data, never an `Rc`, a handle or a `'static` definition), a name constant per
//! call, and `register`, which adds the local handlers. The UI calls with `App::host_call(NAME, &args,
//! |app, reply| …)`.
//!
//! Values the host sends unasked arrive as `Pushed` and are routed by `App::on_pushed`.

pub mod chatgpt;
pub mod completion;
pub mod custom;
pub mod keys;
pub mod local;
pub mod mcp;
pub mod memory;
pub mod models;
pub mod plans;
pub mod program;
pub mod remote;
pub mod sessions;
pub mod startup;
pub mod web;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

pub type Reply = Result<Value, String>;

/// The outcome of one call.
pub enum Answer {
    Ready(Reply),
    Pending(Pin<Box<dyn Future<Output = Reply>>>),
}

/// A value the host sends without being asked (a tail update, a sign-in URL): a name and its data.
pub struct Pushed {
    pub name: String,
    pub data: Value,
}

/// What the host tells a client once, when it connects.
#[derive(Clone, Debug, Default, Serialize, serde::Deserialize)]
pub struct HostInfo {
    /// `$HOME` on the host, used to shorten host paths on screen.
    pub home: PathBuf,
    /// The profile's home directory on the host.
    pub profile_home: PathBuf,
    pub tmp: PathBuf,
    pub cwd: PathBuf,
    /// The profile name on the host (`OMP_PROFILE`), shown in the status line.
    #[serde(default)]
    pub profile: Option<String>,
}

static FACTS: std::sync::OnceLock<HostInfo> = std::sync::OnceLock::new();

/// Records what the host said about itself, for the code that shortens host paths on screen.
pub(crate) fn set_facts(info: HostInfo) {
    let _ = FACTS.set(info);
}

/// The host's home directory, to show a path under it as `~`. Without facts (tests), this process's.
pub(crate) fn host_home() -> Option<String> {
    match FACTS.get() {
        Some(i) => Some(i.home.display().to_string()),
        None => std::env::var("HOME").ok(),
    }
    .filter(|h| !h.is_empty())
}

pub(crate) fn host_tmp() -> PathBuf {
    FACTS.get().map_or_else(std::env::temp_dir, |i| i.tmp.clone())
}

pub(crate) fn host_profile() -> Option<String> {
    match FACTS.get() {
        Some(i) => i.profile.clone(),
        None => std::env::var("OMP_PROFILE").or_else(|_| std::env::var("PI_PROFILE")).ok(),
    }
    .filter(|p| !p.is_empty())
}

/// What comes back through a tunnel to another agent on the host.
pub enum TunnelIn {
    Line(String),
    /// The other end closed it, or it could not be opened (the reason).
    Closed(Option<String>),
}

/// A line-for-line pipe to the socket of another agent on the host, carried by the connection to this one.
pub struct Tunnel {
    pub(crate) id: u64,
    pub(crate) out: UnboundedSender<mu_base::wire::ClientLine>,
    pub rx: UnboundedReceiver<TunnelIn>,
}

impl Tunnel {
    pub fn send(&self, line: String) {
        let _ = self.out.send(mu_base::wire::ClientLine::TunnelLine { id: self.id, line });
    }

    pub fn close(&self) {
        let _ = self.out.send(mu_base::wire::ClientLine::TunnelClose { id: self.id });
    }
}

pub trait Host {
    fn info(&self) -> &HostInfo;
    /// Opens a tunnel to the agent of `session` on the host: only a remote host has one.
    fn tunnel(&self, _session: &str) -> Result<Tunnel, String> {
        Err("not attached to an agent".into())
    }
    /// The host is on another machine: programs that take the terminal run there, not here.
    fn is_remote(&self) -> bool {
        false
    }
    fn call(&self, name: &str, args: Value) -> Answer;
    /// The stream of pushed values. Taken once, by the UI, when it starts.
    fn pushed(&self) -> Option<UnboundedReceiver<Pushed>>;
}

/// Calls the host and waits for the answer: for code that is already async (start-up).
pub async fn call<A: Serialize, R: DeserializeOwned>(host: &dyn Host, name: &str, args: &A) -> Result<R, String> {
    decode(match host.call(name, encode(args)) {
        Answer::Ready(r) => r,
        Answer::Pending(f) => f.await,
    })
}

/// Wraps a host so every call answers later, as a remote host's do: for tests and for loopback runs
/// of the real binary, which then exercise the UI's handling of a reply that is not immediate.
pub struct Deferred<H>(pub H);

impl<H: Host> Host for Deferred<H> {
    fn info(&self) -> &HostInfo {
        self.0.info()
    }

    fn is_remote(&self) -> bool {
        self.0.is_remote()
    }

    fn tunnel(&self, session: &str) -> Result<Tunnel, String> {
        self.0.tunnel(session)
    }

    fn call(&self, name: &str, args: Value) -> Answer {
        match self.0.call(name, args) {
            Answer::Ready(r) => Answer::Pending(Box::pin(async move {
                tokio::task::yield_now().await;
                r
            })),
            pending => pending,
        }
    }

    fn pushed(&self) -> Option<UnboundedReceiver<Pushed>> {
        self.0.pushed()
    }
}

pub(crate) fn encode<A: Serialize>(args: &A) -> Value {
    // Argument types are plain data with string keys: serialising them cannot fail.
    serde_json::to_value(args).unwrap_or(Value::Null)
}

pub(crate) fn decode<R: DeserializeOwned>(reply: Reply) -> Result<R, String> {
    serde_json::from_value(reply?).map_err(|e| format!("unreadable reply from the host: {e}"))
}

/// Answers a call from a handler that needs no waiting.
pub(crate) fn ready<A: DeserializeOwned, R: Serialize>(args: Value, f: impl FnOnce(A) -> Result<R, String>) -> Answer {
    Answer::Ready(
        serde_json::from_value(args).map_err(|e| format!("bad arguments: {e}")).and_then(f).map(|r| encode(&r)),
    )
}

/// Answers a call from a handler that waits (a network call, a child program).
pub(crate) fn pending<A: DeserializeOwned, R: Serialize, F: Future<Output = Result<R, String>> + 'static>(
    args: Value,
    f: impl FnOnce(A) -> F,
) -> Answer {
    match serde_json::from_value(args) {
        Ok(a) => {
            let fut = f(a);
            Answer::Pending(Box::pin(async move { fut.await.map(|r| encode(&r)) }))
        }
        Err(e) => Answer::Ready(Err(format!("bad arguments: {e}"))),
    }
}
