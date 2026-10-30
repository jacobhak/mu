//! Ghost-text engine: whole-prompt repeats, word completion (SmolLM when attached, else the n-gram), and
//! identifiers from the last reply.

use pi_predict::{Config, Predictor, Query, Suggestion};
use std::collections::HashMap;
use std::path::Path;

const BEFORE_LIMIT: usize = 2000;

pub struct State {
    ngram: Box<dyn Predictor>,
    /// The word engine when SmolLM is selected and loaded; replaces the n-gram for word ghosts.
    smollm: Option<Box<dyn Predictor>>,
    /// cwd -> prompt -> times submitted.
    lines: HashMap<String, HashMap<String, u32>>,
}

impl State {
    pub fn open(state_dir: &Path) -> Result<Self, String> {
        // The model is rebuilt from the corpus on every start and never persisted.
        let cfg = Config { state_dir: state_dir.to_owned(), model_dir: None, show_threshold: Some(0.0) };
        let ngram = pi_predict::ngram::open(&cfg).map_err(|e| e.to_string())?;
        Ok(Self { ngram, smollm: None, lines: HashMap::new() })
    }

    pub fn observe(&mut self, text: &str, cwd: &str) {
        self.observe_n(text, cwd, 1);
    }

    /// Feeds the n-gram once and counts the prompt `n` times for whole-prompt ghosts.
    pub fn observe_n(&mut self, text: &str, cwd: &str, n: u32) {
        self.ngram.observe(text);
        if let Some(p) = self.smollm.as_mut() {
            p.observe(text);
        }
        *self.lines.entry(cwd.to_owned()).or_default().entry(text.to_owned()).or_default() += n;
    }

    /// Ghost for `text` at the cursor (end of buffer); empty when there is none.
    pub fn complete(&mut self, text: &str, cwd: &str, assistant: &str, threshold: f32) -> String {
        if let Some(g) = self.line_ghost(text, cwd) {
            return g;
        }
        if let Some(s) = self.word_suggestion(text).filter(|s| s.confidence >= threshold) {
            return s.suffix;
        }
        reply_ghost(text, assistant).unwrap_or_default()
    }

    /// The most repeated prompt in `cwd` that continues `text`.
    pub fn line_ghost(&self, text: &str, cwd: &str) -> Option<String> {
        if text.is_empty() {
            return None;
        }
        let (best, _) = self
            .lines
            .get(cwd)?
            .iter()
            // Exact case: a candidate that differs only in case could never be shown as a suffix, and
            // picking it as the maximum used to hide valid ones.
            .filter(|(e, n)| **n >= 2 && e.len() > text.len() && e.starts_with(text))
            .max_by(|(a, na), (b, nb)| na.cmp(nb).then(a.len().cmp(&b.len())).then(b.cmp(a)))?;
        Some(best[text.len()..].to_owned())
    }

    /// The word ghost for the word ending `text`, with its confidence, from SmolLM when it is loaded, else the n-gram.
    /// SmolLM is asked from the first letter: gating it at two cost it more keystrokes than it saved in wrong ghosts.
    pub fn word_suggestion(&mut self, text: &str) -> Option<Suggestion> {
        let (before, prefix) = split_word(text)?;
        let q = Query { before, prefix };
        match self.smollm.as_mut() {
            Some(p) => p.complete(&q),
            None => self.ngram.complete(&q),
        }
    }

    pub fn smollm_active(&self) -> bool {
        self.smollm.is_some()
    }

    /// Runs a throwaway query so SmolLM re-scores its recent-prompt head now, not on the next keystroke.
    pub fn warm(&mut self) {
        if let Some(p) = self.smollm.as_mut() {
            let _ = p.complete(&Query { before: "", prefix: "th" });
        }
    }

    /// Makes SmolLM the word engine, after learning the prompts it missed while loading.
    pub fn attach_smollm(&mut self, mut p: Box<dyn Predictor>, missed: &[String]) {
        for prompt in missed {
            p.observe(prompt);
        }
        self.smollm = Some(p);
    }

    /// Saves the state an engine keeps across runs (SmolLM's recent prompts). The n-gram is rebuilt from history.
    pub fn persist(&mut self) -> Result<(), String> {
        match self.smollm.as_mut() {
            Some(p) => p.persist().map_err(|e| format!("{e:#}")),
            None => Ok(()),
        }
    }
}

/// Mirrors omp's `word-completion.ts`: the trailing run of letters and apostrophes (at least one
/// letter) and the preceding text capped to its last 2000 chars. Words glued to code or paths
/// (`src/fo`, `a_b`, `v2x`) are skipped.
pub fn split_word(text: &str) -> Option<(&str, &str)> {
    let start =
        text.char_indices().rev().take_while(|(_, c)| c.is_alphabetic() || *c == '\'').last().map(|(i, _)| i)?;
    let prefix = &text[start..];
    if !prefix.chars().any(char::is_alphabetic) {
        return None;
    }
    let before = &text[..start];
    if before.chars().next_back().is_some_and(|c| c.is_ascii_digit() || "/_.:#-".contains(c)) {
        return None;
    }
    let skip = before.chars().count().saturating_sub(BEFORE_LIMIT);
    let cut = before.char_indices().nth(skip).map_or(before.len(), |(i, _)| i);
    Some((&before[cut..], prefix))
}

