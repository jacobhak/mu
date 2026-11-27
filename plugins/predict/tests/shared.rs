//! Drives `mu-predict` over stdio like mu does, with several plugins on one profile.
#![allow(clippy::unwrap_used)] // integration-test helpers outside any #[test] fn

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("mu-predict-shared-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("agent")).unwrap();
        Self(dir)
    }

    fn mu_home(&self) -> PathBuf {
        self.0.join("mu")
    }

    fn predict_dir(&self) -> PathBuf {
        self.mu_home().join("predict")
    }

    /// The pid the daemon's `state.json` names.
    fn daemon_pid(&self) -> Option<i32> {
        let v: Value =
            serde_json::from_str(&std::fs::read_to_string(self.predict_dir().join("state.json")).ok()?).ok()?;
        Some(v["pid"].as_i64()? as i32)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if let Some(pid) = self.daemon_pid() {
            // SAFETY: SIGKILL to the daemon this test started.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Plugin {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: mpsc::Receiver<Value>,
    next: u64,
}

impl Plugin {
    fn start(s: &Scratch, shared: bool) -> Self {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_mu-predict"));
        cmd.env("HOME", &s.0)
            .env("MU_HOME", s.mu_home())
            .env("MU_PREDICT_DAEMON_IDLE_SECS", "3")
            .env_remove("MU_PROFILE")
            .env_remove("OMP_PROFILE")
            .env_remove("PI_PROFILE")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        if !shared {
            cmd.env("MU_PREDICT_SHARED", "0");
        }
        let mut child = cmd.spawn().unwrap();
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Ok(v) = serde_json::from_str::<Value>(&line) {
                    let _ = tx.send(v);
                }
            }
        });
        Self { child, stdin, lines, next: 0 }
    }

    fn call(&mut self, method: &str, params: Value) -> Value {
        self.next += 1;
        let id = self.next;
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{}", json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})).unwrap();
        stdin.flush().unwrap();
        loop {
            let v = self.lines.recv_timeout(Duration::from_secs(60)).expect("the plugin answers");
            if v["id"] == json!(id) {
                assert!(v["error"].is_null(), "{method} failed: {v}");
                return v["result"].clone();
            }
        }
    }

    fn init(&mut self, s: &Scratch, mu_home: &Path) {
        self.call("initialize", json!({"agentDir": s.0.join("agent"), "muHome": mu_home}));
    }

    fn observe(&mut self, text: &str) {
        self.call("predict/observe", json!({"text": text, "cwd": "/x"}));
    }

    fn ghost(&mut self, text: &str) -> String {
        self.call("predict/complete", json!({"text": text, "cwd": "/x", "assistant": "", "threshold": 0.3}))["ghost"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    /// Ends the session the way mu does, and waits for the process.
    fn quit(mut self) {
        self.call("shutdown", json!({}));
        drop(self.stdin.take());
        let _ = self.child.wait();
    }
}

fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Gone, or a zombie nobody has reaped (a container's pid 1 may not).
fn exited(pid: i32) -> bool {
    if !alive(pid) {
        return true;
    }
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .is_ok_and(|s| s.rsplit(") ").next().is_some_and(|r| r.starts_with('Z')))
}

fn wait_for(what: &str, mut ok: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(30);
    while !ok() {
        assert!(Instant::now() < end, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn two_sessions_share_one_engine_and_the_daemon_leaves_when_both_are_gone() {
    let s = Scratch::new("share");
    let mut a = Plugin::start(&s, true);
    let mut b = Plugin::start(&s, true);
    a.init(&s, &s.mu_home());
    let pid = s.daemon_pid().expect("the first session started a daemon");
    b.init(&s, &s.mu_home());
    assert_eq!(s.daemon_pid(), Some(pid), "the second session joined it");

    a.observe("commit and push");
    a.observe("commit and push");
    assert_eq!(b.ghost("comm"), "it and push", "what A typed is B's ghost");
    assert_eq!(std::fs::read_dir(s.predict_dir().join("leases")).unwrap().count(), 2, "one lease each");

    a.quit();
    b.quit();
    assert!(alive(pid));
    wait_for("the idle daemon to remove its state", || s.daemon_pid().is_none());
    wait_for("the idle daemon to exit", || exited(pid));
    assert!(!s.predict_dir().join("daemon.sock").exists());
}

#[test]
fn sessions_starting_together_start_one_daemon() {
    let s = Scratch::new("race");
    let mut plugins: Vec<Plugin> = (0..4).map(|_| Plugin::start(&s, true)).collect();
    let home = s.mu_home();
    std::thread::scope(|scope| {
        for p in &mut plugins {
            scope.spawn(|| p.init(&s, &home));
        }
    });
    let log = std::fs::read_to_string(s.predict_dir().join("daemon.log")).unwrap();
    assert_eq!(log.matches("predict daemon: serving").count(), 1, "one daemon started: {log}");
    assert_eq!(std::fs::read_dir(s.predict_dir().join("leases")).unwrap().count(), 4);
    for p in plugins {
        p.quit();
    }
}

#[test]
fn a_dead_daemon_is_replaced() {
    let s = Scratch::new("dead");
    let mut a = Plugin::start(&s, true);
    a.init(&s, &s.mu_home());
    let pid = s.daemon_pid().unwrap();
    a.observe("commit and push");
    // SAFETY: SIGKILL to the daemon this test started.
    unsafe { libc::kill(pid, libc::SIGKILL) };
    wait_for("the daemon to die", || exited(pid));

    assert_eq!(a.ghost("comm"), "", "an answer, with nothing learned yet by the new daemon");
    let new = s.daemon_pid().expect("the plugin started another daemon");
    assert_ne!(new, pid);
    assert!(alive(new));
    a.quit();
}

#[test]
fn a_session_with_other_settings_keeps_its_own_engine() {
    let s = Scratch::new("other");
    let mut a = Plugin::start(&s, true);
    let mut b = Plugin::start(&s, true);
    a.init(&s, &s.mu_home());
    let pid = s.daemon_pid().unwrap();
    // Another muHome is another configuration: B must not get A's engine.
    b.init(&s, &s.0.join("elsewhere"));
    assert_eq!(s.daemon_pid(), Some(pid));

    b.observe("commit and push");
    b.observe("commit and push");
    assert_eq!(b.ghost("comm"), "it and push");
    assert_eq!(a.ghost("comm"), "", "B learned in its own engine");
    a.quit();
    b.quit();
}

#[test]
fn sharing_can_be_turned_off() {
    let s = Scratch::new("off");
    let mut a = Plugin::start(&s, false);
    a.init(&s, &s.mu_home());
    a.observe("commit and push");
    a.observe("commit and push");
    assert_eq!(a.ghost("comm"), "it and push");
    assert_eq!(s.daemon_pid(), None, "no daemon was started");
    a.quit();
}
