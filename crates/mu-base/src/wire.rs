//! The agent <-> front end protocol as lines of JSON, for a front end that is not in the agent's process.
//!
//! In one process the two talk through `Command` and `AgentEvent`. Both are serialisable except for the
//! five events that carry an in-process reply handle; across a wire those become a request with an id on
//! the event side and a reply command on the other. `AgentAdapter` (next to the agent) and `ClientAdapter`
//! (next to the UI) convert, so neither the agent's tools nor the UI's handlers change.
//!
//! Not `mu-provider`'s `provider/wire`, which is about model APIs.

use crate::events::{ActionChoice, AgentEvent, Answer, AskQuestion, Command, PtyInput, QueueState, ReviewRequest};
use crate::image;
use crate::message::UserInput;
use crate::permissions::{ApprovalDecision, ApprovalRequest};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot};

/// Raised with every change to `WireCommand`, `WireEvent` or a host request, or to how a connection
/// starts (4: plain JSON lines behind the hello; encrypting the pipe is up to whatever carries it;
/// 5: the plan dialog became a generic document review).
pub const WIRE: u32 = 5;

/// Front end to agent.
#[derive(Serialize, Deserialize)]
pub enum WireCommand {
    Command(Command),
    Answer {
        id: u64,
        answers: Option<Vec<Answer>>,
    },
    Approval {
        id: u64,
        decision: ApprovalDecision,
    },
    WebSetupDone {
        id: u64,
        ready: bool,
    },
    Decided {
        id: u64,
        choice: ActionChoice,
    },
    /// The size a pseudo-terminal call is waiting for.
    PtySize {
        id: u64,
        cols: u16,
        rows: u16,
    },
    PtyInput {
        id: u64,
        input: PtyInput,
    },
    /// The client dropped the reply handle of request `id`: the agent drops its own, with the effect a
    /// dropped handle has in one process (a denial, a cancelled question, a tool that falls back).
    Dropped {
        id: u64,
    },
}

/// Agent to front end.
#[derive(Serialize, Deserialize)]
pub enum WireEvent {
    /// Any `AgentEvent` that carries no reply handle.
    Event(AgentEvent),
    Ask {
        id: u64,
        questions: Vec<AskQuestion>,
    },
    Approve {
        id: u64,
        request: ApprovalRequest,
    },
    WebSetup {
        id: u64,
    },
    Review {
        id: u64,
        request: ReviewRequest,
    },
    /// A bash call wants a terminal; `tool` is its call id.
    PtyOpen {
        id: u64,
        tool: String,
    },
    /// Everything the client missed has been sent: from here on events are live. Until it arrives the
    /// client does not ring the bell or notify for what it is shown.
    CaughtUp,
    /// The request ids the agent still waits on, sent in a catch-up before those requests: a dialog
    /// the client shows for any other id was answered elsewhere and is closed.
    Pending {
        ids: Vec<u64>,
    },
    /// Another client took the session, or the agent ended the connection: the client prints the
    /// reason and exits.
    Detached {
        reason: String,
    },
}

/// The lines a client sends, over the one stream that also carries the host requests.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ClientLine {
    Hello {
        wire: u32,
        version: String,
        /// The connection follows a lost one: it must not take the session from another client.
        #[serde(default)]
        reconnect: bool,
        /// The client's host name, shown to whoever else wants the session.
        #[serde(default)]
        host: String,
        /// Stands for one client across its connections: a lost connection's successor is not "another".
        #[serde(default)]
        client: String,
        /// Take the session from the client that has it, instead of being answered `Busy`.
        #[serde(default)]
        take: bool,
    },
    Command(WireCommand),
    /// A request for host state; answered by an `AgentLine::HostReply` with the same id.
    Host {
        id: u64,
        call: String,
        args: serde_json::Value,
    },
    /// Leave the agent running and close the connection.
    Detach,
    /// Opens a byte-for-byte tunnel to the agent of `session` that runs on the same host (a session taken
    /// over from a `-p` run): what the client sends as `TunnelLine` goes to that agent's socket, and what
    /// the agent writes comes back as `AgentLine::TunnelLine`. The tunnel's lines do not enter the turn log.
    TunnelOpen {
        id: u64,
        session: String,
    },
    TunnelLine {
        id: u64,
        line: String,
    },
    TunnelClose {
        id: u64,
    },
}

