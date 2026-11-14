use super::*;
use mu_tools::permissions::Verdict;

/// Whether the loop compacts before its next request.
#[derive(Clone, Copy, PartialEq)]
enum CompactionCheck {
    /// The first request of a pass, and the retry after an overflow compaction, are not checked.
    Skip,
    Due,
}

/// Whether an overflow was already answered with a compaction in this pass.
#[derive(Clone, Copy, PartialEq)]
enum Overflow {
    Fresh,
    Compacted,
}

struct LoopState {
    compaction: CompactionCheck,
    overflow: Overflow,
    /// Stop reminders sent so far: at most two per pass.
    todo_nags: u8,
}

impl Default for LoopState {
    fn default() -> Self {
        Self { compaction: CompactionCheck::Skip, overflow: Overflow::Fresh, todo_nags: 0 }
    }
}

impl Agent {
    pub fn push(&mut self, m: Message) {
        self.push_as(m, false);
    }

    /// Stores the end-of-turn `ask` reminder, marked so that nothing has to recognise its text.
    fn push_reminder(&mut self) {
        self.push_as(Message::user(ask_nudge::REMINDER), true);
    }

    fn push_as(&mut self, mut m: Message, reminder: bool) {
        let hits = self.redact_message(&mut m);
        if !hits.is_empty() && matches!(m, Message::User { .. }) {
            let names: Vec<String> = hits.iter().map(secrets::Hit::placeholder).collect();
            self.notice(format!("secrets replaced before sending: {}", names.join(", ")));
        }
        self.log_redaction("message", &hits);
        if let Message::Assistant { usage, provider, model, .. } = &mut m {
            self.costs.cost.record(provider, model, usage);
        }
        let plugin_view =
            if m.image_stats().0 > 0 { serde_json::to_value(m.without_images()) } else { serde_json::to_value(&m) };
        if let Ok(v) = plugin_view {
            self.turn_msgs.push(v);
        }
        let id = self.session.as_mut().and_then(|s| s.message(&m));
        // A reply's status goes out from `record_reply`, once its quota is applied.
        let reply = matches!(m, Message::Assistant { .. });
        self.messages.push(m, id, reminder);
        self.compaction.since += 1;
        if !reply {
            self.emit_status();
        }
    }

    /// One turn. `first` is `None` for a wake-up that only delivers finished background jobs.
    pub async fn turn(&mut self, first: Option<UserInput>) {
        self.turn_from(first.map_or(Start::Wake, Start::User)).await;
    }

