use crate::compaction::Outcome;
use crate::context::{self, Context, PromptInput};
use crate::cost::CostLedger;
use crate::events::{AgentEvent, Command, EventTx, SharedInbox, StatusSnapshot};
use crate::gitinfo::{self, GitInfo};
use crate::jobs::{self, JobsHandle};
use crate::mem;
use crate::message::UserInput;
use crate::message::{Block, Message, StopReason, now_ms, text_of};
use crate::models::{ModelSpec, Thinking};
use crate::plugin::{InitParams, PluginHost};
use crate::policy::ToolPolicy;
use crate::pricing;
use crate::provider::{ProviderError, Providers, Request, StreamEvent, ToolDef};
use crate::quota::QuotaTracker;
use crate::session::{self, CompactionState, SessionFile};
use crate::tools::{ToolCtx, ToolOutput, Tools};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
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
    turn_msgs: Vec<Value>,
    jobs: Option<Rc<JobsHandle>>,
    wake_rx: Option<UnboundedReceiver<()>>,
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
    compact: bool,
    model: Option<String>,
}

const PLAN_APPROVED: &str = include_str!("prompts/plan-approved.md");

/// The hidden prompt that starts executing an approved plan.
fn plan_prompt(p: &ExecPlan) -> String {
    let context = if p.compact {
        "- History usable; the plan below authoritative if it conflicts with earlier exploration.\n"
    } else {
        ""
    };
    PLAN_APPROVED.replace("{{context}}", context).replace("{{path}}", &p.uri).replace("{{plan}}", p.content.trim_end())
}

const PLAN_COMPACT_FOCUS: &str = "The user approved an implementation plan; it will be supplied in full right after this summary. Preserve what implementing it needs: decisions, constraints, file paths, symbols, and findings from exploration. Omit the plan text itself.";

const PLAN_PROMPT: &str = include_str!("prompts/plan-mode.md");

pub struct AgentInit {
    pub providers: Rc<Providers>,
    pub plugins: Rc<PluginHost>,
    pub cwd: PathBuf,
    pub ctx: Rc<Context>,
    pub model: ModelSpec,
    pub thinking: Thinking,
    pub events: EventTx,
    pub inbox: SharedInbox,
}

/// How a loop pass ended.
enum Flow {
    Done,
    Aborted,
    Failed,
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

fn tool_calls(m: &Message) -> Vec<(String, String, Value)> {
    match m {
        Message::Assistant { content, .. } => content
            .iter()
            .filter_map(|b| match b {
                Block::ToolCall { id, name, arguments } => Some((id.clone(), name.clone(), arguments.clone())),
                _ => None,
            })
            .collect(),
        _ => vec![],
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
            turn_msgs: Vec::new(),
            jobs: None,
            wake_rx: None,
        }
    }

    /// Interactive sessions only: `task` runs in the background and finished jobs wake the agent.
    pub fn enable_jobs(&mut self) {
        let (wake, wake_rx) = unbounded_channel();
        let jobs = JobsHandle::new(wake, self.events.clone());
        self.tools.jobs = Some(jobs.clone());
        self.jobs = Some(jobs);
        self.wake_rx = Some(wake_rx);
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
        let policy = self.policy();
        v.retain(|d| policy.permits(&d.name));
        v
    }

    /// The tool limits for this run; enforced in `run_tool` and used to build the advertised list.
    pub fn policy(&self) -> ToolPolicy {
        ToolPolicy {
            allow: self.allow.clone(),
            excluded: self.excluded.clone(),
            depth: crate::tools::task::depth(),
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
        context::system_prompt(&PromptInput {
            model: &self.model.id,
            cwd: &self.cwd,
            ctx: &self.ctx,
            plan: self.plan.as_ref().map(|_| PLAN_PROMPT),
            subagent: self.subagent,
            agent_prompt: self.agent_prompt.as_deref(),
            plugin_ctx: &self.plugin_ctx,
        })
    }

    /// Tokens of the system prompt, measured again only when what it is built from changes.
    pub fn system_tokens(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        (&self.model.id, self.plan.is_some(), &self.plugin_ctx, &self.agent_prompt, self.subagent).hash(&mut h);
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
            subscription: self.quota.is_subscription(&self.model.provider),
            plan: self.plan.is_some(),
            context_tokens: Some(self.context_tokens()),
            context_window: self.model.context_window,
            cost: self.cost.total,
            tokens_in: self.cost.tokens_in,
            tokens_out: self.cost.tokens_out,
            title: self.title.clone(),
            session_path: self.session.as_ref().map(|s| s.path.to_string_lossy().into_owned()),
            git: self.git.clone(),
            usage: self.quota.usage_for(&self.model.provider),
        }
    }

