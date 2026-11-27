//! Helpers shared by the integration tests. They drive the real `mu-remote` binary against a real `mu`
//! (the workspace's `target/<profile>/mu`, or the binary named by `$MU_REMOTE_TEST_MU`) and a fake Anthropic API.
#![allow(dead_code, clippy::unwrap_used)]

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

pub const MODEL: &str = "anthropic/claude-sonnet-5-5";

/// The `mu` under test: `$MU_REMOTE_TEST_MU`, else the `mu` the workspace built beside `mu-remote`.
pub fn mu_bin() -> PathBuf {
    if let Some(p) = std::env::var_os("MU_REMOTE_TEST_MU") {
        return PathBuf::from(p);
    }
    let path = Path::new(env!("CARGO_BIN_EXE_mu-remote")).parent().unwrap().join("mu");
    assert!(
        path.exists(),
        "no mu binary at {}: run the workspace's tests (cargo test --workspace builds it) or set MU_REMOTE_TEST_MU",
        path.display()
    );
    path
}

/// The `mu` under test.
#[macro_export]
macro_rules! need_mu {
    () => {
        $crate::common::mu_bin()
    };
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

/// A fresh, empty, canonical temp dir. It is under `/tmp` and its name is short: macOS's `$TMPDIR` alone is ~50
/// bytes, and the agent's and worker's Unix sockets live below this dir (the limit is 104 bytes there).
pub fn tmp() -> PathBuf {
    let d = PathBuf::from("/tmp").join(format!("mrit-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]));
    std::fs::create_dir_all(&d).unwrap();
    d.canonicalize().unwrap()
}

/// Runs `git` in `dir` with a fixed identity, panics on failure, returns stdout.
pub fn sh(dir: &Path, args: &[&str]) -> String {
    let o = Command::new("git")
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

pub fn block_on<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(f)
}

pub const ANSWER: &str = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m1\"}}\n\n\
    event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n\
    event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hello from the fake\"}}\n\n\
    event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
    event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"input_tokens\":10,\"output_tokens\":2}}\n\n\
    event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";

pub fn serve(replies: Vec<Reply>) -> String {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        block_on(async move {
            let (base, _) = fake_server(replies).await;
            tx.send(base).unwrap();
            std::future::pending::<()>().await;
        })
    });
    rx.recv().unwrap()
}

pub fn sse(body: &str) -> Reply {
    Reply::new(200, body).header("content-type", "text/event-stream")
}

/// A `MU_HOME` (also `HOME`) whose key store holds an Anthropic key that points at `base`.
pub fn home_with_key(base: &str) -> PathBuf {
    let home = tmp();
    let store = json!({"version": 1, "providers": {"anthropic": {"key": "sk-test-key", "baseUrl": base}}});
    std::fs::write(home.join("auth.json"), store.to_string()).unwrap();
    std::fs::create_dir_all(home.join("empty-bin")).unwrap();
    home
}

/// The `mu` under test with a private home and cwd and an empty PATH (so no omp is found).
pub fn mu(home: &Path, cwd: &Path) -> Command {
    let mut c = Command::new(mu_bin());
    c.current_dir(cwd)
        .env("MU_TEST_HOME", home)
        .env("MU_HOME", home)
        .env("HOME", home)
        .env("PATH", home.join("empty-bin"))
        .env("MU_OMP", home.join("no-omp"))
        .env_remove("OMP_PROFILE")
        .env_remove("MU_GATEWAY_URL")
        .env_remove("MU_THEME")
        .env_remove("ANTHROPIC_API_KEY");
    c
}

pub struct Agent {
    pub child: Child,
    pub socket: PathBuf,
    pub session: String,
}

impl Agent {
    pub fn start(home: &Path, cwd: &Path, extra: &[&str]) -> Agent {
        Agent::start_env(home, cwd, extra, &[])
    }

    pub fn start_env(home: &Path, cwd: &Path, extra: &[&str], env: &[(&str, &Path)]) -> Agent {
        let mut command = mu(home, cwd);
        let mode: &[&str] = if extra.contains(&"--permissions") { &[] } else { &["--permissions", "yolo"] };
        command
            .envs(env.iter().copied())
            .args(["agent", "--model", MODEL])
            .args(mode)
            .args(extra)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        // SAFETY: umask is async-signal-safe. A umask of 0 shows the agent sets its own modes.
        unsafe {
            command.pre_exec(|| {
                libc::umask(0);
                Ok(())
            })
        };
        let mut child = command.spawn().unwrap();
        let mut first = String::new();
        BufReader::new(child.stdout.take().unwrap()).read_line(&mut first).unwrap();
        let v: Value = serde_json::from_str(&first).unwrap_or_else(|e| panic!("first line {first:?}: {e}"));
        Agent {
            socket: PathBuf::from(v["socket"].as_str().unwrap()),
            session: v["session"].as_str().unwrap().to_string(),
            child,
        }
    }

    pub fn wait_exit(&mut self) -> std::process::ExitStatus {
        for _ in 0..100 {
            if let Some(s) = self.child.try_wait().unwrap() {
                return s;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!("the agent did not end");
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub struct Pid(pub i32);

impl Drop for Pid {
    fn drop(&mut self) {
        unsafe { libc::kill(self.0, libc::SIGKILL) };
    }
}

pub fn pid_of(home: &Path, id: &str) -> Pid {
    let text = std::fs::read_to_string(home.join("run").join(format!("{id}.json"))).unwrap();
    let rec: Value = serde_json::from_str(&text).unwrap();
    Pid(rec["pid"].as_u64().unwrap() as i32)
}

pub fn agent_list(home: &Path, cwd: &Path) -> Vec<Value> {
    let o = mu(home, cwd).args(["agent", "list", "--json"]).output().unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8_lossy(&o.stdout).lines().map(|l| serde_json::from_str(l).unwrap()).collect()
}

pub fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    for _ in 0..300 {
        if done() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("timed out waiting for {what}");
}

/// A terminal for the real client: a pty with a reader thread that keeps everything the program drew.
pub struct Term {
    pub child: Child,
    writer: std::fs::File,
    out: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
}

impl Term {
    pub fn spawn(mut command: Command) -> Term {
        use std::os::fd::{FromRawFd, OwnedFd};
        let (mut m, mut s) = (-1, -1);
        let ws = libc::winsize { ws_row: 50, ws_col: 220, ws_xpixel: 0, ws_ypixel: 0 };
        // SAFETY: valid out-pointers; the descriptors are fresh and owned below.
        let (master, slave) = unsafe {
            let r = libc::openpty(
                &mut m,
                &mut s,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::addr_of!(ws).cast_mut(),
            );
            assert_eq!(r, 0);
            (OwnedFd::from_raw_fd(m), OwnedFd::from_raw_fd(s))
        };
        command
            .env("TERM", "xterm-256color")
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave));
        // SAFETY: setsid and ioctl are async-signal-safe.
        unsafe {
            command.pre_exec(|| {
                libc::setsid();
                libc::ioctl(0, libc::TIOCSCTTY as _, 0);
                Ok(())
            })
        };
        let child = command.spawn().unwrap();
        drop(command);
        let writer = std::fs::File::from(master.try_clone().unwrap());
        let mut reader = std::fs::File::from(master);
        let out = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = out.clone();
        std::thread::spawn(move || {
            use std::io::Read;
            let mut buf = [0u8; 4096];
            while let Ok(n @ 1..) = reader.read(&mut buf) {
                sink.lock().unwrap().extend_from_slice(&buf[..n]);
            }
        });
        Term { child, writer, out }
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.out.lock().unwrap()).into_owned()
    }

    pub fn wait_for(&self, needle: &str) {
        for _ in 0..300 {
            if self.text().contains(needle) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("timed out waiting for {needle:?}; the terminal got: {:?}", self.text());
    }

    /// Types into the composer once the UI is drawn, then presses Enter.
    pub fn command(&mut self, line: &str) {
        self.wait_for("type here");
        self.send(line);
        std::thread::sleep(Duration::from_millis(300));
        self.send("\r");
    }

    pub fn send(&mut self, bytes: &str) {
        self.writer.write_all(bytes.as_bytes()).unwrap();
    }

    pub fn wait_exit(&mut self) -> std::process::ExitStatus {
        for _ in 0..200 {
            if let Some(s) = self.child.try_wait().unwrap() {
                return s;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("the client did not end; it drew: {}", self.text());
    }
}

impl Drop for Term {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
