//! Helpers shared by mu's test suites. Dev-only: nothing here ships in the binary.

use std::future::Future;
use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// A private `MU_HOME` for this test process, created once. Tests that touch mu's state
/// (sessions, blobs) call it first so they never write to the real `~/.mu`.
pub fn home() -> &'static Path {
    static H: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    H.get_or_init(|| {
        let d = std::env::temp_dir().join(format!("mu-test-home-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        // SAFETY: set once, before any test in this process reads it through `home()`.
        unsafe { std::env::set_var("MU_HOME", &d) };
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

/// A fresh, empty, canonical temp dir.
pub fn tmp() -> PathBuf {
    let d = std::env::temp_dir().join(format!("mu-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&d).unwrap();
    d.canonicalize().unwrap()
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
pub fn repo() -> PathBuf {
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

/// A key press with no modifiers.
pub fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

pub fn key_mods(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
    KeyEvent::new(code, mods)
}
