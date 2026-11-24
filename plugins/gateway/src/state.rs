//! Where state lives, and what the daemon and its clients share through it.
//!
//! `$MU_GATEWAY_HOME` (else `<mu home>/gateway`) holds, per omp profile, `profiles/<name>/{state.json, leases/,
//! daemon.log}` (the profile-less one sits in the home itself). The daemon serves one profile's credentials and
//! `models.yml`, so each profile gets its own. The pi backend keeps the same layout under `$MU_GATEWAY_PI_HOME`
//! (else `<mu home>/gateway-pi`), plus a `token` file.

use mu_common::fs::FileLock;
pub use mu_common::paths::self_exe;
use mu_common::paths::{mu_home, profile, profile_subdir};
use serde_json::{Value, json};
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const IO_TIMEOUT: Duration = Duration::from_secs(2);

/// What the gateway wraps: oh-my-pi's auth broker and gateway, or pi running the proxy extension.
#[derive(Clone, Copy, PartialEq, Eq, Debug, clap::ValueEnum)]
pub enum Backend {
    Omp,
    Pi,
}

static BACKEND: OnceLock<Backend> = OnceLock::new();

impl Backend {
    pub fn name(self) -> &'static str {
        match self {
            Self::Omp => "omp",
            Self::Pi => "pi",
        }
    }

    /// Prefix of the daemon's log lines.
    pub fn log_prefix(self) -> &'static str {
        match self {
            Self::Omp => "mu-gateway",
            Self::Pi => "mu-gateway-pi",
        }
    }
}

/// The flag wins, then `$MU_GATEWAY_BACKEND`, then omp.
pub fn resolve_backend(flag: Option<Backend>, env: Option<&str>) -> Result<Backend, String> {
    if let Some(b) = flag {
        return Ok(b);
    }
    match env.map(str::trim).filter(|v| !v.is_empty()) {
        None | Some("omp") => Ok(Backend::Omp),
        Some("pi") => Ok(Backend::Pi),
        Some(v) => Err(format!("MU_GATEWAY_BACKEND must be `omp` or `pi`, not `{v}`")),
    }
}

/// Fixes the backend for this process; call once, before anything reads state.
pub fn set_backend(b: Backend) {
    let _ = BACKEND.set(b);
}

pub fn backend() -> Backend {
    BACKEND.get().copied().unwrap_or(Backend::Omp)
}

fn env_dir(key: &str) -> Option<PathBuf> {
    std::env::var_os(key).filter(|v| !v.is_empty()).map(PathBuf::from)
}

pub fn home_dir() -> PathBuf {
    home_dir_for(backend())
}

fn home_dir_for(b: Backend) -> PathBuf {
    match b {
        Backend::Omp => env_dir("MU_GATEWAY_HOME").unwrap_or_else(|| mu_home().join("gateway")),
        Backend::Pi => env_dir("MU_GATEWAY_PI_HOME").unwrap_or_else(|| mu_home().join("gateway-pi")),
    }
}

/// The pi binary: `$MU_GATEWAY_PI`, else the `pi` next to this executable when it is a file, else `pi` on `PATH`.
pub fn pi_bin() -> PathBuf {
    let exe_dir = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf));
    pi_bin_in(env_dir("MU_GATEWAY_PI"), exe_dir.as_deref())
}

fn pi_bin_in(env: Option<PathBuf>, exe_dir: Option<&Path>) -> PathBuf {
    if let Some(p) = env {
        return p;
    }
    let pinned = exe_dir.map(|d| d.join("pi"));
    pinned.filter(|p| p.is_file()).unwrap_or_else(|| PathBuf::from("pi"))
}

/// The proxy extension: `$MU_GATEWAY_PI_PROXY`, else `proxy.mjs` in the working directory (mu starts plugins in
/// their own directory), else `<exe dir>/../plugins/gateway-pi/proxy.mjs`. Falls back to the last when none exists,
/// so the error names a real path.
pub fn proxy_path() -> PathBuf {
    let cwd = std::env::current_dir().ok();
    let exe_dir = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf));
    proxy_path_in(env_dir("MU_GATEWAY_PI_PROXY"), cwd.as_deref(), exe_dir.as_deref())
}