/// The lines the agent sends.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(clippy::large_enum_variant)] // one value per line on the wire, never stored
pub enum AgentLine {
    Hello {
        wire: u32,
        version: String,
        /// What the client needs to know about the host (`HostInfo`, in the UI crate's terms).
        host: serde_json::Value,
    },
    Event(WireEvent),
    /// Another client has the session, since `since` (unix seconds): the agent closes the connection
    /// after this line. A hello with `take` takes the session.
    Busy {
        host: String,
        since: i64,
    },
    HostReply {
        id: u64,
        result: Result<serde_json::Value, String>,
    },
    Pushed {
        name: String,
        data: serde_json::Value,
    },
    TunnelLine {
        id: u64,
        line: String,
    },
    /// The tunnel ended, from the other end or because it could not be opened (`error`).
    TunnelClose {
        id: u64,
        error: Option<String>,
    },
    /// The agent refuses the connection (a different `wire`) and closes it.
    Error(String),
}

pub fn to_line<T: Serialize>(v: &T) -> String {
    // The types are plain data with string keys: serialising them cannot fail.
    serde_json::to_string(v).unwrap_or_default()
}

/// A wire event as the agent's line for it.
pub fn event_line(w: WireEvent) -> String {
    to_line(&AgentLine::Event(w))
}

pub fn from_line<T: DeserializeOwned>(line: &str) -> Result<T, serde_json::Error> {
    serde_json::from_str(line)
}

/// A prompt crossing the wire carries its images inline: a blob reference names a file on the machine
/// that attached it.
fn inline(input: &mut UserInput) {
    for img in &mut input.images {
        if let Some(data) = image::resolve(&img.data) {
            img.data = data;
        }
    }
}

/// The receiving side puts inline images into its own blob store.
fn keep(input: &mut UserInput) {
    input.images = std::mem::take(&mut input.images).into_iter().map(image::externalize).collect();
}

fn map_inputs(inputs: &mut [UserInput], f: fn(&mut UserInput)) {
    inputs.iter_mut().for_each(f);
}

fn command_inputs(c: &mut Command, f: fn(&mut UserInput)) {
    if let Command::Prompt(i) | Command::Steer(i) | Command::FollowUp(i) = c {
        f(i);
    }
}

fn event_inputs(e: &mut AgentEvent, f: fn(&mut UserInput)) {
    match e {
        AgentEvent::Queue(QueueState { steer, follow_up }) => {
            map_inputs(steer, f);
            map_inputs(follow_up, f);
        }
        AgentEvent::QueuePopped(i) => f(i),
        AgentEvent::QueuedDropped(v) => map_inputs(v, f),
        _ => {}
    }
}

enum Parked {
    Ask(oneshot::Sender<Option<Vec<Answer>>>),
    Approve(oneshot::Sender<ApprovalDecision>),
    WebSetup(oneshot::Sender<bool>),
    Review(oneshot::Sender<ActionChoice>),
    Pty { tool: String, input: mpsc::UnboundedSender<PtyInput>, reply: Option<oneshot::Sender<(u16, u16)>> },
}

/// The agent's side: turns events into wire events, keeping each reply handle by id until the reply
/// command arrives.
#[derive(Default)]
pub struct AgentAdapter {
    parked: HashMap<u64, Parked>,
    next: u64,
}

