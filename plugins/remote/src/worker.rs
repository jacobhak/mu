//! `worker run|bundle|stop`: the remote end of a launched task, run by an adapter (`worker run --detach`,
//! `bridge ID`, `worker bundle ID`, `worker stop ID`).
//!
//! `run` is a supervisor that outlives its connections. It serves the link (`link.rs`) on a Unix socket
//! (`<state>/run/<launch id>.sock`, which `mu-remote bridge` reaches by the launch id), starts
//! `mu --headless` in the checkout when the first numbered line (the task payload) arrives, and passes lines
//! both ways. What the child writes is numbered and kept until the launcher acknowledges it. When the child
//! ends, whatever it changed is committed as `refs/mu/result/<launch id>` on top of the checkout's commit;
//! `bundle` prints that result as a git bundle, and `stop` ends the worker.

use crate::link::{Backlog, wire};
use crate::rpc::Framed;
use crate::{fs, paths, secure};
use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::io::{BufRead, BufReader as StdBufReader};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::rc::Rc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

/// Unacknowledged lines kept in memory; older ones go to a file.
const MEMORY_CAP: usize = 16 << 20;
/// A worker that never gets its task exits after this long.
const IDLE_START: Duration = Duration::from_secs(30 * 60);
/// `sun_path` holds 104 bytes on macOS (108 on Linux), the terminating NUL included.
const MAX_SOCKET_PATH: usize = 100;
const ACK_EVERY: u32 = 64;
const START_WAIT: Duration = Duration::from_secs(60);

struct Conn {
    serial: u64,
    tx: UnboundedSender<String>,
}

/// What `Hub::take` made of a line from the launcher.
enum Input {
    /// A duplicate, or not a numbered line.
    Skip,
    /// The numbers skip ahead: the connection is dropped so the launcher replays.
    Gap,
    Pass,
    /// The task payload: the child's input (the payload already queued) and the depth it runs at.
    Start(UnboundedReceiver<String>, Option<u64>),
}

/// What the supervisor knows: its backlog, the connection that is up, the child.
struct Hub {
    backlog: Backlog,
    next_seq: u64,
    /// The highest `seq` of the launcher's lines received without a gap.
    in_seen: u64,
    conn: Option<Conn>,
    /// The launcher's static key, as the first connection showed it.
    launcher_key: Option<Vec<u8>>,
    gens: u64,
    child_in: Option<UnboundedSender<String>>,
    child_pid: i32,
    started: bool,
    exited: bool,
    stderr: VecDeque<String>,
}

impl Hub {
    fn new(backlog: Backlog) -> Self {
        Self {
            backlog,
            next_seq: 0,
            in_seen: 0,
            conn: None,
            launcher_key: None,
            gens: 0,
            child_in: None,
            child_pid: 0,
            started: false,
            exited: false,
            stderr: VecDeque::new(),
        }
    }

    /// Numbers a line the child wrote, keeps it and sends it to the connection that is up.
    fn emit(&mut self, raw: &str) {
        self.next_seq += 1;
        let line = wire::numbered(raw, self.next_seq);
        self.backlog.push(self.next_seq, line.clone());
        if let Some(c) = &self.conn {
            let _ = c.tx.send(line);
        }
    }

    /// A connection says it has everything up to `after`: tells it what this side has, then replays what
    /// it lacks. Returns the connection's number.
    fn attach(&mut self, tx: UnboundedSender<String>, after: u64) -> u64 {
        self.gens += 1;
        self.conn = Some(Conn { serial: self.gens, tx: tx.clone() });
        let _ = tx.send(wire::resume(self.in_seen));
        self.backlog.ack(after);
        self.backlog.replay(after, |l| tx.send(l.to_string()).is_ok());
        self.gens
    }

    fn detach(&mut self, serial: u64) {
        if self.conn.as_ref().is_some_and(|c| c.serial == serial) {
            self.conn = None;
        }
    }

    fn current(&self, serial: u64) -> bool {
        self.conn.as_ref().is_some_and(|c| c.serial == serial)
    }

    fn send_ack(&self) {
        if let Some(c) = &self.conn {
            let _ = c.tx.send(wire::ack(self.in_seen));
        }
    }

