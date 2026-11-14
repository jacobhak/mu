use super::*;
use crate::review;

impl Agent {
    pub(super) async fn enter_plan(&mut self) {
        self.enter_plan_as(false).await;
    }

    /// `by_model`: the model asked for plan mode through `enter_plan`, rather than the user.
    pub(super) async fn enter_plan_as(&mut self, by_model: bool) {
        self.ensure_session().await;
        let prev =
            PlanState { prev_model: Some(self.model.model.id.clone()), prev_thinking: self.model.thinking, by_model };
        if self.ports.providers.cfg.role("plan").is_some() {
            self.set_model("@plan");
        }
        if let Some(s) = self.session.as_mut() {
            s.mode_change(
                "plan",
                json!({"planFilePath": "local://", "prevModel": prev.prev_model, "prevThinking": prev.prev_thinking.as_str(), "byModel": by_model}),
            );
        }
        self.plan.plan = Some(prev);
        self.notice("plan mode on");
        self.emit_status();
    }

    pub(super) fn exit_plan(&mut self) {
        self.leave_plan_mode(true);
    }

    /// Leaving to park a plan: the session keeps the model and thinking level it planned with,
    /// rather than the ones plan mode replaced. `/plan` off and an approved plan still restore.
    pub(super) fn exit_plan_keeping_model(&mut self) {
        self.leave_plan_mode(false);
    }

    fn leave_plan_mode(&mut self, restore: bool) {
        let Some(p) = self.plan.plan.take() else { return };
        if restore {
            if let Some(m) = p.prev_model {
                self.set_model(&m);
            }
            self.set_thinking(p.prev_thinking);
        }
        if let Some(s) = self.session.as_mut() {
            s.mode_change("none", json!({}));
        }
        self.notice("plan mode off");
        self.emit_status();
    }

    /// Gives a fresh session the approved plan as a `local://` file, so the path in the execution
    /// prompt resolves, and names the session after the plan.
    fn seed_plan(&mut self, p: &ExecPlan) {
        if let Err(e) = self.write_local_file(&p.uri, &p.content) {
            self.error(format!("cannot copy the plan to {}: {e}", p.uri));
        }
        if self.title.text.is_none() && !p.title.trim().is_empty() {
            if let Some(s) = self.session.as_mut() {
                s.set_title(&p.title, TitleSource::Auto);
            }
            self.title.text = Some(p.title.clone());
            self.emit_status();
        }
    }

    pub(super) async fn save_plan(&self, d: &Value) -> (plans::Store, Result<PlanMeta, String>) {
        let store = plans::store_for(&self.cwd).await;
        let uri = d["uri"].as_str().unwrap_or_default();
        let slug = uri.strip_prefix("local://").and_then(|u| u.strip_suffix("-plan.md")).unwrap_or_default();
        let content = match d["content"].as_str() {
            Some(c) => c.to_string(),
            None => self.tools.resolve(uri).ok().and_then(|r| std::fs::read_to_string(r.path).ok()).unwrap_or_default(),
        };
        let (base, read) = match &store.root {
            Some(root) => (plans::base_now(root).await, plans::reads_from(&self.messages.to_vec(), &self.cwd, root)),
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
            adopt: self.plan.stored_plan.as_deref(),
        });
        (store, saved)
    }

    /// Starts the approved plan in this session. `None`: it could not start (already reported).
    pub(super) async fn begin_plan(&mut self, p: ExecPlan, cancel: &CancellationToken) -> Option<UserInput> {
        self.exit_plan();
        if let Some(m) = p.decision.model() {
            self.continue_with(m);
        }
        match p.decision {
            PlanDecision::Execute { .. } => {
                self.end_session().await;
                self.reset_conversation();
                self.notice("── new session");
                if !self.ensure_session().await {
                    return None;
                }
            }
            PlanDecision::Compact { .. } => {
                if !self.ensure_session().await {
                    return None;
                }
                match self.compact(Some(PLAN_COMPACT_FOCUS), cancel).await {
                    Outcome::Done => {}
                    Outcome::Skipped(why) => self.notice(why),
                    Outcome::Failed(e) => self.report_compaction_failure(e, cancel),
                }
                if cancel.is_cancelled() {
                    self.notice("Plan approved, but compaction was cancelled — execution not dispatched. Submit a turn to continue.");
                    return None;
                }
            }
            PlanDecision::Keep { .. } => {
                if !self.ensure_session().await {
                    return None;
                }
            }
            _ => return None,
        }
        self.seed_plan(&p);
        self.set_review_base(review::ReviewBase {
            tree: p.base.clone(),
            plan_uri: Some(p.uri.clone()),
            since: p.approved_at,
        });
        if let Some(id) = &p.stored {
            let session = self.session.as_ref().map(|s| s.path.clone());
            if let Err(e) = plans::store_for(&self.cwd).await.approve(id, p.decision.wire_name(), session.as_deref()) {
                self.error(format!("could not record the approval: {e}"));
            }
        }
        self.notice(format!("▸ Plan approved: {}", p.title));
        Some(plan_prompt(&p).into())
    }

    pub(super) async fn list_plans(&self) {
        let store = plans::store_for(&self.cwd).await;
        let (plans, warnings) = store.list(false);
        for w in warnings {
            self.notice(format!("warning: {w}"));
        }
        self.emit(AgentEvent::PlanList(plans));
    }

    /// `/deliver`: runs the injected `DeliverFn`, which wakes this session when the delivery finishes.
    pub(super) async fn deliver(&mut self, id: &str) {
        if !self.ensure_session().await {
            return;
        }
        let Some(session) = self.session.as_ref().map(|s| s.path.clone()) else { return };
        self.notice(format!("starting delivery of plan {id}…"));
        match (self.ports.deliver)(id.to_string(), session, self.cwd.clone()).await {
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
    pub(super) async fn open_plan(&mut self, id: &str) -> Option<Start> {
        let (store, meta, shown, drift) = self.pending_plan(id).await?;
        let (tx, rx) = tokio::sync::oneshot::channel();
        // Esc closes the dialog without a decision, as it ends a turn.
        let cancel = CancellationToken::new();
        self.ports.inbox.borrow_mut().cancel = cancel.clone();
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
        let decision = tokio::select! {
            r = rx => r.ok()?,
            _ = cancel.cancelled() => return None,
        };
        // The user may have edited the file with Ctrl-G while the dialog was up.
        let content = std::fs::read_to_string(store.md_path(id)).unwrap_or(shown);
        self.apply_stored(&store, &meta, content, drift.describe(), decision).await
    }

    pub(super) async fn start_plan(&mut self, id: &str, decision: PlanDecision, anyway: bool) -> Option<Start> {
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

    pub(super) async fn refine_plan(&mut self, id: &str, text: Option<String>) -> Option<Start> {
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
        let decision = match decision {
            PlanDecision::Drop => {
                match store.drop(id) {
                    Ok(()) => self.notice(format!("dropped plan {id}")),
                    Err(e) => self.error(e),
                }
                return None;
            }
            PlanDecision::Refine(text) => {
                if self.plan.plan.is_none() {
                    self.enter_plan().await;
                }
                self.plan.stored_plan = Some(id.to_string());
                if let Err(e) = self.write_local_file(&uri, &content) {
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
            approved => approved,
        };
        Some(Start::Plan(ExecPlan {
            content,
            title: meta.title.clone(),
            uri,
            decision,
            base: crate::review::facts::snapshot_tree(&self.cwd).await.ok(),
            approved_at: now_ms(),
            stored: Some(id.to_string()),
        }))
    }
}
