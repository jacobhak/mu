//! Moving files and ports between this machine and the one the agent runs on. When attached,
//! `MU_ATTACH_HELPER` (a shell command line, set by whatever launched `mu --attach`; see
//! docs/remote.md) carries them; without it, or when it answers exit 64 ("not supported here"), files go
//! over the attach stream as `read_bytes` and `write_bytes` host calls and ports cannot be forwarded.

use super::*;
use crate::host::files::{self, READ_BYTES, ReadBytes, ReadReply, STREAM_FILE_MAX, WRITE_BYTES, WriteBytes};
use base64::Engine;
use mu_base::process::{kill_group, spawn_grouped};
use std::path::Path;
use std::process::Stdio;

/// The most a helper may send or take.
pub(super) const HELPER_MAX: u64 = 512 * 1024 * 1024;
pub(super) const FILE_TIMEOUT: Duration = Duration::from_secs(600);
pub(super) const PORT_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a helper's leftover children may hold its output open after it exits.
const LEFTOVER_GRACE: Duration = Duration::from_millis(500);
/// The helper's exit code for "not supported here".
const UNSUPPORTED: i32 = 64;

#[derive(Debug, PartialEq)]
pub(super) enum Failure {
    Unsupported,
    Other(String),
}

/// `$MU_ATTACH_HELPER`, when set and not empty.
pub(super) fn helper() -> Option<String> {
    std::env::var("MU_ATTACH_HELPER").ok().filter(|h| !h.trim().is_empty())
}

/// Runs `sh -c '<helper> "$@"' mu-attach-helper VERB ARGS…` in its own process group, feeds it `stdin`
/// and returns its stdout. Past `limit` bytes of output or `timeout`, the group is killed.
pub(super) async fn run_helper(
    helper: &str,
    verb: &str,
    args: &[String],
    stdin: Option<Vec<u8>>,
    limit: u64,
    timeout: Duration,
) -> Result<Vec<u8>, Failure> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-c").arg(format!("{helper} \"$@\"")).arg("mu-attach-helper").arg(verb).args(args);
    cmd.stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child =
        spawn_grouped(&mut cmd, false).map_err(|e| Failure::Other(format!("cannot run the helper: {e}")))?;
    let pgid = child.id().map_or(0, |p| p as i32);
    let (Some(mut out), Some(mut err)) = (child.stdout.take(), child.stderr.take()) else {
        return Err(Failure::Other("cannot read the helper's output".into()));
    };
    let mut input = child.stdin.take();
    let run = async {
        let write = async {
            if let (Some(si), Some(bytes)) = (input.as_mut(), stdin) {
                // A helper that exits without reading (it does not support the verb) closes the pipe.
                let _ = si.write_all(&bytes).await;
            }
            drop(input.take());
        };
        let read_out = async {
            let mut data = Vec::new();
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                match out.read(&mut buf).await {
                    Ok(0) | Err(_) => return Some(data),
                    Ok(n) => {
                        data.extend_from_slice(&buf[..n]);
                        if data.len() as u64 > limit {
                            kill_group(pgid, libc::SIGKILL);
                            return None;
                        }
                    }
                }
            }
        };
        let read_err = async {
            let mut text = Vec::new();
            let mut buf = [0u8; 1024];
            // Everything is read so the helper never blocks on a full pipe; only the start is kept.
            while let Ok(n @ 1..) = err.read(&mut buf).await {
                if text.len() < 4096 {
                    text.extend_from_slice(&buf[..n]);
                }
            }
            text
        };
        let all = async { tokio::join!(write, read_out, read_err) };
        tokio::pin!(all);
        let mut done = None;
        let status = loop {
            tokio::select! {
                r = &mut all, if done.is_none() => done = Some(r),
                st = child.wait() => break st,
            }
        };
        let (_, data, text) = match done {
            Some(r) => r,
            None => match tokio::time::timeout(LEFTOVER_GRACE, &mut all).await {
                Ok(r) => r,
                Err(_) => {
                    kill_group(pgid, libc::SIGKILL);
                    all.await
                }
            },
        };
        (status, data, text)
    };
    let (status, data, text) = match tokio::time::timeout(timeout, run).await {
        Ok(r) => r,
        Err(_) => {
            kill_group(pgid, libc::SIGKILL);
            return Err(Failure::Other(format!("the helper did not finish in {} s", timeout.as_secs())));
        }
    };
    let Some(data) = data else {
        return Err(Failure::Other(format!("more than {limit} bytes")));
    };
    match status {
        Ok(s) if s.success() => Ok(data),
        Ok(s) if s.code() == Some(UNSUPPORTED) => Err(Failure::Unsupported),
        Ok(s) => {
            let line = String::from_utf8_lossy(&text).lines().next().unwrap_or("").trim().to_string();
            Err(Failure::Other(if line.is_empty() { format!("the helper failed ({s})") } else { line }))
        }
        Err(e) => Err(Failure::Other(format!("the helper failed: {e}"))),
    }
}

