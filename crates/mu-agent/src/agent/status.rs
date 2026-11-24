use super::*;
use mu_base::events::{CacheBreak, CacheStatus};
use mu_base::models::{ThinkingMode, same_origin};
use mu_provider::auth::chatgpt;
use mu_provider::provider::custom::AuthMode;
use mu_provider::provider::providers::native_def;

/// What a request's prompt-cache prefix is built from. A field that changed since the last request
/// makes the next one miss the cache.
#[derive(Clone, PartialEq)]
pub(super) struct PrefixKey {
    model: String,
    thinking: Thinking,
    system: u64,
    tools: u64,
    summary: u64,
}

/// Context tokens a reply's usage reports, when it describes a request to `target`: same api,
/// provider and model, a completed reply, and a usage that counts any input.
fn usage_anchor(m: &Message, target: &ModelSpec) -> Option<u64> {
    let Message::Assistant { api, provider, model, usage, stop_reason, .. } = m else { return None };
    let input = usage.input + usage.cache_read + usage.cache_write;
    (same_origin(api, provider, target)
        && model == &target.id
        && !matches!(stop_reason, StopReason::Error | StopReason::Aborted)
        && input > 0)
        .then_some(input + usage.output)
}

impl Agent {
    /// A hash of what the system prompt is built from, so it is only measured again when that changes.
    pub(crate) fn system_key(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        (
            &self.cwd,
            &self.model.model.id,
            self.plan.mode.as_ref().map(|m| &m.name),
            &self.plugin_ctx,
            &self.run.agent_prompt,
            self.run.subagent,
            self.policy().permits("ask"),
            self.policy().permits("task"),
        )
            .hash(&mut h);
        self.ports.plugins.mcp_instructions().hash(&mut h);
        h.finish()
    }

