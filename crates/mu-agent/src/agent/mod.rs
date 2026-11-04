mod compaction;
mod inbox;
#[cfg(test)]
mod loop_tests;
mod plan;
mod review;
mod session;
mod status;
mod title;
mod tool_exec;
mod turn;
use self::compaction::Outcome;
use crate::plans;
use crate::policy::ToolPolicy;
use crate::prompt::{self, PromptInput};
use mu_base::ask_nudge;
use mu_base::events::{AgentEvent, Command, EventTx, GitInfo, PlanDecision, PlanMeta, SharedInbox, StatusSnapshot};
use mu_base::git;
use mu_base::mem;
use mu_base::message::UserInput;
use mu_base::message::{Block, Message, StopReason, history_events, is_dismissal, now_ms, text_of, tool_calls};
use mu_base::models::{ModelSpec, Thinking};
use mu_base::secrets;
use mu_base::tool::{ToolDef, ToolMeta, ToolOutput};
use mu_provider::gateway::quota::QuotaTracker;
use mu_provider::model::cost::CostLedger;
use mu_provider::model::pricing;
use mu_provider::provider::client::Providers;
use mu_provider::provider::types::{ProviderError, Request, StreamEvent};
use mu_session::session::{CompactionState, SessionFile, TitleSource};
use mu_tools::context::{self, Context};
use mu_tools::jobs::{self, JobsHandle};
use mu_tools::plugin::host::{InitParams, PluginHost};
use mu_tools::tools::{ToolCtx, Tools};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::rc::Rc;

use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio_util::sync::CancellationToken;

pub struct Agent {
    pub providers: Rc<Providers>,
    pub plugins: Rc<PluginHost>,
    pub tools: Tools,
    pub session: Option<SessionFile>,
    pub messages: Vec<Message>,
    /// Parallel to `messages`.
    pub entry_ids: Vec<String>,
    pub model: ModelSpec,
    pub thinking: Thinking,
    /// Mirror of `thinking` that the request reads, so a level set mid-turn (or during retries)
    /// applies to the next attempt.
    live_thinking: Rc<std::cell::Cell<Thinking>>,
    pub title: Option<String>,
    pub events: EventTx,
    pub inbox: SharedInbox,
    pub cwd: PathBuf,
    pub ctx: Rc<Context>,
    pub subagent: bool,
    pub plugin_ctx: BTreeMap<String, String>,
    /// Built-in and plugin tool definitions, built on the first request: plugin schemas are not
    /// read at startup.
    base_defs: std::cell::OnceCell<Vec<ToolDef>>,
    /// Tool names hidden from the model (e.g. `ask` when headless).
    pub excluded: Vec<&'static str>,
    pub plan: Option<PlanState>,
    /// Only these tools are offered when set.
    pub allow: Option<Vec<String>>,
    /// Custom-agent instructions, appended verbatim to the system prompt.
    pub agent_prompt: Option<String>,
    pub parent_session: Option<PathBuf>,
    exec_plan: Option<ExecPlan>,
    /// The stored plan being revised in plan mode; `propose_plan` updates it in place.
    stored_plan: Option<String>,
    pub review_base: Option<crate::review::ReviewBase>,
    pub pin_model: bool,
    pub pin_thinking: bool,
    pub compaction: Option<CompactionState>,
    /// Index into `messages` from which assistant usage reflects the current context.
    pub fresh_from: usize,
    pub since_compaction: usize,
    pub title_requested: bool,
    /// Cancels an in-flight title request when the session ends.
    pub title_cancel: CancellationToken,
    pub title_tx: UnboundedSender<(String, String)>,
    title_rx: Option<UnboundedReceiver<(String, String)>>,
    resumed: bool,
    user_turn: u32,
    cost: CostLedger,
    /// `(inputs hash, tokens)` of the system prompt, see `system_tokens`.
    system_memo: std::cell::RefCell<Option<(u64, u64)>>,
    /// `((message count, last timestamp), tokens)`, see `messages_tokens`.
    messages_memo: std::cell::RefCell<Option<((usize, i64), u64)>>,
    git: Option<GitInfo>,
    quota: QuotaTracker,
    pricing_checked: bool,
    turn_msgs: Vec<Value>,
    jobs: Option<Rc<JobsHandle>>,
    wake_rx: Option<UnboundedReceiver<()>>,
    /// `-p --resume`: when the session cannot be opened, the run ends instead of starting a fresh one.
    pub exit_on_resume_failure: bool,
    wake_tx: Option<UnboundedSender<()>>,
    /// Interactive only: drain `mu notify` messages (the SIGUSR2 handler is installed).
    notify: bool,
    /// Inbox files that were handed over but could not be deleted (session write failed).
    notify_seen: HashSet<String>,
    /// Inbox files announced when the session was opened; they wait for the next prompt.
    notify_announced: HashSet<String>,
    deliver: DeliverFn,
}

