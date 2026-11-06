use super::*;
use crate::review;

impl Agent {
    pub fn tool_defs(&self) -> Vec<ToolDef> {
        let mut v = self
            .memo
            .base_defs
            .get_or_init(|| Tools::definitions().into_iter().chain(self.ports.plugins.tool_defs()).collect())
            .clone();
        if self.plan.plan.is_some() {
            v.push(Tools::plan_def());
        }
        v.push(Tools::task_def(self.tools.jobs.is_some(), &self.ctx.agents));
        if self.tools.jobs.is_some() {
            v.push(Tools::job_cancel_def());
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
        let mut out = match blocked.or(denied) {
            Some(b) => ToolOutput::err(b),
            None => {
                let ctx = ToolCtx {
                    events: self.ports.events.clone(),
                    cancel: cancel.clone(),
                    call_id: id.into(),
                    plan: self.plan.plan.is_some(),
                };
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
