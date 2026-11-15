//! The terminal UI as a client of a `mu agent` that a `Link` reaches: the shell command of
//! `mu --attach COMMAND`, or of a take-over's nested client.
//!
//! The command's process has stdin and stdout piped: they carry the wire lines (see `mu_base::wire`),
//! plain JSON behind the hello, so the stream is as private as whatever the command runs over. stderr
//! lines are shown as notices. The UI runs as it does in one process, over a `RemoteHost` and the
//! client adapter. When the connection drops the UI stays up and says so; the command is run again
//! after 1 s, doubling to 30 s, and the agent's catch-up repaints the conversation. The agent ending, a
//! take-over by another client and `/detach` end the client for good. An agent that already has
//! another client answers `Busy`: a reconnect then stops retrying and asks whether to take the session
//! over.

use super::unlock::{self, Calls, Terminal};
use anyhow::{Result, bail};
use mu_base::config::Configs;
use mu_base::events::{AgentEvent, Answer, AskQuestion, Command};
use mu_base::wire::{AgentLine, ClientAdapter, ClientLine, Incoming, WIRE, WireCommand, to_line};
use mu_common::rpc::{Framed, Line};
use mu_tui::host::remote::RemoteHost;
use mu_tui::host::{Host, HostInfo};
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command as Process};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

const HELLO_WAIT: Duration = Duration::from_secs(30);
const FIRST_PAUSE: Duration = Duration::from_secs(1);
const LONGEST_PAUSE: Duration = Duration::from_secs(30);

/// Who this client is to an agent: the same across its connections, so a lost one's successor is not
/// taken for another client.
struct Who {
    host: String,
    client: String,
}

#[derive(Clone, Copy)]
struct Join {
    reconnect: bool,
    take: bool,
}

enum Refused {
    /// Another client has the session.
    Busy {
        host: String,
        since: i64,
    },
    Failed(String),
}

impl From<String> for Refused {
    fn from(e: String) -> Self {
        Refused::Failed(e)
    }
}

fn hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: the buffer is as long as the length passed.
    let ok = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len() - 1) } == 0;
    let end = buf.iter().position(|b| *b == 0).unwrap_or(0);
    match ok {
        true if end > 0 => String::from_utf8_lossy(&buf[..end]).into_owned(),
        _ => "unknown host".into(),
    }
}

/// `since` (unix seconds) as the local time of day, with the date when it is not today.
fn since_text(since: i64) -> String {
    let Ok(at) = jiff::Timestamp::from_second(since) else { return "an unknown time".into() };
    let zone = jiff::tz::TimeZone::system();
    let at = at.to_zoned(zone.clone());
    let today = jiff::Timestamp::now().to_zoned(zone).date();
    if at.date() == today { at.strftime("%H:%M").to_string() } else { at.strftime("%Y-%m-%d %H:%M").to_string() }
}

/// What the client's connection runs each time it connects: a shell command that is a raw pipe to an
/// agent (`mu attach --stdio ID` somewhere), and the SSH key that unlocks the agent's sealed state.
#[derive(Clone)]
pub struct Link {
    pub command: String,
    pub state_key: PathBuf,
}

impl Link {
    pub fn new(command: String, state_key: Option<PathBuf>) -> Self {
        let state_key = state_key.unwrap_or_else(|| mu_common::paths::home().join(".ssh/id_ed25519"));
        Self { command, state_key }
    }

    fn describe(&self) -> String {
        format!("`{}`", self.command)
    }

    fn process(&self) -> Process {
        let mut p = Process::new("/bin/sh");
        p.arg("-c")
            .arg(&self.command)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        p
    }
}

/// The link's stderr lines: kept for the error while the hello is awaited, shown as notices after.
type Stderr = Rc<RefCell<(Vec<String>, bool)>>;

