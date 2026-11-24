//! The agent's side of the screen: the transcript, running tools, streaming tool calls, background
//! jobs and the todo list. `AgentView::on_event` reduces agent events into it without an `App`;
//! what it cannot do alone (overlays, the inbox, the bell) comes back as an `Effect` for `App`.

use super::App;
use super::ui_msgs::UiMsg;
use crate::chat::blocks;
use crate::chat::partial_json::Partial;
use crate::chat::transcript::{Stream, Transcript};
use crate::host::cwd::{SET_CWD, Switched};
use crate::host::sessions::{self, Spend, TreeArgs};
use crate::panels::ptyview::PtyView;
use crate::term::bell::WokenBell;
use crate::text::line::{Line, dim, styled};
use crate::theme::{self, Role};
use crate::widgets::overlay::Overlay;
use mu_base::config::MuBell;
use mu_base::events::{
    ActionChoice, AgentEvent, Answer, AskQuestion, PlanMeta, QueueState, ReviewRequest, RewindPoint, StatusSnapshot,
};
use mu_base::events::{JobKind, SubagentState, SubagentStatus};
use mu_base::message::UserInput;
use mu_base::permissions::{ApprovalDecision, ApprovalRequest};
use mu_base::text::truncate_text as trunc;
use mu_base::tool::ToolOutput;
use mu_session::session::tree_cost::Child;
use mu_tools::tools::todo::{Phase, Status};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::time::Instant;
use tokio::sync::oneshot;

/// Most lines of a running tool's output shown under its header.
const TAIL_MAX: usize = 10;

/// Bash commands kept for `/copy`.
const BASH_CMDS_MAX: usize = 50;

/// A retry wait at least this long counts down in the viewport; a shorter one only gets the notice.
const RETRY_COUNTDOWN_FROM: std::time::Duration = std::time::Duration::from_secs(10);

pub(super) struct RunningTool {
    pub(super) id: String,
    name: String,
    args: Value,
    started: Instant,
    /// The last complete lines of output.
    tail: VecDeque<String>,
    /// Output after the last newline.
    partial: String,
}

pub(super) struct StreamingCall {
    id: String,
    name: String,
    json: Partial,
    args: Value,
    /// New bytes arrived since `args` was last computed.
    stale: bool,
}

/// What the reducer leaves for `App`: the parts that need an overlay, the inbox or the bell.
pub(super) enum Effect {
    /// The turn is waiting for the user: ring the terminal, if the setting is on.
    Ring,
    /// The session's working directory changed.
    Cwd(PathBuf),
    /// The view starts afresh (a client about to replay a conversation): clear the terminal too.
    ClearScreen,
    Ask {
        questions: Vec<AskQuestion>,
        reply: oneshot::Sender<Option<Vec<Answer>>>,
    },
    /// A tool call needs the user's approval.
    Approve {
        request: ApprovalRequest,
        reply: oneshot::Sender<ApprovalDecision>,
    },
    Review {
        request: ReviewRequest,
        reply: oneshot::Sender<ActionChoice>,
    },
    /// `web_search` needs a backend: run the setup wizard and reply whether web search is ready.
    WebSetup {
        reply: oneshot::Sender<bool>,
    },
    PlanList(Vec<PlanMeta>),
    /// The prompts `/rewind` can go back to.
    RewindPoints(Vec<RewindPoint>),
    /// A subagent finished: read what its session cost, then print its row.
    SubagentCost(SubagentStatus),
    /// The job snapshot changed: an open jobs panel takes it.
    JobsChanged,
    /// A bash job's output changed: an open jobs panel takes it from `AgentView::job_out`.
    JobOutput(String),
    /// The turn ended.
    TurnEnded {
        aborted: bool,
    },
    /// A queued message taken back by `PopQueue`: it goes to the composer.
    Popped(UserInput),
    /// Queued input the turn did not take, back to the composer.
    Restore {
        queued: Vec<UserInput>,
    },
}

/// Where the read of the subagents' costs stands: one is asked for at a time.
#[derive(Default)]
pub(super) struct SpendRead {
    pub(super) asked: bool,
    /// The jobs or the session moved on while it was out.
    again: bool,
    last: Option<Instant>,
}

impl SpendRead {
    /// Has it been a while since the last ask (the host reads children that are still running).
    pub(super) fn due(&self) -> bool {
        self.last.is_none_or(|t| t.elapsed() >= std::time::Duration::from_secs(1))
    }
}

pub(super) struct AgentView {
    /// The streamed conversation: committed lines, open text and thinking, smooth reveal.
    pub(super) tr: Transcript,
    pub(super) tools: Vec<RunningTool>,
    /// Tool calls whose arguments are still streaming or that are not yet executing.
    pub(super) calls: Vec<StreamingCall>,
    /// Latest background-job snapshot, and finished jobs already printed to the transcript.
    pub(super) jobs: Vec<SubagentStatus>,
    /// The host's read of the subagents' session files for the status line's cost.
    pub(super) spend: SpendRead,
    /// What the subagents have spent, as the last read found it.
    pub(super) subagent_cost: f64,
    /// What each subagent session has cost by itself, as the last read found it, for the running rows.
    pub(super) child_costs: HashMap<PathBuf, f64>,
    jobs_committed: HashSet<String>,
    /// Newest output of each background bash job, for the jobs panel.
    pub(super) job_out: HashMap<String, String>,
    /// The running job the inline marker is on, by name: `Alt+K` cancels it.
    pub(super) job_focus: Option<String>,
    pub(super) todo: Vec<Phase>,
    pub(super) status: StatusSnapshot,
    /// Bash commands run, for `/copy`.
    pub(super) bash_cmds: Vec<String>,
    /// Files the tools read, wrote or edited, newest first, for `/open`.
    pub(super) recent_files: std::collections::VecDeque<String>,
    /// Embedded terminal of a running `bash` call with `pty: true`.
    pub(super) pty: Option<PtyView>,
    pub(super) running: bool,
    pub(super) compacting: bool,
    /// A retry wait long enough to count down in the viewport: its end and the attempt that failed.
    retry_wait: Option<(Instant, u32, u32)>,
    pub(super) turn_started: Option<Instant>,
    pub(super) expanded: bool,
    pub(super) queued: QueueState,
    /// The last turn was aborted: input it leaves queued goes back to the composer without a notice.
    pub(super) turn_aborted: bool,
    bell: WokenBell,
    bell_cfg: MuBell,
    /// A client is being shown what it missed: nothing rings.
    pub(super) catching_up: bool,
    /// `show_thinking` and `smooth`, for a view that starts afresh.
    fresh: (bool, bool),
}

impl AgentView {
    pub(super) fn with_bell(mut self, cfg: MuBell) -> Self {
        self.bell_cfg = cfg;
        self
    }

    pub(super) fn new(show_thinking: bool, smooth: bool) -> Self {
        Self {
            tr: Transcript::new(show_thinking, smooth),
            tools: Vec::new(),
            calls: Vec::new(),
            jobs: Vec::new(),
            spend: SpendRead::default(),
            subagent_cost: 0.0,
            child_costs: HashMap::new(),
            jobs_committed: HashSet::new(),
            job_out: HashMap::new(),
            job_focus: None,
            todo: Vec::new(),
            status: StatusSnapshot::default(),
            bash_cmds: Vec::new(),
            recent_files: Default::default(),
            pty: None,
            running: false,
            compacting: false,
            retry_wait: None,
            turn_started: None,
            expanded: false,
            queued: QueueState::default(),
            turn_aborted: false,
            bell: WokenBell::default(),
            bell_cfg: MuBell::default(),
            catching_up: false,
            fresh: (show_thinking, smooth),
        }
    }

    /// Whether the screen needs redrawing on a timer: something is running or streaming.
    pub(super) fn busy(&self) -> bool {
        self.running
            || self.compacting
            || self.tr.thinking_active()
            || !self.tools.is_empty()
            || !self.calls.is_empty()
            || self.jobs.iter().any(|j| j.state == SubagentState::Running)
    }

    /// Recomputes the arguments of streaming calls that got new bytes; at most once per frame,
    /// since deltas arrive far more often than frames are drawn.
    pub(super) fn refresh_call_args(&mut self) {
        for c in &mut self.calls {
            if c.stale {
                if let Some(v) = c.json.value() {
                    c.args = v;
                }
                c.stale = false;
            }
        }
    }