    /// Tokens of the system prompt, measured again only when what it is built from changes.
    pub(crate) fn system_tokens(&self) -> u64 {
        let key = self.system_key();
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

    /// Sum of the messages' estimates for the current model, kept until the message list or the
    /// target model changes.
    pub(crate) fn messages_tokens(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        (self.model.model.api.as_str(), &self.model.model.provider, &self.model.model.id).hash(&mut h);
        let key = (self.messages.len(), self.messages.last().map_or(0, Message::timestamp), h.finish());
        let mut memo = self.memo.messages.borrow_mut();
        match *memo {
            Some((k, t)) if k == key => t,
            _ => {
                let t = self.messages.iter().map(|m| m.estimate_tokens(&self.model.model)).sum();
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

    /// A native model's reasoning support is what its spec says; unknown-price catalogue heuristics
    /// are for gateway models only.
    fn reasoning_capable(&self) -> bool {
        let m = &self.model.model;
        if m.native {
            return !matches!(m.thinking.mode, ThinkingMode::None | ThinkingMode::Gateway);
        }
        pricing::reasoning(&m.id).unwrap_or(true)
    }

    /// The model is a custom endpoint configured without authentication.
    fn native_keyless(&self) -> bool {
        let m = &self.model.model;
        m.native
            && native_def(&m.provider).is_none()
            && self
                .ports
                .providers
                .custom_registry()
                .get(&m.provider)
                .ok()
                .flatten()
                .is_some_and(|c| c.auth == AuthMode::None)
    }

    pub fn snapshot(&self) -> StatusSnapshot {
        let none = self.model.model.is_unselected();
        StatusSnapshot {
            model_id: self.model.model.id.clone(),
            model_name: if none { "no model".into() } else { pricing::display_name(&self.model.model.id) },
            reasoning: !none && self.reasoning_capable(),
            thinking: self.model.live_thinking.get().as_str().into(),
            subscription: self.on_chatgpt_plan()
                || (!self.gateway_idle() && self.costs.quota.borrow().is_subscription(&self.model.model.provider)),
            native: self.model.model.native,
            native_keyless: self.native_keyless(),
            chatgpt_plan: self.on_chatgpt_plan(),
            mode: self.plan.mode.as_ref().map(|m| m.name.clone()),
            context_tokens: Some(self.context_tokens()),
            context_window: self.model.model.context_window,
            cost: self.costs.cost.total,
            tokens_in: self.costs.cost.tokens_in,
            tokens_out: self.costs.cost.tokens_out,
            title: self.title.text.clone(),
            session_path: self.session.as_ref().map(|s| s.path.to_string_lossy().into_owned()),
            local_dir: self.session.as_ref().map(|_| self.tools.local_root().to_string_lossy().into_owned()),
            git: self.costs.git.borrow().clone(),
            usage: if self.gateway_idle() {
                None
            } else {
                self.costs.quota.borrow().usage_for(&self.model.model.provider)
            },
            permissions: self.tools.permissions.mode(),
            cache: self.cache_status(),
        }
    }

    /// Git state for the status line, read off the draw path.
    pub(super) async fn refresh_external(&mut self) {
        *self.costs.git.borrow_mut() = git::git_status(&self.cwd).await;
        self.tick_quota();
        self.settle_prices();
    }

    pub(super) async fn refresh_status(&mut self) {
        if !self.gateway_idle() {
            self.costs.quota.borrow_mut().force_refresh(&self.model.model.provider);
        }
        *self.costs.git.borrow_mut() = git::git_status(&self.cwd).await;
        self.emit_status();
    }

    /// Reads the working tree every `period` for as long as the front end listens, in a turn or out
    /// of one, and sends the last status again when it changed. A turn only emits status as replies
    /// arrive, and an idle agent not at all, so edits made meanwhile would otherwise show late.
    pub(crate) fn watch_git(&self, period: std::time::Duration) {
        let (cwd, git, last, events) = (
            self.costs.watch_cwd.clone(),
            self.costs.git.clone(),
            self.costs.last_status.clone(),
            self.ports.events.clone(),
        );
        tokio::task::spawn_local(async move {
            let mut tick = tokio::time::interval(period);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            tick.tick().await;
            while !events.is_closed() {
                tick.tick().await;
                // A read that timed out under load says nothing; it is not a repository that vanished.
                let dir = cwd.borrow().clone();
                let Some(now) = git::git_status(&dir).await else { continue };
                // A switch during the read: its result is for the old directory.
                if *cwd.borrow() != dir {
                    continue;
                }
                if git.borrow().as_ref() == Some(&now) {
                    continue;
                }
                let now = Some(now);
                *git.borrow_mut() = now.clone();
                let Some(mut s) = last.borrow().clone() else { continue };
                s.git = now;
                *last.borrow_mut() = Some(s.clone());
                let _ = events.send(AgentEvent::Status(s));
            }
        });
    }

    /// Fetches and applies the provider's quota on its own schedule, in a turn or out of one, and
    /// sends the last status again with the new windows. Without this a fetch finished mid-turn
    /// waited for the next reply to be applied or to start the following one.
    pub(crate) fn watch_quota(&self) {
        self.costs.quota.borrow_mut().set_provider(&self.model.model.provider);
        let (quota, last, events) =
            (self.costs.quota.clone(), self.costs.last_status.clone(), self.ports.events.clone());
        let mut wake = quota.borrow_mut().take_wake();
        tokio::task::spawn_local(async move {
            loop {
                let wait = quota.borrow().wait();
                tokio::select! {
                    () = tokio::time::sleep(wait) => {}
                    Some(()) = recv_opt(&mut wake) => {}
                }
                if events.is_closed() {
                    break;
                }
                let mut q = quota.borrow_mut();
                if q.is_paused() {
                    continue;
                }
                let provider = q.provider().to_string();
                let (usage, sub) = (q.usage_for(&provider), q.is_subscription(&provider));
                q.tick(&provider);
                let (now_usage, now_sub) = (q.usage_for(&provider), q.is_subscription(&provider));
                drop(q);
                if (usage, sub) == (now_usage.clone(), now_sub) {
                    continue;
                }
                let Some(mut s) = last.borrow().clone() else { continue };
                s.usage = now_usage;
                s.subscription = s.chatgpt_plan || now_sub;
                *last.borrow_mut() = Some(s.clone());
                let _ = events.send(AgentEvent::Status(s));
            }
        });
    }

    /// Native models have no gateway quota to poll.
    pub(super) fn tick_quota(&mut self) {
        if self.gateway_idle() {
            return;
        }
        let provider = self.model.model.provider.clone();
        self.costs.quota.borrow_mut().tick(&provider);
    }

    /// Prices usage recorded before the table knew its model. A model the table does not list forces
    /// one catalogue refresh per process.
    pub(super) fn settle_prices(&mut self) {
        if self.costs.cost.settle() && self.costs.quota.borrow().enabled && !self.gateway_idle() {
            tokio::task::spawn_local(pricing::refresh_missing(self.ports.providers.gateway_prices()));
        }
    }

    /// Picks up gateway-reported context windows once they are known.
    pub(super) fn refresh_model_meta(&mut self) {
        if let Ok((spec, _)) = self.ports.providers.resolve(&self.model.model.id) {
            // A custom endpoint whose definition vanished mid-session stays native, so its next
            // request fails locally instead of starting the gateway.
            let m = &self.model.model;
            if m.native && !spec.native && native_def(&m.provider).is_none() {
                return;
            }
            self.model.model = spec;
            self.sync_model_state();
        }
    }

    /// Native models pause quota polling. The gateway plugin's price list is read only once a gateway model
    /// is in use, so a native-only session never starts the gateway.
    pub(super) fn sync_model_state(&mut self) {
        self.costs.quota.borrow_mut().set_paused(self.gateway_idle());
        if !self.gateway_idle() && !self.model.pricing_checked && !self.ports.providers.is_relayed() {
            self.model.pricing_checked = true;
            tokio::task::spawn_local(pricing::refresh(self.ports.providers.gateway_prices()));
        }
    }

    pub(crate) fn emit_status(&self) {
        let s = self.snapshot();
        self.costs.quota.borrow_mut().set_provider(&self.model.model.provider);
        *self.costs.last_status.borrow_mut() = Some(s.clone());
        self.emit(AgentEvent::Status(s));
    }

    pub(crate) fn fresh_usage_tokens(&self) -> Option<u64> {
        let msgs = self.messages.entries().get(self.compaction.fresh_from..)?;
        let target = &self.model.model;
        let (i, anchor) = msgs.iter().enumerate().rev().find_map(|(i, e)| Some((i, usage_anchor(&e.msg, target)?)))?;
        let trailing: u64 = msgs[i + 1..].iter().map(|e| e.msg.estimate_tokens(target)).sum();
        Some(anchor + trailing)
    }

    /// The prefix a request sent now would start with, for comparing against the last request sent.
    pub(super) fn prefix_key(&self, defs: &[ToolDef]) -> PrefixKey {
        use std::hash::{Hash, Hasher};
        let mut tools = std::collections::hash_map::DefaultHasher::new();
        for d in defs {
            d.name.hash(&mut tools);
            d.description.hash(&mut tools);
            d.est_bytes().hash(&mut tools);
        }
        let summary = self.compaction.state.as_ref().map_or(0, |c| {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            c.summary.hash(&mut h);
            h.finish()
        });
        PrefixKey {
            model: self.model.model.id.clone(),
            thinking: self.model.live_thinking.get(),
            system: self.system_key(),
            tools: tools.finish(),
            summary,
        }
    }

    /// The prompt cache as of the last reply, with what changed since the last request. `None` when
    /// no reply has reported a cache hit.
    fn cache_status(&self) -> Option<CacheStatus> {
        let (usage, at_ms) = self.messages.iter().rev().find_map(|m| match m {
            Message::Assistant { usage, timestamp, .. } if usage.cache_read + usage.cache_write > 0 => {
                Some((usage, *timestamp))
            }
            _ => None,
        })?;
        let changed = self.memo.sent_prefix.as_ref().and_then(|sent| {
            let now = self.prefix_key(&self.tool_defs());
            if sent.model != now.model {
                Some(CacheBreak::Model)
            } else if sent.thinking != now.thinking {
                Some(CacheBreak::Thinking)
            } else if sent.tools != now.tools {
                Some(CacheBreak::Tools)
            } else if sent.system != now.system {
                Some(CacheBreak::SystemPrompt)
            } else if sent.summary != now.summary {
                Some(CacheBreak::Compacted)
            } else {
                None
            }
        });
        Some(CacheStatus {
            input: usage.input,
            cache_read: usage.cache_read,
            cache_write: usage.cache_write,
            at_ms,
            changed,
        })
    }

    pub(crate) fn set_model(&mut self, sel: &str) {
        match self.ports.providers.resolve(sel) {
            Ok((spec, level)) => {
                if let Some(s) = self.session.as_mut() {
                    s.model_change(&spec.id);
                }
                if self.model.model.provider != spec.provider {
                    self.costs.quota.borrow_mut().provider_changed();
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

#[cfg(test)]
mod tests {
    use super::*;
    use mu_base::message::Usage;

    fn reply(provider: &str, model: &str, stop: StopReason, input: u64) -> Message {
        Message::Assistant {
            content: vec![],
            api: "anthropic-messages".into(),
            provider: provider.into(),
            model: model.into(),
            usage: Usage { input, ..Default::default() },
            stop_reason: stop,
            timestamp: 0,
            response_id: None,
            error_message: None,
        }
    }

    #[test]
    fn usage_anchors_only_on_a_reply_to_the_same_target() {
        let target = ModelSpec::gateway("anthropic/claude", 200_000, 32_000);
        let r = |provider, model, stop, input| usage_anchor(&reply(provider, model, stop, input), &target);
        assert_eq!(r("anthropic", "anthropic/claude", StopReason::Stop, 100), Some(100));
        assert_eq!(r("openai", "anthropic/claude", StopReason::Stop, 100), None);
        assert_eq!(r("anthropic", "anthropic/other", StopReason::Stop, 100), None);
        assert_eq!(r("anthropic", "anthropic/claude", StopReason::Aborted, 100), None);
        assert_eq!(r("anthropic", "anthropic/claude", StopReason::Error, 100), None);
        assert_eq!(r("anthropic", "anthropic/claude", StopReason::Stop, 0), None);
    }
}
