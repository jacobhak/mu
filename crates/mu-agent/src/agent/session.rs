use super::*;
use mu_session::{inbox, session};

impl Agent {
    pub fn session_id(&self) -> String {
        self.session.as_ref().map(|s| s.id()).unwrap_or_default()
    }

    /// Makes sure a session file exists. `false` (with an error shown) when it cannot be created.
    pub(super) async fn ensure_session(&mut self) -> bool {
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
    pub(super) fn apply_plugin_context(&mut self, results: Vec<(String, Value)>) {
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

    pub(super) fn continue_with(&mut self, sel: &str) {
        let before = self.model.id.clone();
        self.set_model(sel);
        if self.model.id != before {
            self.notice(format!("continuing with {sel}: {}", self.model.id));
        }
    }

    pub(super) fn reset_conversation(&mut self) {
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

    pub(super) async fn end_session(&mut self) {
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
    pub(super) fn replay_history(&self) {
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
}
