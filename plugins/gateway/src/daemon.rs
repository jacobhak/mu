//! The shared gateway daemon: one `omp auth-broker` + `omp auth-gateway` pair (or one pi proxy process) for every
//! client process.

use crate::sealed_state;
use crate::state::{self, Backend};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Read};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

/// How long `spawn_omp` waits for the child to report its URL.
const SPAWN_WAIT: Duration = Duration::from_secs(20);
const TOKEN_TIMEOUT: Duration = Duration::from_secs(15);
/// Slack for the daemon's own startup on top of its steps.
const START_MARGIN: Duration = Duration::from_secs(5);
/// The daemon's steps run one after the other: broker spawn, token command, gateway spawn.
const START_TIMEOUT: Duration =
    Duration::from_secs(2 * SPAWN_WAIT.as_secs() + TOKEN_TIMEOUT.as_secs() + START_MARGIN.as_secs());
const POLL: Duration = Duration::from_millis(200);
const STOP_GRACE: Duration = Duration::from_secs(2);
/// How long the omp pair gets to exit after SIGTERM before SIGKILL. omp's auth broker finishes a token refresh
/// already in flight first (up to its 10 s cleanup deadline); killing it sooner leaves a spent refresh token in
/// the store. A pair with nothing in flight exits at once, so this only bounds the worst case.
const CHILD_GRACE: Duration = Duration::from_secs(12);
const STOP_POLL: Duration = Duration::from_millis(200);

/// Set by SIGTERM, SIGHUP or SIGINT: `serve` stops and its children go with it.
static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn request_stop(_sig: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
}

/// Spawns (detached, setsid) `mu-gateway daemon` unless one is already healthy, and returns its URL.
/// Called with the profile directory's lock held.
pub fn start(dir: &Path) -> Result<String, String> {
    stop_orphaned_pair(dir);
    fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let log_path = state::daemon_log();
    let log = OpenOptions::new().create(true).append(true).open(&log_path).map_err(|e| e.to_string())?;
    let exe = state::self_exe().map_err(|e| e.to_string())?;
    let mut cmd = Command::new(exe);
    cmd.args(backend_args()).arg("daemon").stdin(Stdio::null()).stdout(Stdio::null()).stderr(log);
    if state::backend() == Backend::Omp {
        cmd.envs(sealed_state::envs());
    }
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
        if let Some(st) = state::read_state(dir)
            && st.pid == pid as i32
            && state::healthy(&st.url)
        {
            return Ok(st.url);
        }
        if !state::alive(pid as i32) {
            return Err(fail());
        }
        if Instant::now() >= deadline {
            // Still starting after the deadline: it is not coming up, so do not leave it running.
            // The omp children sit in their own groups, so the daemon's group does not reach them.
            let mut kids = daemon_children(pid as i32);
            signal_group(pid as i32, libc::SIGTERM);
            thread::sleep(STOP_GRACE.min(Duration::from_millis(500)));
            for k in daemon_children(pid as i32) {
                if !kids.contains(&k) {
                    kids.push(k);
                }
            }
            signal_group(pid as i32, libc::SIGKILL);
            for k in kids {
                signal_group(k, libc::SIGKILL);
            }
            return Err(fail());
        }
        thread::sleep(POLL);
    }
}

/// A daemon that died without stopping its omp pair (a crash, or SIGKILL) leaves the pair running,
/// and the next start would stack a second one. Stops the pair the dead daemon recorded.
fn stop_orphaned_pair(dir: &Path) {
    let Some(st) = state::read_state(dir) else { return };
    if state::alive(st.pid) {
        return;
    }
    for pid in st.kids {
        let Ok(out) = Command::new("ps").args(["-o", "command=", "-p", &pid.to_string()]).output() else {
            continue;
        };
        if is_backend_child(&String::from_utf8_lossy(&out.stdout)) {
            signal_group(pid, libc::SIGTERM);
        }
    }
}

/// Whether a `ps -o command=` line is one of the omp pair's processes.
fn is_omp_pair(cmdline: &str) -> bool {
    cmdline.contains("auth-broker") || cmdline.contains("auth-gateway")
}

/// Whether a `ps -o command=` line is the pi proxy process (pi running `proxy.mjs`).
fn is_pi_proxy(cmdline: &str) -> bool {
    cmdline.contains("proxy.mjs")
}

/// Whether a `ps -o command=` line is a child of the current backend's daemon.
fn is_backend_child(cmdline: &str) -> bool {
    match state::backend() {
        Backend::Omp => is_omp_pair(cmdline),
        Backend::Pi => is_pi_proxy(cmdline),
    }
}