    pub(super) async fn turn_from(&mut self, start: Start) {
        if self.model.model.is_unselected() {
            self.error("no model chosen: sign in with /login if you have not, then pick one with /switch");
            self.emit(AgentEvent::TurnEnd { aborted: true });
            return;
        }
        if matches!(start, Start::Wake)
            && !self.tools.jobs.as_ref().is_some_and(|j| j.has_undelivered())
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
        self.ports.inbox.borrow_mut().cancel = cancel.clone();
        self.emit_turn_start();
        let mut aborted = false;
        let mut next = match start {
            Start::User(input) => Some(Step::Prompt { input, synthetic: false }),
            Start::Synthetic(input) => Some(Step::Prompt { input, synthetic: true }),
            Start::Wake => Some(Step::Wake),
            Start::Plan(p) => match self.begin_plan(p, &cancel).await {
                Some(input) => Some(Step::Prompt { input, synthetic: true }),
                None => {
                    aborted = true;
                    None
                }
            },
        };
        while let Some(step) = next.take() {
            if !self.begin_pass(step, &cancel).await {
                break;
            }
            match self.run_loop(&cancel).await {
                Flow::Aborted => {
                    aborted = true;
                    self.plan.exec_plan = None;
                    self.turn_end_hook().await;
                    break;
                }
                Flow::Failed => {
                    self.emit(AgentEvent::TurnFailed);
                    self.plan.exec_plan = None;
                    break;
                }
                Flow::Stopped => {
                    self.plan.exec_plan = None;
                    self.turn_end_hook().await;
                    break;
                }
                Flow::Done => {}
            }
            self.turn_end_hook().await;
            if let Some(p) = self.plan.exec_plan.take() {
                match self.begin_plan(p, &cancel).await {
                    Some(input) => {
                        next = Some(Step::Prompt { input, synthetic: true });
                        continue;
                    }
                    None => {
                        aborted = true;
                        break;
                    }
                }
            }
            let follow_up = self.ports.inbox.borrow_mut().follow_up.pop_front();
            if follow_up.is_some() {
                inbox::publish(&self.ports.inbox, &self.ports.events);
            }
            next = follow_up.map(|input| Step::Prompt { input, synthetic: false });
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

    /// Readies one pass: the session, the prompt or wake-up messages, and compaction. `false` ends the turn.
    async fn begin_pass(&mut self, step: Step, cancel: &CancellationToken) -> bool {
        if !self.ensure_session().await {
            return false;
        }
        let extra = match &step {
            Step::Prompt { input, synthetic } => {
                if !*synthetic {
                    self.emit(AgentEvent::UserPrompt(input.text.clone()));
                }
                self.user_turn += 1;
                self.compaction.failed_at = None;
                let prompt = secrets::redact(&input.text, &self.cwd).text;
                let res = self
                    .ports
                    .plugins
                    .hook(
                        "prompt_submit",
                        json!({"prompt": prompt, "userTurn": self.user_turn, "resumed": self.resumed}),
                    )
                    .await;
                self.apply_plugin_context(res);
                estimate_tokens(&input.text) + 1600 * input.images.len() as u64
            }
            Step::Wake => 0,
        };
        if self.needs_compaction(extra) {
            self.compact_reporting(cancel).await;
        }
        match step {
            Step::Prompt { input, synthetic } => {
                self.take_notify();
                if synthetic {
                    self.push(Message::synthetic_user(&input.text));
                } else {
                    self.push(Message::user_input(&input));
                }
            }
            Step::Wake => {
                let jobs = self.take_jobs();
                if !(self.take_notify() | jobs) {
                    return false;
                }
                self.emit(AgentEvent::Woken);
            }
        }
        true
    }

    /// Saves the plan a `propose_plan` result describes into the repository's store.
    async fn turn_end_hook(&mut self) {
        let msgs = std::mem::take(&mut self.turn_msgs);
        self.ports.plugins.hook("turn_end", json!({"userTurn": self.user_turn, "messages": msgs})).await;
    }

    /// The end-of-turn `ask` reminder may fire: `ask` is offered to this run, and the current user
    /// turn has neither called `ask` nor been reminded yet.
    fn ask_nudge_due(&self) -> bool {
        if !self.ports.providers.cfg.mu.ask.nudge || self.run.subagent || !self.policy().permits("ask") {
            return false;
        }
        for e in self.messages.entries().iter().rev() {
            match &e.msg {
                Message::User { .. } => return !e.reminder,
                m @ Message::Assistant { .. } if tool_calls(m).iter().any(|(_, name, _)| name == "ask") => {
                    return false;
                }
                _ => {}
            }
        }
        true
    }

    /// Runs requests until the pass ends: one per tool round, plus the retry after an overflow.
    async fn run_loop(&mut self, cancel: &CancellationToken) -> Flow {
        let mut st = LoopState::default();
        loop {
            if st.compaction == CompactionCheck::Due && self.compaction_due(0) {
                self.compact_reporting(cancel).await;
            }
            st.compaction = CompactionCheck::Due;
            // MCP servers without a cached tool list are started and listed once, before the first request.
            // A subagent restricted to named tools that include no MCP tool never needs the listing.
            if self.run.allow.as_ref().is_none_or(|a| a.iter().any(|n| n.starts_with("mcp__"))) {
                self.ports.plugins.list_missing_until(cancel).await;
            }
            if cancel.is_cancelled() {
                return Flow::Aborted;
            }
            let due = self.budget.as_mut().map(|b| b.step());
            let notice = match (due, self.budget.as_ref()) {
                (Some(d), Some(b)) => b.message(d),
                _ => None,
            };
            if let Some(m) = notice {
                self.push(Message::user(&m));
            }
            if due == Some(budget::Due::End) {
                return Flow::Done;
            }
            // Held until the reply ends: a reply that turns out to be a dismissal is never shown.
            let answering_reminder = self.messages.ends_with_reminder();
            let (reply, dismissed) = self.stream_reply(cancel, answering_reminder).await;
            let msg = match reply {
                Ok(m) => m,
                Err(ProviderError::Aborted) => return Flow::Aborted,
                Err(ProviderError::Overflow(m)) => {
                    if st.overflow == Overflow::Compacted {
                        self.error("context overflow after compaction");
                        return Flow::Failed;
                    }
                    st.overflow = Overflow::Compacted;
                    match self.compact(None, cancel).await {
                        Outcome::Done => {
                            st.compaction = CompactionCheck::Skip;
                            continue;
                        }
                        Outcome::Skipped(_) => {
                            self.error(format!("error: {m}"));
                            return Flow::Failed;
                        }
                        Outcome::Failed(_) if cancel.is_cancelled() => return Flow::Aborted,
                        Outcome::Failed(e) => {
                            self.report_compaction_failure(e, cancel);
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
            self.record_reply(msg, stop);
            if stop != StopReason::ToolUse && !calls.is_empty() {
                // A truncated reply: its tool calls never run, but each still needs a result.
                for (id, name, args) in &calls {
                    self.emit_skipped(id, name, args, SKIP_TRUNCATED);
                }
            }
            if stop != StopReason::ToolUse || calls.is_empty() {
                match self.settle_pass(stop, prose_question, &mut st.todo_nags) {
                    Some(flow) => return flow,
                    None => continue,
                }
            }
            let yielded = self.run_calls(calls, cancel).await;
            if cancel.is_cancelled() {
                return Flow::Aborted;
            }
            if self.ports.inbox.borrow().stop {
                return Flow::Stopped;
            }
            // A successful yield is the subagent's result; another request would only produce an unused reply.
            if yielded || self.plan.exec_plan.is_some() {
                return Flow::Done;
            }
            self.take_steer();
            self.take_jobs();
            self.take_notify();
        }
    }

    /// One streamed request over the current history. A reply that answers the ask reminder is held
    /// back while it streams; `dismissed` says it was a dismissal, so the held events are dropped.
    async fn stream_reply(
        &mut self,
        cancel: &CancellationToken,
        answering_reminder: bool,
    ) -> (Result<Message, ProviderError>, bool) {
        let system = self.system();
        let sid = self.session_id();
        let wire = self.wire_messages();
        // Every tool call in the history has its result and vice versa (`to_wire` also repairs).
        debug_assert!(mu_base::message::check_tool_pairs(&wire).is_ok(), "unpaired tool calls in the request");
        let defs = self.tool_defs();
        self.memo.sent_prefix = Some(self.prefix_key(&defs));
        let mut held = Vec::new();
        let mut failed = Vec::new();
        let reply = {
            let req = Request {
                model: &self.model.model,
                thinking: self.model.live_thinking.clone(),
                system: &system,
                tools: &defs,
                messages: &wire,
                max_tokens: None,
                session_id: Some(&sid),
            };
            let tx = self.ports.events.clone();
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
                    StreamEvent::Retry { attempt, max_attempts, delay, reason } => {
                        AgentEvent::Retry { attempt, max_attempts, delay, reason }
                    }
                    StreamEvent::Notice(m) => AgentEvent::Notice(m),
                    StreamEvent::FailedAttemptUsage(u) => {
                        failed.push(u);
                        return;
                    }
                };
                if answering_reminder {
                    held.push(e);
                } else {
                    let _ = tx.send(e);
                }
            };
            self.ports.providers.stream(&req, cancel, &mut ev).await
        };
        if !failed.is_empty() {
            for mut u in failed {
                self.costs.cost.record(&self.model.model.provider, &self.model.model.id, &mut u);
            }
            self.emit_status();
        }
        let dismissed = answering_reminder && matches!(&reply, Ok(m) if is_dismissal(m));
        if !dismissed {
            for e in held {
                self.emit(e);
            }
        }
        (reply, dismissed)
    }

    /// Stores an assistant reply and refreshes the status line. Context, cost and quota move with
    /// every response, not just at turn end.
    fn record_reply(&mut self, msg: Message, stop: StopReason) {
        self.push(msg);
        self.request_title();
        self.refresh_model_meta();
        self.tick_quota();
        self.settle_prices();
        self.emit_status();
        if stop == StopReason::Length {
            self.error("[output truncated: max_tokens reached]");
        }
    }

    /// Ends a pass that ran no tool calls. `None` continues the loop: steering, background results and
    /// `mu notify` messages were delivered, or a reminder was added. `Some` ends the run.
    fn settle_pass(&mut self, stop: StopReason, prose_question: bool, todo_nags: &mut u8) -> Option<Flow> {
        let steered = self.take_steer();
        let delivered = self.take_jobs() | self.take_notify();
        if steered || delivered {
            return None;
        }
        if self.ports.inbox.borrow().stop {
            return Some(Flow::Stopped);
        }
        // Stop reminder: the model is about to finish with open todo items. Nudge it to
        // update the list or finish the work, at most twice per run so it cannot loop. Not while a
        // background job runs: ending the turn is how the model waits for it, and its result wakes
        // the next turn.
        let waiting = self.tools.jobs.as_ref().is_some_and(|j| j.running() > 0);
        let open =
            if stop == StopReason::Stop && *todo_nags < 2 && !waiting { self.tools.todo_open() } else { Vec::new() };
        if !open.is_empty() {
            *todo_nags += 1;
            let list = open.iter().map(|c| format!("- {c}")).collect::<Vec<_>>().join("\n");
            self.push(Message::user(&format!(
                "<system-reminder>\nYou stopped with {} open todo item(s):\n{list}\nContinue the work on them, marking each `done` only when you have actually finished it. Do not tick items off that you did not do. `drop` an item that is no longer needed; `block` it (with a reason) if you are waiting on the user or something external. Do not reply to this reminder.\n</system-reminder>",
                open.len()
            )));
            return None;
        }
        // Ask reminder: the turn ends with a decision question in prose. Once per user turn.
        if prose_question && self.ask_nudge_due() {
            self.push_reminder();
            return None;
        }
        Some(Flow::Done)
    }

    /// Runs a reply's tool calls in order. Calls that only read and need no approval run together,
    /// up to `MAX_CONCURRENT_CALLS`; any other call waits for the ones before it and runs alone.
    /// Results land in call order. Once the run is cancelled, stopped, steered or a `yield`
    /// succeeded, the rest are skipped with a result. Returns whether a `yield` succeeded.
    async fn run_calls(&mut self, calls: Vec<(String, String, Value)>, cancel: &CancellationToken) -> bool {
        let mut pass = CallPass {
            skip: None,
            yielded: false,
            since: std::time::SystemTime::now(),
            last: calls.len().saturating_sub(1),
            cancel: cancel.clone(),
        };
        let mut batch: Vec<Pending> = Vec::new();
        for (i, (id, name, args)) in calls.into_iter().enumerate() {
            let runnable = pass.skip.is_none()
                && !cancel.is_cancelled()
                && !self.ports.inbox.borrow().stop
                && !self.budget.as_ref().is_some_and(|b| b.stopped());
            let together = runnable && self.tools.permissions.runs_concurrently(&self.tools, &name, &args);
            if !together {
                self.run_batch(&mut batch, cancel, &mut pass).await;
                self.run_alone(i, id, name, args, cancel, &mut pass).await;
                continue;
            }
            let (args, stop) = self.open_tool(&id, &name, args).await;
            if stop.is_some() {
                self.run_batch(&mut batch, cancel, &mut pass).await;
                let out = self.run_opened(&id, &name, args, stop, None, cancel).await;
                self.after_call(i, &id, &name, out, &mut pass);
                continue;
            }
            match self.verdict_for(&name, &args).await {
                Verdict::Allow => batch.push(Pending { i, id, name, args }),
                verdict => {
                    self.run_batch(&mut batch, cancel, &mut pass).await;
                    let out = self.run_opened(&id, &name, args, None, Some(verdict), cancel).await;
                    self.after_call(i, &id, &name, out, &mut pass);
                }
            }
        }
        self.run_batch(&mut batch, cancel, &mut pass).await;
        pass.yielded
    }

    /// One call on its own, or its skip result.
    async fn run_alone(
        &mut self,
        i: usize,
        id: String,
        name: String,
        args: Value,
        cancel: &CancellationToken,
        pass: &mut CallPass,
    ) {
        // Past 1.5x its budget the run only takes the result: `yield` still lands, nothing else.
        if name != "yield" && self.budget.as_ref().is_some_and(|b| b.stopped()) {
            self.emit_skipped(&id, &name, &args, SKIP_BUDGET);
            return;
        }
        if pass.skip.is_none() {
            if cancel.is_cancelled() {
                pass.skip = Some(SKIP_ABORTED);
            } else if self.ports.inbox.borrow().stop {
                pass.skip = Some(SKIP_STOPPED);
            }
        }
        if let Some(reason) = pass.skip {
            self.emit_skipped(&id, &name, &args, reason);
            return;
        }
        let out = self.run_tool(&id, &name, args, cancel).await;
        self.after_call(i, &id, &name, out, pass);
    }

    /// Runs the calls gathered so far at once and records their results in call order.
    async fn run_batch(&mut self, batch: &mut Vec<Pending>, cancel: &CancellationToken, pass: &mut CallPass) {
        if batch.is_empty() {
            return;
        }
        let items = std::mem::take(batch);
        // One interruptible wait for the group: only the latest registered wait hears the user.
        let guard = items.iter().find_map(|p| self.wait_guard(&p.name, &p.args));
        let wait = guard.as_ref().map(|g| g.token());
        let outs: Vec<ToolOutput> = {
            use futures_util::StreamExt;
            let this = &*self;
            futures_util::stream::iter(&items)
                .map(|p| this.exec_tool(&p.id, &p.name, &p.args, cancel, wait.clone()))
                .buffered(MAX_CONCURRENT_CALLS)
                .collect()
                .await
        };
        drop(guard);
        for (p, out) in items.into_iter().zip(outs) {
            let out = self.end_tool(&p.id, &p.name, p.args, None, out).await;
            self.after_call(p.i, &p.id, &p.name, out, pass);
        }
    }

    /// Records a finished call's result and decides whether the rest of the message is skipped.
    fn after_call(&mut self, i: usize, id: &str, name: &str, mut out: ToolOutput, pass: &mut CallPass) {
        if i == pass.last
            && name != "yield"
            && let Some(status) = self.job_status(pass.since)
        {
            out.text.push_str(&status);
        }
        self.push_result(id, name, &out);
        pass.yielded |= name == "yield" && !out.is_error;
        if pass.cancel.is_cancelled() {
            pass.skip = Some(SKIP_ABORTED);
        } else if self.ports.inbox.borrow().stop {
            pass.skip = Some(SKIP_STOPPED);
        } else if pass.yielded {
            pass.skip = Some(SKIP_YIELDED);
        } else if !self.ports.inbox.borrow().steer.is_empty() {
            pass.skip = Some(SKIP_STEER);
        }
    }
}

/// The most read-only calls of one message that run at the same time.
const MAX_CONCURRENT_CALLS: usize = 8;

/// How far a message's calls have got.
struct CallPass {
    skip: Option<&'static str>,
    yielded: bool,
    since: std::time::SystemTime,
    last: usize,
    cancel: CancellationToken,
}

/// A call that has started and passed the gate, waiting for its group to run.
struct Pending {
    i: usize,
    id: String,
    name: String,
    args: Value,
}
