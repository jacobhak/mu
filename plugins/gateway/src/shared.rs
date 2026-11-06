//! `$MU_HOME/gateway/` (`$MU_HOME/gateway/profiles/<name>/` under an omp profile): state shared by the
//! plugin process and the daemon. The daemon serves one profile's credentials and `models.yml`, so each
//! profile gets its own.

use fs4::FileExt;
use mu_common::paths;
use serde_json::{Value, json};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const IO_TIMEOUT: Duration = Duration::from_secs(2);

pub fn gw_dir() -> PathBuf {
    gw_dir_for(paths::profile().as_deref())
}

fn gw_dir_for(profile: Option<&str>) -> PathBuf {
    paths::profile_subdir(paths::mu_home().join("gateway"), profile)
}

pub fn logs_dir() -> PathBuf {
    paths::mu_home().join("logs")
}

pub fn daemon_log() -> PathBuf {
    logs_dir().join("gateway-daemon.log")
}

/// Exclusive lock on `gateway/lock`; released when the returned file drops.
pub fn lock(dir: &Path) -> io::Result<File> {
    let f = open_lock(dir)?;
    FileExt::lock(&f)?;
    Ok(f)
}

/// Non-blocking variant; `None` when another process holds the lock.
pub fn try_lock(dir: &Path) -> io::Result<Option<File>> {
    let f = open_lock(dir)?;
    match FileExt::try_lock(&f) {
        Ok(()) => Ok(Some(f)),
        Err(fs4::TryLockError::WouldBlock) => Ok(None),
        Err(fs4::TryLockError::Error(e)) => Err(e),
    }
}

fn open_lock(dir: &Path) -> io::Result<File> {
    OpenOptions::new().create(true).read(true).write(true).truncate(false).open(dir.join("lock"))
}

pub struct State {
    pub pid: i32,
    pub url: String,
    /// The `omp auth-broker` URL. Absent in state.json files written before `broker/endpoint` existed.
    pub broker_url: Option<String>,
    /// The daemon's omp children (broker, gateway), each the leader of its own process group. Absent
    /// in state.json files written before this field existed.
    pub kids: Vec<i32>,
}

pub fn read_state(dir: &Path) -> Option<State> {
    let v: Value = serde_json::from_str(&fs::read_to_string(dir.join("state.json")).ok()?).ok()?;
    Some(State {
        pid: v["pid"].as_i64()? as i32,
        url: v["url"].as_str()?.to_string(),
        broker_url: v["brokerUrl"].as_str().filter(|u| !u.is_empty()).map(str::to_string),
        kids: v["kids"]
            .as_array()
            .map_or_else(Vec::new, |a| a.iter().filter_map(|k| k.as_i64()).map(|k| k as i32).collect()),
    })
}

/// Writes a state with no children recorded (the tests in `main.rs` set up states this way).
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
        let base = paths::mu_home().join("gateway");
        assert_eq!(gw_dir_for(None), base);
        assert_eq!(gw_dir_for(Some("private")), base.join("profiles/private"));
        assert_ne!(gw_dir_for(Some("a")), gw_dir_for(Some("b")));
        // Path-like names must not escape or alias another profile's directory.
        assert_eq!(gw_dir_for(Some("../x")), base);
        assert_eq!(gw_dir_for(Some("..")), base);
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mu-gw-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn state_round_trips_broker_url() {
        let dir = scratch("state-rt");
        write_state(&dir, 42, "http://127.0.0.1:1", "http://127.0.0.1:2").unwrap();
        let st = read_state(&dir).expect("state");
        assert_eq!((st.pid, st.url.as_str()), (42, "http://127.0.0.1:1"));
        assert_eq!(st.broker_url.as_deref(), Some("http://127.0.0.1:2"));
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
    fn state_without_broker_url_still_reads() {
        // A state.json written before the field existed must still parse, with no broker URL.
        let dir = scratch("state-old");
        fs::write(dir.join("state.json"), r#"{"pid":42,"url":"http://127.0.0.1:1","startedAt":0}"#).unwrap();
        let st = read_state(&dir).expect("state");
        assert_eq!((st.pid, st.url.as_str()), (42, "http://127.0.0.1:1"));
        assert!(st.broker_url.is_none());
    }
}