/// The backend's pids among the direct children of `parent` (each is its own group leader).
fn daemon_children(parent: i32) -> Vec<i32> {
    match Command::new("ps").args(["-A", "-o", "pid=,ppid=,command="]).output() {
        Ok(out) => parse_children(&String::from_utf8_lossy(&out.stdout), parent, is_backend_child),
        Err(_) => Vec::new(),
    }
}

fn parse_children(ps: &str, parent: i32, matches: fn(&str) -> bool) -> Vec<i32> {
    ps.lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let pid: i32 = it.next()?.parse().ok()?;
            let ppid: i32 = it.next()?.parse().ok()?;
            (ppid == parent && matches(l)).then_some(pid)
        })
        .collect()
}

/// The global options a re-exec of this binary (`daemon`, `guard`) must carry.
fn backend_args() -> [&'static str; 2] {
    ["--backend", state::backend().name()]
}

/// `mu-gateway guard <pgid>`: blocks on stdin, whose write end the daemon holds. When the daemon
/// dies by any means, stdin reaches EOF and the guard stops the omp group. Works on Linux and macOS.
pub fn guard(pgid: i32) -> i32 {
    watch_group(pgid, std::io::stdin());
    0
}

fn watch_group(pgid: i32, mut pipe: impl Read) {
    let mut buf = [0u8; 64];
    loop {
        match pipe.read(&mut buf) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    signal_group(pgid, libc::SIGTERM);
    let deadline = Instant::now() + CHILD_GRACE;
    while group_alive(pgid) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
    }
    signal_group(pgid, libc::SIGKILL);
}

/// Starts a guard for the omp group `pgid`; it lives in its own session so it outlives the daemon.
fn spawn_guard(pgid: u32) -> Option<Child> {
    let exe = state::self_exe().ok()?;
    let mut cmd = Command::new(exe);
    cmd.args(backend_args())
        .args(["guard", &pgid.to_string()])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: setsid is async-signal-safe.
    unsafe {
        cmd.pre_exec(|| if libc::setsid() < 0 { Err(std::io::Error::last_os_error()) } else { Ok(()) });
    }
    cmd.spawn().ok()
}

/// Sends `sig` to the process group `pgid`. The omp children are spawned with `process_group(0)`, so
/// their pid is their group id.
fn signal_group(pgid: i32, sig: libc::c_int) {
    if pgid > 1 {
        // SAFETY: plain kill(2) on a group this daemon (or an earlier one) created.
        unsafe { libc::kill(-pgid, sig) };
    }
}

fn group_alive(pgid: i32) -> bool {
    if pgid <= 1 {
        return false;
    }
    // SAFETY: signal 0 only asks whether the group has members.
    unsafe { libc::kill(-pgid, 0) == 0 }
}

/// Stops the running daemon of `dir` (SIGTERM to its group, then SIGKILL after the grace its children get).
/// True when a daemon was running.
pub fn stop(dir: &Path) -> bool {
    let Some(st) = state::read_state(dir).filter(|st| state::alive(st.pid)) else { return false };
    signal_group(st.pid, libc::SIGTERM);
    let deadline = Instant::now() + CHILD_GRACE + STOP_GRACE;
    while state::alive(st.pid) && Instant::now() < deadline {
        thread::sleep(STOP_POLL);
    }
    if state::alive(st.pid) {
        signal_group(st.pid, libc::SIGKILL);
        for k in &st.kids {
            signal_group(*k, libc::SIGKILL);
        }
    }
    state::remove_state_if_owned(dir, st.pid as u32);
    true
}

/// Sleeps for `tick` in `STOP_POLL` steps; true as soon as a stop signal has arrived.
fn sleep_unless_stopped(tick: Duration) -> bool {
    let deadline = Instant::now() + tick;
    loop {
        if STOP.load(Ordering::SeqCst) {
            return true;
        }
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        thread::sleep(STOP_POLL.min(deadline - now));
    }
}

/// Daemon entry point (`mu-gateway daemon`). Returns the process exit code.
pub fn run() -> i32 {
    match serve() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("{} daemon: {e}", state::backend().log_prefix());
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
    Duration::try_from_secs_f64(secs).unwrap_or(Duration::from_secs_f64(default))
}