    /// A numbered line from the launcher. The first starts the child; the rest are its input.
    fn take(&mut self, v: Value) -> Input {
        let Some(seq) = wire::seq_of(&v) else { return Input::Skip };
        if seq <= self.in_seen {
            return Input::Skip;
        }
        if seq != self.in_seen + 1 {
            return Input::Gap;
        }
        self.in_seen = seq;
        let depth = v["depth"].as_u64();
        let line = wire::plain(v);
        if !self.started {
            self.started = true;
            let (tx, rx) = unbounded_channel();
            let _ = tx.send(line);
            self.child_in = Some(tx);
            return Input::Start(rx, depth);
        }
        if let Some(tx) = &self.child_in {
            let _ = tx.send(line);
        }
        Input::Pass
    }

    fn exit_line(&self, code: i32) -> String {
        json!({"type": "exit", "code": code, "stderr": self.stderr.iter().cloned().collect::<Vec<_>>().join("\n")})
            .to_string()
    }
}

type Shared = Rc<RefCell<Hub>>;

struct Place {
    launch: String,
    dir: PathBuf,
    base: String,
    exe: String,
}

/// The worker's own record, for `bundle` and `stop`, which run in other processes.
#[derive(serde::Serialize, serde::Deserialize)]
struct State {
    pid: u32,
    launch: String,
    dir: PathBuf,
    base: String,
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 100 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn record_dir(state: &Path, launch: &str) -> PathBuf {
    state.join("workers").join(launch)
}

fn read_state_in(state: &Path, launch: &str) -> Result<State> {
    if !valid_id(launch) {
        bail!("invalid worker id `{launch}`");
    }
    let path = record_dir(state, launch).join("state.json");
    let text = std::fs::read_to_string(&path).map_err(|_| anyhow!("no worker {launch} is known here"))?;
    serde_json::from_str(&text).with_context(|| path.display().to_string())
}

fn read_state(launch: &str) -> Result<State> {
    read_state_in(&paths::state_dir(), launch)
}

fn socket_in(state: &Path, launch: &str) -> PathBuf {
    state.join("run").join(format!("{launch}.sock"))
}

fn socket_path(launch: &str) -> Result<PathBuf> {
    let path = socket_in(&paths::state_dir(), launch);
    if path.as_os_str().len() > MAX_SOCKET_PATH {
        bail!("the socket path is too long for a Unix socket ({MAX_SOCKET_PATH} bytes at most): {}", path.display());
    }
    Ok(path)
}

/// Whether process `pid` is alive. A zombie (a worker that was killed under a parent that never reaps) is not.
fn running(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else { return false };
    if pid <= 0 {
        return false;
    }
    // SAFETY: signal 0 only asks whether the process exists.
    if unsafe { libc::kill(pid, 0) } != 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::EPERM) {
        return false;
    }
    #[cfg(target_os = "linux")]
    if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat"))
        && let Some(rest) = stat.rsplit(')').next()
    {
        return !rest.trim_start().starts_with('Z');
    }
    true
}

fn socket_of_in(state: &Path, launch: &str) -> Option<PathBuf> {
    let st = read_state_in(state, launch).ok()?;
    let socket = socket_in(state, launch);
    (running(st.pid) && socket.exists()).then_some(socket)
}

/// The Unix socket worker `launch` serves, when it is one this machine has and is running.
pub fn socket_of(launch: &str) -> Option<PathBuf> {
    socket_of_in(&paths::state_dir(), launch)
}

fn result_ref(launch: &str) -> String {
    format!("refs/mu/result/{launch}")
}

