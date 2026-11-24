//! What the CLI commands and the mu adapter both do. They block, so async callers run them on a blocking thread.

use crate::state::Backend;
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

/// The bearer token: omp's `auth-gateway.token`, or the pi daemon's own `token` file in `dir`.
fn read_token(dir: &std::path::Path) -> Result<String, String> {
    let (path, hint) = match state::backend() {
        Backend::Omp => (
            sealed_state::credential_root().join("auth-gateway.token"),
            "run `omp auth-gateway token` (or `mu-gateway login`)",
        ),
        Backend::Pi => (state::token_path(dir), "the daemon writes it on start"),
    };
    fs::read_to_string(&path)
        .map(|t| t.trim().to_string())
        .ok()
        .filter(|t| !t.is_empty())
        .ok_or_else(|| format!("gateway token not found at {}; {hint}", path.display()))
}

/// A live gateway URL and its bearer token, with a lease touched for `pid`. With `start`, the daemon is started if
/// it is not running; without it, `None` when nothing is running, and no lease is touched.
pub fn endpoint(pid: u32, start: bool) -> Result<Option<Endpoint>, String> {
    let dir = state::gw_dir();
    if !start {
        let Some(st) = state::read_state(&dir).filter(|st| state::alive(st.pid) && state::healthy(&st.url)) else {
            return Ok(None);
        };
        let token = read_token(&dir)?;
        return Ok(Some(Endpoint { base_url: st.url, token }));
    }
    fs::create_dir_all(dir.join("leases")).map_err(|e| e.to_string())?;
    let _lock = state::lock(&dir).map_err(|e| e.to_string())?;
    let url = match state::read_state(&dir) {
        Some(st) if state::alive(st.pid) && state::healthy(&st.url) => st.url,
        _ => {
            let url = daemon::start(&dir)?;
            if state::backend() == Backend::Omp {
                // The daemon may have rotated or created credentials while starting.
                sealed_state::flush();
            }
            url
        }
    };
    let token = read_token(&dir)?;
    state::touch_lease(&dir, pid).map_err(|e| e.to_string())?;
    Ok(Some(Endpoint { base_url: url, token }))
}

/// The model prices: the parsed output of `omp models ls --json`, or what pi's proxy extension writes in its
/// one-shot prices mode.
pub fn prices() -> Result<Value, String> {
    match state::backend() {
        Backend::Omp => omp_prices(),
        Backend::Pi => pi_prices(),
    }
}

fn pi_prices() -> Result<Value, String> {
    let proxy = state::proxy_path();
    if !proxy.is_file() {
        return Err(format!("pi proxy extension not found at {} (set MU_GATEWAY_PI_PROXY)", proxy.display()));
    }
    let proxy = std::path::absolute(&proxy).unwrap_or(proxy);
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
    let out = std::env::temp_dir().join(format!("mu-gateway-pi-prices-{}-{nanos}.json", std::process::id()));
    let mut cmd = daemon::pi_proxy_command(&proxy);
    // Held open for the run: pi exits on stdin EOF, and the extension stops it itself once the file is written.
    cmd.current_dir(std::env::temp_dir())
        .env("MU_GW_PI_MODE", "prices")
        .env("MU_GW_PI_OUT", &out)
        .stdin(Stdio::piped());
    let label = "pi prices run";
    let result = run_capture(cmd, "pi", label).and_then(|(status, _, err)| {
        if !status.success() {
            return Err(format!("`{label}` failed ({status}): {}", String::from_utf8_lossy(&err).trim()));
        }
        let body = fs::read(&out).map_err(|e| format!("`{label}` wrote no prices file: {e}"))?;
        serde_json::from_slice(&body).map_err(|e| format!("`{label}` wrote no JSON: {e}"))
    });
    let _ = fs::remove_file(&out);
    result
}

fn omp_prices() -> Result<Value, String> {
    let mut cmd = Command::new(state::omp_bin());
    cmd.args(["models", "ls", "--json"]).envs(sealed_state::envs()).stdin(Stdio::null());
    let (status, out, err) = run_capture(cmd, "omp", "omp models ls --json")?;
    sealed_state::flush();
    if !status.success() {
        return Err(format!("`omp models ls --json` failed ({status}): {}", String::from_utf8_lossy(&err).trim()));
    }
    serde_json::from_slice(&out).map_err(|e| format!("`omp models ls --json` printed no JSON: {e}"))
}

/// Runs `cmd` with stdout and stderr captured, killing it after `PRICES_TIMEOUT`.
fn run_capture(
    mut cmd: Command,
    program: &str,
    label: &str,
) -> Result<(std::process::ExitStatus, Vec<u8>, Vec<u8>), String> {
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run `{program}` ({e})"))?;
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
                return Err(format!("`{label}` timed out after {}s", PRICES_TIMEOUT.as_secs()));
            }
            Err(e) => return Err(e.to_string()),
        }
    };
    Ok((status, out.join().unwrap_or_default(), err.join().unwrap_or_default()))
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

/// `omp login [provider]` (`pi auth login [provider]` for pi) with stdio inherited; replaces this process.
pub fn login(provider: Option<&str>) -> String {
    use std::os::unix::process::CommandExt;
    let (mut cmd, shown) = match state::backend() {
        Backend::Omp => {
            let mut cmd = Command::new(state::omp_bin());
            cmd.arg("login");
            (cmd, "omp login")
        }
        Backend::Pi => {
            let mut cmd = Command::new(state::pi_bin());
            cmd.args(["auth", "login"]);
            (cmd, "pi auth login")
        }
    };
    cmd.args(provider);
    let e = cmd.exec();
    format!("cannot run `{shown}` ({e})")
}