fn proxy_path_in(env: Option<PathBuf>, cwd: Option<&Path>, exe_dir: Option<&Path>) -> PathBuf {
    if let Some(p) = env {
        return p;
    }
    if let Some(p) = cwd.map(|d| d.join("proxy.mjs")).filter(|p| p.is_file()) {
        return p;
    }
    exe_dir.map_or_else(
        || PathBuf::from("proxy.mjs"),
        |d| d.join("..").join("plugins").join("gateway-pi").join("proxy.mjs"),
    )
}

/// `<dir>/token`: the pi proxy's bearer token, created (0600) before the proxy starts.
pub fn token_path(dir: &Path) -> PathBuf {
    dir.join("token")
}

/// A fresh random token written to `<dir>/token` with mode 0600 (replacing any old one).
pub fn write_token(dir: &Path) -> io::Result<String> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mut raw = [0u8; 32];
    getrandom::fill(&mut raw).map_err(|e| io::Error::other(e.to_string()))?;
    let token: String = raw.iter().map(|b| format!("{b:02x}")).collect();
    let path = token_path(dir);
    let tmp = dir.join("token.tmp");
    let _ = fs::remove_file(&tmp);
    let mut f = OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
    f.write_all(token.as_bytes())?;
    f.set_permissions(fs::Permissions::from_mode(0o600))?;
    fs::rename(tmp, path)?;
    Ok(token)
}

/// The omp binary: `$MU_GATEWAY_OMP`, else the `omp` next to this executable (where mu's installer puts the
/// pinned copy) when it is a file, else `omp` on `PATH`.
pub fn omp_bin() -> PathBuf {
    if let Some(p) = env_dir("MU_GATEWAY_OMP") {
        return p;
    }
    let pinned = std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.join("omp")));
    pinned.filter(|p| p.is_file()).unwrap_or_else(|| PathBuf::from("omp"))
}

pub fn gw_dir() -> PathBuf {
    gw_dir_for(profile().as_deref())
}

fn gw_dir_for(profile: Option<&str>) -> PathBuf {
    profile_subdir(home_dir(), profile)
}

#[cfg(test)]
fn gw_dir_of(b: Backend, profile: Option<&str>) -> PathBuf {
    profile_subdir(home_dir_for(b), profile)
}

pub fn daemon_log() -> PathBuf {
    gw_dir().join("daemon.log")
}

/// Exclusive lock on `<dir>/lock`; released when the returned guard drops.
pub fn lock(dir: &Path) -> io::Result<FileLock> {
    FileLock::exclusive(&dir.join("lock"))
}

/// Non-blocking variant; `None` when another process holds the lock.
pub fn try_lock(dir: &Path) -> io::Result<Option<FileLock>> {
    FileLock::try_exclusive(&dir.join("lock"))
}

pub struct State {
    pub pid: i32,
    pub url: String,
    /// The daemon's omp children (broker, gateway), each the leader of its own process group. Absent
    /// in state.json files written before this field existed.
    pub kids: Vec<i32>,
}

pub fn read_state(dir: &Path) -> Option<State> {
    let v: Value = serde_json::from_str(&fs::read_to_string(dir.join("state.json")).ok()?).ok()?;
    Some(State {
        pid: v["pid"].as_i64()? as i32,
        url: v["url"].as_str()?.to_string(),
        kids: v["kids"]
            .as_array()
            .map_or_else(Vec::new, |a| a.iter().filter_map(|k| k.as_i64()).map(|k| k as i32).collect()),
    })
}

/// Writes a state with no children recorded (tests set up states this way).
#[cfg(test)]
pub fn write_state(dir: &Path, pid: u32, url: &str, broker_url: &str) -> io::Result<()> {
    write_state_with_kids(dir, pid, url, broker_url, &[])
}

/// As `write_state`, also recording the daemon's child pids, so a later start can clean them up
/// after a crash.
pub fn write_state_with_kids(dir: &Path, pid: u32, url: &str, broker_url: &str, kids: &[i32]) -> io::Result<()> {
    let started = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
    let tmp = dir.join("state.json.tmp");
    let body = json!({"pid": pid, "url": url, "brokerUrl": broker_url, "kids": kids, "startedAt": started});
    fs::write(&tmp, body.to_string())?;
    fs::rename(tmp, dir.join("state.json"))
}

