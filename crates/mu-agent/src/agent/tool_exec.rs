use super::*;
use crate::review;
use mu_base::events::{JobKind, SubagentState, SubagentStatus};
use mu_base::permissions::{ApprovalDecision, ApprovalRequest, ApprovalScope, PermissionMode};
use mu_tools::permissions::Verdict;
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
            .get_or_init(|| Tools::definitions().into_iter().chain(self.ports.plugins.tool_defs()).collect())
            .clone();
        if self.tools.interruptible_wait_enabled()
            && let Some(d) = v.iter_mut().find(|d| d.name == "bash")
        {
            *d = Tools::bash_def(true);
        }
        if self.plan.plan.is_some() {
            v.push(Tools::plan_def());
        }
        let roles: Vec<String> = self.delegated_roles().into_iter().map(|(n, _)| format!("@{n}")).collect();
        *self.tools.delegated.borrow_mut() = roles.clone();
        let role_names: Vec<&str> = roles.iter().map(String::as_str).collect();
        v.push(Tools::task_def(
            self.tools.jobs.is_some(),
            self.tools.interruptible_wait_enabled(),
            &self.ctx.agents,
            &role_names,
        ));
        if self.tools.jobs.is_some() {
            v.push(Tools::job_cancel_def());
        }
        if self.tools.interruptible_wait_enabled() {
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
            allow: self.run.allow.clone(),
            excluded: self.run.excluded.clone(),
            depth: mu_tools::tools::task::depth(),
            plan: self.plan.plan.is_some(),
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

    /// Why a `bash` call is not run: it waits (see `wait::waits`) while a background job runs. The
    /// model waits for a job by ending its turn; the job's result starts the next one.
    pub(super) fn wait_refusal(&self, name: &str, args: &Value) -> Option<String> {
        // With the experiment on, an ordinary command may wait: `wait` is the way to do it.
        if name != "bash" || self.tools.interruptible_wait_enabled() {
            return None;
        }
        let running = self.tools.jobs.as_ref()?.running_ids();
        let command = args["command"].as_str()?;
        if running.is_empty()
            || !mu_tools::tools::wait::waits(command, self.ports.providers.cfg.mu.bash.wait_sleep_secs)
        {
            return None;
        }
        let jobs = match running.as_slice() {
            [one] => format!("background job {one} is"),
            many => format!("background jobs {} are", many.join(", ")),
        };
        Some(format!(
            "Not run: this command waits, and {jobs} still running. Your turn has ended; when a job finishes, mu \
             automatically starts your next turn with its result. To wait for a job, end your turn with a short \
             status and no tool call."
        ))
    }

    /// Registers an interruptible wait for this call, when the experiment is on and the call can
    /// wait: `wait`, or `bash` that is neither `pty` nor `async`. User input releases the wait
    /// without cancelling the work; input queued before this call steers at once instead.
    fn wait_guard(&self, name: &str, args: &Value) -> Option<mu_tools::jobs::ActiveWait> {
        if !self.tools.interruptible_wait_enabled() {
            return None;
        }
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
                let next = if self.tools.interruptible_wait_enabled() {
                    "When it finishes, mu automatically starts your next turn with its result; use `wait` when you \
                     need it now."
                } else {
                    "When it finishes, mu automatically starts your next turn with its result; no user input is \
                     needed. If your next step needs it, end your turn now with a short status."
                };
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
                let next = if self.tools.interruptible_wait_enabled() {
                    "When each finishes, mu automatically starts your next turn with its result; use `wait` when you \
                     need one now."
                } else {
                    "When each finishes, mu automatically starts your next turn with its result; no user input is \
                     needed. If your next step needs one of them, end your turn now with a short status."
                };
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
        self.emit(AgentEvent::TurnStart);
        let args = json!({"command": cmd});
        let id = "user-bash".to_string();
        self.emit(AgentEvent::ToolStart { id: id.clone(), name: "bash".into(), args: args.clone() });
        let ctx = ToolCtx {
            events: self.ports.events.clone(),
            cancel: cancel.clone(),
            call_id: id.clone(),
            plan: self.plan.plan.is_some(),
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

    pub(super) async fn run_tool(
        &mut self,
        id: &str,
        name: &str,
        mut args: Value,
        cancel: &CancellationToken,
    ) -> ToolOutput {
        let mut blocked = None;
        for (plugin, v) in self.ports.plugins.hook("tool_call", json!({"name": name, "arguments": args})).await {
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
        let mut user_note = None;
        let mut out = match blocked.or(denied) {
            Some(b) => ToolOutput::err(b),
            None => {
                let mut warns = Vec::new();
                self.tools.permissions.ensure_project(&self.cwd, &mut warns).await;
                for w in warns {
                    self.notice(w);
                }
                let mut stop = None;
                match self.tools.permissions.check(&self.tools, name, &args) {
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
                                    && let Err(e) = self.tools.permissions.remember(&g.picked(patterns), scope, true)
                                {
                                    self.notice(format!("could not save the approval: {e}"));
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
                                stop = Some(ToolOutput::err(approval_unavailable(
                                    name,
                                    &reason,
                                    self.tools.permissions.mode(),
                                )));
                            }
                        }
                    }
                }
                match stop {
                    Some(o) => o,
                    None => {
                        // Only the call itself is interruptible: the approval above is not.
                        let guard = self.wait_guard(name, &args);
                        let ctx = ToolCtx {
                            events: self.ports.events.clone(),
                            cancel: cancel.clone(),
                            call_id: id.into(),
                            plan: self.plan.plan.is_some(),
                            wait_interrupt: guard.as_ref().map(|g| g.token()),
                        };
                        use futures_util::FutureExt;
                        let out =
                            match std::panic::AssertUnwindSafe(self.tools.call(name, &args, &ctx)).catch_unwind().await
                            {
                                Ok(o) => o,
                                Err(p) => {
                                    let msg = p
                                        .downcast_ref::<&str>()
                                        .map(|s| s.to_string())
                                        .or_else(|| p.downcast_ref::<String>().cloned())
                                        .unwrap_or_else(|| "unknown panic".into());
                                    ToolOutput::err(format!("tool panicked: {msg}"))
                                }
                            };
                        drop(guard);
                        out
                    }
                }
            }
        };
        if let Some(n) = user_note {
            out.text.push_str(&format!("\n\nUser note: {n}"));
        }
        let hits = self.redact_output(&mut out, true);
        self.log_redaction(&format!("tool:{name}"), &hits);
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
            self.apply_plan_decision(d, &mut out).await;
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

    /// Acts on a `propose_plan` result: the plan is started, kept as the review base, or parked.
    async fn apply_plan_decision(&mut self, d: &Value, out: &mut ToolOutput) {
        let decision = plan_decision(d);
        let (base, now) = match decision {
            Some(PlanDecision::Execute { .. } | PlanDecision::Compact { .. } | PlanDecision::Keep { .. }) => {
                (crate::review::facts::snapshot_tree(&self.cwd).await.ok(), now_ms())
            }
            _ => (None, 0),
        };
        match decision {
            Some(k @ (PlanDecision::Execute { .. } | PlanDecision::Compact { .. })) => {
                let (_, saved) = self.save_plan(d).await;
                let stored = match saved {
                    Ok(m) => Some(m.id),
                    Err(e) => {
                        self.error(format!("could not save the plan: {e}"));
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
                });
            }
            Some(k @ PlanDecision::Keep { .. }) => {
                let (store, saved) = self.save_plan(d).await;
                match saved {
                    Ok(m) => {
                        let session = self.session.as_ref().map(|s| s.path.clone());
                        if let Err(e) = store.approve(&m.id, k.wire_name(), session.as_deref()) {
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
                if let Some(m) = k.model() {
                    self.continue_with(m);
                }
            }
            Some(PlanDecision::Park) => {
                let (store, saved) = self.save_plan(d).await;
                match saved {
                    Ok(m) => {
                        self.exit_plan();
                        self.plan.stored_plan = None;
                        self.emit(AgentEvent::PlanParked { id: m.id.clone(), path: store.md_path(&m.id) });
                        out.text.push_str(&format!(
                            " Parked as `{0}`; approve it with /plans or `mu plan approve {0}`.",
                            m.id
                        ));
                    }
                    Err(e) => *out = ToolOutput::err(format!("Could not park the plan: {e}")),
                }
            }
            _ => {}
        }
    }
}

/// Reads the decision a `propose_plan` result records in its details: the tool boundary.
fn plan_decision(d: &Value) -> Option<PlanDecision> {
    let model = d["model"].as_str().map(String::from);
    Some(match d["planDecision"].as_str()? {
        "execute" => PlanDecision::Execute { model },
        "compact" => PlanDecision::Compact { model },
        "keep" => PlanDecision::Keep { model },
        "park" => PlanDecision::Park,
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