/// Runs git in `dir` with `GIT_INDEX_FILE` set to `index` when given. Stdout on success, git's first stderr
/// line otherwise.
fn git(dir: &Path, args: &[&str], index: Option<&Path>) -> Result<String, String> {
    let mut cmd = std::process::Command::new("git");
    cmd.args(args).current_dir(dir).env("GIT_OPTIONAL_LOCKS", "0").stdin(Stdio::null());
    if let Some(i) = index {
        cmd.env("GIT_INDEX_FILE", i);
    }
    let name = args.iter().find(|a| !a.starts_with('-') && !a.contains('=')).copied().unwrap_or("");
    let out = cmd.output().map_err(|e| format!("git {name}: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        Err(err.lines().next().unwrap_or("git failed").trim().to_string())
    }
}

/// A tree sha of the whole working tree, untracked non-ignored files included. Uses a copy of the index, so
/// neither the real index nor the working tree is touched.
fn snapshot_tree(cwd: &Path) -> Result<String, String> {
    let real = git(cwd, &["rev-parse", "--git-path", "index"], None)?;
    let real = cwd.join(real.trim());
    let tmp = std::env::temp_dir().join(format!("mu-remote-snapshot-index-{}", uuid::Uuid::new_v4()));
    let res = (|| {
        if real.exists() {
            std::fs::copy(&real, &tmp).map_err(|e| format!("cannot copy index: {e}"))?;
            // Git's racy-entry check compares index entries with the index file's mtime; the copy must keep
            // the real index's mtime, or a same-tick rewrite of a tracked file looks unchanged.
            let mtime =
                std::fs::metadata(&real).and_then(|m| m.modified()).map_err(|e| format!("cannot stat index: {e}"))?;
            std::fs::File::options()
                .write(true)
                .open(&tmp)
                .and_then(|f| f.set_modified(mtime))
                .map_err(|e| format!("cannot stamp index copy: {e}"))?;
        }
        git(cwd, &["add", "-A"], Some(&tmp))?;
        git(cwd, &["write-tree"], Some(&tmp))
    })();
    let _ = std::fs::remove_file(&tmp);
    res.map(|s| s.trim().to_string())
}

/// Commits what the worker changed in `dir` (untracked files included) as the launch's result ref, on top of
/// `base`. `None` when the tree is what `base` holds. Safe to run again: an unchanged tree keeps its commit.
fn commit_result(dir: &Path, launch: &str, base: &str) -> Result<Option<String>, String> {
    let tree = snapshot_tree(dir)?;
    let base_tree = git(dir, &["rev-parse", &format!("{base}^{{tree}}")], None)?;
    let result = result_ref(launch);
    if tree == base_tree.trim() {
        let _ = git(dir, &["update-ref", "-d", &result], None);
        return Ok(None);
    }
    let kept = git(dir, &["rev-parse", "--verify", "-q", &format!("{result}^{{tree}}")], None);
    if kept.is_ok_and(|t| t.trim() == tree) {
        return Ok(Some(result));
    }
    let message = format!("mu-remote worker {launch}");
    let args = [
        "-c",
        "user.name=mu-remote",
        "-c",
        "user.email=mu-remote@localhost",
        "commit-tree",
        &tree,
        "-p",
        base,
        "-m",
        &message,
    ];
    let commit = git(dir, &args, None)?;
    git(dir, &["update-ref", &result, commit.trim()], None)?;
    Ok(Some(result))
}

async fn commit_result_async(place: &Place) -> Result<Option<String>, String> {
    let (dir, launch, base) = (place.dir.clone(), place.launch.clone(), place.base.clone());
    tokio::task::spawn_blocking(move || commit_result(&dir, &launch, &base)).await.map_err(|e| e.to_string())?
}

fn current_thread_runtime() -> Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_current_thread().enable_all().build().context("cannot start the async runtime")
}

fn place_of(launch: &str, dir: &Path) -> Result<Place> {
    if !valid_id(launch) {
        bail!("invalid worker id `{launch}`");
    }
    let dir = std::fs::canonicalize(dir).with_context(|| dir.display().to_string())?;
    let base = git(&dir, &["rev-parse", "HEAD"], None)
        .map(|s| s.trim().to_string())
        .map_err(|e| anyhow!("the worker's directory is not a git checkout: {e}"))?;
    Ok(Place { launch: launch.to_string(), dir, base, exe: paths::mu_program() })
}

/// `worker run`: serves worker `launch`, whose checkout is `dir`, until it is stopped.
pub fn run(launch: &str, dir: &Path, detach: bool, detached: bool) -> Result<i32> {
    let place = place_of(launch, dir)?;
    socket_path(launch)?;
    if detach {
        return start_detached(&place);
    }
    let state = record_dir(&paths::state_dir(), launch);
    fs::create_private_dir(&state)?;
    let record = State {
        pid: std::process::id(),
        launch: place.launch.clone(),
        dir: place.dir.clone(),
        base: place.base.clone(),
    };
    fs::write_atomic(&state.join("state.json"), serde_json::to_string(&record)?.as_bytes(), 0o600)?;
    std::env::set_current_dir(&place.dir)?;
    let rt = current_thread_runtime()?;
    let local = tokio::task::LocalSet::new();
    local.block_on(&rt, serve(place, detached))
}