impl AgentAdapter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Requests that still wait for an answer.
    pub fn parked(&self) -> usize {
        self.parked.len()
    }

    pub fn is_parked(&self, id: u64) -> bool {
        self.parked.contains_key(&id)
    }

    fn park(&mut self, p: Parked) -> u64 {
        self.next += 1;
        self.parked.insert(self.next, p);
        self.next
    }

    pub fn event(&mut self, ev: AgentEvent) -> WireEvent {
        match ev {
            AgentEvent::Ask { questions, reply } => WireEvent::Ask { id: self.park(Parked::Ask(reply)), questions },
            AgentEvent::Approve { request, reply } => {
                WireEvent::Approve { id: self.park(Parked::Approve(reply)), request }
            }
            AgentEvent::WebSetup { reply } => WireEvent::WebSetup { id: self.park(Parked::WebSetup(reply)) },
            AgentEvent::Review { request, reply } => {
                WireEvent::Review { id: self.park(Parked::Review(reply)), request }
            }
            AgentEvent::PtyOpen { id: tool, input, reply } => {
                let id = self.park(Parked::Pty { tool: tool.clone(), input, reply: Some(reply) });
                WireEvent::PtyOpen { id, tool }
            }
            mut ev => {
                if let AgentEvent::ToolEnd { id, .. } = &ev {
                    self.parked.retain(|_, p| !matches!(p, Parked::Pty { tool, .. } if tool == id));
                }
                event_inputs(&mut ev, inline);
                WireEvent::Event(ev)
            }
        }
    }

    /// Completes the handle a reply names; returns the command a `WireCommand::Command` carries.
    pub fn command(&mut self, c: WireCommand) -> Option<Command> {
        match c {
            WireCommand::Command(mut c) => {
                command_inputs(&mut c, keep);
                return Some(c);
            }
            WireCommand::Answer { id, answers } => {
                if let Some(Parked::Ask(tx)) = self.take(id, |p| matches!(p, Parked::Ask(_))) {
                    let _ = tx.send(answers);
                }
            }
            WireCommand::Approval { id, decision } => {
                if let Some(Parked::Approve(tx)) = self.take(id, |p| matches!(p, Parked::Approve(_))) {
                    let _ = tx.send(decision);
                }
            }
            WireCommand::WebSetupDone { id, ready } => {
                if let Some(Parked::WebSetup(tx)) = self.take(id, |p| matches!(p, Parked::WebSetup(_))) {
                    let _ = tx.send(ready);
                }
            }
            WireCommand::Decided { id, choice } => {
                if let Some(Parked::Review(tx)) = self.take(id, |p| matches!(p, Parked::Review(_))) {
                    let _ = tx.send(choice);
                }
            }
            WireCommand::PtySize { id, cols, rows } => {
                if let Some(Parked::Pty { reply, input, .. }) = self.parked.get_mut(&id) {
                    match reply.take() {
                        Some(tx) => {
                            let _ = tx.send((cols, rows));
                        }
                        None => {
                            let _ = input.send(PtyInput::Resize { cols, rows });
                        }
                    }
                }
            }
            WireCommand::PtyInput { id, input } => {
                if let Some(Parked::Pty { input: tx, .. }) = self.parked.get(&id) {
                    let _ = tx.send(input);
                }
            }
            WireCommand::Dropped { id } => {
                self.parked.remove(&id);
            }
        }
        None
    }

    fn take(&mut self, id: u64, want: fn(&Parked) -> bool) -> Option<Parked> {
        if self.parked.get(&id).is_some_and(want) { self.parked.remove(&id) } else { None }
    }
}

#[allow(clippy::large_enum_variant)] // passed straight on, never stored
pub enum Incoming {
    Event(AgentEvent),
    CaughtUp,
    Detached(String),
    /// Nothing to show: a request the client already shows, or the list of pending ones.
    Skip,
}

/// The front end's side: turns wire events back into events with fresh local reply handles, whose
/// answers go out as reply commands.
///
/// Needs a Tokio runtime: each reply handle gets a task that waits for it.
pub struct ClientAdapter {
    out: mpsc::UnboundedSender<WireCommand>,
    /// The requests shown and not yet answered, by id: the task that waits for each reply handle.
    live: Arc<Mutex<HashMap<u64, tokio::task::AbortHandle>>>,
}

impl ClientAdapter {
    pub fn new(out: mpsc::UnboundedSender<WireCommand>) -> Self {
        Self { out, live: Arc::default() }
    }

    pub fn command(&self, mut c: Command) -> WireCommand {
        command_inputs(&mut c, inline);
        WireCommand::Command(c)
    }

