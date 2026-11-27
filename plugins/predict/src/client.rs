//! A plugin's side of the shared daemon: finds or starts it, takes a lease, and sends it requests. Blocking;
//! the plugin calls it from the blocking pool.

use crate::daemon::{self, IO_TIMEOUT};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const START_TIMEOUT: Duration = Duration::from_secs(30);
const START_POLL: Duration = Duration::from_millis(50);

static ACTIVE: AtomicBool = AtomicBool::new(false);
/// How many of the daemon's notes this process has passed on.
static SEEN: AtomicUsize = AtomicUsize::new(0);
static LEASED: Mutex<Option<Instant>> = Mutex::new(None);

#[derive(Debug)]
pub enum CallError {
    /// No daemon answered: it is gone, or the connection broke.
    Down(String),
    /// The daemon did not answer in time; it may still be working.
    Timeout,
    /// The daemon answered with an error.
    Remote(String),
}

/// `MU_PREDICT_SHARED=0` keeps each session's engine to itself.
pub fn enabled() -> bool {
    std::env::var("MU_PREDICT_SHARED").map_or(true, |v| v.trim() != "0")
}

pub fn active() -> bool {
    ACTIVE.load(Ordering::SeqCst)
}

pub fn deactivate() {
    ACTIVE.store(false, Ordering::SeqCst);
}

/// Joins the profile's daemon, starting it when none runs. `Err` says why this process should run its own engine.
pub fn connect(agent_dir: &Path, mu_home: &Path) -> Result<(), String> {
    let dir = daemon::dir();
    mu_common::fs::create_private_dir(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let _lock = mu_common::fs::FileLock::exclusive(&daemon::lock_path(&dir)).map_err(|e| format!("lock: {e}"))?;
    let identity = daemon::identity(agent_dir, mu_home);
    match daemon::read_state(&dir).filter(|s| daemon::alive(s.pid)) {
        Some(st) => {
            if st.identity != identity {
                return Err("a daemon of another build or configuration is running".into());
            }
            exchange("hello", Value::Null).map_err(|e| format!("the daemon does not answer ({e:?})"))?;
        }
        None => start(&dir, agent_dir, mu_home)?,
    }
    daemon::touch_lease(&dir).map_err(|e| format!("lease: {e}"))?;
    *LEASED.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
    ACTIVE.store(true, Ordering::SeqCst);
    Ok(())
}

/// Spawns the daemon, detached, and waits until it answers. Called with the lock held.
fn start(dir: &Path, agent_dir: &Path, mu_home: &Path) -> Result<(), String> {
    daemon::forget(dir, None);
    let log_path = daemon::log_path(dir);
    let log = mu_common::fs::open_private_append(&log_path).map_err(|e| format!("log: {e}"))?;
    let exe = mu_common::paths::self_exe().map_err(|e| e.to_string())?;
    let mut cmd = Command::new(exe);
    cmd.arg("daemon")
        .arg("--agent-dir")
        .arg(agent_dir)
        .arg("--mu-home")
        .arg(mu_home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log);
    // SAFETY: setsid only detaches the child from this process's session; it touches no shared state.
    unsafe {
        cmd.pre_exec(|| if libc::setsid() < 0 { Err(std::io::Error::last_os_error()) } else { Ok(()) });
    }
    let mut child = cmd.spawn().map_err(|e| format!("cannot start the daemon: {e}"))?;
    let pid = child.id();
    let exited = std::sync::Arc::new(AtomicBool::new(false));
    let flag = exited.clone();
    std::thread::spawn(move || {
        let _ = child.wait();
        flag.store(true, Ordering::SeqCst);
    });
    let why = || format!("the daemon did not start; see {}", log_path.display());
    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        if daemon::read_state(dir).is_some_and(|s| s.pid == pid as i32) && exchange("hello", Value::Null).is_ok() {
            return Ok(());
        }
        if exited.load(Ordering::SeqCst) {
            return Err(why());
        }
        if Instant::now() >= deadline {
            // SAFETY: SIGTERM to the daemon this call just started.
            unsafe { libc::kill(pid as i32, libc::SIGTERM) };
            return Err(why());
        }
        std::thread::sleep(START_POLL);
    }
}

/// Sends `method` to the daemon, keeping the lease fresh.
pub fn call(method: &str, params: Value) -> Result<Value, CallError> {
    refresh_lease();
    exchange(method, params)
}

fn refresh_lease() {
    let mut last = LEASED.lock().unwrap_or_else(|e| e.into_inner());
    if last.is_none_or(|t| t.elapsed() >= daemon::lease_interval()) {
        let _ = daemon::touch_lease(&daemon::dir());
        *last = Some(Instant::now());
    }
}

fn exchange(method: &str, params: Value) -> Result<Value, CallError> {
    let down = |e: std::io::Error| CallError::Down(e.to_string());
    let mut stream = UnixStream::connect(daemon::socket_path(&daemon::dir())).map_err(down)?;
    stream.set_read_timeout(Some(IO_TIMEOUT)).map_err(down)?;
    stream.set_write_timeout(Some(IO_TIMEOUT)).map_err(down)?;
    let req = json!({
        "method": method,
        "params": params,
        "client": std::process::id().to_string(),
        "seen": SEEN.load(Ordering::SeqCst),
    });
    stream.write_all(format!("{req}\n").as_bytes()).map_err(down)?;
    let mut line = String::new();
    match BufReader::new(&stream).read_line(&mut line) {
        Ok(0) => return Err(CallError::Down("the daemon closed the connection".into())),
        Ok(_) => {}
        Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {
            return Err(CallError::Timeout);
        }
        Err(e) => return Err(down(e)),
    }
    let reply: Value = serde_json::from_str(&line).map_err(|e| CallError::Down(format!("bad reply: {e}")))?;
    if let Some(e) = reply["error"].as_str() {
        return Err(CallError::Remote(e.to_owned()));
    }
    pass_on_notes(&reply);
    Ok(reply["result"].clone())
}

/// Shows the user the daemon's notes this process has not shown yet, once however many requests carried them.
fn pass_on_notes(reply: &Value) {
    let notes: Vec<&str> =
        reply["notes"].as_array().map_or_else(Vec::new, |a| a.iter().filter_map(Value::as_str).collect());
    let total = reply["total"].as_u64().unwrap_or(0) as usize;
    let sent = total.saturating_sub(notes.len());
    let before = SEEN.fetch_max(total, Ordering::SeqCst);
    for note in notes.iter().skip(before.saturating_sub(sent)) {
        crate::notice::send(note);
    }
}
