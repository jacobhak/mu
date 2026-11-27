//! The shared daemon: one engine per profile behind a unix socket, for every session's `mu-predict`.
//!
//! Layout under `<profile home>/predict/`: `daemon.sock`, `state.json` (pid and identity), `lock` (held by whoever
//! starts or stops the daemon), `leases/<pid>` (a client's lease: its mtime is the last time it said it was
//! alive) and `daemon.log`. The daemon exits when no lease has been touched for the grace period.
//!
//! The wire is one request per connection: a JSON line `{method, params, client, seen}` answered by one line
//! `{result, notes, total}` or `{error}`. `hello` answers with the daemon's identity.

use crate::local::{self, Submitted};
use crate::{notice, settings};
use mu_common::fs::{FileLock, create_private_dir, with_umask, write_private};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const GRACE_DEFAULT: Duration = Duration::from_secs(600);
const POLL: Duration = Duration::from_millis(250);
pub const IO_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_REQUEST: u64 = 8 << 20;

pub fn dir() -> PathBuf {
    mu_common::paths::profile_home().join("predict")
}

pub fn socket_path(dir: &Path) -> PathBuf {
    dir.join("daemon.sock")
}

pub fn lock_path(dir: &Path) -> PathBuf {
    dir.join("lock")
}

pub fn log_path(dir: &Path) -> PathBuf {
    dir.join("daemon.log")
}

fn state_path(dir: &Path) -> PathBuf {
    dir.join("state.json")
}

/// How long the daemon outlives its last lease. `MU_PREDICT_DAEMON_IDLE_SECS` overrides it (tests).
pub fn grace() -> Duration {
    std::env::var("MU_PREDICT_DAEMON_IDLE_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map_or(GRACE_DEFAULT, Duration::from_secs)
}

/// How often a client refreshes its lease: well inside the grace period.
pub fn lease_interval() -> Duration {
    (grace() / 4).clamp(Duration::from_millis(100), Duration::from_secs(30))
}

/// What must match for a client to share a daemon: the build, the directories it was told and the `[predict]`
/// settings. A client that differs runs its own engine rather than get someone else's configuration.
pub fn identity(agent_dir: &Path, mu_home: &Path) -> String {
    format!("{}|{}|{}|{:?}", exe_stamp(), agent_dir.display(), mu_home.display(), settings::read(mu_home))
}

fn exe_stamp() -> &'static str {
    static STAMP: OnceLock<String> = OnceLock::new();
    STAMP.get_or_init(|| {
        let meta = mu_common::paths::self_exe().and_then(std::fs::metadata);
        let secs = meta
            .as_ref()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_secs());
        format!("{}-{secs}", meta.map_or(0, |m| m.len()))
    })
}

pub struct Running {
    pub pid: i32,
    pub identity: String,
}

pub fn read_state(dir: &Path) -> Option<Running> {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(state_path(dir)).ok()?).ok()?;
    Some(Running { pid: v["pid"].as_i64()? as i32, identity: v["identity"].as_str()?.to_owned() })
}

fn write_state(dir: &Path, identity: &str) -> std::io::Result<()> {
    write_private(&state_path(dir), json!({ "pid": std::process::id(), "identity": identity }).to_string().as_bytes())
}

/// Forgets a daemon's files; `pid` says whose state may be removed (`None`: whoever's it is).
pub fn forget(dir: &Path, pid: Option<u32>) {
    if pid.is_none_or(|p| read_state(dir).is_some_and(|s| s.pid == p as i32)) {
        let _ = std::fs::remove_file(state_path(dir));
        let _ = std::fs::remove_file(socket_path(dir));
    }
}

/// `kill(pid, 0)`: true when the process exists (EPERM counts as alive).
pub fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    unsafe { libc::kill(pid, 0) == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM) }
}

/// Marks this process as using the daemon now.
pub fn touch_lease(dir: &Path) -> std::io::Result<()> {
    let leases = dir.join("leases");
    create_private_dir(&leases)?;
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(leases.join(std::process::id().to_string()))?;
    f.set_modified(SystemTime::now())
}

/// The newest lease time, if any.
fn newest_lease(dir: &Path) -> Option<SystemTime> {
    std::fs::read_dir(dir.join("leases")).ok()?.flatten().filter_map(|e| e.metadata().ok()?.modified().ok()).max()
}

