//! Memory plugin shim. `mu` starts one `mu-memory` per session and speaks plugin JSON-RPC on its stdio; this
//! process only relays that to the shared `mu-memory-daemon` over a unix socket, starting the daemon when none
//! is running. The daemon (embedder, sqlite, Bun runtime) is the memory cost, and it exists once per `$MU_HOME`.

use fs4::FileExt;
use mu_common::paths;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const READY_LINE: &[u8] = b"mu-memory ready\n";
const START_TIMEOUT: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_millis(100);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(2);
/// A daemon found exiting (idle shutdown racing our connect) is replaced; this many tries is plenty.
const ATTEMPTS: usize = 3;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("mu-memory: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let stream = connect().map_err(|e| format!("cannot reach the memory daemon: {e}"))?;
    relay(stream)
}

/// `$MU_HOME/memory/`: one daemon for every profile. Each session passes its own agent dir in `initialize`.
fn mem_dir() -> PathBuf {
    paths::mu_home().join("memory")
}

fn sock_path(dir: &Path) -> PathBuf {
    dir.join("sock")
}

/// Connects to a live daemon, starting one under `memory/lock` when needed. The lock is held only while
/// connecting, so concurrent sessions start at most one daemon.
fn connect() -> Result<UnixStream, String> {
    let dir = mem_dir();
    fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir).map_err(|e| e.to_string())?;
    let lock = open_lock(&dir).map_err(|e| e.to_string())?;
    FileExt::lock(&lock).map_err(|e| e.to_string())?;
    let sock = sock_path(&dir);
    let mut last = String::from("daemon did not start");
    for _ in 0..ATTEMPTS {
        if let Some(s) = try_connect(&sock) {
            return Ok(s);
        }
        match start_daemon(&sock) {
            Ok(s) => return Ok(s),
            Err(e) => last = e,
        }
    }
    Err(last)
}

fn open_lock(dir: &Path) -> io::Result<File> {
    OpenOptions::new().create(true).read(true).write(true).truncate(false).open(dir.join("lock"))
}

/// Connects and waits for the daemon's ready line; `None` for a dead socket or a daemon that is shutting down.
fn try_connect(sock: &Path) -> Option<UnixStream> {
    let mut s = UnixStream::connect(sock).ok()?;
    s.set_read_timeout(Some(HANDSHAKE_TIMEOUT)).ok()?;
    let mut got = [0u8; READY_LINE.len()];
    s.read_exact(&mut got).ok()?;
    if got != READY_LINE {
        return None;
    }
    s.set_read_timeout(None).ok()?;
    Some(s)
}

fn daemon_bin() -> PathBuf {
    if let Some(p) = std::env::var_os("MU_MEMORY_DAEMON") {
        return PathBuf::from(p);
    }
    paths::self_exe()
        .ok()
        .and_then(|e| e.parent().map(|d| d.join("mu-memory-daemon")))
        .filter(|p| p.exists())
        .unwrap_or_else(|| PathBuf::from("mu-memory-daemon"))
}

/// Spawns the daemon detached (setsid, own log) and waits for its socket to answer.
fn start_daemon(sock: &Path) -> Result<UnixStream, String> {
    let logs = paths::mu_home().join("logs");
    fs::create_dir_all(&logs).map_err(|e| e.to_string())?;
    let log_path = logs.join("memory-daemon.log");
    let log = OpenOptions::new().create(true).append(true).open(&log_path).map_err(|e| e.to_string())?;
    let bin = daemon_bin();
    let mut cmd = Command::new(&bin);
    cmd.args(["daemon", "--socket"]).arg(sock).stdin(Stdio::null()).stdout(Stdio::null()).stderr(log);
    // SAFETY: setsid is async-signal-safe and runs between fork and exec.
    unsafe {
        cmd.pre_exec(|| if libc::setsid() < 0 { Err(io::Error::last_os_error()) } else { Ok(()) });
    }
    let mut child = cmd.spawn().map_err(|e| format!("cannot start {}: {e}", bin.display()))?;
    let fail = |why: &str| format!("memory daemon {why}; see {}", log_path.display());
    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        if let Some(s) = try_connect(sock) {
            // Reap in the background so a later exit does not leave a zombie.
            thread::spawn(move || {
                let _ = child.wait();
            });
            return Ok(s);
        }
        if matches!(child.try_wait(), Ok(Some(_))) {
            return Err(fail("exited at startup"));
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(fail("did not start in time"));
        }
        thread::sleep(POLL);
    }
}

/// Copies stdin to the daemon and the daemon's replies to stdout until the daemon closes the connection
/// (it does so after `shutdown` and after stdin EOF, once the session's store is flushed).
fn relay(stream: UnixStream) -> Result<(), String> {
    let up = stream.try_clone().map_err(|e| e.to_string())?;
    thread::spawn(move || {
        let mut up = up;
        let mut stdin = io::stdin().lock();
        let mut buf = [0u8; 16 * 1024];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if up.write_all(&buf[..n]).is_err() {
                        return;
                    }
                }
            }
        }
        // Host closed our stdin: let the daemon finish the session and hang up.
        let _ = up.shutdown(std::net::Shutdown::Write);
    });
    let mut down = stream;
    let mut stdout = io::stdout().lock();
    let mut buf = [0u8; 16 * 1024];
    loop {
        match down.read(&mut buf) {
            Ok(0) => return Ok(()),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.to_string()),
            Ok(n) => {
                if stdout.write_all(&buf[..n]).and_then(|()| stdout.flush()).is_err() {
                    return Ok(());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    fn serve_once(path: &Path, reply: &'static [u8]) -> thread::JoinHandle<()> {
        let l = UnixListener::bind(path).unwrap();
        thread::spawn(move || {
            let (mut c, _) = l.accept().unwrap();
            c.write_all(reply).unwrap();
        })
    }

    #[test]
    fn connects_only_after_the_ready_line() {
        let dir = std::env::temp_dir().join(format!("mu-memory-shim-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let ok = dir.join("ok");
        let h = serve_once(&ok, READY_LINE);
        assert!(try_connect(&ok).is_some());
        h.join().unwrap();
        // A daemon that hangs up (idle exit racing the connect) or says something else is not usable.
        let closed = dir.join("closed");
        let h = serve_once(&closed, b"");
        assert!(try_connect(&closed).is_none());
        h.join().unwrap();
        let other = dir.join("other");
        let h = serve_once(&other, b"hello, world\n\n\n\n");
        assert!(try_connect(&other).is_none());
        h.join().unwrap();
        assert!(try_connect(&dir.join("missing")).is_none());
        let _ = fs::remove_dir_all(&dir);
    }
}