    /// Applies one agent event. `size` is the screen's (columns, rows).
    pub(super) fn on_event(&mut self, e: AgentEvent, size: (usize, usize)) -> Vec<Effect> {
        #[cfg(test)]
        let e = if mu_base::wire::loopback_on() { mu_base::wire::roundtrip_event(e) } else { e };
        let w = size.0;
        if !matches!(
            e,
            AgentEvent::Text(_)
                | AgentEvent::Thinking(_)
                | AgentEvent::ToolCallDelta { .. }
                | AgentEvent::ToolOutput { .. }
                | AgentEvent::Status(_)
                | AgentEvent::ThinkingLevel(_)
                | AgentEvent::PermissionMode(_)
                | AgentEvent::Cwd(_)
                | AgentEvent::Jobs(_)
                | AgentEvent::Todo(_)
                | AgentEvent::TurnStart
                | AgentEvent::Retry { .. }
                | AgentEvent::ChildEntry { .. }
        ) {
            self.tr.flush_reveal(w);
        }
        let mut fx = Vec::new();
        match e {
            AgentEvent::Cwd(p) => fx.push(Effect::Cwd(p)),
            AgentEvent::TurnMark { .. } => {}
            AgentEvent::Replay(true) => {
                let (bell_cfg, (show, smooth)) = (self.bell_cfg, self.fresh);
                *self = AgentView::new(show, smooth).with_bell(bell_cfg);
                self.catching_up = true;
                fx.push(Effect::ClearScreen);
            }
            AgentEvent::Replay(false) => self.catching_up = false,
            AgentEvent::TurnStart => {
                self.running = true;
                self.turn_started = Some(Instant::now());
                self.bell.on_turn_start();
            }
            AgentEvent::Woken => self.bell.on_woken(),
            AgentEvent::UserPrompt(t) => {
                let mut l = vec![vec![]];
                l.extend(blocks::user_prompt(&t, w));
                l.push(vec![]);
                self.tr.commit(l);
            }
            AgentEvent::Text(s) => self.tr.queue_reveal(Stream::Text, s, w),
            AgentEvent::Thinking(s) => self.tr.queue_reveal(Stream::Thinking, s, w),
            AgentEvent::ToolCallStart { id, name } => {
                self.tr.finish_thinking(w);
                self.calls.push(StreamingCall { id, name, json: Default::default(), args: json!({}), stale: false });
            }
            AgentEvent::ToolCallDelta { id, json } => {
                if let Some(c) = self.calls.iter_mut().find(|c| c.id == id) {
                    c.json.push(&json);
                    c.stale = true;
                }
            }
            AgentEvent::BlockEnd => self.tr.end_block(w),
            AgentEvent::ToolStart { id, name, args } => {
                self.calls.retain(|c| c.id != id);
                self.tr.finish_thinking(w);
                if name == "bash"
                    && let Some(c) = args["command"].as_str()
                {
                    push_capped(&mut self.bash_cmds, c.to_string());
                }
                self.tools.push(RunningTool {
                    id,
                    name,
                    args,
                    started: Instant::now(),
                    tail: VecDeque::new(),
                    partial: String::new(),
                });
            }
            AgentEvent::PtyOpen { id, input, reply } => {
                // A pty is already open: dropping `reply` makes the tool fall back.
                if self.pty.is_none() {
                    let cells = PtyView::size_for(size.0, size.1);
                    self.pty = Some(PtyView::new(id, input, cells));
                    let _ = reply.send(cells);
                }
            }
            AgentEvent::PtyWaiting { id, waiting } => {
                if let Some(p) = self.pty.as_mut().filter(|p| p.id == id) {
                    p.waiting = waiting;
                    if waiting {
                        self.ring_if_attention(&mut fx);
                    }
                }
            }
            AgentEvent::ToolOutput { id, chunk } => {
                if let Some(p) = self.pty.as_mut().filter(|p| p.id == id) {
                    p.feed(&chunk);
                } else if let Some(t) = self.tools.iter_mut().find(|t| t.id == id) {
                    t.partial.push_str(&chunk);
                    while let Some(i) = t.partial.find('\n') {
                        let line: String = t.partial.drain(..=i).collect();
                        t.tail.push_back(line.trim_end_matches(['\n', '\r']).to_string());
                        if t.tail.len() > TAIL_MAX {
                            t.tail.pop_front();
                        }
                    }
                }
            }
            AgentEvent::ToolEnd { id, name, output } => {
                if self.pty.as_ref().is_some_and(|p| p.id == id) {
                    self.pty = None;
                }
                let args = self
                    .tools
                    .iter()
                    .position(|t| t.id == id)
                    .map(|i| self.tools.remove(i).args)
                    .unwrap_or(Value::Null);
                self.commit_tool_result(&name, &args, &output, w);
            }
            AgentEvent::Retry { attempt, max_attempts, delay, reason } => {
                self.tr.reset_stream();
                self.calls.clear();
                let msg = format!("↻ {}", mu_base::events::retry_notice(attempt, max_attempts, delay, &reason));
                self.tr.commit(notice_lines(&msg));
                self.retry_wait =
                    (delay >= RETRY_COUNTDOWN_FROM).then(|| (Instant::now() + delay, attempt, max_attempts));
            }
            AgentEvent::Notice(s) => self.tr.commit(notice_lines(&s)),
            AgentEvent::Error(s) => self.tr.commit(error_lines(&s)),
            AgentEvent::Compacting => self.compacting = true,
            AgentEvent::Compacted { before, after } => {
                self.compacting = false;
                self.tr.commit(notice_lines(&format!("── compacted {before}→{after} tokens")));
            }
            AgentEvent::Jobs(v) => {
                if v.is_empty() {
                    self.jobs_committed.clear();
                }
                for r in v.iter().filter(|r| r.state != SubagentState::Running) {
                    if self.jobs_committed.insert(r.name.clone()) {
                        // Its file is final now, and the periodic read may not have seen the end of it:
                        // the app reads it on a blocking thread and commits the row when it has the cost.
                        if r.kind == JobKind::Subagent && r.session.is_some() {
                            fx.push(Effect::SubagentCost(r.clone()));
                            continue;
                        }
                        let lines = match r.kind {
                            // A bash job's result is its output, run time and end, like a foreground run's.
                            JobKind::Bash => {
                                let out = self.job_out.get(&r.name).map(String::as_str);
                                blocks::bash_job_result(r, out, self.expanded, w)
                            }
                            JobKind::Subagent => blocks::subagent_rows(
                                std::slice::from_ref(r),
                                std::time::SystemTime::now(),
                                ' ',
                                |_| None,
                            ),
                        };
                        self.tr.commit(lines);
                    }
                }
                self.job_out.retain(|id, _| v.iter().any(|j| &j.name == id));
                self.jobs = v;
                if !self.job_focus.as_ref().is_some_and(|f| self.running_names().contains(f)) {
                    self.job_focus = None;
                }
                fx.push(Effect::JobsChanged);
            }
            AgentEvent::JobOutput { id, tail } => {
                self.job_out.insert(id.clone(), tail);
                fx.push(Effect::JobOutput(id));
            }
            AgentEvent::JobsDelivered(ids) => {
                self.tr.commit(vec![styled(format!("↳ job results delivered: {}", ids.join(", ")), |_| dim())]);
            }
            AgentEvent::Ask { questions, reply } => {
                self.ring_if_attention(&mut fx);
                fx.push(Effect::Ask { questions, reply });
            }
            AgentEvent::Approve { request, reply } => {
                self.ring_if_attention(&mut fx);
                fx.push(Effect::Approve { request, reply });
            }
            AgentEvent::WebSetup { reply } => {
                self.ring_if_attention(&mut fx);
                fx.push(Effect::WebSetup { reply });
            }
            AgentEvent::Review { request, reply } => {
                self.ring_if_attention(&mut fx);
                if let Some(file) = &request.file {
                    crate::host::plans::hand_out(file);
                }
                fx.push(Effect::Review { request, reply });
            }
            AgentEvent::PlanParked { id, path } => {
                self.tr.commit(notice_lines(&format!("plan parked: {id} · {}", path.display())));
            }
            AgentEvent::PlanList(list) => fx.push(Effect::PlanList(list)),
            AgentEvent::RewindPoints(list) => fx.push(Effect::RewindPoints(list)),
            AgentEvent::Status(s) => self.status = s,
            AgentEvent::ThinkingLevel(t) => self.status.thinking = t.as_str().into(),
            AgentEvent::PermissionMode(m) => self.status.permissions = m,
            AgentEvent::Todo(v) => self.todo = serde_json::from_value(v).unwrap_or_default(),
            AgentEvent::TurnEnd { aborted } => {
                self.pty = None;
                self.calls.clear();
                self.tr.end_turn(w);
                self.tools.clear();
                self.compacting = false;
                self.retry_wait = None;
                self.running = false;
                self.turn_started = None;
                let jobs_running = self.jobs.iter().any(|j| j.state == SubagentState::Running);
                if self.bell.on_turn_end(aborted, jobs_running) && self.bell_cfg.turn_end {
                    fx.push(Effect::Ring);
                }
                if aborted {
                    self.tr.commit(notice_lines("[aborted]"));
                }
                fx.push(Effect::TurnEnded { aborted });
            }
            AgentEvent::Queue(q) => self.queued = q,
            AgentEvent::QueuedDropped(queued) => fx.push(Effect::Restore { queued }),
            AgentEvent::QueuePopped(p) => fx.push(Effect::Popped(p)),
            AgentEvent::Exit
            | AgentEvent::TurnFailed
            | AgentEvent::ToolSkipped { .. }
            | AgentEvent::ChildEntry { .. } => {}
        }
        fx
    }

