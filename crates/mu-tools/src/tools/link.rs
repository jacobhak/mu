//! The parent's end of a worker on a remote. The worker (`mu worker run`) outlives its connection:
//! this side reconnects through the remote's `connect` and asks for what it missed.
//!
//! Lines the worker sends carry `seq` (1, 2, ...). On every connection the first line is
//! `{"type":"resume","after":N}`, N being the last `seq` seen; the worker replays from N+1. Until a
//! numbered line has arrived the task is sent too (`{"type":"task","payload":...}`; a worker that has
//! it ignores a second one). `{"type":"ack","seq":N}` lets the worker drop what it kept. Lines the
//! parent sends (relay answers, approvals) are not kept: the worker asks again for what it never
//! got answered. The last line of a worker is `{"type":"exit",...}`.

use super::subagent::read_line_capped;
use serde_json::Value;
use std::collections::VecDeque;
use std::io::Write;
use std::path::Path;
use std::process::Child;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::process::{ChildStderr, ChildStdin, ChildStdout};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio_util::sync::CancellationToken;

/// A remote that runs workers: an adapter of the contract, behind the calls a launch makes. Every
/// call blocks; errors are one line of text.
pub trait WorkerRemote: Send + Sync {
    /// The remote's name, for messages.
    fn name(&self) -> &str;
    /// Whether a worker for the repository at `root` may run there: its trust, and what mu would send.
    fn admit(&self, root: &Path) -> Result<(), String>;
    /// Starts the worker for `launch` in `dir` from `bundle` and returns the id to connect to.
    fn start(&self, launch: &str, bundle: &Path, dir: &str, progress: &mut dyn FnMut(&str)) -> Result<String, String>;
    /// A process whose stdin and stdout are the worker's stream.
    fn connect(&self, id: &str) -> Result<Child, String>;
    /// Writes the worker's result bundle to `out`; nothing when it changed nothing.
    fn fetch(&self, id: &str, out: &mut (dyn Write + Send)) -> Result<(), String>;
    fn stop(&self, id: &str) -> Result<(), String>;
}

/// Where `task` subagents of some kinds run: `remote`, for the kinds in `kinds`.
#[derive(Clone)]
pub struct WorkerLaunch {
    pub remote: Arc<dyn WorkerRemote>,
    pub kinds: Vec<String>,
    /// How long a lost stream is retried before the result is fetched as it stands.
    pub reconnect: Duration,
}

impl std::fmt::Debug for WorkerLaunch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkerLaunch")
            .field("remote", &self.remote.name())
            .field("kinds", &self.kinds)
            .field("reconnect", &self.reconnect)
            .finish()
    }
}

impl WorkerLaunch {
    pub fn runs(&self, agent: &str) -> bool {
        self.kinds.iter().any(|k| k == agent)
    }
}

pub mod wire {
    use serde_json::{Value, json};

    pub fn resume(after: u64) -> String {
        json!({"type": "resume", "after": after}).to_string()
    }

    pub fn ack(seq: u64) -> String {
        json!({"type": "ack", "seq": seq}).to_string()
    }

    pub fn task(payload: &Value) -> String {
        json!({"type": "task", "payload": payload}).to_string()
    }

    pub fn seq_of(v: &Value) -> Option<u64> {
        v.get("seq").and_then(Value::as_u64)
    }

    /// `line` with `seq` added. A line that is not a JSON object becomes a `stray` one.
    pub fn numbered(line: &str, seq: u64) -> String {
        let mut v = match serde_json::from_str::<Value>(line) {
            Ok(v @ Value::Object(_)) => v,
            _ => json!({"type": "stray", "text": line}),
        };
        v["seq"] = json!(seq);
        v.to_string()
    }
}

/// What the stream gives the run.
pub enum Item {
    Line(Vec<u8>),
    /// A line over the cap, dropped.
    TooLong,
    /// The stream is over: the worker said `exit`, the run was cancelled, or the parent gave up.
    End {
        gave_up: bool,
        note: Option<String>,
    },
}

const BACKOFF_START: Duration = Duration::from_millis(250);
const BACKOFF_MAX: Duration = Duration::from_secs(10);
const ACK_EVERY: u32 = 64;
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);
/// A connection that stayed up this long was a working one, whatever it carried.
const WORKED: Duration = Duration::from_secs(10);

enum Read {
    Line(Vec<u8>),
    TooLong,
    Eof,
}

enum Ended {
    Exit,
    Cancelled,
    Lost,
}

struct State {
    seen: u64,
    started: bool,
}

