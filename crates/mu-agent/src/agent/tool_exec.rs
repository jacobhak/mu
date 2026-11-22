use super::*;
use crate::review;
use mu_base::events::{JobKind, SubagentState, SubagentStatus};
use mu_base::permissions::{ApprovalDecision, ApprovalRequest, ApprovalScope, PermissionMode};
use mu_tools::permissions::{GrantOutcome, Verdict};
use std::time::SystemTime;

/// The model's reason when nobody can approve a call.
fn approval_unavailable(name: &str, reason: &str, mode: PermissionMode) -> String {
    let how = reason
        .strip_prefix("rule \"")
        .and_then(|rest| rest.split_once('"'))
        .map(|(pattern, _)| format!("by rule \"{pattern}\""))
        .unwrap_or_else(|| format!("in {} mode", mode.as_str()));
    format!(
        "Not run: {name} needs approval {how} and nobody can approve it in this run. The user can allow it with a [permissions.rules] entry in ~/.mu/config.toml or run with --permissions yolo."
    )
}

fn elapsed_text(secs: u64) -> String {
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m {}s", secs / 60, secs % 60),
        _ => format!("{}h {}m", secs / 3600, secs % 3600 / 60),
    }
}

impl Agent {
    /// The custom roles the model may pick for a `task`'s `model`; only the top-level session has them.
    pub(super) fn delegated_roles(&self) -> Vec<(String, String)> {
        if self.run.subagent { Vec::new() } else { mu_base::roles::delegated(&self.ports.providers.cfg) }
    }

    pub fn tool_defs(&self) -> Vec<ToolDef> {
        let mut v = self
            .memo
            .base_defs
            .get_or_init(|| {
                let plugins = &self.ports.plugins;
                Tools::definitions().into_iter().chain(plugins.tool_defs()).chain(plugins.relay_tool_defs()).collect()
            })
            .clone();
        let jobs = self.tools.jobs.is_some();
        if jobs && let Some(d) = v.iter_mut().find(|d| d.name == "bash") {
            *d = Tools::bash_def(true);
        }
        self.sync_mode_tables();
        match self.active_mode() {
            Some(m) => v.push(Tools::exit_def(m)),
            None => {
                let enterable: Vec<(String, String)> = self
                    .ctx
                    .modes
                    .iter()
                    .filter(|m| m.model_entry != context::ModelEntry::Never)
                    .map(|m| (m.name.clone(), m.description.clone()))
                    .collect();
                if !enterable.is_empty() {
                    v.push(Tools::enter_mode_def(&enterable));
                }
            }
        }
        let roles: Vec<String> = self.delegated_roles().into_iter().map(|(n, _)| format!("@{n}")).collect();
        *self.tools.delegated.borrow_mut() = roles.clone();
        *self.tools.agent_models.borrow_mut() =
            self.ctx.agents.iter().map(|a| (a.name.clone(), a.model.clone())).collect();
        let role_names: Vec<&str> = roles.iter().map(String::as_str).collect();
        v.push(Tools::task_def(jobs, &self.ctx.agents, &role_names));
        if jobs {
            v.push(Tools::job_cancel_def());
            v.push(Tools::wait_def());
        }
        if let Some(o) = self.tools.output_schema.borrow().as_ref() {
            v.push(Tools::yield_def(o));
        }
        let policy = self.policy();
        v.retain(|d| policy.permits(&d.name));
        v
    }

    /// Tells the tools which modes exist: how the model may enter each and which tools exit them.
    pub(super) fn sync_mode_tables(&self) {
        *self.tools.mode_entry.borrow_mut() = self
            .ctx
            .modes
            .iter()
            .map(|m| (m.name.clone(), m.model_entry, m.description.clone(), m.read_only()))
            .collect();
        *self.tools.mode_tools.borrow_mut() =
            self.ctx.modes.iter().map(|m| (m.exit.tool.clone(), m.name.clone())).collect();
    }

