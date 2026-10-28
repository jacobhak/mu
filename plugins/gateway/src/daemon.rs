//! The shared gateway daemon: one `omp auth-broker` + `omp auth-gateway` pair for every mu process.

use crate::shared;
use mu_common::paths;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Read};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

const START_TIMEOUT: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_millis(200);
const TOKEN_TIMEOUT: Duration = Duration::from_secs(15);
const STOP_GRACE: Duration = Duration::from_secs(2);

/// Spawns (detached, setsid) `mu-gateway daemon` unless one is already healthy, and returns its URL.
/// Called with the `gateway/lock` held.
pub fn start(dir: &Path) -> Result<String, String> {
    fs::create_dir_all(shared::logs_dir()).map_err(|e| e.to_string())?;
    let log_path = shared::daemon_log();
    let log = OpenOptions::new().create(true).append(true).open(&log_path).map_err(|e| e.to_string())?;
    let exe = paths::self_exe().map_err(|e| e.to_string())?;
    let mut cmd = Command::new(exe);
    cmd.arg("daemon").stdin(Stdio::null()).stdout(Stdio::null()).stderr(log);
    unsafe {
        cmd.pre_exec(|| if libc::setsid() < 0 { Err(std::io::Error::last_os_error()) } else { Ok(()) });
    }
    let mut child = cmd.spawn().map_err(|e| format!("cannot start gateway daemon: {e}"))?;
    let pid = child.id();
    // Reap in the background so an early exit does not leave a zombie.
    thread::spawn(move || {
        let _ = child.wait();
    });
    let fail = || format!("gateway daemon did not start; see {}", log_path.display());
    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        if let Some(st) = shared::read_state(dir)
            && st.pid == pid as i32
            && shared::healthy(&st.url)
        {
            return Ok(st.url);
        }
        if !shared::alive(pid as i32) {
            return Err(fail());
        }
        if Instant::now() >= deadline {
            // Still starting after the deadline: it is not coming up, so do not leave it running.
            // SAFETY: the daemon called setsid, so -pid is its own process group.
            unsafe {
                libc::kill(-(pid as i32), libc::SIGTERM);
            }
            thread::sleep(STOP_GRACE.min(Duration::from_millis(500)));
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
            return Err(fail());
        }
        thread::sleep(POLL);
    }
}

/// Daemon entry point (`mu-gateway daemon`). Returns the process exit code.
pub fn run() -> i32 {
    match serve() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("mu-gateway daemon: {e}");
            1
        }
    }
}