fn serve() -> Result<i32, String> {
    let dir = state::gw_dir();
    fs::create_dir_all(dir.join("leases")).map_err(|e| e.to_string())?;
    let idle = env_secs("MU_GATEWAY_IDLE_SECS", 1800.0);
    let tick = env_secs("MU_GATEWAY_TICK_SECS", 30.0);
    let me = std::process::id();
    let prefix = state::backend().log_prefix();
    for sig in [libc::SIGTERM, libc::SIGHUP, libc::SIGINT] {
        // SAFETY: the handler only stores to an atomic, which is async-signal-safe.
        let handler = request_stop as extern "C" fn(libc::c_int);
        unsafe { libc::signal(sig, handler as libc::sighandler_t) };
    }

    // Dropping `kids` (on any early return) terminates the children.
    let mut kids = Kids(Vec::new(), Vec::new());
    let (gw_url, broker_url) = match state::backend() {
        Backend::Omp => spawn_omp_pair(&mut kids)?,
        Backend::Pi => (spawn_pi_proxy(&dir, &mut kids)?, String::new()),
    };
    let kid_pids: Vec<i32> = kids.0.iter().map(|c| c.id() as i32).collect();
    state::write_state_with_kids(&dir, me, &gw_url, &broker_url, &kid_pids).map_err(|e| e.to_string())?;
    eprintln!("{prefix} daemon {me}: gateway at {gw_url}");

    loop {
        if sleep_unless_stopped(tick) {
            state::remove_state_if_owned(&dir, me);
            eprintln!("{prefix} daemon {me}: stopping on a signal");
            return Ok(0);
        }
        if kids.exited() {
            let lock = state::lock(&dir).map_err(|e| e.to_string())?;
            state::remove_state_if_owned(&dir, me);
            drop(lock);
            let what = match state::backend() {
                Backend::Omp => "broker or gateway",
                Backend::Pi => "pi proxy",
            };
            eprintln!("{prefix} daemon {me}: {what} exited; stopping");
            return Ok(1);
        }
        state::prune_leases(&dir);
        if state::has_fresh_lease(&dir, idle) {
            continue;
        }
        // Re-check under the lock: a plugin holding it is about to adopt this gateway.
        if let Some(lock) = state::try_lock(&dir).map_err(|e| e.to_string())?
            && !state::has_fresh_lease(&dir, idle)
        {
            state::remove_state_if_owned(&dir, me);
            drop(lock);
            eprintln!("{prefix} daemon {me}: no client lease within {}s; stopping", idle.as_secs_f64());
            return Ok(0);
        }
    }
}

/// Owns the omp children and their guards; terminates them on drop.
struct Kids(Vec<Child>, Vec<Child>);

impl Kids {
    /// True when any child has exited (reaps it).
    fn exited(&mut self) -> bool {
        self.0.iter_mut().any(|c| matches!(c.try_wait(), Ok(Some(_))))
    }