    pub(super) fn running_names(&self) -> Vec<String> {
        self.jobs.iter().filter(|j| j.state == SubagentState::Running).map(|j| j.name.clone()).collect()
    }

    /// Moves the inline marker over the running jobs by `by` places; past either end it is off.
    pub(super) fn focus_job(&mut self, by: i8) {
        let names = self.running_names();
        let slot = self.job_focus.as_ref().and_then(|f| names.iter().position(|n| n == f)).map_or(0, |i| i + 1);
        let next = (slot as isize + by as isize).rem_euclid(names.len() as isize + 1) as usize;
        self.job_focus = next.checked_sub(1).map(|i| names[i].clone());
    }

    /// What `Alt+K` cancels: the marked job, or the only running job. Otherwise the reason there is none.
    pub(super) fn cancel_target(&self) -> Result<String, &'static str> {
        let names = self.running_names();
        match (&self.job_focus, names.as_slice()) {
            (_, []) => Err("no jobs are running"),
            (Some(f), _) => Ok(f.clone()),
            (None, [only]) => Ok(only.clone()),
            (None, _) => Err("several jobs are running: Alt+↓ marks one, then Alt+K cancels it"),
        }
    }

    fn ring_if_attention(&mut self, fx: &mut Vec<Effect>) {
        if self.bell.on_attention() && self.bell_cfg.attention {
            fx.push(Effect::Ring);
        }
    }

    fn commit_tool_result(&mut self, name: &str, args: &Value, output: &ToolOutput, w: usize) {
        if let Some(mu_base::tool::ToolMeta::File(f)) = &output.meta {
            super::open::note_recent(&mut self.recent_files, &f.path);
        }
        let mut l = blocks::tool_result(name, args, output, self.expanded, w);
        // Breathing room: a blank row after every tool block, as text blocks have.
        l.push(vec![]);
        self.tr.commit(l);
    }

    /// The rows of what is happening now: open text, running tools, streaming calls and running
    /// jobs. Also returns where the pty's cursor is, when a pty is open.
    pub(super) fn live_lines(&self, w: usize, spin: char) -> (Vec<Line>, Option<(usize, usize)>) {
        let mut pty_cursor = None;
        let mut out: Vec<Line> = Vec::new();
        out.extend(self.tr.live_lines(w, spin));
        for t in &self.tools {
            if let Some(p) = self.pty.as_ref().filter(|p| p.id == t.id) {
                let (rows, cur) = p.screen();
                let (lines, c) = blocks::pty_live(&t.args, &rows, cur, t.started.elapsed().as_secs(), p.waiting, w);
                pty_cursor = c.map(|c| (out.len() + c.0, c.1));
                out.extend(lines);
                continue;
            }
            let mut tail: Vec<String> = t.tail.iter().cloned().collect();
            if !t.partial.is_empty() {
                tail.push(t.partial.clone());
            }
            let tail = &tail[tail.len().saturating_sub(TAIL_MAX)..];
            out.extend(blocks::tool_live(&t.name, &t.args, tail, spin, t.started.elapsed().as_secs(), w));
        }
        for c in &self.calls {
            out.extend(blocks::tool_streaming(&c.name, &c.args, spin, w));
        }
        for j in self.jobs.iter().filter(|j| j.state == SubagentState::Running) {
            let now = std::time::SystemTime::now();
            let mut rows = blocks::job_rows(
                std::slice::from_ref(j),
                now,
                spin,
                |j| j.session.as_ref().and_then(|p| self.child_costs.get(p)).copied(),
                !self.expanded,
            );
            if self.job_focus.as_deref() == Some(j.name.as_str()) {
                rows = rows.into_iter().flat_map(|r| blocks::focused_job(r, w)).collect();
            }
            out.extend(rows);
            if self.expanded {
                out.extend(blocks::job_details(j, now, self.job_out.get(&j.name).map(String::as_str), w));
            }
        }
        if self.compacting {
            out.push(styled(format!("{spin} Compacting context…"), |_| theme::get().fg(Role::Warn)));
        }
        if let Some((until, attempt, max)) = self.retry_wait.filter(|(u, ..)| *u > Instant::now()) {
            let left = mu_base::events::wait_text(until - Instant::now());
            let text = format!("{spin} Retrying in {left} (attempt {attempt}/{max}) · Esc cancels");
            out.push(styled(trunc(&text, w), |_| theme::get().fg(Role::Warn)));
        }
        (out, pty_cursor)
    }

    /// The todo list in a few rows above the composer: the counts, the task in progress and the next ones.
    pub(super) fn todo_hud(&self, w: usize) -> Vec<Line> {
        let all: Vec<&mu_tools::tools::todo::Task> = self.todo.iter().flat_map(|p| p.tasks.iter()).collect();
        if all.is_empty() {
            return vec![];
        }
        let open = |t: &mu_tools::tools::todo::Task| matches!(t.status, Status::Pending | Status::InProgress);
        let Some(phase) = self.todo.iter().find(|p| p.tasks.iter().any(open)) else { return vec![] };
        let done = all.iter().filter(|t| matches!(t.status, Status::Completed | Status::Abandoned)).count();
        let mut out = vec![styled(trunc(&format!("todo {done}/{} · {}", all.len(), phase.name), w), |_| dim())];
        for t in phase.tasks.iter().filter(|t| t.status == Status::InProgress).take(1) {
            out.push(styled(trunc(&format!("  ▸ {}", t.content), w), |_| theme::get().fg(Role::Warn)));
        }
        for t in phase.tasks.iter().filter(|t| t.status == Status::Pending).take(3) {
            out.push(styled(trunc(&format!("  ○ {}", t.content), w), |_| dim()));
        }
        out.truncate(6);
        out
    }
}

impl App {
    /// Streamed text and thinking go to the transcript; this applies one agent event to the
    /// view and carries out the effects it leaves.
    pub(super) fn on_agent(&mut self, e: AgentEvent) {
        self.screen.dirty = true;
        // A new session path or a job's progress can change what the subagents have spent.
        let spend = matches!(e, AgentEvent::Status(_) | AgentEvent::Jobs(_));
        let fx = self.agent.on_event(e, self.screen.renderer.size());
        for effect in fx {
            self.apply(effect);
        }
        self.close_stale_dialogs();
        self.open_next_dialog();
        if spend {
            self.poll_spend();
        }
        self.drop_finished_forwards();
    }

    /// A job's forwarded ports go when it stops: they name a process that is gone.
    fn drop_finished_forwards(&mut self) {
        let running: Vec<&str> =
            self.agent.jobs.iter().filter(|j| j.state == SubagentState::Running).map(|j| j.name.as_str()).collect();
        for job in self.forwards.stale(|j| running.contains(&j)) {
            let forwards = self.forwards.take_job(&job);
            self.unforward(forwards);
        }
    }