/// The refusal for a link that ended before an agent answered: its stderr, which is the adapter's error.
async fn ended(child: &mut Child, task: tokio::task::JoinHandle<()>, seen: &Stderr, fallback: String) -> Refused {
    let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
    let _ = tokio::time::timeout(Duration::from_secs(1), task).await;
    let lines = seen.borrow().0.join("\n");
    Refused::Failed(if lines.is_empty() { fallback } else { lines })
}

struct Conn {
    child: Child,
    lines: UnboundedReceiver<AgentLine>,
    out: UnboundedSender<ClientLine>,
    /// Set by the writer when it sends `Detach`: the end of the connection is then not a loss.
    detached: Rc<Cell<bool>>,
    /// Lines read ahead while a host call was awaited before the session loop started: served first.
    backlog: VecDeque<AgentLine>,
}

/// Runs the link, says hello and waits for the agent's. Returns the connection and what the host told.
async fn connect(
    link: &Link,
    notices: &UnboundedSender<String>,
    who: &Who,
    join: Join,
) -> Result<(Conn, HostInfo), Refused> {
    let command = link.describe();
    let mut child = link.process().spawn().map_err(|e| format!("cannot run {command}: {e}"))?;
    let (Some(mut stdin), Some(stdout), Some(stderr)) = (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        return Err("cannot reach the command's streams".to_string().into());
    };
    let seen: Stderr = Rc::default();
    let (to_notices, kept) = (notices.clone(), seen.clone());
    let stderr_task = tokio::task::spawn_local(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(l)) = lines.next_line().await {
            let mut k = kept.borrow_mut();
            if k.1 {
                let _ = to_notices.send(l);
            } else {
                k.0.push(l);
            }
        }
    });

    let hello = ClientLine::Hello {
        wire: WIRE,
        version: env!("MU_VERSION").into(),
        reconnect: join.reconnect,
        host: who.host.clone(),
        client: who.client.clone(),
        take: join.take,
    };
    if let Err(e) = stdin.write_all(format!("{}\n", to_line(&hello)).as_bytes()).await {
        return Err(ended(&mut child, stderr_task, &seen, format!("{command}: {e}")).await);
    }
    let mut framed = Framed::new(BufReader::new(stdout));
    let answer = tokio::time::timeout(HELLO_WAIT, framed.next_line())
        .await
        .map_err(|_| format!("{command} did not answer within {}s", HELLO_WAIT.as_secs()))?;
    let value = match answer {
        Some(Line::Value(v)) => v,
        _ => {
            let none = format!("{command} ended without an answer from an agent");
            return Err(ended(&mut child, stderr_task, &seen, none).await);
        }
    };
    let info = match serde_json::from_value::<AgentLine>(value) {
        Ok(AgentLine::Hello { wire, host, .. }) if wire == WIRE => serde_json::from_value::<HostInfo>(host)
            .map_err(|e| format!("the agent's host facts are unreadable: {e}"))?,
        Ok(AgentLine::Hello { wire, .. }) => {
            return Err(
                format!("the agent speaks wire {wire}, this mu wire {WIRE}: use the same mu on both sides").into()
            );
        }
        Ok(AgentLine::Busy { host, since }) => return Err(Refused::Busy { host, since }),
        Ok(AgentLine::Error(e)) => return Err(e.into()),
        _ => return Err(format!("{command} is not an agent's stream").into()),
    };
    {
        let mut k = seen.borrow_mut();
        for l in k.0.drain(..) {
            let _ = notices.send(l);
        }
        k.1 = true;
    }

    let (lines_tx, lines) = unbounded_channel();
    let broken = notices.clone();
    tokio::task::spawn_local(async move {
        loop {
            match framed.next_line().await {
                Some(Line::Value(v)) => {
                    if let Ok(line) = serde_json::from_value::<AgentLine>(v)
                        && lines_tx.send(line).is_err()
                    {
                        return;
                    }
                }
                Some(Line::Bad(e)) => {
                    let _ = broken.send(format!("a line of the stream from the agent is unreadable: {e}"));
                }
                None => return,
            }
        }
    });
    let (out, mut out_rx) = unbounded_channel::<ClientLine>();
    let detached = Rc::new(Cell::new(false));
    let marked = detached.clone();
    tokio::task::spawn_local(async move {
        while let Some(line) = out_rx.recv().await {
            if matches!(line, ClientLine::Detach) {
                marked.set(true);
            }
            if stdin.write_all(to_line(&line).as_bytes()).await.is_err() || stdin.write_all(b"\n").await.is_err() {
                return;
            }
        }
    });
    Ok((Conn { child, lines, out, detached, backlog: VecDeque::new() }, info))
}