/// `path:12` and `path:12-30` name a line range; the file is what comes before.
pub(super) fn strip_line_suffix(arg: &str) -> &str {
    let Some((file, tail)) = arg.rsplit_once(':') else { return arg };
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let range = match tail.split_once('-') {
        Some((a, b)) => digits(a) && digits(b),
        None => digits(tail),
    };
    if range && !file.is_empty() { file } else { arg }
}

/// A path the user typed: `~` is `home`, `local://x` is under the session's local directory, and a
/// relative path is under `cwd`.
pub(super) fn resolve_path(arg: &str, home: &Path, cwd: &Path, local_dir: Option<&str>) -> Result<PathBuf, String> {
    let arg = arg.trim();
    if let Some(rest) = arg.strip_prefix("local://") {
        let dir = local_dir.ok_or("no session directory yet")?;
        return Ok(Path::new(dir).join(rest.trim_start_matches('/')));
    }
    let path = paths::expand_tilde(arg, home).unwrap_or_else(|| PathBuf::from(arg));
    Ok(if path.is_absolute() { path } else { cwd.join(path) })
}

fn decode_b64(data: &str) -> Result<Vec<u8>, String> {
    base64::engine::general_purpose::STANDARD.decode(data).map_err(|e| format!("unreadable file data: {e}"))
}

impl App {
    /// A path the user typed, resolved on the machine the agent runs on.
    pub(super) fn resolve_host_path(&self, arg: &str) -> Result<PathBuf, String> {
        let info = self.host.info();
        resolve_path(arg, &info.home, &info.cwd, self.agent.status.local_dir.as_deref())
    }

    /// Reads a file on the agent's machine (an ordinary read when not attached).
    pub(super) fn fetch_file(&mut self, path: PathBuf, then: impl FnOnce(&mut App, Result<Vec<u8>, String>) + 'static) {
        if !self.host.is_remote() {
            return then(self, files::read_capped(&path, HELPER_MAX));
        }
        let Some(helper) = helper() else { return self.fetch_over_stream(path, then) };
        let tx = self.ui_tx.clone();
        let args = vec![path.to_string_lossy().into_owned()];
        tokio::task::spawn_local(async move {
            let r = run_helper(&helper, "read", &args, None, HELPER_MAX, FILE_TIMEOUT).await;
            let _ = tx.send(UiMsg::Host(Box::new(move |app| match r {
                Ok(bytes) => then(app, Ok(bytes)),
                Err(Failure::Unsupported) => app.fetch_over_stream(path, then),
                Err(Failure::Other(e)) => then(app, Err(format!("{}: {e}", path.display()))),
            })));
        });
    }

    fn fetch_over_stream(&mut self, path: PathBuf, then: impl FnOnce(&mut App, Result<Vec<u8>, String>) + 'static) {
        let args = ReadBytes { path, max: STREAM_FILE_MAX };
        self.host_call(READ_BYTES, &args, move |app, r: Result<ReadReply, String>| {
            then(app, r.and_then(|r| decode_b64(&r.data_b64)))
        });
    }