/// Starts `run --detached` in a session of its own, so closing the terminal or the connection that ran this
/// command does not reach it, and prints `{"id":ID}` once its socket is up. Its output goes to
/// `<state>/logs/worker-<id>.log`.
fn start_detached(place: &Place) -> Result<i32> {
    let logs = paths::state_dir().join("logs");
    fs::create_private_dir(&logs).with_context(|| logs.display().to_string())?;
    let starting = logs.join(format!("worker-starting-{}.log", std::process::id()));
    let log = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(&starting)
        .with_context(|| starting.display().to_string())?;
    let exe = std::env::current_exe().context("cannot locate mu-remote")?;
    let mut command = std::process::Command::new(exe);
    command
        .args(["worker", "run", "--launch", &place.launch, "--dir"])
        .arg(&place.dir)
        .arg("--detached")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(log);
    // SAFETY: setsid is async-signal-safe.
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        })
    };
    let mut child = command.spawn().context("cannot start the worker")?;
    let stdout = child.stdout.take().context("the worker has no stdout")?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let _ = StdBufReader::new(stdout).read_line(&mut line);
        let _ = tx.send(line);
    });
    let line = rx.recv_timeout(START_WAIT).unwrap_or_default();
    let listening = serde_json::from_str::<Value>(&line).ok().is_some_and(|v| v["id"].as_str() == Some(&place.launch));
    if !listening {
        let _ = child.kill();
        let _ = child.wait();
        let text = std::fs::read_to_string(&starting).unwrap_or_default();
        let mut tail: Vec<&str> = text.lines().rev().take(5).collect();
        tail.reverse();
        bail!(
            "the worker did not start ({}){}",
            starting.display(),
            if tail.is_empty() { String::new() } else { format!(": {}", tail.join(" | ")) }
        );
    }
    // The open descriptor follows the rename.
    let _ = std::fs::rename(&starting, logs.join(format!("worker-{}.log", place.launch)));
    println!("{}", json!({ "id": place.launch }));
    Ok(0)
}

async fn serve(place: Place, detached: bool) -> Result<i32> {
    let path = socket_path(&place.launch)?;
    let run_dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
    fs::create_private_dir(&run_dir)?;
    let _ = std::fs::remove_file(&path);
    // Born 0600, so it is never reachable between bind and chmod.
    let listener = fs::with_umask(0o077, || UnixListener::bind(&path)).with_context(|| path.display().to_string())?;
    println!("{}", json!({"id": place.launch, "socket": path}));
    if detached {
        let _ = std::io::Write::flush(&mut std::io::stdout());
        // SAFETY: both descriptors are open.
        unsafe { libc::dup2(2, 1) };
    }
    let backlog = Backlog::new(MEMORY_CAP, record_dir(&paths::state_dir(), &place.launch).join("backlog"));
    let hub: Shared = Rc::new(RefCell::new(Hub::new(backlog)));
    // In memory only: a worker's key lives as long as the worker.
    let key = Rc::new(secure::Keypair::generate().map_err(|e| anyhow!(e))?);
    let place = Rc::new(place);
    let (mut term, mut int, mut hup) =
        (signal(SignalKind::terminate())?, signal(SignalKind::interrupt())?, signal(SignalKind::hangup())?);
    let idle = tokio::time::sleep(IDLE_START);
    tokio::pin!(idle);
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                if let Ok((stream, _)) = accepted {
                    tokio::task::spawn_local(connection(stream, hub.clone(), place.clone(), key.clone()));
                }
            }
            _ = term.recv() => break,
            _ = int.recv() => break,
            _ = hup.recv() => {
                if !detached {
                    break;
                }
            }
            () = &mut idle, if !hub.borrow().started => break,
        }
    }
    let _ = std::fs::remove_file(&path);
    let pid = hub.borrow().child_pid;
    if pid > 0 && !hub.borrow().exited {
        terminate_group(pid).await;
    }
    if let Err(e) = commit_result_async(&place).await {
        eprintln!("mu-remote worker: the result was not committed: {e}");
    }
    Ok(0)
}

fn kill_group(pgid: i32, sig: libc::c_int) -> bool {
    if pgid <= 1 {
        return false;
    }
    // SAFETY: a negative pid addresses the process group the caller created with `process_group(0)`.
    unsafe { libc::kill(-pgid, sig) == 0 }
}

