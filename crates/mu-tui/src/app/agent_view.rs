//! The agent's side of the screen: the transcript, running tools, streaming tool calls, background
//! jobs and the todo list. `AgentView::on_event` reduces agent events into it without an `App`;
//! what it cannot do alone (overlays, the inbox, the bell) comes back as an `Effect` for `App`.

use super::App;
use super::ui_msgs::UiMsg;
use crate::chat::blocks;
use crate::chat::partial_json::Partial;
use crate::chat::transcript::{Stream, Transcript};
use crate::panels::ptyview::PtyView;
use crate::term::bell::WokenBell;
use crate::text::line::{Line, dim, styled};
use crate::theme::{self, Role};
use crate::widgets::overlay::Overlay;
use mu_base::config::MuBell;
use mu_base::events::{
    AgentEvent, Answer, AskQuestion, PlanDecision, PlanMeta, QueueState, RewindPoint, StatusSnapshot,
};
use mu_base::events::{JobKind, SubagentState, SubagentStatus};
use mu_base::message::UserInput;
use mu_base::permissions::{ApprovalDecision, ApprovalRequest};
use mu_base::text::truncate_text as trunc;
use mu_base::tool::ToolOutput;
use mu_session::session::tree_cost::{Child, TreeCost, session_cost};
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
    Ask {
        questions: Vec<AskQuestion>,
        reply: oneshot::Sender<Option<Vec<Answer>>>,
    },
    /// A tool call needs the user's approval.
    Approve {
        request: ApprovalRequest,
        reply: oneshot::Sender<ApprovalDecision>,
    },
    PlanReview {
        title: String,
        uri: String,
        file: PathBuf,
        content: String,
        stored: bool,
        note: Option<String>,
        reply: oneshot::Sender<PlanDecision>,
    },
    /// `web_search` needs a backend: run the setup wizard and reply whether web search is ready.
    WebSetup {
        reply: oneshot::Sender<bool>,
    },
    PlanList(Vec<PlanMeta>),
    /// The prompts `/rewind` can go back to.
    RewindPoints(Vec<RewindPoint>),
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

