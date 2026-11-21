//! What the CLI commands and the mu adapter both do. They block, so async callers run them on a blocking thread.

use crate::{daemon, sealed_state, state};
use serde_json::{Value, json};
use std::fs;
use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const PRICES_TIMEOUT: Duration = Duration::from_secs(60);

pub struct Endpoint {
    pub base_url: String,
    pub token: String,
}

impl Endpoint {
    pub fn to_json(&self) -> Value {
        json!({"baseUrl": self.base_url, "token": self.token})
    }
}

fn read_token(root: &std::path::Path) -> Result<String, String> {
    let path = root.join("auth-gateway.token");
    fs::read_to_string(&path).map(|t| t.trim().to_string()).ok().filter(|t| !t.is_empty()).ok_or_else(|| {
        format!(
            "gateway token not found at {}; run `omp auth-gateway token` (or `ai-provider-gateway login`)",
            path.display()
        )
    })
}

/// A live gateway URL and its bearer token, with a lease touched for `pid`. With `start`, the daemon is started if
/// it is not running; without it, `None` when nothing is running, and no lease is touched.
pub fn endpoint(pid: u32, start: bool) -> Result<Option<Endpoint>, String> {
    let dir = state::gw_dir();
    if !start {
        let Some(st) = state::read_state(&dir).filter(|st| state::alive(st.pid) && state::healthy(&st.url)) else {
            return Ok(None);
        };
        let token = read_token(&sealed_state::credential_root())?;
        return Ok(Some(Endpoint { base_url: st.url, token }));
    }
    fs::create_dir_all(dir.join("leases")).map_err(|e| e.to_string())?;
    let _lock = state::lock(&dir).map_err(|e| e.to_string())?;
    let url = match state::read_state(&dir) {
        Some(st) if state::alive(st.pid) && state::healthy(&st.url) => st.url,
        _ => {
            let url = daemon::start(&dir)?;
            // The daemon may have rotated or created credentials while starting.
            sealed_state::flush();
            url
        }
    };
    let token = read_token(&sealed_state::credential_root())?;
    state::touch_lease(&dir, pid).map_err(|e| e.to_string())?;
    Ok(Some(Endpoint { base_url: url, token }))
}

/// The parsed output of `omp models ls --json`.
pub fn prices() -> Result<Value, String> {
    let mut cmd = Command::new(state::omp_bin());
    cmd.args(["models", "ls", "--json"]).envs(sealed_state::envs());
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run `omp` ({e})"))?;
    let drain = |r: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut r) = r {
                let _ = r.read_to_end(&mut buf);
            }
            buf
        })
    };
    let out = drain(child.stdout.take().map(|s| Box::new(s) as Box<dyn Read + Send>));
    let err = drain(child.stderr.take().map(|s| Box::new(s) as Box<dyn Read + Send>));
    let deadline = Instant::now() + PRICES_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("`omp models ls --json` timed out after {}s", PRICES_TIMEOUT.as_secs()));
            }
            Err(e) => return Err(e.to_string()),
        }
    };
    let out = out.join().unwrap_or_default();
    let err = err.join().unwrap_or_default();
    sealed_state::flush();
    if !status.success() {
        return Err(format!("`omp models ls --json` failed ({status}): {}", String::from_utf8_lossy(&err).trim()));
    }
    serde_json::from_slice(&out).map_err(|e| format!("`omp models ls --json` printed no JSON: {e}"))
}

pub fn status() -> Value {
    let dir = state::gw_dir();
    match state::read_state(&dir).filter(|st| state::alive(st.pid) && state::healthy(&st.url)) {
        Some(st) => json!({"running": true, "pid": st.pid, "url": st.url, "leases": state::live_leases(&dir)}),
        None => json!({"running": false, "pid": null, "url": null, "leases": state::live_leases(&dir)}),
    }
}

/// Stops the daemon; true when one was running.
pub fn stop() -> bool {
    daemon::stop(&state::gw_dir())
}

/// `omp login [provider]` with stdio inherited; replaces this process.
pub fn login(provider: Option<&str>) -> String {
    use std::os::unix::process::CommandExt;
    let mut cmd = Command::new(state::omp_bin());
    cmd.arg("login").args(provider);
    let e = cmd.exec();
    format!("cannot run `omp login` ({e})")
}
