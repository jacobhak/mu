//! What an agent keeps so a client that attaches late, or again, sees what it missed.
//!
//! The agent end of a wire outlives connections: with no client it goes on reading the agent's events
//! into this, and never drops a reply handle (a dropped approval would deny the call and end the turn).
//! On attach `Catchup::catch_up` yields the lines that bring a client level: the stored history up to
//! the turn in progress, that turn's events, the latest status, jobs, queue and todo, every request
//! still waiting for an answer, then `CaughtUp`.

use crate::events::{AgentEvent, Command};
use crate::wire::{AgentAdapter, WireCommand, WireEvent, event_line};
use std::collections::BTreeMap;

/// The turn log is bounded: past either limit whole blocks are dropped from the front.
const MAX_EVENTS: usize = 20_000;
const MAX_BYTES: usize = 16 * 1024 * 1024;
const TRUNCATED: &str = "earlier output of this turn is not shown";

/// The events of which only the newest matters to a client that has missed the rest.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum State {
    Status,
    Jobs,
    Queue,
    Todo,
    PermissionMode,
    ThinkingLevel,
    Cwd,
}

fn state_of(e: &AgentEvent) -> Option<State> {
    Some(match e {
        AgentEvent::Status(_) => State::Status,
        AgentEvent::Jobs(_) => State::Jobs,
        AgentEvent::Queue(_) => State::Queue,
        AgentEvent::Todo(_) => State::Todo,
        AgentEvent::PermissionMode(_) => State::PermissionMode,
        AgentEvent::ThinkingLevel(_) => State::ThinkingLevel,
        AgentEvent::Cwd(_) => State::Cwd,
        _ => return None,
    })
}

/// Whether a dropped log may start just after this event.
fn is_boundary(e: &AgentEvent) -> bool {
    matches!(e, AgentEvent::BlockEnd | AgentEvent::ToolEnd { .. })
}

/// A question or an approval that waits for an answer, as a one-line summary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Waiting {
    pub id: u64,
    pub approval: bool,
    pub summary: String,
}

#[derive(Default)]
pub struct Catchup {
    adapter: AgentAdapter,
    /// The turn in progress: each event's line and whether a log may be cut after it.
    turn: Vec<(bool, String)>,
    turn_bytes: usize,
    cut: bool,
    /// `Some(leaf)` while a turn runs: the session entry it began after.
    base: Option<Option<String>>,
    last: BTreeMap<State, String>,
    /// Requests waiting for an answer, by id, as the lines to send again.
    requests: BTreeMap<u64, String>,
    waiting: BTreeMap<u64, Waiting>,
}

impl Catchup {
    pub fn new() -> Self {
        Self::default()
    }

    /// Takes one event from the agent and returns the line to send to a connected client.
    pub fn event(&mut self, ev: AgentEvent) -> String {
        let boundary = is_boundary(&ev);
        let state = state_of(&ev);
        let mark = if let AgentEvent::TurnMark { leaf } = &ev { Some(leaf.clone()) } else { None };
        let ends = matches!(ev, AgentEvent::TurnEnd { .. } | AgentEvent::TurnFailed);
        let waits = match &ev {
            AgentEvent::Approve { request, .. } => Some((true, request.title.clone())),
            AgentEvent::Ask { questions, .. } => {
                Some((false, questions.first().map(|q| q.question.clone()).unwrap_or_default()))
            }
            _ => None,
        };
        let wire = self.adapter.event(ev);
        let request = match &wire {
            WireEvent::Ask { id, .. }
            | WireEvent::Approve { id, .. }
            | WireEvent::WebSetup { id }
            | WireEvent::Review { id, .. }
            | WireEvent::PtyOpen { id, .. } => Some(*id),
            _ => None,
        };
        let line = event_line(wire);
        match request {
            Some(id) => {
                self.requests.insert(id, line.clone());
                if let Some((approval, summary)) = waits {
                    self.waiting.insert(id, Waiting { id, approval, summary });
                }
            }
            None => {
                if let Some(leaf) = mark {
                    self.turn.clear();
                    self.turn_bytes = 0;
                    self.cut = false;
                    self.base = Some(leaf);
                } else if self.base.is_some() {
                    self.log(boundary, line.clone());
                    if ends {
                        self.turn.clear();
                        self.turn_bytes = 0;
                        self.cut = false;
                        self.base = None;
                    }
                }
                if let Some(s) = state {
                    self.last.insert(s, line.clone());
                }
            }
        }
        // A terminal call's request goes when its tool ends.
        self.forget_answered();
        line
    }

