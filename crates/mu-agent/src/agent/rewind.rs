//! `/rewind` and `/fork`: going back to before an earlier prompt or to just after an earlier reply,
//! and branching into a copy of the session.
use super::session::Reopen;
use super::*;
use mu_base::events::RewindPoint;
use mu_session::session::fork;

/// What the user typed in `m`, or `None` for a message mu sent itself (a plan, a reminder).
fn prompt_of(m: &Message) -> Option<UserInput> {
    let Message::User { content, synthetic: false, command, .. } = m else { return None };
    let text = text_of(content);
    if text.starts_with("<system-reminder>") {
        return None;
    }
    let images = content.iter().filter_map(|b| if let Block::Image(i) = b { Some(i.clone()) } else { None }).collect();
    Some(UserInput { text, images, command: command.clone() })
}

/// The text of an assistant message that ends a turn (no tool calls, no error), or `None`.
fn closing_reply(m: &Message) -> Option<String> {
    let Message::Assistant { content, error_message: None, .. } = m else { return None };
    if !tool_calls(m).is_empty() || is_dismissal(m) {
        return None;
    }
    let text = text_of(content);
    (!text.trim().is_empty()).then_some(text)
}

impl Agent {
    /// The prompts and closing replies of the loaded conversation, oldest first. Those a
    /// compaction summarized away are not loaded, so they are not offered. The last message is not
    /// offered as a reply: rewinding to it changes nothing.
    pub(super) fn list_rewind(&self) {
        let last = self.messages.len().saturating_sub(1);
        let points = self
            .messages
            .entries()
            .iter()
            .enumerate()
            .filter_map(|(i, e)| {
                let id = e.id.clone()?;
                match prompt_of(&e.msg) {
                    Some(p) => Some(RewindPoint { id, text: p.text, reply: false }),
                    None if i < last => closing_reply(&e.msg).map(|text| RewindPoint { id, text, reply: true }),
                    None => None,
                }
            })
            .collect();
        self.emit(AgentEvent::RewindPoints(points));
    }

    /// Rewinds to entry `id`: for a prompt, the session continues from just before it, the way a
    /// reload would find it, and the prompt returns to the composer; for a reply, from just after.
    pub(super) async fn rewind(&mut self, id: &str) {
        let Some(m) = self.messages.position_of_id(id).map(|i| &self.messages[i]) else {
            return self.error("that point is not in the conversation any more");
        };
        let draft = prompt_of(m);
        let reply = closing_reply(m);
        if draft.is_none() && reply.is_none() {
            return self.error("that point is not in the conversation any more");
        }
        let Some(session) = self.session.as_mut() else { return self.error("no session yet") };
        let path = session.path.clone();
        let rewound = if draft.is_some() { session.rewind_before(id) } else { session.rewind_after(id) };
        if let Err(e) = rewound {
            return self.error(e.to_string());
        }
        if !self.reopen(&path, Reopen::Rewind).await {
            return;
        }
        if let Some(draft) = draft {
            let first = draft.text.lines().next().unwrap_or_default();
            self.notice(format!("── rewound to before: {}", mu_base::text::truncate_text(first, 80)));
            self.emit(AgentEvent::QueuePopped(draft));
        } else if let Some(reply) = reply {
            let first = reply.lines().find(|l| !l.trim().is_empty()).unwrap_or_default().trim();
            self.notice(format!("── rewound to after: {}", mu_base::text::truncate_text(first, 80)));
        }
    }

    /// Continues in a copy of this session; the original stays as it is, to `/resume` later.
    pub(super) async fn fork(&mut self) {
        let Some(from) = self.session.as_ref().map(|s| s.path.clone()).filter(|_| !self.messages.is_empty()) else {
            return self.error("nothing to fork yet");
        };
        match fork::fork(&from, &self.cwd) {
            Ok(to) => {
                if self.reopen(&to, Reopen::Fork).await {
                    self.notice(format!("the original stays at {}", from.display()));
                }
            }
            Err(e) => self.error(format!("fork failed: {e:#}")),
        }
    }
}
