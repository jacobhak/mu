//! The streamed conversation as the terminal shows it: committed lines waiting to be printed, the
//! open (uncommitted) text and thinking, and the smooth-reveal queue that paces streamed text.

use std::collections::VecDeque;
use std::time::Instant;

use crate::chat::blocks;
use crate::text::line::Line;
use crate::text::markdown;

const REVEAL_MIN_STEP: usize = 3;
const REVEAL_CATCHUP_FRAMES: usize = 8;
/// Assistant text blocks kept for `/copy`.
const REPLIES_KEPT: usize = 50;

#[derive(Clone, Copy, PartialEq)]
pub enum Stream {
    Text,
    Thinking,
}

fn reveal_step(backlog: usize) -> usize {
    backlog.div_ceil(REVEAL_CATCHUP_FRAMES).max(REVEAL_MIN_STEP)
}

/// `((width, live generation), rendered lines)`.
type LiveCache = ((usize, u64), Vec<Line>);

pub struct Transcript {
    /// Lines committed since the last draw; the renderer prints them into scrollback.
    pub pending: Vec<Line>,
    live_text: String,
    committed_in_block: bool,
    /// Uncommitted thinking text (stable chunks are committed while thinking is shown).
    thinking: Option<(Instant, String)>,
    /// A chunk of the current thinking block is committed: later chunks start with a gutter row, and
    /// the block ends with one blank row.
    thinking_shown: bool,
    block_text: String,
    /// Reference definitions (`[x]: url`) from text already committed in this block, prepended
    /// when later chunks are rendered so their `[x]` links still resolve.
    md_refs: String,
    /// Bumped whenever `live_text` or `md_refs` changes, so the cache cannot match stale text.
    live_gen: u64,
    /// Rendered lines of the open (uncommitted) text.
    live_cache: std::cell::RefCell<Option<LiveCache>>,
    /// Assistant text blocks (newest last, capped), for `/copy`.
    pub replies: Vec<String>,
    show_thinking: bool,
    smooth: bool,
    /// Streamed text and thinking not yet shown, oldest first.
    reveal: VecDeque<(Stream, String)>,
    /// Total `char` count of `reveal`.
    reveal_chars: usize,
    /// Something changed that needs a redraw; the app clears it.
    pub dirty: bool,
}

impl Transcript {
    pub fn new(show_thinking: bool, smooth: bool) -> Self {
        Self {
            pending: Vec::new(),
            live_text: String::new(),
            committed_in_block: false,
            thinking: None,
            thinking_shown: false,
            block_text: String::new(),
            md_refs: String::new(),
            live_gen: 0,
            live_cache: Default::default(),
            replies: Vec::new(),
            show_thinking,
            smooth,
            reveal: VecDeque::new(),
            reveal_chars: 0,
            dirty: false,
        }
    }

    pub fn commit(&mut self, lines: Vec<Line>) {
        self.pending.extend(lines);
        self.dirty = true;
    }

    /// Streamed text still revealing: the frame loop keeps drawing until it is shown.
    pub fn revealing(&self) -> bool {
        self.reveal_chars > 0
    }

    pub fn thinking_active(&self) -> bool {
        self.thinking.is_some()
    }

    /// The gutter row (after earlier chunks of the block) and the body for one thinking chunk.
    fn thinking_rows(&self, text: &str, w: usize) -> Vec<Line> {
        let mut l = Vec::new();
        if self.thinking_shown {
            l.push(blocks::thinking_gap());
        }
        l.extend(blocks::thinking_body(text.trim(), w));
        l
    }

    pub fn finish_thinking(&mut self, w: usize) {
        let Some((start, text)) = self.thinking.take() else { return };
        if self.show_thinking {
            let blank = text.trim().is_empty();
            let mut l = if blank { Vec::new() } else { self.thinking_rows(&text, w) };
            if self.thinking_shown || !blank {
                l.push(vec![]);
            }
            if !l.is_empty() {
                self.commit(l);
            }
        } else {
            self.commit(vec![blocks::thinking_done(start.elapsed().as_secs())]);
        }
        self.thinking_shown = false;
    }

