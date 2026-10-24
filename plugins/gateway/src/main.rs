//! Gateway plugin. `mu-gateway` (no args) serves plugin JSON-RPC on stdio; `mu-gateway daemon`
//! runs the shared broker + gateway that every mu process reuses.

mod daemon;
mod shared;

use mu_common::{paths, rpc};
use serde_json::{Value, json};
use std::fs;

fn main() {
    if std::env::args().nth(1).as_deref() == Some("daemon") {
        std::process::exit(daemon::run());
    }
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("tokio runtime");
    rt.block_on(rpc::serve_stdio(handle));
}

async fn handle(req: rpc::Request, _client: rpc::Client) -> Result<Value, rpc::RpcError> {
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
    let token = fs::read_to_string(&token_path)
        .map(|t| t.trim().to_string())
        .ok()
        .filter(|t| !t.is_empty())
        .ok_or_else(|| {
            format!("gateway token not found at {}; run `omp auth-gateway token`", token_path.display())
        })?;
    shared::touch_lease(&dir, pid).map_err(|e| e.to_string())?;
    Ok((url, token))
}