/// SIGTERM to a whole process group, then SIGKILL after a short grace.
async fn terminate_group(pgid: i32) {
    if !kill_group(pgid, 0) {
        return;
    }
    kill_group(pgid, libc::SIGTERM);
    tokio::time::sleep(Duration::from_millis(200)).await;
    kill_group(pgid, libc::SIGKILL);
}

/// One connection: the handshake (the launcher's key is the one the first connection showed), then, inside the
/// channel, the launcher's first line `resume`; then its acknowledgements and numbered lines. A newer
/// connection replaces this one.
async fn connection(stream: UnixStream, hub: Shared, place: Rc<Place>, key: Rc<secure::Keypair>) {
    let (rd, mut wr) = stream.into_split();
    let mut rd = Framed::new(BufReader::new(rd));
    let limit = Duration::from_secs(10);
    let (mut sealer, mut opener, launcher) =
        match tokio::time::timeout(limit, secure::accept(&key, &mut rd, &mut wr)).await {
            Ok(Ok(c)) => c,
            Ok(Err(e)) => return eprintln!("mu-remote worker: a connection's handshake failed: {e}"),
            Err(_) => return,
        };
    {
        let mut h = hub.borrow_mut();
        match &h.launcher_key {
            None => h.launcher_key = Some(launcher),
            Some(k) if *k == launcher => {}
            Some(_) => return eprintln!("mu-remote worker: refused a connection whose key is not the launcher's"),
        }
    }
    let (tx, mut rx) = unbounded_channel::<String>();
    tokio::task::spawn_local(async move {
        while let Some(l) = rx.recv().await {
            if secure::write_line(&mut wr, &mut sealer, &l).await.is_err() {
                break;
            }
        }
    });
    let first = tokio::time::timeout(limit, secure::next_line(&mut rd, &mut opener)).await;
    let after = match first {
        Ok(Ok(Some(l))) => match serde_json::from_str::<Value>(&l) {
            Ok(v) if v["type"] == "resume" => v["after"].as_u64().unwrap_or(0),
            _ => return,
        },
        _ => return,
    };
    let serial = hub.borrow_mut().attach(tx, after);
    let mut since_ack = 0u32;
    while let Ok(Some(line)) = secure::next_line(&mut rd, &mut opener).await {
        if !hub.borrow().current(serial) {
            break;
        }
        let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
        if wire::seq_of(&v).is_none() {
            if v["type"] == "ack" {
                hub.borrow_mut().backlog.ack(v["seq"].as_u64().unwrap_or(0));
            }
            continue;
        }
        let input = hub.borrow_mut().take(v);
        match input {
            Input::Gap => break,
            Input::Skip => {}
            Input::Pass => {}
            Input::Start(rx, depth) => {
                tokio::task::spawn_local(run_child(hub.clone(), place.clone(), rx, depth));
            }
        }
        since_ack += 1;
        if since_ack >= ACK_EVERY {
            since_ack = 0;
            hub.borrow().send_ack();
        }
    }
    let mut h = hub.borrow_mut();
    if h.current(serial) {
        h.send_ack();
    }
    h.detach(serial);
}

/// Runs `mu --headless` on the task, feeds it what the launcher sends, numbers what it writes, and commits its
/// changes when it ends.
async fn run_child(hub: Shared, place: Rc<Place>, input: UnboundedReceiver<String>, depth: Option<u64>) {
    let code = child_output(&hub, &place, input, depth).await;
    if let Err(e) = commit_result_async(&place).await {
        hub.borrow_mut().stderr.push_back(format!("the result was not committed: {e}"));
    }
    let mut h = hub.borrow_mut();
    h.exited = true;
    h.child_in = None;
    let line = h.exit_line(code);
    h.emit(&line);
}

