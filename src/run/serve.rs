//! `mu agent`: the agent with no terminal, serving one client at a time over a Unix socket.
//!
//! The socket sits beside the session's owner record (`<profile home>/run/<session id>.sock`, mode
//! 0600) and there is no network listener: reaching a remote agent is `ssh host mu attach` or
//! `kubectl exec -i pod -- mu attach`. A connection starts with one hello line each way; then the client's
//! commands and host requests come in, and the agent's events, host replies and pushed values go out.
//! The agent keeps running when the client goes away (`mu_base::catchup::Catchup` keeps what a client
//! that attaches later needs) and ends only on `Command::Exit` or a signal.

use mu_base::catchup::Catchup;
use mu_base::events::{AgentEvent, Command};
use mu_base::wire::{AgentLine, ClientLine, WIRE, WireEvent, event_line, to_line};
use mu_common::paths;
use mu_common::rpc::{Framed, Line};
use mu_session::session::format;
use mu_tui::host::Host;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::sync::oneshot;

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
}

/// What a connection's tasks tell the loop.
enum Inbox {
    /// A client said hello: its writer, ready for lines, and the signal that the writer has finished.
    Joined {
        id: u64,
        writer: UnboundedSender<String>,
        written: oneshot::Receiver<()>,
    },
    Line(u64, ClientLine),
    Closed(u64),
}

struct Client {
    id: u64,
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
    if let Some(dir) = mu_session::session::list::header_cwd(&path).filter(|d| d.is_dir()) {
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
        std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        // A stale file from a process that died: nobody holds the session's lock, so it is ours.
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).map_err(|e| format!("{}: {e}", path.display()))?;
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

    fn join(&mut self, id: u64, writer: UnboundedSender<String>, written: oneshot::Receiver<()>) {
        if let Some(old) = self.client.take() {
            self.tunnels.clear();
            mu_tui::host::program::kill_all();
            let _ = old.writer.send(to_line(&AgentLine::Event(WireEvent::Detached {
                reason: "another client took this session".into(),
            })));
        }
        let hello = AgentLine::Hello {
            wire: WIRE,
            version: env!("MU_VERSION").into(),
            host: serde_json::to_value(self.host.info()).unwrap_or_default(),
        };
        let _ = writer.send(to_line(&hello));
        for line in self.catchup.catch_up(self.history()) {
            let _ = writer.send(line);
        }
        self.client = Some(Client { id, writer, written });
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
                        let writer = client.writer.clone();
                        tokio::task::spawn_local(async move {
                            let _ = writer.send(reply(fut.await));
                        });
                    }
                }
            }
        }
    }
}

/// One accepted connection: the hello exchange, then lines to the loop; writes come from a channel.
async fn connection(id: u64, stream: UnixStream, inbox: UnboundedSender<Inbox>) {
    let (read, mut write) = stream.into_split();
    let mut lines = Framed::new(BufReader::new(read));
    let hello = tokio::time::timeout(std::time::Duration::from_secs(10), lines.next_line()).await;
    let Ok(Some(Line::Value(v))) = hello else { return };
    match serde_json::from_value::<ClientLine>(v) {
        Ok(ClientLine::Hello { wire, .. }) if wire == WIRE => {}
        Ok(ClientLine::Hello { wire, .. }) => {
            let refusal = AgentLine::Error(format!("this agent speaks wire {WIRE}, the client wire {wire}"));
            let _ = write.write_all(format!("{}\n", to_line(&refusal)).as_bytes()).await;
            return;
        }
        _ => return,
    }
    let (writer, mut out) = unbounded_channel::<String>();
    let (done, written) = oneshot::channel();
    if inbox.send(Inbox::Joined { id, writer, written }).is_err() {
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
    while let Some(l) = lines.next_line().await {
        let Line::Value(v) = l else { continue };
        match serde_json::from_value::<ClientLine>(v) {
            Ok(line) => {
                if inbox.send(Inbox::Line(id, line)).is_err() {
                    return;
                }
            }
            Err(e) => eprintln!("mu agent: unreadable line from the client: {e}"),
        }
    }
    let _ = inbox.send(Inbox::Closed(id));
}

pub async fn run(init: Init) -> i32 {
    let Init { host, cmd, mut ev, mut notices, resumed } = init;
    let mut server = Server {
        host,
        cmd,
        catchup: Catchup::new(),
        session: None,
        socket: None,
        listener: None,
        client: None,
        tunnels: Default::default(),
    };
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
    println!("{}", serde_json::json!({"session": mu_session::session::list::id_of(&session), "socket": socket}));

    let mut pushed = server.host.pushed();
    let (inbox_tx, mut inbox) = unbounded_channel::<Inbox>();
    let mut next_id = 0u64;
    let mut asked_to_stop = false;
    loop {
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
                Inbox::Joined { id, writer, written } => server.join(id, writer, written),
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
            Some(n) = notices.recv() => eprintln!("mu agent: {n}"),
            _ = term.recv() => stop(&mut asked_to_stop, &server.cmd, 143),
            _ = hup.recv() => stop(&mut asked_to_stop, &server.cmd, 129),
            _ = int.recv() => stop(&mut asked_to_stop, &server.cmd, 130),
        }
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
        std::process::exit(code);
    }
    *asked = true;
    let _ = cmd.send(Command::Exit);
}