    /// The working directory changed: this view's copy, the host's copy and its context follow. The
    /// host reloads the context where it runs, so an attached client whose agent is on another
    /// machine asks that machine, and a directory this one cannot read only changes the display.
    fn on_cwd(&mut self, cwd: PathBuf) {
        if cwd == self.cwd {
            return;
        }
        self.prompt.composer.set_cwd(&cwd);
        self.cwd = cwd.clone();
        let quiet = self.agent.catching_up;
        self.host_call(SET_CWD, &cwd, move |app, r: Result<Switched, String>| match r {
            Ok(s) => {
                if s.applied {
                    app.refresh_commands();
                }
                if s.untrusted_mcp > 0 && !quiet {
                    app.notice("this directory's MCP servers are off until you trust them: /mcp trust");
                }
            }
            Err(e) if !quiet => app.error(&format!("cannot reload the context for the new directory: {e}")),
            Err(_) => {}
        });
    }

    /// Asks the host for what the subagents have spent; it reads their session files when that is
    /// due and answers with the total.
    pub(super) fn poll_spend(&mut self) {
        if self.agent.spend.asked {
            self.agent.spend.again = true;
            return;
        }
        let root = self.agent.status.session_path.as_deref().map(PathBuf::from);
        let children: Vec<Child> = self
            .agent
            .jobs
            .iter()
            .filter(|j| j.kind == JobKind::Subagent)
            .filter_map(|j| Some(Child { path: j.session.clone()?, running: j.state == SubagentState::Running }))
            .collect();
        self.agent.spend.asked = true;
        self.agent.spend.last = Some(Instant::now());
        self.host_call(sessions::TREE_COST, &TreeArgs { root, children }, |app, r: Result<Spend, String>| {
            app.agent.spend.asked = false;
            if let Ok(s) = r {
                app.agent.subagent_cost = s.total;
                app.agent.child_costs = s.costs.into_iter().collect();
            }
            // The jobs or the session may have moved on while the files were read.
            if std::mem::take(&mut app.agent.spend.again) {
                app.poll_spend();
            }
        });
    }

    /// Shows a finished subagent's row with what its session cost (`None` when that is unknown).
    fn commit_subagent(&mut self, row: SubagentStatus, cost: Option<f64>) {
        let lines = blocks::subagent_rows(std::slice::from_ref(&row), std::time::SystemTime::now(), ' ', |_| cost);
        self.agent.tr.commit(lines);
    }

    fn apply(&mut self, effect: Effect) {
        match effect {
            Effect::Ring => {
                if !self.agent.catching_up {
                    self.ring();
                }
            }
            Effect::ClearScreen => {
                let _ = self.screen.renderer.clear_all();
            }
            Effect::Cwd(p) => self.on_cwd(p),
            Effect::Ask { questions, reply } => self.start_ask(questions, reply),
            Effect::Approve { request, reply } => self.start_approve(request, reply),
            Effect::WebSetup { reply } => self.start_web_wizard(Some(reply)),
            Effect::Review { request, reply } => {
                self.set_aside_approval();
                let context = self
                    .agent
                    .status
                    .context_tokens
                    .filter(|_| self.agent.status.context_window > 0)
                    .map(|t| (t, self.agent.status.context_window as u64));
                self.with_plan_tiers(move |app, tiers| {
                    let review = crate::panels::doc_review::DocReview::new(request, context, tiers);
                    app.open_overlay(Overlay::Review { review: Box::new(review), reply: Some(reply) });
                });
            }
            Effect::PlanList(list) => self.show_plans(list),
            Effect::RewindPoints(list) => self.show_rewind(list),
            Effect::SubagentCost(row) => match row.session.clone() {
                Some(p) => self.host_call(sessions::SESSION_COST, &p, |app, r: Result<f64, String>| {
                    app.commit_subagent(row, r.ok())
                }),
                None => {
                    let _ = self.ui_tx.send(UiMsg::Host(Box::new(move |app| app.commit_subagent(row, None))));
                }
            },
            Effect::JobsChanged => {
                if let Some(Overlay::Jobs { st }) = &mut self.overlay {
                    st.set_jobs(self.agent.jobs.clone());
                }
            }
            Effect::JobOutput(id) => {
                if let (Some(Overlay::Jobs { st }), Some(tail)) = (&mut self.overlay, self.agent.job_out.get(&id)) {
                    st.set_output(&id, tail);
                }
            }
            Effect::TurnEnded { aborted } => {
                self.agent.turn_aborted = aborted;
                // A cancelled turn drops its approval: close a dialog nobody will answer.
                if matches!(&self.overlay, Some(Overlay::Approve(a)) if a.reply_closed()) {
                    self.overlay = None;
                }
                // Prompts whose deadline passed during the turn go out now.
                self.deliver_scheduled();
            }
            Effect::Restore { queued } => {
                if !self.agent.turn_aborted {
                    self.notice("queued messages were not sent; they are back in the composer");
                }
                self.restore_queued(queued);
            }
            Effect::Popped(p) => self.restore_queued(vec![p]),
        }
    }
}

/// Newest entries win once the list holds `BASH_CMDS_MAX`.
fn push_capped<T>(v: &mut Vec<T>, item: T) {
    if v.len() == BASH_CMDS_MAX {
        v.remove(0);
    }
    v.push(item);
}

/// Lines of a notice: dim.
pub(super) fn notice_lines(s: &str) -> Vec<Line> {
    s.lines().map(|l| styled(l.to_string(), |_| dim())).collect()
}

/// Lines of an error: the error colour.
pub(super) fn error_lines(s: &str) -> Vec<Line> {
    s.lines().map(|l| styled(l.to_string(), |_| theme::get().fg(Role::Err))).collect()
}

#[cfg(test)]
mod tests {
    use super::super::keys::tests::test_app;
    use super::*;
    use crate::text::line::line_text;
    use mu_base::events::{JobKind, StatusSnapshot};
    use mu_base::permissions::ApprovalScope;
    use mu_base::tool::ToolMeta;
    use std::time::{Duration, SystemTime};

    const SIZE: (usize, usize) = (80, 24);

    fn view() -> AgentView {
        AgentView::new(true, false)
    }

    fn pending(v: &AgentView) -> String {
        v.tr.pending.iter().map(line_text).collect::<Vec<_>>().join("\n")
    }

    fn job(name: &str, state: SubagentState, kind: JobKind) -> SubagentStatus {
        SubagentStatus {
            name: name.into(),
            agent: "task".into(),
            tools: 0,
            last: String::new(),
            state,
            kind,
            cwd: PathBuf::from("/"),
            task: "do it".into(),
            started: SystemTime::now(),
            ended: None,
            delivered: false,
            note: None,
            session: None,
            model: None,
            changes: None,
            progress: None,
            ports: Vec::new(),
        }
    }

    fn output(text: &str) -> ToolOutput {
        ToolOutput::ok(text)
    }

    #[test]
    fn text_streams_into_the_open_block() {
        let mut v = view();
        assert!(v.on_event(AgentEvent::Text("Hello".into()), SIZE).is_empty());
        let open: Vec<String> = v.tr.live_lines(80, ' ').iter().map(line_text).collect();
        assert!(open.iter().any(|l| l.contains("Hello")), "{open:?}");
        v.on_event(AgentEvent::Notice("done".into()), SIZE);
        assert!(pending(&v).contains("done"));
    }

    #[test]
    fn thinking_is_active_until_the_turn_ends() {
        let mut v = view();
        v.on_event(AgentEvent::Thinking("pondering".into()), SIZE);
        assert!(v.tr.thinking_active());
        v.on_event(AgentEvent::TurnEnd { aborted: false }, SIZE);
        assert!(!v.tr.thinking_active());
    }

    #[test]
    fn a_tool_runs_from_start_through_output_to_end() {
        let mut v = view();
        v.on_event(
            AgentEvent::ToolStart { id: "t1".into(), name: "bash".into(), args: json!({"command": "ls -l"}) },
            SIZE,
        );
        assert_eq!(v.bash_cmds, ["ls -l"]);
        assert_eq!(v.tools.len(), 1);
        v.on_event(AgentEvent::ToolOutput { id: "t1".into(), chunk: "one\ntwo\npart".into() }, SIZE);
        let t = &v.tools[0];
        assert_eq!(t.tail.iter().map(String::as_str).collect::<Vec<_>>(), ["one", "two"]);
        assert_eq!(t.partial, "part");
        v.on_event(AgentEvent::ToolEnd { id: "t1".into(), name: "bash".into(), output: output("ok") }, SIZE);
        assert!(v.tools.is_empty());
        assert!(!pending(&v).is_empty());
    }