/// Removes `state.json` only when it still names `pid`, so a daemon that replaced us is not clobbered.
pub fn remove_state_if_owned(dir: &Path, pid: u32) {
    if read_state(dir).is_some_and(|s| s.pid == pid as i32) {
        let _ = fs::remove_file(dir.join("state.json"));
    }
}

/// `kill(pid, 0)`: true when the process exists (EPERM counts as alive).
pub fn alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM) }
}

/// `GET {url}/healthz` over a plain TCP socket; true on HTTP 200.
pub fn healthy(url: &str) -> bool {
    let Some(hostport) = url.strip_prefix("http://").map(|r| r.split('/').next().unwrap_or("")) else {
        return false;
    };
    let Some(addr) = hostport.to_socket_addrs().ok().and_then(|mut a| a.next()) else { return false };
    let Ok(mut s) = TcpStream::connect_timeout(&addr, IO_TIMEOUT) else { return false };
    let _ = s.set_read_timeout(Some(IO_TIMEOUT));
    let _ = s.set_write_timeout(Some(IO_TIMEOUT));
    let req = format!("GET /healthz HTTP/1.1\r\nHost: {hostport}\r\nConnection: close\r\n\r\n");
    if s.write_all(req.as_bytes()).is_err() {
        return false;
    }
    let mut buf = [0u8; 64];
    let mut n = 0;
    while n < buf.len() {
        match s.read(&mut buf[n..]) {
            Ok(0) | Err(_) => break,
            Ok(k) => {
                n += k;
                if buf[..n].contains(&b'\n') {
                    break;
                }
            }
        }
    }
    buf[..n].starts_with(b"HTTP/1.1 200") || buf[..n].starts_with(b"HTTP/1.0 200")
}

/// Marks a client pid as using the gateway now (mtime = lease time).
pub fn touch_lease(dir: &Path, pid: u32) -> io::Result<()> {
    let f =
        OpenOptions::new().create(true).write(true).truncate(false).open(dir.join("leases").join(pid.to_string()))?;
    f.set_modified(SystemTime::now())
}

/// Deletes leases whose client process is gone.
pub fn prune_leases(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir.join("leases")) else { return };
    for e in entries.flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|n| n.parse::<i32>().ok()) else { continue };
        if !alive(pid) {
            let _ = fs::remove_file(e.path());
        }
    }
}

/// Leases whose client process is alive.
pub fn live_leases(dir: &Path) -> usize {
    let Ok(entries) = fs::read_dir(dir.join("leases")) else { return 0 };
    entries.flatten().filter_map(|e| e.file_name().to_str()?.parse::<i32>().ok()).filter(|p| alive(*p)).count()
}

