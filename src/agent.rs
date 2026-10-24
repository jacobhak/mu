use crate::compaction::Outcome;
use crate::context::{self, Context, PromptInput};
use crate::events::{AgentEvent, Command, EventTx, SharedInbox, StatusSnapshot, UserInput};
use crate::jobs::{self, JobsHandle};
use crate::mem;
use crate::gitinfo::{self, GitInfo};
use crate::pricing;
use crate::quota::{self, ProviderUsage};
use crate::message::{Block, Message, StopReason, now_ms};
use crate::models::{ModelSpec, Thinking};
use crate::plugin::{InitParams, PluginHost};
use crate::provider::{ProviderError, Providers, Request, StreamEvent, ToolDef};
use crate::session::{self, CompactionState, SessionFile};
use crate::tools::{ToolCtx, ToolOutput, Tools};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
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
    pub title: Option<String>,
    pub events: EventTx,
    pub inbox: SharedInbox,
    pub cwd: PathBuf,
    pub ctx: Rc<Context>,
    pub subagent: bool,
    pub plugin_ctx: BTreeMap<String, String>,
    pub base_defs: Vec<ToolDef>,
    /// Tool names hidden from the model (e.g. `ask` when headless).
    pub excluded: Vec<&'static str>,
    pub plan: Option<PlanState>,
    /// Only these tools are offered when set.
    pub allow: Option<Vec<&'static str>>,
    pub parent_session: Option<PathBuf>,
    exec_plan: Option<String>,
    pub pin_model: bool,
    pub pin_thinking: bool,
    pub compaction: Option<CompactionState>,
    /// Index into `messages` from which assistant usage reflects the current context.
    pub fresh_from: usize,
    pub since_compaction: usize,
    pub title_requested: bool,
    pub title_tx: UnboundedSender<(String, String)>,
    title_rx: Option<UnboundedReceiver<(String, String)>>,
    resumed: bool,
    user_turn: u32,
    session_cost: f64,
    tokens_in: u64,
    tokens_out: u64,
    git: Option<GitInfo>,
    quota: Option<ProviderUsage>,
    quota_at: Option<Instant>,
    turn_msgs: Vec<Value>,
    jobs: Option<Rc<JobsHandle>>,
    wake_rx: Option<UnboundedReceiver<()>>,
}

