//! `mu worker`: the remote end of a launched task, run by an adapter (`mu worker run --detach`,
//! `mu attach --stdio ID`, `mu worker bundle ID`, `mu worker stop ID`).
//!
//! `run` is a supervisor that outlives its connections. It serves the stream on a socket
//! (`<profile home>/run/<launch id>.sock`, which `mu attach --stdio` reaches by the launch id), starts
//! `mu --headless` in the checkout when the first connection brings the task, and passes lines both
//! ways. What the child writes is numbered and kept until the parent acknowledges it, so a parent that
//! reconnects with `{"type":"resume","after":N}` gets the rest. Calls and approvals the parent never
//! answered are asked again. When the child ends, whatever it changed is committed as
//! `refs/mu/result/<launch id>` on top of the checkout's commit; `bundle` prints that result as a git
//! bundle, and `stop` ends the worker. The protocol is described in `mu_tools::tools::link`.

use anyhow::{Context, Result, anyhow, bail};
use mu_base::git::{git, snapshot_tree};
use mu_common::paths;
use mu_provider::provider::relay::{Call, Reply};
use mu_session::session::{list, owner};
use mu_tools::tools::link::wire;
use serde_json::{Value, json};
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::rc::Rc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};

/// Unacknowledged lines kept in memory; older ones go to a file.
const MEMORY_CAP: usize = 16 << 20;
/// A worker that never gets its task exits after this long.
const IDLE_START: Duration = Duration::from_secs(30 * 60);
/// `sun_path` holds 104 bytes on macOS (108 on Linux), the terminating NUL included.
const MAX_SOCKET_PATH: usize = 100;

/// The lines the child wrote that the parent has not acknowledged: the newest in memory, the rest
/// in a file once memory is over its cap.
struct Backlog {
    mem: VecDeque<(u64, String)>,
    bytes: usize,
    cap: usize,
    path: PathBuf,
    spill: Option<Spill>,
}

struct Spill {
    file: std::fs::File,
    /// (seq, offset, length) of each line in the file, oldest first.
    index: VecDeque<(u64, u64, usize)>,
    end: u64,
}

impl Backlog {
    fn new(cap: usize, path: PathBuf) -> Self {
        Self { mem: VecDeque::new(), bytes: 0, cap, path, spill: None }
    }

    fn push(&mut self, seq: u64, line: String) {
        self.bytes += line.len();
        self.mem.push_back((seq, line));
        while self.bytes > self.cap && self.mem.len() > 1 {
            let Some((s, l)) = self.mem.pop_front() else { break };
            if self.spill(s, &l).is_err() {
                // The disk refuses: keep it in memory rather than lose it.
                self.mem.push_front((s, l));
                break;
            }
            self.bytes -= l.len();
        }
    }

    fn spill(&mut self, seq: u64, line: &str) -> std::io::Result<()> {
        if self.spill.is_none() {
            let file = std::fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .read(true)
                .write(true)
                .mode(0o600)
                .open(&self.path)?;
            self.spill = Some(Spill { file, index: VecDeque::new(), end: 0 });
        }
        let Some(s) = self.spill.as_mut() else { return Ok(()) };
        s.file.write_all_at(line.as_bytes(), s.end)?;
        s.index.push_back((seq, s.end, line.len()));
        s.end += line.len() as u64;
        Ok(())
    }

    /// Drops every line up to and including `upto`.
    fn ack(&mut self, upto: u64) {
        while self.mem.front().is_some_and(|(s, _)| *s <= upto) {
            if let Some((_, l)) = self.mem.pop_front() {
                self.bytes -= l.len();
            }
        }
        if let Some(s) = self.spill.as_mut() {
            while s.index.front().is_some_and(|(q, ..)| *q <= upto) {
                s.index.pop_front();
            }
            if s.index.is_empty() {
                let _ = s.file.set_len(0);
                s.end = 0;
            }
        }
    }