    /// SIGTERM to each child's group, wait up to `CHILD_GRACE` for the groups to empty, then SIGKILL.
    fn terminate(&mut self) {
        let mut kids = std::mem::take(&mut self.0);
        let pids: Vec<i32> = kids.iter().map(|c| c.id() as i32).collect();
        for &pid in &pids {
            signal_group(pid, libc::SIGTERM);
        }
        let deadline = Instant::now() + CHILD_GRACE;
        loop {
            // Reap leaders first: a zombie leader keeps its group visible to `group_alive`.
            for c in &mut kids {
                let _ = c.try_wait();
            }
            if pids.iter().all(|&p| !group_alive(p)) || Instant::now() >= deadline {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        for &pid in &pids {
            signal_group(pid, libc::SIGKILL);
        }
        for c in &mut kids {
            let _ = c.kill();
            let _ = c.wait();
        }
        // Groups are gone: stop the guards before their pipes close and they signal a reused pid.
        for mut g in std::mem::take(&mut self.1) {
            let _ = g.kill();
            let _ = g.wait();
        }
    }
}

impl Drop for Kids {
    fn drop(&mut self) {
        self.terminate();
    }
}

/// Starts the broker, then the gateway against it; returns the gateway's and the broker's URLs.
fn spawn_omp_pair(kids: &mut Kids) -> Result<(String, String), String> {
    let (broker, broker_url) = spawn_omp(&["auth-broker", "serve", "--bind=127.0.0.1:0"], &[], &mut kids.1)?;
    kids.0.push(broker);
    let mut token_cmd = Command::new(state::omp_bin());
    token_cmd.envs(sealed_state::envs());
    run_with_timeout(
        token_cmd.args(["auth-gateway", "token"]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()),
        TOKEN_TIMEOUT,
    );
    let token_path = sealed_state::credential_root().join("auth-broker.token");
    let token = fs::read_to_string(&token_path).map(|t| t.trim().to_string()).unwrap_or_default();
    if token.is_empty() {
        return Err(format!("broker token not found at {}", token_path.display()));
    }
    let (gw, gw_url) = spawn_omp(
        &["auth-gateway", "serve", "--bind=127.0.0.1:0"],
        &[("OMP_AUTH_BROKER_URL", &broker_url), ("OMP_AUTH_BROKER_TOKEN", &token)],
        &mut kids.1,
    )?;
    kids.0.push(gw);
    Ok((gw_url, broker_url))
}

/// The pi command line that serves the proxy extension. The extra flags keep the user's own extensions, MCP
/// servers, skills, prompt templates, themes and project files out of the proxy process.
pub fn pi_proxy_command(proxy: &Path) -> Command {
    let mut cmd = Command::new(state::pi_bin());
    cmd.args(["--mode", "rpc", "--no-session", "-ne", "-e"])
        .arg(proxy)
        .args(["--no-mcp", "--no-skills", "--no-prompt-templates", "--no-themes", "--no-context-files", "-na"])
        .env_remove("OMP_PROFILE");
    cmd
}

/// Writes a fresh token, then starts pi with the proxy extension; returns the URL the proxy reports.
fn spawn_pi_proxy(dir: &Path, kids: &mut Kids) -> Result<String, String> {
    let proxy = state::proxy_path();
    if !proxy.is_file() {
        return Err(format!("pi proxy extension not found at {} (set MU_GATEWAY_PI_PROXY)", proxy.display()));
    }
    let proxy = std::path::absolute(&proxy).unwrap_or(proxy);
    let token = state::write_token(dir).map_err(|e| format!("cannot write the gateway token: {e}"))?;
    let mut cmd = pi_proxy_command(&proxy);
    cmd.current_dir(dir).env("MU_GW_PI_MODE", "serve").env("MU_GW_PI_TOKEN", token);
    // Piped and never written: pi exits when stdin reaches EOF, which happens when this daemon dies.
    cmd.stdin(Stdio::piped());
    let (child, url) = spawn_listening(cmd, "pi proxy", PI_LISTENING, &mut kids.1)?;
    kids.0.push(child);
    Ok(url)
}

fn spawn_omp(args: &[&str], env: &[(&str, &str)], guards: &mut Vec<Child>) -> Result<(Child, String), String> {
    let mut cmd = Command::new(state::omp_bin());
    cmd.envs(sealed_state::envs());
    cmd.args(args).envs(env.iter().copied()).stdin(Stdio::null());
    spawn_listening(cmd, &format!("omp {}", args.join(" ")), "", guards)
}

/// Spawns `cmd` and returns it with the first `http://` URL it prints on stdout or stderr. `tag` names it in
/// the daemon log. Output is forwarded to the log. The child dies with the daemon: PDEATHSIG on Linux, and on
/// every platform a guard process that kills the child's group when the daemon's pipe to it closes.
fn spawn_listening(
    mut cmd: Command,
    tag: &str,
    marker: &str,
    guards: &mut Vec<Child>,
) -> Result<(Child, String), String> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    cmd.process_group(0);
    #[cfg(target_os = "linux")]
    unsafe {
        cmd.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            Ok(())
        });
    }
    let mut child = cmd.spawn().map_err(|e| format!("cannot run `{}` ({e})", tag.split(' ').next().unwrap_or(tag)))?;
    match spawn_guard(child.id()) {
        Some(g) => guards.push(g),
        None => eprintln!("{} daemon: no guard for `{tag}`", state::backend().log_prefix()),
    }
    let tag = tag.to_string();
    let (tx, rx) = mpsc::channel::<String>();
    let out = child.stdout.take().expect("stdout is piped");
    let err = child.stderr.take().expect("stderr is piped");
    for r in [Box::new(out) as Box<dyn Read + Send>, Box::new(err)] {
        let (tx, tag) = (tx.clone(), tag.clone());
        thread::spawn(move || {
            for line in BufReader::new(r).lines().map_while(Result::ok) {
                eprintln!("[{tag}] {line}");
                let _ = tx.send(line);
            }
        });
    }
    drop(tx);
    let mut seen = String::new();
    loop {
        match rx.recv_timeout(SPAWN_WAIT) {
            Ok(line) => {
                if line.contains(marker)
                    && let Some(url) = listen_url(&line)
                {
                    return Ok((child, url));
                }
                seen.push_str(&line);
                seen.push('\n');
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("`{tag}` did not report a listening URL:\n{seen}"));
            }
        }
    }
}