    pub fn commit_text(&mut self, text: &str, blank_after: bool, w: usize) {
        let mut lines = markdown::render_linked(&format!("{}{text}", self.md_refs), w);
        self.md_refs.push_str(&markdown::reference_defs(text));
        self.live_gen += 1;
        if blank_after {
            lines.push(vec![]);
        }
        self.committed_in_block = true;
        self.commit(lines);
    }

    fn ingest_text(&mut self, s: &str, w: usize) {
        self.finish_thinking(w);
        self.live_text.push_str(s);
        self.live_gen += 1;
        self.block_text.push_str(s);
        // A block can only become complete at a line end; skip the parse for mid-line deltas.
        if !s.contains('\n') {
            return;
        }
        let n = markdown::stable_prefix(&self.live_text);
        if n > 0 {
            let chunk: String = self.live_text.drain(..n).collect();
            self.commit_text(chunk.trim_end(), true, w);
        }
    }

    fn ingest_thinking(&mut self, s: &str, w: usize) {
        match self.thinking.as_mut() {
            Some((_, t)) => t.push_str(s),
            None => self.thinking = Some((Instant::now(), s.to_string())),
        }
        // Like `ingest_text`, parse only at line ends; `finish_thinking` commits the rest.
        if self.show_thinking && s.contains('\n') {
            self.commit_stable_thinking(w);
        }
    }

    fn commit_stable_thinking(&mut self, w: usize) {
        let Some((_, text)) = self.thinking.as_mut() else { return };
        let n = markdown::stable_prefix(text);
        if n == 0 {
            return;
        }
        let chunk: String = text.drain(..n).collect();
        if !chunk.trim().is_empty() {
            let l = self.thinking_rows(&chunk, w);
            self.thinking_shown = true;
            self.commit(l);
        }
    }

    fn ingest(&mut self, k: Stream, s: &str, w: usize) {
        match k {
            Stream::Text => self.ingest_text(s, w),
            Stream::Thinking => self.ingest_thinking(s, w),
        }
    }

    pub fn queue_reveal(&mut self, k: Stream, s: String, w: usize) {
        if !self.smooth {
            self.ingest(k, &s, w);
            return;
        }
        self.reveal_chars += s.chars().count();
        match self.reveal.back_mut() {
            Some((bk, t)) if *bk == k => t.push_str(&s),
            _ => self.reveal.push_back((k, s)),
        }
    }

    pub fn flush_reveal(&mut self, w: usize) {
        while let Some((k, s)) = self.reveal.pop_front() {
            self.ingest(k, &s, w);
        }
        self.reveal_chars = 0;
    }

    pub fn advance_reveal(&mut self, w: usize) {
        if self.reveal_chars == 0 {
            return;
        }
        let mut n = reveal_step(self.reveal_chars);
        while n > 0 {
            let Some((k, t)) = self.reveal.front_mut() else { break };
            let k = *k;
            let end = t.char_indices().nth(n).map_or(t.len(), |(i, _)| i);
            let piece: String = t.drain(..end).collect();
            if t.is_empty() {
                self.reveal.pop_front();
            }
            let c = piece.chars().count();
            n -= c;
            self.reveal_chars -= c;
            self.ingest(k, &piece, w);
        }
    }

    /// The end of an assistant text block: commit what is left and remember the block for `/copy`.
    pub fn end_block(&mut self, w: usize) {
        self.finish_thinking(w);
        if !self.live_text.trim().is_empty() {
            let t = std::mem::take(&mut self.live_text);
            self.commit_text(t.trim_end(), false, w);
        }
        self.live_text.clear();
        self.md_refs.clear();
        self.live_gen += 1;
        let block = std::mem::take(&mut self.block_text);
        if !block.trim().is_empty() {
            if self.replies.len() == REPLIES_KEPT {
                self.replies.remove(0);
            }
            self.replies.push(block);
        }
        if self.committed_in_block {
            self.commit(vec![vec![]]);
        }
        self.committed_in_block = false;
    }

    /// A turn ended: finish thinking and commit any text still open.
    pub fn end_turn(&mut self, w: usize) {
        self.finish_thinking(w);
        if !self.live_text.trim().is_empty() {
            let t = std::mem::take(&mut self.live_text);
            self.commit_text(t.trim_end(), false, w);
        }
        self.live_text.clear();
        self.live_gen += 1;
    }

