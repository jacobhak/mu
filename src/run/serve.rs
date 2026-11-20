//! `mu agent`: the agent with no terminal, serving one client at a time over a Unix socket.
//!
//! The socket sits beside the session's owner record (`<profile home>/run/<session id>.sock`, mode
//! 0600) and there is no network listener: reaching a remote agent is `ssh host mu attach --stdio ID` or
//! `kubectl exec -i pod -- mu attach --stdio ID`, a raw pipe to this socket. Every line is plain JSON,
//! so the stream is as private as the pipe that carries it (see docs/remote.md). A connection starts with
//! the client's hello (the wire number, so a mismatch gets a readable answer, and who the client is) and
//! the agent's hello back (or `Busy`, when another client has the session); then the client's
//! commands and host requests come in, and the agent's events, host replies and pushed values go out.
//! The agent keeps running when the client goes away (`mu_base::catchup::Catchup` keeps what a client
//! that attaches later needs) and ends only on `Command::Exit` or a signal.

use mu_base::catchup::{Catchup, Waiting};
use mu_base::config::MuAgent;
use mu_base::events::{AgentEvent, Command};
use mu_base::permissions::ApprovalDecision;
use mu_base::wire::{AgentLine, ClientLine, WIRE, WireCommand, WireEvent, event_line, to_line};
use mu_common::paths;
use mu_common::rpc::{Framed, Line};
use mu_session::session::format;
use mu_session::session::owner::{self, ClientInfo};
use mu_tui::host::Host;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::sync::oneshot;

/// Set for an agent started by `mu agent --detach`.
pub static DETACHED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// `sun_path` holds 104 bytes on macOS (108 on Linux), the terminating NUL included.
const MAX_SOCKET_PATH: usize = 100;

pub struct Init {
    pub host: Rc<dyn Host>,
    pub cmd: UnboundedSender<Command>,
    pub ev: UnboundedReceiver<AgentEvent>,
    /// Plugin and start-up notices; nobody reads them, so they go to stderr.
    pub notices: UnboundedReceiver<String>,
    /// The session is resumed, so it exists already.
    pub resumed: bool,
    pub config: MuAgent,
    /// The model selector that could not be resolved while the state was locked: set once it is open.
    pub deferred_model: Option<String>,
}

/// Who said hello (see `ClientLine::Hello`).
struct Hello {
    client: String,
    host: String,
    take: bool,
}

/// What a connection's tasks tell the loop.
enum Inbox {
    /// A client said hello: its writer, ready for lines, and the signal that the writer has finished.
    Joined {
        id: u64,
        hello: Hello,
        writer: UnboundedSender<String>,
        written: oneshot::Receiver<()>,
    },
    Line(u64, ClientLine),
    Closed(u64),
}

struct Client {
    id: u64,
    /// The client's own id, the same across its connections.
    who: String,
    info: ClientInfo,
    writer: UnboundedSender<String>,
    written: oneshot::Receiver<()>,
}

struct Server {
    host: Rc<dyn Host>,
    cmd: UnboundedSender<Command>,
    catchup: Catchup,
    session: Option<PathBuf>,
    socket: Option<PathBuf>,
    listener: Option<UnixListener>,
    client: Option<Client>,
    /// Open tunnels, by id: where the client's lines for each go.
    tunnels: std::collections::HashMap<u64, UnboundedSender<String>>,
    config: MuAgent,
    /// The requests that wait with no client attached, and since when.
    unattended: std::collections::HashMap<u64, std::time::Instant>,
    flagged: bool,
    /// The profile has a state key, so the agent is locked until something unlocks it.
    sealed: bool,
    locked: bool,
    deferred_model: Option<String>,
    /// Replies of calls that finished later, by the id of the connection that asked. They go through the loop so
    /// that everything the call pushed before it ended is sent first.
    replies: UnboundedSender<(u64, String)>,
}

