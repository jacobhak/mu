use super::*;
use crate::review;
use mu_base::permissions::ModelChoice;

impl Agent {
    /// Whether a model selector from a mode file can be used now: a role needs a model set for it,
    /// anything else must resolve.
    fn selector_usable(&self, sel: &str) -> bool {
        match sel.strip_prefix('@') {
            Some(role) => self.ports.providers.cfg.role(role).is_some(),
            None => self.ports.providers.resolve(sel).is_ok(),
        }
    }

    /// Switches this session to mode `name`, leaving the one that is on. `by_model`: the model
    /// asked for it through `enter_mode`, rather than the user.
    pub(super) async fn enter_mode(&mut self, name: &str, by_model: bool) {
        let Some(def) = self.ctx.mode(name).cloned() else {
            self.error(format!("unknown mode {name}"));
            return;
        };
        let chosen = self.plan.entry_model.take();
        if self.plan.mode.is_some() {
            self.leave_mode(true);
        }
        self.ensure_session().await;
        let prev = ModeState {
            name: def.name.clone(),
            prev_model: Some(self.model.model.id.clone()),
            prev_thinking: self.model.thinking,
            by_model,
        };
        if let Some(sel) = &chosen {
            self.set_model(sel);
        } else {
            if let Some(sel) = def.model.iter().find(|s| self.selector_usable(s)) {
                self.set_model(sel);
            }
            if let Some(t) = def.thinking.as_deref().and_then(Thinking::parse) {
                self.set_thinking(t);
            }
        }
        if let Some(s) = self.session.as_mut() {
            s.mode_change(
                name,
                json!({"prevModel": prev.prev_model, "prevThinking": prev.prev_thinking.as_str(), "byModel": by_model}),
            );
        }
        self.plan.mode = Some(prev);
        self.notice(format!("{name} mode on"));
        self.emit_status();
    }

    /// The other models the user may enter mode `name` on, offered with the approval: the session's
    /// own model when the mode would replace it, then each `cycle_order` role that has a model.
    pub(super) fn entry_models(&self, name: &str) -> Vec<ModelChoice> {
        let Some(def) = self.ctx.mode(name) else { return Vec::new() };
        let current = &self.model.model.id;
        let mut out = Vec::new();
        if def.model.iter().any(|s| self.selector_usable(s)) {
            out.push(ModelChoice {
                label: "Yes, on the current model".into(),
                detail: current.clone(),
                selector: current.clone(),
            });
        }
        let cfg = &self.ports.providers.cfg;
        for role in cfg.role_cycle() {
            let selector = format!("@{role}");
            let Ok((spec, _)) = self.ports.providers.resolve(&selector) else { continue };
            if &spec.id != current {
                out.push(ModelChoice { label: format!("Yes, on {selector}"), detail: spec.id, selector });
            }
        }
        out
    }

    pub(super) fn exit_mode(&mut self) {
        self.leave_mode(true);
    }

    /// Leaving to park a document: the session keeps the model and thinking level it worked with,
    /// rather than the ones the mode replaced. `/<mode>` off and an approved document still restore.
    pub(super) fn exit_mode_keeping_model(&mut self) {
        self.leave_mode(false);
    }

    fn leave_mode(&mut self, restore: bool) {
        let Some(p) = self.plan.mode.take() else { return };
        if restore {
            if let Some(m) = p.prev_model {
                self.set_model(&m);
            }
            self.set_thinking(p.prev_thinking);
        }
        if let Some(s) = self.session.as_mut() {
            s.mode_change("none", json!({}));
        }
        self.notice(format!("{} mode off", p.name));
        self.emit_status();
    }

    /// The mode that wrote a stored document; one that is not defined here falls back to plan.
    fn mode_of_document(&self, name: &str) -> Option<context::ModeDef> {
        if let Some(m) = self.ctx.mode(name) {
            return Some(m.clone());
        }
        let plan = self.ctx.mode("plan").cloned();
        match &plan {
            Some(_) => self.notice(format!("mode {name} is not defined here; using plan")),
            None => self.error(format!("mode {name} is not defined here")),
        }
        plan
    }

