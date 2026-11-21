//! Helpers shared by mu's test suites. Dev-only: nothing here ships in the binary.

#![allow(clippy::unwrap_used)] // test-only helpers: panicking on failure is the point
use std::future::Future;
use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// A private `MU_HOME` for this test process, created once, with no omp profile, so per-profile state
/// is `MU_HOME` itself whatever the developer's shell sets. Tests that touch mu's state (sessions,
/// blobs) call it first so they never write to the real `~/.mu`. A home this call created is removed
/// at process exit; one given by `MU_TEST_HOME` belongs to the test that spawned this process.
pub fn home() -> &'static Path {
    static H: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    static OWNED: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    extern "C" fn remove_owned() {
        if let Some(d) = OWNED.get() {
            let _ = std::fs::remove_dir_all(d);
        }
    }
    H.get_or_init(|| {
        // A spawned test binary (tests/modes.rs) is given its home by the test that starts it.
        let d = match std::env::var_os("MU_TEST_HOME") {
            Some(d) => PathBuf::from(d),
            None => {
                let d = std::env::temp_dir().join(format!("mu-test-home-{}", uuid::Uuid::new_v4()));
                let _ = OWNED.set(d.clone());
                // SAFETY: the handler only reads a `OnceLock` and removes a directory.
                unsafe { libc::atexit(remove_owned) };
                d
            }
        };
        std::fs::create_dir_all(&d).unwrap();
        // SAFETY: set once, before any test in this process reads it through `home()`.
        unsafe {
            std::env::set_var("MU_HOME", &d);
            std::env::remove_var("OMP_PROFILE");
            std::env::remove_var("PI_PROFILE");
        }
        d
    })
}

/// One canned HTTP response of a `fake_server`.
pub struct Reply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Reply {
    pub fn new(status: u16, body: &str) -> Self {
        Self { status, headers: Vec::new(), body: body.into() }
    }

    pub fn header(mut self, k: &str, v: &str) -> Self {
        self.headers.push((k.into(), v.into()));
        self
    }
}

/// A local HTTP server answering each request with the next reply (the last one repeats).
/// Returns its base URL (`http://127.0.0.1:<port>`) and the raw text of every request received,
/// headers and body, in order.
pub async fn fake_server(replies: Vec<Reply>) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let log = seen.clone();
    tokio::spawn(async move {
        let mut i = 0;
        loop {
            let Ok((mut sock, _)) = listener.accept().await else { return };
            let mut buf = Vec::new();
            let mut chunk = [0u8; 8192];
            let (head_end, want) = loop {
                let n = sock.read(&mut chunk).await.unwrap_or(0);
                if n == 0 {
                    break (buf.len(), buf.len());
                }
                buf.extend_from_slice(&chunk[..n]);
                if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&buf[..p]).to_lowercase();
                    let len = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:").and_then(|v| v.trim().parse::<usize>().ok()))
                        .unwrap_or(0);
                    break (p + 4, p + 4 + len);
                }
            };
            while buf.len() < want {
                let n = sock.read(&mut chunk).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            let _ = head_end;
            log.lock().unwrap().push(String::from_utf8_lossy(&buf).into_owned());
            let r = &replies[i.min(replies.len() - 1)];
            i += 1;
            let mut out =
                format!("HTTP/1.1 {} X\r\ncontent-length: {}\r\nconnection: close\r\n", r.status, r.body.len());
            for (k, v) in &r.headers {
                out.push_str(&format!("{k}: {v}\r\n"));
            }
            out.push_str("\r\n");
            out.push_str(&r.body);
            let _ = sock.write_all(out.as_bytes()).await;
            let _ = sock.shutdown().await;
        }
    });
    (base, seen)
}

/// A fresh, empty, canonical temp dir. It is under `/tmp` with a short name: an agent's socket lives below it
/// (`run/<id>.sock`) and macOS's `$TMPDIR` alone leaves too little of the 100-byte socket path limit.
/// The directory is removed when the guard drops, so keep the guard for as long as the path is used.
pub fn tmp() -> TempDir {
    let d = PathBuf::from("/tmp").join(format!("mu-t-{}", &uuid::Uuid::new_v4().simple().to_string()[..10]));
    std::fs::create_dir_all(&d).unwrap();
    TempDir(d.canonicalize().unwrap())
}

/// Guard for a directory made by `tmp()`: removes it recursively on drop.
#[derive(Debug)]
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl std::ops::Deref for TempDir {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.0
    }
}

impl AsRef<Path> for TempDir {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

/// Runs `git` in `dir` with a fixed identity, panics on failure, returns stdout.
pub fn sh(dir: &Path, args: &[&str]) -> String {
    let o = std::process::Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(o.status.success(), "{args:?}: {}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8_lossy(&o.stdout).into_owned()
}

/// A fresh git repo whose initial commit holds `a.txt` and `b.txt`, both `one\n`.
pub fn repo() -> TempDir {
    let d = tmp();
    sh(&d, &["init", "-q"]);
    std::fs::write(d.join("a.txt"), "one\n").unwrap();
    std::fs::write(d.join("b.txt"), "one\n").unwrap();
    sh(&d, &["add", "."]);
    sh(&d, &["commit", "-qm", "init"]);
    d
}

/// A current-thread runtime with all drivers enabled.
pub fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
}

/// Runs `f` to completion on a fresh current-thread runtime.
pub fn block_on<F: Future>(f: F) -> F::Output {
    rt().block_on(f)
}

/// Like `block_on`, but inside a `LocalSet` so `spawn_local` works.
pub fn block_on_local<F: Future>(f: F) -> F::Output {
    rt().block_on(tokio::task::LocalSet::new().run_until(f))
}

/// Polls `ready` every 10 ms until it is true; panics naming `what` after 10 s. For waiting on
/// observable state (a file, a pid) instead of sleeping a fixed time.
pub async fn wait_until(what: &str, mut ready: impl FnMut() -> bool) {
    for _ in 0..1000 {
        if ready() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for {what}");
}

/// Waits for `file` to hold a pid (a script's `echo $$ > file`) and returns it.
pub async fn pid_in(file: &Path) -> i32 {
    wait_until(&format!("a pid in {}", file.display()), || {
        std::fs::read_to_string(file).is_ok_and(|s| s.trim().parse::<i32>().is_ok())
    })
    .await;
    std::fs::read_to_string(file).unwrap().trim().parse().unwrap()
}

/// Whether process `pid` is running: gone, or a zombie nothing reaps (a container whose PID 1 does
/// not reap, as in the release build), is not.
pub fn pid_running(pid: i32) -> bool {
    let out = std::process::Command::new("ps").args(["-o", "stat=", "-p", &pid.to_string()]).output().unwrap();
    out.status.success() && !String::from_utf8_lossy(&out.stdout).trim_start().starts_with('Z')
}

/// Waits until process `pid` is gone.
pub async fn wait_gone(pid: i32) {
    wait_until(&format!("pid {pid} to end"), || !pid_running(pid)).await;
}

/// A key press with no modifiers.
pub fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

pub fn key_mods(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
    KeyEvent::new(code, mods)
}