pub(super) struct AgentView {
    /// The streamed conversation: committed lines, open text and thinking, smooth reveal.
    pub(super) tr: Transcript,
    pub(super) tools: Vec<RunningTool>,
    /// Tool calls whose arguments are still streaming or that are not yet executing.
    pub(super) calls: Vec<StreamingCall>,
    /// Latest background-job snapshot, and finished jobs already printed to the transcript.
    pub(super) jobs: Vec<SubagentStatus>,
    /// Reads the subagents' session files for the status line's cost. `None` while that read runs on
    /// a blocking thread, which hands it back in `UiMsg::Spend`.
    pub(super) spend: Option<TreeCost>,
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
    /// Embedded terminal of a running `bash` call with `pty: true`.
    pub(super) pty: Option<PtyView>,
    pub(super) running: bool,
    pub(super) compacting: bool,
    /// A retry wait long enough to count down in the viewport: its end and the attempt that failed.
    retry_wait: Option<(Instant, u32)>,
    pub(super) turn_started: Option<Instant>,
    pub(super) expanded: bool,
    pub(super) queued: QueueState,
    /// The last turn was aborted: input it leaves queued goes back to the composer without a notice.
    pub(super) turn_aborted: bool,
    bell: WokenBell,
    bell_cfg: MuBell,
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
            spend: Some(TreeCost::default()),
            subagent_cost: 0.0,
            child_costs: HashMap::new(),
            jobs_committed: HashSet::new(),
            job_out: HashMap::new(),
            job_focus: None,
            todo: Vec::new(),
            status: StatusSnapshot::default(),
            bash_cmds: Vec::new(),
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
                | AgentEvent::Jobs(_)
                | AgentEvent::Todo(_)
                | AgentEvent::TurnStart
                | AgentEvent::Retry { .. }
        ) {
            self.tr.flush_reveal(w);
        }
        let mut fx = Vec::new();
        match e {
            AgentEvent::TurnStart => {
                self.running = true;
                self.turn_started = Some(Instant::now());
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
            AgentEvent::Retry { attempt, delay, reason } => {
                self.tr.reset_stream();
                self.calls.clear();
                let msg = format!("↻ {}", mu_base::events::retry_notice(attempt, delay, &reason));
                self.tr.commit(notice_lines(&msg));
                self.retry_wait = (delay >= RETRY_COUNTDOWN_FROM).then(|| (Instant::now() + delay, attempt));
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
                        // Its file is final now, and the periodic read may not have seen the end of it.
                        let cost = r.session.as_deref().map(session_cost);
                        let lines = match r.kind {
                            // A bash job's result is its output, run time and end, like a foreground run's.
                            JobKind::Bash => {
                                let out = self.job_out.get(&r.name).map(String::as_str);
                                blocks::bash_job_result(r, out, self.expanded, w)
                            }
                            JobKind::Subagent => blocks::subagent_rows(std::slice::from_ref(r), ' ', |_| cost),
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
            AgentEvent::PlanReview { title, uri, file, content, stored, note, reply } => {
                self.ring_if_attention(&mut fx);
                fx.push(Effect::PlanReview { title, uri, file, content, stored, note, reply });
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
            AgentEvent::Exit | AgentEvent::TurnFailed | AgentEvent::ToolSkipped { .. } => {}
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
                let (lines, c) = blocks::pty_live(&t.args, &rows, cur, t.started.elapsed().as_secs(), w);
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
            let mut rows = blocks::subagent_rows(std::slice::from_ref(j), spin, |j| {
                j.session.as_ref().and_then(|p| self.child_costs.get(p)).copied()
            });
            if self.job_focus.as_deref() == Some(j.name.as_str()) {
                rows = rows.into_iter().flat_map(|r| blocks::focused_job(r, w)).collect();
            }
            out.extend(rows);
            if self.expanded {
                out.extend(blocks::job_details(j, self.job_out.get(&j.name).map(String::as_str), w));
            }
        }
        if self.compacting {
            out.push(styled(format!("{spin} Compacting context…"), |_| theme::get().fg(Role::Warn)));
        }
        if let Some((until, attempt)) = self.retry_wait.filter(|(u, _)| *u > Instant::now()) {
            let left = mu_base::events::wait_text(until - Instant::now());
            let text = format!("{spin} Retrying in {left} (attempt {attempt}/5) · Esc cancels");
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
        self.open_queued_approval();
        if spend {
            self.poll_spend();
        }
    }

    /// Starts reading the subagents' session files on a blocking thread when that is due; the total
    /// comes back in `UiMsg::Spend`, so no file I/O runs on the draw path.
    pub(super) fn poll_spend(&mut self) {
        let root = self.agent.status.session_path.as_deref().map(PathBuf::from);
        let children: Vec<Child> = self
            .agent
            .jobs
            .iter()
            .filter(|j| j.kind == JobKind::Subagent)
            .filter_map(|j| Some(Child { path: j.session.clone()?, running: j.state == SubagentState::Running }))
            .collect();
        let Some(mut tracker) = self.agent.spend.take_if(|t| t.due(root.as_deref(), &children)) else { return };
        let tx = self.ui_tx.clone();
        tokio::task::spawn_local(async move {
            let tracker = tokio::task::spawn_blocking(move || {
                tracker.poll(root.as_deref(), &children);
                tracker
            })
            .await
            .unwrap_or_default();
            let _ = tx.send(UiMsg::Spend(Box::new(tracker)));
        });
    }

    fn apply(&mut self, effect: Effect) {
        match effect {
            Effect::Ring => self.ring(),
            Effect::Ask { questions, reply } => self.start_ask(questions, reply),
            Effect::Approve { request, reply } => self.start_approve(request, reply),
            Effect::WebSetup { reply } => self.start_web_wizard(Some(reply)),
            Effect::PlanReview { title, uri, file, content, stored, note, reply } => {
                self.set_aside_approval();
                let context = self
                    .agent
                    .status
                    .context_tokens
                    .filter(|_| self.agent.status.context_window > 0)
                    .map(|t| (t, self.agent.status.context_window as u64));
                let tiers = self.plan_tiers();
                let review = crate::panels::plan_review::PlanReview::new(
                    title, uri, file, content, context, tiers, stored, note,
                );
                self.overlay = Some(Overlay::Review { review: Box::new(review), reply: Some(reply), stored });
            }
            Effect::PlanList(list) => self.show_plans(list),
            Effect::RewindPoints(list) => self.show_rewind(list),
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
        v.on_event(AgentEvent::Retry { attempt: 2, delay: Duration::from_secs(4), reason: "timeout".into() }, SIZE);
        assert!(pending(&v).contains("retrying in 4s (timeout) [attempt 2/5]"));
        assert!(v.calls.is_empty());
    }

    #[test]
    fn a_long_retry_wait_counts_down_until_it_ends() {
        let mut v = view();
        v.on_event(AgentEvent::Retry { attempt: 1, delay: Duration::from_secs(3600), reason: "429".into() }, SIZE);
        assert!(pending(&v).contains("retrying in 1h"));
        let rows = |v: &AgentView| v.live_lines(80, '*').0.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(rows(&v).contains("Retrying in 59m") && rows(&v).contains("Esc cancels"), "{}", rows(&v));
        v.on_event(AgentEvent::TurnEnd { aborted: true }, SIZE);
        assert!(!rows(&v).contains("Retrying"));
        // A short wait only gets the notice.
        v.on_event(AgentEvent::Retry { attempt: 1, delay: Duration::from_secs(4), reason: "x".into() }, SIZE);
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
    fn a_question_rings_only_in_a_turn_the_user_did_not_start() {
        let (tx, _rx) = oneshot::channel::<Option<Vec<Answer>>>();
        let mut v = view();
        let fx = v.on_event(AgentEvent::Ask { questions: vec![], reply: tx }, SIZE);
        assert!(matches!(fx.as_slice(), [Effect::Ask { .. }]));

        let (tx, _rx) = oneshot::channel::<Option<Vec<Answer>>>();
        let mut v = view();
        v.on_event(AgentEvent::Woken, SIZE);
        let fx = v.on_event(AgentEvent::Ask { questions: vec![], reply: tx }, SIZE);
        assert!(matches!(fx.as_slice(), [Effect::Ring, Effect::Ask { .. }]));
    }

    #[test]
    fn a_web_setup_request_becomes_an_effect() {
        let (tx, _rx) = oneshot::channel::<bool>();
        let mut v = view();
        let fx = v.on_event(AgentEvent::WebSetup { reply: tx }, SIZE);
        assert!(matches!(fx.as_slice(), [Effect::WebSetup { .. }]));
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
            grant: Some(mu_base::permissions::Grant { tool: tool.into(), patterns: vec!["touch *".into()] }),
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
        assert!(matches!(rx.try_recv(), Ok(ApprovalDecision::Allow { scope: ApprovalScope::Once, note: None })));
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
}
