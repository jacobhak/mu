use super::Agent;
use mu_base::config::Threshold;
use mu_base::events::AgentEvent;
use mu_base::message::{Block, Message, text_of};
use mu_base::models::ModelSpec;
use mu_session::session::format::CompactionState;
use serde_json::json;
use std::collections::BTreeMap;
use tokio_util::sync::CancellationToken;

const SUMMARY_PROMPT: &str = include_str!("../prompts/compaction-summary.md");
const UPDATE_PROMPT: &str = include_str!("../prompts/compaction-update-summary.md");
const SYSTEM_PROMPT: &str = include_str!("../prompts/summarization-system.md");
const CONTEXT_TEMPLATE: &str = include_str!("../prompts/compaction-summary-context.md");
const KEEP_TOKENS: u64 = 20_000;
const SUMMARY_MAX_TOKENS: u32 = 13_107;
const TOOL_RESULT_CHARS: usize = 2000;

/// The summary input may fill 60% of the summarizer's window (about 4 chars per token).
fn summary_input_chars(window: u32) -> usize {
    (window as usize).saturating_mul(4) * 6 / 10
}

const TITLE_SYSTEM: &str = "Write a 3–7 word title for a coding session that starts with the user request below. Reply with only <title>…</title>.";

/// Exact key, else the longest `*`-suffixed prefix, matched against `provider/model`.
fn lookup(th: &BTreeMap<String, Threshold>, id: &str) -> Option<Threshold> {
    if let Some(t) = th.get(id) {
        return Some(*t);
    }
    th.iter()
        .filter_map(|(k, v)| k.strip_suffix('*').filter(|p| id.starts_with(p)).map(|p| (p.len(), *v)))
        .max_by_key(|(n, _)| *n)
        .map(|(_, v)| v)
}

/// Effective window and compaction threshold in tokens.
pub(crate) fn window_and_threshold(window: u32, th: Option<Threshold>) -> (u64, u64) {
    let mut w = window as u64;
    let mut fixed = None;
    match th {
        Some(Threshold::Window(v)) => w = w.min(v as u64),
        Some(Threshold::Percent(p)) => fixed = Some(w * p as u64 / 100),
        Some(Threshold::Fixed(n)) => fixed = Some(n as u64),
        None => {}
    }
    // Large windows keep a reserve of max(15%, 16384); small ones fall back to 85% (capped at 16384)
    // so the result never drops as the window grows.
    let reserve = w * 15 / 100;
    let default = w.saturating_sub(reserve.max(16384)).max((w - reserve).min(16384));
    (w, fixed.unwrap_or(default))
}

/// Tokens of tail a compaction keeps: `KEEP_TOKENS`, but at most half the threshold so the context ends under it.
fn keep_tokens(threshold: u64) -> u64 {
    KEEP_TOKENS.min(threshold / 2)
}

/// Index of the first kept message: the smallest user/assistant index whose suffix fits `keep` tokens,
/// but never past the last user/assistant message.
pub(crate) fn cut_point(messages: &[Message], target: &ModelSpec, keep: u64) -> usize {
    let Some(last) = messages.iter().rposition(|m| !matches!(m, Message::ToolResult { .. })) else { return 0 };
    let mut sum = 0;
    let mut chosen = last;
    for i in (0..messages.len()).rev() {
        sum += messages[i].estimate_tokens(target);
        if sum > keep {
            break;
        }
        if !matches!(messages[i], Message::ToolResult { .. }) {
            chosen = i;
        }
    }
    chosen.min(last)
}

/// Keeps the head and tail of `t` when it is longer than `max` chars, with an elision marker between.
pub(crate) fn cap_chars(t: &str, max: usize) -> String {
    let n = t.chars().count();
    if n <= max {
        return t.to_string();
    }
    let head = max / 2;
    let tail = max - head;
    let h: String = t.chars().take(head).collect();
    let l: String = t.chars().skip(n - tail).collect();
    format!("{h}\n[… {} chars elided …]\n{l}", n - max)
}

