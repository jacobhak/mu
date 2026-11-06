//! `/rewind` and `/fork`: going back to an earlier prompt, and branching into a copy of the session.
use super::session::Reopen;
use super::*;
use mu_base::events::RewindPoint;
use mu_session::session::fork;

/// What the user typed in `m`, or `None` for a message mu sent itself (a plan, a reminder).
fn prompt_of(m: &Message) -> Option<UserInput> {
    let Message::User { content, synthetic: false, .. } = m else { return None };
    let text = text_of(content);
    if text.starts_with("<system-reminder>") {
        return None;
    }
    let images = content.iter().filter_map(|b| if let Block::Image(i) = b { Some(i.clone()) } else { None }).collect();
    Some(UserInput { text, images })
}

impl Agent {
    /// The prompts of the loaded conversation, oldest first. Those a compaction summarized away
    /// are not loaded, so they are not offered.
    pub(super) fn list_rewind(&self) {
        let points = self
            .messages
            .iter()
            .zip(&self.entry_ids)
            .filter(|(_, id)| !id.is_empty())
            .filter_map(|(m, id)| prompt_of(m).map(|p| RewindPoint { id: id.clone(), text: p.text }))
            .collect();
        self.emit(AgentEvent::RewindPoints(points));
    }

    /// Takes back the prompt in entry `id`: the session continues from just before it, the way a
    /// reload would find it, and the prompt returns to the composer.
    pub(super) async fn rewind(&mut self, id: &str) {
        let Some(draft) = self.entry_ids.iter().position(|e| e == id).and_then(|i| prompt_of(&self.messages[i])) else {
            return self.error("that prompt is not in the conversation any more");
        };
        let Some(session) = self.session.as_mut() else { return self.error("no session yet") };
        let path = session.path.clone();
        if let Err(e) = session.rewind_before(id) {
            return self.error(e.to_string());
        }
        if !self.reopen(&path, Reopen::Rewind).await {
            return;
        }
        let first = draft.text.lines().next().unwrap_or_default();
        self.notice(format!("── rewound to before: {}", mu_base::text::truncate_text(first, 80)));
        self.emit(AgentEvent::QueuePopped(draft));
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