    #[test]
    fn a_foreground_call_prints_its_own_result_even_while_jobs_run() {
        use mu_base::tool::BashMeta;

        let mut v = view();
        v.on_event(AgentEvent::JobOutput { id: "bg_1".into(), tail: "job output".into() }, SIZE);
        v.on_event(AgentEvent::Jobs(vec![job("bg_1", SubagentState::Done, JobKind::Bash)]), SIZE);
        assert!(pending(&v).contains("job output"));
        v.on_event(
            AgentEvent::ToolStart { id: "t1".into(), name: "bash".into(), args: json!({"command": "ls"}) },
            SIZE,
        );
        let mut out = output("own output");
        out.meta = Some(ToolMeta::Bash(BashMeta { output_end: "own output".len(), ..Default::default() }));
        v.on_event(AgentEvent::ToolEnd { id: "t1".into(), name: "bash".into(), output: out }, SIZE);
        assert!(v.tools.is_empty());
        assert!(pending(&v).contains("own output"), "a foreground result is never left to a job card");
    }

    #[test]
    fn a_resumed_completed_foreground_result_keeps_its_full_body_and_footer() {
        use mu_base::message::{Message, history_events};
        use serde::Deserialize;

        let mut out = ToolOutput::ok("the output\nexit 3");
        out.meta = Some(ToolMeta::Bash(mu_base::tool::BashMeta {
            output_end: "the output\n".len(),
            exit_code: Some(3),
            artifact: Some(2),
            ..Default::default()
        }));
        let stored = out.stored_details();
        let back = ToolOutput::from_stored(out.text.clone(), out.is_error, stored.clone());
        assert_eq!(back.meta, out.meta, "the metadata survives storage");

        let call = Message::deserialize(json!({"role":"assistant","content":[{"type":"toolCall",
            "id":"t1","name":"bash","arguments":{"command":"make"}}],"api":"a","provider":"p","model":"m",
            "usage":{"input":1,"output":1,"cacheRead":0,"cacheWrite":0,"totalTokens":2,"cost":{"total":0.0}},
            "stopReason":"toolUse","timestamp":2}))
        .unwrap();
        let result = Message::deserialize(json!({"role":"toolResult","toolCallId":"t1","toolName":"bash",
            "content":[{"type":"text","text":back.text}],"isError":false,"timestamp":3,
            "details":stored}))
        .unwrap();
        let mut v = view();
        for e in history_events(&[call, result]) {
            v.on_event(e, SIZE);
        }
        let shown = pending(&v);
        assert!(shown.contains("the output"), "{shown}");
        assert!(shown.contains("artifact://2") && shown.contains("Exit: 3"), "{shown}");
    }

    #[test]
    fn a_streaming_call_is_dropped_when_its_tool_starts() {
        let mut v = view();
        v.on_event(AgentEvent::ToolCallStart { id: "c1".into(), name: "write".into() }, SIZE);
        v.on_event(AgentEvent::ToolCallDelta { id: "c1".into(), json: "{\"path\"".into() }, SIZE);
        assert!(v.calls[0].stale);
        v.on_event(AgentEvent::ToolStart { id: "c1".into(), name: "write".into(), args: json!({}) }, SIZE);
        assert!(v.calls.is_empty());
    }

    #[test]
    fn todo_updates_replace_the_list() {
        use mu_tools::tools::todo::Task;
        let phase = Phase {
            name: "Build".into(),
            tasks: vec![Task { content: "compile".into(), status: Status::InProgress, blocker: None }],
        };
        let mut v = view();
        v.on_event(AgentEvent::Todo(serde_json::to_value(vec![phase]).unwrap()), SIZE);
        assert_eq!(v.todo.len(), 1);
        assert_eq!(v.todo[0].tasks.len(), 1);
        assert!(v.todo_hud(80).iter().any(|l| line_text(l).contains("compile")));
        v.on_event(AgentEvent::Todo(json!([])), SIZE);
        assert!(v.todo.is_empty());
    }

    #[test]
    fn a_turn_tracks_running_and_reports_its_end() {
        let mut v = view();
        v.on_event(AgentEvent::TurnStart, SIZE);
        assert!(v.running && v.turn_started.is_some());
        let fx = v.on_event(AgentEvent::TurnEnd { aborted: false }, SIZE);
        assert!(!v.running && v.turn_started.is_none());
        assert!(matches!(fx.as_slice(), [Effect::Ring, Effect::TurnEnded { aborted: false }]));
        assert!(!pending(&v).contains("[aborted]"));
    }

    #[test]
    fn an_aborted_turn_says_so() {
        let mut v = view();
        v.on_event(AgentEvent::TurnStart, SIZE);
        let fx = v.on_event(AgentEvent::TurnEnd { aborted: true }, SIZE);
        assert!(pending(&v).contains("[aborted]"));
        assert!(matches!(fx.as_slice(), [Effect::TurnEnded { aborted: true }]));
    }

    #[test]
    fn errors_and_retries_are_printed() {
        let mut v = view();
        v.calls.push(StreamingCall {
            id: "c".into(),
            name: "x".into(),
            json: Default::default(),
            args: json!({}),
            stale: false,
        });
        v.on_event(AgentEvent::Error("boom".into()), SIZE);
        assert!(pending(&v).contains("boom"));
        v.on_event(
            AgentEvent::Retry { attempt: 2, max_attempts: 7, delay: Duration::from_secs(4), reason: "timeout".into() },
            SIZE,
        );
        assert!(pending(&v).contains("retrying in 4s (timeout) [attempt 2/7]"));
        assert!(v.calls.is_empty());
    }

    #[test]
    fn a_long_retry_wait_counts_down_until_it_ends() {
        let mut v = view();
        let retry = |attempt, secs| AgentEvent::Retry {
            attempt,
            max_attempts: 3,
            delay: Duration::from_secs(secs),
            reason: "429".into(),
        };
        v.on_event(retry(1, 3600), SIZE);
        assert!(pending(&v).contains("retrying in 1h"));
        let rows = |v: &AgentView| v.live_lines(80, '*').0.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(rows(&v).contains("Retrying in 59m") && rows(&v).contains("(attempt 1/3)"), "{}", rows(&v));
        assert!(rows(&v).contains("Esc cancels"), "{}", rows(&v));
        v.on_event(AgentEvent::TurnEnd { aborted: true }, SIZE);
        assert!(!rows(&v).contains("Retrying"));
        // A short wait only gets the notice.
        v.on_event(retry(1, 4), SIZE);
        assert!(!rows(&v).contains("Retrying"));
    }

    #[test]
    fn compaction_is_reported_and_ends() {
        let mut v = view();
        v.on_event(AgentEvent::Compacting, SIZE);
        assert!(v.compacting && v.busy());
        v.on_event(AgentEvent::Compacted { before: 100, after: 40 }, SIZE);
        assert!(!v.compacting);
        assert!(pending(&v).contains("compacted 100→40 tokens"));
    }

    #[test]
    fn a_finished_subagent_with_a_session_waits_for_its_cost_before_it_is_printed() {
        let mut v = view();
        let mut done = job("j1", SubagentState::Done, JobKind::Subagent);
        done.session = Some(PathBuf::from("/x/s.jsonl"));
        let fx = v.on_event(AgentEvent::Jobs(vec![done]), SIZE);
        assert!(matches!(fx.as_slice(), [Effect::SubagentCost(r), Effect::JobsChanged] if r.name == "j1"));
        assert!(v.tr.pending.is_empty(), "no file is read here; the app commits the row with the cost");
    }

    #[test]
    fn a_finished_job_is_printed_once_until_the_list_empties() {
        let mut v = view();
        let done = vec![job("j1", SubagentState::Done, JobKind::Subagent)];
        let fx = v.on_event(AgentEvent::Jobs(done.clone()), SIZE);
        assert!(matches!(fx.as_slice(), [Effect::JobsChanged]));
        let once = v.tr.pending.len();
        assert!(once > 0);
        v.on_event(AgentEvent::Jobs(done.clone()), SIZE);
        assert_eq!(v.tr.pending.len(), once, "the same finished job is not printed again");
        v.on_event(AgentEvent::Jobs(vec![]), SIZE);
        v.on_event(AgentEvent::Jobs(done), SIZE);
        assert_eq!(v.tr.pending.len(), 2 * once, "after an empty snapshot it is printed again");
    }