    /// A request is being retried: drop the live text, thinking and reveal state of this attempt. Lines
    /// already committed to the scrollback stay (review B8).
    pub fn reset_stream(&mut self) {
        self.live_text.clear();
        self.live_gen += 1;
        self.block_text.clear();
        self.thinking = None;
        self.thinking_shown = false;
        self.reveal.clear();
        self.reveal_chars = 0;
    }

    /// Ctrl+T: show or hide thinking. Turning it on commits what is already stable.
    pub fn toggle_thinking(&mut self, w: usize) {
        self.show_thinking = !self.show_thinking;
        if self.show_thinking {
            self.commit_stable_thinking(w);
        }
    }

    /// Viewport rows for the open text and thinking. `spin` is the spinner glyph.
    pub fn live_lines(&self, w: usize, spin: char) -> Vec<Line> {
        let mut out: Vec<Line> = Vec::new();
        if !self.live_text.is_empty() {
            // The open block is rendered (and its fence highlighted) once per change, not per frame.
            let mut cache = self.live_cache.borrow_mut();
            let key = (w, self.live_gen);
            if cache.as_ref().is_none_or(|(k, _)| *k != key) {
                let lines = markdown::render_linked(&format!("{}{}", self.md_refs, self.live_text), w);
                *cache = Some((key, lines));
            }
            if let Some((_, lines)) = cache.as_ref() {
                out.extend(lines.iter().cloned());
            }
        }
        if let Some((start, text)) = &self.thinking {
            if self.show_thinking && !text.trim().is_empty() {
                out.extend(self.thinking_rows(text, w));
            }
            out.push(blocks::thinking_live(spin, start.elapsed().as_secs()));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps() {
        assert_eq!(reveal_step(0), 3);
        assert_eq!(reveal_step(8), 3);
        assert_eq!(reveal_step(100), 13);
        assert_eq!(reveal_step(800), 100);
    }

    #[test]
    fn text_commits_at_complete_blocks_and_ends_cleanly() {
        let mut t = Transcript::new(true, false);
        t.queue_reveal(Stream::Text, "first\n\nsecond".into(), 40);
        assert!(!t.pending.is_empty(), "the first paragraph is complete and committed");
        assert!(!t.live_lines(40, '⠋').is_empty(), "the open one is live");
        t.end_block(40);
        assert_eq!(t.replies, ["first\n\nsecond"]);
        assert!(t.live_lines(40, '⠋').is_empty());
    }

    #[test]
    fn a_retry_drops_the_failed_attempts_text_from_copy_and_the_live_rows() {
        let mut t = Transcript::new(true, false);
        t.queue_reveal(Stream::Text, "first try".into(), 40);
        let shown = |t: &Transcript| t.live_lines(40, '⠋').iter().map(crate::text::line::line_text).collect::<String>();
        assert!(shown(&t).contains("first try"));
        t.reset_stream();
        // Same length as the dropped text: a cache keyed by length would still show the old rows.
        t.queue_reveal(Stream::Text, "retry now".into(), 40);
        assert!(shown(&t).contains("retry now") && !shown(&t).contains("first try"), "{}", shown(&t));
        t.end_block(40);
        assert_eq!(t.replies, ["retry now"]);
    }

    #[test]
    fn smooth_reveal_paces_text_and_flush_shows_everything() {
        let mut t = Transcript::new(true, true);
        t.queue_reveal(Stream::Text, "x".repeat(40), 40);
        assert!(t.revealing());
        t.advance_reveal(40);
        assert!(t.revealing(), "a step shows only part of the backlog");
        t.flush_reveal(40);
        assert!(!t.revealing());
    }

    #[test]
    fn thinking_commits_only_at_line_ends() {
        let mut t = Transcript::new(true, false);
        t.queue_reveal(Stream::Thinking, "first\n\n".into(), 40);
        assert!(t.pending.is_empty(), "one paragraph is not yet stable");
        t.queue_reveal(Stream::Thinking, "second".into(), 40);
        assert!(t.pending.is_empty(), "a delta without a line end is not parsed");
        t.queue_reveal(Stream::Thinking, "\n".into(), 40);
        assert!(!t.pending.is_empty(), "the line end makes the first paragraph stable");
        t.finish_thinking(40);
        assert!(!t.thinking_active());
    }
}
