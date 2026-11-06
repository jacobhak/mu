use super::*;
use mu_provider::auth::chatgpt;

impl Agent {
    /// Tokens of the system prompt, measured again only when what it is built from changes.
    pub(crate) fn system_tokens(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        (
            &self.model.model.id,
            self.plan.plan.is_some(),
            &self.plugin_ctx,
            &self.run.agent_prompt,
            self.run.subagent,
            self.policy().permits("ask"),
        )
            .hash(&mut h);
        self.ports.plugins.mcp_instructions().hash(&mut h);
        let key = h.finish();
        let mut memo = self.memo.system.borrow_mut();
        match *memo {
            Some((k, t)) if k == key => t,
            _ => {
                let t = estimate_tokens(&self.system());
                *memo = Some((key, t));
                t
            }
        }
    }

    /// Sum of the messages' estimates, kept until the message list changes.
    pub(crate) fn messages_tokens(&self) -> u64 {
        let key = (self.messages.len(), self.messages.last().map_or(0, Message::timestamp));
        let mut memo = self.memo.messages.borrow_mut();
        match *memo {
            Some((k, t)) if k == key => t,
            _ => {
                let t = self.messages.iter().map(Message::estimate_tokens).sum();
                *memo = Some((key, t));
                t
            }
        }
    }

    /// The current model runs on the ChatGPT plan: reported like a subscription, never billed per token.
    fn on_chatgpt_plan(&self) -> bool {
        self.model.model.provider == chatgpt::PROVIDER_ID
    }

    /// Nothing is asked of the gateway: the model is native or none is chosen yet.
    fn gateway_idle(&self) -> bool {
        self.model.model.native || self.model.model.is_unselected()
    }

    pub fn snapshot(&self) -> StatusSnapshot {
        let none = self.model.model.is_unselected();
        StatusSnapshot {
            model_id: self.model.model.id.clone(),
            model_name: if none { "no model".into() } else { pricing::display_name(&self.model.model.id) },
            reasoning: !none && pricing::reasoning(&self.model.model.id).unwrap_or(true),
            thinking: self.model.thinking.as_str().into(),
            subscription: self.on_chatgpt_plan()
                || (!self.gateway_idle() && self.costs.quota.is_subscription(&self.model.model.provider)),
            native: self.model.model.native,
            chatgpt_plan: self.on_chatgpt_plan(),
            plan: self.plan.plan.is_some(),
            context_tokens: Some(self.context_tokens()),
            context_window: self.model.model.context_window,
            cost: self.costs.cost.total,
            tokens_in: self.costs.cost.tokens_in,
            tokens_out: self.costs.cost.tokens_out,
            title: self.title.text.clone(),
            session_path: self.session.as_ref().map(|s| s.path.to_string_lossy().into_owned()),
            git: self.costs.git.clone(),
            usage: if self.gateway_idle() { None } else { self.costs.quota.usage_for(&self.model.model.provider) },
        }
    }

    /// Git state for the status line. Runs between turns, never from the draw path.
    pub(super) async fn refresh_external(&mut self) {
        self.costs.git = git::git_status(&self.cwd).await;
        self.tick_quota();
        self.settle_prices();
    }

    /// Native models are metered by API key: there is no subscription quota to poll.
    pub(super) fn tick_quota(&mut self) {
        if self.gateway_idle() {
            return;
        }
        let provider = self.model.model.provider.clone();
        self.costs.quota.tick(&provider);
    }

    /// Prices usage recorded before the table knew its model. A model the table does not list forces
    /// one catalogue refresh per process.
    pub(super) fn settle_prices(&mut self) {
        if self.costs.cost.settle() && self.costs.quota.enabled && !self.gateway_idle() {
            tokio::task::spawn_local(pricing::refresh_missing());
        }
    }

    /// Picks up gateway-reported context windows once they are known.
    pub(super) fn refresh_model_meta(&mut self) {
        if let Ok((spec, _)) = self.ports.providers.resolve(&self.model.model.id) {
            self.model.model = spec;
            self.sync_model_state();
        }
    }

    /// Native models pause quota polling. omp's catalogue is read only once a gateway model is in use,
    /// so a native-only session never starts omp.
    pub(super) fn sync_model_state(&mut self) {
        self.costs.quota.set_paused(self.gateway_idle());
        if !self.gateway_idle() && !self.model.pricing_checked {
            self.model.pricing_checked = true;
            tokio::task::spawn_local(pricing::refresh());
        }
    }

    pub(crate) fn emit_status(&self) {
        self.emit(AgentEvent::Status(self.snapshot()));
    }

    pub(crate) fn fresh_usage_tokens(&self) -> Option<u64> {
        let msgs = self.messages.get(self.compaction.fresh_from..)?;
        let i = msgs.iter().rposition(|m| matches!(m, Message::Assistant { .. }))?;
        let Message::Assistant { usage, .. } = &msgs[i] else { return None };
        let trailing: u64 = msgs[i + 1..].iter().map(Message::estimate_tokens).sum();
        Some(usage.input + usage.cache_read + usage.cache_write + usage.output + trailing)
    }

    pub(crate) fn set_model(&mut self, sel: &str) {
        match self.ports.providers.resolve(sel) {
            Ok((spec, level)) => {
                if let Some(s) = self.session.as_mut() {
                    s.model_change(&spec.id);
                }
                if self.model.model.provider != spec.provider {
                    self.costs.quota.provider_changed();
                }
                self.model.model = spec;
                self.sync_model_state();
                if let Some(l) = level {
                    self.set_thinking(l);
                }
                self.emit_status();
            }
            Err(e) => self.error(e.to_string()),
        }
    }

    pub(crate) fn set_thinking(&mut self, t: Thinking) {
        self.model.thinking = t;
        self.model.live_thinking.set(t);
        if let Some(s) = self.session.as_mut() {
            s.thinking_change(t.as_str());
        }
        self.emit_status();
    }
}
