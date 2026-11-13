//! Gateway plugin. `mu-gateway` (no args) serves plugin JSON-RPC on stdio; `mu-gateway daemon`
//! runs the shared broker + gateway that every mu process reuses.

mod daemon;
mod shared;

use mu_common::{paths, rpc};
use serde_json::{Value, json};
use std::fs;
use std::path::Path;

fn main() {
    if std::env::args().nth(1).as_deref() == Some("daemon") {
        std::process::exit(daemon::run());
    }
    if std::env::args().nth(1).as_deref() == Some("guard") {
        let pgid = std::env::args().nth(2).and_then(|p| p.parse().ok()).unwrap_or(0);
        std::process::exit(daemon::guard(pgid));
    }
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("tokio runtime");
    rt.block_on(rpc::serve_stdio(handle));
}

async fn handle(req: rpc::Request) -> Result<Value, rpc::RpcError> {
    match req.method.as_str() {
        "initialize" => Ok(json!({})),
        "provider/endpoint" => {
            let pid = req
                .params
                .get("clientPid")
                .and_then(Value::as_u64)
                .and_then(|p| u32::try_from(p).ok())
                .ok_or_else(|| err(-32602, "provider/endpoint needs clientPid"))?;
            let (base_url, token) = tokio::task::spawn_blocking(move || endpoint(pid))
                .await
                .map_err(|e| err(-32000, e.to_string()))?
                .map_err(|e| err(-32000, e))?;
            Ok(json!({"baseUrl": base_url, "token": token}))
        }
        "broker/endpoint" => {
            let (dir, root) = (shared::gw_dir(), paths::omp_config_root());
            tokio::task::spawn_blocking(move || broker_endpoint_rpc(&dir, &root))
                .await
                .map_err(|e| err(-32000, e.to_string()))?
        }
        "shutdown" => Ok(Value::Null),
        m => Err(err(-32601, format!("method not found: {m}"))),
    }
}

fn err(code: i64, message: impl Into<String>) -> rpc::RpcError {
    rpc::RpcError { code, message: message.into() }
}

/// Returns a live gateway URL (starting the daemon if needed) and its bearer token; touches the client's lease.
fn endpoint(pid: u32) -> Result<(String, String), String> {
    let dir = shared::gw_dir();
    fs::create_dir_all(dir.join("leases")).map_err(|e| e.to_string())?;
    let _lock = shared::lock(&dir).map_err(|e| e.to_string())?;
    let url = match shared::read_state(&dir) {
        Some(st) if shared::alive(st.pid) && shared::healthy(&st.url) => st.url,
        _ => daemon::start(&dir)?,
    };
    let token_path = paths::omp_config_root().join("auth-gateway.token");
    let token =
        fs::read_to_string(&token_path).map(|t| t.trim().to_string()).ok().filter(|t| !t.is_empty()).ok_or_else(
            || format!("gateway token not found at {}; run `omp auth-gateway token`", token_path.display()),
        )?;
    shared::touch_lease(&dir, pid).map_err(|e| e.to_string())?;
    Ok((url, token))
}

/// `broker/endpoint`: the broker URL and token, only while the daemon pair is alive. Never starts the daemon.
fn broker_endpoint_rpc(dir: &Path, omp_root: &Path) -> Result<Value, rpc::RpcError> {
    let (url, token) = broker_endpoint(dir, omp_root).map_err(|e| err(-32000, e))?;
    Ok(json!({"url": url, "token": token}))
}

fn broker_endpoint(dir: &Path, omp_root: &Path) -> Result<(String, String), String> {
    let st = shared::read_state(dir)
        .filter(|st| shared::alive(st.pid) && shared::healthy(&st.url))
        .ok_or_else(|| "gateway not running".to_string())?;
    let url = st.broker_url.ok_or_else(|| "gateway broker URL unknown; restart the gateway daemon".to_string())?;
    let token_path = omp_root.join("auth-broker.token");
    let token =
        fs::read_to_string(&token_path).map(|t| t.trim().to_string()).ok().filter(|t| !t.is_empty()).ok_or_else(
            || format!("broker token not found at {}; run `omp auth-gateway token`", token_path.display()),
        )?;
    Ok((url, token))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::thread;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("mu-gw-main-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn broker_endpoint_errors_when_no_daemon_is_alive() {
        let dir = scratch("none");
        let err = broker_endpoint_rpc(&dir, &dir).unwrap_err();
        assert_eq!(err.code, -32000);
        assert_eq!(err.message, "gateway not running");
    }

    #[test]
    fn broker_endpoint_errors_when_state_is_stale() {
        // Our own pid is alive, but nothing answers at the recorded URL.
        let dir = scratch("stale");
        shared::write_state(&dir, std::process::id(), "http://127.0.0.1:1", "http://127.0.0.1:2").unwrap();
        let err = broker_endpoint_rpc(&dir, &dir).unwrap_err();
        assert_eq!(err.message, "gateway not running");
    }

    #[test]
    fn broker_endpoint_returns_url_and_token_when_alive() {
        let dir = scratch("alive");
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let gw_url = format!("http://{}", listener.local_addr().unwrap());
        thread::spawn(move || {
            if let Ok((mut s, _)) = listener.accept() {
                use std::io::Write;
                let _ = s.write_all(b"HTTP/1.1 200 OK\r\n\r\n");
            }
        });
        shared::write_state(&dir, std::process::id(), &gw_url, "http://127.0.0.1:9").unwrap();
        fs::write(dir.join("auth-broker.token"), "tok-123\n").unwrap();
        let v = broker_endpoint_rpc(&dir, &dir).unwrap();
        assert_eq!(v, json!({"url": "http://127.0.0.1:9", "token": "tok-123"}));
    }

    #[test]
    fn broker_endpoint_errors_for_state_without_broker_url() {
        let dir = scratch("old");
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let gw_url = format!("http://{}", listener.local_addr().unwrap());
        thread::spawn(move || {
            if let Ok((mut s, _)) = listener.accept() {
                use std::io::Write;
                let _ = s.write_all(b"HTTP/1.1 200 OK\r\n\r\n");
            }
        });
        fs::write(dir.join("state.json"), json!({"pid": std::process::id(), "url": gw_url}).to_string()).unwrap();
        let err = broker_endpoint_rpc(&dir, &dir).unwrap_err();
        assert!(err.message.contains("broker URL unknown"), "{}", err.message);
    }
}