    /// Creates a file on the agent's machine; an existing one is an error.
    pub(super) fn put_file(
        &mut self,
        path: PathBuf,
        bytes: Vec<u8>,
        then: impl FnOnce(&mut App, Result<(), String>) + 'static,
    ) {
        if !self.host.is_remote() {
            return then(self, files::write_new(&path, &bytes));
        }
        let Some(helper) = helper() else { return self.put_over_stream(path, bytes, then) };
        let tx = self.ui_tx.clone();
        let args = vec![path.to_string_lossy().into_owned()];
        tokio::task::spawn_local(async move {
            let r = run_helper(&helper, "write", &args, Some(bytes.clone()), 1 << 20, FILE_TIMEOUT).await;
            let _ = tx.send(UiMsg::Host(Box::new(move |app| match r {
                Ok(_) => then(app, Ok(())),
                Err(Failure::Unsupported) => app.put_over_stream(path, bytes, then),
                Err(Failure::Other(e)) => then(app, Err(format!("{}: {e}", path.display()))),
            })));
        });
    }

    fn put_over_stream(
        &mut self,
        path: PathBuf,
        bytes: Vec<u8>,
        then: impl FnOnce(&mut App, Result<(), String>) + 'static,
    ) {
        if bytes.len() as u64 > STREAM_FILE_MAX {
            return then(
                self,
                Err(format!("{} is too large for the attach stream and no attach helper can carry it", path.display())),
            );
        }
        let args = WriteBytes { path, data_b64: base64::engine::general_purpose::STANDARD.encode(&bytes) };
        self.host_call(WRITE_BYTES, &args, move |app, r: Result<(), String>| then(app, r));
    }

    /// Makes `127.0.0.1:N` here reach `localhost:port` on the agent's machine; answers with N.
    pub(super) fn forward_port(&mut self, port: u16, then: impl FnOnce(&mut App, Result<u16, String>) + 'static) {
        let Some(helper) = helper() else {
            return then(
                self,
                Err(format!(
                    "no attach helper: forward port {port} yourself (for ssh: ssh -L {port}:localhost:{port} HOST)"
                )),
            );
        };
        let tx = self.ui_tx.clone();
        tokio::task::spawn_local(async move {
            let r = run_helper(&helper, "forward", &[port.to_string()], None, 4096, PORT_TIMEOUT).await;
            let r = match r {
                Ok(out) => parse_forward(&out),
                Err(Failure::Unsupported) => Err("this remote cannot forward ports".into()),
                Err(Failure::Other(e)) => Err(format!("cannot forward port {port}: {e}")),
            };
            let _ = tx.send(UiMsg::Host(Box::new(move |app| then(app, r))));
        });
    }

    /// Removes forwards, best effort, one after another on a blocking-capable task.
    pub(super) fn unforward(&mut self, forwards: Vec<Forward>) {
        let Some(helper) = helper() else { return };
        if forwards.is_empty() {
            return;
        }
        tokio::task::spawn_local(async move {
            for f in forwards {
                let args = [f.port.to_string(), f.local.to_string()];
                let _ = run_helper(&helper, "unforward", &args, None, 4096, PORT_TIMEOUT).await;
            }
        });
    }
}

/// One forwarded port: the port the job listens on, and the one it answers on here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Forward {
    pub port: u16,
    pub local: u16,
}

/// The forwards this client holds, each for the job that asked (`""` for none), so a job's forwards
/// go when it does and every one goes when mu exits.
#[derive(Default)]
pub(super) struct Forwards(Vec<(String, Forward)>);

impl Forwards {
    pub(super) fn local(&self, job: &str, port: u16) -> Option<u16> {
        self.0.iter().find(|(j, f)| j == job && f.port == port).map(|(_, f)| f.local)
    }

    pub(super) fn add(&mut self, job: &str, port: u16, local: u16) {
        if self.local(job, port).is_none() {
            self.0.push((job.to_string(), Forward { port, local }));
        }
    }

