use mu_base::ask_nudge;
use mu_base::message::{Message, text_of};
use std::ops::Index;

/// One message of the conversation with the session entry that stores it.
pub(crate) struct Entry {
    pub msg: Message,
    /// `None` when the message was never written to a session file.
    pub id: Option<String>,
    /// The end-of-turn `ask` reminder, which is not a user turn.
    pub reminder: bool,
}

/// The conversation as sent to the provider, each message paired with its session entry.
#[derive(Default)]
pub(crate) struct History(Vec<Entry>);

impl History {
    /// A stored conversation. The reminder is stored as an ordinary user message, so it is
    /// recognised by its text here and by nothing afterwards.
    pub fn load(messages: Vec<Message>, ids: Vec<String>) -> Self {
        let mut ids = ids.into_iter();
        Self(
            messages
                .into_iter()
                .map(|msg| {
                    let reminder =
                        matches!(&msg, Message::User { content, .. } if ask_nudge::is_reminder(&text_of(content)));
                    Entry { msg, id: ids.next().filter(|id| !id.is_empty()), reminder }
                })
                .collect(),
        )
    }

    pub fn push(&mut self, msg: Message, id: Option<String>, reminder: bool) {
        self.0.push(Entry { msg, id, reminder });
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn clear(&mut self) {
        self.0.clear();
    }

    /// Drops the first `n` entries.
    pub fn drain_front(&mut self, n: usize) {
        self.0.drain(..n);
    }

    pub fn entries(&self) -> &[Entry] {
        &self.0
    }

    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &Message> + ExactSizeIterator {
        self.0.iter().map(|e| &e.msg)
    }

    pub fn last(&self) -> Option<&Message> {
        self.0.last().map(|e| &e.msg)
    }

    /// The newest message is the `ask` reminder.
    pub fn ends_with_reminder(&self) -> bool {
        self.0.last().is_some_and(|e| e.reminder)
    }

    pub fn last_id(&self) -> Option<&str> {
        self.0.last().and_then(|e| e.id.as_deref())
    }

    pub fn id(&self, i: usize) -> Option<&str> {
        self.0.get(i).and_then(|e| e.id.as_deref())
    }

    pub fn position_of_id(&self, id: &str) -> Option<usize> {
        self.0.iter().position(|e| e.id.as_deref() == Some(id))
    }

    /// The messages alone, for code that wants a slice.
    pub fn to_vec(&self) -> Vec<Message> {
        self.iter().cloned().collect()
    }
}

impl Index<usize> for History {
    type Output = Message;

    fn index(&self, i: usize) -> &Message {
        &self.0[i].msg
    }
}