pub struct PlanState {
    prev_model: Option<String>,
    prev_thinking: Thinking,
}

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
        let base_defs = Tools::definitions().into_iter().chain(i.plugins.tool_defs()).collect();
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
            title: None,
            events: i.events,
            inbox: i.inbox,
            cwd: i.cwd,
            ctx: i.ctx,
            subagent: false,
            plugin_ctx: BTreeMap::new(),
            base_defs,
            excluded: Vec::new(),
            plan: None,
            allow: None,
            parent_session: None,
            exec_plan: None,
            pin_model: false,
            pin_thinking: false,
            compaction: None,
            fresh_from: 0,
            since_compaction: 0,
            title_requested: false,
            title_tx,
            title_rx: Some(title_rx),
            resumed: false,
            user_turn: 0,
            session_cost: 0.0,
            tokens_in: 0,
            tokens_out: 0,
            git: None,
            quota: None,
            quota_at: None,
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
        let mut v = self.base_defs.clone();
        if self.plan.is_some() {
            v.push(Tools::plan_def());
        }
        if crate::tools::task::depth() < 2 {
            v.push(Tools::task_def(self.jobs.is_some()));
        }
        if self.jobs.is_some() {
            v.push(Tools::wait_def());
        }
        v.retain(|d| !self.excluded.contains(&d.name.as_str()));
        if let Some(allow) = &self.allow {
            v.retain(|d| allow.contains(&d.name.as_str()));
        }
        v
    }

    async fn enter_plan(&mut self) {
        self.ensure_session().await;
        let prev = PlanState { prev_model: Some(self.model.id.clone()), prev_thinking: self.thinking };
        if self.providers.cfg.role("plan").is_some() {
            self.set_model("@plan");
        }
        if let Some(s) = self.session.as_mut() {
            s.mode_change("plan", json!({"planFilePath": "local://"}));
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
            plugin_ctx: &self.plugin_ctx,
        })
    }

    pub fn session_id(&self) -> String {
        self.session.as_ref().map(|s| s.id()).unwrap_or_default()
    }

    pub fn snapshot(&self) -> StatusSnapshot {
        StatusSnapshot {
            model_name: pricing::display_name(&self.model.id),
            reasoning: pricing::reasoning(&self.model.id).unwrap_or(true),
            thinking: self.thinking.as_str().into(),
            subscription: true,
            plan: self.plan.is_some(),
            context_tokens: Some(self.context_tokens()),
            context_window: self.model.context_window,
            cost: self.session_cost,
            tokens_in: self.tokens_in,
            tokens_out: self.tokens_out,
            title: self.title.clone(),
            session_path: self.session.as_ref().map(|s| s.path.to_string_lossy().into_owned()),
            git: self.git.clone(),
            usage: self.quota.clone().filter(|q| q.provider == self.model.provider),
        }
    }

    /// Git and quota state for the status line. Runs between turns, never from the draw path.
    /// Quota is fetched at most once a minute, and only from an already-running gateway.
    async fn refresh_external(&mut self) {
        const QUOTA_INTERVAL: Duration = Duration::from_secs(60);
        self.git = gitinfo::git_status(&self.cwd).await;
        let stale = self.quota_at.is_none_or(|t| t.elapsed() >= QUOTA_INTERVAL);
        if stale {
            self.quota_at = Some(Instant::now());
            self.quota = quota::fetch(&self.model.provider).await;
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
        let trailing: u64 = msgs[i + 1..].iter().map(crate::compaction::est).sum();
        Some(usage.input + usage.cache_read + usage.cache_write + usage.output + trailing)
    }

    async fn ensure_session(&mut self) {
        if self.session.is_some() {
            return;
        }
        match SessionFile::create(&self.cwd, self.parent_session.as_deref()) {
            Ok(mut s) => {
                s.model_change(&self.model.id);
                s.thinking_change(self.thinking.as_str());
                self.session = Some(s);
                self.tools.set_session(&self.session_id(), self.session.as_ref().map(|s| s.path.clone()));
                self.sync_plugin_session();
                let res = self.plugins.hook("session_start", json!({"resumed": self.resumed})).await;
                self.apply_plugin_context(res);
            }
            Err(e) => self.error(format!("session write failed: {e}")),
        }
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

    pub fn set_model(&mut self, sel: &str) {
        match self.providers.resolve(sel) {
            Ok((spec, level)) => {
                if let Some(s) = self.session.as_mut() {
                    s.model_change(&spec.id);
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
        if let Some(s) = self.session.as_mut() {
            s.thinking_change(t.as_str());
        }
        self.emit_status();
    }

    pub fn push(&mut self, mut m: Message) {
        if let Message::Assistant { usage, .. } = &mut m {
            if let Some(c) = pricing::cost(&self.model.id, usage) {
                usage.cost = c;
            }
            self.session_cost += usage.cost.total;
            self.tokens_in += usage.input + usage.cache_read + usage.cache_write;
            self.tokens_out += usage.output;
        }
        let plugin_view = if m.image_stats().0 > 0 { serde_json::to_value(m.without_images()) } else { serde_json::to_value(&m) };
        if let Ok(v) = plugin_view {
            self.turn_msgs.push(v);
        }
        let id = self.session.as_mut().map(|s| s.message(&m)).unwrap_or_default();
        self.entry_ids.push(id);
        self.messages.push(m);
        self.since_compaction += 1;
    }

    fn push_result(&mut self, id: &str, name: &str, out: &ToolOutput) {
        self.push(Message::ToolResult {
            tool_call_id: id.into(),
            tool_name: name.into(),
            content: vec![Block::text(out.text.clone())],
            is_error: out.is_error,
            timestamp: now_ms(),
            details: out.details.clone(),
        });
    }

    fn reset_conversation(&mut self) {
        self.session = None;
        self.messages.clear();
        self.entry_ids.clear();
        self.plugin_ctx.clear();
        self.title = None;
        self.user_turn = 0;
        self.session_cost = 0.0;
        self.tokens_in = 0;
        self.tokens_out = 0;
        self.turn_msgs.clear();
        self.compaction = None;
        self.fresh_from = 0;
        self.since_compaction = 0;
        self.title_requested = false;
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
        let text_of = |blocks: &[Block]| {
            blocks.iter().filter_map(|b| if let Block::Text { text } = b { Some(text.as_str()) } else { None }).collect::<Vec<_>>().join("\n")
        };
        for m in &self.messages {
            match m {
                Message::User { content, .. } => self.emit(AgentEvent::UserPrompt(text_of(content))),
                Message::Assistant { content, .. } => {
                    for b in content {
                        match b {
                            Block::Text { text } => {
                                self.emit(AgentEvent::Text(text.clone()));
                                self.emit(AgentEvent::BlockEnd);
                            }
                            Block::ToolCall { id, name, arguments } => {
                                self.emit(AgentEvent::ToolStart { id: id.clone(), name: name.clone(), args: arguments.clone() });
                            }
                            _ => {}
                        }
                    }
                }
                Message::ToolResult { tool_call_id, tool_name, content, is_error, details, .. } => {
                    let output = ToolOutput { text: text_of(content), is_error: *is_error, display: None, details: details.clone() };
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
        let mut s = match SessionFile::open_append(path) {
            Ok(s) => s,
            Err(e) => return self.error(e.to_string()),
        };
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
        } else if self.pin_thinking {
            s.thinking_change(self.thinking.as_str());
        }
        self.session_cost = loaded.messages.iter().map(|m| match m {
            Message::Assistant { usage, .. } => usage.cost.total,
            _ => 0.0,
        }).sum();
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
        if loaded.mode.as_ref().is_some_and(|(m, _)| m == "plan") {
            self.plan = Some(PlanState { prev_model: None, prev_thinking: self.thinking });
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
        tokio::task::spawn_local(pricing::refresh());
        self.refresh_external().await;
        self.emit_status();
        let mut title_rx = self.title_rx.take().expect("run once");
        let mut wake_rx = self.wake_rx.take();
        loop {
            let cmd = tokio::select! {
                c = rx.recv() => match c { Some(c) => c, None => break },
                Some((sid, t)) = title_rx.recv() => {
                    self.apply_title(&sid, t);
                    continue;
                }
                Some(()) = recv_opt(&mut wake_rx) => {
                    self.turn(None).await;
                    continue;
                }
            };
            match cmd {
                Command::Prompt(t) => self.turn(Some(t)).await,
                Command::SetModel(sel) => self.set_model(&sel),
                Command::SetThinking(t) => self.set_thinking(t),
                Command::NewSession => {
                    self.end_session().await;
                    self.reset_conversation();
                    self.notice("new session");
                    self.emit_status();
                }
                Command::Resume(p) => {
                    self.end_session().await;
                    self.load_session(&p).await;
                }
                Command::TogglePlan(prompt) => match (self.plan.is_some(), prompt) {
                    (false, p) => {
                        self.enter_plan().await;
                        if let Some(p) = p {
                            self.turn(Some(p.into())).await;
                        }
                    }
                    (true, Some(p)) => self.turn(Some(p.into())).await,
                    (true, None) => self.exit_plan(),
                },
                Command::Compact(focus) => self.manual_compact(focus).await,
                Command::Rename(t) => {
                    self.ensure_session().await;
                    if let Some(s) = self.session.as_mut() {
                        s.set_title(&t, "user");
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

    fn apply_title(&mut self, sid: &str, t: String) {
        if self.session_id() != sid || self.title.is_some() {
            return;
        }
        if let Some(s) = self.session.as_mut() {
            s.set_title(&t, "auto");
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
            Outcome::Failed(e) => self.error(format!("compaction failed: {e}")),
        }
        self.emit(AgentEvent::TurnEnd { aborted: cancel.is_cancelled() });
    }

    async fn user_bash(&mut self, cmd: String, include: bool) {
        self.ensure_session().await;
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
        }
        self.emit(AgentEvent::ToolEnd { id, name: "bash".into(), output: out });
        self.emit(AgentEvent::TurnEnd { aborted: cancel.is_cancelled() });
    }

    /// One turn. `first` is `None` for a wake-up that only delivers finished background jobs.
    async fn turn(&mut self, first: Option<UserInput>) {
        if first.is_none() && !self.jobs.as_ref().is_some_and(|j| j.has_undelivered()) {
            return;
        }
        self.ensure_session().await;
        let cancel = CancellationToken::new();
        self.inbox.borrow_mut().cancel = cancel.clone();
        self.emit(AgentEvent::TurnStart);
        let mut next = Some(first);
        let mut aborted = false;
        while let Some(input) = next.take() {
            self.ensure_session().await;
            let extra = match &input {
                Some(i) => {
                    self.emit(AgentEvent::UserPrompt(i.text.clone()));
                    self.user_turn += 1;
                    let res = self
                        .plugins
                        .hook("prompt_submit", json!({"prompt": i.text, "userTurn": self.user_turn, "resumed": self.resumed}))
                        .await;
                    self.apply_plugin_context(res);
                    i.text.len() as u64 / 4 + 1600 * i.images.len() as u64
                }
                None => 0,
            };
            if self.needs_compaction(extra) {
                if let Outcome::Failed(e) = self.compact(None, &cancel).await {
                    self.error(format!("compaction failed: {e}"));
                }
            }
            match &input {
                Some(i) => self.push(Message::user_input(i)),
                None => {
                    self.take_jobs();
                }
            }
            match self.run_loop(&cancel).await {
                Flow::Aborted => {
                    aborted = true;
                    break;
                }
                Flow::Failed => break,
                Flow::Done => {}
            }
            self.turn_end_hook().await;
            if let Some(content) = self.exec_plan.take() {
                self.exit_plan();
                self.end_session().await;
                self.reset_conversation();
                self.notice("── new session");
                next = Some(Some(format!("Execute this plan:\n\n{content}").into()));
                continue;
            }
            next = self.inbox.borrow_mut().follow_up.pop_front().map(Some);
        }
        if aborted {
            self.turn_end_hook().await;
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
        let mut compacted_for_overflow = false;
        loop {
            if !first && self.needs_compaction(0) {
                if let Outcome::Failed(e) = self.compact(None, cancel).await {
                    self.error(format!("compaction failed: {e}"));
                }
            }
            first = false;
            let system = self.system();
            let wire = self.wire_messages();
            let defs = self.tool_defs();
            let reply = {
                let req = Request {
                    model: &self.model,
                    thinking: self.thinking,
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
                        StreamEvent::ToolCallStart => return,
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
            if stop == StopReason::Length {
                self.error("[output truncated: max_tokens reached]");
            }
            if stop != StopReason::ToolUse || calls.is_empty() {
                let steered = self.take_steer();
                let delivered = self.take_jobs();
                if steered || delivered {
                    continue;
                }
                return Flow::Done;
            }
            let mut skip: Option<&str> = None;
            for (id, name, args) in calls {
                if let Some(reason) = skip {
                    let out = ToolOutput::err(reason);
                    self.emit(AgentEvent::ToolStart { id: id.clone(), name: name.clone(), args: args.clone() });
                    self.emit(AgentEvent::ToolEnd { id: id.clone(), name: name.clone(), output: ToolOutput::err(reason) });
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
        let mut out = match blocked {
            Some(b) => ToolOutput::err(b),
            None => {
                let ctx = ToolCtx {
                    events: self.events.clone(),
                    cancel: cancel.clone(),
                    call_id: id.into(),
                };
                self.tools.call(name, &args, &ctx).await
            }
        };
        let sub = format!("tool_result:{name}");
        for (_, v) in self.plugins.hook(&sub, json!({"name": name, "arguments": args, "content": out.text, "isError": out.is_error})).await {
            if let Some(a) = v["append"].as_str().filter(|a| !a.is_empty()) {
                out.text.push_str("\n\n");
                out.text.push_str(a);
            }
        }
        match out.details.as_ref().and_then(|d| d["planDecision"].as_str()) {
            Some("execute") => self.exec_plan = out.details.as_ref().and_then(|d| d["content"].as_str()).map(String::from),
            Some("keep") => self.exit_plan(),
            _ => {}
        }
        if name == "todo" && !out.is_error {
            self.emit(AgentEvent::Todo(self.tools.todo_json()));
        }
        self.emit(AgentEvent::ToolEnd {
            id: id.into(),
            name: name.into(),
            output: ToolOutput { text: out.text.clone(), is_error: out.is_error, display: out.display.clone(), details: out.details.clone() },
        });
        out
    }
}
