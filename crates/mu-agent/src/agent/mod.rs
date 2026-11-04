mod compaction;
use self::compaction::Outcome;
use crate::plans;
use crate::policy::ToolPolicy;
use crate::prompt::{self, PromptInput};
use crate::review;
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
use mu_provider::provider::{ProviderError, Providers, Request, StreamEvent};
use mu_session::inbox;
use mu_session::session::{self, CompactionState, SessionFile};
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

    pub fn tool_defs(&self) -> Vec<ToolDef> {
        let mut v = self
            .base_defs
            .get_or_init(|| Tools::definitions().into_iter().chain(self.plugins.tool_defs()).collect())
            .clone();
        if self.plan.is_some() {
            v.push(Tools::plan_def());
        }
        v.push(Tools::task_def(self.jobs.is_some(), &self.ctx.agents));
        if self.jobs.is_some() {
            v.push(Tools::wait_def());
        }
        if let Some(o) = self.tools.output_schema.borrow().as_ref() {
            v.push(Tools::yield_def(o));
        }
        let policy = self.policy();
        v.retain(|d| policy.permits(&d.name));
        v
    }

    /// The tool limits for this run; enforced in `run_tool` and used to build the advertised list.
    pub fn policy(&self) -> ToolPolicy {
        ToolPolicy {
            allow: self.allow.clone(),
            excluded: self.excluded.clone(),
            depth: mu_tools::tools::task::depth(),
            plan: self.plan.is_some(),
        }
    }

    async fn enter_plan(&mut self) {
        self.ensure_session().await;
        let prev = PlanState { prev_model: Some(self.model.id.clone()), prev_thinking: self.thinking };
        if self.providers.cfg.role("plan").is_some() {
            self.set_model("@plan");
        }
        if let Some(s) = self.session.as_mut() {
            s.mode_change(
                "plan",
                json!({"planFilePath": "local://", "prevModel": prev.prev_model, "prevThinking": prev.prev_thinking.as_str()}),
            );
        }
        self.plan = Some(prev);
        self.tools.plan.set(true);
        self.notice("plan mode on");
        self.emit_status();
    }

    fn exit_plan(&mut self) {
        let Some(p) = self.plan.take() else { return };
        if let Some(m) = p.prev_model {
            self.set_model(&m);
        }
        self.set_thinking(p.prev_thinking);
        if let Some(s) = self.session.as_mut() {
            s.mode_change("none", json!({}));
        }
        self.tools.plan.set(false);
        self.notice("plan mode off");
        self.emit_status();
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

    /// Drops the cached tool list so the next request rebuilds it (after an MCP login added tools).
    pub fn refresh_tool_defs(&mut self) {
        self.base_defs.take();
    }

    /// Tokens of the system prompt, measured again only when what it is built from changes.
    pub fn system_tokens(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        (
            &self.model.id,
            self.plan.is_some(),
            &self.plugin_ctx,
            &self.agent_prompt,
            self.subagent,
            self.policy().permits("ask"),
        )
            .hash(&mut h);
        self.plugins.mcp_instructions().hash(&mut h);
        let key = h.finish();
        let mut memo = self.system_memo.borrow_mut();
        match *memo {
            Some((k, t)) if k == key => t,
            _ => {
                let t = self.system().len() as u64 / 4;
                *memo = Some((key, t));
                t
            }
        }
    }

    /// Sum of the messages' estimates, kept until the message list changes.
    pub fn messages_tokens(&self) -> u64 {
        let key = (self.messages.len(), self.messages.last().map_or(0, Message::timestamp));
        let mut memo = self.messages_memo.borrow_mut();
        match *memo {
            Some((k, t)) if k == key => t,
            _ => {
                let t = self.messages.iter().map(Message::estimate_tokens).sum();
                *memo = Some((key, t));
                t
            }
        }
    }

    pub fn session_id(&self) -> String {
        self.session.as_ref().map(|s| s.id()).unwrap_or_default()
    }

    pub fn snapshot(&self) -> StatusSnapshot {
        StatusSnapshot {
            model_id: self.model.id.clone(),
            model_name: pricing::display_name(&self.model.id),
            reasoning: pricing::reasoning(&self.model.id).unwrap_or(true),
            thinking: self.thinking.as_str().into(),
            subscription: !self.model.native && self.quota.is_subscription(&self.model.provider),
            native: self.model.native,
            plan: self.plan.is_some(),
            context_tokens: Some(self.context_tokens()),
            context_window: self.model.context_window,
            cost: self.cost.total,
            tokens_in: self.cost.tokens_in,
            tokens_out: self.cost.tokens_out,
            title: self.title.clone(),
            session_path: self.session.as_ref().map(|s| s.path.to_string_lossy().into_owned()),
            git: self.git.clone(),
            usage: if self.model.native { None } else { self.quota.usage_for(&self.model.provider) },
        }
    }

    /// Git state for the status line. Runs between turns, never from the draw path.
    async fn refresh_external(&mut self) {
        self.git = git::git_status(&self.cwd).await;
        self.tick_quota();
        self.settle_prices();
    }

    /// Native models are metered by API key: there is no subscription quota to poll.
    fn tick_quota(&mut self) {
        if self.model.native {
            return;
        }
        let provider = self.model.provider.clone();
        self.quota.tick(&provider);
    }

    /// Prices usage recorded before the table knew its model. A model the table does not list forces
    /// one catalogue refresh per process.
    fn settle_prices(&mut self) {
        if self.cost.settle() && self.quota.enabled && !self.model.native {
            tokio::task::spawn_local(pricing::refresh_missing());
        }
    }

    /// Picks up gateway-reported context windows once they are known.
    fn refresh_model_meta(&mut self) {
        if let Ok((spec, _)) = self.providers.resolve(&self.model.id) {
            self.model = spec;
            self.sync_model_state();
        }
    }

    /// Native models pause quota polling. omp's catalogue is read only once a gateway model is in use,
    /// so a native-only session never starts omp.
    fn sync_model_state(&mut self) {
        self.quota.set_paused(self.model.native);
        if !self.model.native && !self.pricing_checked {
            self.pricing_checked = true;
            tokio::task::spawn_local(pricing::refresh());
        }
    }

    pub fn emit_status(&self) {
        self.emit(AgentEvent::Status(self.snapshot()));
    }

    pub fn fresh_usage_tokens(&self) -> Option<u64> {
        let msgs = self.messages.get(self.fresh_from..)?;
        let i = msgs.iter().rposition(|m| matches!(m, Message::Assistant { .. }))?;
        let Message::Assistant { usage, .. } = &msgs[i] else { return None };
        let trailing: u64 = msgs[i + 1..].iter().map(Message::estimate_tokens).sum();
        Some(usage.input + usage.cache_read + usage.cache_write + usage.output + trailing)
    }

    /// Makes sure a session file exists. `false` (with an error shown) when it cannot be created.
    async fn ensure_session(&mut self) -> bool {
        if self.session.is_some() {
            return true;
        }
        match SessionFile::create(&self.cwd, self.parent_session.as_deref()) {
            Ok(mut s) => {
                self.attach_error_sink(&mut s);
                s.model_change(&self.model.id);
                s.thinking_change(self.thinking.as_str());
                self.session = Some(s);
                self.tools.set_session(&self.session_id(), self.session.as_ref().map(|s| s.path.clone()));
                self.sync_plugin_session();
                let res = self.plugins.hook("session_start", json!({"resumed": self.resumed})).await;
                self.apply_plugin_context(res);
                true
            }
            Err(e) => {
                self.error(format!("cannot create the session file: {e}"));
                false
            }
        }
    }

    /// Session write errors reach the user as error events.
    fn attach_error_sink(&self, s: &mut SessionFile) {
        let tx = self.events.clone();
        s.on_error = Some(Box::new(move |m| {
            let _ = tx.send(AgentEvent::Error(m));
        }));
    }

    fn sync_plugin_session(&self) {
        self.plugins.set_session(InitParams {
            cwd: self.cwd.to_string_lossy().into(),
            session_id: self.session_id(),
            session_file: self.session.as_ref().map(|s| s.path.to_string_lossy().into()),
        });
    }

    /// `null`/absent keeps the previous context, `""` clears it.
    fn apply_plugin_context(&mut self, results: Vec<(String, Value)>) {
        for (plugin, v) in results {
            let Some(ctx) = v["context"].as_str() else { continue };
            let r = secrets::redact(ctx, &self.cwd);
            let ctx = r.text.as_str();
            let changed = match self.plugin_ctx.get(&plugin) {
                Some(old) => old != ctx,
                None => !ctx.is_empty(),
            };
            if !changed {
                continue;
            }
            self.log_redaction(&format!("plugin:{plugin}"), &r.hits);
            if ctx.is_empty() {
                self.plugin_ctx.remove(&plugin);
            } else {
                self.plugin_ctx.insert(plugin.clone(), ctx.into());
            }
            if let Some(s) = self.session.as_mut() {
                s.custom("plugin_context", json!({"plugin": plugin, "context": ctx}));
            }
        }
    }

    fn continue_with(&mut self, sel: &str) {
        let before = self.model.id.clone();
        self.set_model(sel);
        if self.model.id != before {
            self.notice(format!("continuing with {sel}: {}", self.model.id));
        }
    }

    pub fn set_model(&mut self, sel: &str) {
        match self.providers.resolve(sel) {
            Ok((spec, level)) => {
                if let Some(s) = self.session.as_mut() {
                    s.model_change(&spec.id);
                }
                if self.model.provider != spec.provider {
                    self.quota.provider_changed();
                }
                self.model = spec;
                self.sync_model_state();
                if let Some(l) = level {
                    self.set_thinking(l);
                }
                self.emit_status();
            }
            Err(e) => self.error(e.to_string()),
        }
    }

    pub fn set_thinking(&mut self, t: Thinking) {
        self.thinking = t;
        self.live_thinking.set(t);
        if let Some(s) = self.session.as_mut() {
            s.thinking_change(t.as_str());
        }
        self.emit_status();
    }

    pub fn push(&mut self, mut m: Message) {
        let hits = self.redact_message(&mut m);
        if !hits.is_empty() && matches!(m, Message::User { .. }) {
            let names: Vec<String> = hits.iter().map(secrets::Hit::placeholder).collect();
            self.notice(format!("secrets replaced before sending: {}", names.join(", ")));
        }
        self.log_redaction("message", &hits);
        if let Message::Assistant { usage, provider, model, .. } = &mut m {
            self.cost.record(provider, model, usage);
        }
        let plugin_view =
            if m.image_stats().0 > 0 { serde_json::to_value(m.without_images()) } else { serde_json::to_value(&m) };
        if let Ok(v) = plugin_view {
            self.turn_msgs.push(v);
        }
        let id = self.session.as_mut().and_then(|s| s.message(&m)).unwrap_or_default();
        self.entry_ids.push(id);
        self.messages.push(m);
        self.since_compaction += 1;
    }

    fn push_result(&mut self, id: &str, name: &str, out: &ToolOutput) {
        self.push(Message::ToolResult {
            tool_call_id: id.into(),
            tool_name: name.into(),
            content: std::iter::once(Block::text(out.text.clone()))
                .chain(out.images.iter().cloned().map(Block::Image))
                .collect(),
            is_error: out.is_error,
            timestamp: now_ms(),
            details: out.stored_details(),
        });
    }

    /// Replaces secrets in the text of a user message or tool result.
    fn redact_message(&self, m: &mut Message) -> Vec<secrets::Hit> {
        let (Message::User { content, .. } | Message::ToolResult { content, .. }) = m else { return Vec::new() };
        let mut hits = Vec::new();
        for b in content.iter_mut() {
            if let Block::Text { text } = b {
                let r = secrets::redact(text, &self.cwd);
                if !r.hits.is_empty() {
                    *text = r.text;
                    secrets::merge(&mut hits, r.hits);
                }
            }
        }
        hits
    }

    /// Records what was replaced (names, never values), so how often the filter has to step in can
    /// be measured per session.
    fn log_redaction(&mut self, at: &str, hits: &[secrets::Hit]) {
        if hits.is_empty() {
            return;
        }
        if let Some(s) = self.session.as_mut() {
            s.custom("secrets_redacted", json!({"at": at, "hits": hits}));
        }
    }

    /// Replaces secrets in a tool's output before anyone sees it: the UI, plugins, the session and
    /// the model. `note` tells the model what happened.
    fn redact_output(&self, out: &mut ToolOutput, note: bool) -> Vec<secrets::Hit> {
        let mut hits = Vec::new();
        let mut red = |s: &mut String| {
            let r = secrets::redact(s, &self.cwd);
            if !r.hits.is_empty() {
                *s = r.text;
                secrets::merge(&mut hits, r.hits);
            }
        };
        match &mut out.meta {
            // Output and mu's notes are redacted apart, so `output_end` still splits them.
            Some(ToolMeta::Bash(m)) if out.text.is_char_boundary(m.output_end.min(out.text.len())) => {
                let end = m.output_end.min(out.text.len());
                let (mut body, mut notes) = (out.text[..end].to_string(), out.text[end..].to_string());
                red(&mut body);
                red(&mut notes);
                m.output_end = body.len();
                out.text = body + &notes;
            }
            _ => red(&mut out.text),
        }
        if let Some(d) = out.display.as_mut() {
            red(d);
        }
        if let Some(d) = out.details.as_mut() {
            secrets::merge(&mut hits, secrets::redact_json(d, &self.cwd));
        }
        if note && !hits.is_empty() {
            out.text.push_str("\n\n");
            out.text.push_str(&secrets::feedback(&hits));
        }
        hits
    }

    fn reset_conversation(&mut self) {
        self.session = None;
        self.messages.clear();
        self.entry_ids.clear();
        self.notify_seen.clear();
        self.notify_announced.clear();
        self.plugin_ctx.clear();
        self.review_base = None;
        self.title = None;
        self.user_turn = 0;
        self.cost.reset();
        self.turn_msgs.clear();
        self.compaction = None;
        self.fresh_from = 0;
        self.since_compaction = 0;
        self.title_requested = false;
        self.title_cancel.cancel();
        self.title_cancel = CancellationToken::new();
        self.plan = None;
        self.tools.plan.set(false);
        self.exec_plan = None;
        self.stored_plan = None;
        self.resumed = false;
        self.tools.set_todo(Vec::new());
        self.emit(AgentEvent::Todo(self.tools.todo_json()));
    }

    async fn end_session(&mut self) {
        if let Some(jobs) = &self.jobs {
            jobs.clear();
        }
        if self.session.is_some() {
            self.plugins.hook("session_end", json!({})).await;
            // plugins re-initialize with the next session's id on next use
            self.plugins.shutdown_all().await;
        }
    }

    /// Loads `path`, applying its model/thinking unless pinned by the command line.
    /// Re-emits the loaded conversation as UI events so a resumed session shows its history.
    fn replay_history(&self) {
        for e in history_events(&self.messages) {
            self.emit(e);
        }
    }

    /// `false` (with an error shown) when the session could not be loaded or opened.
    pub async fn load_session(&mut self, path: &Path) -> bool {
        let loaded = match session::load(path) {
            Ok(l) => l,
            Err(e) => {
                self.error(e.to_string());
                return false;
            }
        };
        // Re-opening the session that is current: release its lock first.
        let same = self.session.as_ref().is_some_and(|c| c.path == path);
        if same {
            self.end_session().await;
            self.session = None;
        }
        let mut s = match SessionFile::open_append(path) {
            Ok(mut s) => {
                self.attach_error_sink(&mut s);
                s
            }
            Err(e) => {
                self.error(e.to_string());
                return false;
            }
        };
        // Only now, with the target loaded and open, does the current session end.
        if !same {
            self.end_session().await;
        }
        if !loaded.warnings.is_empty() {
            self.notice(format!("session repaired on load: {}", loaded.warnings.join("; ")));
        }
        self.reset_conversation();
        if let Some(m) = loaded.model.as_deref().filter(|_| !self.pin_model) {
            match self.providers.resolve(m) {
                Ok((spec, _)) => self.model = spec,
                Err(e) => self.error(format!("session model {m}: {e}")),
            }
        } else if self.pin_model && loaded.model.as_deref() != Some(self.model.id.as_str()) {
            s.model_change(&self.model.id);
        }
        if let Some(t) = loaded.thinking.as_deref().and_then(Thinking::parse).filter(|_| !self.pin_thinking) {
            self.thinking = t;
            self.live_thinking.set(t);
        } else if self.pin_thinking {
            s.thinking_change(self.thinking.as_str());
        }
        // Stored costs, except turns that were saved unpriced (price table missing the model at the time).
        for m in &loaded.messages {
            if let Message::Assistant { usage, provider, model, .. } = m {
                self.cost.add_stored(provider, model, usage);
            }
        }
        self.settle_prices();
        self.messages = loaded.messages;
        // Sessions written before redaction, or with it off, may hold secrets.
        let mut msgs = std::mem::take(&mut self.messages);
        let mut resume_hits = Vec::new();
        for m in msgs.iter_mut() {
            secrets::merge(&mut resume_hits, self.redact_message(m));
        }
        self.messages = msgs;
        self.entry_ids = loaded.entry_ids;
        self.plugin_ctx = loaded.plugin_context;
        for v in self.plugin_ctx.values_mut() {
            let r = secrets::redact(v, &self.cwd);
            *v = r.text;
            secrets::merge(&mut resume_hits, r.hits);
        }
        self.review_base = loaded.review_base.and_then(|v| serde_json::from_value(v).ok());
        let phases = loaded.todo.and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default();
        self.tools.set_todo(phases);
        self.emit(AgentEvent::Todo(self.tools.todo_json()));
        self.title = loaded.title;
        self.title_requested = self.title.is_some();
        self.compaction = loaded.compaction;
        self.fresh_from = if self.compaction.is_some() { self.messages.len() } else { 0 };
        self.since_compaction = self.messages.len();
        self.user_turn = self.messages.iter().filter(|m| matches!(m, Message::User { .. })).count() as u32;
        self.session = Some(s);
        self.log_redaction("resume", &resume_hits);
        self.tools.set_session(&self.session_id(), self.session.as_ref().map(|s| s.path.clone()));
        if let Some((_, data)) = loaded.mode.as_ref().filter(|(m, _)| m == "plan") {
            // The model and thinking level in force before plan mode, so leaving it restores them.
            let prev_model = data["prevModel"].as_str().map(String::from);
            let prev_thinking = data["prevThinking"].as_str().and_then(Thinking::parse).unwrap_or(self.thinking);
            self.plan = Some(PlanState { prev_model, prev_thinking });
            self.tools.plan.set(true);
        }
        self.resumed = true;
        self.sync_plugin_session();
        let res = self.plugins.hook("session_start", json!({"resumed": true})).await;
        self.apply_plugin_context(res);
        self.replay_history();
        self.notice(format!("resumed {}", path.display()));
        if self.notify {
            let waiting = inbox::pending(&self.session_id());
            if !waiting.is_empty() {
                let mut from: Vec<&str> = Vec::new();
                for n in &waiting {
                    if !from.contains(&n.from.as_str()) {
                        from.push(&n.from);
                    }
                }
                self.notice(format!(
                    "{} message(s) from mu notify waiting (from {}); they go out with your next prompt",
                    waiting.len(),
                    from.join(", ")
                ));
                self.notify_announced.extend(waiting.into_iter().map(|n| n.name));
            }
        }
        self.emit_status();
        true
    }

    pub async fn run(mut self, mut rx: UnboundedReceiver<Command>) {
        self.quota.enabled = true;
        self.sync_model_state();
        let providers = self.providers.clone();
        tokio::task::spawn_local(async move {
            providers.refresh_native_models(false).await;
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
                        s.set_title(&t, session::TitleSource::User);
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

    /// Gives a fresh session the approved plan as a `local://` file, so the path in the execution
    /// prompt resolves, and names the session after the plan.
    fn seed_plan(&mut self, p: &ExecPlan) {
        match self.tools.resolve(&p.uri) {
            Ok(r) => {
                if let Err(e) = std::fs::create_dir_all(r.path.parent().unwrap_or(&r.path))
                    .and_then(|_| std::fs::write(&r.path, &p.content))
                {
                    self.error(format!("cannot copy the plan to {}: {e}", p.uri));
                }
            }
            Err(e) => self.error(format!("cannot copy the plan to {}: {e}", p.uri)),
        }
        if self.title.is_none() && !p.title.trim().is_empty() {
            if let Some(s) = self.session.as_mut() {
                s.set_title(&p.title, session::TitleSource::Auto);
            }
            self.title = Some(p.title.clone());
            self.emit_status();
        }
    }

    fn apply_title(&mut self, sid: &str, t: String) {
        if self.session_id() != sid || self.title.is_some() {
            return;
        }
        if let Some(s) = self.session.as_mut() {
            s.set_title(&t, session::TitleSource::Auto);
        }
        self.title = Some(t);
        self.emit_status();
    }

    async fn manual_compact(&mut self, focus: Option<String>) {
        let cancel = CancellationToken::new();
        self.inbox.borrow_mut().cancel = cancel.clone();
        self.emit(AgentEvent::TurnStart);
        match self.compact(focus.as_deref(), &cancel).await {
            Outcome::Done => {}
            Outcome::Skipped(why) => self.notice(why),
            Outcome::Failed(e) if !cancel.is_cancelled() => self.error(format!("compaction failed: {e}")),
            Outcome::Failed(_) => {}
        }
        self.emit(AgentEvent::TurnEnd { aborted: cancel.is_cancelled() });
    }

    /// One single-answer question to the user; a cancel or a closed dialog is `Cancelled`.
    async fn review_ask(&self, q: mu_base::events::AskQuestion, cancel: &CancellationToken) -> mu_base::events::Answer {
        use mu_base::events::Answer;
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.emit(AgentEvent::Ask { questions: vec![q], reply: tx });
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Answer::Cancelled,
            r = rx => r.ok().flatten().and_then(|mut a| (!a.is_empty()).then(|| a.remove(0))).unwrap_or(Answer::Cancelled),
        }
    }

    /// `/review`: a fresh-context lead reviewer checks the changes since plan approval (or the
    /// uncommitted changes) and the user picks which findings go to the implementer.
    async fn review(&mut self, focus: Option<String>) {
        if !self.ensure_session().await {
            return;
        }
        let cancel = CancellationToken::new();
        self.inbox.borrow_mut().cancel = cancel.clone();
        self.emit(AgentEvent::TurnStart);
        let sent = self.review_run(focus, &cancel).await;
        self.emit(AgentEvent::TurnEnd { aborted: cancel.is_cancelled() });
        if let Some(msg) = sent {
            self.turn(Some(msg.into())).await;
        }
    }

    /// Runs the review and the selection dialog. Returns the message to send to the implementer.
    async fn review_run(&mut self, focus: Option<String>, cancel: &CancellationToken) -> Option<String> {
        use mu_base::events::{Answer, AskOption, AskQuestion};
        let short = |s: &str| s.chars().take(7).collect::<String>();
        let root = match review::repo_root(&self.cwd).await {
            Ok(r) => r,
            Err(e) => {
                self.error(e);
                return None;
            }
        };
        let cur = match review::snapshot_tree(&root).await {
            Ok(c) => c,
            Err(e) => {
                self.error(format!("review: {e}"));
                return None;
            }
        };
        let (base, base_label) = match self.review_base.as_ref().and_then(|b| b.tree.clone()) {
            Some(t) => (t, "plan approval snapshot"),
            None => {
                self.notice("no plan base recorded; reviewing uncommitted changes against HEAD");
                match review::head_tree(&root).await {
                    Ok(t) => (t, "HEAD (no plan approved in this session)"),
                    Err(e) => {
                        self.error(format!("review: {e}"));
                        return None;
                    }
                }
            }
        };
        let since = self.review_base.as_ref().map_or(0, |b| b.since);
        let stats = match review::numstat(&root, &base, &cur).await {
            Ok(s) => s,
            Err(e) => {
                self.error(format!("review: {e}"));
                return None;
            }
        };
        if stats.is_empty() {
            self.notice(format!("nothing to review: no changes since {}", short(&base)));
            return None;
        }
        let (files, excluded) = review::split_excluded(stats);
        if files.is_empty() {
            self.notice("nothing to review: only binary, generated or lock files changed");
            return None;
        }
        let plan_uri = self.review_base.as_ref().and_then(|b| b.plan_uri.clone());
        let plan_path = match &plan_uri {
            Some(u) => match self.tools.resolve(u) {
                Ok(r) if r.path.exists() => Some(r.path),
                _ => {
                    self.notice(format!("plan file {u} not found; reviewing without it"));
                    None
                }
            },
            None => None,
        };
        let plan_uri = plan_path.as_ref().and(plan_uri);
        let notes = review::user_notes(&self.messages, since);
        let request = review::request(&review::RequestInput {
            root: &root,
            base: &base,
            cur: &cur,
            base_label,
            plan_path,
            files: &files,
            excluded: &excluded,
            notes: &notes,
            focus: focus.as_deref(),
        });
        let model = mu_base::models::role_selector("review", &self.providers.cfg)
            .and_then(|s| self.providers.resolve(&s).ok())
            .map_or_else(|| self.model.id.clone(), |(m, _)| m.id);
        let (added, removed) =
            files.iter().fold((0, 0), |(a, r), f| (a + f.added.unwrap_or(0), r + f.removed.unwrap_or(0)));
        let exe = match mu_common::paths::self_exe() {
            Ok(e) => e,
            Err(e) => {
                self.error(format!("cannot locate mu executable: {e}"));
                return None;
            }
        };
        let payload = json!({
            "task": request,
            "agent": context::REVIEW_LEAD,
            "outputSchema": review::report_schema(),
            "parentSession": self.session.as_ref().map(|s| s.path.clone()),
        });
        let id = "review".to_string();
        self.emit(AgentEvent::ToolStart {
            id: id.clone(),
            name: "review".into(),
            args: json!({"scope": format!("{} files +{added}/−{removed} vs {} · {model}", files.len(), short(&base))}),
        });
        let out = {
            let (events, id) = (self.events.clone(), id.clone());
            mu_tools::tools::subagent::run_child(&exe, &root, &payload, cancel, move |name, summary| {
                let _ = events.send(AgentEvent::ToolOutput { id: id.clone(), chunk: format!("{name} {summary}\n") });
            })
            .await
        };
        if cancel.is_cancelled() {
            self.emit(AgentEvent::ToolEnd { id, name: "review".into(), output: ToolOutput::err("review cancelled") });
            return None;
        }
        if !out.ok {
            self.emit(AgentEvent::ToolEnd { id, name: "review".into(), output: ToolOutput::err(out.text) });
            return None;
        }
        let report = review::report_of(&out);
        let md = match &report {
            Some(r) => review::render_markdown(
                r,
                &review::Meta {
                    base_short: short(&base),
                    cur_short: short(&cur),
                    files: files.len(),
                    added,
                    removed,
                    model,
                },
            ),
            None => format!("reviewer returned an unstructured report\n\n{}", out.text),
        };
        let report_uri = review::report_uri();
        match self.tools.resolve(&report_uri) {
            Ok(r) => {
                if let Err(e) = std::fs::create_dir_all(r.path.parent().unwrap_or(&r.path))
                    .and_then(|_| std::fs::write(&r.path, &md))
                {
                    self.error(format!("cannot save the review to {report_uri}: {e}"));
                }
            }
            Err(e) => self.error(format!("cannot save the review to {report_uri}: {e}")),
        }
        self.emit(AgentEvent::ToolEnd { id, name: "review".into(), output: ToolOutput::ok(md) });

        let opt =
            |label: String, description: Option<String>| AskOption { label, description, preview: None, kind: None };
        let Some(report) = report else {
            let q = AskQuestion {
                question: "The reviewer returned an unstructured report.".into(),
                context: None,
                header: Some("Review".into()),
                options: vec![opt("Send report to the implementer".into(), None), opt("Dismiss".into(), None)],
                multi: false,
                recommended: Some(1),
                confirm: false,
            };
            return match self.review_ask(q, cancel).await {
                Answer::Options(o) | Answer::Noted { picks: o, .. }
                    if o.first().is_some_and(|l| l.starts_with("Send")) =>
                {
                    Some(review::raw_message(&out.text, plan_uri.as_deref(), &report_uri))
                }
                _ => None,
            };
        };
        if report.findings.is_empty() {
            self.notice("review: no findings");
            return None;
        }
        let all = &report.findings;
        let blocking: Vec<usize> = (0..all.len()).filter(|&i| review::is_blocking(&all[i])).collect();
        let (n, m) = (blocking.len(), all.len());
        let mut options = Vec::new();
        if n > 0 {
            options.push(opt(format!("Send {n} blocking findings"), None));
        }
        if m > n {
            options.push(opt(format!("Send all {m} findings"), None));
        }
        options.push(opt("Choose findings…".into(), None));
        options.push(opt("Dismiss".into(), None));
        let recommended = if n > 0 { 0 } else { options.len() - 1 };
        let q = AskQuestion {
            question: "Send findings to the implementer?".into(),
            context: None,
            header: Some("Review".into()),
            options,
            multi: false,
            recommended: Some(recommended),
            confirm: false,
        };
        let default_set = if n > 0 { blocking.clone() } else { (0..m).collect() };
        let (answer, typed) = match self.review_ask(q, cancel).await {
            Answer::Noted { picks, note } => (Answer::Options(picks), Some(note)),
            a => (a, None),
        };
        let (chosen, note): (Vec<usize>, Option<String>) = match answer {
            Answer::Options(o) => match o.first().map(String::as_str) {
                Some(l) if l.starts_with("Send all") => ((0..m).collect(), None),
                Some(l) if l.starts_with("Send ") => (blocking.clone(), None),
                Some(l) if l.starts_with("Choose") => {
                    let options = all
                        .iter()
                        .enumerate()
                        .map(|(i, f)| {
                            let first = f.body.lines().next().unwrap_or("");
                            let at = match f.line {
                                Some(l) => format!("{}:{l}", f.file),
                                None => f.file.clone(),
                            };
                            opt(format!("{}. [P{}] {}", i + 1, f.priority, f.title), Some(format!("{at} — {first}")))
                        })
                        .collect();
                    let q = AskQuestion {
                        question: "Which findings should be sent?".into(),
                        context: None,
                        header: Some("Review".into()),
                        options,
                        multi: true,
                        recommended: None,
                        confirm: false,
                    };
                    let (sel, typed) = match self.review_ask(q, cancel).await {
                        Answer::Options(sel) => (sel, None),
                        Answer::Noted { picks, note } => (picks, Some(note)),
                        _ => (Vec::new(), None),
                    };
                    let idx = sel
                        .iter()
                        .filter_map(|l| l.split('.').next()?.trim().parse::<usize>().ok())
                        .filter(|i| (1..=m).contains(i))
                        .map(|i| i - 1)
                        .collect::<std::collections::BTreeSet<_>>();
                    (idx.into_iter().collect(), typed)
                }
                _ => (Vec::new(), None),
            },
            Answer::Custom(t) => (default_set, Some(t)),
            Answer::Cancelled | Answer::Noted { .. } => (Vec::new(), None),
        };
        let note = note.or(typed);
        if chosen.is_empty() {
            return None;
        }
        let picked: Vec<&review::Finding> = chosen.iter().map(|&i| &all[i]).collect();
        Some(review::fix_message(&picked, plan_uri.as_deref(), &report_uri, note.as_deref()))
    }

    fn set_review_base(&mut self, b: review::ReviewBase) {
        if let Some(s) = self.session.as_mut() {
            s.custom("review_base", serde_json::to_value(&b).unwrap_or_default());
        }
        self.review_base = Some(b);
    }

    async fn user_bash(&mut self, cmd: String, include: bool) {
        if !self.ensure_session().await {
            return;
        }
        let cancel = CancellationToken::new();
        self.inbox.borrow_mut().cancel = cancel.clone();
        self.emit(AgentEvent::TurnStart);
        let args = json!({"command": cmd});
        let id = "user-bash".to_string();
        self.emit(AgentEvent::ToolStart { id: id.clone(), name: "bash".into(), args: args.clone() });
        let ctx = ToolCtx { events: self.events.clone(), cancel: cancel.clone(), call_id: id.clone() };
        let mut out = self.tools.call("bash", &args, &ctx).await;
        let hits = self.redact_output(&mut out, false);
        self.log_redaction("user-bash", &hits);
        if include {
            self.push(Message::user(&format!("I ran `{cmd}`:\n```\n{}\n```", out.text)));
            self.turn_msgs.clear(); // not a model turn: no `turn_end` hook for it
        }
        self.emit(AgentEvent::ToolEnd { id, name: "bash".into(), output: out });
        self.emit(AgentEvent::TurnEnd { aborted: cancel.is_cancelled() });
    }

    /// One turn. `first` is `None` for a wake-up that only delivers finished background jobs.
    pub async fn turn(&mut self, first: Option<UserInput>) {
        self.turn_from(first.map_or(Start::Wake, Start::User)).await;
    }

    async fn turn_from(&mut self, start: Start) {
        if matches!(start, Start::Wake)
            && !self.jobs.as_ref().is_some_and(|j| j.has_undelivered())
            && !self.notify_pending(true)
        {
            return;
        }
        // An approved stored plan opens its own session when it needs one.
        if !matches!(start, Start::Plan(_)) && !self.ensure_session().await {
            // Without this the caller (e.g. a headless subagent) waits for a `TurnEnd` forever.
            self.emit(AgentEvent::TurnEnd { aborted: true });
            return;
        }
        let cancel = CancellationToken::new();
        self.inbox.borrow_mut().cancel = cancel.clone();
        self.emit(AgentEvent::TurnStart);
        let wake = matches!(start, Start::Wake);
        let mut aborted = false;
        let mut synthetic = false;
        let mut next = match start {
            Start::User(i) => Some(Some(i)),
            Start::Synthetic(i) => {
                synthetic = true;
                Some(Some(i))
            }
            Start::Wake => Some(None),
            Start::Plan(p) => match self.begin_plan(p, &cancel).await {
                Some(i) => {
                    synthetic = true;
                    Some(Some(i))
                }
                None => {
                    aborted = true;
                    None
                }
            },
        };
        while let Some(input) = next.take() {
            if !self.ensure_session().await {
                break;
            }
            let extra = match &input {
                Some(i) => {
                    if !synthetic {
                        self.emit(AgentEvent::UserPrompt(i.text.clone()));
                    }
                    self.user_turn += 1;
                    let prompt = secrets::redact(&i.text, &self.cwd).text;
                    let res = self
                        .plugins
                        .hook(
                            "prompt_submit",
                            json!({"prompt": prompt, "userTurn": self.user_turn, "resumed": self.resumed}),
                        )
                        .await;
                    self.apply_plugin_context(res);
                    i.text.len() as u64 / 4 + 1600 * i.images.len() as u64
                }
                None => 0,
            };
            if self.needs_compaction(extra)
                && let Outcome::Failed(e) = self.compact(None, &cancel).await
                && !cancel.is_cancelled()
            {
                self.error(format!("compaction failed: {e}"));
            }
            match &input {
                Some(i) => {
                    self.take_notify();
                    if synthetic {
                        self.push(Message::synthetic_user(&i.text));
                    } else {
                        self.push(Message::user_input(i));
                    }
                }
                None => {
                    let jobs = self.take_jobs();
                    if !(self.take_notify() | jobs) {
                        break;
                    }
                    // Only a wake that delivered something is a woken turn (and rings at its end).
                    if wake {
                        self.emit(AgentEvent::Woken);
                    }
                }
            }
            synthetic = false;
            match self.run_loop(&cancel).await {
                Flow::Aborted => {
                    aborted = true;
                    self.exec_plan = None;
                    self.turn_end_hook().await;
                    break;
                }
                Flow::Failed => {
                    self.emit(AgentEvent::TurnFailed);
                    self.exec_plan = None;
                    break;
                }
                Flow::Stopped => {
                    self.exec_plan = None;
                    self.turn_end_hook().await;
                    break;
                }
                Flow::Done => {}
            }
            self.turn_end_hook().await;
            if let Some(p) = self.exec_plan.take() {
                match self.begin_plan(p, &cancel).await {
                    Some(i) => {
                        synthetic = true;
                        next = Some(Some(i));
                        continue;
                    }
                    None => {
                        aborted = true;
                        break;
                    }
                }
            }
            next = self.inbox.borrow_mut().follow_up.pop_front().map(Some);
        }
        // Failed and cancelled passes leave messages the hook has not seen; do not let them
        // leak into the next turn's `turn_end`.
        if !self.turn_msgs.is_empty() {
            self.turn_end_hook().await;
        }
        if let Some(s) = &self.session {
            s.sync();
        }
        mem::trim();
        self.refresh_model_meta();
        self.refresh_external().await;
        self.emit_status();
        self.emit(AgentEvent::TurnEnd { aborted });
    }

    /// Saves the plan a `propose_plan` result describes into the repository's store.
    async fn save_plan(&self, d: &Value) -> (plans::Store, Result<PlanMeta, String>) {
        let store = plans::store_for(&self.cwd).await;
        let uri = d["uri"].as_str().unwrap_or_default();
        let slug = uri.strip_prefix("local://").and_then(|u| u.strip_suffix("-plan.md")).unwrap_or_default();
        let content = match d["content"].as_str() {
            Some(c) => c.to_string(),
            None => self.tools.resolve(uri).ok().and_then(|r| std::fs::read_to_string(r.path).ok()).unwrap_or_default(),
        };
        let (base, read) = match &store.root {
            Some(root) => (plans::base_now(root).await, plans::reads_from(&self.messages, &self.cwd, root)),
            None => (None, Vec::new()),
        };
        let session = self.session.as_ref().map(|s| s.path.clone());
        let saved = store.save(plans::Save {
            slug,
            title: d["title"].as_str().unwrap_or_default(),
            content: &content,
            read,
            session: session.as_deref(),
            base,
            adopt: self.stored_plan.as_deref(),
        });
        (store, saved)
    }

    /// Starts the approved plan in this session. `None`: it could not start (already reported).
    async fn begin_plan(&mut self, p: ExecPlan, cancel: &CancellationToken) -> Option<UserInput> {
        self.exit_plan();
        if let Some(m) = &p.model {
            self.continue_with(m);
        }
        match p.mode {
            PlanMode::Fresh => {
                self.end_session().await;
                self.reset_conversation();
                self.notice("── new session");
                if !self.ensure_session().await {
                    return None;
                }
            }
            PlanMode::Compact => {
                if !self.ensure_session().await {
                    return None;
                }
                match self.compact(Some(PLAN_COMPACT_FOCUS), cancel).await {
                    Outcome::Done => {}
                    Outcome::Skipped(why) => self.notice(why),
                    Outcome::Failed(e) if !cancel.is_cancelled() => self.error(format!("compaction failed: {e}")),
                    Outcome::Failed(_) => {}
                }
                if cancel.is_cancelled() {
                    self.notice("Plan approved, but compaction was cancelled — execution not dispatched. Submit a turn to continue.");
                    return None;
                }
            }
            PlanMode::Keep => {
                if !self.ensure_session().await {
                    return None;
                }
            }
        }
        self.seed_plan(&p);
        self.set_review_base(review::ReviewBase {
            tree: p.base.clone(),
            plan_uri: Some(p.uri.clone()),
            since: p.approved_at,
        });
        if let Some(id) = &p.stored {
            let decision = match p.mode {
                PlanMode::Fresh => "execute",
                PlanMode::Compact => "compact",
                PlanMode::Keep => "keep",
            };
            let session = self.session.as_ref().map(|s| s.path.clone());
            if let Err(e) = plans::store_for(&self.cwd).await.approve(id, decision, session.as_deref()) {
                self.error(format!("could not record the approval: {e}"));
            }
        }
        self.notice(format!("▸ Plan approved: {}", p.title));
        Some(plan_prompt(&p).into())
    }

    async fn list_plans(&self) {
        let store = plans::store_for(&self.cwd).await;
        self.emit(AgentEvent::PlanList(store.list(false)));
    }

    /// `/deliver`: runs the injected `DeliverFn`, which wakes this session when the delivery finishes.
    async fn deliver(&mut self, id: &str) {
        if !self.ensure_session().await {
            return;
        }
        let Some(session) = self.session.as_ref().map(|s| s.path.clone()) else { return };
        self.notice(format!("starting delivery of plan {id}…"));
        match (self.deliver)(id.to_string(), session, self.cwd.clone()).await {
            Ok(out) => self.notice(format!("▸ {out}")),
            Err(e) => self.error(e),
        }
    }

    /// A pending plan from the store with its drift; failures are reported and give `None`.
    async fn pending_plan(&self, id: &str) -> Option<(plans::Store, PlanMeta, String, plans::Drift)> {
        let store = plans::store_for(&self.cwd).await;
        match store.load_pending(id) {
            Ok((meta, text)) => {
                let drift = plans::drift(&meta, &self.cwd).await;
                Some((store, meta, text, drift))
            }
            Err(e) => {
                self.error(e);
                None
            }
        }
    }

    /// `/plans`: shows the plan in the review dialog and applies the decision to this session.
    async fn open_plan(&mut self, id: &str) -> Option<Start> {
        let (store, meta, shown, drift) = self.pending_plan(id).await?;
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.emit(AgentEvent::PlanReview {
            title: meta.title.clone(),
            uri: format!("local://{id}-plan.md"),
            file: store.md_path(id),
            content: shown.clone(),
            stored: true,
            note: Some(format!(
                "written against {} · {}",
                crate::plans::base_label(&meta, &self.cwd).await,
                drift.describe()
            )),
            reply: tx,
        });
        let decision = rx.await.ok()?;
        // The user may have edited the file with Ctrl-G while the dialog was up.
        let content = std::fs::read_to_string(store.md_path(id)).unwrap_or(shown);
        self.apply_stored(&store, &meta, content, drift.describe(), decision).await
    }

    async fn start_plan(&mut self, id: &str, decision: PlanDecision, anyway: bool) -> Option<Start> {
        let (store, meta, content, drift) = self.pending_plan(id).await?;
        if matches!(drift, plans::Drift::Changed { .. }) && !anyway {
            self.error(format!(
                "plan '{id}' is stale: {}. Refine it with `mu plan refine {id}` or run it anyway with --anyway.",
                drift.describe()
            ));
            return None;
        }
        self.apply_stored(&store, &meta, content, drift.describe(), decision).await
    }

    async fn refine_plan(&mut self, id: &str, text: Option<String>) -> Option<Start> {
        let (store, meta, content, drift) = self.pending_plan(id).await?;
        let text = text.unwrap_or_else(|| "Bring the plan up to date with the current code.".into());
        self.apply_stored(&store, &meta, content, drift.describe(), PlanDecision::Refine(text)).await
    }

    /// Carries out a decision on a stored plan; returns the turn to run, if any.
    async fn apply_stored(
        &mut self,
        store: &plans::Store,
        meta: &PlanMeta,
        content: String,
        drift: String,
        decision: PlanDecision,
    ) -> Option<Start> {
        let id = meta.id.as_str();
        let uri = format!("local://{id}-plan.md");
        let (mode, model) = match decision {
            PlanDecision::Execute { model } => (PlanMode::Fresh, model),
            PlanDecision::Compact { model } => (PlanMode::Compact, model),
            PlanDecision::Keep { model } => (PlanMode::Keep, model),
            PlanDecision::Drop => {
                match store.drop(id) {
                    Ok(()) => self.notice(format!("dropped plan {id}")),
                    Err(e) => self.error(e),
                }
                return None;
            }
            PlanDecision::Refine(text) => {
                if self.plan.is_none() {
                    self.enter_plan().await;
                }
                self.stored_plan = Some(id.to_string());
                let written = self.tools.resolve(&uri).and_then(|r| {
                    std::fs::create_dir_all(r.path.parent().unwrap_or(&r.path))
                        .and_then(|()| std::fs::write(&r.path, &content))
                        .map_err(|e| e.to_string())
                });
                if let Err(e) = written {
                    self.error(format!("cannot copy the plan to {uri}: {e}"));
                    return None;
                }
                let prompt = PLAN_REFINE
                    .replace("{{id}}", id)
                    .replace("{{path}}", &uri)
                    .replace("{{drift}}", &drift)
                    .replace("{{request}}", &text);
                return Some(Start::Synthetic(prompt.into()));
            }
            PlanDecision::Park => return None,
        };
        Some(Start::Plan(ExecPlan {
            content,
            title: meta.title.clone(),
            uri,
            mode,
            model,
            base: review::snapshot_tree(&self.cwd).await.ok(),
            approved_at: now_ms(),
            stored: Some(id.to_string()),
        }))
    }

    async fn turn_end_hook(&mut self) {
        let msgs = std::mem::take(&mut self.turn_msgs);
        self.plugins.hook("turn_end", json!({"userTurn": self.user_turn, "messages": msgs})).await;
    }

    /// Takes queued steering text and pushes it as user messages. Returns whether any was pending.
    fn take_steer(&mut self) -> bool {
        let steer: Vec<UserInput> = self.inbox.borrow_mut().steer.drain(..).collect();
        for s in &steer {
            self.emit(AgentEvent::UserPrompt(s.text.clone()));
            self.push(Message::user_input(s));
        }
        !steer.is_empty()
    }

    /// Hands finished background jobs to the model as one `<system-notice>` user message.
    /// Returns whether any were pending.
    fn take_jobs(&mut self) -> bool {
        let Some(jobs) = &self.jobs else { return false };
        let done = jobs.take_undelivered(&[]);
        if done.is_empty() {
            return false;
        }
        self.emit(AgentEvent::JobsDelivered(done.iter().map(|d| d.id.clone()).collect()));
        self.push(Message::user(&jobs::notice(&done)));
        true
    }

    /// Is there a `mu notify` message to deliver? `wake`: for a turn nobody asked for, so
    /// messages announced at open (which wait for the next prompt) do not count.
    fn notify_pending(&self, wake: bool) -> bool {
        self.notify
            && self.session.is_some()
            && inbox::has_pending(&self.session_id())
            && inbox::pending(&self.session_id())
                .iter()
                .any(|n| !self.notify_seen.contains(&n.name) && !(wake && self.notify_announced.contains(&n.name)))
    }

    /// Hands queued `mu notify` messages to the model as one `<system-notice>` user message and
    /// deletes the files, but only once that message is in the session file.
    fn take_notify(&mut self) -> bool {
        if !self.notify || self.session.is_none() || !inbox::has_pending(&self.session_id()) {
            return false;
        }
        let id = self.session_id();
        let notes: Vec<inbox::Note> =
            inbox::pending(&id).into_iter().filter(|n| !self.notify_seen.contains(&n.name)).collect();
        if notes.is_empty() {
            return false;
        }
        for n in &notes {
            let line: String = n.text.lines().next().unwrap_or("").chars().take(80).collect();
            self.notice(format!("↳ mu notify from {}: {line}", n.from));
        }
        self.push(Message::user(&inbox::notice(&notes)));
        let names: Vec<String> = notes.into_iter().map(|n| n.name).collect();
        if self.entry_ids.last().is_some_and(|e| !e.is_empty()) {
            if let Some(s) = &self.session {
                s.sync();
            }
            inbox::remove(&id, &names);
        } else {
            // Not written: keep the files for the next open, but never deliver them twice here.
            self.notify_seen.extend(names);
        }
        true
    }

    /// The end-of-turn `ask` reminder may fire: `ask` is offered to this run, and the current user
    /// turn has neither called `ask` nor been reminded yet.
    fn ask_nudge_due(&self) -> bool {
        if !self.providers.cfg.mu.ask.nudge || self.subagent || !self.policy().permits("ask") {
            return false;
        }
        for m in self.messages.iter().rev() {
            match m {
                Message::User { content, .. } if ask_nudge::is_reminder(&text_of(content)) => return false,
                Message::User { .. } => return true,
                Message::Assistant { .. } if tool_calls(m).iter().any(|(_, name, _)| name == "ask") => return false,
                _ => {}
            }
        }
        true
    }

    async fn run_loop(&mut self, cancel: &CancellationToken) -> Flow {
        let mut first = true;
        let mut todo_nags = 0u8;
        let mut compacted_for_overflow = false;
        loop {
            if !first
                && self.needs_compaction(0)
                && let Outcome::Failed(e) = self.compact(None, cancel).await
                && !cancel.is_cancelled()
            {
                self.error(format!("compaction failed: {e}"));
            }
            first = false;
            // MCP servers without a cached tool list are started and listed once, before the first request.
            // A subagent restricted to named tools that include no MCP tool never needs the listing.
            if self.allow.as_ref().is_none_or(|a| a.iter().any(|n| n.starts_with("mcp__"))) {
                self.plugins.list_missing_until(cancel).await;
            }
            if cancel.is_cancelled() {
                return Flow::Aborted;
            }
            let system = self.system();
            let sid = self.session_id();
            let wire = self.wire_messages();
            // Every tool call in the history has its result and vice versa (`to_wire` also repairs).
            debug_assert!(mu_base::message::check_tool_pairs(&wire).is_ok(), "unpaired tool calls in the request");
            let defs = self.tool_defs();
            // Held until the reply ends: a reply that turns out to be a dismissal is never shown.
            let answering_reminder = matches!(self.messages.last(), Some(Message::User { content, .. }) if ask_nudge::is_reminder(&text_of(content)));
            let mut held = Vec::new();
            let reply = {
                let req = Request {
                    model: &self.model,
                    thinking: self.live_thinking.clone(),
                    system: &system,
                    tools: &defs,
                    messages: &wire,
                    max_tokens: None,
                    session_id: Some(&sid),
                };
                let tx = self.events.clone();
                let mut ev = |e: StreamEvent| {
                    let e = match e {
                        StreamEvent::Text(t) => AgentEvent::Text(t.into()),
                        StreamEvent::Thinking(t) => AgentEvent::Thinking(t.into()),
                        StreamEvent::ToolCallStart { id, name } => {
                            AgentEvent::ToolCallStart { id: id.into(), name: name.into() }
                        }
                        StreamEvent::ToolCallDelta { id, json } => {
                            AgentEvent::ToolCallDelta { id: id.into(), json: json.into() }
                        }
                        StreamEvent::Retry { attempt, delay, reason } => AgentEvent::Retry { attempt, delay, reason },
                    };
                    if answering_reminder {
                        held.push(e);
                    } else {
                        let _ = tx.send(e);
                    }
                };
                self.providers.stream(&req, cancel, &mut ev).await
            };
            let dismissed = answering_reminder && matches!(&reply, Ok(m) if is_dismissal(m));
            if !dismissed {
                for e in held {
                    self.emit(e);
                }
            }
            let msg = match reply {
                Ok(m) => m,
                Err(ProviderError::Aborted) => return Flow::Aborted,
                Err(ProviderError::Overflow(m)) => {
                    if compacted_for_overflow {
                        self.error("context overflow after compaction");
                        return Flow::Failed;
                    }
                    compacted_for_overflow = true;
                    match self.compact(None, cancel).await {
                        Outcome::Done => {
                            first = true;
                            continue;
                        }
                        Outcome::Skipped(_) => {
                            self.error(format!("error: {m}"));
                            return Flow::Failed;
                        }
                        Outcome::Failed(_) if cancel.is_cancelled() => return Flow::Aborted,
                        Outcome::Failed(e) => {
                            self.error(format!("compaction failed: {e}"));
                            return Flow::Failed;
                        }
                    }
                }
                Err(e) => {
                    self.error(format!("error: {e}"));
                    return Flow::Failed;
                }
            };
            if !dismissed {
                self.emit(AgentEvent::BlockEnd);
            }
            let calls = tool_calls(&msg);
            let stop = match &msg {
                Message::Assistant { stop_reason, .. } => *stop_reason,
                _ => StopReason::Stop,
            };
            let prose_question = stop == StopReason::Stop
                && calls.is_empty()
                && matches!(&msg, Message::Assistant { content, .. } if ask_nudge::asks_in_prose(&text_of(content)));
            self.push(msg);
            self.request_title();
            // Context, cost and quota move with every response, not just at turn end.
            self.refresh_model_meta();
            self.tick_quota();
            self.settle_prices();
            self.emit_status();
            if stop == StopReason::Length {
                self.error("[output truncated: max_tokens reached]");
            }
            if stop != StopReason::ToolUse && !calls.is_empty() {
                // A truncated reply: its tool calls never run, but each still needs a result.
                for (id, name, args) in &calls {
                    let reason = SKIP_TRUNCATED;
                    let out = ToolOutput::err(reason);
                    self.emit(AgentEvent::ToolSkipped { id: id.clone() });
                    self.emit(AgentEvent::ToolStart { id: id.clone(), name: name.clone(), args: args.clone() });
                    self.emit(AgentEvent::ToolEnd {
                        id: id.clone(),
                        name: name.clone(),
                        output: ToolOutput::err(reason),
                    });
                    self.push_result(id, name, &out);
                }
            }
            if stop != StopReason::ToolUse || calls.is_empty() {
                let steered = self.take_steer();
                let delivered = self.take_jobs() | self.take_notify();
                if steered || delivered {
                    continue;
                }
                if self.inbox.borrow().stop {
                    return Flow::Stopped;
                }
                // Stop reminder: the model is about to finish with open todo items. Nudge it to
                // update the list or finish the work, at most twice per run so it cannot loop.
                let open = if stop == StopReason::Stop && todo_nags < 2 { self.tools.todo_open() } else { Vec::new() };
                if !open.is_empty() {
                    todo_nags += 1;
                    let list = open.iter().map(|c| format!("- {c}")).collect::<Vec<_>>().join("\n");
                    self.push(Message::user(&format!(
                        "<system-reminder>\nYou stopped with {} open todo item(s):\n{list}\nContinue the work on them, marking each `done` only when you have actually finished it. Do not tick items off that you did not do. `drop` an item that is no longer needed; `block` it if you are waiting on the user. Do not reply to this reminder.\n</system-reminder>",
                        open.len()
                    )));
                    continue;
                }
                // Ask reminder: the turn ends with a decision question in prose. Once per user turn.
                if prose_question && self.ask_nudge_due() {
                    self.push(Message::user(ask_nudge::REMINDER));
                    continue;
                }
                return Flow::Done;
            }
            let mut skip: Option<&str> = None;
            let mut yielded = false;
            for (id, name, args) in calls {
                if skip.is_none() {
                    if cancel.is_cancelled() {
                        skip = Some(SKIP_ABORTED);
                    } else if self.inbox.borrow().stop {
                        skip = Some(SKIP_STOPPED);
                    }
                }
                if let Some(reason) = skip {
                    let out = ToolOutput::err(reason);
                    self.emit(AgentEvent::ToolSkipped { id: id.clone() });
                    self.emit(AgentEvent::ToolStart { id: id.clone(), name: name.clone(), args: args.clone() });
                    self.emit(AgentEvent::ToolEnd {
                        id: id.clone(),
                        name: name.clone(),
                        output: ToolOutput::err(reason),
                    });
                    self.push_result(&id, &name, &out);
                    continue;
                }
                let out = self.run_tool(&id, &name, args, cancel).await;
                self.push_result(&id, &name, &out);
                yielded |= name == "yield" && !out.is_error;
                if cancel.is_cancelled() {
                    skip = Some(SKIP_ABORTED);
                } else if self.inbox.borrow().stop {
                    skip = Some(SKIP_STOPPED);
                } else if yielded {
                    skip = Some(SKIP_YIELDED);
                } else if !self.inbox.borrow().steer.is_empty() {
                    skip = Some(SKIP_STEER);
                }
            }
            if cancel.is_cancelled() {
                return Flow::Aborted;
            }
            if self.inbox.borrow().stop {
                return Flow::Stopped;
            }
            // A successful yield is the subagent's result; another request would only produce an unused reply.
            if yielded || self.exec_plan.is_some() {
                return Flow::Done;
            }
            self.take_steer();
            self.take_jobs();
            self.take_notify();
        }
    }

    async fn run_tool(&mut self, id: &str, name: &str, mut args: Value, cancel: &CancellationToken) -> ToolOutput {
        let mut blocked = None;
        for (plugin, v) in self.plugins.hook("tool_call", json!({"name": name, "arguments": args})).await {
            if let Some(b) = v["block"].as_str() {
                blocked = Some(format!("Blocked by plugin {plugin}: {b}"));
                break;
            }
            if v["arguments"].is_object() {
                args = v["arguments"].clone();
            }
        }
        self.emit(AgentEvent::ToolStart { id: id.into(), name: name.into(), args: args.clone() });
        let denied = self.policy().check(name).err().or_else(|| secrets::guard(name, &args).err());
        let mut out = match blocked.or(denied) {
            Some(b) => ToolOutput::err(b),
            None => {
                let ctx = ToolCtx { events: self.events.clone(), cancel: cancel.clone(), call_id: id.into() };
                use futures_util::FutureExt;
                match std::panic::AssertUnwindSafe(self.tools.call(name, &args, &ctx)).catch_unwind().await {
                    Ok(o) => o,
                    Err(p) => {
                        let msg = p
                            .downcast_ref::<&str>()
                            .map(|s| s.to_string())
                            .or_else(|| p.downcast_ref::<String>().cloned())
                            .unwrap_or_else(|| "unknown panic".into());
                        ToolOutput::err(format!("tool panicked: {msg}"))
                    }
                }
            }
        };
        let hits = self.redact_output(&mut out, true);
        self.log_redaction(&format!("tool:{name}"), &hits);
        let sub = format!("tool_result:{name}");
        for (_, v) in self
            .plugins
            .hook(&sub, json!({"name": name, "arguments": args, "content": out.text, "isError": out.is_error}))
            .await
        {
            if let Some(a) = v["append"].as_str().filter(|a| !a.is_empty()) {
                out.text.push_str("\n\n");
                out.text.push_str(a);
            }
        }
        let details = out.details.clone();
        if let Some(d) = details.as_ref() {
            let model = d["model"].as_str().map(String::from);
            let kind = d["planDecision"].as_str();
            let (base, now) = match kind {
                Some("execute" | "compact" | "keep") => (review::snapshot_tree(&self.cwd).await.ok(), now_ms()),
                _ => (None, 0),
            };
            match kind {
                Some(k @ ("execute" | "compact")) => {
                    let (_, saved) = self.save_plan(d).await;
                    let stored = match saved {
                        Ok(m) => Some(m.id),
                        Err(e) => {
                            self.error(format!("could not save the plan: {e}"));
                            None
                        }
                    };
                    self.exec_plan = Some(ExecPlan {
                        content: d["content"].as_str().unwrap_or_default().into(),
                        title: d["title"].as_str().unwrap_or_default().into(),
                        uri: d["uri"].as_str().unwrap_or_default().into(),
                        mode: if k == "compact" { PlanMode::Compact } else { PlanMode::Fresh },
                        model,
                        base,
                        approved_at: now,
                        stored,
                    });
                }
                Some("keep") => {
                    let (store, saved) = self.save_plan(d).await;
                    match saved {
                        Ok(m) => {
                            let session = self.session.as_ref().map(|s| s.path.clone());
                            if let Err(e) = store.approve(&m.id, "keep", session.as_deref()) {
                                self.error(format!("could not record the approval: {e}"));
                            }
                        }
                        Err(e) => self.error(format!("could not save the plan: {e}")),
                    }
                    self.set_review_base(review::ReviewBase {
                        tree: base,
                        plan_uri: d["uri"].as_str().map(String::from),
                        since: now,
                    });
                    self.exit_plan();
                    if let Some(m) = model {
                        self.continue_with(&m);
                    }
                }
                Some("park") => {
                    let (store, saved) = self.save_plan(d).await;
                    match saved {
                        Ok(m) => {
                            self.exit_plan();
                            self.stored_plan = None;
                            self.emit(AgentEvent::PlanParked { id: m.id.clone(), path: store.md_path(&m.id) });
                            out.text.push_str(&format!(
                                " Parked as `{0}`; approve it with /plans or `mu plan approve {0}`.",
                                m.id
                            ));
                        }
                        Err(e) => out = ToolOutput::err(format!("Could not park the plan: {e}")),
                    }
                }
                _ => {}
            }
        }
        if name == "todo" && !out.is_error {
            self.emit(AgentEvent::Todo(self.tools.todo_json()));
        }
        self.emit(AgentEvent::ToolEnd {
            id: id.into(),
            name: name.into(),
            output: ToolOutput {
                text: out.text.clone(),
                is_error: out.is_error,
                display: out.display.clone(),
                details: out.details.clone(),
                meta: out.meta.clone(),
                images: Vec::new(),
            },
        });
        out
    }
}

#[cfg(test)]
mod loop_tests {
    use super::*;
    use crate::testing::{
        agent, agent_with, assistant, call, drain, git_repo, results, save_plan_in, tmp_dir, yield_call,
    };
    use mu_base::config::Configs;
    use mu_provider::provider::ProviderError;
    use mu_session::inbox::enqueue;
    use mu_testutil::block_on_local as run;

    #[test]
    fn tool_panic_becomes_an_error_result() {
        run(async {
            let script = vec![
                Ok(assistant(vec![call("t1", "__panic")], StopReason::ToolUse)),
                Ok(assistant(vec![Block::text("done")], StopReason::Stop)),
            ];
            let (mut a, _rx) = agent(script);
            a.turn(Some("go".to_string().into())).await;
            assert!(mu_base::message::check_tool_pairs(&a.messages).is_ok());
            let err = a.messages.iter().find_map(|m| match m {
                Message::ToolResult { is_error: true, content, .. } => Some(format!("{content:?}")),
                _ => None,
            });
            assert!(err.unwrap().contains("tool panicked: boom"));
            assert_eq!(a.messages.len(), 4, "the turn continued after the panic");
        });
    }

    #[test]
    fn secrets_are_replaced_explained_and_never_written_back() {
        run(async {
            let tok = format!("ghp_{}", "a1B2".repeat(9));
            // The command prints the token without holding it whole.
            let print = json!({"command": format!("printf '%s%s' ghp_ {}", &tok[4..])});
            let write_back = json!({"command": "echo [REDACTED:github-token] > f"});
            let script = vec![
                Ok(assistant(
                    vec![Block::ToolCall { id: "t1".into(), name: "bash".into(), arguments: print }],
                    StopReason::ToolUse,
                )),
                Ok(assistant(vec![Block::text("done")], StopReason::Stop)),
                Ok(assistant(
                    vec![Block::ToolCall { id: "t2".into(), name: "bash".into(), arguments: write_back }],
                    StopReason::ToolUse,
                )),
                Ok(assistant(vec![Block::text("done")], StopReason::Stop)),
            ];
            let (mut a, _rx) = agent(script);
            a.turn(Some("go".to_string().into())).await;
            a.turn(Some(format!("mine is {tok}").into())).await;
            let texts: Vec<(bool, String)> = a
                .messages
                .iter()
                .filter_map(|m| match m {
                    Message::ToolResult { content, is_error, .. } => Some((*is_error, text_of(content))),
                    Message::User { content, .. } => Some((false, text_of(content))),
                    _ => None,
                })
                .collect();
            assert!(texts.iter().all(|(_, t)| !t.contains(&tok)), "{texts:?}");
            let printed = &texts[1].1;
            assert!(printed.starts_with("[REDACTED:github-token]"), "{printed}");
            assert!(printed.contains("(recognised key format)"), "the model is told: {printed}");
            assert_eq!(texts[2].1, "mine is [REDACTED:github-token]");
            let (refused, why) = &texts[3];
            assert!(*refused && why.contains("placeholder"), "{why}");
            // Each replacement is logged by name, never by value.
            let file = std::fs::read_to_string(&a.session.as_ref().unwrap().path).unwrap();
            assert!(!file.contains(&tok));
            let logged: Vec<Value> = file
                .lines()
                .filter_map(|l| serde_json::from_str::<Value>(l).ok())
                .filter(|v| v["customType"] == "secrets_redacted")
                .map(|v| v["data"]["at"].clone())
                .collect();
            assert_eq!(logged, [json!("tool:bash"), json!("message")]);
        });
    }

    #[test]
    fn truncated_reply_with_calls_stays_paired() {
        run(async {
            let script = vec![
                Ok(assistant(vec![call("t1", "read")], StopReason::Length)),
                Ok(assistant(vec![Block::text("ok")], StopReason::Stop)),
            ];
            let (mut a, _rx) = agent(script);
            a.turn(Some("go".to_string().into())).await;
            assert!(mu_base::message::check_tool_pairs(&a.messages).is_ok());
            a.turn(Some("again".to_string().into())).await;
            assert!(mu_base::message::check_tool_pairs(&a.messages).is_ok());
            assert!(a.messages.iter().any(|m| matches!(m, Message::ToolResult { is_error: true, .. })));
        });
    }

    #[test]
    fn plan_prompt_inlines_plan_and_path() {
        let p = ExecPlan {
            content: "do it\n".into(),
            title: "T".into(),
            uri: "local://t-plan.md".into(),
            mode: PlanMode::Compact,
            model: None,
            base: None,
            approved_at: 0,
            stored: None,
        };
        let s = plan_prompt(&p);
        assert!(s.contains("<plan path=\"local://t-plan.md\">\ndo it\n</plan>") && s.contains("History usable"));
        assert!(!s.contains("{{"));
    }

    #[test]
    fn failed_turn_clears_stale_exec_plan() {
        run(async {
            let (mut a, _rx) = agent(vec![Err(ProviderError::Fatal("nope".into()))]);
            a.exec_plan = Some(ExecPlan {
                content: "plan".into(),
                title: String::new(),
                uri: String::new(),
                mode: PlanMode::Fresh,
                model: None,
                base: None,
                approved_at: 0,
                stored: None,
            });
            a.turn(Some("go".to_string().into())).await;
            assert!(a.exec_plan.is_none());
        });
    }

    #[test]
    fn policy_is_enforced_at_dispatch_and_in_defs() {
        run(async {
            let script = vec![
                Ok(assistant(vec![call("t1", "bash")], StopReason::ToolUse)),
                Ok(assistant(vec![Block::text("done")], StopReason::Stop)),
            ];
            let (mut a, _rx) = agent(script);
            assert!(a.tool_defs().iter().any(|d| d.name == "ask"));
            a.allow = Some(vec!["read".into()]);
            a.excluded = vec!["ask"];
            assert!(a.tool_defs().iter().all(|d| d.name == "read"));
            a.turn(Some("go".to_string().into())).await;
            let err = a.messages.iter().find_map(|m| match m {
                Message::ToolResult { is_error: true, content, .. } => Some(format!("{content:?}")),
                _ => None,
            });
            assert!(err.unwrap().contains("not allowed"));
        });
    }

    #[test]
    fn calls_after_a_successful_yield_are_skipped() {
        run(async {
            let script = vec![
                Ok(assistant(
                    vec![yield_call("y1", json!({"data":{"n":1}})), call("t2", "__panic")],
                    StopReason::ToolUse,
                )),
                Err(ProviderError::Fatal("requested after a successful yield".into())),
            ];
            let (mut a, mut rx) = agent(script);
            *a.tools.output_schema.borrow_mut() =
                Some(json!({"type":"object","required":["n"],"properties":{"n":{"type":"integer"}}}));
            a.turn(Some("go".to_string().into())).await;
            assert!(mu_base::message::check_tool_pairs(&a.messages).is_ok());
            let r = results(&a);
            assert_eq!(r.len(), 2, "{r:?}");
            assert!(!r[0].0 && r[0].1.contains("Result submitted"), "{:?}", r[0]);
            assert!(r[1].0 && r[1].1.contains("already submitted with `yield`"), "{:?}", r[1]);
            let mut skipped = Vec::new();
            let mut ended = None;
            while let Ok(e) = rx.try_recv() {
                match e {
                    AgentEvent::TurnEnd { aborted } => ended = Some(aborted),
                    AgentEvent::ToolSkipped { id } => skipped.push(id),
                    _ => {}
                }
            }
            assert_eq!(skipped, ["t2"]);
            assert_eq!(ended, Some(false));
            assert_eq!(a.providers.scripted.borrow().len(), 1);
        });
    }

    #[test]
    fn stop_skips_the_rest_of_the_batch_and_ends_the_turn() {
        run(async {
            let script = vec![
                Ok(assistant(vec![call("t1", "__panic"), call("t2", "__panic")], StopReason::ToolUse)),
                Ok(assistant(vec![Block::text("never")], StopReason::Stop)),
            ];
            let (mut a, mut rx) = agent(script);
            a.inbox.borrow_mut().stop = true;
            a.turn(Some("go".to_string().into())).await;
            assert!(mu_base::message::check_tool_pairs(&a.messages).is_ok());
            let r = results(&a);
            assert_eq!(r.len(), 2);
            assert!(r.iter().all(|x| x.0 && x.1.contains("the run was stopped")), "{r:?}");
            assert_eq!(a.providers.scripted.borrow().len(), 1, "no second request");
            let mut ended = None;
            let mut skipped = Vec::new();
            while let Ok(e) = rx.try_recv() {
                match e {
                    AgentEvent::TurnEnd { aborted } => ended = Some(aborted),
                    AgentEvent::ToolSkipped { id } => skipped.push(id),
                    _ => {}
                }
            }
            assert_eq!(ended, Some(false));
            assert_eq!(skipped, ["t1", "t2"], "each skipped call is marked structurally");
        });
    }

    #[test]
    fn stop_during_a_tool_lets_it_finish_and_skips_the_next() {
        run(async {
            let sleep =
                Block::ToolCall { id: "t1".into(), name: "bash".into(), arguments: json!({"command": "sleep 1"}) };
            let script = vec![
                Ok(assistant(vec![sleep, call("t2", "__panic")], StopReason::ToolUse)),
                Ok(assistant(vec![Block::text("never")], StopReason::Stop)),
            ];
            let (mut a, mut rx) = agent(script);
            let inbox = a.inbox.clone();
            tokio::task::spawn_local(async move {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                inbox.borrow_mut().stop = true;
            });
            a.turn(Some("go".to_string().into())).await;
            assert!(mu_base::message::check_tool_pairs(&a.messages).is_ok());
            let r = results(&a);
            assert_eq!(r.len(), 2);
            assert!(!r[0].0, "the running tool finished: {:?}", r[0]);
            assert!(r[1].0 && r[1].1.contains("the run was stopped"), "{:?}", r[1]);
            let mut skipped = Vec::new();
            while let Ok(e) = rx.try_recv() {
                if let AgentEvent::ToolSkipped { id } = e {
                    skipped.push(id);
                }
            }
            assert_eq!(skipped, ["t2"], "the call that ran is not marked");
            assert_eq!(a.providers.scripted.borrow().len(), 1, "no second request");
        });
    }

    #[test]
    fn stop_with_a_plain_reply_ends_normally() {
        run(async {
            let (mut a, _rx) = agent(vec![Ok(assistant(vec![Block::text("hi")], StopReason::Stop))]);
            a.inbox.borrow_mut().stop = true;
            a.turn(Some("go".to_string().into())).await;
            assert!(matches!(a.messages.last(), Some(Message::Assistant { .. })));
            assert!(a.providers.scripted.borrow().is_empty());
        });
    }

    #[test]
    fn stop_plus_cancel_is_an_abort_and_stays_paired() {
        run(async {
            let sleep =
                Block::ToolCall { id: "t1".into(), name: "bash".into(), arguments: json!({"command": "sleep 5"}) };
            let script = vec![Ok(assistant(vec![sleep, call("t2", "__panic")], StopReason::ToolUse))];
            let (mut a, mut rx) = agent(script);
            let inbox = a.inbox.clone();
            tokio::task::spawn_local(async move {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                inbox.borrow_mut().stop = true;
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                inbox.borrow().cancel.cancel();
            });
            a.turn(Some("go".to_string().into())).await;
            assert!(mu_base::message::check_tool_pairs(&a.messages).is_ok());
            let mut ended = None;
            while let Ok(e) = rx.try_recv() {
                if let AgentEvent::TurnEnd { aborted } = e {
                    ended = Some(aborted);
                }
            }
            assert_eq!(ended, Some(true));
        });
    }

    #[test]
    fn kill_jobs_is_handled_mid_turn_and_other_commands_queue() {
        run(async {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let (etx, mut erx) = tokio::sync::mpsc::unbounded_channel();
            let mut backlog = Default::default();
            tx.send(Command::KillJobs(None)).unwrap();
            tx.send(Command::NewSession).unwrap();
            let live = Rc::new(std::cell::Cell::new(Thinking::Off));
            drive(tokio::time::sleep(std::time::Duration::from_millis(50)), &mut rx, &mut backlog, None, etx, live)
                .await;
            assert!(matches!(erx.try_recv(), Ok(AgentEvent::Notice(m)) if m.contains("killed")));
            assert_eq!(backlog.len(), 1);
            assert!(matches!(backlog.pop_front(), Some(Command::NewSession)));
        });
    }

    #[test]
    fn a_fatal_error_emits_turn_failed_before_turn_end() {
        run(async {
            let (mut a, mut rx) = agent(vec![Err(ProviderError::Fatal("nope".into()))]);
            a.turn(Some("go".to_string().into())).await;
            let mut seen = Vec::new();
            while let Ok(e) = rx.try_recv() {
                match e {
                    AgentEvent::Error(_) => seen.push("error"),
                    AgentEvent::TurnFailed => seen.push("failed"),
                    AgentEvent::TurnEnd { .. } => seen.push("end"),
                    _ => {}
                }
            }
            assert_eq!(seen, ["error", "failed", "end"]);
        });
    }

    // ---- `mu notify` delivery ----

    fn notify_agent(script: Vec<Result<Message, ProviderError>>) -> (Agent, UnboundedReceiver<AgentEvent>) {
        let (mut a, rx) = agent(script);
        a.enable_jobs();
        a.enable_notify();
        (a, rx)
    }

    fn user_text(m: &Message) -> Option<String> {
        match m {
            Message::User { content, .. } => Some(text_of(content)),
            _ => None,
        }
    }

    fn user_texts(a: &Agent) -> Vec<String> {
        a.messages.iter().filter_map(user_text).collect()
    }

    fn session_text(a: &Agent) -> String {
        std::fs::read_to_string(&a.session.as_ref().unwrap().path).unwrap()
    }

    #[test]
    fn a_wake_with_nothing_pending_starts_no_turn() {
        run(async {
            let (mut a, mut rx) = notify_agent(vec![]);
            a.ensure_session().await;
            drain(&mut rx);
            a.turn(None).await;
            assert!(drain(&mut rx).iter().all(|e| !matches!(e, AgentEvent::TurnStart | AgentEvent::Woken)));
        });
    }

    #[test]
    fn a_stop_with_no_calls_skips_the_todo_reminder() {
        use mu_tools::tools::todo::{Phase, Status, Task};
        run(async {
            let script = vec![
                Ok(assistant(vec![Block::text("hi")], StopReason::Stop)),
                Ok(assistant(vec![Block::text("never")], StopReason::Stop)),
            ];
            let (mut a, _rx) = agent(script);
            let task = Task { content: "open item".into(), status: Status::Pending, blocker: None };
            a.tools.set_todo(vec![Phase { name: "p".into(), tasks: vec![task] }]);
            a.inbox.borrow_mut().stop = true;
            a.turn(Some("go".to_string().into())).await;
            assert_eq!(a.messages.len(), 2, "no reminder was pushed: {:?}", a.messages.len());
            assert_eq!(a.providers.scripted.borrow().len(), 1, "no second request");
        });
    }

    /// A turn that ends with a prose question, with `tweak` applied first. Returns the message count
    /// and how many scripted replies were left unused.
    async fn question_turn(tweak: impl FnOnce(&mut Agent)) -> (usize, usize) {
        let (mut a, _rx) = agent(vec![
            Ok(assistant(vec![Block::text("Keep the cache or drop it?")], StopReason::Stop)),
            Ok(assistant(vec![Block::text("never")], StopReason::Stop)),
        ]);
        tweak(&mut a);
        a.turn(Some("go".to_string().into())).await;
        let left = a.providers.scripted.borrow().len();
        (a.messages.len(), left)
    }

    #[test]
    fn ask_nudge_fires_once_then_the_loop_continues() {
        run(async {
            let (mut a, _rx) = agent(vec![
                Ok(assistant(vec![Block::text("Keep the cache or drop it?")], StopReason::Stop)),
                Ok(assistant(vec![Block::text(".")], StopReason::Stop)),
            ]);
            a.turn(Some("go".to_string().into())).await;
            assert_eq!(a.messages.len(), 4);
            assert!(
                matches!(&a.messages[2], Message::User { content, .. } if text_of(content) == mu_base::ask_nudge::REMINDER)
            );
            assert!(a.providers.scripted.borrow().is_empty(), "the loop continued once");
        });
    }

    #[test]
    fn ask_nudge_is_not_repeated_in_the_same_turn() {
        run(async {
            let (mut a, _rx) = agent(vec![
                Ok(assistant(vec![Block::text("Keep the cache or drop it?")], StopReason::Stop)),
                Ok(assistant(vec![Block::text("Still keep it or drop it?")], StopReason::Stop)),
                Ok(assistant(vec![Block::text("never")], StopReason::Stop)),
            ]);
            a.turn(Some("go".to_string().into())).await;
            assert_eq!(a.messages.len(), 4);
            assert_eq!(a.providers.scripted.borrow().len(), 1, "no third request");
        });
    }

    #[test]
    fn ask_nudge_is_skipped_after_an_ask_call() {
        run(async {
            let (mut a, rx) = agent(vec![
                Ok(assistant(vec![call("t1", "ask")], StopReason::ToolUse)),
                Ok(assistant(vec![Block::text("Keep the cache or drop it?")], StopReason::Stop)),
                Ok(assistant(vec![Block::text("never")], StopReason::Stop)),
            ]);
            // Nobody listens for the overlay: the ask call ends at once.
            drop(rx);
            a.turn(Some("go".to_string().into())).await;
            assert_eq!(a.messages.len(), 4, "user, ask call, its result, the question: no reminder");
            assert_eq!(a.providers.scripted.borrow().len(), 1, "no third request");
        });
    }

    #[test]
    fn ask_nudge_is_off_without_the_ask_tool() {
        run(async {
            let (n, left) = question_turn(|a| a.excluded = vec!["ask"]).await;
            assert_eq!((n, left), (2, 1), "-p, ACP and custom agents exclude ask");
            let (n, left) = question_turn(|a| a.subagent = true).await;
            assert_eq!((n, left), (2, 1), "subagents");
            let (n, left) = question_turn(|a| a.allow = Some(vec!["read".into()])).await;
            assert_eq!((n, left), (2, 1), "an allow list without ask");
        });
    }

    #[test]
    fn ask_nudge_follows_the_config_switch() {
        run(async {
            let mut cfg = Configs::default();
            cfg.mu.ask.nudge = false;
            let (mut a, _rx) = agent_with(
                vec![
                    Ok(assistant(vec![Block::text("Keep the cache or drop it?")], StopReason::Stop)),
                    Ok(assistant(vec![Block::text("never")], StopReason::Stop)),
                ],
                cfg,
            );
            a.turn(Some("go".to_string().into())).await;
            assert_eq!(a.messages.len(), 2);
            assert_eq!(a.providers.scripted.borrow().len(), 1);
        });
    }

    #[test]
    fn each_user_turn_gets_its_own_nudge() {
        run(async {
            let (mut a, _rx) = agent(vec![
                Ok(assistant(vec![Block::text("Keep it or drop it?")], StopReason::Stop)),
                Ok(assistant(vec![Block::text(".")], StopReason::Stop)),
                Ok(assistant(vec![Block::text("Keep it or drop it?")], StopReason::Stop)),
                Ok(assistant(vec![Block::text(".")], StopReason::Stop)),
            ]);
            a.turn(Some("one".to_string().into())).await;
            a.turn(Some("two".to_string().into())).await;
            assert_eq!(a.messages.len(), 8);
            assert!(a.providers.scripted.borrow().is_empty());
        });
    }

    #[test]
    fn the_nudge_is_not_echoed_on_resume() {
        run(async {
            let (mut a, mut rx) = agent(vec![]);
            a.messages.push(Message::user(mu_base::ask_nudge::REMINDER));
            a.replay_history();
            assert!(drain(&mut rx).iter().all(|e| !matches!(e, AgentEvent::UserPrompt(_))));
        });
    }

    #[test]
    fn a_dismissal_is_kept_in_history_but_not_streamed() {
        run(async {
            let (mut a, mut rx) = agent(vec![
                Ok(assistant(vec![Block::text("Keep the cache or drop it?")], StopReason::Stop)),
                Ok(assistant(vec![Block::text(".")], StopReason::Stop)),
            ]);
            a.turn(Some("go".to_string().into())).await;
            assert_eq!(a.messages.len(), 4);
            assert!(
                matches!(&a.messages[3], Message::Assistant { content, .. } if text_of(content) == "."),
                "kept in history"
            );
            let events = drain(&mut rx);
            assert!(events.iter().all(|e| !matches!(e, AgentEvent::Text(t) if t == ".")), "not streamed");
            assert!(events.iter().any(|e| matches!(e, AgentEvent::Text(t) if t == "Keep the cache or drop it?")));
        });
    }

    #[test]
    fn a_dismissal_is_not_rendered_on_resume() {
        run(async {
            let (mut a, mut rx) = agent(vec![]);
            a.messages.push(Message::user("go"));
            a.messages.push(assistant(vec![Block::text("Keep it or drop it?")], StopReason::Stop));
            a.messages.push(Message::user(mu_base::ask_nudge::REMINDER));
            a.messages.push(assistant(vec![Block::text(".")], StopReason::Stop));
            a.replay_history();
            let events = drain(&mut rx);
            assert!(events.iter().all(|e| !matches!(e, AgentEvent::Text(t) if t == ".")));
            assert!(events.iter().any(|e| matches!(e, AgentEvent::Text(t) if t == "Keep it or drop it?")));
        });
    }

    #[test]
    fn a_stop_skips_plan_dispatch_and_follow_ups() {
        run(async {
            let (mut a, mut rx) = agent(vec![
                Ok(assistant(vec![Block::text("hi")], StopReason::Stop)),
                Ok(assistant(vec![Block::text("never")], StopReason::Stop)),
            ]);
            a.exec_plan = Some(ExecPlan {
                content: "plan".into(),
                title: "T".into(),
                uri: "local://t-plan.md".into(),
                mode: PlanMode::Fresh,
                model: None,
                base: None,
                approved_at: 0,
                stored: None,
            });
            a.inbox.borrow_mut().follow_up.push_back("later".to_string().into());
            a.inbox.borrow_mut().stop = true;
            a.turn(Some("go".to_string().into())).await;
            assert!(a.exec_plan.is_none(), "the pending plan is dropped, not dispatched");
            assert_eq!(a.inbox.borrow().follow_up.len(), 1, "the follow-up stays queued");
            assert_eq!(a.providers.scripted.borrow().len(), 1, "no second request");
            assert_eq!(a.messages.len(), 2);
            while let Ok(e) = rx.try_recv() {
                assert!(!matches!(e, AgentEvent::Notice(n) if n.contains("Plan approved")));
            }
        });
    }

    #[test]
    fn a_wake_delivers_the_queued_message_and_empties_the_inbox() {
        run(async {
            let (mut a, mut rx) = notify_agent(vec![Ok(assistant(vec![Block::text("PONG")], StopReason::Stop))]);
            a.ensure_session().await;
            let id = a.session_id();
            enqueue(&id, "steward", "say PONG").unwrap();
            a.turn(None).await;
            let evs = drain(&mut rx);
            assert!(evs.iter().any(|e| matches!(e, AgentEvent::Woken)));
            assert!(
                evs.iter()
                    .any(|e| matches!(e, AgentEvent::Notice(n) if n.contains("mu notify from steward: say PONG")))
            );
            assert!(session_text(&a).contains("Message from steward (via mu notify"));
            assert!(!inbox::has_pending(&id));
            assert!(a.providers.scripted.borrow().is_empty());
        });
    }

    #[test]
    fn a_message_during_a_turn_joins_that_turn() {
        run(async {
            let sleep =
                Block::ToolCall { id: "t1".into(), name: "bash".into(), arguments: json!({"command": "sleep 0.3"}) };
            let script = vec![
                Ok(assistant(vec![sleep], StopReason::ToolUse)),
                Ok(assistant(vec![Block::text("one")], StopReason::Stop)),
            ];
            let (mut a, mut rx) = notify_agent(script);
            a.ensure_session().await;
            let id = a.session_id();
            tokio::task::spawn_local(async move {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                enqueue(&id, "steward", "also HELLO").unwrap();
            });
            a.turn(Some("go".to_string().into())).await;
            assert!(a.providers.scripted.borrow().is_empty(), "two requests in one turn");
            let evs = drain(&mut rx);
            assert_eq!(evs.iter().filter(|e| matches!(e, AgentEvent::TurnStart)).count(), 1);
            assert!(!evs.iter().any(|e| matches!(e, AgentEvent::Woken)));
            let at = |f: &dyn Fn(&Message) -> bool| a.messages.iter().position(f).unwrap();
            let result = at(&|m| matches!(m, Message::ToolResult { .. }));
            let notice = at(&|m| user_text(m).is_some_and(|t| t.contains("also HELLO")));
            let reply = at(&|m| matches!(m, Message::Assistant { content, .. } if text_of(content) == "one"));
            assert!(result < notice && notice < reply, "the notice goes out before the model's next reply");
            assert!(!inbox::has_pending(&a.session_id()));
        });
    }

    #[test]
    fn a_print_run_does_not_fall_back_to_a_fresh_session_when_resume_fails() {
        mu_testutil::home();
        run(async {
            let (mut a, mut rx) = agent(vec![Ok(assistant(vec![Block::text("hi")], StopReason::Stop))]);
            a.exit_on_resume_failure = true;
            let (tx, crx) = tokio::sync::mpsc::unbounded_channel();
            let missing = std::env::temp_dir().join(format!("mu-missing-{}.jsonl", uuid::Uuid::new_v4()));
            tx.send(Command::Resume(missing)).unwrap();
            tx.send(Command::EnsureSession).unwrap();
            tx.send(Command::Prompt("go".to_string().into())).unwrap();
            tx.send(Command::Exit).unwrap();
            let providers = a.providers.clone();
            a.run(crx).await;
            assert_eq!(providers.scripted.borrow().len(), 1, "the prompt never ran");
            let (mut errored, mut exited, mut sessions) = (false, false, false);
            while let Ok(e) = rx.try_recv() {
                match e {
                    AgentEvent::Error(_) => errored = true,
                    AgentEvent::Exit => exited = true,
                    AgentEvent::Status(s) if s.session_path.is_some() => sessions = true,
                    _ => {}
                }
            }
            assert!(errored && exited && !sessions);
        });
    }

    #[test]
    fn parking_a_plan_saves_it_and_leaves_plan_mode() {
        run(async {
            let args = json!({"slug": "t", "title": "T"});
            let call = Block::ToolCall { id: "p1".into(), name: "propose_plan".into(), arguments: args };
            let (mut a, mut rx) = agent(vec![
                Ok(assistant(vec![call], StopReason::ToolUse)),
                Ok(assistant(vec![Block::text("parked")], StopReason::Stop)),
            ]);
            a.cwd = tmp_dir();
            a.enter_plan().await;
            let r = a.tools.resolve("local://t-plan.md").unwrap();
            std::fs::create_dir_all(r.path.parent().unwrap()).unwrap();
            std::fs::write(&r.path, "do it\n").unwrap();
            let answer = async {
                let mut parked = None;
                while let Some(e) = rx.recv().await {
                    match e {
                        AgentEvent::PlanReview { reply, stored, .. } => {
                            assert!(!stored);
                            let _ = reply.send(PlanDecision::Park);
                        }
                        AgentEvent::PlanParked { id, .. } => parked = Some(id),
                        AgentEvent::TurnEnd { .. } => break,
                        _ => {}
                    }
                }
                parked
            };
            let (_, parked) = futures_util::future::join(a.turn(Some("go".to_string().into())), answer).await;
            assert_eq!(parked.as_deref(), Some("t"));
            assert!(a.plan.is_none());
            let store = plans::store_for(&a.cwd).await;
            let (meta, text) = store.load("t").unwrap();
            assert_eq!((meta.status, text.as_str()), (mu_base::events::Status::Pending, "do it\n"));
        });
    }

    #[test]
    fn a_stored_plan_starts_in_the_current_session() {
        run(async {
            let (mut a, _rx) = agent(vec![Ok(assistant(vec![Block::text("done")], StopReason::Stop))]);
            a.cwd = git_repo();
            let store = save_plan_in(&a.cwd, &["a.txt"]).await;
            a.turn_from(Start::Plan(ExecPlan {
                content: "do it\n".into(),
                title: "T".into(),
                uri: "local://t-plan.md".into(),
                mode: PlanMode::Keep,
                model: None,
                base: review::snapshot_tree(&a.cwd).await.ok(),
                approved_at: 1,
                stored: Some("t".into()),
            }))
            .await;
            assert!(a.providers.scripted.borrow().is_empty(), "exactly one request was made");
            let plan_msg = a.messages.iter().find_map(|m| match m {
                Message::User { content, synthetic: true, .. } => Some(text_of(content)),
                _ => None,
            });
            assert!(plan_msg.unwrap().contains("<plan path=\"local://t-plan.md\">"));
            let (meta, _) = store.load("t").unwrap();
            assert_eq!(meta.status, mu_base::events::Status::Approved);
            let session = a.session.as_ref().map(|s| s.path.clone());
            assert!(session.is_some() && meta.approved.unwrap().session == session);
            assert_eq!(a.review_base.unwrap().plan_uri.as_deref(), Some("local://t-plan.md"));
        });
    }

    #[test]
    fn messages_waiting_at_open_are_announced_and_go_out_with_the_next_prompt() {
        run(async {
            let (mut a, mut rx) = notify_agent(vec![Ok(assistant(vec![Block::text("ok")], StopReason::Stop))]);
            a.ensure_session().await;
            let (id, path) = (a.session_id(), a.session.as_ref().unwrap().path.clone());
            a.end_session().await;
            a.reset_conversation();
            enqueue(&id, "steward", "later").unwrap();
            drain(&mut rx);
            a.load_session(&path).await;
            let evs = drain(&mut rx);
            assert!(
                evs.iter()
                    .any(|e| matches!(e, AgentEvent::Notice(n) if n.contains("1 message(s) from mu notify waiting")))
            );
            assert!(!evs.iter().any(|e| matches!(e, AgentEvent::TurnStart)));
            assert!(inbox::has_pending(&id));
            a.turn(None).await;
            assert!(drain(&mut rx).iter().all(|e| !matches!(e, AgentEvent::TurnStart)), "announced is not a wake");
            a.turn(Some("hi".to_string().into())).await;
            let users = user_texts(&a);
            assert_eq!(users.len(), 2, "{users:?}");
            assert!(users[0].contains("Message from steward") && users[1] == "hi", "{users:?}");
            assert!(!inbox::has_pending(&id));
        });
    }

    #[test]
    fn the_waiting_notice_follows_the_replayed_history() {
        run(async {
            let (mut a, mut rx) =
                notify_agent(vec![Ok(assistant(vec![Block::text("earlier reply")], StopReason::Stop))]);
            a.ensure_session().await;
            a.turn(Some("earlier prompt".to_string().into())).await;
            let (id, path) = (a.session_id(), a.session.as_ref().unwrap().path.clone());
            a.end_session().await;
            a.reset_conversation();
            enqueue(&id, "steward", "later").unwrap();
            drain(&mut rx);
            a.load_session(&path).await;
            let evs = drain(&mut rx);
            let pos = |f: &dyn Fn(&AgentEvent) -> bool| evs.iter().position(f);
            let waiting: Vec<usize> = evs
                .iter()
                .enumerate()
                .filter(
                    |(_, e)| matches!(e, AgentEvent::Notice(n) if n.contains("1 message(s) from mu notify waiting")),
                )
                .map(|(i, _)| i)
                .collect();
            assert_eq!(waiting.len(), 1, "notice emitted exactly once");
            let prompt =
                pos(&|e| matches!(e, AgentEvent::UserPrompt(t) if t == "earlier prompt")).expect("prompt replayed");
            let reply = pos(&|e| matches!(e, AgentEvent::Text(t) if t == "earlier reply")).expect("reply replayed");
            assert!(prompt < waiting[0] && reply < waiting[0], "notice follows the replayed history");
            assert!(inbox::has_pending(&id), "still delivered with the next prompt, not at open");
        });
    }

    #[test]
    fn the_real_loop_wakes_on_the_channel() {
        run(async {
            let (mut a, mut rx) = agent(vec![Ok(assistant(vec![Block::text("PONG")], StopReason::Stop))]);
            a.enable_jobs();
            let wake = a.enable_notify().unwrap();
            a.ensure_session().await;
            let (id, path) = (a.session_id(), a.session.as_ref().unwrap().path.clone());
            let (ctx, crx) = tokio::sync::mpsc::unbounded_channel();
            let task = tokio::task::spawn_local(a.run(crx));
            enqueue(&id, "steward", "say PONG").unwrap();
            wake.send(()).unwrap();
            let waited = tokio::time::timeout(std::time::Duration::from_secs(20), async {
                while let Some(e) = rx.recv().await {
                    if matches!(e, AgentEvent::TurnEnd { .. }) {
                        return;
                    }
                }
            })
            .await;
            assert!(waited.is_ok(), "no turn ran");
            ctx.send(Command::Exit).unwrap();
            drop(ctx);
            let _ = task.await;
            assert!(std::fs::read_to_string(path).unwrap().contains("Message from steward (via mu notify"));
            assert!(!inbox::has_pending(&id));
        });
    }

    #[test]
    fn a_drifted_plan_is_refused_without_anyway() {
        run(async {
            let (mut a, mut rx) = agent(vec![Ok(assistant(vec![Block::text("never")], StopReason::Stop))]);
            a.cwd = git_repo();
            save_plan_in(&a.cwd, &["a.txt"]).await;
            std::fs::write(a.cwd.join("a.txt"), "two\n").unwrap();
            let start = a.start_plan("t", PlanDecision::Keep { model: None }, false).await;
            assert!(start.is_none());
            assert_eq!(a.providers.scripted.borrow().len(), 1, "no request was made");
            let mut stale = false;
            while let Ok(e) = rx.try_recv() {
                stale |= matches!(e, AgentEvent::Error(m) if m.contains("is stale"));
            }
            assert!(stale);
            assert!(a.start_plan("t", PlanDecision::Keep { model: None }, true).await.is_some());
        });
    }

    #[test]
    fn two_senders_and_one_wake_give_one_notice() {
        run(async {
            let (mut a, _rx) = notify_agent(vec![Ok(assistant(vec![Block::text("ok")], StopReason::Stop))]);
            a.ensure_session().await;
            let id = a.session_id();
            enqueue(&id, "a", "first").unwrap();
            enqueue(&id, "b", "second").unwrap();
            a.turn(None).await;
            let users = user_texts(&a);
            assert_eq!(users.len(), 1, "{users:?}");
            assert!(users[0].contains("Message from a") && users[0].contains("Message from b"));
        });
    }

    #[test]
    fn a_native_model_pauses_the_quota_and_shows_no_windows() {
        run(async {
            let (mut a, _rx) = agent(vec![]);
            a.quota.enabled = true;
            a.quota.set_usage(mu_base::events::ProviderUsage { provider: "anthropic".into(), windows: vec![] });
            a.model = ModelSpec::gateway("anthropic/m", 200_000, 8192);
            a.model.native = true;
            a.sync_model_state();
            assert!(a.quota.wait() >= std::time::Duration::from_secs(3600), "the run loop must not spin");
            assert!(a.snapshot().usage.is_none(), "subscription windows never show under a per-token model");
            assert!(!a.pricing_checked, "a native-only session never starts omp");
            a.model.native = false;
            a.sync_model_state();
            assert!(a.pricing_checked, "the first gateway model reads omp's catalogue, once");
        });
    }

    #[test]
    fn without_notify_pending_files_are_ignored() {
        run(async {
            let (mut a, mut rx) = agent(vec![]);
            a.enable_jobs(); // interactive-like, but `enable_notify` was never called (-p, ACP)
            a.ensure_session().await;
            let (id, path) = (a.session_id(), a.session.as_ref().unwrap().path.clone());
            enqueue(&id, "steward", "x").unwrap();
            a.turn(None).await;
            assert!(drain(&mut rx).iter().all(|e| !matches!(e, AgentEvent::TurnStart)));
            a.end_session().await;
            a.load_session(&path).await;
            assert!(drain(&mut rx).iter().all(|e| !matches!(e, AgentEvent::Notice(n) if n.contains("waiting"))));
            assert!(inbox::has_pending(&id));
        });
    }
}