    fn log(&mut self, boundary: bool, line: String) {
        self.turn_bytes += line.len();
        self.turn.push((boundary, line));
        while self.turn.len() > MAX_EVENTS || self.turn_bytes > MAX_BYTES {
            self.cut = true;
            let upto = self.turn.iter().position(|(b, _)| *b).map_or(self.turn.len(), |i| i + 1);
            let gone: usize = self.turn.drain(..upto).map(|(_, l)| l.len()).sum();
            self.turn_bytes -= gone;
        }
    }

    /// A command from the client: completes the handle a reply names, or returns the command for the agent.
    pub fn command(&mut self, c: WireCommand) -> Option<Command> {
        let out = self.adapter.command(c);
        self.forget_answered();
        out
    }

    fn forget_answered(&mut self) {
        let adapter = &self.adapter;
        self.requests.retain(|id, _| adapter.is_parked(*id));
        self.waiting.retain(|id, _| adapter.is_parked(*id));
    }

    /// The questions and approvals that wait for an answer, oldest first.
    pub fn waiting(&self) -> Vec<Waiting> {
        self.waiting.values().cloned().collect()
    }

    /// The session entry the running turn began after, `None` when no turn runs. `Some(None)` is a turn
    /// that began before any entry existed.
    pub fn turn_base(&self) -> Option<Option<&str>> {
        self.base.as_ref().map(|l| l.as_deref())
    }

    /// The lines that bring a newly attached client level, after `history` (the stored conversation up to
    /// `turn_base`, as lines).
    pub fn catch_up(&self, history: Vec<String>) -> Vec<String> {
        let mut out = history;
        if self.cut {
            out.push(event_line(WireEvent::Event(AgentEvent::Notice(TRUNCATED.into()))));
        }
        out.extend(self.turn.iter().map(|(_, l)| l.clone()));
        out.extend(self.last.values().cloned());
        out.push(event_line(WireEvent::Pending { ids: self.requests.keys().copied().collect() }));
        out.extend(self.requests.values().cloned());
        out.push(event_line(WireEvent::CaughtUp));
        out
    }