pub struct PlanState {
    prev_model: Option<String>,
    prev_thinking: Thinking,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum PlanMode {
    /// A new session (the Execute decision).
    Fresh,
    /// This session, compacted first.
    Compact,
    /// This session as it is.
    Keep,
}

struct ExecPlan {
    content: String,
    title: String,
    /// `local://<slug>-plan.md`
    uri: String,
    mode: PlanMode,
    model: Option<String>,
    /// Tree snapshot at approval, the base for `/review`.
    base: Option<String>,
    approved_at: i64,
    /// The id of the plan in the store, when it has one.
    stored: Option<String>,
}

/// How a turn begins.
enum Start {
    User(UserInput),
    /// A message mu sends itself; the UI does not echo it.
    Synthetic(UserInput),
    /// Only delivers finished background jobs.
    Wake,
    /// Begins executing an approved plan.
    Plan(ExecPlan),
}

const PLAN_APPROVED: &str = include_str!("../prompts/plan-approved.md");
const PLAN_REFINE: &str = include_str!("../prompts/plan-refine.md");

/// The hidden prompt that starts executing an approved plan.
fn plan_prompt(p: &ExecPlan) -> String {
    let context = if p.mode != PlanMode::Fresh {
        "- History usable; the plan below authoritative if it conflicts with earlier exploration.\n"
    } else {
        ""
    };
    PLAN_APPROVED.replace("{{context}}", context).replace("{{path}}", &p.uri).replace("{{plan}}", p.content.trim_end())
}

const PLAN_COMPACT_FOCUS: &str = "The user approved an implementation plan; it will be supplied in full right after this summary. Preserve what implementing it needs: decisions, constraints, file paths, symbols, and findings from exploration. Omit the plan text itself.";

const PLAN_PROMPT: &str = include_str!("../prompts/plan-mode.md");

/// Starts a detached `mu deliver` for a plan, in `cwd`, for `session`. `Ok` is what it printed; `Err` is what to show.
pub type DeliverFn =
    Rc<dyn Fn(String, PathBuf, PathBuf) -> Pin<Box<dyn std::future::Future<Output = Result<String, String>>>>>;

pub struct AgentInit {
    pub providers: Rc<Providers>,
    pub plugins: Rc<PluginHost>,
    pub cwd: PathBuf,
    pub ctx: Rc<Context>,
    pub model: ModelSpec,
    pub thinking: Thinking,
    pub events: EventTx,
    pub inbox: SharedInbox,
    pub deliver: DeliverFn,
}

const SKIP_STOPPED: &str = "Skipped: the run was stopped so the user can take over";
const SKIP_ABORTED: &str = "Tool call aborted by user";
const SKIP_STEER: &str = "Skipped: the user sent a new message";
const SKIP_YIELDED: &str = "Skipped: the result was already submitted with `yield`";
const SKIP_TRUNCATED: &str = "Tool call not executed: output was truncated (max_tokens)";

/// How a loop pass ended.
enum Flow {
    Done,
    Aborted,
    Failed,
    /// A headless run was asked to stop: ended at a boundary, session paired and resumable.
    Stopped,
}

async fn recv_opt(rx: &mut Option<UnboundedReceiver<()>>) -> Option<()> {
    match rx {
        Some(r) => r.recv().await,
        None => std::future::pending().await,
    }
}

/// Runs `fut` (a turn) while still reading commands. Commands that are safe mid-turn (`KillJobs`)
/// take effect at once, so `/jobs kill` works while `wait` blocks; the rest queue in `backlog`.
async fn drive<F: std::future::Future<Output = ()>>(
    fut: F,
    rx: &mut UnboundedReceiver<Command>,
    backlog: &mut std::collections::VecDeque<Command>,
    jobs: Option<Rc<JobsHandle>>,
    events: EventTx,
    live: Rc<std::cell::Cell<Thinking>>,
) {
    tokio::pin!(fut);
    let mut open = true;
    loop {
        tokio::select! {
            () = &mut fut => return,
            c = rx.recv(), if open => match c {
                Some(Command::KillJobs(id)) => {
                    let n = jobs.as_ref().map_or(0, |j| j.kill(id.as_deref()));
                    let _ = events.send(AgentEvent::Notice(format!("killed {n} job(s)")));
                }
                Some(Command::SetThinking(t)) => {
                    // Takes effect on the next request (including retries); also queued so the
                    // session records it once the turn ends.
                    live.set(t);
                    backlog.push_back(Command::SetThinking(t));
                }
                Some(other) => backlog.push_back(other),
                None => open = false,
            },
        }
    }
}

impl Agent {
    pub fn new(i: AgentInit) -> Self {
        let tools = Tools::new(i.cwd.clone(), i.plugins.clone());
        tools.set_skills(i.ctx.skills.iter().map(|s| (s.name.clone(), s.dir.clone())).collect());
        let (title_tx, title_rx) = unbounded_channel();
        Self {
            tools,
            providers: i.providers,
            plugins: i.plugins,
            session: None,
            messages: Vec::new(),
            entry_ids: Vec::new(),
            model: i.model,
            thinking: i.thinking,
            live_thinking: Rc::new(std::cell::Cell::new(i.thinking)),
            title: None,
            events: i.events,
            inbox: i.inbox,
            cwd: i.cwd,
            ctx: i.ctx,
            subagent: false,
            plugin_ctx: BTreeMap::new(),
            base_defs: Default::default(),
            excluded: Vec::new(),
            plan: None,
            allow: None,
            agent_prompt: None,
            parent_session: None,
            exec_plan: None,
            stored_plan: None,
            review_base: None,
            pin_model: false,
            pin_thinking: false,
            compaction: None,
            fresh_from: 0,
            since_compaction: 0,
            title_requested: false,
            title_cancel: CancellationToken::new(),
            title_tx,
            title_rx: Some(title_rx),
            resumed: false,
            user_turn: 0,
            cost: CostLedger::default(),
            system_memo: Default::default(),
            messages_memo: Default::default(),
            git: None,
            quota: QuotaTracker::default(),
            pricing_checked: false,
            turn_msgs: Vec::new(),
            jobs: None,
            wake_rx: None,
            exit_on_resume_failure: false,
            wake_tx: None,
            notify: false,
            notify_seen: HashSet::new(),
            notify_announced: HashSet::new(),
            deliver: i.deliver,
        }
    }