    /// Hands every line after `after` to `send`, oldest first, until it says no.
    fn replay(&self, after: u64, mut send: impl FnMut(&str) -> bool) {
        if let Some(s) = &self.spill {
            for (seq, at, len) in &s.index {
                if *seq <= after {
                    continue;
                }
                let mut buf = vec![0u8; *len];
                if s.file.read_exact_at(&mut buf, *at).is_err() {
                    return;
                }
                if !send(&String::from_utf8_lossy(&buf)) {
                    return;
                }
            }
        }
        for (seq, line) in &self.mem {
            if *seq > after && !send(line) {
                return;
            }
        }
    }
}

struct Conn {
    serial: u64,
    tx: UnboundedSender<String>,
}

/// What the supervisor knows: its backlog, the connection that is up, what the child waits to hear.
struct Hub {
    backlog: Backlog,
    next_seq: u64,
    /// Relay calls and approvals the child sent that nothing has answered: id -> (seq, line).
    calls: HashMap<u64, (u64, String)>,
    asks: HashMap<u64, (u64, String)>,
    conn: Option<Conn>,
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
            calls: HashMap::new(),
            asks: HashMap::new(),
            conn: None,
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
        let seq = self.next_seq;
        if let Ok(v) = serde_json::from_str::<Value>(raw) {
            match v["type"].as_str() {
                Some("relay") => {
                    if let Some((id, call)) = Call::parse(&v)
                        && !matches!(call, Call::Blob { .. } | Call::Cancel)
                    {
                        self.calls.insert(id, (seq, raw.to_string()));
                    }
                }
                Some("approve") => {
                    if let Some(id) = v["id"].as_u64() {
                        self.asks.insert(id, (seq, raw.to_string()));
                    }
                }
                _ => {}
            }
        }
        let line = wire::numbered(raw, seq);
        self.backlog.push(seq, line.clone());
        if let Some(c) = &self.conn {
            let _ = c.tx.send(line);
        }
    }

    /// A connection says it has everything up to `after`: replays what it lacks, then asks again what
    /// the parent had already seen and never answered. Returns the connection's number.
    fn attach(&mut self, tx: UnboundedSender<String>, after: u64) -> u64 {
        self.gens += 1;
        self.conn = Some(Conn { serial: self.gens, tx: tx.clone() });
        self.backlog.ack(after);
        self.backlog.replay(after, |l| tx.send(l.to_string()).is_ok());
        let mut again: Vec<(u64, String)> =
            self.calls.values().chain(self.asks.values()).filter(|(s, _)| *s <= after).cloned().collect();
        again.sort_by_key(|(s, _)| *s);
        for (_, raw) in again {
            self.emit(&raw);
        }
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

    /// A line from the parent for the child. A final relay answer or an approval answer settles what
    /// the child was waiting for.
    fn pass_on(&mut self, line: &str) {
        if let Ok(v) = serde_json::from_str::<Value>(line) {
            if let Some((id, Reply::Done(_) | Reply::Error(_))) = Reply::parse(&v) {
                self.calls.remove(&id);
            }
            if v["type"] == "approval"
                && let Some(id) = v["id"].as_u64()
            {
                self.asks.remove(&id);
            }
        }
        if let Some(tx) = &self.child_in {
            let _ = tx.send(line.to_string());
        }
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
    exe: PathBuf,
}

/// The worker's own record, for `bundle` and `stop`, which run in other processes.
#[derive(serde::Serialize, serde::Deserialize)]
struct State {
    pid: u32,
    launch: String,
    dir: PathBuf,
    base: String,
}

fn state_dir(launch: &str) -> PathBuf {
    paths::profile_home().join("workers").join(launch)
}

fn read_state(launch: &str) -> Result<State> {
    if !list::valid_id(launch) {
        bail!("invalid worker id `{launch}`");
    }
    let path = state_dir(launch).join("state.json");
    let text = std::fs::read_to_string(&path).map_err(|_| anyhow!("no worker {launch} is known here"))?;
    serde_json::from_str(&text).with_context(|| path.display().to_string())
}

fn socket_path(launch: &str) -> Result<PathBuf> {
    let path = paths::profile_home().join("run").join(format!("{launch}.sock"));
    if path.as_os_str().len() > MAX_SOCKET_PATH {
        bail!("the socket path is too long for a Unix socket ({MAX_SOCKET_PATH} bytes at most): {}", path.display());
    }
    Ok(path)
}

fn result_ref(launch: &str) -> String {
    format!("refs/mu/result/{launch}")
}

/// Commits what the worker changed in `dir` (untracked files included) as the launch's result ref,
/// on top of `base`. `None` when the tree is what `base` holds. Safe to run again: an unchanged tree
/// keeps its commit.
async fn commit_result(dir: &Path, launch: &str, base: &str) -> Result<Option<String>, String> {
    let tree = snapshot_tree(dir).await?;
    let base_tree = git(dir, &["rev-parse", &format!("{base}^{{tree}}")], None).await?;
    let result = result_ref(launch);
    if tree == base_tree.trim() {
        let _ = git(dir, &["update-ref", "-d", &result], None).await;
        return Ok(None);
    }
    let kept = git(dir, &["rev-parse", "--verify", "-q", &format!("{result}^{{tree}}")], None).await;
    if kept.is_ok_and(|t| t.trim() == tree) {
        return Ok(Some(result));
    }
    let message = format!("mu worker {launch}");
    let args =
        ["-c", "user.name=mu", "-c", "user.email=mu@localhost", "commit-tree", &tree, "-p", base, "-m", &message];
    let commit = git(dir, &args, None).await?;
    git(dir, &["update-ref", &result, commit.trim()], None).await?;
    Ok(Some(result))
}

fn sync_git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = std::process::Command::new("git").args(args).current_dir(dir).stdin(Stdio::null()).output()?;
    if !out.status.success() {
        bail!("git {}: {}", args.first().unwrap_or(&""), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// `mu worker run`: serves worker `launch`, whose checkout is `dir`, until it is stopped.
pub fn run(launch: &str, dir: &Path, detached: bool) -> Result<i32> {
    if !list::valid_id(launch) {
        bail!("invalid worker id `{launch}`");
    }
    let dir = std::fs::canonicalize(dir).with_context(|| dir.display().to_string())?;
    let base = sync_git(&dir, &["rev-parse", "HEAD"]).context("the worker's directory is not a git checkout")?;
    let place = Place { launch: launch.to_string(), dir, base, exe: paths::self_exe().context("cannot locate mu")? };
    let state = state_dir(launch);
    mu_common::fs::create_private_dir(&state)?;
    let record = State {
        pid: std::process::id(),
        launch: place.launch.clone(),
        dir: place.dir.clone(),
        base: place.base.clone(),
    };
    mu_common::fs::write_atomic_with(&state.join("state.json"), serde_json::to_string(&record)?.as_bytes(), 0o600)?;
    std::env::set_current_dir(&place.dir)?;
    let rt = mu_base::runtime::current_thread_runtime()?;
    let local = tokio::task::LocalSet::new();
    local.block_on(&rt, serve(place, detached))
}

async fn serve(place: Place, detached: bool) -> Result<i32> {
    let path = socket_path(&place.launch)?;
    let run_dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
    mu_common::fs::create_private_dir(&run_dir)?;
    let _ = std::fs::remove_file(&path);
    // Born 0600, so it is never reachable between bind and chmod.
    let listener =
        mu_common::fs::with_umask(0o077, || UnixListener::bind(&path)).with_context(|| path.display().to_string())?;
    let _record = owner::claim_worker(&place.launch, &place.dir);
    println!("{}", json!({"id": place.launch, "socket": path}));
    if detached {
        let _ = std::io::Write::flush(&mut std::io::stdout());
        // SAFETY: both descriptors are open.
        unsafe { libc::dup2(2, 1) };
    }
    let backlog = Backlog::new(MEMORY_CAP, state_dir(&place.launch).join("backlog"));
    let hub: Shared = Rc::new(RefCell::new(Hub::new(backlog)));
    let place = Rc::new(place);
    let (mut term, mut int, mut hup) =
        (signal(SignalKind::terminate())?, signal(SignalKind::interrupt())?, signal(SignalKind::hangup())?);
    let idle = tokio::time::sleep(IDLE_START);
    tokio::pin!(idle);
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                if let Ok((stream, _)) = accepted {
                    tokio::task::spawn_local(connection(stream, hub.clone(), place.clone()));
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
        mu_base::process::terminate_group(pid).await;
    }
    if let Err(e) = commit_result(&place.dir, &place.launch, &place.base).await {
        eprintln!("mu worker: the result was not committed: {e}");
    }
    Ok(0)
}

/// One connection: its first line is `resume`; then the parent's acknowledgements, the task, and
/// lines for the child. A newer connection replaces this one.
async fn connection(stream: UnixStream, hub: Shared, place: Rc<Place>) {
    let (rd, mut wr) = stream.into_split();
    let (tx, mut rx) = unbounded_channel::<String>();
    tokio::task::spawn_local(async move {
        while let Some(l) = rx.recv().await {
            if wr.write_all(l.as_bytes()).await.is_err() || wr.write_all(b"\n").await.is_err() {
                break;
            }
        }
    });
    let mut rd = BufReader::new(rd).lines();
    let after = match rd.next_line().await {
        Ok(Some(l)) => match serde_json::from_str::<Value>(&l) {
            Ok(v) if v["type"] == "resume" => v["after"].as_u64().unwrap_or(0),
            _ => return,
        },
        _ => return,
    };
    let serial = hub.borrow_mut().attach(tx, after);
    while let Ok(Some(line)) = rd.next_line().await {
        if !hub.borrow().current(serial) {
            break;
        }
        let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
        match v["type"].as_str() {
            Some("ack") => hub.borrow_mut().backlog.ack(v["seq"].as_u64().unwrap_or(0)),
            Some("task") => {
                if !std::mem::replace(&mut hub.borrow_mut().started, true) {
                    tokio::task::spawn_local(run_child(hub.clone(), place.clone(), v["payload"].clone()));
                }
            }
            Some("resume") => {}
            _ => hub.borrow_mut().pass_on(&line),
        }
    }
    hub.borrow_mut().detach(serial);
}

/// Runs `mu --headless` on the task, feeds it what the parent sends, numbers what it writes, and
/// commits its changes when it ends.
async fn run_child(hub: Shared, place: Rc<Place>, payload: Value) {
    let code = child_output(&hub, &place, &payload).await;
    if let Err(e) = commit_result(&place.dir, &place.launch, &place.base).await {
        hub.borrow_mut().stderr.push_back(format!("the result was not committed: {e}"));
    }
    let mut h = hub.borrow_mut();
    h.exited = true;
    h.child_in = None;
    let line = h.exit_line(code);
    h.emit(&line);
}

async fn child_output(hub: &Shared, place: &Place, payload: &Value) -> i32 {
    let mut cmd = tokio::process::Command::new(&place.exe);
    cmd.arg("--headless")
        .current_dir(&place.dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(d) = payload["depth"].as_u64() {
        cmd.env("MU_TASK_DEPTH", d.to_string());
    }
    let mut child = match mu_base::process::spawn_grouped(&mut cmd, true) {
        Ok(c) => c,
        Err(e) => {
            hub.borrow_mut().stderr.push_back(format!("cannot start mu: {e}"));
            return -1;
        }
    };
    let (Some(mut stdin), Some(stdout), Some(stderr)) = (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        return -1;
    };
    let (tx, mut rx) = unbounded_channel::<String>();
    let _ = tx.send(payload.to_string());
    {
        let mut h = hub.borrow_mut();
        h.child_pid = child.id().unwrap_or(0) as i32;
        h.child_in = Some(tx);
    }
    tokio::task::spawn_local(async move {
        while let Some(l) = rx.recv().await {
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

/// `mu worker bundle`: commits what the worker has changed so far, if it has not yet, and prints the
/// result as a git bundle on top of the checkout's commit. Prints nothing when nothing changed.
pub fn bundle(launch: &str) -> Result<i32> {
    let st = read_state(launch)?;
    let rt = mu_base::runtime::current_thread_runtime()?;
    let result = rt.block_on(commit_result(&st.dir, &st.launch, &st.base)).map_err(|e| anyhow!(e))?;
    let Some(result) = result else { return Ok(0) };
    let status = std::process::Command::new("git")
        .args(["bundle", "create", "-q", "-", &result, &format!("^{}", st.base)])
        .current_dir(&st.dir)
        .stdin(Stdio::null())
        .status()
        .context("cannot run git")?;
    Ok(if status.success() { 0 } else { 1 })
}

/// `mu worker stop`: ends the worker and its child, committing what it changed first.
pub fn stop(launch: &str) -> Result<i32> {
    let st = read_state(launch)?;
    let socket = socket_path(launch)?;
    if mu_base::process::signal_pid(st.pid, libc::SIGTERM).is_ok() {
        // The supervisor removes its socket before it commits and exits.
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while socket.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
        }
        std::thread::sleep(Duration::from_millis(300));
        if socket.exists() {
            let _ = mu_base::process::signal_pid(st.pid, libc::SIGKILL);
        }
    }
    let rt = mu_base::runtime::current_thread_runtime()?;
    rt.block_on(commit_result(&st.dir, &st.launch, &st.base)).map_err(|e| anyhow!(e))?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backlog(cap: usize) -> Backlog {
        Backlog::new(cap, mu_testutil::tmp().join("backlog"))
    }

    fn lines(b: &Backlog, after: u64) -> Vec<String> {
        let mut out = Vec::new();
        b.replay(after, |l| {
            out.push(l.to_string());
            true
        });
        out
    }

    #[test]
    fn a_backlog_replays_what_follows_and_forgets_what_is_acknowledged() {
        let mut b = backlog(1 << 20);
        for i in 1..=5 {
            b.push(i, format!("line {i}"));
        }
        assert_eq!(lines(&b, 2), ["line 3", "line 4", "line 5"]);
        b.ack(3);
        assert_eq!(lines(&b, 0), ["line 4", "line 5"]);
        b.ack(9);
        assert!(lines(&b, 0).is_empty());
    }

    #[test]
    fn a_backlog_over_its_cap_spills_to_a_file_and_replays_in_order() {
        let mut b = backlog(30);
        for i in 1..=10 {
            b.push(i, format!("line number {i}"));
        }
        assert!(b.bytes <= 30 + "line number 10".len(), "memory stays near the cap: {}", b.bytes);
        let spilled = b.spill.as_ref().map(|s| s.index.len()).unwrap_or(0);
        assert!(spilled >= 6, "the oldest went to the file: {spilled}");
        let want: Vec<String> = (1..=10).map(|i| format!("line number {i}")).collect();
        assert_eq!(lines(&b, 0), want);
        assert_eq!(lines(&b, 7), want[7..]);
        b.ack(8);
        assert_eq!(lines(&b, 0), want[8..]);
        b.ack(10);
        assert!(lines(&b, 0).is_empty());
        assert_eq!(b.spill.as_ref().map(|s| s.end), Some(0), "the file is emptied");
    }

    fn hub() -> Hub {
        Hub::new(backlog(1 << 20))
    }

    fn sent(rx: &mut tokio::sync::mpsc::UnboundedReceiver<String>) -> Vec<Value> {
        let mut out = Vec::new();
        while let Ok(l) = rx.try_recv() {
            out.push(serde_json::from_str(&l).unwrap());
        }
        out
    }

    fn call(id: u64) -> String {
        Call::Search { args: json!({"q": "x"}) }.line(id).to_string()
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
        assert_eq!(seen.iter().map(|v| v["seq"].as_u64().unwrap()).collect::<Vec<_>>(), [1, 2, 3, 4]);
        // The parent saw 1 and 2 only; a second connection replaces the first.
        let (tx2, mut rx2) = unbounded_channel();
        h.attach(tx2, 2);
        h.emit(&json!({"type": "tool", "name": "t5"}).to_string());
        let again = sent(&mut rx2);
        assert_eq!(again.iter().map(|v| v["name"].as_str().unwrap()).collect::<Vec<_>>(), ["t3", "t4", "t5"]);
        assert_eq!(again.iter().map(|v| v["seq"].as_u64().unwrap()).collect::<Vec<_>>(), [3, 4, 5]);
        assert!(sent(&mut rx).is_empty(), "the old connection gets nothing more");
    }

    #[test]
    fn a_call_the_parent_saw_but_never_answered_is_asked_again_with_its_id() {
        let mut h = hub();
        let (tx, mut rx) = unbounded_channel();
        h.attach(tx, 0);
        h.emit(&call(7)); // seq 1
        h.emit(&call(8)); // seq 2
        h.emit(&json!({"type": "approve", "id": 3, "request": {}}).to_string()); // seq 3
        h.emit(&json!({"type": "tool", "name": "x"}).to_string()); // seq 4
        // 8 is answered meanwhile.
        h.pass_on(&Reply::Done(Box::default()).line(8).to_string());
        let _ = sent(&mut rx);
        // The parent reconnects having seen seq 1..=3: call 7 and approval 3 are asked again, 8 is not,
        // and nothing it has yet to see is sent twice.
        let (tx2, mut rx2) = unbounded_channel();
        h.attach(tx2, 3);
        let got = sent(&mut rx2);
        let kinds: Vec<(String, u64)> =
            got.iter().map(|v| (v["type"].as_str().unwrap().to_string(), v["seq"].as_u64().unwrap())).collect();
        assert_eq!(kinds, [("tool".to_string(), 4), ("relay".to_string(), 5), ("approve".to_string(), 6)], "{got:?}");
        assert_eq!(got[1]["id"], 7);
        assert_eq!(got[2]["id"], 3);
        // An answer settles it for good.
        h.pass_on(&json!({"type": "approval", "id": 3, "decision": {"decision": "deny"}}).to_string());
        h.pass_on(
            &Reply::Error(mu_provider::provider::relay::RelayError::new(
                mu_provider::provider::relay::ErrorKind::Fatal,
                "x",
            ))
            .line(7)
            .to_string(),
        );
        let (tx3, mut rx3) = unbounded_channel();
        h.attach(tx3, 6);
        assert!(sent(&mut rx3).is_empty());
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

    #[tokio::test]
    async fn the_result_is_committed_on_the_base_and_bundles_with_nothing_else() {
        let dir = mu_testutil::repo();
        let base = sync_git(&dir, &["rev-parse", "HEAD"]).unwrap();
        assert_eq!(commit_result(&dir, "w-1", &base).await.unwrap(), None, "no change, no result");
        std::fs::write(dir.join("a.txt"), "changed\n").unwrap();
        std::fs::write(dir.join("new.txt"), "new\n").unwrap();
        let r = commit_result(&dir, "w-1", &base).await.unwrap().unwrap();
        assert_eq!(r, "refs/mu/result/w-1");
        let tip = sync_git(&dir, &["rev-parse", &r]).unwrap();
        assert_eq!(sync_git(&dir, &["rev-parse", &format!("{r}^")]).unwrap(), base);
        assert_eq!(commit_result(&dir, "w-1", &base).await.unwrap().as_deref(), Some(r.as_str()));
        assert_eq!(sync_git(&dir, &["rev-parse", &r]).unwrap(), tip, "an unchanged tree keeps its commit");
        let out = tmp_file();
        let status = std::process::Command::new("git")
            .args(["bundle", "create", "-q"])
            .arg(&out)
            .args([&r, &format!("^{base}")])
            .current_dir(&dir)
            .status()
            .unwrap();
        assert!(status.success());
        let heads = sync_git(&dir, &["bundle", "list-heads", &out.to_string_lossy()]).unwrap();
        assert_eq!(heads, format!("{tip} {r}"));
    }

    fn tmp_file() -> PathBuf {
        mu_testutil::tmp().join("x.bundle")
    }
}
