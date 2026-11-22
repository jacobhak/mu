//! Where state lives, and what the daemon and its clients share through it.
//!
//! `$MU_GATEWAY_HOME` (else `<mu home>/gateway`) holds, per omp profile, `profiles/<name>/{state.json, leases/,
//! daemon.log}` (the profile-less one sits in the home itself). The daemon serves one profile's credentials and
//! `models.yml`, so each profile gets its own.

use mu_common::fs::FileLock;
pub use mu_common::paths::self_exe;
use mu_common::paths::{mu_home, profile, profile_subdir};
use serde_json::{Value, json};
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const IO_TIMEOUT: Duration = Duration::from_secs(2);

fn env_dir(key: &str) -> Option<PathBuf> {
    std::env::var_os(key).filter(|v| !v.is_empty()).map(PathBuf::from)
}

pub fn home_dir() -> PathBuf {
    env_dir("MU_GATEWAY_HOME").unwrap_or_else(|| mu_home().join("gateway"))
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