    /// Gives a fresh session the approved document as a `local://` file, so the path in the
    /// execution prompt resolves, and names the session after it.
    fn seed_plan(&mut self, p: &ExecPlan, noun: &str) {
        if let Err(e) = self.write_local_file(&p.uri, &p.content) {
            self.error(format!("cannot copy the {noun} to {}: {e}", p.uri));
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
        let mode = d["mode"].as_str().unwrap_or("plan");
        let suffix = format!("-{}.md", self.ctx.mode(mode).map_or("plan", |m| m.noun()));
        let slug = uri.strip_prefix("local://").and_then(|u| u.strip_suffix(suffix.as_str())).unwrap_or_default();
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
            mode,
        });
        (store, saved)
    }

    /// Starts the approved plan in this session. `None`: it could not start (already reported).
    pub(super) async fn begin_plan(&mut self, p: ExecPlan, cancel: &CancellationToken) -> Option<UserInput> {
        let m = self.mode_of_document(&p.mode)?;
        let noun = m.noun().to_string();
        self.exit_mode();
        if let Some(model) = p.decision.model() {
            self.continue_with(model);
        }
        match p.decision {
            ModeAction::Execute { .. } => {
                self.end_session().await;
                self.reset_conversation();
                self.notice("── new session");
                if !self.ensure_session().await {
                    return None;
                }
            }
            ModeAction::Compact { .. } => {
                if !self.ensure_session().await {
                    return None;
                }
                match self.compact(Some(&m.exit.compact_focus), cancel).await {
                    Outcome::Done => {}
                    Outcome::Skipped(why) => self.notice(why),
                    Outcome::Failed(e) => self.report_compaction_failure(e, cancel),
                }
                if cancel.is_cancelled() {
                    self.notice(format!("{} approved, but compaction was cancelled — execution not dispatched. Submit a turn to continue.", context::capitalize(&noun)));
                    return None;
                }
            }
            ModeAction::Keep { .. } => {
                if !self.ensure_session().await {
                    return None;
                }
            }
            _ => return None,
        }
        self.seed_plan(&p, &noun);
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
        self.notice(format!("▸ {} approved: {}", context::capitalize(&noun), p.title));
        Some(plan_prompt(&p, &m).into())
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
        let m = self.mode_of_document(&meta.mode)?;
        let (tx, rx) = tokio::sync::oneshot::channel();
        // Esc closes the dialog without a decision, as it ends a turn.
        let cancel = CancellationToken::new();
        self.ports.inbox.borrow_mut().cancel = cancel.clone();
        let note =
            format!("written against {} · {}", crate::plans::base_label(&meta, &self.cwd).await, drift.describe());
        let request =
            context::mode_request(&m, &meta.title, &m.file_uri(id), store.md_path(id), shown.clone(), true, Some(note));
        self.emit(AgentEvent::Review { request, reply: tx });
        let choice = tokio::select! {
            r = rx => r.ok()?,
            _ = cancel.cancelled() => return None,
        };
        let decision = context::choice_action(&choice)?;
        // The user may have edited the file with Ctrl-G while the dialog was up.
        let content = std::fs::read_to_string(store.md_path(id)).unwrap_or(shown);
        self.apply_stored(&store, &meta, &m, content, drift.describe(), decision).await
    }

    pub(super) async fn start_plan(&mut self, id: &str, decision: ModeAction, anyway: bool) -> Option<Start> {
        let (store, meta, content, drift) = self.pending_plan(id).await?;
        if matches!(drift, plans::Drift::Changed { .. }) && !anyway {
            self.error(format!(
                "plan '{id}' is stale: {}. Refine it with `mu plan refine {id}` or run it anyway with --anyway.",
                drift.describe()
            ));
            return None;
        }
        let m = self.mode_of_document(&meta.mode)?;
        self.apply_stored(&store, &meta, &m, content, drift.describe(), decision).await
    }

    pub(super) async fn refine_plan(&mut self, id: &str, text: Option<String>) -> Option<Start> {
        let (store, meta, content, drift) = self.pending_plan(id).await?;
        let m = self.mode_of_document(&meta.mode)?;
        let text = text.unwrap_or_else(|| format!("Bring the {} up to date with the current code.", m.noun()));
        self.apply_stored(&store, &meta, &m, content, drift.describe(), ModeAction::Refine(text)).await
    }

    /// Carries out a decision on a stored document; returns the turn to run, if any.
    async fn apply_stored(
        &mut self,
        store: &plans::Store,
        meta: &PlanMeta,
        m: &context::ModeDef,
        content: String,
        drift: String,
        decision: ModeAction,
    ) -> Option<Start> {
        let id = meta.id.as_str();
        let uri = m.file_uri(id);
        let noun = m.noun();
        let decision = match decision {
            ModeAction::Drop => {
                match store.drop(id) {
                    Ok(()) => self.notice(format!("dropped {noun} {id}")),
                    Err(e) => self.error(e),
                }
                return None;
            }
            ModeAction::Refine(text) => {
                if self.plan.mode.as_ref().is_none_or(|s| s.name != m.name) {
                    self.enter_mode(&m.name, false).await;
                }
                self.plan.stored_plan = Some(id.to_string());
                if let Err(e) = self.write_local_file(&uri, &content) {
                    self.error(format!("cannot copy the {noun} to {uri}: {e}"));
                    return None;
                }
                let prompt = m
                    .render(&m.exit.refine)
                    .replace("{{id}}", id)
                    .replace("{{path}}", &uri)
                    .replace("{{drift}}", &drift)
                    .replace("{{request}}", &text);
                return Some(Start::Synthetic(prompt.into()));
            }
            ModeAction::Park | ModeAction::Exit => return None,
            approved => approved,
        };
        Some(Start::Plan(ExecPlan {
            content,
            title: meta.title.clone(),
            uri,
            decision,
            mode: m.name.clone(),
            base: mu_base::git::snapshot_tree(&self.cwd).await.ok(),
            approved_at: now_ms(),
            stored: Some(id.to_string()),
        }))
    }
}