    /// The tool limits for this run; enforced in `run_tool` and used to build the advertised list.
    pub fn policy(&self) -> ToolPolicy {
        ToolPolicy {
            allow: self.run.allow.clone(),
            excluded: self.run.excluded.clone(),
            depth: mu_tools::tools::task::depth(),
            mode: self.active_mode().map(|m| crate::policy::ModePolicy {
                name: m.name.clone(),
                title: m.title(),
                exit_tool: m.exit.tool.clone(),
                allow: m.allow.clone(),
                deny: m.deny.clone(),
            }),
            mode_tools: self.ctx.modes.iter().map(|m| (m.exit.tool.clone(), m.name.clone())).collect(),
        }
    }

    /// Drops the cached tool list so the next request rebuilds it (after an MCP login added tools).
    pub(crate) fn refresh_tool_defs(&mut self) {
        self.memo.base_defs.take();
    }

    pub(super) fn push_result(&mut self, id: &str, name: &str, out: &ToolOutput) {
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

    /// Marks a call that will not run: it still gets a visible start, an end and a result.
    pub(super) fn emit_skipped(&mut self, id: &str, name: &str, args: &Value, reason: &str) {
        self.emit(AgentEvent::ToolSkipped { id: id.into() });
        self.emit(AgentEvent::ToolStart { id: id.into(), name: name.into(), args: args.clone() });
        self.emit(AgentEvent::ToolEnd { id: id.into(), name: name.into(), output: ToolOutput::err(reason) });
        self.push_result(id, name, &ToolOutput::err(reason));
    }

    /// Registers an interruptible wait for this call, when the session has background jobs and the
    /// call can wait: `wait`, or `bash` that is neither `pty` nor `async`. User input releases the
    /// wait without cancelling the work; input queued before this call steers at once instead.
    pub(super) fn wait_guard(&self, name: &str, args: &Value) -> Option<mu_tools::jobs::ActiveWait> {
        let eligible = match name {
            "wait" => true,
            "bash" => args["pty"].as_bool() != Some(true) && args["async"].as_bool() != Some(true),
            _ => false,
        };
        let jobs = self.tools.jobs.clone().filter(|_| eligible)?;
        let guard = jobs.begin_wait();
        if promote_queued(&self.ports.inbox, &self.ports.events) {
            guard.token().cancel();
        }
        Some(guard)
    }

    /// A status block for the model: jobs started before `since` that still run. `None` when there are none.
    pub(super) fn job_status(&self, since: SystemTime) -> Option<String> {
        let running: Vec<_> = self
            .tools
            .jobs
            .as_ref()?
            .snapshot()
            .into_iter()
            .filter(|j| j.state == SubagentState::Running && j.started < since)
            .collect();
        let line = |j: &SubagentStatus| {
            let label = match j.kind {
                JobKind::Bash => format!("bash: {}", j.task),
                JobKind::Subagent => format!("{}: {}", j.agent, j.task),
            };
            let secs = since.duration_since(j.started).map_or(0, |d| d.as_secs());
            (mu_base::text::truncate_text(&label, 80), elapsed_text(secs))
        };
        Some(match running.as_slice() {
            [] => return None,
            [one] => {
                let (label, elapsed) = line(one);
                let next = "When it finishes, mu automatically starts your next turn with its result; use `wait` when \
                            you need it now.";
                format!(
                    "\n\n<background-jobs>\n{} ({label}) has been running for {elapsed}.\n{next}\n</background-jobs>",
                    one.name
                )
            }
            many => {
                let items: String = many
                    .iter()
                    .map(|j| {
                        let (label, elapsed) = line(j);
                        format!("- {} ({label}), {elapsed}\n", j.name)
                    })
                    .collect();
                let next = "When each finishes, mu automatically starts your next turn with its result; use `wait` \
                            when you need one now.";
                format!("\n\n<background-jobs>\nStill running:\n{items}{next}\n</background-jobs>")
            }
        })
    }

    /// Replaces secrets in the text of a user message or tool result.
    pub(super) fn redact_message(&self, m: &mut Message) -> Vec<secrets::Hit> {
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
    pub(super) fn log_redaction(&mut self, at: &str, hits: &[secrets::Hit]) {
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

    pub(super) async fn user_bash(&mut self, cmd: String, include: bool) {
        if !self.ensure_session().await {
            return;
        }
        let cancel = CancellationToken::new();
        self.ports.inbox.borrow_mut().cancel = cancel.clone();
        self.emit_turn_start();
        let args = json!({"command": cmd});
        let id = "user-bash".to_string();
        self.emit(AgentEvent::ToolStart { id: id.clone(), name: "bash".into(), args: args.clone() });
        let ctx = ToolCtx {
            events: self.ports.events.clone(),
            cancel: cancel.clone(),
            call_id: id.clone(),
            mode: self.active_mode().cloned(),
            wait_interrupt: None,
        };
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

    /// Puts one tool call to the user. A cancelled turn, or a front end that drops the dialog,
    /// denies it.
    async fn approve(&self, req: ApprovalRequest, cancel: &CancellationToken) -> ApprovalDecision {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.emit(AgentEvent::Approve { request: req, reply: tx });
        tokio::select! {
            biased;
            _ = cancel.cancelled() => ApprovalDecision::Deny { note: None },
            r = rx => r.unwrap_or(ApprovalDecision::Deny { note: None }),
        }
    }

    /// Runs one call alone: hooks, gate, approval, the tool, the hooks after it.
    pub(super) async fn run_tool(
        &mut self,
        id: &str,
        name: &str,
        args: Value,
        cancel: &CancellationToken,
    ) -> ToolOutput {
        let (args, stop) = self.open_tool(id, name, args).await;
        self.run_opened(id, name, args, stop, None, cancel).await
    }

    /// The `tool_call` hooks and the start event, then the run's own limits. `Some` is the output
    /// of a call that will not run. Returns the arguments the hooks left.
    pub(super) async fn open_tool(&mut self, id: &str, name: &str, mut args: Value) -> (Value, Option<ToolOutput>) {
        let mut blocked = None;
        match self.ports.plugins.tool_call_hook(name, args.clone()).await {
            Ok(a) => args = a,
            Err(b) => blocked = Some(b),
        }
        self.emit(AgentEvent::ToolStart { id: id.into(), name: name.into(), args: args.clone() });
        let denied = self.policy().check(name).err().or_else(|| secrets::guard(name, &args).err());
        (args, blocked.or(denied).map(ToolOutput::err))
    }

    /// What the permission gate says about a call that is to run.
    pub(super) async fn verdict_for(&mut self, name: &str, args: &Value) -> Verdict {
        let mut warns = Vec::new();
        self.tools.permissions.ensure_project(&self.cwd, &mut warns).await;
        for w in warns {
            self.notice(w);
        }
        self.tools.permissions.check(&self.tools, name, args)
    }

    /// Acts on a verdict: asks the user for an `Ask`. Returns the output of a call that must not run,
    /// and the user's note on an approved one.
    async fn settle_verdict(
        &mut self,
        id: &str,
        name: &str,
        verdict: Verdict,
        cancel: &CancellationToken,
    ) -> (Option<ToolOutput>, Option<String>) {
        let mut user_note = None;
        let mut stop = None;
        match verdict {
            Verdict::Allow => {}
            Verdict::Deny(msg) => stop = Some(ToolOutput::err(msg)),
            Verdict::Ask(mut req) => {
                req.call_id = id.into();
                let grant = req.grant.clone();
                let reason = req.reason.clone();
                match self.approve(req, cancel).await {
                    ApprovalDecision::Allow { scope, note, patterns } => {
                        if scope != ApprovalScope::Once
                            && let Some(g) = grant
                        {
                            match self.tools.permissions.remember(&g.picked(patterns), scope, true) {
                                Ok(GrantOutcome::AlreadyCovered(why)) => {
                                    self.notice(format!("approval not saved: already covered ({why})"));
                                }
                                Ok(GrantOutcome::Written) => {}
                                Err(e) => self.notice(format!("could not save the approval: {e}")),
                            }
                        }
                        user_note = note;
                    }
                    ApprovalDecision::Deny { note: Some(n) } => {
                        stop = Some(ToolOutput::err(format!("Not run: the user denied this {name} call: {n}")));
                    }
                    ApprovalDecision::Deny { note: None } => {
                        cancel.cancel();
                        stop = Some(ToolOutput::err("Not run: the user denied this call."));
                    }
                    ApprovalDecision::Unavailable => {
                        stop =
                            Some(ToolOutput::err(approval_unavailable(name, &reason, self.tools.permissions.mode())));
                    }
                }
            }
        }
        (stop, user_note)
    }

    /// The tool itself; a panic becomes an error result. `wait` releases an interruptible wait.
    pub(super) async fn exec_tool(
        &self,
        id: &str,
        name: &str,
        args: &Value,
        cancel: &CancellationToken,
        wait: Option<CancellationToken>,
    ) -> ToolOutput {
        let ctx = ToolCtx {
            events: self.ports.events.clone(),
            cancel: cancel.clone(),
            call_id: id.into(),
            mode: self.active_mode().cloned(),
            wait_interrupt: wait,
        };
        use futures_util::FutureExt;
        match std::panic::AssertUnwindSafe(self.tools.call(name, args, &ctx)).catch_unwind().await {
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

    /// The rest of `run_tool` for a call `open_tool` has started. `stop`: it will not run. `verdict`:
    /// the gate's answer when the caller already asked it.
    pub(super) async fn run_opened(
        &mut self,
        id: &str,
        name: &str,
        args: Value,
        stop: Option<ToolOutput>,
        verdict: Option<Verdict>,
        cancel: &CancellationToken,
    ) -> ToolOutput {
        let mut user_note = None;
        let out = match stop {
            Some(o) => o,
            None => {
                let verdict = match verdict {
                    Some(v) => v,
                    None => self.verdict_for(name, &args).await,
                };
                let (stop, note) = self.settle_verdict(id, name, verdict, cancel).await;
                user_note = note;
                match stop {
                    Some(o) => o,
                    None => {
                        // Only the call itself is interruptible: the approval above is not.
                        let guard = self.wait_guard(name, &args);
                        self.exec_tool(id, name, &args, cancel, guard.as_ref().map(|g| g.token())).await
                    }
                }
            }
        };
        self.end_tool(id, name, args, user_note, out).await
    }

    /// Redaction, the `tool_result` hooks, plan and todo effects and the end event for a finished call.
    pub(super) async fn end_tool(
        &mut self,
        id: &str,
        name: &str,
        args: Value,
        user_note: Option<String>,
        mut out: ToolOutput,
    ) -> ToolOutput {
        if let Some(n) = user_note {
            out.text.push_str(&format!("\n\nUser note: {n}"));
        }
        let hits = self.redact_output(&mut out, true);
        self.log_redaction(&format!("tool:{name}"), &hits);
        if name == "web_search" && !out.is_error {
            self.tools.permissions.note_search_result(&out.text);
        }
        let sub = format!("tool_result:{name}");
        for (_, v) in self
            .ports
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
            if let Some(name) = d["enterMode"].as_str()
                && !out.is_error
                && self.plan.mode.is_none()
            {
                self.enter_mode(name, true).await;
            }
            self.apply_mode_action(d, &mut out).await;
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
                changes: std::mem::take(&mut out.changes),
            },
        });
        out
    }

    /// Acts on an exit tool's result: the document is started, kept as the review base, parked, or the mode ends.
    async fn apply_mode_action(&mut self, d: &Value, out: &mut ToolOutput) {
        let decision = mode_action(d);
        let mode = d["mode"].as_str().unwrap_or("plan").to_string();
        let noun = context::capitalize(self.ctx.mode(&mode).map_or("plan", |m| m.noun()));
        let (base, now) = match decision {
            Some(ModeAction::Execute { .. } | ModeAction::Compact { .. } | ModeAction::Keep { .. }) => {
                (mu_base::git::snapshot_tree(&self.cwd).await.ok(), now_ms())
            }
            _ => (None, 0),
        };
        match decision {
            Some(k @ (ModeAction::Execute { .. } | ModeAction::Compact { .. })) => {
                let (_, saved) = self.save_plan(d).await;
                let stored = match saved {
                    Ok(m) => Some(m.id),
                    Err(e) => {
                        self.error(format!("could not save the {}: {e}", noun.to_lowercase()));
                        None
                    }
                };
                self.plan.exec_plan = Some(ExecPlan {
                    content: d["content"].as_str().unwrap_or_default().into(),
                    title: d["title"].as_str().unwrap_or_default().into(),
                    uri: d["uri"].as_str().unwrap_or_default().into(),
                    decision: k,
                    base,
                    approved_at: now,
                    stored,
                    mode,
                });
            }
            Some(k @ ModeAction::Keep { .. }) => {
                let (store, saved) = self.save_plan(d).await;
                match saved {
                    Ok(m) => {
                        let session = self.session.as_ref().map(|s| s.path.clone());
                        if let Err(e) = store.approve(&m.id, k.wire_name(), session.as_deref()) {
                            self.error(format!("could not record the approval: {e}"));
                        }
                    }
                    Err(e) => self.error(format!("could not save the {}: {e}", noun.to_lowercase())),
                }
                self.set_review_base(review::ReviewBase {
                    tree: base,
                    plan_uri: d["uri"].as_str().map(String::from),
                    since: now,
                });
                self.exit_mode();
                if let Some(m) = k.model() {
                    self.continue_with(m);
                }
            }
            Some(ModeAction::Exit) => self.exit_mode(),
            Some(ModeAction::Park) => {
                let (store, saved) = self.save_plan(d).await;
                match saved {
                    Ok(m) => {
                        let by_model = self.plan.mode.as_ref().is_some_and(|p| p.by_model);
                        self.exit_mode_keeping_model();
                        self.plan.stored_plan = None;
                        self.emit(AgentEvent::PlanParked { id: m.id.clone(), path: store.md_path(&m.id) });
                        if by_model {
                            out.text = format!(
                                "{noun} parked as `{0}`; approve it with /plans or `mu plan approve {0}`. Continue with what the user asked, or end your turn.",
                                m.id
                            );
                        } else {
                            out.text.push_str(&format!(
                                " Parked as `{0}`; approve it with /plans or `mu plan approve {0}`.",
                                m.id
                            ));
                        }
                    }
                    Err(e) => *out = ToolOutput::err(format!("Could not park the {}: {e}", noun.to_lowercase())),
                }
            }
            _ => {}
        }
    }
}

/// Reads the action an exit tool's result records in its details: the tool boundary.
fn mode_action(d: &Value) -> Option<ModeAction> {
    let model = d["model"].as_str().map(String::from);
    Some(match d["modeAction"].as_str()? {
        "execute" => ModeAction::Execute { model },
        "compact" => ModeAction::Compact { model },
        "keep" => ModeAction::Keep { model },
        "park" => ModeAction::Park,
        "exit" => ModeAction::Exit,
        _ => return None,
    })
}
#[cfg(test)]
mod elapsed_tests {
    use super::elapsed_text;

    #[test]
    fn formats_elapsed_time() {
        assert_eq!(elapsed_text(0), "0s");
        assert_eq!(elapsed_text(59), "59s");
        assert_eq!(elapsed_text(65), "1m 5s");
        assert_eq!(elapsed_text(3725), "1h 2m");
    }
}