    #[test]
    fn a_finished_bash_job_is_printed_as_its_output_and_run_time() {
        let mut v = view();
        let mut done = job("bg_1", SubagentState::Done, JobKind::Bash);
        done.task = "cargo build".into();
        done.ended = Some(done.started);
        done.note = Some("exit 0".into());
        v.on_event(AgentEvent::Jobs(vec![job("bg_1", SubagentState::Running, JobKind::Bash)]), SIZE);
        v.on_event(AgentEvent::JobOutput { id: "bg_1".into(), tail: "Compiling mu\n".into() }, SIZE);
        assert!(v.tr.pending.is_empty(), "a running job only gets a live row");
        v.on_event(AgentEvent::Jobs(vec![done]), SIZE);
        let printed: String = v.tr.pending.iter().flat_map(|l| l.iter()).map(|s| s.text.clone()).collect();
        assert!(printed.contains("$ cargo build"), "{printed}");
        assert!(printed.contains("Compiling mu"), "the job's output is shown: {printed}");
        assert!(printed.contains("Wall: 0.0s"), "{printed}");
    }

    #[test]
    fn a_running_job_is_listed_not_printed() {
        let mut v = view();
        v.on_event(AgentEvent::Jobs(vec![job("j2", SubagentState::Running, JobKind::Subagent)]), SIZE);
        assert!(v.tr.pending.is_empty());
        assert!(v.busy(), "a running job keeps the screen busy");
        assert!(v.live_lines(80, ' ').0.iter().any(|l| line_text(l).contains("j2")));
    }

    #[test]
    fn expanding_adds_detail_rows_under_a_running_job() {
        let mut v = view();
        v.on_event(AgentEvent::Jobs(vec![job("b1", SubagentState::Running, JobKind::Bash)]), SIZE);
        v.on_event(AgentEvent::JobOutput { id: "b1".into(), tail: "compiling\n".into() }, SIZE);
        let rows = |v: &AgentView| v.live_lines(80, ' ').0.len();
        let collapsed = rows(&v);
        v.expanded = true;
        assert!(rows(&v) > collapsed);
        assert!(v.live_lines(80, ' ').0.iter().any(|l| line_text(l).contains("compiling")));
    }

    #[test]
    fn job_output_is_kept_and_pruned_with_its_job() {
        let mut v = view();
        v.on_event(AgentEvent::Jobs(vec![job("b1", SubagentState::Running, JobKind::Bash)]), SIZE);
        let fx = v.on_event(AgentEvent::JobOutput { id: "b1".into(), tail: "line".into() }, SIZE);
        assert_eq!(v.job_out.get("b1").map(String::as_str), Some("line"));
        assert!(matches!(fx.as_slice(), [Effect::JobOutput(id)] if id == "b1"));
        v.on_event(AgentEvent::Jobs(vec![]), SIZE);
        assert!(v.job_out.is_empty());
    }

    fn running_jobs(v: &mut AgentView, names: &[&str]) {
        let jobs = names.iter().map(|n| job(n, SubagentState::Running, JobKind::Bash)).collect();
        v.on_event(AgentEvent::Jobs(jobs), SIZE);
    }

    #[test]
    fn the_marker_steps_over_running_jobs_and_wraps_through_off() {
        let mut v = view();
        running_jobs(&mut v, &["a", "b"]);
        v.focus_job(1);
        assert_eq!(v.job_focus.as_deref(), Some("a"));
        v.focus_job(1);
        assert_eq!(v.job_focus.as_deref(), Some("b"));
        v.focus_job(1);
        assert_eq!(v.job_focus, None, "past the last job it is off");
        v.focus_job(-1);
        assert_eq!(v.job_focus.as_deref(), Some("b"), "up from off lands on the last job");
        let rows = v.live_lines(80, ' ').0.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(rows.contains("▸") && rows.contains("k cancel"), "{rows}");
        v.on_event(AgentEvent::Jobs(vec![job("a", SubagentState::Running, JobKind::Bash)]), SIZE);
        assert_eq!(v.job_focus, None, "a job that ended loses the marker");
        v.focus_job(1);
        v.focus_job(1);
        assert_eq!(v.job_focus, None, "one job: on, then off");
    }

    #[test]
    fn alt_k_targets_the_marked_job_else_the_only_one() {
        let mut v = view();
        assert!(v.cancel_target().is_err(), "nothing runs");
        running_jobs(&mut v, &["a"]);
        assert_eq!(v.cancel_target(), Ok("a".into()));
        running_jobs(&mut v, &["a", "b"]);
        assert!(v.cancel_target().is_err(), "two jobs and no marker: say how to choose");
        v.focus_job(1);
        v.focus_job(1);
        assert_eq!(v.cancel_target(), Ok("b".into()));
    }

    #[test]
    fn delivered_jobs_are_announced() {
        let mut v = view();
        v.on_event(AgentEvent::JobsDelivered(vec!["j1".into(), "j2".into()]), SIZE);
        assert!(pending(&v).contains("job results delivered: j1, j2"));
    }

    #[test]
    fn a_question_rings_once_whoever_started_the_turn() {
        let ask = || {
            let (tx, _rx) = oneshot::channel::<Option<Vec<Answer>>>();
            AgentEvent::Ask { questions: vec![], reply: tx }
        };
        let mut v = view();
        let fx = v.on_event(ask(), SIZE);
        assert!(matches!(fx.as_slice(), [Effect::Ring, Effect::Ask { .. }]));
        let fx = v.on_event(ask(), SIZE);
        assert!(matches!(fx.as_slice(), [Effect::Ask { .. }]), "one ring per turn");

        let mut v = view();
        v.on_event(AgentEvent::Woken, SIZE);
        let fx = v.on_event(ask(), SIZE);
        assert!(matches!(fx.as_slice(), [Effect::Ring, Effect::Ask { .. }]));
    }

    #[test]
    fn a_prompt_between_turns_does_not_silence_the_next_turn() {
        let (tx, _rx) = oneshot::channel::<Option<Vec<Answer>>>();
        let mut v = view();
        let fx = v.on_event(AgentEvent::Ask { questions: vec![], reply: tx }, SIZE);
        assert!(matches!(fx.as_slice(), [Effect::Ring, Effect::Ask { .. }]), "a job's question still rings");
        v.on_event(AgentEvent::TurnStart, SIZE);
        let fx = v.on_event(AgentEvent::TurnEnd { aborted: false }, SIZE);
        assert!(matches!(fx.as_slice(), [Effect::Ring, Effect::TurnEnded { aborted: false }]));
    }

    #[test]
    fn a_turn_that_asked_does_not_ring_again_at_the_end() {
        let (tx, _rx) = oneshot::channel::<Option<Vec<Answer>>>();
        let mut v = view();
        v.on_event(AgentEvent::TurnStart, SIZE);
        v.on_event(AgentEvent::Ask { questions: vec![], reply: tx }, SIZE);
        let fx = v.on_event(AgentEvent::TurnEnd { aborted: false }, SIZE);
        assert!(matches!(fx.as_slice(), [Effect::TurnEnded { aborted: false }]));
    }

    #[test]
    fn a_web_setup_request_becomes_an_effect() {
        let (tx, _rx) = oneshot::channel::<bool>();
        let mut v = view();
        let fx = v.on_event(AgentEvent::WebSetup { reply: tx }, SIZE);
        assert!(matches!(fx.as_slice(), [Effect::Ring, Effect::WebSetup { .. }]));
    }

    #[test]
    fn a_woken_turn_does_not_ring_when_it_ends() {
        let mut v = view();
        v.on_event(AgentEvent::Woken, SIZE);
        v.on_event(AgentEvent::TurnStart, SIZE);
        let fx = v.on_event(AgentEvent::TurnEnd { aborted: false }, SIZE);
        assert!(matches!(fx.as_slice(), [Effect::TurnEnded { aborted: false }]));
    }

    #[test]
    fn a_user_turn_rings_at_the_end_only_when_no_job_runs() {
        let mut v = view();
        v.on_event(AgentEvent::TurnStart, SIZE);
        let fx = v.on_event(AgentEvent::TurnEnd { aborted: false }, SIZE);
        assert!(matches!(fx.as_slice(), [Effect::Ring, Effect::TurnEnded { aborted: false }]));

        v.on_event(AgentEvent::Jobs(vec![job("b1", SubagentState::Running, JobKind::Bash)]), SIZE);
        v.on_event(AgentEvent::TurnStart, SIZE);
        let fx = v.on_event(AgentEvent::TurnEnd { aborted: false }, SIZE);
        assert!(matches!(fx.as_slice(), [Effect::TurnEnded { aborted: false }]));
    }