enum End {
    /// The UI is done, or the agent ended, or the client left: nothing to reconnect.
    Over,
    Lost,
}

struct Wires {
    host: Rc<RemoteHost>,
    adapter: ClientAdapter,
    replies: UnboundedReceiver<WireCommand>,
    commands: UnboundedReceiver<Command>,
    events: UnboundedSender<AgentEvent>,
}

async fn next_line(backlog: &mut VecDeque<AgentLine>, lines: &mut UnboundedReceiver<AgentLine>) -> Option<AgentLine> {
    match backlog.pop_front() {
        Some(l) => Some(l),
        None => lines.recv().await,
    }
}

/// Serves one connection until it ends.
async fn session(conn: &mut Conn, w: &mut Wires) -> End {
    let _ = w.events.send(AgentEvent::Replay(true));
    loop {
        tokio::select! {
            line = next_line(&mut conn.backlog, &mut conn.lines) => match line {
                None => return End::Lost,
                Some(AgentLine::Event(wire)) => match w.adapter.incoming(wire) {
                    Incoming::Event(e) => {
                        let exit = matches!(e, AgentEvent::Exit);
                        let _ = w.events.send(e);
                        if exit {
                            return End::Over;
                        }
                    }
                    Incoming::CaughtUp => {
                        let _ = w.events.send(AgentEvent::Replay(false));
                    }
                    Incoming::Detached(reason) => {
                        let _ = w.events.send(AgentEvent::Notice(reason));
                        let _ = w.events.send(AgentEvent::Exit);
                        return End::Over;
                    }
                    // A request dropped by `Pending` closes its dialog once the aborted task has run.
                    Incoming::Skip => tokio::task::yield_now().await,
                },
                Some(AgentLine::HostReply { id, result }) => w.host.reply(id, result),
                Some(AgentLine::Pushed { name, data }) => w.host.push(name, data),
                Some(AgentLine::TunnelLine { id, line }) => w.host.tunnel_line(id, line),
                Some(AgentLine::TunnelClose { id, error }) => w.host.tunnel_closed(id, error),
                Some(AgentLine::Error(e)) => {
                    let _ = w.events.send(AgentEvent::Error(e));
                }
                Some(AgentLine::Hello { .. } | AgentLine::Busy { .. }) => {}
            },
            c = w.commands.recv() => match c {
                None => return End::Over,
                Some(c) => {
                    if conn.out.send(ClientLine::Command(w.adapter.command(c))).is_err() {
                        return End::Lost;
                    }
                }
            },
            Some(r) = w.replies.recv() => {
                if conn.out.send(ClientLine::Command(r)).is_err() {
                    return End::Lost;
                }
            }
            _ = conn.child.wait() => {
                // What the command wrote before it ended is still queued: let it be read first.
                while let Ok(line) = conn.lines.try_recv() {
                    if let AgentLine::Event(wire) = line
                        && let Incoming::Event(e) = w.adapter.incoming(wire)
                    {
                        let _ = w.events.send(e);
                    }
                }
                return End::Lost;
            }
        }
    }
}

/// Host calls made before the session loop runs: the agent's catch-up that arrives meanwhile is kept
/// in `backlog` for it.
struct HostCalls<'a> {
    conn: &'a mut Conn,
    next: u64,
}