    /// Drops and returns what a job holds.
    pub(super) fn take_job(&mut self, job: &str) -> Vec<Forward> {
        let (mine, rest) = std::mem::take(&mut self.0).into_iter().partition(|(j, _)| j == job);
        self.0 = rest;
        mine.into_iter().map(|(_, f)| f).collect()
    }

    /// Drops and returns every forward.
    pub(super) fn take_all(&mut self) -> Vec<Forward> {
        std::mem::take(&mut self.0).into_iter().map(|(_, f)| f).collect()
    }

    /// The jobs holding forwards that `running` says have stopped.
    pub(super) fn stale(&self, running: impl Fn(&str) -> bool) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for (job, _) in &self.0 {
            if !running(job) && !out.contains(job) {
                out.push(job.clone());
            }
        }
        out
    }
}

/// How long a quitting client waits for its forwards to be removed altogether.
const QUIT_UNFORWARD: Duration = Duration::from_secs(5);

/// Removes forwards on the way out: blocking, `QUIT_UNFORWARD` for all of them, best effort. The
/// event loop is over by then, so this cannot go through `run_helper`.
pub(super) fn unforward_now(forwards: Vec<Forward>) {
    let Some(helper) = helper() else { return };
    let deadline = Instant::now() + QUIT_UNFORWARD;
    for f in forwards {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let helper = helper.clone();
        std::thread::spawn(move || {
            let mut cmd = std::process::Command::new("sh");
            cmd.arg("-c")
                .arg(format!("{helper} \"$@\""))
                .arg("mu-attach-helper")
                .arg("unforward")
                .arg(f.port.to_string())
                .arg(f.local.to_string())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            let _ = cmd.status();
            let _ = tx.send(());
        });
        let _ = rx.recv_timeout(left);
    }
}