/// What pi's proxy prints before its URL; pi's own warnings can carry URLs too.
const PI_LISTENING: &str = "proxy listening on ";

fn listen_url(line: &str) -> Option<String> {
    let i = line.find("http://")?;
    Some(line[i..].chars().take_while(|c| !c.is_whitespace() && *c != '"').collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_omp_pair_commands_match() {
        assert!(is_omp_pair("/usr/local/bin/omp auth-broker serve --bind=127.0.0.1:0\n"));
        assert!(is_omp_pair("omp auth-gateway serve --bind=127.0.0.1:0"));
        assert!(!is_omp_pair("sleep 30"));
        assert!(!is_omp_pair(""));
    }

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
    fn the_start_wait_covers_every_daemon_step() {
        assert!(START_TIMEOUT >= SPAWN_WAIT * 2 + TOKEN_TIMEOUT);
        assert!(START_TIMEOUT > Duration::from_secs(30));
    }

    #[test]
    fn omp_children_are_picked_by_parent_and_command() {
        let ps = "  10     1 mu-gateway daemon\n  11    10 omp auth-broker serve --bind=127.0.0.1:0\n  \
                  12    10 mu-gateway guard 11\n  13    99 omp auth-gateway serve\n  14    10 omp auth-gateway serve\n";
        assert_eq!(parse_children(ps, 10, is_omp_pair), vec![11, 14]);
        assert!(parse_children("garbage\n\n", 10, is_omp_pair).is_empty());
    }

    #[test]
    fn the_pi_child_is_picked_by_its_proxy_command() {
        assert!(is_pi_proxy("/home/u/.mu/bin/pi --mode rpc --no-session -ne -e /opt/gateway-pi/proxy.mjs --no-mcp"));
        assert!(!is_pi_proxy("mu-gateway --backend pi guard 11"));
        assert!(!is_pi_proxy("omp auth-gateway serve"));
        let ps = "  10     1 mu-gateway --backend pi daemon\n  11    10 pi --mode rpc -e /x/proxy.mjs -na\n  \
                  12    10 mu-gateway --backend pi guard 11\n  13    99 pi -e /x/proxy.mjs\n";
        assert_eq!(parse_children(ps, 10, is_pi_proxy), vec![11]);
    }

    #[test]
    fn the_pi_command_line_matches_the_contract() {
        let cmd = pi_proxy_command(Path::new("/x/proxy.mjs"));
        let args: Vec<_> = cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(
            args,
            [
                "--mode",
                "rpc",
                "--no-session",
                "-ne",
                "-e",
                "/x/proxy.mjs",
                "--no-mcp",
                "--no-skills",
                "--no-prompt-templates",
                "--no-themes",
                "--no-context-files",
                "-na"
            ]
        );
    }

    fn wait_exit(child: &mut Child) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if matches!(child.try_wait(), Ok(Some(_))) {
                return true;
            }
            thread::sleep(Duration::from_millis(20));
        }
        false
    }

    #[test]
    fn the_guard_kills_the_group_when_its_pipe_closes() {
        let mut leader = Command::new("sleep").arg("60").process_group(0).spawn().unwrap();
        let pgid = leader.id() as i32;
        let (reader, writer) = std::io::pipe().unwrap();
        let guard = thread::spawn(move || watch_group(pgid, reader));
        thread::sleep(Duration::from_millis(200));
        assert!(matches!(leader.try_wait(), Ok(None)), "the group lives while the pipe is open");
        drop(writer);
        guard.join().unwrap();
        assert!(wait_exit(&mut leader), "the group's leader is gone after EOF");
    }
    #[test]
    fn signal_group_reaches_a_child_in_its_own_group() {
        let mut child = Command::new("sleep").arg("60").process_group(0).spawn().unwrap();
        signal_group(child.id() as i32, libc::SIGKILL);
        assert!(wait_exit(&mut child));
    }

    #[test]
    fn a_hung_command_is_killed() {
        let t = Instant::now();
        assert!(!run_with_timeout(Command::new("sleep").arg("30"), Duration::from_millis(200)));
        assert!(t.elapsed() < Duration::from_secs(5));
        assert!(run_with_timeout(&mut Command::new("true"), Duration::from_secs(5)));
    }
}