    #[test]
    fn plan_lists_and_status_updates_are_handed_over_or_kept() {
        let mut v = view();
        let fx = v.on_event(AgentEvent::PlanList(vec![]), SIZE);
        assert!(matches!(fx.as_slice(), [Effect::PlanList(l)] if l.is_empty()));
        let fx = v.on_event(AgentEvent::Status(StatusSnapshot { context_window: 9, ..Default::default() }), SIZE);
        assert!(fx.is_empty());
        assert_eq!(v.status.context_window, 9);
    }

    #[test]
    fn live_thinking_level_updates_status_without_resetting_other_fields() {
        let mut v = view();
        v.on_event(
            AgentEvent::Status(StatusSnapshot { thinking: "medium".into(), context_window: 9, ..Default::default() }),
            SIZE,
        );
        v.on_event(AgentEvent::TurnStart, SIZE);
        let fx = v.on_event(AgentEvent::ThinkingLevel(mu_base::models::Thinking::High), SIZE);
        assert!(fx.is_empty());
        assert!(v.running);
        assert_eq!(v.status.thinking, "high");
        assert_eq!(v.status.context_window, 9);
    }

    #[test]
    fn rewind_points_are_handed_over_and_a_taken_back_prompt_goes_to_the_composer() {
        let mut v = view();
        let points = vec![RewindPoint { id: "a".into(), text: "first".into(), reply: false }];
        let fx = v.on_event(AgentEvent::RewindPoints(points.clone()), SIZE);
        assert!(matches!(fx.as_slice(), [Effect::RewindPoints(p)] if *p == points));
        let fx = v.on_event(AgentEvent::QueuePopped("first".into()), SIZE);
        assert!(matches!(fx.as_slice(), [Effect::Popped(p)] if p.text == "first"));
    }

    fn approval(tool: &str, body: &str) -> ApprovalRequest {
        ApprovalRequest {
            tool: tool.into(),
            call_id: "c1".into(),
            title: format!("{tool}: {body}"),
            reason: "ask mode: bash needs approval".into(),
            body: body.into(),
            from: None,
            grant: Some(mu_base::permissions::Grant {
                tool: tool.into(),
                patterns: vec!["touch *".into()],
                wider: vec![
                    mu_base::permissions::GrantWidth {
                        label: "its directory".into(),
                        patterns: vec!["touch dir/*".into()],
                    },
                    mu_base::permissions::GrantWidth { label: "everything".into(), patterns: vec!["touch /*".into()] },
                ],
            }),
            models: Vec::new(),
        }
    }

    fn open_approval(app: &mut App, req: ApprovalRequest) -> oneshot::Receiver<ApprovalDecision> {
        let (tx, rx) = oneshot::channel();
        app.on_agent(AgentEvent::Approve { request: req, reply: tx });
        rx
    }

    fn press(app: &mut App, code: crossterm::event::KeyCode) {
        app.overlay_key(mu_testutil::key(code));
    }

    #[test]
    fn an_approval_opens_and_y_enter_allows_once() {
        let mut app = test_app(std::env::temp_dir().join("mu-approve-history.jsonl"));
        let mut rx = open_approval(&mut app, approval("bash", "touch a.txt"));
        assert!(matches!(app.overlay, Some(Overlay::Approve(_))));
        press(&mut app, crossterm::event::KeyCode::Char('y'));
        press(&mut app, crossterm::event::KeyCode::Enter);
        assert!(app.overlay.is_none());
        assert!(matches!(rx.try_recv(), Ok(ApprovalDecision::Allow { scope: ApprovalScope::Once, note: None, .. })));
    }

    #[test]
    fn a_model_row_answers_yes_on_that_model_and_no_stays_last() {
        let mut app = test_app(std::env::temp_dir().join("mu-approve-history.jsonl"));
        let mut req = approval("enter_mode", "plan");
        req.grant = None;
        req.models = ["@smol", "@slow"]
            .map(|s| mu_base::permissions::ModelChoice {
                label: format!("Yes, on {s}"),
                detail: "a/b".into(),
                selector: s.into(),
            })
            .to_vec();
        let mut rx = open_approval(&mut app, req.clone());
        press(&mut app, crossterm::event::KeyCode::Char('3'));
        press(&mut app, crossterm::event::KeyCode::Enter);
        match rx.try_recv() {
            Ok(ApprovalDecision::Allow { scope: ApprovalScope::Once, model: Some(m), .. }) => assert_eq!(m, "@slow"),
            other => panic!("expected yes on @slow, got {other:?}"),
        }
        let mut rx = open_approval(&mut app, req.clone());
        press(&mut app, crossterm::event::KeyCode::Char('y'));
        press(&mut app, crossterm::event::KeyCode::Enter);
        assert!(
            matches!(rx.try_recv(), Ok(ApprovalDecision::Allow { model: None, .. })),
            "plain yes keeps the default"
        );
        let mut rx = open_approval(&mut app, req);
        press(&mut app, crossterm::event::KeyCode::Char('n'));
        press(&mut app, crossterm::event::KeyCode::Enter);
        assert!(matches!(rx.try_recv(), Ok(ApprovalDecision::Deny { note: None })));
    }

    #[test]
    fn a_width_then_a_scope_grants_the_directory() {
        let mut app = test_app(std::env::temp_dir().join("mu-approve-history.jsonl"));
        // Widths: once, the exact call, its directory, everything, then No.
        let mut rx = open_approval(&mut app, approval("bash", "touch a.txt"));
        press(&mut app, crossterm::event::KeyCode::Char('3'));
        press(&mut app, crossterm::event::KeyCode::Enter);
        assert!(matches!(app.overlay, Some(Overlay::Approve(_))), "the scope step comes next");
        press(&mut app, crossterm::event::KeyCode::Char('1'));
        press(&mut app, crossterm::event::KeyCode::Enter);
        match rx.try_recv() {
            Ok(ApprovalDecision::Allow { scope: ApprovalScope::Session, patterns: Some(p), .. }) => {
                assert_eq!(p, ["touch dir/*"])
            }
            other => panic!("expected the session's directory, got {other:?}"),
        }
    }

    #[test]
    fn a_typed_pattern_is_asked_for_and_grants_what_it_names() {
        let mut app = test_app(std::env::temp_dir().join("mu-approve-history.jsonl"));
        let mut rx = open_approval(&mut app, approval("bash", "touch a.txt"));
        press(&mut app, crossterm::event::KeyCode::Char('e'));
        press(&mut app, crossterm::event::KeyCode::Enter);
        press(&mut app, crossterm::event::KeyCode::Char('2'));
        press(&mut app, crossterm::event::KeyCode::Enter);
        match rx.try_recv() {
            Ok(ApprovalDecision::Allow { scope: ApprovalScope::Project, patterns: Some(p), .. }) => {
                assert_eq!(p, ["touch dir/*"], "the prefill is kept when it is submitted untouched")
            }
            other => panic!("expected the project scope for the typed pattern, got {other:?}"),
        }
    }

    #[test]
    fn esc_at_the_scope_step_goes_back_to_the_widths() {
        let mut app = test_app(std::env::temp_dir().join("mu-approve-history.jsonl"));
        let mut rx = open_approval(&mut app, approval("bash", "touch a.txt"));
        press(&mut app, crossterm::event::KeyCode::Char('3'));
        press(&mut app, crossterm::event::KeyCode::Enter);
        press(&mut app, crossterm::event::KeyCode::Esc);
        assert!(matches!(app.overlay, Some(Overlay::Approve(_))), "back on the widths, nothing answered");
        assert!(rx.try_recv().is_err());
        press(&mut app, crossterm::event::KeyCode::Esc);
        assert!(matches!(rx.try_recv(), Ok(ApprovalDecision::Deny { note: None })), "Esc on the widths denies");
    }

    #[test]
    fn esc_denies_without_a_note() {
        let mut app = test_app(std::env::temp_dir().join("mu-approve-history.jsonl"));
        let mut rx = open_approval(&mut app, approval("bash", "touch a.txt"));
        press(&mut app, crossterm::event::KeyCode::Esc);
        assert!(app.overlay.is_none());
        assert!(matches!(rx.try_recv(), Ok(ApprovalDecision::Deny { note: None })));
    }