impl Calls for HostCalls<'_> {
    async fn call(&mut self, name: &str, args: serde_json::Value) -> Result<serde_json::Value, String> {
        // Far from the ids the host hands out, so a late reply cannot be taken for one of its own.
        self.next += 1;
        let id = u64::MAX - self.next;
        let line = ClientLine::Host { id, call: name.to_string(), args };
        self.conn.out.send(line).map_err(|_| "the connection to the agent is closed".to_string())?;
        loop {
            match tokio::time::timeout(HELLO_WAIT, self.conn.lines.recv()).await {
                Err(_) => return Err(format!("the agent did not answer {name} within {}s", HELLO_WAIT.as_secs())),
                Ok(None) => return Err("the agent closed the connection".to_string()),
                Ok(Some(AgentLine::HostReply { id: reply, result })) if reply == id => return result,
                Ok(Some(other)) => self.conn.backlog.push_back(other),
            }
        }
    }
}

/// Before the UI draws: the plain terminal.
struct PlainTerminal;

impl Terminal for PlainTerminal {
    fn interactive(&self) -> bool {
        std::io::IsTerminal::is_terminal(&std::io::stdin())
    }

    fn say(&mut self, text: &str) {
        eprintln!("mu: {text}");
    }

    fn confirm(&mut self, question: &str) -> bool {
        eprint!("{question}");
        read_line(true).is_some_and(|a| matches!(a.trim(), "y" | "Y" | "yes"))
    }

    fn secret(&mut self, prompt: &str) -> Option<zeroize::Zeroizing<String>> {
        eprint!("{prompt}");
        read_line(false)
    }
}

/// With the UI up nothing can be asked: what happens is shown as a notice.
struct NoticeTerminal(UnboundedSender<String>);

impl Terminal for NoticeTerminal {
    fn interactive(&self) -> bool {
        false
    }

    fn say(&mut self, text: &str) {
        let _ = self.0.send(text.to_string());
    }

    fn confirm(&mut self, _: &str) -> bool {
        false
    }

    fn secret(&mut self, _: &str) -> Option<zeroize::Zeroizing<String>> {
        None
    }
}

/// A line from the terminal, without echo when `echo` is false (then only a terminal will do). `None`
/// at end of input, on Ctrl-C, or when a secret is wanted and stdin is no terminal.
fn read_line(echo: bool) -> Option<zeroize::Zeroizing<String>> {
    let mut bytes = zeroize::Zeroizing::new(Vec::new());
    // SAFETY: termios is plain data, and fd 0 is stdin.
    unsafe {
        let mut saved: libc::termios = std::mem::zeroed();
        let tty = libc::tcgetattr(0, &mut saved) == 0;
        if !tty && !echo {
            return None;
        }
        if tty && !echo {
            let mut quiet = saved;
            // ISIG off, so that Ctrl-C arrives as a byte and the terminal is put back before leaving.
            quiet.c_lflag &= !(libc::ECHO | libc::ISIG);
            libc::tcsetattr(0, libc::TCSANOW, &quiet);
        }
        let done = loop {
            let mut b = 0u8;
            match libc::read(0, (&mut b as *mut u8).cast(), 1) {
                1 if b == b'\n' || b == b'\r' => break true,
                1 if b == 3 || b == 4 => break false,
                1 => bytes.push(b),
                n if n < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted => {}
                _ => break !bytes.is_empty(),
            }
        };
        if tty && !echo {
            libc::tcsetattr(0, libc::TCSANOW, &saved);
            eprintln!();
        }
        if !done {
            return None;
        }
    }
    Some(zeroize::Zeroizing::new(String::from_utf8_lossy(&bytes).into_owned()))
}

/// Unlocks the agent's state, or sets it up, as far as `term` can ask. The agent stays as it is when that
/// cannot be done: the user was told why, and a turn that needs a credential then fails as locked.
async fn settle_state(conn: &mut Conn, link: &Link, term: &mut impl Terminal) {
    let _ = unlock::settle(&mut HostCalls { conn, next: 0 }, Some(&link.state_key), term).await;
}