async fn child_output(hub: &Shared, place: &Place, mut input: UnboundedReceiver<String>, depth: Option<u64>) -> i32 {
    let mut cmd = tokio::process::Command::new(&place.exe);
    cmd.arg("--headless")
        .current_dir(&place.dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .process_group(0);
    if let Some(d) = depth {
        cmd.env("MU_TASK_DEPTH", d.to_string());
    }
    #[cfg(target_os = "linux")]
    // SAFETY: prctl is async-signal-safe.
    unsafe {
        cmd.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            Ok(())
        });
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            hub.borrow_mut().stderr.push_back(format!("cannot start {}: {e}", place.exe));
            return -1;
        }
    };
    let (Some(mut stdin), Some(stdout), Some(stderr)) = (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        return -1;
    };
    hub.borrow_mut().child_pid = child.id().unwrap_or(0) as i32;
    tokio::task::spawn_local(async move {
        while let Some(l) = input.recv().await {
            if stdin.write_all(l.as_bytes()).await.is_err() || stdin.write_all(b"\n").await.is_err() {
                break;
            }
        }
    });
    let err = tokio::task::spawn_local({
        let hub = hub.clone();
        async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(l)) = lines.next_line().await {
                let mut h = hub.borrow_mut();
                if h.stderr.len() == 20 {
                    h.stderr.pop_front();
                }
                h.stderr.push_back(l);
            }
        }
    });
    let out = tokio::task::spawn_local({
        let hub = hub.clone();
        async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(l)) = lines.next_line().await {
                hub.borrow_mut().emit(&l);
            }
        }
    });
    let status = child.wait().await;
    // A descendant holding a pipe open must not hold the end.
    let _ = tokio::time::timeout(Duration::from_secs(2), out).await;
    let _ = tokio::time::timeout(Duration::from_secs(2), err).await;
    status.ok().and_then(|s| s.code()).unwrap_or(-1)
}

/// `worker bundle`: commits what the worker has changed so far, if it has not yet, and prints the result as a
/// git bundle on top of the checkout's commit. Prints nothing when nothing changed.
pub fn bundle(launch: &str) -> Result<i32> {
    let st = read_state(launch)?;
    let result = commit_result(&st.dir, &st.launch, &st.base).map_err(|e| anyhow!(e))?;
    let Some(result) = result else { return Ok(0) };
    let status = std::process::Command::new("git")
        .args(["bundle", "create", "-q", "-", &result, &format!("^{}", st.base)])
        .current_dir(&st.dir)
        .stdin(Stdio::null())
        .status()
        .context("cannot run git")?;
    Ok(if status.success() { 0 } else { 1 })
}

fn signal_pid(pid: u32, sig: libc::c_int) -> bool {
    let Ok(pid @ 1..) = libc::pid_t::try_from(pid) else { return false };
    // SAFETY: a plain signal to one pid.
    unsafe { libc::kill(pid, sig) == 0 }
}