fn serialize_one(m: &Message) -> Vec<String> {
    let mut out = Vec::new();
    match m {
        Message::User { content, .. } => {
            out.push(format!("[User]: {}", cap_chars(&text_of(content), TOOL_RESULT_CHARS)));
        }
        Message::Assistant { content, .. } => {
            let t = text_of(content);
            if !t.is_empty() {
                out.push(format!("[Assistant]: {t}"));
            }
            for b in content {
                if let Block::ToolCall { name, arguments, .. } = b {
                    out.push(format!(
                        "[Assistant tool call]: {name} {}",
                        cap_chars(&arguments.to_string(), TOOL_RESULT_CHARS)
                    ));
                }
            }
        }
        Message::ToolResult { tool_name, content, is_error, .. } => {
            let t = cap_chars(&text_of(content), TOOL_RESULT_CHARS);
            out.push(format!("[Tool result {tool_name}{}]: {t}", if *is_error { " (error)" } else { "" }));
        }
    }
    out
}

/// Serializes `messages` in order into chunks of at most `max_chars` characters, so every message
/// reaches the summarizer. A part longer than `max_chars` is cut with `cap_chars`.
pub(crate) fn chunks_within(messages: &[Message], max_chars: usize) -> Vec<String> {
    let len = |s: &str| s.chars().count();
    let mut chunks: Vec<String> = Vec::new();
    let mut cur = String::new();
    for part in messages.iter().flat_map(serialize_one) {
        let part = cap_chars(&part, max_chars);
        if !cur.is_empty() && len(&cur) + 2 + len(&part) > max_chars {
            chunks.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push_str("\n\n");
        }
        cur.push_str(&part);
    }
    if !cur.is_empty() {
        chunks.push(cur);
    }
    chunks
}

pub(crate) fn summary_prompt(serialized: &str, prev: Option<&str>, focus: Option<&str>, extra: &[String]) -> String {
    let mut p = format!("<conversation>\n{serialized}\n</conversation>\n\n");
    if let Some(prev) = prev {
        p.push_str(&format!("<previous-summary>\n{prev}\n</previous-summary>\n\n"));
    }
    p.push_str(if prev.is_some() { UPDATE_PROMPT } else { SUMMARY_PROMPT }.trim_end());
    if let Some(f) = focus.filter(|f| !f.trim().is_empty()) {
        p.push_str(&format!("\n\nAdditional focus: {f}"));
    }
    if !extra.is_empty() {
        p.push_str(&format!("\n\n<additional-context>\n{}\n</additional-context>", extra.join("\n\n")));
    }
    p
}

pub(crate) fn context_message(summary: &str) -> Message {
    Message::user(&CONTEXT_TEMPLATE.replace("{{summary}}", summary))
}

fn extract_title(s: &str) -> Option<String> {
    let t = s.split_once("<title>")?.1.split("</title>").next()?.trim();
    (!t.is_empty() && !t.eq_ignore_ascii_case("none")).then(|| t.chars().take(100).collect())
}

pub enum Outcome {
    Done,
    Skipped(&'static str),
    Failed(String),
}

/// The request-size heuristic: about 4 bytes per token.
pub(super) fn estimate_tokens(text: &str) -> u64 {
    text.len() as u64 / 4
}

impl Agent {
    pub fn thresholds(&self) -> (u64, u64) {
        let th = self.ports.providers.cfg.thresholds();
        window_and_threshold(self.model.model.context_window, lookup(&th, &self.model.model.id))
    }

    /// Estimated tokens of the next request: system prompt, tool definitions, summary context and
    /// messages. The system prompt is only measured when its inputs change, and the messages' sum is
    /// reused until a message is added or removed, so frequent status updates do not recompute either.
    pub(crate) fn estimate_tokens(&self) -> u64 {
        self.system_tokens()
            + mu_base::tool::tool_tokens(&self.tool_defs())
            + self.messages_tokens()
            + self.compaction.state.as_ref().map_or(0, |c| estimate_tokens(&c.summary))
    }