const TAKE_IT: &str = "Take it over";

/// Asks in the UI whether to take the session from the client that has it. `false` when the user
/// quits, or the UI is gone.
async fn ask_take_over(w: &mut Wires, host: &str, since: i64) -> bool {
    let (reply, mut answer) = tokio::sync::oneshot::channel();
    let option =
        |label: &str| mu_base::events::AskOption { label: label.into(), description: None, preview: None, kind: None };
    let question = AskQuestion {
        question: format!("{host} has this session since {}: take it over, or quit?", since_text(since)),
        context: None,
        header: None,
        options: vec![option(TAKE_IT), option("Quit")],
        multi: false,
        recommended: Some(1),
        confirm: true,
        skippable: false,
    };
    if w.events.send(AgentEvent::Ask { questions: vec![question], reply }).is_err() {
        return false;
    }
    loop {
        tokio::select! {
            a = &mut answer => {
                return match a {
                    Ok(Some(answers)) => matches!(
                        answers.first(),
                        Some(Answer::Options(p) | Answer::Noted { picks: p, .. }) if p.first().is_some_and(|l| l == TAKE_IT)
                    ),
                    _ => false,
                };
            }
            c = w.commands.recv() => match c {
                None | Some(Command::Exit) => return false,
                Some(_) => {
                    let _ = w.events.send(AgentEvent::Notice("answer the question above first".into()));
                }
            },
        }
    }
}

/// Keeps the UI connected: the first connection is made already, later ones after a pause. A `Busy`
/// answer ends the retrying until the user decides.
async fn drive(link: Link, who: Who, first: Conn, mut w: Wires, notices: UnboundedSender<String>) {
    let mut next = Some(first);
    let mut pause = FIRST_PAUSE;
    let mut take = false;
    loop {
        let mut conn = match next.take() {
            Some(c) => c,
            None => loop {
                if !pause.is_zero() {
                    let _ = w.events.send(AgentEvent::Notice(format!(
                        "connection lost; reconnecting in {}s (Ctrl-C quits)",
                        pause.as_secs()
                    )));
                }
                let sleep = tokio::time::sleep(pause);
                tokio::pin!(sleep);
                loop {
                    tokio::select! {
                        () = &mut sleep => break,
                        c = w.commands.recv() => match c {
                            None | Some(Command::Exit) => {
                                let _ = w.events.send(AgentEvent::Exit);
                                return;
                            }
                            Some(_) => {
                                let _ = w.events.send(AgentEvent::Notice("not connected: waiting to reconnect".into()));
                            }
                        },
                    }
                }
                match connect(&link, &notices, &who, Join { reconnect: true, take }).await {
                    Ok((mut c, _)) => {
                        settle_state(&mut c, &link, &mut NoticeTerminal(notices.clone())).await;
                        pause = FIRST_PAUSE;
                        take = false;
                        break c;
                    }
                    Err(Refused::Busy { host, since }) => {
                        if !ask_take_over(&mut w, &host, since).await {
                            let _ = w.events.send(AgentEvent::Exit);
                            return;
                        }
                        take = true;
                        pause = Duration::ZERO;
                    }
                    Err(Refused::Failed(e)) => {
                        let _ = notices.send(e);
                        pause = (pause * 2).min(LONGEST_PAUSE);
                    }
                }
            },
        };
        w.host.connect(conn.out.clone());
        let end = session(&mut conn, &mut w).await;
        w.host.disconnect();
        match end {
            End::Over => return,
            End::Lost if conn.detached.get() => {
                let _ = w.events.send(AgentEvent::Exit);
                return;
            }
            End::Lost => {}
        }
    }
}

