use super::*;
use mu_session::inbox;

impl Agent {
    /// Takes queued steering text and pushes it as user messages. Returns whether any was pending.
    pub(super) fn take_steer(&mut self) -> bool {
        let steer: Vec<UserInput> = self.inbox.borrow_mut().steer.drain(..).collect();
        for s in &steer {
            self.emit(AgentEvent::UserPrompt(s.text.clone()));
            self.push(Message::user_input(s));
        }
        !steer.is_empty()
    }

    /// Hands finished background jobs to the model as one `<system-notice>` user message.
    /// Returns whether any were pending.
    pub(super) fn take_jobs(&mut self) -> bool {
        let Some(jobs) = &self.tools.jobs else { return false };
        let done = jobs.take_undelivered(&[]);
        if done.is_empty() {
            return false;
        }
        self.emit(AgentEvent::JobsDelivered(done.iter().map(|d| d.id.clone()).collect()));
        self.push(Message::user(&jobs::notice(&done)));
        true
    }

    /// Is there a `mu notify` message to deliver? `wake`: for a turn nobody asked for, so
    /// messages announced at open (which wait for the next prompt) do not count.
    pub(super) fn notify_pending(&self, wake: bool) -> bool {
        self.notify
            && self.session.is_some()
            && inbox::has_pending(&self.session_id())
            && inbox::pending(&self.session_id())
                .iter()
                .any(|n| !self.notify_seen.contains(&n.name) && !(wake && self.notify_announced.contains(&n.name)))
    }

    /// Hands queued `mu notify` messages to the model as one `<system-notice>` user message and
    /// deletes the files, but only once that message is in the session file.
    pub(super) fn take_notify(&mut self) -> bool {
        if !self.notify || self.session.is_none() || !inbox::has_pending(&self.session_id()) {
            return false;
        }
        let id = self.session_id();
        let notes: Vec<inbox::Note> =
            inbox::pending(&id).into_iter().filter(|n| !self.notify_seen.contains(&n.name)).collect();
        if notes.is_empty() {
            return false;
        }
        for n in &notes {
            let line: String = n.text.lines().next().unwrap_or("").chars().take(80).collect();
            self.notice(format!("↳ mu notify from {}: {line}", n.from));
        }
        self.push(Message::user(&inbox::notice(&notes)));
        let names: Vec<String> = notes.into_iter().map(|n| n.name).collect();
        if self.entry_ids.last().is_some_and(|e| !e.is_empty()) {
            if let Some(s) = &self.session {
                s.sync();
            }
            inbox::remove(&id, &names);
        } else {
            // Not written: keep the files for the next open, but never deliver them twice here.
            self.notify_seen.extend(names);
        }
        true
    }
}
