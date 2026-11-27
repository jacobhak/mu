//! `mu-plugin`: serves mu's gateway provider contract over stdio JSON-RPC (`provider = "gateway"` in the plugin
//! manifest), and seals omp's credentials through mu's `core/secrets` store (see `sealed_state`).

use crate::commands;
use crate::sealed_state::{self, Info, Secrets};
use crate::state::{self, Backend};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use mu_common::rpc;
use serde_json::{Value, json};
use std::io;
use std::sync::Arc;
use std::time::Duration;

/// How long a `core/secrets` call may take; mu answers within milliseconds.
const CALL_TIMEOUT: Duration = Duration::from_secs(10);
/// mu keeps stdin open and answers `core/secrets` until it gets the `shutdown` reply (it waits up to 5 s for
/// that): bound the final flush below it.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(4);

/// `core/secrets` over the plugin's stdio, callable from a blocking thread.
struct RpcMu {
    runtime: tokio::runtime::Handle,
    timeout_ms: std::sync::atomic::AtomicU64,
}

impl RpcMu {
    fn call(&self, params: Value) -> io::Result<Value> {
        let (tx, rx) = std::sync::mpsc::channel();
        self.runtime.spawn(async move {
            let _ = tx.send(rpc::request("core/secrets", params).await);
        });
        let wait = Duration::from_millis(self.timeout_ms.load(std::sync::atomic::Ordering::SeqCst));
        match rx.recv_timeout(wait) {
            Ok(r) => r.map_err(|e| io::Error::other(e.message)),
            Err(_) => Err(io::Error::new(io::ErrorKind::TimedOut, "core/secrets did not answer")),
        }
    }
}

impl Secrets for RpcMu {
    fn info(&self) -> io::Result<Info> {
        let v = self.call(json!({"op": "info"}))?;
        Ok(Info { sealed: v["sealed"].as_bool().unwrap_or(false), unlocked: v["unlocked"].as_bool().unwrap_or(false) })
    }

    fn get(&self, name: &str) -> io::Result<Option<Vec<u8>>> {
        let v = self.call(json!({"op": "get", "name": name}))?;
        match v["value"].as_str() {
            None => Ok(None),
            Some(b) => STANDARD.decode(b).map(Some).map_err(|_| io::Error::other("core/secrets: value is not base64")),
        }
    }

    fn put(&self, name: &str, bytes: &[u8]) -> io::Result<()> {
        self.call(json!({"op": "put", "name": name, "value": STANDARD.encode(bytes)})).map(|_| ())
    }
}

pub fn run() {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("tokio runtime");
    rt.block_on(async {
        let mu = Arc::new(RpcMu {
            runtime: tokio::runtime::Handle::current(),
            timeout_ms: (CALL_TIMEOUT.as_millis() as u64).into(),
        });
        rpc::serve_stdio(move |req| handle(mu.clone(), req)).await;
    });
}

fn err(code: i64, message: impl Into<String>) -> rpc::RpcError {
    rpc::RpcError::new(code, message)
}

/// Only omp's credentials are sealed; pi keeps its own files where it keeps them.
fn seals() -> bool {
    state::backend() == Backend::Omp
}

/// Runs `f` on a blocking thread, after making sure omp's state is unsealed when mu has it sealed and unlocked.
async fn blocking<T: Send + 'static>(
    mu: Arc<RpcMu>,
    f: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, rpc::RpcError> {
    tokio::task::spawn_blocking(move || {
        if seals() {
            sealed_state::materialize(mu).map_err(|e| format!("cannot unseal omp's credentials: {e}"))?;
        }
        f()
    })
    .await
    .map_err(|e| err(-32000, e.to_string()))?
    .map_err(|e| err(-32000, e))
}

async fn handle(mu: Arc<RpcMu>, req: rpc::Request) -> Result<Value, rpc::RpcError> {
    match req.method.as_str() {
        "initialize" => {
            // Unseal in the background; the first endpoint or prices call waits for it by taking the same lock.
            if seals() {
                let mu = mu.clone();
                tokio::task::spawn_blocking(move || {
                    let _ = sealed_state::materialize(mu);
                });
            }
            Ok(json!({}))
        }
        "provider/endpoint" => {
            let pid = req
                .params
                .get("clientPid")
                .and_then(Value::as_u64)
                .and_then(|p| u32::try_from(p).ok())
                .ok_or_else(|| err(-32602, "provider/endpoint needs clientPid"))?;
            let start = req.params.get("start").and_then(Value::as_bool).unwrap_or(true);
            let found = blocking(mu, move || commands::endpoint(pid, start)).await?;
            Ok(found.map_or(Value::Null, |e| e.to_json()))
        }
        "provider/prices" => blocking(mu, commands::prices).await,
        "provider/logins" => {
            Ok(json!({"providers": commands::login_providers().iter().map(|p| p.to_json()).collect::<Vec<_>>()}))
        }
        "provider/login" => {
            let provider = match req.params.get("provider") {
                None | Some(Value::Null) => None,
                Some(Value::String(p)) => Some(p.clone()),
                Some(_) => return Err(err(-32602, "provider/login: provider must be a string")),
            };
            blocking(mu, move || commands::login_spec(provider.as_deref())).await.map(|s| s.to_json())
        }
        "provider/login_done" => {
            blocking(mu, || {
                commands::login_done();
                Ok(())
            })
            .await?;
            Ok(json!({}))
        }
        "shutdown" => {
            mu.timeout_ms.store(SHUTDOWN_TIMEOUT.as_millis() as u64, std::sync::atomic::Ordering::SeqCst);
            if seals() {
                let _ = tokio::task::spawn_blocking(sealed_state::shutdown).await;
            }
            Ok(Value::Null)
        }
        m => Err(err(-32601, format!("method not found: {m}"))),
    }
}