    /// Requests that wait for an answer.
    pub fn parked(&self) -> usize {
        self.adapter.parked()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{QueueState, StatusSnapshot};
    use crate::permissions::{ApprovalDecision, ApprovalRequest, ApprovalScope};
    use crate::tool::ToolOutput;
    use crate::wire::{AgentLine, ClientAdapter, Incoming, from_line};
    use tokio::sync::{mpsc, oneshot};

    fn text(t: &str) -> AgentEvent {
        AgentEvent::Text(t.into())
    }

    fn start(c: &mut Catchup, leaf: &str) {
        c.event(AgentEvent::TurnMark { leaf: Some(leaf.into()) });
        c.event(AgentEvent::TurnStart);
    }

    /// What a client shows after reading these lines.
    fn shown(lines: &[String]) -> Vec<String> {
        let (tx, _rx) = mpsc::unbounded_channel();
        let client = ClientAdapter::new(tx);
        lines
            .iter()
            .map(|l| {
                match client.incoming(match from_line::<AgentLine>(l).unwrap() {
                    AgentLine::Event(w) => w,
                    other => panic!("not an event line: {}", crate::wire::to_line(&other)),
                }) {
                    Incoming::Event(AgentEvent::Text(t)) => format!("text:{t}"),
                    Incoming::Event(AgentEvent::Notice(n)) => format!("notice:{n}"),
                    Incoming::Event(AgentEvent::Status(s)) => format!("status:{}", s.model_id),
                    Incoming::Event(AgentEvent::Approve { request, .. }) => format!("approve:{}", request.tool),
                    Incoming::Event(AgentEvent::Queue(q)) => format!("queue:{}", q.steer.len()),
                    Incoming::Event(AgentEvent::PtyOpen { id, .. }) => format!("pty:{id}"),
                    Incoming::Event(e) => match serde_json::to_value(&e).unwrap() {
                        serde_json::Value::String(s) => format!("event:{s}"),
                        serde_json::Value::Object(o) => format!("event:{}", o.keys().next().unwrap()),
                        _ => "event:?".into(),
                    },
                    Incoming::CaughtUp => "caught-up".into(),
                    Incoming::Skip => "skip".into(),
                    Incoming::Detached(r) => format!("detached:{r}"),
                }
            })
            .filter(|l| l != "skip")
            .collect()
    }

    #[tokio::test]
    async fn a_client_that_attaches_mid_turn_gets_the_turn_the_state_and_the_waiting_requests() {
        let mut c = Catchup::new();
        assert_eq!(c.turn_base(), None);
        c.event(AgentEvent::Status(StatusSnapshot { model_id: "old".into(), ..Default::default() }));
        start(&mut c, "e7");
        assert_eq!(c.turn_base(), Some(Some("e7")));
        c.event(text("one"));
        c.event(AgentEvent::BlockEnd);
        c.event(AgentEvent::Status(StatusSnapshot { model_id: "new".into(), ..Default::default() }));
        c.event(AgentEvent::Queue(QueueState { steer: vec!["s".into()], follow_up: vec![] }));
        let (reply, rx) = oneshot::channel();
        c.event(AgentEvent::Approve {
            request: ApprovalRequest {
                tool: "bash".into(),
                call_id: "c".into(),
                title: "t".into(),
                reason: "r".into(),
                body: "b".into(),
                from: None,
                grant: None,
                models: Vec::new(),
            },
            reply,
        });
        c.event(text("two"));

        let lines = c.catch_up(vec![event_line(WireEvent::Event(AgentEvent::UserPrompt("hist".into())))]);
        let got = shown(&lines);
        assert_eq!(got[0], "event:UserPrompt");
        let rest: Vec<&str> = got[1..].iter().map(String::as_str).collect();
        assert_eq!(
            rest,
            [
                "event:TurnStart", // the turn log: TurnStart, text, BlockEnd, status, queue, text
                "text:one",
                "event:BlockEnd",
                "status:new",
                "queue:1",
                "text:two",
                "status:new", // the latest states
                "queue:1",
                "approve:bash", // the request waiting
                "caught-up",
            ]
        );

        // The request is answered once, whoever answers it.
        assert_eq!(c.parked(), 1);
        let allow = ApprovalDecision::Allow { scope: ApprovalScope::Once, note: None, patterns: None, model: None };
        assert!(c.command(WireCommand::Approval { id: 1, decision: allow }).is_none());
        assert!(matches!(rx.await.unwrap(), ApprovalDecision::Allow { .. }));
        assert_eq!(c.parked(), 0);
        assert!(!shown(&c.catch_up(vec![])).iter().any(|l| l.starts_with("approve")));
    }

    #[test]
    fn a_finished_turn_leaves_no_log_but_the_latest_state() {
        let mut c = Catchup::new();
        start(&mut c, "e1");
        c.event(text("x"));
        c.event(AgentEvent::Status(StatusSnapshot { model_id: "m".into(), ..Default::default() }));
        c.event(AgentEvent::TurnEnd { aborted: false });
        assert_eq!(c.turn_base(), None);
        assert_eq!(shown(&c.catch_up(vec![])), ["status:m", "caught-up"]);
        // Events between turns are not logged.
        c.event(text("late"));
        assert_eq!(shown(&c.catch_up(vec![])), ["status:m", "caught-up"]);
    }

    #[test]
    fn a_turn_without_a_session_file_has_no_leaf() {
        let mut c = Catchup::new();
        c.event(AgentEvent::TurnMark { leaf: None });
        assert_eq!(c.turn_base(), Some(None));
    }

    #[test]
    fn a_long_turn_drops_whole_blocks_from_the_front_and_says_so() {
        let mut c = Catchup::new();
        start(&mut c, "e1");
        let blocks = MAX_EVENTS / 2 + 100;
        for i in 0..blocks {
            c.event(text(&format!("a{i}")));
            c.event(AgentEvent::BlockEnd);
        }
        let got = shown(&c.catch_up(vec![]));
        assert!(got.len() < MAX_EVENTS + 8, "bounded: {}", got.len());
        assert_eq!(got[0], format!("notice:{TRUNCATED}"));
        // The log starts at a block start: the first kept event is a text, not a stray BlockEnd.
        assert!(got[1].starts_with("text:"), "{}", got[1]);
        assert_eq!(got.last().map(String::as_str), Some("caught-up"));
        // The newest text is still there.
        assert!(got.iter().any(|l| l == &format!("text:a{}", blocks - 1)));
    }

    #[tokio::test]
    async fn a_reconnecting_client_shows_a_request_once_and_closes_one_the_agent_dropped() {
        let mut c = Catchup::new();
        let (reply, _rx) = oneshot::channel();
        c.event(AgentEvent::Approve {
            request: ApprovalRequest {
                tool: "bash".into(),
                call_id: "c".into(),
                title: "t".into(),
                reason: "r".into(),
                body: "b".into(),
                from: None,
                grant: None,
                models: Vec::new(),
            },
            reply,
        });
        let (tx, _out) = mpsc::unbounded_channel();
        let client = ClientAdapter::new(tx);
        let feed = |lines: Vec<String>| -> Vec<Incoming> {
            lines
                .iter()
                .map(|l| match from_line::<AgentLine>(l).unwrap() {
                    AgentLine::Event(w) => client.incoming(w),
                    _ => unreachable!(),
                })
                .collect()
        };
        let mut shown_reply = None;
        for i in feed(c.catch_up(vec![])) {
            if let Incoming::Event(AgentEvent::Approve { reply, .. }) = i {
                shown_reply = Some(reply);
            }
        }
        let shown_reply = shown_reply.expect("the first catch-up opens the dialog");
        assert!(
            !feed(c.catch_up(vec![])).iter().any(|i| matches!(i, Incoming::Event(AgentEvent::Approve { .. }))),
            "no second dialog for the same request"
        );
        assert!(!shown_reply.is_closed());
        // Answered elsewhere: the next catch-up no longer lists it.
        let allow = ApprovalDecision::Allow { scope: ApprovalScope::Once, note: None, patterns: None, model: None };
        c.command(WireCommand::Approval { id: 1, decision: allow });
        feed(c.catch_up(vec![]));
        tokio::task::yield_now().await;
        assert!(shown_reply.is_closed(), "the dialog for a request the agent no longer has is closed");
    }

    #[tokio::test]
    async fn a_terminal_request_goes_when_its_tool_ends() {
        let mut c = Catchup::new();
        start(&mut c, "e1");
        let (input, _keys) = mpsc::unbounded_channel();
        let (reply, _size) = oneshot::channel();
        c.event(AgentEvent::PtyOpen { id: "call1".into(), input, reply });
        assert!(shown(&c.catch_up(vec![])).contains(&"pty:call1".to_string()));
        c.event(AgentEvent::ToolEnd {
            id: "call1".into(),
            name: "bash".into(),
            output: ToolOutput {
                text: String::new(),
                is_error: false,
                display: None,
                details: None,
                meta: None,
                images: vec![],
                changes: vec![],
            },
        });
        assert_eq!(c.parked(), 0);
        assert!(!shown(&c.catch_up(vec![])).contains(&"pty:call1".to_string()));
    }
}