    /// Newest usage if it postdates the last compaction, else an estimate.
    pub fn context_tokens(&self) -> u64 {
        self.fresh_usage_tokens().unwrap_or_else(|| self.estimate_tokens())
    }

    pub(crate) fn needs_compaction(&self, extra: u64) -> bool {
        self.context_tokens() + extra > self.thresholds().1
    }

    /// Over the threshold, and not a repeat of a failed attempt: after a failure the context must
    /// first grow by a margin (a new prompt clears the memo).
    pub(crate) fn compaction_due(&self, extra: u64) -> bool {
        if !self.needs_compaction(extra) {
            return false;
        }
        let margin = (self.thresholds().1 / 10).max(1_000);
        self.compaction.failed_at.is_none_or(|at| self.context_tokens() >= at + margin)
    }

    /// Compacts and reports a failure once, remembering the context size it failed at.
    pub(super) async fn compact_reporting(&mut self, cancel: &CancellationToken) {
        if let Outcome::Failed(e) = self.compact(None, cancel).await {
            self.compaction.failed_at = Some(self.context_tokens());
            self.report_compaction_failure(e, cancel);
        }
    }

    /// Messages as sent to the provider: the summary context first when compacted.
    pub(crate) fn wire_messages(&self) -> Vec<Message> {
        let mut v = Vec::with_capacity(self.messages.len() + 1);
        if let Some(c) = &self.compaction.state {
            v.push(context_message(&c.summary));
        }
        v.extend(self.messages.iter().cloned());
        v
    }

    pub async fn compact(&mut self, focus: Option<&str>, cancel: &CancellationToken) -> Outcome {
        if self.messages.len() < 2 {
            return Outcome::Skipped("Nothing to compact");
        }
        if self.compaction.since == 0 {
            return Outcome::Skipped("Already compacted");
        }
        let history = self.messages.to_vec();
        let cut = cut_point(&history, &self.model.model, keep_tokens(self.thresholds().1));
        if cut == 0 {
            return Outcome::Skipped("Nothing to compact");
        }
        self.emit(AgentEvent::Compacting);
        let before = self.context_tokens();
        let extra: Vec<String> = self
            .ports
            .plugins
            .hook("pre_compaction", json!({}))
            .await
            .into_iter()
            .filter_map(|(_, v)| v["context"].as_str().filter(|c| !c.is_empty()).map(String::from))
            .collect();
        // Each chunk is folded into the summary so far, so the last call has seen everything.
        let chunks = match chunks_within(&history[..cut], summary_input_chars(self.model.model.context_window)) {
            c if c.is_empty() => vec![String::new()],
            c => c,
        };
        let mut prev = self.compaction.state.as_ref().map(|c| c.summary.clone());
        let mut summary = String::new();
        for chunk in &chunks {
            let prompt = summary_prompt(chunk, prev.as_deref(), focus, &extra);
            summary = match self
                .ports
                .providers
                .complete(&self.model.model.id, SYSTEM_PROMPT, &prompt, SUMMARY_MAX_TOKENS, cancel)
                .await
            {
                Ok(s) if !s.trim().is_empty() => s.trim().to_string(),
                Ok(_) => return Outcome::Failed("empty summary".into()),
                Err(e) => return Outcome::Failed(e.to_string()),
            };
            prev = Some(summary.clone());
        }
        let first_kept = self.messages.id(cut).map(String::from);
        if self.session.is_some() && first_kept.is_none() {
            // The entry the summary would point at was never written; cutting there would corrupt the chain.
            return Outcome::Skipped("Cannot compact: the session file is missing entries");
        }
        self.messages.drain_front(cut);
        let after_est = self.system_tokens()
            + mu_base::tool::tool_tokens(&self.tool_defs())
            + estimate_tokens(&summary)
            + self.messages.iter().map(|m| m.estimate_tokens(&self.model.model)).sum::<u64>();
        if let Some(s) = self.session.as_mut() {
            s.compaction(&summary, first_kept.as_deref().unwrap_or_default(), before, after_est);
        }
        self.compaction.state = Some(CompactionState { summary });
        self.compaction.fresh_from = self.messages.len();
        self.compaction.since = 0;
        self.compaction.failed_at = None;
        self.emit(AgentEvent::Compacted { before, after: after_est });
        self.emit_status();
        Outcome::Done
    }