    /// Waits for the local reply handle; a handle dropped unanswered is reported so the agent drops its own.
    fn relay<T: Send + 'static>(&self, id: u64, rx: oneshot::Receiver<T>, reply: fn(u64, T) -> WireCommand) {
        let out = self.out.clone();
        let live = self.live.clone();
        let task = tokio::spawn(async move {
            let command = match rx.await {
                Ok(v) => reply(id, v),
                Err(_) => WireCommand::Dropped { id },
            };
            if let Ok(mut l) = live.lock() {
                l.remove(&id);
            }
            let _ = out.send(command);
        });
        if let Ok(mut l) = self.live.lock() {
            l.insert(id, task.abort_handle());
        }
    }

    /// The agent's pending ids: a request shown for any other id is forgotten, which closes its
    /// reply handle without telling the agent (it no longer has the request).
    fn settle(&self, pending: &[u64]) {
        if let Ok(mut l) = self.live.lock() {
            l.retain(|id, task| {
                let keep = pending.contains(id);
                if !keep {
                    task.abort();
                }
                keep
            });
        }
    }

    fn shown(&self, id: u64) -> bool {
        self.live.lock().is_ok_and(|l| l.contains_key(&id))
    }

    /// What a wire event is to the front end: an event, or one of the two that end or settle a connection.
    pub fn incoming(&self, w: WireEvent) -> Incoming {
        match w {
            WireEvent::CaughtUp => Incoming::CaughtUp,
            WireEvent::Detached { reason } => Incoming::Detached(reason),
            WireEvent::Pending { ids } => {
                self.settle(&ids);
                Incoming::Skip
            }
            WireEvent::Ask { id, .. }
            | WireEvent::Approve { id, .. }
            | WireEvent::WebSetup { id }
            | WireEvent::Review { id, .. }
            | WireEvent::PtyOpen { id, .. }
                if self.shown(id) =>
            {
                Incoming::Skip
            }
            w => Incoming::Event(self.event(w)),
        }
    }

    /// The event a wire event stands for. `CaughtUp` and `Detached` are not events (see `incoming`):
    /// they come out as an empty notice.
    pub fn event(&self, w: WireEvent) -> AgentEvent {
        match w {
            WireEvent::CaughtUp | WireEvent::Detached { .. } | WireEvent::Pending { .. } => {
                AgentEvent::Notice(String::new())
            }
            WireEvent::Event(mut ev) => {
                event_inputs(&mut ev, keep);
                ev
            }
            WireEvent::Ask { id, questions } => {
                let (reply, rx) = oneshot::channel();
                self.relay(id, rx, |id, answers| WireCommand::Answer { id, answers });
                AgentEvent::Ask { questions, reply }
            }
            WireEvent::Approve { id, request } => {
                let (reply, rx) = oneshot::channel();
                self.relay(id, rx, |id, decision| WireCommand::Approval { id, decision });
                AgentEvent::Approve { request, reply }
            }
            WireEvent::WebSetup { id } => {
                let (reply, rx) = oneshot::channel();
                self.relay(id, rx, |id, ready| WireCommand::WebSetupDone { id, ready });
                AgentEvent::WebSetup { reply }
            }
            WireEvent::Review { id, request } => {
                let (reply, rx) = oneshot::channel();
                self.relay(id, rx, |id, choice| WireCommand::Decided { id, choice });
                AgentEvent::Review { request, reply }
            }
            WireEvent::PtyOpen { id, tool } => {
                let (reply, rx) = oneshot::channel();
                self.relay(id, rx, |id, (cols, rows)| WireCommand::PtySize { id, cols, rows });
                let (input, mut keys) = mpsc::unbounded_channel();
                let out = self.out.clone();
                tokio::spawn(async move {
                    while let Some(input) = keys.recv().await {
                        if out.send(WireCommand::PtyInput { id, input }).is_err() {
                            break;
                        }
                    }
                });
                AgentEvent::PtyOpen { id: tool, input, reply }
            }
        }
    }
}

/// Whether `MU_WIRE_LOOPBACK=1`: the ordinary one-process mu sends every command and event through
/// this module's lines, so day-to-day use and the UI tests exercise the wire types.
pub fn loopback_on() -> bool {
    std::env::var_os("MU_WIRE_LOOPBACK").is_some_and(|v| v == "1")
}

/// A command as the agent receives it from a client.
pub fn roundtrip_command(c: Command) -> Command {
    let (tx, _rx) = mpsc::unbounded_channel();
    let line = to_line(&ClientAdapter::new(tx).command(c));
    match from_line::<WireCommand>(&line).ok().and_then(|w| AgentAdapter::new().command(w)) {
        Some(c) => c,
        None => unreachable!("a command line decodes to a command"),
    }
}

/// An event as a client receives it. The five that carry a reply handle are returned as they are:
/// a handle cannot be rebuilt without a runtime to wait on it (`ClientAdapter` does that).
pub fn roundtrip_event(e: AgentEvent) -> AgentEvent {
    if matches!(
        e,
        AgentEvent::Ask { .. }
            | AgentEvent::Approve { .. }
            | AgentEvent::WebSetup { .. }
            | AgentEvent::Review { .. }
            | AgentEvent::PtyOpen { .. }
    ) {
        return e;
    }
    let line = to_line(&AgentAdapter::new().event(e));
    match from_line::<WireEvent>(&line) {
        Ok(WireEvent::Event(mut ev)) => {
            event_inputs(&mut ev, keep);
            ev
        }
        _ => unreachable!("an event line decodes to an event"),
    }
}