/// `worker stop`: ends the worker and its child, committing what it changed first.
pub fn stop(launch: &str) -> Result<i32> {
    let st = read_state(launch)?;
    let socket = socket_path(launch)?;
    if running(st.pid) && signal_pid(st.pid, libc::SIGTERM) {
        // The supervisor removes its socket before it commits and exits.
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while socket.exists() && running(st.pid) && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
        }
        std::thread::sleep(Duration::from_millis(300));
        if running(st.pid) && socket.exists() {
            signal_pid(st.pid, libc::SIGKILL);
        }
    }
    commit_result(&st.dir, &st.launch, &st.base).map_err(|e| anyhow!(e))?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc::Line;
    use crate::testutil::{repo, sh, tmp};

    fn hub() -> Hub {
        Hub::new(Backlog::new(1 << 20, tmp().join("backlog")))
    }

    fn sent(rx: &mut UnboundedReceiver<String>) -> Vec<Value> {
        let mut out = Vec::new();
        while let Ok(l) = rx.try_recv() {
            out.push(serde_json::from_str(&l).unwrap());
        }
        out
    }

    fn seqs(v: &[Value]) -> Vec<u64> {
        v.iter().filter_map(wire::seq_of).collect()
    }

    #[test]
    fn lines_are_numbered_and_a_reconnect_gets_the_rest_once() {
        let mut h = hub();
        let (tx, mut rx) = unbounded_channel();
        h.attach(tx, 0);
        for i in 1..=4 {
            h.emit(&json!({"type": "tool", "name": format!("t{i}")}).to_string());
        }
        let seen = sent(&mut rx);
        assert_eq!(seen[0], json!({"type": "resume", "after": 0}), "the worker says what it has first");
        assert_eq!(seqs(&seen), [1, 2, 3, 4]);
        // The launcher saw 1 and 2 only; a second connection replaces the first.
        let (tx2, mut rx2) = unbounded_channel();
        h.attach(tx2, 2);
        h.emit(&json!({"type": "tool", "name": "t5"}).to_string());
        let again = sent(&mut rx2);
        assert_eq!(again.iter().filter_map(|v| v["name"].as_str()).collect::<Vec<_>>(), ["t3", "t4", "t5"]);
        assert_eq!(seqs(&again), [3, 4, 5]);
        assert!(sent(&mut rx).is_empty(), "the old connection gets nothing more");
    }

    #[test]
    fn a_line_that_is_not_json_is_wrapped_before_it_is_numbered() {
        let mut h = hub();
        let (tx, mut rx) = unbounded_channel();
        h.attach(tx, 0);
        h.emit("plain text");
        let got = sent(&mut rx);
        assert_eq!(
            (got[1]["type"].as_str(), got[1]["text"].as_str(), got[1]["seq"].as_u64()),
            (Some("stray"), Some("plain text"), Some(1))
        );
    }

    #[test]
    fn the_first_numbered_line_starts_the_child_and_the_rest_follow_in_order_once() {
        let mut h = hub();
        let Input::Start(mut rx, depth) = h.take(json!({"task": "go", "depth": 2, "seq": 1})) else {
            panic!("the payload starts the child")
        };
        assert_eq!(depth, Some(2));
        assert!(matches!(h.take(json!({"type": "approval", "id": 1, "seq": 2})), Input::Pass));
        assert!(matches!(h.take(json!({"type": "approval", "id": 1, "seq": 2})), Input::Skip), "a duplicate");
        assert!(matches!(h.take(json!({"type": "approval", "id": 3, "seq": 4})), Input::Gap), "a gap");
        assert!(matches!(h.take(json!({"type": "x"})), Input::Skip), "not numbered");
        assert!(matches!(h.take(json!({"type": "approval", "id": 2, "seq": 3})), Input::Pass));
        let got: Vec<String> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert_eq!(
            got,
            [r#"{"task":"go","depth":2}"#, r#"{"type":"approval","id":1}"#, r#"{"type":"approval","id":2}"#]
        );
        assert_eq!(h.in_seen, 3);
        let (tx, mut out) = unbounded_channel();
        h.attach(tx, 0);
        assert_eq!(sent(&mut out)[0], json!({"type": "resume", "after": 3}));
    }

    type Dialed = (
        Framed<BufReader<tokio::net::unix::OwnedReadHalf>>,
        secure::Sealer,
        secure::Opener,
        tokio::net::unix::OwnedWriteHalf,
    );

    /// A launcher with static key `me` connecting to a worker whose key is `key`: handshake, then `resume`.
    async fn dial(hub: &Shared, key: &Rc<secure::Keypair>, me: &secure::Keypair) -> Dialed {
        let (a, b) = UnixStream::pair().unwrap();
        let place = Rc::new(Place { launch: "t".into(), dir: tmp(), base: String::new(), exe: String::new() });
        tokio::task::spawn_local(connection(b, hub.clone(), place, key.clone()));
        let (rd, mut wr) = a.into_split();
        let mut rd = Framed::new(BufReader::new(rd));
        let mut hs = secure::Handshake::initiator(me).unwrap();
        wr.write_all(format!("{}\n", hs.write().unwrap()).as_bytes()).await.unwrap();
        let Some(Line::Value(v)) = rd.next_line().await else { panic!("no handshake answer") };
        hs.read(&v).unwrap();
        wr.write_all(format!("{}\n", hs.write().unwrap()).as_bytes()).await.unwrap();
        let (mut sealer, opener, seen) = hs.finish().unwrap();
        assert_eq!(seen, key.public(), "the launcher sees the worker's key");
        let _ = secure::write_line(&mut wr, &mut sealer, &wire::resume(0)).await;
        (rd, sealer, opener, wr)
    }

    async fn connections(hub: &Shared, n: u64) {
        for _ in 0..200 {
            if hub.borrow().gens >= n {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the worker did not take connection {n}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn the_stream_is_encrypted_and_only_the_launcher_that_came_first_may_reconnect() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let hub: Shared = Rc::new(RefCell::new(hub()));
                let key = Rc::new(secure::Keypair::generate().unwrap());
                let launcher = secure::Keypair::generate().unwrap();
                let (mut rd, _s, mut op, _w) = dial(&hub, &key, &launcher).await;
                connections(&hub, 1).await;
                let first = secure::next_line(&mut rd, &mut op).await.unwrap().unwrap();
                assert_eq!(first, wire::resume(0), "the worker's first line is its resume");
                hub.borrow_mut().emit(r#"{"type":"tool","name":"x"}"#);
                let line = secure::next_line(&mut rd, &mut op).await.unwrap().unwrap();
                assert_eq!(serde_json::from_str::<Value>(&line).unwrap()["seq"], 1);

                // The same launcher reconnects and has the same key; the worker replays what it was not told of.
                let (mut rd2, _s2, mut op2, _w2) = dial(&hub, &key, &launcher).await;
                connections(&hub, 2).await;
                let _resume = secure::next_line(&mut rd2, &mut op2).await.unwrap().unwrap();
                let again = secure::next_line(&mut rd2, &mut op2).await.unwrap().unwrap();
                assert_eq!(serde_json::from_str::<Value>(&again).unwrap()["seq"], 1);

                // Another key is refused: its stream just ends.
                let stranger = secure::Keypair::generate().unwrap();
                let (mut rd3, ..) = dial(&hub, &key, &stranger).await;
                assert!(rd3.next_line().await.is_none(), "a connection with another key gets nothing");
                assert_eq!(hub.borrow().gens, 2, "and does not replace the launcher's");
            })
            .await;
    }

    #[test]
    fn a_stale_connection_is_not_current() {
        let mut h = hub();
        let (a, _ra) = unbounded_channel();
        let (b, _rb) = unbounded_channel();
        let first = h.attach(a, 0);
        let second = h.attach(b, 0);
        assert!(!h.current(first) && h.current(second));
        h.detach(first);
        assert!(h.current(second), "a late close of the old connection leaves the new one");
    }

    #[test]
    fn the_result_is_committed_on_the_base_and_bundles_with_nothing_else() {
        let dir = repo();
        let base = sh(&dir, &["rev-parse", "HEAD"]).trim().to_string();
        assert_eq!(commit_result(&dir, "w-1", &base).unwrap(), None, "no change, no result");
        std::fs::write(dir.join("a.txt"), "changed\n").unwrap();
        std::fs::write(dir.join("new.txt"), "new\n").unwrap();
        let r = commit_result(&dir, "w-1", &base).unwrap().unwrap();
        assert_eq!(r, "refs/mu/result/w-1");
        let tip = sh(&dir, &["rev-parse", &r]).trim().to_string();
        assert_eq!(sh(&dir, &["rev-parse", &format!("{r}^")]).trim(), base);
        assert_eq!(commit_result(&dir, "w-1", &base).unwrap().as_deref(), Some(r.as_str()));
        assert_eq!(sh(&dir, &["rev-parse", &r]).trim(), tip, "an unchanged tree keeps its commit");
        let out = tmp().join("x.bundle");
        sh(&dir, &["bundle", "create", "-q", &out.to_string_lossy(), &r, &format!("^{base}")]);
        let heads = sh(&dir, &["bundle", "list-heads", &out.to_string_lossy()]);
        assert_eq!(heads.trim(), format!("{tip} {r}"));
    }

    #[test]
    fn a_worker_is_known_while_its_process_runs_and_its_socket_exists() {
        let state = tmp();
        assert!(socket_of_in(&state, "w-1").is_none());
        let dir = record_dir(&state, "w-1");
        std::fs::create_dir_all(&dir).unwrap();
        let record = State { pid: std::process::id(), launch: "w-1".into(), dir: tmp(), base: String::new() };
        std::fs::write(dir.join("state.json"), serde_json::to_string(&record).unwrap()).unwrap();
        assert!(socket_of_in(&state, "w-1").is_none(), "no socket yet");
        std::fs::create_dir_all(state.join("run")).unwrap();
        std::fs::write(socket_in(&state, "w-1"), "").unwrap();
        assert_eq!(socket_of_in(&state, "w-1"), Some(socket_in(&state, "w-1")));
        let record = State { pid: 0x7fff_fff0, ..record };
        std::fs::write(dir.join("state.json"), serde_json::to_string(&record).unwrap()).unwrap();
        assert!(socket_of_in(&state, "w-1").is_none(), "a dead process is not a worker");
        assert!(socket_of_in(&state, "../x").is_none());
    }
}