/// Starts the stream to worker `id`: reconnects until it says `exit`, `cancel` fires, or the stream
/// has been down for `give_up`. `lines` are what the parent sends; while no connection is up they are dropped.
pub fn open(
    remote: Arc<dyn WorkerRemote>,
    id: String,
    task: Value,
    lines: UnboundedReceiver<String>,
    cancel: CancellationToken,
    give_up: Duration,
) -> UnboundedReceiver<Item> {
    let (tx, rx) = unbounded_channel();
    tokio::task::spawn_local(async move {
        let end = run(remote, &id, &task, lines, &tx, &cancel, give_up).await;
        let _ = tx.send(end);
    });
    rx
}

async fn run(
    remote: Arc<dyn WorkerRemote>,
    id: &str,
    task: &Value,
    mut lines: UnboundedReceiver<String>,
    tx: &UnboundedSender<Item>,
    cancel: &CancellationToken,
    give_up: Duration,
) -> Item {
    let mut st = State { seen: 0, started: false };
    let mut backoff = BACKOFF_START;
    let mut down_since: Option<Instant> = None;
    let mut last_error = String::new();
    loop {
        if cancel.is_cancelled() {
            return Item::End { gave_up: false, note: None };
        }
        let before = st.seen;
        let r = remote.clone();
        let want = id.to_string();
        let started = Instant::now();
        match tokio::task::spawn_blocking(move || r.connect(&want)).await {
            Ok(Ok(child)) => {
                let (ended, tail) = connection(child, &mut st, task, &mut lines, tx, cancel).await;
                match ended {
                    Ended::Exit | Ended::Cancelled => return Item::End { gave_up: false, note: None },
                    Ended::Lost => {}
                }
                if !tail.is_empty() {
                    last_error = tail;
                }
                if st.seen > before || started.elapsed() >= WORKED {
                    down_since = None;
                    backoff = BACKOFF_START;
                }
            }
            Ok(Err(e)) => last_error = e,
            Err(e) => last_error = format!("connect: {e}"),
        }
        let since = *down_since.get_or_insert_with(Instant::now);
        if since.elapsed() >= give_up {
            let note = format!(
                "lost the stream to worker {id} for {}s{}",
                give_up.as_secs_f64(),
                if last_error.is_empty() { String::new() } else { format!(": {last_error}") }
            );
            return Item::End { gave_up: true, note: Some(note) };
        }
        // What the parent says while no connection is up goes nowhere: the worker asks again.
        let until = tokio::time::Instant::now() + backoff;
        loop {
            tokio::select! {
                () = tokio::time::sleep_until(until) => break,
                l = lines.recv() => {
                    if l.is_none() {
                        return Item::End { gave_up: false, note: None };
                    }
                }
                () = cancel.cancelled() => return Item::End { gave_up: false, note: None },
            }
        }
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

/// Last lines of a connect process's stderr.
async fn drain(stderr: ChildStderr) -> String {
    let mut tail: VecDeque<String> = VecDeque::new();
    let mut rd = tokio::io::AsyncBufReadExt::lines(BufReader::new(stderr));
    while let Ok(Some(l)) = rd.next_line().await {
        if tail.len() == 5 {
            tail.pop_front();
        }
        tail.push_back(l);
    }
    tail.into_iter().collect::<Vec<_>>().join("\n")
}

async fn write(stdin: &mut ChildStdin, line: &str) -> bool {
    let w = async {
        stdin.write_all(line.as_bytes()).await?;
        stdin.write_all(b"\n").await?;
        stdin.flush().await
    };
    matches!(tokio::time::timeout(WRITE_TIMEOUT, w).await, Ok(Ok(())))
}

fn reap(mut child: Child) {
    let _ = child.kill();
    drop(tokio::task::spawn_blocking(move || child.wait()));
}

/// One connection, up to its end. Also the connect process's stderr tail.
async fn connection(
    mut child: Child,
    st: &mut State,
    task: &Value,
    lines: &mut UnboundedReceiver<String>,
    tx: &UnboundedSender<Item>,
    cancel: &CancellationToken,
) -> (Ended, String) {
    let pipes = (child.stdin.take(), child.stdout.take(), child.stderr.take());
    let (Some(stdin), Some(stdout), Some(stderr)) = pipes else {
        reap(child);
        return (Ended::Lost, "the connect process has no pipes".into());
    };
    let (Ok(mut stdin), Ok(stdout), Ok(stderr)) =
        (ChildStdin::from_std(stdin), ChildStdout::from_std(stdout), ChildStderr::from_std(stderr))
    else {
        reap(child);
        return (Ended::Lost, "cannot read the connect process".into());
    };
    while lines.try_recv().is_ok() {}
    let tail = tokio::task::spawn_local(drain(stderr));
    let (rtx, mut rrx) = unbounded_channel::<Read>();
    let reader = tokio::task::spawn_local(async move {
        let mut out = BufReader::new(stdout);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            let msg = match read_line_capped(&mut out, &mut buf, mu_common::rpc::MAX_LINE).await {
                Ok(Some(false)) => Read::Line(std::mem::take(&mut buf)),
                Ok(Some(true)) => Read::TooLong,
                _ => Read::Eof,
            };
            let eof = matches!(msg, Read::Eof);
            if rtx.send(msg).is_err() || eof {
                break;
            }
        }
    });
    let mut ended = Ended::Lost;
    let mut ok = write(&mut stdin, &wire::resume(st.seen)).await;
    if ok && !st.started {
        ok = write(&mut stdin, &wire::task(task)).await;
    }
    let mut since_ack = 0u32;
    while ok {
        tokio::select! {
            r = rrx.recv() => match r {
                None | Some(Read::Eof) => break,
                Some(Read::TooLong) => {
                    let _ = tx.send(Item::TooLong);
                }
                Some(Read::Line(b)) => {
                    let v = serde_json::from_slice::<Value>(&b).ok();
                    if let Some(seq) = v.as_ref().and_then(wire::seq_of) {
                        if seq <= st.seen {
                            continue;
                        }
                        if seq != st.seen + 1 {
                            break;
                        }
                        st.seen = seq;
                        st.started = true;
                    }
                    let exit = v.as_ref().is_some_and(|v| v["type"] == "exit");
                    let _ = tx.send(Item::Line(b));
                    if exit {
                        ended = Ended::Exit;
                        break;
                    }
                    since_ack += 1;
                    if since_ack >= ACK_EVERY {
                        since_ack = 0;
                        ok = write(&mut stdin, &wire::ack(st.seen)).await;
                    }
                }
            },
            l = lines.recv() => match l {
                Some(l) => ok = write(&mut stdin, &l).await,
                None => {
                    ended = Ended::Cancelled;
                    break;
                }
            },
            () = cancel.cancelled() => {
                ended = Ended::Cancelled;
                break;
            }
        }
    }
    reader.abort();
    drop(stdin);
    reap(child);
    let tail = match tokio::time::timeout(Duration::from_secs(1), tail).await {
        Ok(Ok(t)) => t,
        _ => String::new(),
    };
    (ended, tail)
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use std::path::PathBuf;
    use std::process::{Command, Stdio};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(["-c", "user.name=w", "-c", "user.email=w@w"])
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// What a worker's `bundle` prints: `file` holding `text`, committed on top of the task snapshot
    /// in `task_bundle`. `orphan` makes a commit that does not descend from it; `name` is the ref
    /// the bundle holds, the launch's own result ref when `None`.
    pub fn result_bundle(
        task_bundle: &Path,
        launch: &str,
        file: &str,
        text: &str,
        orphan: bool,
        name: Option<&str>,
    ) -> Vec<u8> {
        let w = mu_testutil::tmp();
        git(&w, &["init", "-q"]);
        let task = format!("refs/mu/task/{launch}");
        git(&w, &["fetch", "-q", &task_bundle.to_string_lossy(), &format!("{task}:{task}")]);
        if orphan {
            git(&w, &["checkout", "-q", "--orphan", "other"]);
        } else {
            git(&w, &["checkout", "-q", "--detach", &task]);
        }
        std::fs::write(w.join(file), format!("{text}\n")).unwrap();
        git(&w, &["add", "-A"]);
        git(&w, &["commit", "-q", "-m", "work"]);
        let result = name.map_or_else(|| format!("refs/mu/result/{launch}"), str::to_string);
        git(&w, &["update-ref", &result, "HEAD"]);
        let out = w.join("out.bundle");
        let base = format!("^{task}");
        let mut args = vec!["bundle", "create", "-q", out.to_str().unwrap(), &result];
        if !orphan {
            args.push(&base);
        }
        git(&w, &args);
        std::fs::read(out).unwrap()
    }

    type Result_ = Box<dyn Fn(&Path, &str) -> Vec<u8> + Send + Sync>;

    /// A remote whose connections are shell scripts that play the worker's side of the stream.
    pub struct Fake {
        /// What each connection runs (`sh -c`); the last serves every later one.
        pub scripts: Vec<String>,
        pub connects: AtomicUsize,
        /// What `fetch` writes, given the task bundle and the launch id; nothing when `None`.
        pub result: Option<Result_>,
        pub fetch_error: Option<String>,
        pub start_error: Option<String>,
        pub refuse: Option<String>,
        pub bundle: Mutex<Option<PathBuf>>,
        /// (launch id, dir, heads of the bundle) of each start.
        pub started: Mutex<Vec<(String, String, String)>>,
        pub fetched: AtomicUsize,
        pub stopped: Mutex<Vec<String>>,
    }

    impl Fake {
        pub fn new(scripts: &[&str]) -> Self {
            Self {
                scripts: scripts.iter().map(|s| s.to_string()).collect(),
                connects: AtomicUsize::new(0),
                result: None,
                fetch_error: None,
                start_error: None,
                refuse: None,
                bundle: Mutex::new(None),
                started: Mutex::new(Vec::new()),
                fetched: AtomicUsize::new(0),
                stopped: Mutex::new(Vec::new()),
            }
        }

        /// A worker that commits `text` as `file`.
        pub fn writing(mut self, file: &'static str, text: &'static str) -> Self {
            self.result = Some(Box::new(move |b, id| result_bundle(b, id, file, text, false, None)));
            self
        }
    }

    impl WorkerRemote for Fake {
        fn name(&self) -> &str {
            "fake"
        }
        fn admit(&self, _: &Path) -> Result<(), String> {
            self.refuse.clone().map_or(Ok(()), Err)
        }
        fn start(&self, launch: &str, bundle: &Path, dir: &str, _: &mut dyn FnMut(&str)) -> Result<String, String> {
            if let Some(e) = &self.start_error {
                return Err(e.clone());
            }
            let heads = git(&std::env::temp_dir(), &["bundle", "list-heads", &bundle.to_string_lossy()]);
            self.started.lock().unwrap().push((launch.to_string(), dir.to_string(), heads));
            *self.bundle.lock().unwrap() = Some(bundle.to_path_buf());
            Ok(launch.to_string())
        }
        fn connect(&self, _: &str) -> Result<Child, String> {
            let n = self.connects.fetch_add(1, Ordering::SeqCst);
            let script = &self.scripts[n.min(self.scripts.len() - 1)];
            Command::new("sh")
                .arg("-c")
                .arg(script)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .map_err(|e| e.to_string())
        }
        fn fetch(&self, id: &str, out: &mut (dyn Write + Send)) -> Result<(), String> {
            self.fetched.fetch_add(1, Ordering::SeqCst);
            if let Some(e) = &self.fetch_error {
                return Err(e.clone());
            }
            let bundle = self.bundle.lock().unwrap().clone().expect("started");
            if let Some(make) = &self.result {
                out.write_all(&make(&bundle, id)).map_err(|e| e.to_string())?;
            }
            Ok(())
        }
        fn stop(&self, id: &str) -> Result<(), String> {
            self.stopped.lock().unwrap().push(id.to_string());
            Ok(())
        }
    }

    /// A remote no test reaches.
    pub struct Unused;

    impl WorkerRemote for Unused {
        fn name(&self) -> &str {
            "unused"
        }
        fn admit(&self, _: &Path) -> Result<(), String> {
            Ok(())
        }
        fn start(&self, _: &str, _: &Path, _: &str, _: &mut dyn FnMut(&str)) -> Result<String, String> {
            Err("unused".into())
        }
        fn connect(&self, _: &str) -> Result<Child, String> {
            Err("unused".into())
        }
        fn fetch(&self, _: &str, _: &mut (dyn Write + Send)) -> Result<(), String> {
            Err("unused".into())
        }
        fn stop(&self, _: &str) -> Result<(), String> {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn lines_are_numbered_and_strays_wrapped() {
        let v: Value = serde_json::from_str(&wire::numbered(r#"{"type":"tool","name":"x"}"#, 7)).unwrap();
        assert_eq!((v["type"].as_str(), wire::seq_of(&v)), (Some("tool"), Some(7)));
        let v: Value = serde_json::from_str(&wire::numbered("plain text", 8)).unwrap();
        assert_eq!(
            (v["type"].as_str(), v["text"].as_str(), wire::seq_of(&v)),
            (Some("stray"), Some("plain text"), Some(8))
        );
        assert_eq!(serde_json::from_str::<Value>(&wire::resume(3)).unwrap(), json!({"type":"resume","after":3}));
    }
}
