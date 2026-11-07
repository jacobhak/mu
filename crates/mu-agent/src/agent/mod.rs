mod compaction;
mod state;
pub use state::RunConfig;
use state::{CompactionFlow, Costs, Mailbox, Memo, ModelState, PlanFlow, Ports, SharedInbox, TitleState, new_inbox};
use std::cell::Cell;
mod inbox;
use inbox::{inbox_cmd, report_dropped};
#[cfg(test)]
mod loop_tests;
#[cfg(test)]
mod permissions_tests;
mod plan;
#[cfg(test)]
mod queue_tests;
mod review;
mod rewind;
mod session;
mod status;
mod title;
mod tool_exec;
mod turn;
use self::compaction::{Outcome, estimate_tokens};
use crate::plans;
use crate::policy::ToolPolicy;
use crate::prompt::{self, PromptInput};
use mu_base::ask_nudge;
use mu_base::events::{AgentEvent, Command, EventTx, PlanDecision, PlanMeta, QueueState, StatusSnapshot};
use mu_base::git;
use mu_base::mem;
use mu_base::message::UserInput;
use mu_base::message::{Block, Message, StopReason, history_events, is_dismissal, now_ms, text_of, tool_calls};
use mu_base::models::{ModelSpec, Thinking};
use mu_base::permissions::PermissionMode;
use mu_base::secrets;
use mu_base::tool::{ToolDef, ToolMeta, ToolOutput};
use mu_provider::gateway::quota::QuotaTracker;
use mu_provider::model::cost::CostLedger;
use mu_provider::model::pricing;
use mu_provider::provider::client::Providers;
use mu_provider::provider::types::{ProviderError, Request, StreamEvent};
use mu_session::session::file::SessionFile;
use mu_session::session::format::TitleSource;
use mu_tools::context::{self, Context};
use mu_tools::jobs::{self, JobsHandle};
use mu_tools::permissions::Permissions;
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
    tools: Tools,
    cwd: PathBuf,
    ctx: Rc<Context>,
    session: Option<SessionFile>,
    pub(crate) messages: Vec<Message>,
    /// Parallel to `messages`.
    entry_ids: Vec<String>,
    ports: Ports,
    model: ModelState,
    title: TitleState,
    plan: PlanFlow,
    mail: Mailbox,
    run: RunConfig,
    costs: Costs,
    compaction: CompactionFlow,
    memo: Memo,
    plugin_ctx: BTreeMap<String, String>,
    resumed: bool,
    user_turn: u32,
    turn_msgs: Vec<Value>,
    /// Test seam: the executable `/review` runs as its child instead of this mu.
    #[cfg(test)]
    review_child: Option<PathBuf>,
}

pub struct PlanState {
    prev_model: Option<String>,
    prev_thinking: Thinking,
}