/// `{path}`: takes the session over for a client that has no terminal here: a `mu -p` run holding it is
/// stopped after its current step, then `mu agent --resume` starts on it, and its session id comes back
/// once it listens. That agent is an ordinary one: it runs until its client quits it or it is signalled.
async fn take(args: serde_json::Value) -> Result<serde_json::Value, String> {
    use tokio::io::AsyncBufReadExt;
    let path = args["path"].as_str().map(PathBuf::from).ok_or("take needs a session path")?;
    let quiet = path.clone();
    tokio::task::spawn_blocking(move || crate::workflow::take::release_quiet(&quiet))
        .await
        .map_err(|e| e.to_string())??;
    let exe = paths::self_exe().map_err(|e| format!("cannot locate mu: {e}"))?;
    let mut cmd = tokio::process::Command::new(exe);
    cmd.arg("agent").arg("--resume").arg(&path).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped());
    let probe = path.clone();
    let worktree = tokio::task::spawn_blocking(move || mu_session::session::mirror::checkout(&probe))
        .await
        .map_err(|e| e.to_string())??;
    if let Some(dir) = worktree.or_else(|| mu_session::session::list::header_cwd(&path).filter(|d| d.is_dir())) {
        cmd.current_dir(dir);
    }
    cmd.process_group(0);
    let mut child = cmd.spawn().map_err(|e| format!("cannot start the agent: {e}"))?;
    let stdout = child.stdout.take().ok_or("the agent has no stdout")?;
    let mut lines = BufReader::new(stdout).lines();
    let first = tokio::time::timeout(std::time::Duration::from_secs(60), lines.next_line()).await;
    let line = match first {
        Ok(Ok(Some(l))) => l,
        _ => return Err("the agent did not start; see its log".into()),
    };
    // Its pipe and its exit status belong to it until it ends.
    tokio::task::spawn_local(async move {
        let _keep = lines;
        let _ = child.wait().await;
    });
    let v: serde_json::Value =
        serde_json::from_str(&line).map_err(|e| format!("unreadable start line {line:?}: {e}"))?;
    Ok(serde_json::json!({ "session": v["session"] }))
}

pub fn socket_path(session: &Path) -> Result<PathBuf, String> {
    let id = mu_session::session::list::id_of(session);
    let path = paths::profile_home().join("run").join(format!("{id}.sock"));
    if path.as_os_str().len() > MAX_SOCKET_PATH {
        return Err(format!(
            "the socket path is too long for a Unix socket ({MAX_SOCKET_PATH} bytes at most): {}",
            path.display()
        ));
    }
    Ok(path)
}