    /// Fire-and-forget title for the first exchange of a new session.
    pub(crate) fn request_title(&mut self) {
        if self.title.text.is_some() || self.title.requested {
            return;
        }
        let Some(Message::User { content, .. }) =
            self.messages.iter().find(|m| matches!(m, Message::User { synthetic: false, .. }))
        else {
            return;
        };
        let prompt: String = text_of(content).chars().take(2000).collect();
        self.title.requested = true;
        let (providers, tx, sid) = (self.ports.providers.clone(), self.title.tx.clone(), self.session_id());
        let cancel = self.title.cancel.clone();
        tokio::task::spawn_local(async move {
            let r = providers.complete("smol", TITLE_SYSTEM, &prompt, 64, &cancel).await;
            if let Some(t) = r.ok().and_then(|s| extract_title(&s)) {
                let _ = tx.send((sid, t));
            }
        });
    }

    pub(super) async fn manual_compact(&mut self, focus: Option<String>) {
        let cancel = CancellationToken::new();
        self.ports.inbox.borrow_mut().cancel = cancel.clone();
        self.emit_turn_start();
        match self.compact(focus.as_deref(), &cancel).await {
            Outcome::Done => {}
            Outcome::Skipped(why) => self.notice(why),
            Outcome::Failed(e) => self.report_compaction_failure(e, &cancel),
        }
        self.emit(AgentEvent::TurnEnd { aborted: cancel.is_cancelled() });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caps_keep_head_and_tail() {
        let c = cap_chars(&"a".repeat(3000), 2000);
        assert!(c.starts_with("aaa") && c.contains("1000 chars elided") && c.chars().count() < 2100);
        assert_eq!(cap_chars("short", 2000), "short");
        assert_eq!(summary_input_chars(200_000), 480_000);
    }

    #[test]
    fn chunks_keep_every_message_in_order() {
        let msgs: Vec<Message> = (0..10).map(|i| Message::user(&format!("{i}{}", "x".repeat(900)))).collect();
        let chunks = chunks_within(&msgs, 3000);
        assert!(chunks.len() > 1 && chunks.iter().all(|c| c.chars().count() <= 3000), "{}", chunks.len());
        let all = chunks.join("\n\n");
        let at: Vec<usize> = (0..10).map(|i| all.find(&format!("[User]: {i}x")).expect("kept")).collect();
        assert!(at.windows(2).all(|w| w[0] < w[1]));
        assert!(!all.contains("omitted"));
    }

    #[test]
    fn three_chunks_from_a_small_budget() {
        let msgs: Vec<Message> = (0..3).map(|i| Message::user(&format!("{i}{}", "x".repeat(900)))).collect();
        let chunks = chunks_within(&msgs, 1000);
        assert_eq!(chunks.len(), 3);
        for (i, chunk) in chunks.iter().enumerate() {
            assert!(chunk.contains(&format!("[User]: {i}{}", "x".repeat(900))), "message {i} is whole");
        }
    }

    #[test]
    fn an_oversized_part_is_cut_not_dropped() {
        let chunks = chunks_within(&[Message::user(&"a".repeat(1900))], 1000);
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].starts_with("[User]: aaa") && chunks[0].contains("chars elided"));
        assert!(chunks_within(&[], 1000).is_empty());
    }