/// Before the UI is up: says who has the session and reads one key. `true` for `t`.
fn ask_key(host: &str, since: i64) -> Result<bool> {
    eprint!("{host} has this session since {}: [t]ake it over, [q]uit ", since_text(since));
    let key = read_key()?;
    eprintln!();
    Ok(key == b't')
}

/// One byte from the terminal, without waiting for Enter; `q` for end of input and Ctrl-C.
fn read_key() -> Result<u8> {
    // SAFETY: termios is plain data, and fd 0 is stdin.
    unsafe {
        let mut saved: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(0, &mut saved) != 0 {
            bail!("mu: the session is taken: stdin is not a terminal to ask on");
        }
        let mut raw = saved;
        raw.c_lflag &= !(libc::ICANON | libc::ECHO);
        raw.c_cc[libc::VMIN] = 1;
        raw.c_cc[libc::VTIME] = 0;
        libc::tcsetattr(0, libc::TCSANOW, &raw);
        let key = loop {
            let mut b = 0u8;
            match libc::read(0, (&mut b as *mut u8).cast(), 1) {
                1 if matches!(b, b't' | b'T') => break b't',
                1 if matches!(b, b'q' | b'Q' | 3) => break b'q',
                1 => {}
                n if n < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted => {}
                _ => break b'q',
            }
        };
        libc::tcsetattr(0, libc::TCSANOW, &saved);
        Ok(key)
    }
}

/// Runs the UI over `link`; `pick_session` opens the session picker (of the agent's sessions) first.
pub fn run(link: Link, pick_session: bool) -> Result<i32> {
    if !std::io::IsTerminal::is_terminal(&std::io::stdout()) {
        bail!("mu: interactive mode needs a terminal; use -p");
    }
    let cfgs = Configs::load();
    let rt = mu_base::runtime::current_thread_runtime()?;
    let local = tokio::task::LocalSet::new();
    let who = Who { host: hostname(), client: uuid::Uuid::new_v4().to_string() };
    local.block_on(&rt, async move {
        let (notice_tx, notice_rx) = unbounded_channel::<String>();
        let mut join = Join { reconnect: false, take: false };
        let (first, info) = loop {
            match connect(&link, &notice_tx, &who, join).await {
                Ok(c) => break c,
                Err(Refused::Failed(e)) => bail!("{e}"),
                Err(Refused::Busy { host, since }) => {
                    if join.take || !ask_key(&host, since)? {
                        return Ok(0);
                    }
                    join.take = true;
                }
            }
        };
        let mut first = first;
        settle_state(&mut first, &link, &mut PlainTerminal).await;
        let host = RemoteHost::new(info.clone());
        // The UI asks the host for start-up facts at once; the driver would connect it too late.
        host.connect(first.out.clone());
        let (cmd_tx, commands) = unbounded_channel();
        let (events, ev_rx) = unbounded_channel();
        let (replies_tx, replies) = unbounded_channel();
        let wires = Wires { host: host.clone(), adapter: ClientAdapter::new(replies_tx), replies, commands, events };
        tokio::task::spawn_local(drive(link, who, first, wires, notice_tx));
        let host: Rc<dyn Host> = host;
        Ok(mu_tui::app::run(mu_tui::app::AppInit {
            host,
            cmd_tx,
            ev_rx,
            notice_rx,
            cwd: info.cwd,
            pick_session,
            status_cfg: cfgs.omp.status_line.clone(),
            theme: cfgs.mu.theme.clone(),
            notify: cfgs.omp.completion.notify,
            bell: cfgs.mu.bell,
            hide_thinking: cfgs.omp.hide_thinking_block,
            smooth_streaming: cfgs.omp.display.smooth_streaming,
            status_levels: cfgs.mu.status,
        })
        .await)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use mu_base::wire::from_line;

    #[test]
    fn lines_of_either_direction_round_trip_through_the_envelope() {
        let line = to_line(&ClientLine::Detach);
        assert!(matches!(from_line::<ClientLine>(&line), Ok(ClientLine::Detach)));
    }
}