/// Removes leases older than `grace`: their clients are long gone.
fn prune_leases(dir: &Path, grace: Duration) {
    let Ok(entries) = std::fs::read_dir(dir.join("leases")) else { return };
    for e in entries.flatten() {
        let old = e
            .metadata()
            .and_then(|m| m.modified())
            .is_ok_and(|t| SystemTime::now().duration_since(t).is_ok_and(|age| age > grace));
        if old {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

static STOP: AtomicBool = AtomicBool::new(false);
static IDENTITY: OnceLock<String> = OnceLock::new();

extern "C" fn request_stop(_sig: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
}

/// `mu-predict daemon --agent-dir DIR --mu-home DIR`: serves until idle or signalled. The exit code.
pub fn run(args: &[String]) -> i32 {
    let arg = |name: &str| args.windows(2).find(|w| w[0] == name).map(|w| PathBuf::from(&w[1]));
    let (Some(agent_dir), Some(mu_home)) = (arg("--agent-dir"), arg("--mu-home")) else {
        eprintln!("usage: mu-predict daemon --agent-dir DIR --mu-home DIR");
        return 2;
    };
    notice::keep_for_clients();
    for sig in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
        // SAFETY: the handler only stores to an atomic.
        unsafe { libc::signal(sig, request_stop as *const () as libc::sighandler_t) };
    }
    let dir = dir();
    let started = SystemTime::now();
    let identity = identity(&agent_dir, &mu_home);
    let _ = IDENTITY.set(identity.clone());
    if let Err(e) = local::initialize(&agent_dir, &mu_home) {
        eprintln!("predict daemon: {e}");
        return 1;
    }
    let sock = socket_path(&dir);
    let _ = std::fs::remove_file(&sock);
    let listener = match with_umask(0o077, || UnixListener::bind(&sock)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("predict daemon: cannot listen on {}: {e}", sock.display());
            return 1;
        }
    };
    if let Err(e) = write_state(&dir, &identity) {
        eprintln!("predict daemon: cannot write state: {e}");
        return 1;
    }
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            std::thread::spawn(move || serve_connection(conn));
        }
    });
    eprintln!("predict daemon: serving {} (pid {})", sock.display(), std::process::id());
    let grace = grace();
    let idle = |dir: &Path| {
        let last = newest_lease(dir).map_or(started, |t| t.max(started));
        SystemTime::now().duration_since(last).is_ok_and(|age| age > grace)
    };
    loop {
        std::thread::sleep(POLL);
        if STOP.load(Ordering::SeqCst) {
            break;
        }
        if idle(&dir) {
            // A client takes its lease under this lock, so the check cannot race one that is joining.
            let Ok(Some(_lock)) = FileLock::try_exclusive(&lock_path(&dir)) else { continue };
            if idle(&dir) {
                eprintln!("predict daemon: idle, exiting");
                local::shutdown_blocking();
                forget(&dir, Some(std::process::id()));
                prune_leases(&dir, Duration::ZERO);
                return 0;
            }
        }
        prune_leases(&dir, grace);
    }
    local::shutdown_blocking();
    forget(&dir, Some(std::process::id()));
    0
}

fn serve_connection(stream: UnixStream) {
    let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
    let mut line = String::new();
    if BufReader::new((&stream).take(MAX_REQUEST)).read_line(&mut line).unwrap_or(0) == 0 {
        return;
    }
    let reply = match serde_json::from_str::<Value>(&line) {
        Ok(req) => handle(&req),
        Err(e) => json!({ "error": format!("bad request: {e}") }),
    };
    let _ = (&stream).write_all(format!("{reply}\n").as_bytes());
}

fn handle(req: &Value) -> Value {
    let method = req["method"].as_str().unwrap_or_default();
    let client = req["client"].as_str().unwrap_or_default();
    let seen = req["seen"].as_u64().unwrap_or(0) as usize;
    let result = match method {
        "hello" => Ok(json!({ "identity": IDENTITY.get(), "pid": std::process::id() })),
        _ => match local::submit(method, &req["params"], client) {
            Ok(Submitted::Done(v)) => Ok(v),
            Ok(Submitted::Ghost(rx)) => Ok(local::ghost_reply(rx.blocking_recv().unwrap_or_default())),
            Err(e) => Err(e.message),
        },
    };
    match result {
        Ok(v) => {
            let (notes, total) = notice::since(seen);
            json!({ "result": v, "notes": notes, "total": total })
        }
        Err(e) => json!({ "error": e }),
    }
}