impl Server {
    /// Binds the socket of `session` and drops the one of the session it replaces.
    fn bind(&mut self, session: &Path) -> Result<(), String> {
        let path = socket_path(session)?;
        if self.socket.as_deref() == Some(path.as_path()) {
            return Ok(());
        }
        let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
        mu_common::fs::create_private_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        // A stale file from a process that died: nobody holds the session's lock, so it is ours.
        let _ = std::fs::remove_file(&path);
        // The socket is born 0600 so it is never reachable between bind and chmod.
        let listener = mu_common::fs::with_umask(0o077, || UnixListener::bind(&path))
            .map_err(|e| format!("{}: {e}", path.display()))?;
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                .map_err(|e| format!("{}: {e}", path.display()))?;
        }
        if let Some(old) = self.socket.replace(path) {
            let _ = std::fs::remove_file(old);
        }
        self.listener = Some(listener);
        self.session = Some(session.to_path_buf());
        Ok(())
    }

    /// The client left: what it started for itself goes with it (programs on a terminal, tunnels).
    fn client_gone(&mut self) {
        self.client = None;
        self.tunnels.clear();
        mu_tui::host::program::kill_all();
        owner::set_attention(|a| a.client = None);
    }

    /// Keeps what waits with no client in view: the owner record's `waiting`, the notify command (once
    /// per request) and the clock the approval timeout runs on. A locked agent is `locked` and `waiting`
    /// too, and runs the notify command once when it becomes so.
    fn attend(&mut self) {
        let locked = self.sealed && !mu_common::sealed::is_unlocked();
        if locked != self.locked {
            self.locked = locked;
            owner::set_attention(|a| a.locked = locked);
            if locked {
                self.notify_summary("locked: attach to unlock");
            } else if let Some(model) = self.deferred_model.take() {
                let _ = self.cmd.send(Command::SetModel(model));
            }
        }
        let waiting = self.catchup.waiting();
        self.unattended.retain(|id, _| waiting.iter().any(|w| w.id == *id));
        if self.client.is_some() {
            self.unattended.clear();
        } else {
            for w in &waiting {
                let known = self.unattended.len();
                self.unattended.entry(w.id).or_insert_with(std::time::Instant::now);
                if self.unattended.len() > known {
                    self.notify(w);
                }
            }
        }
        let flag = !self.unattended.is_empty() || self.locked;
        if flag != self.flagged {
            self.flagged = flag;
            owner::set_attention(|a| a.waiting = flag);
        }
    }

    fn notify(&self, w: &Waiting) {
        let what = if w.approval { "approval" } else { "question" };
        self.notify_summary(&format!("{what}: {}", w.summary));
    }

    fn notify_summary(&self, summary: &str) {
        let (Some(command), Some(session)) = (&self.config.notify_command, &self.session) else { return };
        let summary = summary.replace('\n', " ");
        let child = tokio::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(command)
            .arg("mu-notify")
            .arg(mu_session::session::list::id_of(session))
            .arg(summary)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        match child {
            Ok(mut c) => {
                tokio::task::spawn_local(async move {
                    let _ = c.wait().await;
                });
            }
            Err(e) => eprintln!("mu agent: cannot run notify_command: {e}"),
        }
    }

    fn timeout(&self) -> Option<std::time::Duration> {
        (self.config.approval_timeout > 0).then(|| std::time::Duration::from_secs(self.config.approval_timeout))
    }

    /// When the first unattended request runs out of time.
    fn next_expiry(&self) -> Option<tokio::time::Instant> {
        let t = self.timeout()?;
        self.unattended.values().min().map(|s| (*s + t).into())
    }

    /// Answers the requests nobody took up in time: an approval as unavailable, a question as cancelled.
    fn expire(&mut self) {
        let Some(limit) = self.timeout() else { return };
        for w in self.catchup.waiting() {
            if !self.unattended.get(&w.id).is_some_and(|s| s.elapsed() >= limit) {
                continue;
            }
            let answer = if w.approval {
                WireCommand::Approval { id: w.id, decision: ApprovalDecision::Unavailable }
            } else {
                WireCommand::Answer { id: w.id, answers: None }
            };
            self.catchup.command(answer);
            let note = format!(
                "no client answered the {} within {}s: {}",
                if w.approval { "approval" } else { "question" },
                limit.as_secs(),
                w.summary
            );
            eprintln!("mu agent: {note}");
            self.catchup.event(AgentEvent::Notice(note));
        }
    }

    /// A tunnel to the agent of `session`, which runs on this host: its lines go both ways untouched.
    fn open_tunnel(&mut self, id: u64, session: &str) {
        let Some(client) = &self.client else { return };
        let out = client.writer.clone();
        let closed = move |error: Option<String>| {
            let _ = out.send(to_line(&AgentLine::TunnelClose { id, error }));
        };
        if session.is_empty() || !session.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            return closed(Some(format!("`{session}` is not a session id")));
        }
        let socket = paths::profile_home().join("run").join(format!("{session}.sock"));
        let (tx, mut to_socket) = unbounded_channel::<String>();
        self.tunnels.insert(id, tx);
        let lines_out = client.writer.clone();
        tokio::task::spawn_local(async move {
            let stream = match UnixStream::connect(&socket).await {
                Ok(s) => s,
                Err(e) => return closed(Some(format!("cannot connect to {}: {e}", socket.display()))),
            };
            let (read, mut write) = stream.into_split();
            tokio::task::spawn_local(async move {
                while let Some(line) = to_socket.recv().await {
                    if write.write_all(line.as_bytes()).await.is_err() || write.write_all(b"\n").await.is_err() {
                        return;
                    }
                }
                let _ = write.shutdown().await;
            });
            let mut lines = BufReader::new(read).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if lines_out.send(to_line(&AgentLine::TunnelLine { id, line })).is_err() {
                    return;
                }
            }
            closed(None);
        });
    }

    fn send(&self, line: &AgentLine) {
        if let Some(c) = &self.client {
            let _ = c.writer.send(to_line(line));
        }
    }

    fn on_event(&mut self, ev: AgentEvent) -> Option<PathBuf> {
        let moved = match &ev {
            AgentEvent::Status(s) => {
                s.session_path.as_deref().map(PathBuf::from).filter(|p| Some(p) != self.session.as_ref())
            }
            _ => None,
        };
        if matches!(ev, AgentEvent::Exit) {
            return None;
        }
        // The client may edit the plan file; the host answers `read_file`/`write_file` only for paths it sent.
        if let AgentEvent::PlanReview { file, .. } = &ev {
            mu_tui::host::plans::hand_out(file);
        }
        let line = self.catchup.event(ev);
        if let Some(c) = &self.client {
            let _ = c.writer.send(line);
        }
        moved
    }

    /// The stored conversation up to where the turn in progress began, as lines.
    fn history(&self) -> Vec<String> {
        let Some(path) = &self.session else { return Vec::new() };
        let Ok(loaded) = format::load(path) else { return Vec::new() };
        let upto = match self.catchup.turn_base() {
            None => loaded.messages.len(),
            Some(None) => 0,
            Some(Some(leaf)) => loaded
                .entry_ids
                .iter()
                .position(|id| !id.is_empty() && id == leaf)
                .map_or(loaded.messages.len(), |i| i + 1),
        };
        mu_base::message::history_events(&loaded.messages[..upto])
            .into_iter()
            .map(|e| event_line(WireEvent::Event(e)))
            .collect()
    }

    fn join(&mut self, id: u64, hello: Hello, writer: UnboundedSender<String>, written: oneshot::Receiver<()>) {
        if let Some(old) = self.client.take() {
            let same = !old.who.is_empty() && old.who == hello.client;
            if !same && !hello.take {
                let _ = writer.send(to_line(&AgentLine::Busy { host: old.info.host.clone(), since: old.info.since }));
                self.client = Some(old);
                return;
            }
            self.tunnels.clear();
            mu_tui::host::program::kill_all();
            let _ = old.writer.send(to_line(&AgentLine::Event(WireEvent::Detached {
                reason: "another client took this session".into(),
            })));
        }
        let info = ClientInfo { host: hello.host, since: jiff::Timestamp::now().as_second() };
        owner::set_attention(|a| a.client = Some(info.clone()));
        let greeting = AgentLine::Hello {
            wire: WIRE,
            version: env!("MU_VERSION").into(),
            host: serde_json::to_value(self.host.info()).unwrap_or_default(),
        };
        let _ = writer.send(to_line(&greeting));
        for line in self.catchup.catch_up(self.history()) {
            let _ = writer.send(line);
        }
        self.client = Some(Client { id, who: hello.client, info, writer, written });
    }

    fn on_line(&mut self, id: u64, line: ClientLine) {
        if self.client.as_ref().map(|c| c.id) != Some(id) {
            return;
        }
        match line {
            ClientLine::Hello { .. } => {}
            ClientLine::Command(c) => {
                if let Some(c) = self.catchup.command(c) {
                    let _ = self.cmd.send(c);
                }
            }
            ClientLine::Detach => self.client_gone(),
            ClientLine::Host { id, call, args } if call == "take" => {
                let Some(client) = &self.client else { return };
                let writer = client.writer.clone();
                tokio::task::spawn_local(async move {
                    let result = take(args).await;
                    let _ = writer.send(to_line(&AgentLine::HostReply { id, result }));
                });
            }
            ClientLine::TunnelOpen { id, session } => self.open_tunnel(id, &session),
            ClientLine::TunnelLine { id, line } => {
                if let Some(tx) = self.tunnels.get(&id) {
                    let _ = tx.send(line);
                }
            }
            ClientLine::TunnelClose { id } => {
                self.tunnels.remove(&id);
            }
            ClientLine::Host { id, call, args } => {
                let Some(client) = &self.client else { return };
                let reply = move |result| to_line(&AgentLine::HostReply { id, result });
                match self.host.call(&call, args) {
                    mu_tui::host::Answer::Ready(r) => {
                        let _ = client.writer.send(reply(r));
                    }
                    mu_tui::host::Answer::Pending(fut) => {
                        let (replies, client_id) = (self.replies.clone(), client.id);
                        tokio::task::spawn_local(async move {
                            let _ = replies.send((client_id, reply(fut.await)));
                        });
                    }
                }
            }
        }
    }
}