/// Runs both ends of a wire inside this process and joins them with in-memory lines: the agent's
/// channels in, the channels a front end uses out. Needs a Tokio runtime.
pub fn loopback(
    cmd_tx: mpsc::UnboundedSender<Command>,
    mut ev_rx: mpsc::UnboundedReceiver<AgentEvent>,
) -> (mpsc::UnboundedSender<Command>, mpsc::UnboundedReceiver<AgentEvent>) {
    let (to_agent, mut at_agent) = mpsc::unbounded_channel::<String>();
    let (to_client, mut at_client) = mpsc::unbounded_channel::<String>();
    let (ui_cmd_tx, mut ui_cmd_rx) = mpsc::unbounded_channel::<Command>();
    let (ui_ev_tx, ui_ev_rx) = mpsc::unbounded_channel::<AgentEvent>();

    tokio::spawn(async move {
        let mut adapter = AgentAdapter::new();
        loop {
            tokio::select! {
                ev = ev_rx.recv() => {
                    let Some(ev) = ev else { break };
                    if to_client.send(to_line(&adapter.event(ev))).is_err() {
                        break;
                    }
                }
                line = at_agent.recv() => match line {
                    Some(line) => {
                        if let Some(c) = from_line::<WireCommand>(&line).ok().and_then(|w| adapter.command(w)) {
                            let _ = cmd_tx.send(c);
                        }
                    }
                    None => break,
                },
            }
        }
    });

    tokio::spawn(async move {
        let (replies_tx, mut replies) = mpsc::unbounded_channel();
        let adapter = ClientAdapter::new(replies_tx);
        loop {
            tokio::select! {
                c = ui_cmd_rx.recv() => {
                    let Some(c) = c else { break };
                    if to_agent.send(to_line(&adapter.command(c))).is_err() {
                        break;
                    }
                }
                Some(r) = replies.recv() => {
                    if to_agent.send(to_line(&r)).is_err() {
                        break;
                    }
                }
                line = at_client.recv() => match line {
                    Some(line) => {
                        let ev = match from_line::<WireEvent>(&line) {
                            Ok(w) => adapter.event(w),
                            Err(e) => AgentEvent::Error(format!("wire: unreadable event: {e}")),
                        };
                        if ui_ev_tx.send(ev).is_err() {
                            break;
                        }
                    }
                    None => break,
                },
            }
        }
    });

    (ui_cmd_tx, ui_ev_rx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{
        ActionInput, ActionSpec, CacheBreak, CacheStatus, GitInfo, JobKind, ModeAction, ProviderUsage, QuotaWindow,
        RewindPoint, StatusSnapshot, SubagentState, SubagentStatus,
    };
    use crate::message::Image;
    use crate::permissions::{ApprovalScope, PermissionMode};
    use crate::tool::{FileChange, ToolOutput};
    use serde_json::json;
    use std::time::{Duration, SystemTime};

    fn through<T: Serialize + DeserializeOwned>(v: &T) -> T {
        from_line(&to_line(v)).unwrap()
    }

    fn json_of<T: Serialize>(v: &T) -> serde_json::Value {
        serde_json::to_value(v).unwrap()
    }

    #[test]
    fn every_command_survives_a_line() {
        let cmds = vec![
            Command::Prompt("hi".into()),
            Command::EnsureSession,
            Command::SetModel("a/b".into()),
            Command::RefreshModel,
            Command::RefreshStatus,
            Command::SetThinking(crate::models::Thinking::High),
            Command::CycleThinking,
            Command::SetPermissionMode(PermissionMode::Edits),
            Command::NewSession,
            Command::Compact(Some("keep x".into())),
            Command::Review(None),
            Command::Mode { name: "plan".into(), prompt: Some("a".into()) },
            Command::ExitMode,
            Command::Resume("/tmp/s.jsonl".into()),
            Command::Fork,
            Command::ListRewind,
            Command::Rewind("e1".into()),
            Command::Bash { cmd: "ls".into(), include: true },
            Command::Rename("n".into()),
            Command::KillJobs(None),
            Command::RefreshTools,
            Command::Exit,
            Command::ListPlans,
            Command::OpenPlan("p".into()),
            Command::Deliver("p".into()),
            Command::StartPlan { id: "p".into(), decision: ModeAction::Keep { model: Some("m".into()) }, anyway: true },
            Command::RefinePlan { id: "p".into(), text: None },
            Command::Steer("s".into()),
            Command::FollowUp("f".into()),
            Command::PopQueue,
            Command::Cancel,
            Command::Stop,
        ];
        for c in cmds {
            let before = json_of(&c);
            assert_eq!(json_of(&through(&c)), before);
        }
    }

    fn status() -> StatusSnapshot {
        StatusSnapshot {
            model_id: "p/m".into(),
            context_tokens: Some(5),
            git: Some(GitInfo { branch: Some("main".into()), staged: 1, ..Default::default() }),
            usage: Some(ProviderUsage {
                provider: "p".into(),
                windows: vec![QuotaWindow {
                    label: "5h".into(),
                    used_percent: 12.5,
                    resets_in: Duration::from_secs(90),
                }],
            }),
            cache: Some(CacheStatus {
                input: 1,
                cache_read: 2,
                cache_write: 3,
                at_ms: 4,
                changed: Some(CacheBreak::Tools),
            }),
            permissions: PermissionMode::Ask,
            ..Default::default()
        }
    }

    fn job() -> SubagentStatus {
        SubagentStatus {
            name: "w".into(),
            agent: "task".into(),
            tools: 2,
            last: "x".into(),
            state: SubagentState::Running,
            kind: JobKind::Bash,
            cwd: "/tmp".into(),
            task: "t".into(),
            started: SystemTime::UNIX_EPOCH + Duration::from_millis(1_700_000_000_123),
            ended: None,
            delivered: false,
            note: Some("n".into()),
            session: Some("/s".into()),
            model: None,
            changes: Some(crate::events::JobChanges {
                r#ref: "refs/mu/result/x".into(),
                base: "0".repeat(40),
                applied: false,
            }),
            progress: None,
        }
    }

    #[test]
    fn every_plain_event_survives_a_line() {
        let events = vec![
            AgentEvent::TurnStart,
            AgentEvent::Woken,
            AgentEvent::UserPrompt("u".into()),
            AgentEvent::Text("t".into()),
            AgentEvent::Thinking("t".into()),
            AgentEvent::ThinkingLevel(crate::models::Thinking::Low),
            AgentEvent::PermissionMode(PermissionMode::Yolo),
            AgentEvent::BlockEnd,
            AgentEvent::ToolCallStart { id: "1".into(), name: "read".into() },
            AgentEvent::ToolCallDelta { id: "1".into(), json: "{\"a".into() },
            AgentEvent::ToolStart { id: "1".into(), name: "read".into(), args: json!({"path": "a"}) },
            AgentEvent::ToolOutput { id: "1".into(), chunk: "\u{1b}[0mx".into() },
            AgentEvent::Todo(json!([{"name": "p"}])),
            AgentEvent::ToolSkipped { id: "1".into() },
            AgentEvent::ToolEnd {
                id: "1".into(),
                name: "edit".into(),
                output: ToolOutput {
                    text: "ok".into(),
                    is_error: false,
                    display: Some("d".into()),
                    details: Some(json!({"a": 1})),
                    meta: None,
                    images: vec![],
                    changes: vec![FileChange { path: "/a".into(), old: None, new: Some("n".into()) }],
                },
            },
            AgentEvent::Retry { attempt: 2, max_attempts: 5, delay: Duration::from_millis(1500), reason: "r".into() },
            AgentEvent::Notice("n".into()),
            AgentEvent::ChildEntry { file: "f".into(), line: "l".into() },
            AgentEvent::Error("e".into()),
            AgentEvent::Compacting,
            AgentEvent::Compacted { before: 9, after: 3 },
            AgentEvent::Jobs(vec![job()]),
            AgentEvent::JobsDelivered(vec!["w".into()]),
            AgentEvent::JobOutput { id: "w".into(), tail: "t".into() },
            AgentEvent::PlanParked { id: "p".into(), path: "/p".into() },
            AgentEvent::PlanList(vec![]),
            AgentEvent::Status(status()),
            AgentEvent::TurnEnd { aborted: true },
            AgentEvent::TurnFailed,
            AgentEvent::Queue(QueueState { steer: vec!["a".into()], follow_up: vec!["b".into()] }),
            AgentEvent::QueuePopped("q".into()),
            AgentEvent::RewindPoints(vec![RewindPoint { id: "e".into(), text: "t".into(), reply: true }]),
            AgentEvent::QueuedDropped(vec!["d".into()]),
            AgentEvent::Exit,
        ];
        let (tx, _rx) = mpsc::unbounded_channel();
        let client = ClientAdapter::new(tx);
        let mut agent = AgentAdapter::new();
        for ev in events {
            let before = json_of(&ev);
            let wire = agent.event(ev);
            let back = client.event(through(&wire));
            assert_eq!(json_of(&back), before);
        }
        assert_eq!(agent.parked(), 0);
    }

    #[test]
    fn answers_and_decisions_survive_a_line() {
        for a in [
            Answer::Options(vec!["a".into()]),
            Answer::Noted { picks: vec!["a".into()], note: "n".into() },
            Answer::Custom("c".into()),
            Answer::Skipped,
            Answer::Cancelled,
        ] {
            assert_eq!(json_of(&through(&a)), json_of(&a));
        }
        for d in [
            ApprovalDecision::Allow { scope: ApprovalScope::Project, note: None, patterns: Some(vec!["x".into()]) },
            ApprovalDecision::Deny { note: Some("no".into()) },
            ApprovalDecision::Unavailable,
        ] {
            assert_eq!(json_of(&through(&d)), json_of(&d));
        }
    }

    fn request() -> ApprovalRequest {
        ApprovalRequest {
            tool: "bash".into(),
            call_id: "c".into(),
            title: "t".into(),
            reason: "r".into(),
            body: "b".into(),
            from: None,
            grant: None,
        }
    }

    /// The loop the two adapters make: event out, line over, reply back, line over, handle completed.
    fn exchange(agent: &mut AgentAdapter, client: &ClientAdapter, ev: AgentEvent) -> AgentEvent {
        let wire: WireEvent = through(&agent.event(ev));
        client.event(wire)
    }

    async fn deliver(agent: &mut AgentAdapter, out: &mut mpsc::UnboundedReceiver<WireCommand>) {
        let reply: WireCommand = through(&out.recv().await.unwrap());
        assert!(agent.command(reply).is_none());
    }

    #[tokio::test]
    async fn each_reply_reaches_the_parked_handle() {
        let (tx, mut out) = mpsc::unbounded_channel();
        let client = ClientAdapter::new(tx);
        let mut agent = AgentAdapter::new();

        let (reply, ask_rx) = oneshot::channel();
        let AgentEvent::Ask { reply: local, questions } = exchange(
            &mut agent,
            &client,
            AgentEvent::Ask {
                questions: vec![AskQuestion {
                    question: "q".into(),
                    context: None,
                    header: None,
                    options: vec![],
                    multi: false,
                    recommended: None,
                    confirm: true,
                    skippable: false,
                }],
                reply,
            },
        ) else {
            panic!("not an Ask")
        };
        assert_eq!(questions.len(), 1);
        local.send(Some(vec![Answer::Skipped])).unwrap();
        deliver(&mut agent, &mut out).await;
        assert!(matches!(ask_rx.await.unwrap().unwrap()[0], Answer::Skipped));

        let (reply, rx) = oneshot::channel();
        let AgentEvent::Approve { reply: local, .. } =
            exchange(&mut agent, &client, AgentEvent::Approve { request: request(), reply })
        else {
            panic!("not an Approve")
        };
        local.send(ApprovalDecision::Deny { note: Some("n".into()) }).unwrap();
        deliver(&mut agent, &mut out).await;
        assert!(matches!(rx.await.unwrap(), ApprovalDecision::Deny { note: Some(n) } if n == "n"));

        let (reply, rx) = oneshot::channel();
        let AgentEvent::WebSetup { reply: local } = exchange(&mut agent, &client, AgentEvent::WebSetup { reply })
        else {
            panic!("not a WebSetup")
        };
        local.send(true).unwrap();
        deliver(&mut agent, &mut out).await;
        assert!(rx.await.unwrap());

        let (reply, rx) = oneshot::channel();
        let request = ReviewRequest {
            header: "Plan review".into(),
            title: "T".into(),
            uri: Some("u".into()),
            file: Some("/f".into()),
            content: "c".into(),
            note: None,
            actions: vec![ActionSpec {
                id: "refine".into(),
                label: "Refine plan".into(),
                disabled: false,
                input: ActionInput::Required,
                keeps_context: false,
                approves: false,
            }],
            default: 0,
            models: true,
            cancel_turn: true,
            cancel_notice: "n".into(),
        };
        let AgentEvent::Review { reply: local, request, .. } =
            exchange(&mut agent, &client, AgentEvent::Review { request, reply })
        else {
            panic!("not a Review")
        };
        assert!(request.title == "T" && request.actions[0].input == ActionInput::Required);
        local.send(ActionChoice { id: "refine".into(), text: Some("more".into()), model: None }).unwrap();
        deliver(&mut agent, &mut out).await;
        let got = rx.await.unwrap();
        assert!(got.id == "refine" && got.text.as_deref() == Some("more"));
        assert_eq!(agent.parked(), 0);
    }

    #[tokio::test]
    async fn a_loopback_carries_commands_events_and_replies() {
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();
        let (ev_tx, ev_rx) = mpsc::unbounded_channel();
        let (ui_cmd, mut ui_ev) = loopback(cmd_tx, ev_rx);

        // The scripted agent: a prompt starts a turn that needs one approval, then ends; Exit ends it.
        let agent = tokio::spawn(async move {
            let mut approved = None;
            while let Some(c) = cmd_rx.recv().await {
                match c {
                    Command::Prompt(p) => {
                        let _ = ev_tx.send(AgentEvent::UserPrompt(p.text));
                        let (reply, rx) = oneshot::channel();
                        let _ = ev_tx.send(AgentEvent::Approve { request: request(), reply });
                        approved = Some(rx.await);
                        let _ = ev_tx.send(AgentEvent::TurnEnd { aborted: false });
                    }
                    Command::Exit => {
                        let _ = ev_tx.send(AgentEvent::Exit);
                        break;
                    }
                    _ => {}
                }
            }
            approved
        });

        ui_cmd.send(Command::Prompt("hello".into())).unwrap();
        assert!(matches!(ui_ev.recv().await, Some(AgentEvent::UserPrompt(t)) if t == "hello"));
        let Some(AgentEvent::Approve { request, reply }) = ui_ev.recv().await else { panic!("no approval") };
        assert_eq!(request.tool, "bash");
        reply.send(ApprovalDecision::Allow { scope: ApprovalScope::Once, note: None, patterns: None }).unwrap();
        assert!(matches!(ui_ev.recv().await, Some(AgentEvent::TurnEnd { aborted: false })));
        ui_cmd.send(Command::Exit).unwrap();
        assert!(matches!(ui_ev.recv().await, Some(AgentEvent::Exit)));
        assert!(matches!(agent.await.unwrap(), Some(Ok(ApprovalDecision::Allow { .. }))));
        assert!(ui_ev.recv().await.is_none(), "the events end with the agent");
    }

    #[test]
    fn the_test_round_trips_return_what_went_in() {
        let c = roundtrip_command(Command::Bash { cmd: "ls".into(), include: true });
        assert!(matches!(c, Command::Bash { cmd, include: true } if cmd == "ls"));
        let e = roundtrip_event(AgentEvent::Retry {
            attempt: 1,
            max_attempts: 3,
            delay: Duration::from_millis(250),
            reason: "r".into(),
        });
        assert!(matches!(e, AgentEvent::Retry { delay, .. } if delay == Duration::from_millis(250)));
        let (reply, _rx) = oneshot::channel();
        assert!(matches!(roundtrip_event(AgentEvent::WebSetup { reply }), AgentEvent::WebSetup { .. }));
    }

    #[tokio::test]
    async fn a_dropped_client_handle_drops_the_agents() {
        let (tx, mut out) = mpsc::unbounded_channel();
        let client = ClientAdapter::new(tx);
        let mut agent = AgentAdapter::new();
        let (reply, rx) = oneshot::channel::<ApprovalDecision>();
        let local = exchange(&mut agent, &client, AgentEvent::Approve { request: request(), reply });
        assert_eq!(agent.parked(), 1);
        drop(local);
        deliver(&mut agent, &mut out).await;
        assert!(rx.await.is_err(), "the agent's handle is dropped as the UI's was");
        assert_eq!(agent.parked(), 0);
    }

    #[tokio::test]
    async fn a_terminal_gets_its_size_then_its_input_and_is_forgotten_at_its_end() {
        let (tx, mut out) = mpsc::unbounded_channel();
        let client = ClientAdapter::new(tx);
        let mut agent = AgentAdapter::new();

        let (input, mut keys) = mpsc::unbounded_channel();
        let (reply, size) = oneshot::channel();
        let AgentEvent::PtyOpen { id, input: local_input, reply: local_reply } =
            exchange(&mut agent, &client, AgentEvent::PtyOpen { id: "call1".into(), input, reply })
        else {
            panic!("not a PtyOpen")
        };
        assert_eq!(id, "call1");
        local_reply.send((80, 24)).unwrap();
        deliver(&mut agent, &mut out).await;
        assert_eq!(size.await.unwrap(), (80, 24));

        local_input.send(PtyInput::Bytes(b"ls\n".to_vec())).unwrap();
        deliver(&mut agent, &mut out).await;
        assert!(matches!(keys.recv().await.unwrap(), PtyInput::Bytes(b) if b == b"ls\n"));
        local_input.send(PtyInput::Resize { cols: 100, rows: 30 }).unwrap();
        deliver(&mut agent, &mut out).await;
        assert!(matches!(keys.recv().await.unwrap(), PtyInput::Resize { cols: 100, rows: 30 }));

        assert_eq!(agent.parked(), 1);
        let end = ToolOutput {
            text: String::new(),
            is_error: false,
            display: None,
            details: None,
            meta: None,
            images: vec![],
            changes: vec![],
        };
        agent.event(AgentEvent::ToolEnd { id: "call1".into(), name: "bash".into(), output: end });
        assert_eq!(agent.parked(), 0);
    }

    #[test]
    fn an_image_attached_on_one_side_is_readable_on_the_other() {
        mu_testutil::home();
        let png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        let client_img = image::from_bytes(png.clone()).unwrap();
        assert!(image::blob_hash(&client_img.data).is_some(), "stored as a reference");
        let input = UserInput { text: "see".into(), images: vec![client_img.clone()], command: None };

        let (tx, _rx) = mpsc::unbounded_channel();
        let line = to_line(&ClientAdapter::new(tx).command(Command::Prompt(input)));
        assert!(!line.contains("blob:sha256:"), "the line carries the bytes, not the reference");

        let mut agent = AgentAdapter::new();
        let Some(Command::Prompt(got)) = agent.command(from_line(&line).unwrap()) else { panic!("not a prompt") };
        assert_eq!(got.images[0], client_img, "same content, same reference");
        let Image { data, .. } = &got.images[0];
        assert_eq!(image::resolve(data).unwrap(), base64_of(&png));
    }

    fn base64_of(b: &[u8]) -> String {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(b)
    }
}