    #[test]
    fn thresholds() {
        assert_eq!(window_and_threshold(200_000, None), (200_000, 170_000));
        assert_eq!(window_and_threshold(1_000_000, None), (1_000_000, 850_000));
        assert_eq!(window_and_threshold(10_000, None), (10_000, 8_500));
        assert_eq!(window_and_threshold(19_000, None), (19_000, 16_150));
        assert_eq!(window_and_threshold(20_000, None), (20_000, 16_384));
        assert_eq!(window_and_threshold(200_000, Some(Threshold::Window(100_000))), (100_000, 83_616));
        assert_eq!(window_and_threshold(200_000, Some(Threshold::Percent(50))), (200_000, 100_000));
        assert_eq!(window_and_threshold(200_000, Some(Threshold::Fixed(20_000))), (200_000, 20_000));
        let mut m = BTreeMap::new();
        m.insert("a/*".into(), Threshold::Fixed(1));
        m.insert("a/b*".into(), Threshold::Fixed(2));
        m.insert("a/bc".into(), Threshold::Fixed(3));
        assert_eq!(lookup(&m, "a/bc"), Some(Threshold::Fixed(3)));
        assert_eq!(lookup(&m, "a/bd"), Some(Threshold::Fixed(2)));
        assert_eq!(lookup(&m, "a/x"), Some(Threshold::Fixed(1)));
        assert_eq!(lookup(&m, "z/x"), None);
    }

    #[test]
    fn default_threshold_never_drops_as_the_window_grows() {
        let mut prev = 0;
        for w in (1_000..=2_000_000u32).step_by(100) {
            let t = window_and_threshold(w, None).1;
            assert!(t >= prev, "window {w}: {t} < {prev}");
            assert!(t < w as u64);
            prev = t;
        }
    }

    #[test]
    fn kept_tail_is_under_the_threshold_on_small_windows() {
        for w in (1_000..=40_000u32).step_by(500) {
            let t = window_and_threshold(w, None).1;
            assert!(keep_tokens(t) < t, "window {w}");
        }
        assert_eq!(keep_tokens(window_and_threshold(200_000, None).1), KEEP_TOKENS);
    }

    #[test]
    fn cut_keeps_recent_suffix_and_last_group() {
        let big = "x".repeat(40_000); // ~10k tokens each
        let u = |t: &str| Message::user(t);
        let target = ModelSpec::gateway("anthropic/test", 200_000, 32_000);
        let msgs = vec![u(&big), u(&big), u(&big), u("tail")];
        let cut = cut_point(&msgs, &target, KEEP_TOKENS);
        assert_eq!(cut, 2); // last two ≈ 10k + tiny fit, third does not
        assert_eq!(cut_point(&msgs, &target, 5_000), 3); // a smaller budget keeps only the tail
        assert_eq!(cut_point(&[u("a")], &target, KEEP_TOKENS), 0);
        let tr = Message::ToolResult {
            tool_call_id: "1".into(),
            tool_name: "read".into(),
            content: vec![Block::text(big.clone())],
            is_error: false,
            timestamp: 0,
            details: None,
        };
        let msgs = vec![u("a"), u(&big), tr.clone(), tr.clone(), tr];
        assert_eq!(cut_point(&msgs, &target, KEEP_TOKENS), 1); // never lands on a tool result
    }

    #[test]
    fn prompts_and_titles() {
        let p = summary_prompt("S", Some("OLD"), Some("focus"), &["ctx".into()]);
        assert!(
            p.starts_with("<conversation>\nS\n</conversation>\n\n<previous-summary>\nOLD\n</previous-summary>\n\n")
        );
        assert!(
            p.contains("\n\nAdditional focus: focus")
                && p.ends_with("<additional-context>\nctx\n</additional-context>")
        );
        assert_eq!(extract_title("<title> Fix bug </title>").as_deref(), Some("Fix bug"));
        assert_eq!(extract_title("<title>none</title>"), None);
        assert_eq!(extract_title("no tags"), None);
    }
}