/// One accepted connection. The client's hello gets a readable answer when the wire numbers differ;
/// otherwise it says who the client is. Lines go to the loop; writes come from a channel.
async fn connection(id: u64, stream: UnixStream, inbox: UnboundedSender<Inbox>) {
    let (read, mut write) = stream.into_split();
    let mut lines = Framed::new(BufReader::new(read));
    let limit = std::time::Duration::from_secs(10);
    let hello = tokio::time::timeout(limit, lines.next_line()).await;
    let Ok(Some(Line::Value(v))) = hello else { return };
    let hello = match serde_json::from_value::<ClientLine>(v) {
        Ok(ClientLine::Hello { wire, host, client, take, .. }) if wire == WIRE => Hello { client, host, take },
        Ok(ClientLine::Hello { wire, .. }) => {
            let refusal = AgentLine::Error(format!("this agent speaks wire {WIRE}, the client wire {wire}"));
            let _ = write.write_all(format!("{}\n", to_line(&refusal)).as_bytes()).await;
            // Closing with the client's next bytes unread would reset the connection and could drop the answer.
            let _ = write.shutdown().await;
            let _ = tokio::time::timeout(std::time::Duration::from_secs(2), async {
                while lines.next_line().await.is_some() {}
            })
            .await;
            return;
        }
        _ => return,
    };
    let (writer, mut out) = unbounded_channel::<String>();
    let (done, written) = oneshot::channel();
    if inbox.send(Inbox::Joined { id, hello, writer, written }).is_err() {
        return;
    }
    tokio::task::spawn_local(async move {
        while let Some(line) = out.recv().await {
            if write.write_all(line.as_bytes()).await.is_err() || write.write_all(b"\n").await.is_err() {
                break;
            }
        }
        let _ = write.shutdown().await;
        let _ = done.send(());
    });
    while let Some(next) = lines.next_line().await {
        match next {
            Line::Value(v) => match serde_json::from_value::<ClientLine>(v) {
                Ok(line) => {
                    if inbox.send(Inbox::Line(id, line)).is_err() {
                        return;
                    }
                }
                Err(e) => eprintln!("mu agent: unreadable line from the client: {e}"),
            },
            Line::Bad(e) => eprintln!("mu agent: unreadable line from the client: {e}"),
        }
    }
    let _ = inbox.send(Inbox::Closed(id));
}

