use super::*;
use mu_session::session::inbox;

impl Agent {
    /// Takes queued steering text and pushes it as user messages. Returns whether any was pending.
    pub(super) fn take_steer(&mut self) -> bool {
        let steer: Vec<UserInput> = self.ports.inbox.borrow_mut().steer.drain(..).collect();
        if !steer.is_empty() {
            publish(&self.ports.inbox, &self.ports.events);
        }
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
        let done = jobs.take_undelivered();
        let matches = jobs.take_matches();
        if !matches.is_empty() {
            for m in &matches {
                let line: String = m.lines.first().map(|l| l.chars().take(80).collect()).unwrap_or_default();
                self.notice(format!("↳ {} output matched: {line}", m.id));
            }
            self.push(Message::user(&jobs::matches_notice(&matches)));
        }
        if done.is_empty() {
            return !matches.is_empty();
        }
        self.emit(AgentEvent::JobsDelivered(done.iter().map(|d| d.id.clone()).collect()));
        self.push(Message::user(&jobs::notice(&done)));
        true
    }

    /// Is there a `mu notify` message to deliver? `wake`: for a turn nobody asked for, so
    /// messages announced at open (which wait for the next prompt) do not count.
    pub(super) fn notify_pending(&self, wake: bool) -> bool {
        self.mail.notify
            && self.session.is_some()
            && inbox::has_pending(&self.session_id())
            && inbox::pending(&self.session_id())
                .iter()
                .any(|n| !self.mail.seen.contains(&n.name) && !(wake && self.mail.announced.contains(&n.name)))
    }

    /// Hands queued `mu notify` messages to the model as one `<system-notice>` user message and
    /// deletes the files, but only once that message is in the session file.
    pub(super) fn take_notify(&mut self) -> bool {
        if !self.mail.notify || self.session.is_none() || !inbox::has_pending(&self.session_id()) {
            return false;
        }
        let id = self.session_id();
        let notes: Vec<inbox::Note> =
            inbox::pending(&id).into_iter().filter(|n| !self.mail.seen.contains(&n.name)).collect();
        if notes.is_empty() {
            return false;
        }
        for n in &notes {
            let line: String = n.text.lines().next().unwrap_or("").chars().take(80).collect();
            self.notice(format!("↳ mu notify from {}: {line}", n.from));
        }
        self.push(Message::user(&inbox::notice(&notes)));
        let names: Vec<String> = notes.into_iter().map(|n| n.name).collect();
        if self.messages.last_id().is_some() {
            if let Some(s) = &self.session {
                s.sync();
            }
            inbox::remove(&id, &names);
        } else {
            // Not written: keep the files for the next open, but never deliver them twice here.
            self.mail.seen.extend(names);
        }
        true
    }
}

/// Drains steer and follow-up, steer first.
fn take_queue(inbox: &SharedInbox) -> Vec<UserInput> {
    let mut ib = inbox.borrow_mut();
    let mut q: Vec<UserInput> = ib.steer.drain(..).collect();
    q.extend(ib.follow_up.drain(..));
    q
}

/// Appends `input` to the steer queue: after earlier steering, and after queued follow-ups, which
/// become steering too.
pub(super) fn steer_input(input: UserInput, inbox: &SharedInbox, events: &EventTx) {
    {
        let mut ib = inbox.borrow_mut();
        let follow: Vec<UserInput> = ib.follow_up.drain(..).collect();
        ib.steer.extend(follow);
        ib.steer.push_back(input);
    }
    publish(inbox, events);
}

/// Consumes a submitted prompt as steering while an interruptible wait is active. `false` for any
/// other command, which keeps its usual handling.
pub(super) fn steer_now(c: Command, inbox: &SharedInbox, events: &EventTx) -> bool {
    let input = match c {
        Command::Prompt(i) | Command::Steer(i) | Command::FollowUp(i) => i,
        _ => return false,
    };
    steer_input(input, inbox, events);
    true
}

/// Moves already-queued steering and follow-ups into steering, so a newly registered wait is
/// released by input that arrived before it. Returns whether there was any.
pub(super) fn promote_queued(inbox: &SharedInbox, events: &EventTx) -> bool {
    {
        let mut ib = inbox.borrow_mut();
        if ib.steer.is_empty() && ib.follow_up.is_empty() {
            return false;
        }
        let follow: Vec<UserInput> = ib.follow_up.drain(..).collect();
        ib.steer.extend(follow);
    }
    publish(inbox, events);
    true
}

/// Sends the queue's current contents to the front end.
pub(super) fn publish(inbox: &SharedInbox, events: &EventTx) {
    let ib = inbox.borrow();
    let state =
        QueueState { steer: ib.steer.iter().cloned().collect(), follow_up: ib.follow_up.iter().cloned().collect() };
    let _ = events.send(AgentEvent::Queue(state));
}

/// Queue and stop commands act on the inbox at once, mid-turn or between turns. Any other command is handed back.
pub(super) fn inbox_cmd(c: Command, inbox: &SharedInbox, events: &EventTx) -> Result<(), Command> {
    match c {
        Command::Steer(i) => inbox.borrow_mut().steer.push_back(i),
        Command::FollowUp(i) => inbox.borrow_mut().follow_up.push_back(i),
        Command::PopQueue => {
            let popped = {
                let mut ib = inbox.borrow_mut();
                ib.steer.pop_back().or_else(|| ib.follow_up.pop_back())
            };
            if let Some(p) = popped {
                let _ = events.send(AgentEvent::QueuePopped(p));
            }
        }
        Command::Cancel => {
            inbox.borrow().cancel.cancel();
            return Ok(());
        }
        Command::Stop => {
            inbox.borrow_mut().stop = true;
            return Ok(());
        }
        other => return Err(other),
    }
    publish(inbox, events);
    Ok(())
}

/// Hands what a finished turn did not take to the front end, after its `TurnEnd`; the front end
/// restores it to the composer.
pub(super) fn report_dropped(inbox: &SharedInbox, events: &EventTx) {
    let queued = take_queue(inbox);
    if !queued.is_empty() {
        publish(inbox, events);
        let _ = events.send(AgentEvent::QueuedDropped(queued));
    }
}