struct ExecPlan {
    content: String,
    title: String,
    /// `local://<slug>-plan.md`
    uri: String,
    decision: PlanDecision,
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

/// One pass of a turn: a prompt to run, or a wake-up that delivers what finished meanwhile.
enum Step {
    /// `synthetic`: a message mu sends itself, which the UI does not echo.
    Prompt {
        input: UserInput,
        synthetic: bool,
    },
    Wake,
}

const PLAN_APPROVED: &str = include_str!("../prompts/plan-approved.md");
const PLAN_REFINE: &str = include_str!("../prompts/plan-refine.md");

/// The hidden prompt that starts executing an approved plan.
fn plan_prompt(p: &ExecPlan) -> String {
    let context = if !matches!(p.decision, PlanDecision::Execute { .. }) {
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
    pub deliver: DeliverFn,
    pub run: RunConfig,
}

const SKIP_STOPPED: &str = "Skipped: the run was stopped so the user can take over";
const SKIP_ABORTED: &str = "Tool call aborted by user";
const SKIP_STEER: &str = "Skipped: the user sent a new message";
const SKIP_YIELDED: &str = "Skipped: the result was already submitted with `yield`";
const SKIP_TRUNCATED: &str = "Tool call not executed: output was truncated (max_tokens)";

/// Tools a headless front end never offers: nobody is there to answer `ask` or approve a plan.
pub const HEADLESS_EXCLUDED: [&str; 2] = ["ask", "propose_plan"];

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

/// Commands read while a turn runs; see `drive`.
struct Queue {
    rx: UnboundedReceiver<Command>,
    inbox: SharedInbox,
    backlog: std::collections::VecDeque<Command>,
    jobs: Option<Rc<JobsHandle>>,
    events: EventTx,
    live: Rc<std::cell::Cell<Thinking>>,
    perms: Rc<Permissions>,
}

/// Reports a new mode to the front end and the transcript.
fn announce_mode(events: &EventTx, m: PermissionMode) {
    let _ = events.send(AgentEvent::PermissionMode(m));
    let _ = events.send(AgentEvent::Notice(format!("permissions: {}", m.as_str())));
}

/// Runs `fut` (a turn) while still reading commands. Commands that are safe mid-turn (`KillJobs`)
/// take effect at once, so `/jobs kill` works mid-turn; the rest queue in `backlog`.
async fn drive<F: std::future::Future<Output = ()>>(fut: F, q: &mut Queue) {
    tokio::pin!(fut);
    let mut open = true;
    loop {
        tokio::select! {
            () = &mut fut => break,
            c = q.rx.recv(), if open => match c {
                Some(Command::KillJobs(id)) => {
                    let n = q.jobs.as_ref().map_or(0, |j| j.kill(id.as_deref()));
                    let _ = q.events.send(AgentEvent::Notice(format!("killed {n} job(s)")));
                }
                Some(Command::SetPermissionMode(m)) => {
                    // The gate checks per call, so this applies to the next call of this turn.
                    q.perms.set_mode(m);
                    announce_mode(&q.events, m);
                }
                Some(c @ (Command::SetThinking(_) | Command::CycleThinking)) => {
                    let t = match c {
                        Command::SetThinking(t) => t,
                        Command::CycleThinking => q.live.get().cycle(),
                        _ => unreachable!(),
                    };
                    // Takes effect on the next request (including retries); also queued so the
                    // session records it once the turn ends.
                    q.live.set(t);
                    let _ = q.events.send(AgentEvent::ThinkingLevel(t));
                    q.backlog.push_back(Command::SetThinking(t));
                }
                Some(other) => {
                    if let Err(c) = inbox_cmd(other, &q.inbox, &q.events) {
                        q.backlog.push_back(c);
                    }
                }
                None => open = false,
            },
        }
    }
    report_dropped(&q.inbox, &q.events);
}

impl Agent {
    pub fn new(i: AgentInit) -> Self {
        let mut tools = Tools::new(i.cwd.clone(), i.plugins.clone());
        tools.set_skills(i.ctx.skills.iter().map(|s| (s.name.clone(), s.dir.clone())).collect());
        tools.set_context_page(prompt::context_page(&i.ctx));
        // The flag wins over inheritance, which wins over the config.
        let permissions = Rc::new(Permissions::new(&i.providers.cfg.mu.permissions));
        if let Some(v) = &i.run.inherited_permissions {
            permissions.apply_inherited(v);
        }
        if let Some(m) = i.run.permission_mode {
            permissions.set_mode(m);
        }
        tools.permissions = permissions;
        let (title_tx, title_rx) = unbounded_channel();
        Self {
            tools,
            cwd: i.cwd,
            ctx: i.ctx,
            session: None,
            messages: Vec::new(),
            entry_ids: Vec::new(),
            ports: Ports {
                providers: i.providers,
                plugins: i.plugins,
                events: i.events,
                inbox: new_inbox(),
                deliver: i.deliver,
            },
            model: ModelState {
                model: i.model,
                thinking: i.thinking,
                live_thinking: Rc::new(Cell::new(i.thinking)),
                pricing_checked: false,
            },
            title: TitleState {
                text: None,
                requested: false,
                cancel: CancellationToken::new(),
                tx: title_tx,
                rx: Some(title_rx),
            },
            plan: PlanFlow { plan: None, exec_plan: None, stored_plan: None, review_base: None },
            mail: Mailbox {
                wake_rx: None,
                wake_tx: None,
                notify: false,
                seen: HashSet::new(),
                announced: HashSet::new(),
            },
            run: i.run,
            costs: Costs { cost: CostLedger::default(), quota: QuotaTracker::default(), git: None },
            compaction: CompactionFlow { state: None, fresh_from: 0, since: 0 },
            memo: Memo::default(),
            plugin_ctx: BTreeMap::new(),
            resumed: false,
            user_turn: 0,
            turn_msgs: Vec::new(),
            #[cfg(test)]
            review_child: None,
        }
    }

    /// Interactive sessions only: `task` runs in the background and finished jobs wake the agent.
    pub(crate) fn enable_jobs(&mut self) {
        let (wake, wake_rx) = unbounded_channel();
        self.mail.wake_tx = Some(wake.clone());
        let jobs = JobsHandle::new(wake, self.ports.events.clone());
        self.tools.jobs = Some(jobs);
        self.mail.wake_rx = Some(wake_rx);
    }

    /// Interactive sessions only: `mu notify` messages are delivered. Returns the sender the
    /// SIGUSR2 handler uses to wake the agent (the jobs wake channel), `None` without jobs.
    pub(crate) fn enable_notify(&mut self) -> Option<UnboundedSender<()>> {
        self.mail.notify = true;
        self.mail.wake_tx.clone()
    }

    /// Structured-output schema for `yield`; set by a front end before the run starts.
    pub fn set_output_schema(&self, schema: Option<serde_json::Value>) {
        *self.tools.output_schema.borrow_mut() = schema;
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn providers(&self) -> Rc<Providers> {
        self.ports.providers.clone()
    }

    /// Test seam: lets scripted replies call the panicking `__panic` tool.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn enable_panic_tool(&self) {
        self.tools.panic_tool.set(true);
    }

    pub fn emit(&self, e: AgentEvent) {
        let _ = self.ports.events.send(e);
    }

    pub fn notice(&self, s: impl Into<String>) {
        self.emit(AgentEvent::Notice(s.into()));
    }

    pub fn error(&self, s: impl Into<String>) {
        self.emit(AgentEvent::Error(s.into()));
    }

    /// A failed compaction is an error unless the run was cancelled.
    fn report_compaction_failure(&self, e: impl std::fmt::Display, cancel: &CancellationToken) {
        if !cancel.is_cancelled() {
            self.error(format!("compaction failed: {e}"));
        }
    }

    /// Writes a `local://` file, creating its directory. The error is the cause alone.
    fn write_local_file(&self, uri: &str, content: &str) -> Result<(), String> {
        let r = self.tools.resolve(uri)?;
        std::fs::create_dir_all(r.path.parent().unwrap_or(&r.path))
            .and_then(|()| std::fs::write(&r.path, content))
            .map_err(|e| e.to_string())
    }

    pub fn system(&self) -> String {
        let mcp = self.ports.plugins.mcp_instructions();
        prompt::system_prompt(&PromptInput {
            model: &self.model.model.id,
            cwd: &self.cwd,
            ctx: &self.ctx,
            plan: self.plan.plan.as_ref().map(|_| PLAN_PROMPT),
            subagent: self.run.subagent,
            ask: self.policy().permits("ask"),
            task: self.policy().permits("task"),
            agent_prompt: self.run.agent_prompt.as_deref(),
            plugin_ctx: &self.plugin_ctx,
            mcp_instructions: mcp.as_deref(),
            hashline: mu_tools::tools::edit::is_hashline(),
        })
    }

    pub async fn run(mut self, rx: UnboundedReceiver<Command>) {
        self.costs.quota.enabled = true;
        self.sync_model_state();
        let providers = self.ports.providers.clone();
        tokio::task::spawn_local(async move {
            providers.refresh_direct_models(false).await;
        });
        self.refresh_external().await;
        self.emit_status();
        let mut title_rx = self.title.rx.take().expect("run once");
        let mut wake_rx = self.mail.wake_rx.take();
        let mut quota_rx = self.costs.quota.take_wake();
        let mut q = Queue {
            rx,
            backlog: Default::default(),
            jobs: self.tools.jobs.clone(),
            events: self.ports.events.clone(),
            live: self.model.live_thinking.clone(),
            inbox: self.ports.inbox.clone(),
            perms: self.tools.permissions.clone(),
        };
        loop {
            let cmd = if let Some(c) = q.backlog.pop_front() {
                c
            } else {
                tokio::select! {
                c = q.rx.recv() => match c { Some(c) => c, None => break },
                () = tokio::time::sleep(self.costs.quota.wait()) => {
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
                    drive(self.turn(None), &mut q).await;
                    continue;
                }
                }
            };
            if !self.command(cmd, &mut q).await {
                break;
            }
        }
        self.emit(AgentEvent::Exit);
    }

    /// Runs one command. `false` ends the run loop.
    async fn command(&mut self, cmd: Command, q: &mut Queue) -> bool {
        let cmd = match cmd {
            Command::Steer(i) | Command::FollowUp(i) => {
                // Only a running turn takes queued input; an idle one goes back to the composer.
                self.emit(AgentEvent::QueuedDropped(vec![i]));
                return true;
            }
            other => match inbox_cmd(other, &self.ports.inbox, &self.ports.events) {
                Ok(()) => return true,
                Err(c) => c,
            },
        };
        match cmd {
            Command::Steer(_) | Command::FollowUp(_) | Command::PopQueue | Command::Cancel | Command::Stop => {
                unreachable!("queue and stop commands are handled above")
            }
            Command::Prompt(t) => drive(self.turn(Some(t)), q).await,
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
            Command::CycleThinking => self.set_thinking(self.model.live_thinking.get().cycle()),
            Command::SetPermissionMode(m) => {
                self.tools.permissions.set_mode(m);
                announce_mode(&self.ports.events, m);
                self.emit_status();
            }
            Command::NewSession => {
                self.end_session().await;
                self.reset_conversation();
                self.notice("new session");
                self.emit_status();
            }
            Command::Resume(p) => {
                // A `-p` run that asked for a session must not quietly run in another one.
                if !self.load_session(&p).await && self.run.exit_on_resume_failure {
                    return false;
                }
            }
            Command::Fork => self.fork().await,
            Command::ListRewind => self.list_rewind(),
            Command::Rewind(id) => self.rewind(&id).await,
            Command::TogglePlan(prompt) => match (self.plan.plan.is_some(), prompt) {
                (false, p) => {
                    self.enter_plan().await;
                    if let Some(p) = p {
                        drive(self.turn(Some(p.into())), q).await;
                    }
                }
                (true, Some(p)) => drive(self.turn(Some(p.into())), q).await,
                (true, None) => self.exit_plan(),
            },
            Command::Compact(focus) => drive(self.manual_compact(focus), q).await,
            Command::Review(focus) => drive(self.review(focus), q).await,
            Command::Rename(t) => {
                self.ensure_session().await;
                if let Some(s) = self.session.as_mut() {
                    s.set_title(&t, TitleSource::User);
                }
                self.title.text = Some(t);
                self.emit_status();
            }
            Command::Bash { cmd, include } => drive(self.user_bash(cmd, include), q).await,
            Command::KillJobs(id) => {
                let n = self.tools.jobs.as_ref().map_or(0, |j| j.kill(id.as_deref()));
                self.notice(format!("killed {n} job(s)"));
            }
            Command::RefreshTools => self.refresh_tool_defs(),
            Command::ListPlans => self.list_plans().await,
            Command::OpenPlan(id) => {
                // The dialog runs under `drive`, so Esc and cancel reach it.
                let mut start = None;
                drive(
                    async {
                        start = self.open_plan(&id).await;
                    },
                    q,
                )
                .await;
                if let Some(s) = start {
                    drive(self.turn_from(s), q).await;
                }
            }
            Command::Deliver(id) => self.deliver(&id).await,
            Command::StartPlan { id, decision, anyway } => {
                if let Some(s) = self.start_plan(&id, decision, anyway).await {
                    drive(self.turn_from(s), q).await;
                }
            }
            Command::RefinePlan { id, text } => {
                if let Some(s) = self.refine_plan(&id, text).await {
                    drive(self.turn_from(s), q).await;
                }
            }
            Command::Exit => {
                self.end_session().await;
                return false;
            }
        }
        true
    }
}
