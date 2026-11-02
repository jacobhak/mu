//! Follows a session file that another process is appending to: the jobs panel's view of a running
//! subagent. The file is append-only JSONL, so a reader keeps a byte offset and parses only whole
//! lines; a half-written last line waits for the next poll. It takes no lock (the owner holds the
//! exclusive one) and never writes.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;

use serde::Deserialize;
use serde_json::Value;

use crate::message::Message;

/// Most bytes read per poll, so a huge session fills in over several ticks instead of stalling one.
const MAX_READ: u64 = 1 << 20;

pub struct Tail {
    path: PathBuf,
    offset: u64,
    /// Messages to keep (the newest); 0 keeps none and only totals.
    keep: usize,
    messages: Vec<Message>,
    /// Messages parsed but no longer kept.
    dropped: usize,
    cost: f64,
    tokens: u64,
    /// The last poll reached the end of the file.
    caught_up: bool,
}

impl Tail {
    pub fn new(path: impl Into<PathBuf>, keep: usize) -> Self {
        Self {
            path: path.into(),
            offset: 0,
            keep,
            messages: Vec::new(),
            dropped: 0,
            cost: 0.0,
            tokens: 0,
            caught_up: false,
        }
    }

    /// The newest kept messages, oldest first.
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// Messages that came before the kept ones.
    pub fn dropped(&self) -> usize {
        self.dropped
    }

    /// Dollars spent by every assistant message read so far.
    pub fn cost(&self) -> f64 {
        self.cost
    }

    /// Input plus output tokens of every assistant message read so far.
    pub fn tokens(&self) -> u64 {
        self.tokens
    }

    /// False until the first poll has read to the end of the file: the totals are partial before.
    pub fn caught_up(&self) -> bool {
        self.caught_up
    }

    /// Reads what was appended since the last poll. `true` when the messages or totals changed. A file
    /// that shrank (replaced by another) starts over; one that cannot be opened yet changes nothing.
    pub fn poll(&mut self) -> bool {
        let Ok(mut f) = File::open(&self.path) else { return false };
        let Ok(len) = f.metadata().map(|m| m.len()) else { return false };
        let mut changed = false;
        if len < self.offset {
            *self = Self::new(std::mem::take(&mut self.path), self.keep);
            changed = true;
        }
        if f.seek(SeekFrom::Start(self.offset)).is_err() {
            return changed;
        }
        let mut buf = Vec::new();
        if f.take((len - self.offset).min(MAX_READ)).read_to_end(&mut buf).is_err() {
            return changed;
        }
        let Some(end) = buf.iter().rposition(|&b| b == b'\n') else {
            // No whole line yet; a line longer than the read limit would never complete otherwise.
            if buf.len() as u64 >= MAX_READ {
                self.offset += buf.len() as u64;
            }
            self.caught_up = self.offset >= len;
            return changed;
        };
        for line in buf[..end].split(|&b| b == b'\n') {
            changed |= self.take_line(line);
        }
        self.offset += end as u64 + 1;
        self.caught_up = self.offset >= len;
        changed
    }