    /// Interactive sessions only: `task` runs in the background and finished jobs wake the agent.
    pub fn enable_jobs(&mut self) {
        let (wake, wake_rx) = unbounded_channel();
        self.wake_tx = Some(wake.clone());
        let jobs = JobsHandle::new(wake, self.events.clone());
        self.tools.jobs = Some(jobs.clone());
        self.jobs = Some(jobs);
        self.wake_rx = Some(wake_rx);
    }

    /// Interactive sessions only: `mu notify` messages are delivered. Returns the sender the
    /// SIGUSR2 handler uses to wake the agent (the jobs wake channel), `None` without jobs.
    pub fn enable_notify(&mut self) -> Option<UnboundedSender<()>> {
        self.notify = true;
        self.wake_tx.clone()
    }

    pub fn emit(&self, e: AgentEvent) {
        let _ = self.events.send(e);
    }

    pub fn notice(&self, s: impl Into<String>) {
        self.emit(AgentEvent::Notice(s.into()));
    }

    pub fn error(&self, s: impl Into<String>) {
        self.emit(AgentEvent::Error(s.into()));
    }

    pub fn system(&self) -> String {
        let mcp = self.plugins.mcp_instructions();
        prompt::system_prompt(&PromptInput {
            model: &self.model.id,
            cwd: &self.cwd,
            ctx: &self.ctx,
            plan: self.plan.as_ref().map(|_| PLAN_PROMPT),
            subagent: self.subagent,
            ask: self.policy().permits("ask"),
            agent_prompt: self.agent_prompt.as_deref(),
            plugin_ctx: &self.plugin_ctx,
            mcp_instructions: mcp.as_deref(),
            hashline: mu_tools::tools::edit::is_hashline(),
        })
    }