/// True when some lease was touched within `idle`.
pub fn has_fresh_lease(dir: &Path, idle: Duration) -> bool {
    let Ok(entries) = fs::read_dir(dir.join("leases")) else { return false };
    entries.flatten().any(|e| {
        let Ok(m) = e.metadata().and_then(|m| m.modified()) else { return false };
        SystemTime::now().duration_since(m).map_or(true, |age| age < idle)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gw_dir_is_per_profile() {
        let base = home_dir();
        assert_eq!(gw_dir_for(None), base);
        assert_eq!(gw_dir_for(Some("private")), base.join("profiles/private"));
        assert_ne!(gw_dir_for(Some("a")), gw_dir_for(Some("b")));
        // Path-like names must not escape or alias another profile's directory.
        assert_eq!(gw_dir_for(Some("../x")), base);
        assert_eq!(gw_dir_for(Some("..")), base);
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("apg-state-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn backend_flag_beats_env_and_default_is_omp() {
        assert_eq!(resolve_backend(None, None), Ok(Backend::Omp));
        assert_eq!(resolve_backend(None, Some("")), Ok(Backend::Omp));
        assert_eq!(resolve_backend(None, Some("pi")), Ok(Backend::Pi));
        assert_eq!(resolve_backend(Some(Backend::Omp), Some("pi")), Ok(Backend::Omp));
        assert_eq!(resolve_backend(Some(Backend::Pi), Some("nonsense")), Ok(Backend::Pi));
        assert!(resolve_backend(None, Some("nonsense")).is_err());
    }

    #[test]
    fn backends_keep_separate_state_dirs() {
        assert_ne!(gw_dir_of(Backend::Omp, None), gw_dir_of(Backend::Pi, None));
        assert_eq!(gw_dir_of(Backend::Pi, Some("p")), home_dir_for(Backend::Pi).join("profiles/p"));
    }

    #[test]
    fn pi_binary_resolution_order() {
        let dir = scratch("pi-bin");
        assert_eq!(pi_bin_in(Some("/x/pi".into()), Some(&dir)), PathBuf::from("/x/pi"));
        assert_eq!(pi_bin_in(None, Some(&dir)), PathBuf::from("pi"), "no pi beside the executable");
        fs::write(dir.join("pi"), "").unwrap();
        assert_eq!(pi_bin_in(None, Some(&dir)), dir.join("pi"));
        assert_eq!(pi_bin_in(None, None), PathBuf::from("pi"));
    }

    #[test]
    fn proxy_resolution_order() {
        let dir = scratch("proxy");
        let exe = dir.join("bin");
        assert_eq!(proxy_path_in(Some("/x/p.mjs".into()), Some(&dir), Some(&exe)), PathBuf::from("/x/p.mjs"));
        assert_eq!(
            proxy_path_in(None, Some(&dir), Some(&exe)),
            exe.join("../plugins/gateway-pi/proxy.mjs"),
            "no proxy.mjs in the cwd"
        );
        fs::write(dir.join("proxy.mjs"), "").unwrap();
        assert_eq!(proxy_path_in(None, Some(&dir), Some(&exe)), dir.join("proxy.mjs"));
    }

    #[test]
    fn the_token_file_is_private_random_and_replaced() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("token");
        let a = write_token(&dir).unwrap();
        assert_eq!(a.len(), 64);
        assert!(a.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(fs::read_to_string(token_path(&dir)).unwrap(), a);
        assert_eq!(fs::metadata(token_path(&dir)).unwrap().permissions().mode() & 0o777, 0o600);
        let b = write_token(&dir).unwrap();
        assert_ne!(a, b);
        assert_eq!(fs::read_to_string(token_path(&dir)).unwrap(), b);
        assert!(!dir.join("token.tmp").exists());
    }

    #[test]
    fn state_round_trips() {
        let dir = scratch("state-rt");
        write_state(&dir, 42, "http://127.0.0.1:1", "http://127.0.0.1:2").unwrap();
        let st = read_state(&dir).expect("state");
        assert_eq!((st.pid, st.url.as_str()), (42, "http://127.0.0.1:1"));
        let raw: Value = serde_json::from_str(&fs::read_to_string(dir.join("state.json")).unwrap()).unwrap();
        assert_eq!(raw["brokerUrl"], "http://127.0.0.1:2", "kept in the file for debugging");
    }

    #[test]
    fn state_records_the_daemon_children() {
        let dir = scratch("state-kids");
        write_state_with_kids(&dir, 42, "http://127.0.0.1:1", "http://127.0.0.1:2", &[7, 8]).unwrap();
        assert_eq!(read_state(&dir).expect("state").kids, [7, 8]);
        write_state(&dir, 42, "http://127.0.0.1:1", "http://127.0.0.1:2").unwrap();
        assert!(read_state(&dir).expect("state").kids.is_empty());
    }

    #[test]
    fn state_without_kids_still_reads() {
        let dir = scratch("state-old");
        fs::write(dir.join("state.json"), r#"{"pid":42,"url":"http://127.0.0.1:1","startedAt":0}"#).unwrap();
        let st = read_state(&dir).expect("state");
        assert_eq!((st.pid, st.url.as_str()), (42, "http://127.0.0.1:1"));
        assert!(st.kids.is_empty());
    }

    #[test]
    fn leases_count_only_live_clients() {
        let dir = scratch("leases");
        fs::create_dir_all(dir.join("leases")).unwrap();
        touch_lease(&dir, std::process::id()).unwrap();
        fs::write(dir.join("leases/not-a-pid"), "").unwrap();
        assert_eq!(live_leases(&dir), 1);
        assert!(has_fresh_lease(&dir, Duration::from_secs(60)));
        prune_leases(&dir);
        assert_eq!(live_leases(&dir), 1);
    }
}