    #[test]
    fn n_then_a_note_denies_with_the_reason() {
        let mut app = test_app(std::env::temp_dir().join("mu-approve-history.jsonl"));
        let mut rx = open_approval(&mut app, approval("write", "b.txt"));
        press(&mut app, crossterm::event::KeyCode::Char('n'));
        press(&mut app, crossterm::event::KeyCode::Tab);
        for c in "why".chars() {
            press(&mut app, crossterm::event::KeyCode::Char(c));
        }
        press(&mut app, crossterm::event::KeyCode::Enter);
        assert!(app.overlay.is_none());
        match rx.try_recv() {
            Ok(ApprovalDecision::Deny { note: Some(n) }) => assert_eq!(n, "why"),
            other => panic!("expected a deny with a note, got {other:?}"),
        }
    }

    #[test]
    fn a_second_approval_waits_until_the_first_is_answered() {
        let mut app = test_app(std::env::temp_dir().join("mu-approve-history.jsonl"));
        let mut first = open_approval(&mut app, approval("bash", "touch a.txt"));
        let mut second = open_approval(&mut app, approval("bash", "touch b.txt"));
        assert_eq!(app.approvals.len(), 1);
        press(&mut app, crossterm::event::KeyCode::Esc);
        assert!(matches!(app.overlay, Some(Overlay::Approve(_))), "the queued one takes over");
        assert!(matches!(first.try_recv(), Ok(ApprovalDecision::Deny { note: None })));
        press(&mut app, crossterm::event::KeyCode::Esc);
        assert!(app.overlay.is_none() && app.approvals.is_empty());
        assert!(matches!(second.try_recv(), Ok(ApprovalDecision::Deny { note: None })));
    }

    #[test]
    fn a_picker_opened_over_an_approval_waits_and_the_approval_is_not_answered() {
        use crate::widgets::picker::Picker;
        use crate::widgets::select::Select;
        let mut app = test_app(std::env::temp_dir().join("mu-approve-history.jsonl"));
        let mut rx = open_approval(&mut app, approval("bash", "touch a.txt"));
        let picker = || Overlay::Logout { pick: Picker::placeholder(vec![], |items| Select::new("t", items)) };
        app.open_overlay(picker());
        app.open_overlay(picker());
        assert!(matches!(app.overlay, Some(Overlay::Approve(_))), "the approval stays on screen");
        assert_eq!(app.parked.len(), 1, "one picker per kind is kept");
        assert!(rx.try_recv().is_err(), "the approval is unanswered");
        press(&mut app, crossterm::event::KeyCode::Esc);
        assert!(matches!(app.overlay, Some(Overlay::Logout { .. })), "the queued picker opens next");
        assert!(matches!(rx.try_recv(), Ok(ApprovalDecision::Deny { note: None })));
    }

    #[test]
    fn a_cwd_event_moves_the_app_and_reloads_the_hosts_context_with_either_host() {
        for deferred in [false, true] {
            let mut app = test_app(std::env::temp_dir().join("mu-cwd-history.jsonl"));
            let mut rx = with_host(&mut app, deferred);
            let dir = std::env::temp_dir().join(format!("mu-cwd-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).unwrap();
            super::super::keys::tests::in_local(async {
                app.on_agent(AgentEvent::Cwd(dir.clone()));
                assert_eq!(app.cwd, dir);
                assert!(app.prompt.commands.is_empty(), "the list is rebuilt once the context is loaded");
                // The switch's answer asks for the completion list; a host may answer that inline or as a second message.
                while app.prompt.commands.is_empty() {
                    let m = tokio::time::timeout(Duration::from_secs(10), rx.recv()).await.unwrap().unwrap();
                    app.on_ui(m);
                }
            });
            assert!(!app.prompt.commands.is_empty());
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn a_cwd_event_for_the_directory_already_held_asks_nothing_of_the_host() {
        let mut app = test_app(std::env::temp_dir().join("mu-cwd-same-history.jsonl"));
        let mut rx = with_host(&mut app, true);
        app.on_agent(AgentEvent::Cwd(app.cwd.clone()));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn cd_sends_the_directory_and_bare_cd_shows_the_current_one() {
        use super::super::keys::tests::{test_app_with_commands, transcript_text};
        use mu_base::events::Command;
        let (mut app, mut cmds) = test_app_with_commands(std::env::temp_dir().join("mu-cd-history.jsonl"));
        app.cwd = PathBuf::from("/work/here");
        app.submit("/cd /elsewhere".into(), false);
        assert!(matches!(cmds.try_recv(), Ok(Command::SetCwd(p)) if p == std::path::Path::new("/elsewhere")));
        app.submit("/cd sub".into(), false);
        assert!(matches!(cmds.try_recv(), Ok(Command::SetCwd(p)) if p == std::path::Path::new("/work/here/sub")));
        app.submit("/cd".into(), false);
        assert!(cmds.try_recv().is_err());
        assert!(transcript_text(&app).contains("/work/here"));
    }

    #[test]
    fn retry_sends_the_retry_command_and_echoes_nothing() {
        use super::super::keys::tests::{test_app_with_commands, transcript_text};
        use mu_base::events::Command;
        let (mut app, mut cmds) = test_app_with_commands(std::env::temp_dir().join("mu-retry-history.jsonl"));
        app.submit("/retry".into(), false);
        assert!(matches!(cmds.try_recv(), Ok(Command::Retry)));
        assert!(!app.agent.running, "the agent's turn start marks it running");
        assert!(!transcript_text(&app).contains("retry"), "the command leaves no line behind");
    }

    fn subagent_session(cost: f64) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mu-subcost-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        let path = d.join("2026-01-01T00-00-01-000Z_kid.jsonl");
        let reply = json!({"role":"assistant","content":[],"usage":{"input":1,"output":1,"cacheRead":0,
            "cacheWrite":0,"totalTokens":2,"cost":{"total":cost}},"stopReason":"stop","timestamp":1});
        let entry = json!({"type":"message","id":"a","parentId":null,"message":reply});
        std::fs::write(&path, format!("{{\"type\":\"session\",\"id\":\"kid\"}}\n{entry}\n")).unwrap();
        path
    }

    fn with_host(app: &mut App, deferred: bool) -> tokio::sync::mpsc::UnboundedReceiver<UiMsg> {
        use crate::host::{Deferred, local::LocalHost};
        use std::rc::Rc;
        let local = LocalHost::new(app.services.providers.clone(), app.ctx.clone(), app.plugins.clone(), "/".into());
        app.host = if deferred { Rc::new(Deferred(local)) } else { Rc::new(local) };
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        app.ui_tx = tx;
        rx
    }

    #[test]
    fn a_finished_subagents_cost_is_read_through_the_host_with_either_host() {
        for deferred in [false, true] {
            let mut app = test_app(std::env::temp_dir().join("mu-subcost-history.jsonl"));
            let mut rx = with_host(&mut app, deferred);
            let row = SubagentStatus {
                session: Some(subagent_session(0.25)),
                state: SubagentState::Done,
                ..job("scout", SubagentState::Done, JobKind::Subagent)
            };
            super::super::keys::tests::in_local(async {
                app.apply(Effect::SubagentCost(row));
                while !pending(&app.agent).contains("0.25") {
                    let m = tokio::time::timeout(Duration::from_secs(10), rx.recv()).await.unwrap().unwrap();
                    app.on_ui(m);
                }
            });
        }
    }

    #[test]
    fn the_subagent_spend_comes_back_through_the_host_and_one_read_is_out_at_a_time() {
        use mu_base::events::AgentEvent;
        let kid = subagent_session(0.25);
        let root = kid.with_file_name("2026-01-01T00-00-00-000Z_root.jsonl");
        std::fs::write(&root, "{\"type\":\"session\",\"id\":\"root\"}\n").unwrap();
        let mut app = test_app(std::env::temp_dir().join("mu-spend-history.jsonl"));
        let mut rx = with_host(&mut app, true);
        let child = SubagentStatus {
            session: Some(kid),
            state: SubagentState::Done,
            ..job("scout", SubagentState::Done, JobKind::Subagent)
        };
        super::super::keys::tests::in_local(async {
            let status =
                StatusSnapshot { session_path: Some(root.to_string_lossy().into_owned()), ..Default::default() };
            app.on_agent(AgentEvent::Status(status));
            assert!(app.agent.spend.asked, "the read is out");
            app.on_agent(AgentEvent::Jobs(vec![child]));
            while app.agent.subagent_cost == 0.0 {
                let m = tokio::time::timeout(Duration::from_secs(10), rx.recv()).await.unwrap().unwrap();
                app.on_ui(m);
            }
        });
        assert!((app.agent.subagent_cost - 0.25).abs() < 1e-9);
        assert!(!app.agent.spend.asked);
    }
}