pub async fn run(init: Init) -> i32 {
    let Init { host, cmd, mut ev, mut notices, resumed, config, deferred_model } = init;
    let (replies_tx, mut replies) = unbounded_channel::<(u64, String)>();
    let mut server = Server {
        host,
        cmd,
        catchup: Catchup::new(),
        session: None,
        socket: None,
        listener: None,
        client: None,
        tunnels: Default::default(),
        config,
        unattended: Default::default(),
        flagged: false,
        sealed: false,
        locked: false,
        deferred_model,
        replies: replies_tx,
    };
    server.sealed = mu_common::sealed::is_initialised(&paths::profile_home());
    // Before the socket exists: whoever reads the first line may signal at once.
    let (mut term, mut hup, mut int) = {
        use tokio::signal::unix::{SignalKind, signal};
        match (signal(SignalKind::terminate()), signal(SignalKind::hangup()), signal(SignalKind::interrupt())) {
            (Ok(t), Ok(h), Ok(i)) => (t, h, i),
            _ => {
                eprintln!("mu agent: cannot install the signal handlers");
                let _ = server.cmd.send(Command::Exit);
                return 1;
            }
        }
    };
    if !resumed {
        let _ = server.cmd.send(Command::EnsureSession);
    }
    // The first status that names a session file: its id is the socket's.
    loop {
        match ev.recv().await {
            Some(AgentEvent::Exit) | None => return 1,
            Some(AgentEvent::Error(m)) if m.starts_with(mu_base::events::SESSION_CREATE_FAILED) => {
                eprintln!("mu agent: {m}");
                let _ = server.cmd.send(Command::Exit);
                return 1;
            }
            Some(e) => {
                if let Some(path) = server.on_event(e) {
                    if let Err(e) = server.bind(&path) {
                        eprintln!("mu agent: {e}");
                        let _ = server.cmd.send(Command::Exit);
                        return 1;
                    }
                    break;
                }
            }
        }
    }
    let (Some(session), Some(socket)) = (server.session.clone(), server.socket.clone()) else { return 1 };
    server.attend();
    println!("{}", serde_json::json!({"session": mu_session::session::list::id_of(&session), "socket": socket}));
    let detached = DETACHED.load(std::sync::atomic::Ordering::SeqCst);
    if detached {
        // The starter has what it needs and closes its end: later output goes to the log with stderr.
        let _ = std::io::Write::flush(&mut std::io::stdout());
        // SAFETY: both descriptors are open.
        unsafe { libc::dup2(2, 1) };
    }

    let mut pushed = server.host.pushed();
    let (inbox_tx, mut inbox) = unbounded_channel::<Inbox>();
    let mut next_id = 0u64;
    let mut asked_to_stop = false;
    loop {
        let expiry = server.next_expiry();
        tokio::select! {
            e = ev.recv() => match e {
                Some(AgentEvent::Exit) | None => break,
                Some(e) => {
                    if let Some(path) = server.on_event(e)
                        && let Err(err) = server.bind(&path)
                    {
                        eprintln!("mu agent: {err}");
                    }
                }
            },
            accepted = async {
                match &server.listener {
                    Some(l) => l.accept().await,
                    None => std::future::pending().await,
                }
            } => {
                if let Ok((stream, _)) = accepted {
                    next_id += 1;
                    tokio::task::spawn_local(connection(next_id, stream, inbox_tx.clone()));
                }
            }
            Some(m) = inbox.recv() => match m {
                Inbox::Joined { id, hello, writer, written } => server.join(id, hello, writer, written),
                Inbox::Line(id, line) => server.on_line(id, line),
                Inbox::Closed(id) => {
                    if server.client.as_ref().map(|c| c.id) == Some(id) {
                        server.client_gone();
                    }
                }
            },
            Some(p) = async { match pushed.as_mut() { Some(r) => r.recv().await, None => std::future::pending().await } } => {
                server.send(&AgentLine::Pushed { name: p.name, data: p.data });
            }
            Some((client_id, line)) = replies.recv() => {
                while let Some(p) = pushed.as_mut().and_then(|r| r.try_recv().ok()) {
                    server.send(&AgentLine::Pushed { name: p.name, data: p.data });
                }
                if let Some(c) = server.client.as_ref().filter(|c| c.id == client_id) {
                    let _ = c.writer.send(line);
                }
            }
            Some(n) = notices.recv() => eprintln!("mu agent: {n}"),
            () = async {
                match expiry {
                    Some(t) => tokio::time::sleep_until(t).await,
                    None => std::future::pending().await,
                }
            } => server.expire(),
            _ = term.recv() => stop(&mut asked_to_stop, &server.cmd, 143),
            _ = hup.recv() => {
                if !detached {
                    stop(&mut asked_to_stop, &server.cmd, 129);
                }
            }
            _ = int.recv() => stop(&mut asked_to_stop, &server.cmd, 130),
        }
        server.attend();
    }
    if let Some(socket) = &server.socket {
        let _ = std::fs::remove_file(socket);
    }
    // The client learns the agent ended: its last lines are written before the process goes.
    server.send(&AgentLine::Event(WireEvent::Event(AgentEvent::Exit)));
    if let Some(c) = server.client.take() {
        drop(c.writer);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), c.written).await;
    }
    0
}

/// The first signal ends the agent the proper way (jobs, hooks, plugins); a second means it is stuck.
fn stop(asked: &mut bool, cmd: &UnboundedSender<Command>, code: i32) {
    if *asked {
        super::state::shutdown();
        std::process::exit(code);
    }
    *asked = true;
    let _ = cmd.send(Command::Exit);
}
