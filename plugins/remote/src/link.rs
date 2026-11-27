//! The launcher's end of the link to a worker, and the pieces both ends share. The worker (`worker.rs`)
//! outlives its connections: this side reconnects through the adapter's `connect` and asks for what it missed.
//!
//! Every connection starts with the Noise handshake (`secure.rs`): the launcher makes a key for the task, the
//! worker one of its own, the first connection pins the worker's key and a different one later ends the task.
//! Inside the channel the lines are JSON. Each side's first line is `{"type":"resume","after":N}`, N being the
//! highest `seq` of the other side's numbered lines it has received without a gap; it also acknowledges them.
//! Numbered lines carry `seq` (1, 2, … per direction): launcher to worker they are mu's stdin lines (the first
//! is the task payload), worker to launcher the child's stdout lines and last `{"type":"exit",…}`. A side keeps
//! what it sent until `{"type":"ack","seq":N}` (every 64 lines, and when a connection ends cleanly) and replays
//! what follows the peer's `resume` on a new connection. A receiver drops a line it already has and drops the
//! connection on a gap, so the other side replays.

use crate::rpc::{Framed, Line};
use crate::secure::{Handshake, Keypair, Opener, Sealer};
use serde_json::Value;
use std::collections::VecDeque;
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufRead, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};
use tokio_util::sync::CancellationToken;

pub mod wire {
    use serde_json::{Value, json};

    pub fn resume(after: u64) -> String {
        json!({"type": "resume", "after": after}).to_string()
    }

    pub fn ack(seq: u64) -> String {
        json!({"type": "ack", "seq": seq}).to_string()
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

    /// `v` without its `seq`, as the plain line the other end of the child's pipe sees.
    pub fn plain(mut v: Value) -> String {
        if let Some(o) = v.as_object_mut() {
            o.remove("seq");
        }
        v.to_string()
    }
}

/// The numbered lines a side sent that the other has not acknowledged: the newest in memory, the rest in a
/// file once memory is over its cap.
pub struct Backlog {
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
    pub fn new(cap: usize, path: PathBuf) -> Self {
        Self { mem: VecDeque::new(), bytes: 0, cap, path, spill: None }
    }

    /// A backlog that never leaves memory.
    pub fn in_memory() -> Self {
        Self::new(usize::MAX, PathBuf::new())
    }