/// Runs `cmd` and waits up to `limit`; a command still running then is killed. Returns whether it exited in time.
fn run_with_timeout(cmd: &mut Command, limit: Duration) -> bool {
    let Ok(mut child) = cmd.spawn() else { return false };
    let deadline = Instant::now() + limit;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return true,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

fn env_secs(key: &str, default: f64) -> Duration {
    let secs = std::env::var(key).ok().and_then(|v| v.parse::<f64>().ok()).filter(|v| *v >= 0.0).unwrap_or(default);
    Duration::from_secs_f64(secs)
}

fn serve() -> Result<i32, String> {
    let dir = shared::gw_dir();
    fs::create_dir_all(dir.join("leases")).map_err(|e| e.to_string())?;
    let idle = env_secs("MU_GATEWAY_IDLE_SECS", 1800.0);
    let tick = env_secs("MU_GATEWAY_TICK_SECS", 30.0);
    let me = std::process::id();

    // Dropping `kids` (on any early return) terminates the children.
    let mut kids = Kids(Vec::new());
    let (broker, broker_url) = spawn_omp(&["auth-broker", "serve", "--bind=127.0.0.1:0"], &[])?;
    kids.0.push(broker);
    run_with_timeout(
        Command::new(mu_common::paths::omp_bin())
            .args(["auth-gateway", "token"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null()),
        TOKEN_TIMEOUT,
    );
    let token_path = paths::omp_config_root().join("auth-broker.token");
    let token = fs::read_to_string(&token_path).map(|t| t.trim().to_string()).unwrap_or_default();
    if token.is_empty() {
        return Err(format!("broker token not found at {}", token_path.display()));
    }
    let (gw, gw_url) = spawn_omp(
        &["auth-gateway", "serve", "--bind=127.0.0.1:0"],
        &[("OMP_AUTH_BROKER_URL", &broker_url), ("OMP_AUTH_BROKER_TOKEN", &token)],
    )?;
    kids.0.push(gw);
    shared::write_state(&dir, me, &gw_url, &broker_url).map_err(|e| e.to_string())?;
    eprintln!("mu-gateway daemon {me}: gateway at {gw_url}");

    loop {
        thread::sleep(tick);
        if kids.exited() {
            let lock = shared::lock(&dir).map_err(|e| e.to_string())?;
            shared::remove_state_if_owned(&dir, me);
            drop(lock);
            eprintln!("mu-gateway daemon {me}: broker or gateway exited; stopping");
            return Ok(1);
        }
        shared::prune_leases(&dir);
        if shared::has_fresh_lease(&dir, idle) {
            continue;
        }
        // Re-check under the lock: a plugin holding it is about to adopt this gateway.
        if let Some(lock) = shared::try_lock(&dir).map_err(|e| e.to_string())?
            && !shared::has_fresh_lease(&dir, idle)
        {
            shared::remove_state_if_owned(&dir, me);
            drop(lock);
            eprintln!("mu-gateway daemon {me}: no client lease within {}s; stopping", idle.as_secs_f64());
            return Ok(0);
        }
    }
}

/// Owns the omp children; terminates them on drop.
struct Kids(Vec<Child>);

impl Kids {
    /// True when any child has exited (reaps it).
    fn exited(&mut self) -> bool {
        self.0.iter_mut().any(|c| matches!(c.try_wait(), Ok(Some(_))))
    }

    /// SIGTERM, wait up to `STOP_GRACE`, then SIGKILL.
    fn terminate(&mut self) {
        let mut kids = std::mem::take(&mut self.0);
        for c in &kids {
            unsafe { libc::kill(c.id() as i32, libc::SIGTERM) };
        }
        let deadline = Instant::now() + STOP_GRACE;
        loop {
            kids.retain_mut(|c| matches!(c.try_wait(), Ok(None)));
            if kids.is_empty() || Instant::now() >= deadline {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        for c in &mut kids {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

impl Drop for Kids {
    fn drop(&mut self) {
        self.terminate();
    }
}

/// Spawns `omp <args>` on an ephemeral loopback port and returns the URL it reports.
/// Output is forwarded to the daemon log; the child dies with the daemon (PDEATHSIG).
fn spawn_omp(args: &[&str], env: &[(&str, &str)]) -> Result<(Child, String), String> {
    let mut cmd = Command::new(mu_common::paths::omp_bin());
    cmd.args(args).envs(env.iter().copied()).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    cmd.process_group(0);
    #[cfg(target_os = "linux")]
    unsafe {
        cmd.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            Ok(())
        });
    }
    let mut child = cmd.spawn().map_err(|e| format!("cannot run `omp` ({e})"))?;
    let tag = args.join(" ");
    let (tx, rx) = mpsc::channel::<String>();
    let out = child.stdout.take().unwrap();
    let err = child.stderr.take().unwrap();
    for r in [Box::new(out) as Box<dyn Read + Send>, Box::new(err)] {
        let (tx, tag) = (tx.clone(), tag.clone());
        thread::spawn(move || {
            for line in BufReader::new(r).lines().map_while(Result::ok) {
                eprintln!("[omp {tag}] {line}");
                let _ = tx.send(line);
            }
        });
    }
    drop(tx);
    let mut seen = String::new();
    loop {
        match rx.recv_timeout(Duration::from_secs(20)) {
            Ok(line) => {
                if let Some(url) = listen_url(&line) {
                    return Ok((child, url));
                }
                seen.push_str(&line);
                seen.push('\n');
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("`omp {tag}` did not report a listening URL:\n{seen}"));
            }
        }
    }
}

fn listen_url(line: &str) -> Option<String> {
    let i = line.find("http://")?;
    Some(line[i..].chars().take_while(|c| !c.is_whitespace() && *c != '"').collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_from_listening_line() {
        assert_eq!(listen_url("auth-gateway listening on http://127.0.0.1:4321"), Some("http://127.0.0.1:4321".into()));
        assert_eq!(listen_url(r#"{"url":"http://127.0.0.1:9"}"#), Some("http://127.0.0.1:9".into()));
        assert_eq!(listen_url("no url here"), None);
    }
}

#[cfg(test)]
mod timeout_tests {
    use super::*;

    #[test]
    fn a_hung_command_is_killed() {
        let t = Instant::now();
        assert!(!run_with_timeout(Command::new("sleep").arg("30"), Duration::from_millis(200)));
        assert!(t.elapsed() < Duration::from_secs(5));
        assert!(run_with_timeout(&mut Command::new("true"), Duration::from_secs(5)));
    }
}