fn is_start(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

fn is_body(c: char) -> bool {
    is_start(c) || "./:#-".contains(c)
}

/// `(start byte, token)` for every `[A-Za-z0-9_][A-Za-z0-9_./:#-]*` run in `s`.
fn tokens(s: &str) -> Vec<(usize, &str)> {
    let mut out = Vec::new();
    let mut start: Option<usize> = None;
    for (i, c) in s.char_indices() {
        match start {
            None if is_start(c) => start = Some(i),
            Some(st) if !is_body(c) => {
                out.push((st, &s[st..i]));
                start = None;
            }
            _ => {}
        }
    }
    if let Some(st) = start {
        out.push((st, &s[st..]));
    }
    out
}

/// Completes the identifier or path being typed from tokens of the last assistant reply.
pub fn reply_ghost(text: &str, assistant: &str) -> Option<String> {
    let &(_, tail) = tokens(text).last().filter(|(st, t)| st + t.len() == text.len())?;
    if tail.chars().count() < 2 {
        return None;
    }
    // token -> (count, last occurrence)
    let mut seen: HashMap<&str, (u32, usize)> = HashMap::new();
    for (i, (_, t)) in tokens(assistant).into_iter().enumerate() {
        let t = t.trim_end_matches(['.', ':', '#', '-']);
        let e = seen.entry(t).or_default();
        *e = (e.0 + 1, i);
    }
    let (best, _) =
        seen.into_iter().filter(|(t, _)| t.len() > tail.len() && t.starts_with(tail)).max_by_key(|&(_, v)| v)?;
    Some(best[tail.len()..].to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A word engine that always offers "XYZ" at confidence 0.9.
    struct Fake;

    impl Predictor for Fake {
        fn complete(&mut self, _: &Query<'_>) -> Option<Suggestion> {
            Some(Suggestion { suffix: "XYZ".into(), confidence: 0.9 })
        }
        fn observe(&mut self, _: &str) {}
        fn feedback(&mut self, _: &Query<'_>, _: &str, _: bool) {}
        fn persist(&mut self) -> anyhow::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn smollm_replaces_the_word_ghost_from_the_first_letter() {
        let mut s = state();
        s.attach_smollm(Box::new(Fake), &[]);
        assert!(s.smollm_active());
        assert_eq!(s.complete("fix the pa", "/x", "", 0.3), "XYZ");
        assert_eq!(s.complete("fix the pa", "/x", "", 0.95), "", "below the request threshold");
        assert_eq!(s.complete("fix the p", "/x", "", 0.3), "XYZ", "one letter: SmolLM is asked");
        assert_eq!(s.persist(), Ok(()));
    }

    fn state() -> State {
        let dir = std::env::temp_dir().join(format!("mu-predict-test-{}", std::process::id()));
        State::open(&dir).unwrap()
    }

    #[test]
    fn line_ghost_needs_two_in_same_cwd() {
        let mut s = state();
        s.observe("commit and push", "/x");
        assert_eq!(s.line_ghost("comm", "/x"), None, "one sighting is not a repeat");
        s.observe("commit and push", "/x");
        assert_eq!(s.complete("comm", "/x", "", 0.3), "it and push");
        assert_eq!(s.line_ghost("comm", "/y"), None);
        assert_eq!(s.line_ghost("COMM", "/x"), None, "casing must match");
    }

    #[test]
    fn a_case_variant_does_not_hide_a_valid_candidate() {
        let mut s = state();
        // "Commit" is more frequent but cannot continue "comm"; "commit" can.
        for _ in 0..5 {
            s.observe("Commit all of it now please", "/z");
        }
        for _ in 0..2 {
            s.observe("commit it", "/z");
        }
        assert_eq!(s.line_ghost("comm", "/z"), Some("it it".into()));
    }

    #[test]
    fn reply_token_ghost() {
        let mut s = state();
        assert_eq!(s.complete("see crates/mu-co", "/x", "edit crates/mu-common/src/rpc.rs", 0.3), "mmon/src/rpc.rs");
        assert_eq!(reply_ghost("see x", "edit xyz"), None, "single char tail");
        assert_eq!(reply_ghost("see fo", "foo bar foo.rs foo.rs."), Some("o.rs".into()), "most frequent wins");
    }

    #[test]
    fn word_split() {
        assert_eq!(split_word("fix the bu"), Some(("fix the ", "bu")));
        assert_eq!(split_word("fix the "), None);
        assert_eq!(split_word("src/fo"), None);
        assert_eq!(split_word("don't sto"), Some(("don't ", "sto")));
        let long = "x ".repeat(3000) + "wor";
        assert_eq!(split_word(&long).unwrap().0.chars().count(), 2000);
    }
}