    /// Git state for the status line. Runs between turns, never from the draw path.
    async fn refresh_external(&mut self) {
        self.git = gitinfo::git_status(&self.cwd).await;
        self.tick_quota();
        self.settle_prices();
    }

    fn tick_quota(&mut self) {
        let provider = self.model.provider.clone();
        self.quota.tick(&provider);
    }

    /// Prices usage recorded before the table knew its model. A model the table does not list forces
    /// one catalogue refresh per process.
    fn settle_prices(&mut self) {
        if self.cost.settle() && self.quota.enabled {
            tokio::task::spawn_local(pricing::refresh_missing());
        }
    }

    /// Picks up gateway-reported context windows once they are known.
    fn refresh_model_meta(&mut self) {
        if let Ok((spec, _)) = self.providers.resolve(&self.model.id) {
            self.model = spec;
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
            let changed = match self.plugin_ctx.get(&plugin) {
                Some(old) => old != ctx,
                None => !ctx.is_empty(),
            };
            if !changed {
                continue;
            }
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

    fn reset_conversation(&mut self) {
        self.session = None;
        self.messages.clear();
        self.entry_ids.clear();
        self.plugin_ctx.clear();
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
        for m in &self.messages {
            match m {
                Message::User { synthetic: true, .. } => self.notice("▸ Plan approved"),
                Message::User { content, .. } => self.emit(AgentEvent::UserPrompt(text_of(content))),
                Message::Assistant { content, .. } => {
                    for b in content {
                        match b {
                            Block::Thinking { thinking, .. } if !thinking.trim().is_empty() => {
                                self.emit(AgentEvent::Thinking(thinking.clone()));
                            }
                            Block::Text { text } => {
                                self.emit(AgentEvent::Text(text.clone()));
                                self.emit(AgentEvent::BlockEnd);
                            }
                            Block::ToolCall { id, name, arguments } => {
                                self.emit(AgentEvent::ToolStart {
                                    id: id.clone(),
                                    name: name.clone(),
                                    args: arguments.clone(),
                                });
                            }
                            _ => {}
                        }
                    }
                }
                Message::ToolResult { tool_call_id, tool_name, content, is_error, details, .. } => {
                    let output = ToolOutput::from_stored(text_of(content), *is_error, details.clone());
                    self.emit(AgentEvent::ToolEnd { id: tool_call_id.clone(), name: tool_name.clone(), output });
                }
            }
        }
    }

    pub async fn load_session(&mut self, path: &Path) {
        let loaded = match session::load(path) {
            Ok(l) => l,
            Err(e) => return self.error(e.to_string()),
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
            Err(e) => return self.error(e.to_string()),
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
        self.entry_ids = loaded.entry_ids;
        self.plugin_ctx = loaded.plugin_context;
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
        self.emit_status();
    }

    pub async fn run(mut self, mut rx: UnboundedReceiver<Command>) {
        self.quota.enabled = true;
        tokio::task::spawn_local(pricing::refresh());
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
                Command::SetThinking(t) => self.set_thinking(t),
                Command::NewSession => {
                    self.end_session().await;
                    self.reset_conversation();
                    self.notice("new session");
                    self.emit_status();
                }
                Command::Resume(p) => {
                    self.load_session(&p).await;
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
        let out = self.tools.call("bash", &args, &ctx).await;
        if include {
            self.push(Message::user(&format!("I ran `{cmd}`:\n```\n{}\n```", out.text)));
            self.turn_msgs.clear(); // not a model turn: no `turn_end` hook for it
        }
        self.emit(AgentEvent::ToolEnd { id, name: "bash".into(), output: out });
        self.emit(AgentEvent::TurnEnd { aborted: cancel.is_cancelled() });
    }

    /// One turn. `first` is `None` for a wake-up that only delivers finished background jobs.
    async fn turn(&mut self, first: Option<UserInput>) {
        if first.is_none() && !self.jobs.as_ref().is_some_and(|j| j.has_undelivered()) {
            return;
        }
        if !self.ensure_session().await {
            return;
        }
        let cancel = CancellationToken::new();
        self.inbox.borrow_mut().cancel = cancel.clone();
        self.emit(AgentEvent::TurnStart);
        let mut next = Some(first);
        let mut aborted = false;
        let mut synthetic = false;
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
                    let res = self
                        .plugins
                        .hook(
                            "prompt_submit",
                            json!({"prompt": i.text, "userTurn": self.user_turn, "resumed": self.resumed}),
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
                Some(i) if synthetic => self.push(Message::synthetic_user(&i.text)),
                Some(i) => self.push(Message::user_input(i)),
                None => {
                    self.take_jobs();
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
                    self.exec_plan = None;
                    break;
                }
                Flow::Done => {}
            }
            self.turn_end_hook().await;
            if let Some(p) = self.exec_plan.take() {
                self.exit_plan();
                if let Some(m) = &p.model {
                    self.continue_with(m);
                }
                if p.compact {
                    match self.compact(Some(PLAN_COMPACT_FOCUS), &cancel).await {
                        Outcome::Done => {}
                        Outcome::Skipped(why) => self.notice(why),
                        Outcome::Failed(e) if !cancel.is_cancelled() => self.error(format!("compaction failed: {e}")),
                        Outcome::Failed(_) => {}
                    }
                    if cancel.is_cancelled() {
                        self.notice("Plan approved, but compaction was cancelled — execution not dispatched. Submit a turn to continue.");
                        aborted = true;
                        break;
                    }
                } else {
                    self.end_session().await;
                    self.reset_conversation();
                    self.notice("── new session");
                    if self.ensure_session().await {
                        self.seed_plan(&p);
                    }
                }
                self.notice(format!("▸ Plan approved: {}", p.title));
                synthetic = true;
                next = Some(Some(plan_prompt(&p).into()));
                continue;
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
            let system = self.system();
            let wire = self.wire_messages();
            // Every tool call in the history has its result and vice versa (`to_wire` also repairs).
            debug_assert!(crate::message::check_tool_pairs(&wire).is_ok(), "unpaired tool calls in the request");
            let defs = self.tool_defs();
            let reply = {
                let req = Request {
                    model: &self.model,
                    thinking: self.live_thinking.clone(),
                    system: &system,
                    tools: &defs,
                    messages: &wire,
                    max_tokens: None,
                };
                let tx = self.events.clone();
                let mut ev = |e: StreamEvent| {
                    let _ = tx.send(match e {
                        StreamEvent::Text(t) => AgentEvent::Text(t.into()),
                        StreamEvent::Thinking(t) => AgentEvent::Thinking(t.into()),
                        StreamEvent::ToolCallStart { id, name } => {
                            AgentEvent::ToolCallStart { id: id.into(), name: name.into() }
                        }
                        StreamEvent::ToolCallDelta { id, json } => {
                            AgentEvent::ToolCallDelta { id: id.into(), json: json.into() }
                        }
                        StreamEvent::Retry { attempt, delay, reason } => AgentEvent::Retry { attempt, delay, reason },
                    });
                };
                self.providers.stream(&req, cancel, &mut ev).await
            };
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
            self.emit(AgentEvent::BlockEnd);
            let calls = tool_calls(&msg);
            let stop = match &msg {
                Message::Assistant { stop_reason, .. } => *stop_reason,
                _ => StopReason::Stop,
            };
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
                    let reason = "Tool call not executed: output was truncated (max_tokens)";
                    let out = ToolOutput::err(reason);
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
                let delivered = self.take_jobs();
                if steered || delivered {
                    continue;
                }
                // Stop reminder: the model is about to finish with open todo items. Nudge it to
                // update the list or finish the work, at most twice per run so it cannot loop.
                let open = if stop == StopReason::Stop && todo_nags < 2 { self.tools.todo_open() } else { Vec::new() };
                if !open.is_empty() {
                    todo_nags += 1;
                    let list = open.iter().map(|c| format!("- {c}")).collect::<Vec<_>>().join("\n");
                    self.push(Message::user(&format!(
                        "<system-reminder>\nYou stopped with {} open todo item(s):\n{list}\nContinue the work and keep the list current with `todo` (`done`/`start`, in the same response as your next action). If an item is finished or no longer needed, mark it `done` or `drop` it; if you are waiting on the user, `block` it. Do not reply to this reminder.\n</system-reminder>",
                        open.len()
                    )));
                    continue;
                }
                return Flow::Done;
            }
            let mut skip: Option<&str> = None;
            for (id, name, args) in calls {
                if let Some(reason) = skip {
                    let out = ToolOutput::err(reason);
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
                if cancel.is_cancelled() {
                    skip = Some("Tool call aborted by user");
                } else if !self.inbox.borrow().steer.is_empty() {
                    skip = Some("Skipped: the user sent a new message");
                }
            }
            if cancel.is_cancelled() {
                return Flow::Aborted;
            }
            if self.exec_plan.is_some() {
                return Flow::Done;
            }
            self.take_steer();
            self.take_jobs();
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
        let denied = self.policy().check(name).err();
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
        if let Some(d) = out.details.as_ref() {
            let model = d["model"].as_str().map(String::from);
            match d["planDecision"].as_str() {
                Some(k @ ("execute" | "compact")) => {
                    self.exec_plan = Some(ExecPlan {
                        content: d["content"].as_str().unwrap_or_default().into(),
                        title: d["title"].as_str().unwrap_or_default().into(),
                        uri: d["uri"].as_str().unwrap_or_default().into(),
                        compact: k == "compact",
                        model,
                    });
                }
                Some("keep") => {
                    self.exit_plan();
                    if let Some(m) = model {
                        self.continue_with(&m);
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
    use crate::config::Configs;
    use crate::events::new_inbox;
    use crate::provider::ProviderError;

    fn assistant(blocks: Vec<Block>, stop: StopReason) -> Message {
        Message::Assistant {
            content: blocks,
            api: crate::models::API.into(),
            provider: "anthropic".into(),
            model: "m".into(),
            usage: Default::default(),
            stop_reason: stop,
            timestamp: 0,
            response_id: None,
            error_message: None,
        }
    }

    fn call(id: &str, name: &str) -> Block {
        Block::ToolCall { id: id.into(), name: name.into(), arguments: json!({}) }
    }

    fn agent(script: Vec<Result<Message, ProviderError>>) -> (Agent, tokio::sync::mpsc::UnboundedReceiver<AgentEvent>) {
        let (ntx, _nrx) = tokio::sync::mpsc::unbounded_channel();
        let plugins = PluginHost::new(vec![], vec![], ntx, Default::default());
        let providers =
            Rc::new(Providers::new(Rc::new(Configs::default()), crate::gateway::GatewaySource::new(plugins.clone())));
        providers.scripted.borrow_mut().extend(script);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let ctx = Rc::new(crate::context::Context {
            warnings: vec![],
            files: vec![],
            rules: vec![],
            skills: vec![],
            commands: vec![],
            agents: vec![],
        });
        let model = ModelSpec {
            id: "anthropic/m".into(),
            provider: "anthropic".into(),
            context_window: 200_000,
            max_output: 8192,
        };
        let a = Agent::new(AgentInit {
            providers,
            plugins,
            cwd: std::env::temp_dir(),
            ctx,
            model,
            thinking: Thinking::parse("off").unwrap(),
            events: tx,
            inbox: new_inbox(),
        });
        (a, rx)
    }

    fn run<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(tokio::task::LocalSet::new().run_until(f))
    }

    #[test]
    fn tool_panic_becomes_an_error_result() {
        run(async {
            let script = vec![
                Ok(assistant(vec![call("t1", "__panic")], StopReason::ToolUse)),
                Ok(assistant(vec![Block::text("done")], StopReason::Stop)),
            ];
            let (mut a, _rx) = agent(script);
            a.turn(Some("go".to_string().into())).await;
            assert!(crate::message::check_tool_pairs(&a.messages).is_ok());
            let err = a.messages.iter().find_map(|m| match m {
                Message::ToolResult { is_error: true, content, .. } => Some(format!("{content:?}")),
                _ => None,
            });
            assert!(err.unwrap().contains("tool panicked: boom"));
            assert_eq!(a.messages.len(), 4, "the turn continued after the panic");
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
            assert!(crate::message::check_tool_pairs(&a.messages).is_ok());
            a.turn(Some("again".to_string().into())).await;
            assert!(crate::message::check_tool_pairs(&a.messages).is_ok());
            assert!(a.messages.iter().any(|m| matches!(m, Message::ToolResult { is_error: true, .. })));
        });
    }

    #[test]
    fn plan_prompt_inlines_plan_and_path() {
        let p = ExecPlan {
            content: "do it\n".into(),
            title: "T".into(),
            uri: "local://t-plan.md".into(),
            compact: true,
            model: None,
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
                compact: false,
                model: None,
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
}