    pub async fn run(mut self, mut rx: UnboundedReceiver<Command>) {
        self.quota.enabled = true;
        self.sync_model_state();
        let providers = self.providers.clone();
        tokio::task::spawn_local(async move {
            providers.refresh_direct_models(false).await;
        });
        self.refresh_external().await;
        self.emit_status();
        let mut title_rx = self.title_rx.take().expect("run once");
        let mut wake_rx = self.wake_rx.take();
        let mut quota_rx = self.quota.take_wake();
        let mut backlog: std::collections::VecDeque<Command> = Default::default();
        loop {
            let cmd = if let Some(c) = backlog.pop_front() {
                c
            } else {
                tokio::select! {
                c = rx.recv() => match c { Some(c) => c, None => break },
                () = tokio::time::sleep(self.quota.wait()) => {
                    self.tick_quota();
                    continue;
                }
                Some(()) = recv_opt(&mut quota_rx) => {
                    self.tick_quota();
                    self.emit_status();
                    continue;
                }
                Some((sid, t)) = title_rx.recv() => {
                    self.apply_title(&sid, t);
                    continue;
                }
                Some(()) = recv_opt(&mut wake_rx) => {
                    let (jobs, events, live) = (self.jobs.clone(), self.events.clone(), self.live_thinking.clone());
                    drive(self.turn(None), &mut rx, &mut backlog, jobs, events, live).await;
                    continue;
                }
                }
            };
            let (jobs, events, live) = (self.jobs.clone(), self.events.clone(), self.live_thinking.clone());
            match cmd {
                Command::Prompt(t) => drive(self.turn(Some(t)), &mut rx, &mut backlog, jobs, events, live).await,
                Command::EnsureSession => {
                    self.ensure_session().await;
                    self.emit_status();
                }
                Command::SetModel(sel) => self.set_model(&sel),
                Command::RefreshModel => {
                    self.refresh_model_meta();
                    self.emit_status();
                }
                Command::SetThinking(t) => self.set_thinking(t),
                Command::NewSession => {
                    self.end_session().await;
                    self.reset_conversation();
                    self.notice("new session");
                    self.emit_status();
                }
                Command::Resume(p) => {
                    // A `-p` run that asked for a session must not quietly run in another one.
                    if !self.load_session(&p).await && self.exit_on_resume_failure {
                        break;
                    }
                }
                Command::TogglePlan(prompt) => match (self.plan.is_some(), prompt) {
                    (false, p) => {
                        self.enter_plan().await;
                        if let Some(p) = p {
                            drive(self.turn(Some(p.into())), &mut rx, &mut backlog, jobs, events, live).await;
                        }
                    }
                    (true, Some(p)) => {
                        drive(self.turn(Some(p.into())), &mut rx, &mut backlog, jobs, events, live).await
                    }
                    (true, None) => self.exit_plan(),
                },
                Command::Compact(focus) => self.manual_compact(focus).await,
                Command::Review(focus) => drive(self.review(focus), &mut rx, &mut backlog, jobs, events, live).await,
                Command::Rename(t) => {
                    self.ensure_session().await;
                    if let Some(s) = self.session.as_mut() {
                        s.set_title(&t, TitleSource::User);
                    }
                    self.title = Some(t);
                    self.emit_status();
                }
                Command::Bash { cmd, include } => self.user_bash(cmd, include).await,
                Command::KillJobs(id) => {
                    let n = self.jobs.as_ref().map_or(0, |j| j.kill(id.as_deref()));
                    self.notice(format!("killed {n} job(s)"));
                }
                Command::RefreshTools => self.refresh_tool_defs(),
                Command::ListPlans => self.list_plans().await,
                Command::OpenPlan(id) => {
                    if let Some(s) = self.open_plan(&id).await {
                        drive(self.turn_from(s), &mut rx, &mut backlog, jobs, events, live).await;
                    }
                }
                Command::Deliver(id) => self.deliver(&id).await,
                Command::StartPlan { id, decision, anyway } => {
                    if let Some(s) = self.start_plan(&id, decision, anyway).await {
                        drive(self.turn_from(s), &mut rx, &mut backlog, jobs, events, live).await;
                    }
                }
                Command::RefinePlan { id, text } => {
                    if let Some(s) = self.refine_plan(&id, text).await {
                        drive(self.turn_from(s), &mut rx, &mut backlog, jobs, events, live).await;
                    }
                }
                Command::Exit => {
                    self.end_session().await;
                    break;
                }
            }
        }
        self.emit(AgentEvent::Exit);
    }
}