    fn take_line(&mut self, line: &[u8]) -> bool {
        let Ok(v) = serde_json::from_slice::<Value>(line) else { return false };
        if v["type"] != "message" {
            return false;
        }
        let m = &v["message"];
        if m["role"] == "assistant" {
            let u = &m["usage"];
            self.cost += u["cost"]["total"].as_f64().unwrap_or(0.0);
            self.tokens +=
                ["input", "cacheRead", "cacheWrite", "output"].iter().map(|k| u[*k].as_u64().unwrap_or(0)).sum::<u64>();
        }
        if self.keep > 0
            && let Ok(msg) = Message::deserialize(m)
        {
            self.messages.push(msg);
            if self.messages.len() > self.keep {
                // Drop in blocks so a long run does not shift the vector on every message.
                let cut = (self.messages.len() - self.keep).max(self.keep / 4);
                let cut = cut.min(self.messages.len());
                self.messages.drain(..cut);
                self.dropped += cut;
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;
    use std::path::Path;

    fn file() -> PathBuf {
        let d = std::env::temp_dir().join(format!("mu-tail-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d.join("s.jsonl")
    }

    fn user(text: &str) -> String {
        format!(
            "{}\n",
            json!({"type":"message","id":"1","parentId":null,"message":{"role":"user","content":text,"timestamp":1}})
        )
    }

    fn assistant(text: &str, cost: f64, tokens: (u64, u64)) -> String {
        let m = json!({"role":"assistant","content":[{"type":"text","text":text}],"api":"a","provider":"p","model":"m",
            "usage":{"input":tokens.0,"output":tokens.1,"cacheRead":0,"cacheWrite":0,"totalTokens":0,"cost":{"total":cost}},
            "stopReason":"stop","timestamp":2});
        format!("{}\n", json!({"type":"message","id":"2","parentId":"1","message":m}))
    }

    fn append(p: &Path, s: &str) {
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(p).unwrap();
        f.write_all(s.as_bytes()).unwrap();
    }

    #[test]
    fn a_missing_file_changes_nothing_until_it_appears() {
        let p = file();
        let mut t = Tail::new(&p, 10);
        assert!(!t.poll());
        assert!(!t.caught_up());
        append(&p, &user("hi"));
        assert!(t.poll());
        assert_eq!(t.messages().len(), 1);
        assert!(t.caught_up());
    }

    #[test]
    fn only_whole_lines_are_read_and_nothing_is_counted_twice() {
        let p = file();
        let mut t = Tail::new(&p, 10);
        let line = assistant("hello", 0.25, (100, 20));
        let (a, b) = line.split_at(line.len() / 2);
        append(&p, &user("hi"));
        append(&p, a);
        assert!(t.poll());
        assert_eq!(t.messages().len(), 1, "a half line waits");
        assert!(!t.poll(), "and polling again changes nothing");
        append(&p, b);
        assert!(t.poll());
        assert_eq!(t.messages().len(), 2);
        assert_eq!((t.cost(), t.tokens()), (0.25, 120));
        assert!(!t.poll());
        assert_eq!((t.cost(), t.tokens()), (0.25, 120));
    }

    #[test]
    fn totals_equal_a_whole_file_read_and_other_entries_are_skipped() {
        let p = file();
        append(&p, "{\"type\":\"session\",\"version\":3,\"id\":\"a\",\"timestamp\":\"t\",\"cwd\":\"/\"}\n");
        append(&p, "not json\n");
        append(&p, &user("hi"));
        append(&p, &assistant("one", 0.5, (10, 1)));
        append(&p, "{\"type\":\"compaction\",\"summary\":\"s\"}\n");
        append(&p, &assistant("two", 0.25, (20, 2)));
        let mut once = Tail::new(&p, 10);
        once.poll();
        let mut often = Tail::new(&p, 10);
        for _ in 0..3 {
            often.poll();
        }
        assert_eq!((once.cost(), once.tokens(), once.messages().len()), (0.75, 33, 3));
        assert_eq!((often.cost(), often.tokens(), often.messages().len()), (0.75, 33, 3));
    }

    #[test]
    fn the_oldest_messages_drop_past_the_limit_but_totals_keep_counting() {
        let p = file();
        for i in 0..40 {
            append(&p, &assistant(&format!("m{i}"), 0.01, (1, 1)));
        }
        let mut t = Tail::new(&p, 8);
        t.poll();
        assert!(t.messages().len() <= 8 && !t.messages().is_empty());
        assert_eq!(t.messages().len() + t.dropped(), 40);
        assert!(matches!(t.messages().last(), Some(Message::Assistant { content, .. })
            if matches!(&content[0], crate::message::Block::Text { text } if text == "m39")));
        assert!((t.cost() - 0.40).abs() < 1e-9);
        assert_eq!(t.tokens(), 80);
        let mut totals_only = Tail::new(&p, 0);
        totals_only.poll();
        assert!(totals_only.messages().is_empty());
        assert_eq!(totals_only.tokens(), 80);
    }

    #[test]
    fn a_replaced_shorter_file_starts_over() {
        let p = file();
        append(&p, &assistant("one", 1.0, (1, 1)));
        append(&p, &assistant("two", 1.0, (1, 1)));
        let mut t = Tail::new(&p, 10);
        t.poll();
        assert_eq!(t.cost(), 2.0);
        std::fs::write(&p, user("new")).unwrap();
        assert!(t.poll());
        assert_eq!((t.messages().len(), t.cost()), (1, 0.0));
    }

    #[test]
    fn a_big_file_is_read_in_bounded_steps() {
        let p = file();
        let line = assistant(&"x".repeat(1000), 0.001, (1, 1));
        let n = (MAX_READ as usize / line.len()) * 2 + 10;
        for _ in 0..n {
            append(&p, &line);
        }
        let mut t = Tail::new(&p, 0);
        assert!(t.poll());
        assert!(!t.caught_up(), "one poll reads at most {MAX_READ} bytes");
        let mut polls = 1;
        while !t.caught_up() {
            t.poll();
            polls += 1;
            assert!(polls < 10);
        }
        assert_eq!(t.tokens(), 2 * n as u64);
    }
}