fn parse_forward(out: &[u8]) -> Result<u16, String> {
    let line = String::from_utf8_lossy(out);
    let line = line.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    serde_json::from_str::<serde_json::Value>(line)
        .ok()
        .and_then(|v| v["local"].as_u64())
        .and_then(|n| u16::try_from(n).ok())
        .ok_or_else(|| format!("the attach helper answered `{}` to forward", line.trim()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn script(dir: &Path, body: &str) -> String {
        let path = dir.join("helper.sh");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        format!("'{}'", path.display())
    }

    fn run(
        helper: &str,
        verb: &str,
        args: &[&str],
        stdin: Option<&[u8]>,
        limit: u64,
        ms: u64,
    ) -> Result<Vec<u8>, Failure> {
        let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        mu_testutil::block_on(run_helper(
            helper,
            verb,
            &args,
            stdin.map(<[u8]>::to_vec),
            limit,
            Duration::from_millis(ms),
        ))
    }

    #[test]
    fn the_helper_gets_the_verb_and_arguments_and_its_output_comes_back() {
        let dir = mu_testutil::tmp();
        let h = script(dir.path(), r#"printf '%s|%s|%s' "$1" "$2" "$3""#);
        assert_eq!(run(&h, "read", &["/a b", "x"], None, 100, 5000).unwrap(), b"read|/a b|x");
    }

    #[test]
    fn a_helper_with_arguments_of_its_own_runs_through_sh() {
        let dir = mu_testutil::tmp();
        let h = format!("{} --flag", script(dir.path(), r#"printf '%s' "$1$2""#));
        assert_eq!(run(&h, "v", &["a"], None, 100, 5000).unwrap(), b"--flagv");
    }

    #[test]
    fn the_input_reaches_the_helper() {
        let dir = mu_testutil::tmp();
        let target = dir.path().join("out");
        let h = script(dir.path(), r#"cat > "$2""#);
        run(&h, "write", &[target.to_str().unwrap()], Some(b"hello"), 100, 5000).unwrap();
        assert_eq!(std::fs::read(target).unwrap(), b"hello");
    }

    #[test]
    fn exit_64_means_not_supported() {
        let dir = mu_testutil::tmp();
        let h = script(dir.path(), "echo no >&2; exit 64");
        assert_eq!(run(&h, "read", &["/x"], None, 100, 5000), Err(Failure::Unsupported));
    }

    #[test]
    fn another_failure_reports_the_first_line_of_stderr() {
        let dir = mu_testutil::tmp();
        let h = script(dir.path(), "echo boom >&2; echo more >&2; exit 1");
        assert_eq!(run(&h, "read", &["/x"], None, 100, 5000), Err(Failure::Other("boom".into())));
        let h = script(dir.path(), "exit 3");
        assert!(matches!(run(&h, "read", &["/x"], None, 100, 5000), Err(Failure::Other(e)) if e.contains("failed")));
    }

    #[test]
    fn a_read_past_the_limit_kills_the_helper() {
        let dir = mu_testutil::tmp();
        let pid = dir.path().join("pid");
        let h = script(
            dir.path(),
            &format!(r#"echo $$ > '{}'; while :; do echo xxxxxxxxxxxxxxxxxxxxxxxx; done"#, pid.display()),
        );
        let r = run(&h, "read", &["/x"], None, 1000, 20_000);
        assert!(matches!(r, Err(Failure::Other(ref e)) if e.contains("more than 1000 bytes")), "{r:?}");
        let pid: i32 = std::fs::read_to_string(pid).unwrap().trim().parse().unwrap();
        mu_testutil::block_on(mu_testutil::wait_until("the helper to die", || !mu_testutil::pid_running(pid)));
    }

    #[test]
    fn a_helper_that_does_not_finish_is_killed() {
        let dir = mu_testutil::tmp();
        let pid = dir.path().join("pid");
        let h = script(dir.path(), &format!("echo $$ > '{}'; sleep 30", pid.display()));
        let r = run(&h, "read", &["/x"], None, 1000, 300);
        assert!(matches!(r, Err(Failure::Other(ref e)) if e.contains("did not finish")), "{r:?}");
        let pid: i32 = std::fs::read_to_string(pid).unwrap().trim().parse().unwrap();
        mu_testutil::block_on(mu_testutil::wait_until("the helper to die", || !mu_testutil::pid_running(pid)));
    }

    #[test]
    fn a_leftover_child_holding_the_output_does_not_hang_the_call() {
        let dir = mu_testutil::tmp();
        let h = script(dir.path(), "sleep 30 &\necho ok");
        let started = std::time::Instant::now();
        assert_eq!(run(&h, "forward", &["1"], None, 100, 20_000).unwrap(), b"ok\n");
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn paths_resolve_on_the_agent_machine() {
        let (home, cwd) = (Path::new("/home/me"), Path::new("/work"));
        let r = |a| resolve_path(a, home, cwd, Some("/s/local"));
        assert_eq!(r("~").unwrap(), Path::new("/home/me"));
        assert_eq!(r("~/x.txt").unwrap(), Path::new("/home/me/x.txt"));
        assert_eq!(r("a/b").unwrap(), Path::new("/work/a/b"));
        assert_eq!(r("/etc/hosts").unwrap(), Path::new("/etc/hosts"));
        assert_eq!(r("local://uploads/a").unwrap(), Path::new("/s/local/uploads/a"));
        assert!(resolve_path("local://a", home, cwd, None).is_err());
    }

    #[test]
    fn a_line_suffix_is_not_part_of_the_path() {
        assert_eq!(strip_line_suffix("src/main.rs:12"), "src/main.rs");
        assert_eq!(strip_line_suffix("src/main.rs:12-30"), "src/main.rs");
        assert_eq!(strip_line_suffix("src/main.rs"), "src/main.rs");
        assert_eq!(strip_line_suffix("a:b"), "a:b");
        assert_eq!(strip_line_suffix(":12"), ":12");
    }

    #[test]
    fn the_forward_answer_is_a_json_line() {
        assert_eq!(parse_forward(b"{\"local\":4001}\n"), Ok(4001));
        assert!(parse_forward(b"nope").is_err());
        assert!(parse_forward(b"{\"local\":70000}").is_err());
    }
}
