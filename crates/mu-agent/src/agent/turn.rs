use super::*;

impl Agent {
    pub fn push(&mut self, mut m: Message) {
        let hits = self.redact_message(&mut m);
        if !hits.is_empty() && matches!(m, Message::User { .. }) {
            let names: Vec<String> = hits.iter().map(secrets::Hit::placeholder).collect();
            self.notice(format!("secrets replaced before sending: {}", names.join(", ")));
        }
        self.log_redaction("message", &hits);
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

    /// One turn. `first` is `None` for a wake-up that only delivers finished background jobs.
    pub async fn turn(&mut self, first: Option<UserInput>) {
        self.turn_from(first.map_or(Start::Wake, Start::User)).await;
    }

    pub(super) async fn turn_from(&mut self, start: Start) {
        if matches!(start, Start::Wake)
            && !self.jobs.as_ref().is_some_and(|j| j.has_undelivered())
            && !self.notify_pending(true)
        {
            return;
        }
        // An approved stored plan opens its own session when it needs one.
        if !matches!(start, Start::Plan(_)) && !self.ensure_session().await {
            // Without this the caller (e.g. a headless subagent) waits for a `TurnEnd` forever.
            self.emit(AgentEvent::TurnEnd { aborted: true });
            return;
        }
        let cancel = CancellationToken::new();
        self.inbox.borrow_mut().cancel = cancel.clone();
        self.emit(AgentEvent::TurnStart);
        let wake = matches!(start, Start::Wake);
        let mut aborted = false;
        let mut synthetic = false;
        let mut next = match start {
            Start::User(i) => Some(Some(i)),
            Start::Synthetic(i) => {
                synthetic = true;
                Some(Some(i))
            }
            Start::Wake => Some(None),
            Start::Plan(p) => match self.begin_plan(p, &cancel).await {
                Some(i) => {
                    synthetic = true;
                    Some(Some(i))
                }
                None => {
                    aborted = true;
                    None
                }
            },
        };
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
                    let prompt = secrets::redact(&i.text, &self.cwd).text;
                    let res = self
                        .plugins
                        .hook(
                            "prompt_submit",
                            json!({"prompt": prompt, "userTurn": self.user_turn, "resumed": self.resumed}),
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
                Some(i) => {
                    self.take_notify();
                    if synthetic {
                        self.push(Message::synthetic_user(&i.text));
                    } else {
                        self.push(Message::user_input(i));
                    }
                }
                None => {
                    let jobs = self.take_jobs();
                    if !(self.take_notify() | jobs) {
                        break;
                    }
                    // Only a wake that delivered something is a woken turn (and rings at its end).
                    if wake {
                        self.emit(AgentEvent::Woken);
                    }
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
                    self.emit(AgentEvent::TurnFailed);
                    self.exec_plan = None;
                    break;
                }
                Flow::Stopped => {
                    self.exec_plan = None;
                    self.turn_end_hook().await;
                    break;
                }
                Flow::Done => {}
            }
            self.turn_end_hook().await;
            if let Some(p) = self.exec_plan.take() {
                match self.begin_plan(p, &cancel).await {
                    Some(i) => {
                        synthetic = true;
                        next = Some(Some(i));
                        continue;
                    }
                    None => {
                        aborted = true;
                        break;
                    }
                }
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

    /// Saves the plan a `propose_plan` result describes into the repository's store.
    async fn turn_end_hook(&mut self) {
        let msgs = std::mem::take(&mut self.turn_msgs);
        self.plugins.hook("turn_end", json!({"userTurn": self.user_turn, "messages": msgs})).await;
    }

    /// The end-of-turn `ask` reminder may fire: `ask` is offered to this run, and the current user
    /// turn has neither called `ask` nor been reminded yet.
    fn ask_nudge_due(&self) -> bool {
        if !self.providers.cfg.mu.ask.nudge || self.subagent || !self.policy().permits("ask") {
            return false;
        }
        for m in self.messages.iter().rev() {
            match m {
                Message::User { content, .. } if ask_nudge::is_reminder(&text_of(content)) => return false,
                Message::User { .. } => return true,
                Message::Assistant { .. } if tool_calls(m).iter().any(|(_, name, _)| name == "ask") => return false,
                _ => {}
            }
        }
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
            // MCP servers without a cached tool list are started and listed once, before the first request.
            // A subagent restricted to named tools that include no MCP tool never needs the listing.
            if self.allow.as_ref().is_none_or(|a| a.iter().any(|n| n.starts_with("mcp__"))) {
                self.plugins.list_missing_until(cancel).await;
            }
            if cancel.is_cancelled() {
                return Flow::Aborted;
            }
            let system = self.system();
            let sid = self.session_id();
            let wire = self.wire_messages();
            // Every tool call in the history has its result and vice versa (`to_wire` also repairs).
            debug_assert!(mu_base::message::check_tool_pairs(&wire).is_ok(), "unpaired tool calls in the request");
            let defs = self.tool_defs();
            // Held until the reply ends: a reply that turns out to be a dismissal is never shown.
            let answering_reminder = matches!(self.messages.last(), Some(Message::User { content, .. }) if ask_nudge::is_reminder(&text_of(content)));
            let mut held = Vec::new();
            let reply = {
                let req = Request {
                    model: &self.model,
                    thinking: self.live_thinking.clone(),
                    system: &system,
                    tools: &defs,
                    messages: &wire,
                    max_tokens: None,
                    session_id: Some(&sid),
                };
                let tx = self.events.clone();
                let mut ev = |e: StreamEvent| {
                    let e = match e {
                        StreamEvent::Text(t) => AgentEvent::Text(t.into()),
                        StreamEvent::Thinking(t) => AgentEvent::Thinking(t.into()),
                        StreamEvent::ToolCallStart { id, name } => {
                            AgentEvent::ToolCallStart { id: id.into(), name: name.into() }
                        }
                        StreamEvent::ToolCallDelta { id, json } => {
                            AgentEvent::ToolCallDelta { id: id.into(), json: json.into() }
                        }
                        StreamEvent::Retry { attempt, delay, reason } => AgentEvent::Retry { attempt, delay, reason },
                    };
                    if answering_reminder {
                        held.push(e);
                    } else {
                        let _ = tx.send(e);
                    }
                };
                self.providers.stream(&req, cancel, &mut ev).await
            };
            let dismissed = answering_reminder && matches!(&reply, Ok(m) if is_dismissal(m));
            if !dismissed {
                for e in held {
                    self.emit(e);
                }
            }
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
            if !dismissed {
                self.emit(AgentEvent::BlockEnd);
            }
            let calls = tool_calls(&msg);
            let stop = match &msg {
                Message::Assistant { stop_reason, .. } => *stop_reason,
                _ => StopReason::Stop,
            };
            let prose_question = stop == StopReason::Stop
                && calls.is_empty()
                && matches!(&msg, Message::Assistant { content, .. } if ask_nudge::asks_in_prose(&text_of(content)));
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
                    let reason = SKIP_TRUNCATED;
                    let out = ToolOutput::err(reason);
                    self.emit(AgentEvent::ToolSkipped { id: id.clone() });
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
                let delivered = self.take_jobs() | self.take_notify();
                if steered || delivered {
                    continue;
                }
                if self.inbox.borrow().stop {
                    return Flow::Stopped;
                }
                // Stop reminder: the model is about to finish with open todo items. Nudge it to
                // update the list or finish the work, at most twice per run so it cannot loop.
                let open = if stop == StopReason::Stop && todo_nags < 2 { self.tools.todo_open() } else { Vec::new() };
                if !open.is_empty() {
                    todo_nags += 1;
                    let list = open.iter().map(|c| format!("- {c}")).collect::<Vec<_>>().join("\n");
                    self.push(Message::user(&format!(
                        "<system-reminder>\nYou stopped with {} open todo item(s):\n{list}\nContinue the work on them, marking each `done` only when you have actually finished it. Do not tick items off that you did not do. `drop` an item that is no longer needed; `block` it if you are waiting on the user. Do not reply to this reminder.\n</system-reminder>",
                        open.len()
                    )));
                    continue;
                }
                // Ask reminder: the turn ends with a decision question in prose. Once per user turn.
                if prose_question && self.ask_nudge_due() {
                    self.push(Message::user(ask_nudge::REMINDER));
                    continue;
                }
                return Flow::Done;
            }
            let mut skip: Option<&str> = None;
            let mut yielded = false;
            for (id, name, args) in calls {
                if skip.is_none() {
                    if cancel.is_cancelled() {
                        skip = Some(SKIP_ABORTED);
                    } else if self.inbox.borrow().stop {
                        skip = Some(SKIP_STOPPED);
                    }
                }
                if let Some(reason) = skip {
                    let out = ToolOutput::err(reason);
                    self.emit(AgentEvent::ToolSkipped { id: id.clone() });
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
                yielded |= name == "yield" && !out.is_error;
                if cancel.is_cancelled() {
                    skip = Some(SKIP_ABORTED);
                } else if self.inbox.borrow().stop {
                    skip = Some(SKIP_STOPPED);
                } else if yielded {
                    skip = Some(SKIP_YIELDED);
                } else if !self.inbox.borrow().steer.is_empty() {
                    skip = Some(SKIP_STEER);
                }
            }
            if cancel.is_cancelled() {
                return Flow::Aborted;
            }
            if self.inbox.borrow().stop {
                return Flow::Stopped;
            }
            // A successful yield is the subagent's result; another request would only produce an unused reply.
            if yielded || self.exec_plan.is_some() {
                return Flow::Done;
            }
            self.take_steer();
            self.take_jobs();
            self.take_notify();
        }
    }
}