    pub fn push(&mut self, seq: u64, line: String) {
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
    pub fn ack(&mut self, upto: u64) {
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
    pub fn replay(&self, after: u64, mut send: impl FnMut(&str) -> bool) {
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

/// Runs a closure when dropped: reaps the process behind a stream.
pub struct Guard(Option<Box<dyn FnOnce()>>);

impl Drop for Guard {
    fn drop(&mut self) {
        if let Some(f) = self.0.take() {
            f();
        }
    }
}

/// One connection to the worker: the pipes of the adapter's `connect` process, or a stand-in.
pub struct Stream {
    pub stdin: Box<dyn AsyncWrite + Unpin>,
    pub stdout: Box<dyn AsyncBufRead + Unpin>,
    pub stderr: Option<Box<dyn AsyncRead + Unpin>>,
    pub guard: Guard,
}

impl Stream {
    /// The stream of a process spawned with piped stdio. It is killed and reaped when the stream is dropped.
    pub fn from_child(mut child: std::process::Child) -> Result<Stream, String> {
        let pipes = (child.stdin.take(), child.stdout.take(), child.stderr.take());
        let (Some(stdin), Some(stdout), Some(stderr)) = pipes else {
            let _ = child.kill();
            let _ = child.wait();
            return Err("the connect process has no pipes".into());
        };
        let ends = (
            tokio::process::ChildStdin::from_std(stdin),
            tokio::process::ChildStdout::from_std(stdout),
            tokio::process::ChildStderr::from_std(stderr),
        );
        let (Ok(stdin), Ok(stdout), Ok(stderr)) = ends else {
            let _ = child.kill();
            let _ = child.wait();
            return Err("cannot read the connect process".into());
        };
        let reap = move || {
            let _ = child.kill();
            std::thread::spawn(move || child.wait());
        };
        Ok(Stream {
            stdin: Box::new(stdin),
            stdout: Box::new(BufReader::new(stdout)),
            stderr: Some(Box::new(stderr)),
            guard: Guard(Some(Box::new(reap))),
        })
    }
}

pub struct Opts {
    /// How long a lost stream is retried before the run gives up.
    pub give_up: Duration,
    /// Whether the stream is encrypted. Only a test double that plays a worker's plaintext side says no.
    pub secure: bool,
}

/// How the link ended.
#[derive(Debug, PartialEq)]
pub enum End {
    /// The worker said `exit`.
    Exit {
        code: i64,
        stderr: String,
    },
    Cancelled,
    /// The stream was down for the whole `give_up`; the text says so and why.
    GaveUp(String),
    /// The worker is not the one it was: no retry.
    Refused(String),
}

const BACKOFF_START: Duration = Duration::from_millis(250);
const BACKOFF_MAX: Duration = Duration::from_secs(10);
const ACK_EVERY: u32 = 64;
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);
const HANDSHAKE_WAIT: Duration = Duration::from_secs(30);
/// A connection that stayed up this long was a working one, whatever it carried.
const WORKED: Duration = Duration::from_secs(10);

/// What the launcher keeps between connections.
struct State {
    /// The highest `seq` of the worker's lines received without a gap.
    seen: u64,
    /// The worker's static key, as the first connection showed it.
    worker_key: Option<Vec<u8>>,
    next_out: u64,
    /// mu's lines, numbered, until the worker acknowledges them.
    out: Backlog,
}

impl State {
    /// Numbers and keeps a line from mu; returns it as it goes on the wire.
    fn queue(&mut self, line: &str) -> String {
        self.next_out += 1;
        let numbered = wire::numbered(line, self.next_out);
        self.out.push(self.next_out, numbered.clone());
        numbered
    }
}

enum Read {
    Value(Value),
    Bad(String),
    Eof,
}

struct Channel {
    sealer: Sealer,
    opener: Opener,
}

pub struct Link<'a> {
    /// Starts the connection to the worker.
    pub connect: &'a dyn Fn() -> Result<Stream, String>,
    pub opts: Opts,
    /// The worker's id, for messages.
    pub id: &'a str,
    /// mu's stdin lines, in order. Their end is not the end of the run.
    pub from_mu: UnboundedReceiver<String>,
    pub cancel: CancellationToken,
}

async fn next_from_mu(rx: &mut Option<UnboundedReceiver<String>>) -> String {
    loop {
        match rx {
            Some(r) => match r.recv().await {
                Some(l) => return l,
                None => *rx = None,
            },
            None => std::future::pending::<()>().await,
        }
    }
}

impl Link<'_> {
    /// Reconnects until the worker says `exit`, the run is cancelled, or the stream has been down for
    /// `give_up`. `out` gets each plain line the worker's child wrote.
    pub async fn run(self, out: &mut dyn FnMut(&str)) -> End {
        let Link { connect, opts, id, from_mu, cancel } = self;
        let mut from_mu = Some(from_mu);
        let mut st = State { seen: 0, worker_key: None, next_out: 0, out: Backlog::in_memory() };
        // In memory only, and new for every task.
        let key = match Keypair::generate() {
            Ok(k) => k,
            Err(e) => return End::GaveUp(e),
        };
        let mut backoff = BACKOFF_START;
        let mut down_since: Option<Instant> = None;
        let mut last_error = String::new();
        loop {
            if cancel.is_cancelled() {
                return End::Cancelled;
            }
            let before = st.seen;
            let started = Instant::now();
            match connect() {
                Ok(stream) => {
                    let secure = opts.secure.then_some(&key);
                    let (ended, tail) = connection(stream, &mut st, secure, &mut from_mu, &cancel, out).await;
                    if let Some(end) = ended {
                        return end;
                    }
                    if !tail.is_empty() {
                        last_error = tail;
                    }
                    if st.seen > before || started.elapsed() >= WORKED {
                        down_since = None;
                        backoff = BACKOFF_START;
                    }
                }
                Err(e) => last_error = e,
            }
            let since = *down_since.get_or_insert_with(Instant::now);
            if since.elapsed() >= opts.give_up {
                let why = if last_error.is_empty() { String::new() } else { format!(": {last_error}") };
                return End::GaveUp(format!("lost the stream to worker {id} for {}s{why}", opts.give_up.as_secs_f64()));
            }
            let until = tokio::time::Instant::now() + backoff;
            loop {
                tokio::select! {
                    () = tokio::time::sleep_until(until) => break,
                    l = next_from_mu(&mut from_mu) => {
                        st.queue(&l);
                    }
                    () = cancel.cancelled() => return End::Cancelled,
                }
            }
            backoff = (backoff * 2).min(BACKOFF_MAX);
        }
    }
}

/// Last lines of a connect process's stderr.
async fn drain(stderr: Box<dyn AsyncRead + Unpin>) -> String {
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

async fn write(stdin: &mut (dyn AsyncWrite + Unpin), line: &str) -> bool {
    let w = async {
        stdin.write_all(line.as_bytes()).await?;
        stdin.write_all(b"\n").await?;
        stdin.flush().await
    };
    matches!(tokio::time::timeout(WRITE_TIMEOUT, w).await, Ok(Ok(())))
}

/// Sends `line`: through the channel when there is one.
async fn send(stdin: &mut (dyn AsyncWrite + Unpin), chan: &mut Option<Channel>, line: &str) -> bool {
    let Some(c) = chan else { return write(stdin, line).await };
    let Ok(wire) = c.sealer.seal(line) else { return false };
    for w in &wire {
        if !write(stdin, w).await {
            return false;
        }
    }
    true
}

/// The handshake as the initiator: the worker's key comes back with the channel.
async fn handshake(
    key: &Keypair,
    stdin: &mut (dyn AsyncWrite + Unpin),
    rrx: &mut UnboundedReceiver<Read>,
) -> Result<(Channel, Vec<u8>), String> {
    let work = async {
        let closed = || "the stream closed during the handshake".to_string();
        let mut hs = Handshake::initiator(key)?;
        if !write(stdin, &hs.write()?).await {
            return Err(closed());
        }
        let line = match rrx.recv().await {
            Some(Read::Value(v)) => v,
            Some(Read::Bad(_)) => return Err("the worker did not answer the handshake".into()),
            _ => return Err(closed()),
        };
        hs.read(&line)?;
        if !write(stdin, &hs.write()?).await {
            return Err(closed());
        }
        let (sealer, opener, remote) = hs.finish()?;
        Ok((Channel { sealer, opener }, remote))
    };
    tokio::time::timeout(HANDSHAKE_WAIT, work)
        .await
        .unwrap_or_else(|_| Err("the worker did not answer the handshake".into()))
}

/// One connection, up to its end: `Some(end)` when the run is over, `None` when the stream was lost. Also the
/// connect process's stderr tail, and what ended the connection.
async fn connection(
    stream: Stream,
    st: &mut State,
    key: Option<&Keypair>,
    from_mu: &mut Option<UnboundedReceiver<String>>,
    cancel: &CancellationToken,
    out: &mut dyn FnMut(&str),
) -> (Option<End>, String) {
    let Stream { mut stdin, stdout, stderr, guard: _guard } = stream;
    let tail = tokio::task::spawn_local(async move {
        match stderr {
            Some(e) => drain(e).await,
            None => String::new(),
        }
    });
    let (rtx, mut rrx) = unbounded_channel::<Read>();
    let reader = tokio::task::spawn_local(async move {
        let mut lines = Framed::new(stdout);
        loop {
            let msg = match lines.next_line().await {
                Some(Line::Value(v)) => Read::Value(v),
                Some(Line::Bad(e)) => Read::Bad(e),
                None => Read::Eof,
            };
            let eof = matches!(msg, Read::Eof);
            if rtx.send(msg).is_err() || eof {
                break;
            }
        }
    });
    let mut chan = None;
    if let Some(key) = key {
        match handshake(key, &mut *stdin, &mut rrx).await {
            Ok((c, remote)) => match &st.worker_key {
                None => {
                    st.worker_key = Some(remote);
                    chan = Some(c);
                }
                Some(k) if *k == remote => chan = Some(c),
                Some(_) => {
                    reader.abort();
                    let note = "the worker's key is not the one it showed first: the stream was taken over, so the \
                                task is dropped";
                    return (Some(End::Refused(note.into())), String::new());
                }
            },
            Err(e) => {
                reader.abort();
                return (None, e);
            }
        }
    }
    let mut note = String::new();
    let mut ended = None;
    // The worker has said what it has: until then nothing but `resume` goes to it.
    let mut ready = false;
    let mut ok = send(&mut *stdin, &mut chan, &wire::resume(st.seen)).await;
    let mut since_ack = 0u32;
    while ok {
        tokio::select! {
            r = rrx.recv() => {
                let wire_value = match r {
                    None | Some(Read::Eof) => break,
                    Some(Read::Bad(e)) => {
                        if chan.is_some() {
                            note = format!("a line of the stream is not encrypted: {e}");
                            break;
                        }
                        continue;
                    }
                    Some(Read::Value(v)) => v,
                };
                let m = match chan.as_mut() {
                    Some(c) => match c.opener.open(&wire_value) {
                        Ok(Some(line)) => match serde_json::from_str::<Value>(&line) {
                            Ok(m) => m,
                            Err(_) => continue,
                        },
                        Ok(None) => continue,
                        Err(e) => {
                            note = e;
                            break;
                        }
                    },
                    None => wire_value,
                };
                if let Some(seq) = wire::seq_of(&m) {
                    if seq <= st.seen {
                        continue;
                    }
                    if seq != st.seen + 1 {
                        break;
                    }
                    st.seen = seq;
                    if m["type"] == "exit" {
                        let code = m["code"].as_i64().unwrap_or(-1);
                        let stderr = m["stderr"].as_str().unwrap_or_default().to_string();
                        let _ = send(&mut *stdin, &mut chan, &wire::ack(st.seen)).await;
                        ended = Some(End::Exit { code, stderr });
                        break;
                    }
                    out(&wire::plain(m));
                    since_ack += 1;
                    if since_ack >= ACK_EVERY {
                        since_ack = 0;
                        ok = send(&mut *stdin, &mut chan, &wire::ack(st.seen)).await;
                    }
                } else {
                    match m["type"].as_str() {
                        Some("resume") => {
                            let after = m["after"].as_u64().unwrap_or(0);
                            st.out.ack(after);
                            let mut again = Vec::new();
                            st.out.replay(after, |l| {
                                again.push(l.to_string());
                                true
                            });
                            ready = true;
                            for l in again {
                                ok = ok && send(&mut *stdin, &mut chan, &l).await;
                            }
                        }
                        Some("ack") => st.out.ack(m["seq"].as_u64().unwrap_or(0)),
                        _ => {}
                    }
                }
            },
            l = next_from_mu(from_mu) => {
                let numbered = st.queue(&l);
                if ready {
                    ok = send(&mut *stdin, &mut chan, &numbered).await;
                }
            },
            () = cancel.cancelled() => {
                let _ = send(&mut *stdin, &mut chan, &wire::ack(st.seen)).await;
                ended = Some(End::Cancelled);
                break;
            }
        }
    }
    reader.abort();
    drop(stdin);
    let tail = match tokio::time::timeout(Duration::from_secs(1), tail).await {
        Ok(Ok(t)) => t,
        _ => String::new(),
    };
    let tail = match (note.is_empty(), tail.is_empty()) {
        (true, _) => tail,
        (false, true) => note,
        (false, false) => format!("{note}\n{tail}"),
    };
    (ended, tail)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secure;
    use crate::testutil::tmp;
    use serde_json::json;
    use std::cell::{Cell, RefCell};
    use std::process::{Command, Stdio};

    fn backlog(cap: usize) -> Backlog {
        Backlog::new(cap, tmp().join("backlog"))
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
        assert_eq!(wire::plain(json!({"type":"tool","seq":4})), r#"{"type":"tool"}"#);
    }

    /// A connection that runs a shell script playing the worker's side, without encryption.
    fn script(body: &str) -> Result<Stream, String> {
        let child = Command::new("sh")
            .arg("-c")
            .arg(body)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| e.to_string())?;
        Stream::from_child(child)
    }

    /// What a plain worker says first, and the line the launcher's `resume` is read into.
    fn hello(after: u64) -> String {
        format!("echo '{{\"type\":\"resume\",\"after\":{after}}}'; IFS= read -r a\n")
    }

    const DONE: &str = r#"echo '{"type":"tool","name":"t","summary":"a","seq":1}'
echo '{"type":"result","text":"done","seq":2}'
echo '{"type":"exit","code":0,"stderr":"","seq":3}'"#;

    struct Ran {
        end: End,
        out: Vec<String>,
        connects: usize,
    }

    /// Runs the link over `scripts` (the last serves every later connection) with `mu_lines` queued first.
    async fn run_plain(scripts: Vec<String>, mu_lines: &[&str], give_up: Duration) -> Ran {
        let (tx, rx) = unbounded_channel();
        for l in mu_lines {
            tx.send(l.to_string()).unwrap();
        }
        let connects = Cell::new(0usize);
        let connect = || {
            let n = connects.get();
            connects.set(n + 1);
            script(&scripts[n.min(scripts.len() - 1)])
        };
        let link = Link {
            connect: &connect,
            opts: Opts { give_up, secure: false },
            id: "w-1",
            from_mu: rx,
            cancel: CancellationToken::new(),
        };
        let out = RefCell::new(Vec::new());
        let end = tokio::task::LocalSet::new().run_until(link.run(&mut |l| out.borrow_mut().push(l.to_string()))).await;
        drop(tx);
        Ran { end, out: out.into_inner(), connects: connects.get() }
    }

    fn tool(summary: &str, seq: u64) -> String {
        format!("echo '{{\"type\":\"tool\",\"name\":\"t\",\"summary\":\"{summary}\",\"seq\":{seq}}}'")
    }

    fn summaries(out: &[String]) -> Vec<String> {
        out.iter().map(|l| serde_json::from_str::<Value>(l).unwrap()["summary"].to_string()).collect()
    }

    #[tokio::test(flavor = "current_thread")]
    async fn lines_reach_mu_plain_and_the_exit_line_ends_the_run() {
        let ran = run_plain(vec![format!("{}{DONE}", hello(0))], &[], Duration::from_secs(5)).await;
        assert_eq!(ran.end, End::Exit { code: 0, stderr: String::new() });
        assert_eq!(ran.out.len(), 2);
        assert!(ran.out.iter().all(|l| !l.contains("seq")), "{:?}", ran.out);
    }

    /// The second connection resumes after the last line seen, gets the lines it missed once, and the
    /// task it already has is not sent again.
    #[tokio::test(flavor = "current_thread")]
    async fn a_dropped_stream_resumes_with_no_line_lost_or_repeated() {
        let log = tmp();
        let first = format!(
            "{h}IFS= read -r b; printf '%s\\n%s\\n' \"$a\" \"$b\" > {l}/in1\n{}\n{}\n{}\nsleep 0.2",
            tool("a", 1),
            tool("b", 2),
            tool("c", 3),
            h = hello(0),
            l = log.display()
        );
        // A worker that kept b and c unacknowledged replays them: the launcher has them already.
        let second = format!(
            "echo '{{\"type\":\"resume\",\"after\":1}}'\nIFS= read -r a; printf '%s\\n' \"$a\" > {l}/in2\n{}\n{}\n{}\n\
             echo '{{\"type\":\"result\",\"text\":\"done\",\"seq\":5}}'\n\
             echo '{{\"type\":\"exit\",\"code\":0,\"stderr\":\"\",\"seq\":6}}'",
            tool("b", 2),
            tool("c", 3),
            tool("d", 4),
            l = log.display()
        );
        let ran = run_plain(vec![first, second], &[r#"{"task":"go"}"#], Duration::from_secs(5)).await;
        assert!(matches!(ran.end, End::Exit { code: 0, .. }), "{:?}", ran.end);
        assert_eq!(ran.connects, 2);
        assert_eq!(summaries(&ran.out[..4]), ["\"a\"", "\"b\"", "\"c\"", "\"d\""]);
        let seen = std::fs::read_to_string(log.join("in1")).unwrap();
        let mut seen = seen.lines();
        assert_eq!(seen.next(), Some(wire::resume(0).as_str()));
        assert_eq!(seen.next(), Some(r#"{"task":"go","seq":1}"#), "the payload is seq 1, after the worker's resume");
        let again = std::fs::read_to_string(log.join("in2")).unwrap();
        assert_eq!(again.lines().collect::<Vec<_>>(), [wire::resume(3)], "resume after 3, no payload again");
    }

    /// An answer sent while the stream was down is replayed after the reconnect, once.
    #[tokio::test(flavor = "current_thread")]
    async fn an_answer_sent_while_the_stream_is_down_is_replayed_after_the_reconnect() {
        let log = tmp();
        // The first connection never says `resume` and dies: mu's lines are only kept.
        let first = "sleep 0.1".to_string();
        let second = format!(
            "echo '{{\"type\":\"resume\",\"after\":1}}'\nexec 3<&0; IFS= read -r a; cat <&3 > {l}/in2 &\n{}\nsleep 0.5\n{}\n\
             echo '{{\"type\":\"exit\",\"code\":0,\"stderr\":\"\",\"seq\":3}}'",
            tool("a", 1),
            tool("b", 2),
            l = log.display()
        );
        let lines =
            [r#"{"task":"go"}"#, r#"{"type":"approval","id":1,"decision":{"decision":"allow"}}"#, "not json at all"];
        let ran = run_plain(vec![first, second], &lines, Duration::from_secs(5)).await;
        assert!(matches!(ran.end, End::Exit { .. }), "{:?}", ran.end);
        let got = std::fs::read_to_string(log.join("in2")).unwrap();
        let got: Vec<Value> =
            got.lines().map(|l| serde_json::from_str::<Value>(l).unwrap()).filter(|v| v["type"] != "ack").collect();
        assert_eq!(got.iter().map(|v| v["seq"].as_u64().unwrap()).collect::<Vec<_>>(), [2, 3], "{got:?}");
        assert_eq!(got[0]["type"], "approval");
        assert_eq!((got[1]["type"].as_str(), got[1]["text"].as_str()), (Some("stray"), Some("not json at all")));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_gap_in_the_numbers_drops_the_connection_and_asks_again() {
        let first = format!("{}{}\n{}\nsleep 0.2", hello(0), tool("a", 1), tool("c", 3));
        let second = format!(
            "{}{}\n{}\n{}\n{}",
            hello(1),
            tool("b", 2),
            tool("c", 3),
            r#"echo '{"type":"result","text":"done","seq":4}'"#,
            r#"echo '{"type":"exit","code":0,"stderr":"","seq":5}'"#
        );
        let ran = run_plain(vec![first, second], &[], Duration::from_secs(5)).await;
        assert!(matches!(ran.end, End::Exit { .. }), "{:?}", ran.end);
        assert_eq!(summaries(&ran.out[..3]), ["\"a\"", "\"b\"", "\"c\""]);
        assert_eq!(ran.out.len(), 4);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_stream_down_past_the_timeout_gives_up_and_says_why() {
        let one = format!("{}{}\necho 'worker is gone' >&2", hello(0), tool("a", 1));
        let started = Instant::now();
        let ran = run_plain(vec![one], &[], Duration::from_millis(700)).await;
        let End::GaveUp(note) = &ran.end else { panic!("{:?}", ran.end) };
        assert!(
            note.starts_with("lost the stream to worker w-1 for 0.7s") && note.contains("worker is gone"),
            "{note}"
        );
        assert!(ran.connects >= 2, "it tried again");
        assert!(started.elapsed() < Duration::from_secs(10), "{:?}", started.elapsed());
        assert_eq!(summaries(&ran.out), ["\"a\""], "each connection replays line 1, and it counts once");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_connect_that_fails_is_retried_then_reported() {
        let (_tx, rx) = unbounded_channel();
        let connect = || Err::<Stream, _>("no route".to_string());
        let link = Link {
            connect: &connect,
            opts: Opts { give_up: Duration::from_millis(300), secure: false },
            id: "w-1",
            from_mu: rx,
            cancel: CancellationToken::new(),
        };
        let end = tokio::task::LocalSet::new().run_until(link.run(&mut |_| {})).await;
        assert_eq!(end, End::GaveUp("lost the stream to worker w-1 for 0.3s: no route".into()));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancel_ends_the_run_with_the_stream_up() {
        let (_tx, rx) = unbounded_channel();
        let connect = || script(&format!("{}{}\nexec sleep 30", hello(0), tool("a", 1)));
        let cancel = CancellationToken::new();
        let link = Link {
            connect: &connect,
            opts: Opts { give_up: Duration::from_secs(5), secure: false },
            id: "w-1",
            from_mu: rx,
            cancel: cancel.clone(),
        };
        let seen = Cell::new(0);
        let end = tokio::task::LocalSet::new()
            .run_until(async {
                let stopper = async {
                    while seen.get() == 0 {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                    cancel.cancel();
                };
                let mut count = |_: &str| seen.set(seen.get() + 1);
                tokio::join!(link.run(&mut count), stopper).0
            })
            .await;
        assert_eq!(end, End::Cancelled);
    }

    /// A worker in this process: serves the encrypted side over in-memory pipes, one `Stream` per call.
    struct Peer {
        key: secure::Keypair,
        /// What it sends after its `resume`.
        says: Vec<String>,
        /// Whether it then drops the stream.
        close: bool,
        /// What the launcher sent it on each connection, decrypted.
        got: std::rc::Rc<RefCell<Vec<Vec<String>>>>,
    }

    impl Peer {
        fn stream(&self, after: u64) -> Stream {
            let (to_peer, from_launcher) = tokio::io::duplex(1 << 16);
            let (to_launcher, from_peer) = tokio::io::duplex(1 << 16);
            let (key, says, got) = (self.key.clone(), self.says.clone(), self.got.clone());
            let close = self.close;
            tokio::task::spawn_local(async move {
                let mut rd = Framed::new(BufReader::new(from_launcher));
                let mut wr = to_launcher;
                let Ok((mut sealer, mut opener, _)) = secure::accept(&key, &mut rd, &mut wr).await else { return };
                let n = {
                    got.borrow_mut().push(Vec::new());
                    got.borrow().len() - 1
                };
                let _ = secure::write_line(&mut wr, &mut sealer, &wire::resume(after)).await;
                for l in &says {
                    let _ = secure::write_line(&mut wr, &mut sealer, l).await;
                }
                if close {
                    return;
                }
                while let Ok(Some(l)) = secure::next_line(&mut rd, &mut opener).await {
                    got.borrow_mut()[n].push(l);
                }
            });
            Stream {
                stdin: Box::new(to_peer),
                stdout: Box::new(BufReader::new(from_peer)),
                stderr: None,
                guard: Guard(None),
            }
        }
    }

    fn peer(says: &[&str], close: bool) -> Peer {
        Peer {
            key: secure::Keypair::generate().unwrap(),
            says: says.iter().map(|s| s.to_string()).collect(),
            close,
            got: Default::default(),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn an_encrypted_stream_carries_the_payload_and_the_lines() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let p = peer(
                    &[
                        r#"{"type":"tool","name":"t","summary":"a","seq":1}"#,
                        r#"{"type":"exit","code":3,"stderr":"boom","seq":2}"#,
                    ],
                    false,
                );
                let (tx, rx) = unbounded_channel();
                tx.send(r#"{"task":"go"}"#.to_string()).unwrap();
                let connect = || Ok(p.stream(0));
                let link = Link {
                    connect: &connect,
                    opts: Opts { give_up: Duration::from_secs(5), secure: true },
                    id: "w-1",
                    from_mu: rx,
                    cancel: CancellationToken::new(),
                };
                let out = RefCell::new(Vec::new());
                let end = link.run(&mut |l| out.borrow_mut().push(l.to_string())).await;
                assert_eq!(end, End::Exit { code: 3, stderr: "boom".into() });
                assert_eq!(out.borrow().len(), 1);
                tokio::time::sleep(Duration::from_millis(100)).await;
                let got = p.got.borrow();
                assert_eq!(got[0][0], wire::resume(0));
                assert_eq!(got[0][1], r#"{"task":"go","seq":1}"#);
                assert_eq!(got[0].last().map(String::as_str), Some(wire::ack(2).as_str()), "acknowledged at the end");
                drop(tx);
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_different_worker_key_on_a_later_connection_is_refused() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let a = peer(&[r#"{"type":"tool","name":"t","summary":"a","seq":1}"#], true);
                let b = peer(&[r#"{"type":"exit","code":0,"stderr":"","seq":2}"#], false);
                let (_tx, rx) = unbounded_channel();
                let n = Cell::new(0);
                let connect = || {
                    n.set(n.get() + 1);
                    // The first peer drops its stream after one line; an impostor answers the next.
                    Ok(if n.get() == 1 { a.stream(0) } else { b.stream(1) })
                };
                let link = Link {
                    connect: &connect,
                    opts: Opts { give_up: Duration::from_secs(5), secure: true },
                    id: "w-1",
                    from_mu: rx,
                    cancel: CancellationToken::new(),
                };
                let end = link.run(&mut |_| {}).await;
                let End::Refused(note) = end else { panic!("{end:?}") };
                assert!(note.contains("not the one it showed first"), "{note}");
            })
            .await;
    }
}
