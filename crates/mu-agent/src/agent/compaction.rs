use super::Agent;
use mu_base::config::Threshold;
use mu_base::events::AgentEvent;
use mu_base::message::{Block, Message, text_of};
use mu_session::session::CompactionState;
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
pub fn window_and_threshold(window: u32, th: Option<Threshold>) -> (u64, u64) {
    let mut w = window as u64;
    let mut fixed = None;
    match th {
        Some(Threshold::Window(v)) => w = w.min(v as u64),
        Some(Threshold::Percent(p)) => fixed = Some(w * p as u64 / 100),
        Some(Threshold::Fixed(n)) => fixed = Some(n as u64),
        None => {}
    }
    let reserve = w * 15 / 100;
    let default = if 16384 >= w - reserve { w - reserve } else { w - reserve.max(16384) };
    (w, fixed.unwrap_or(default))
}

/// Index of the first kept message: the smallest user/assistant index whose suffix fits `KEEP_TOKENS`,
/// but never past the last user/assistant message.
pub fn cut_point(messages: &[Message]) -> usize {
    let Some(last) = messages.iter().rposition(|m| !matches!(m, Message::ToolResult { .. })) else { return 0 };
    let mut sum = 0;
    let mut chosen = last;
    for i in (0..messages.len()).rev() {
        sum += messages[i].estimate_tokens();
        if sum > KEEP_TOKENS {
            break;
        }
        if !matches!(messages[i], Message::ToolResult { .. }) {
            chosen = i;
        }
    }
    chosen.min(last)
}

/// Keeps the head and tail of `t` when it is longer than `max` chars, with an elision marker between.
pub fn cap_chars(t: &str, max: usize) -> String {
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

/// Serializes `messages` into at most about `max_chars` characters, dropping the oldest first.
pub fn serialize_within(messages: &[Message], max_chars: usize) -> String {
    let parts: Vec<Vec<String>> = messages.iter().map(serialize_one).collect();
    let size = |p: &Vec<String>| p.iter().map(|s| s.len() + 2).sum::<usize>();
    let mut total: usize = parts.iter().map(size).sum();
    let mut skip = 0;
    while total > max_chars && skip + 1 < parts.len() {
        total -= size(&parts[skip]);
        skip += 1;
    }
    let mut out: Vec<String> = Vec::new();
    if skip > 0 {
        out.push(format!("[{skip} older messages omitted to fit the summarizer's context window]"));
    }
    out.extend(parts.into_iter().skip(skip).flatten());
    out.join("\n\n")
}

pub fn summary_prompt(serialized: &str, prev: Option<&str>, focus: Option<&str>, extra: &[String]) -> String {
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

pub fn context_message(summary: &str) -> Message {
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
        let th = self.providers.cfg.thresholds();
        window_and_threshold(self.model.context_window, lookup(&th, &self.model.id))
    }

    /// Estimated tokens of the next request: system prompt, summary context and messages. The
    /// system prompt is only measured when its inputs change, and the messages' sum is reused until
    /// a message is added or removed, so frequent status updates do not recompute either.
    pub fn estimate_tokens(&self) -> u64 {
        self.system_tokens()
            + self.messages_tokens()
            + self.compaction.as_ref().map_or(0, |c| estimate_tokens(&c.summary))
    }

    /// Newest usage if it postdates the last compaction, else an estimate.
    pub fn context_tokens(&self) -> u64 {
        self.fresh_usage_tokens().unwrap_or_else(|| self.estimate_tokens())
    }

    pub fn needs_compaction(&self, extra: u64) -> bool {
        self.context_tokens() + extra > self.thresholds().1
    }

    /// Messages as sent to the provider: the summary context first when compacted.
    pub fn wire_messages(&self) -> Vec<Message> {
        let mut v = Vec::with_capacity(self.messages.len() + 1);
        if let Some(c) = &self.compaction {
            v.push(context_message(&c.summary));
        }
        v.extend(self.messages.iter().cloned());
        v
    }

    pub async fn compact(&mut self, focus: Option<&str>, cancel: &CancellationToken) -> Outcome {
        if self.messages.len() < 2 {
            return Outcome::Skipped("Nothing to compact");
        }
        if self.since_compaction == 0 {
            return Outcome::Skipped("Already compacted");
        }
        let cut = cut_point(&self.messages);
        if cut == 0 {
            return Outcome::Skipped("Nothing to compact");
        }
        self.emit(AgentEvent::Compacting);
        let before = self.context_tokens();
        let extra: Vec<String> = self
            .plugins
            .hook("pre_compaction", json!({}))
            .await
            .into_iter()
            .filter_map(|(_, v)| v["context"].as_str().filter(|c| !c.is_empty()).map(String::from))
            .collect();
        let prompt = summary_prompt(
            &serialize_within(&self.messages[..cut], summary_input_chars(self.model.context_window)),
            self.compaction.as_ref().map(|c| c.summary.as_str()),
            focus,
            &extra,
        );
        let summary =
            match self.providers.complete(&self.model.id, SYSTEM_PROMPT, &prompt, SUMMARY_MAX_TOKENS, cancel).await {
                Ok(s) if !s.trim().is_empty() => s.trim().to_string(),
                Ok(_) => return Outcome::Failed("empty summary".into()),
                Err(e) => return Outcome::Failed(e.to_string()),
            };
        let first_kept = self.entry_ids.get(cut).cloned().unwrap_or_default();
        if self.session.is_some() && first_kept.is_empty() {
            // The entry the summary would point at was never written; cutting there would corrupt the chain.
            return Outcome::Skipped("Cannot compact: the session file is missing entries");
        }
        self.messages.drain(..cut);
        self.entry_ids.drain(..cut);
        let after_est = self.system_tokens()
            + estimate_tokens(&summary)
            + self.messages.iter().map(Message::estimate_tokens).sum::<u64>();
        if let Some(s) = self.session.as_mut() {
            s.compaction(&summary, &first_kept, before, after_est);
        }
        self.compaction = Some(CompactionState { summary });
        self.fresh_from = self.messages.len();
        self.since_compaction = 0;
        self.emit(AgentEvent::Compacted { before, after: after_est });
        self.emit_status();
        Outcome::Done
    }

    /// Fire-and-forget title for the first exchange of a new session.
    pub fn request_title(&mut self) {
        if self.title.is_some() || self.title_requested {
            return;
        }
        let Some(Message::User { content, .. }) =
            self.messages.iter().find(|m| matches!(m, Message::User { synthetic: false, .. }))
        else {
            return;
        };
        let prompt: String = text_of(content).chars().take(2000).collect();
        self.title_requested = true;
        let (providers, tx, sid) = (self.providers.clone(), self.title_tx.clone(), self.session_id());
        let cancel = self.title_cancel.clone();
        tokio::task::spawn_local(async move {
            let r = providers.complete("smol", TITLE_SYSTEM, &prompt, 64, &cancel).await;
            if let Some(t) = r.ok().and_then(|s| extract_title(&s)) {
                let _ = tx.send((sid, t));
            }
        });
    }

    pub(super) async fn manual_compact(&mut self, focus: Option<String>) {
        let cancel = CancellationToken::new();
        self.inbox.borrow_mut().cancel = cancel.clone();
        self.emit(AgentEvent::TurnStart);
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
    fn caps_keep_head_and_tail_and_oldest_messages_go_first() {
        let c = cap_chars(&"a".repeat(3000), 2000);
        assert!(c.starts_with("aaa") && c.contains("1000 chars elided") && c.chars().count() < 2100);
        assert_eq!(cap_chars("short", 2000), "short");
        let msgs: Vec<Message> = (0..10).map(|i| Message::user(&format!("{i}{}", "x".repeat(900)))).collect();
        let s = serialize_within(&msgs, 3000);
        assert!(s.contains("older messages omitted") && s.contains("[User]: 9") && !s.contains("[User]: 0"));
        assert!(s.len() < 4000);
        assert_eq!(summary_input_chars(200_000), 480_000);
    }

    #[test]
    fn thresholds() {
        assert_eq!(window_and_threshold(200_000, None), (200_000, 170_000));
        assert_eq!(window_and_threshold(1_000_000, None), (1_000_000, 850_000));
        assert_eq!(window_and_threshold(10_000, None), (10_000, 8_500));
        assert_eq!(window_and_threshold(20_000, None), (20_000, 3_616));
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
    fn cut_keeps_recent_suffix_and_last_group() {
        let big = "x".repeat(40_000); // ~10k tokens each
        let u = |t: &str| Message::user(t);
        let msgs = vec![u(&big), u(&big), u(&big), u("tail")];
        let cut = cut_point(&msgs);
        assert_eq!(cut, 2); // last two ≈ 10k + tiny fit, third does not
        assert_eq!(cut_point(&[u("a")]), 0);
        let tr = Message::ToolResult {
            tool_call_id: "1".into(),
            tool_name: "read".into(),
            content: vec![Block::text(big.clone())],
            is_error: false,
            timestamp: 0,
            details: None,
        };
        let msgs = vec![u("a"), u(&big), tr.clone(), tr.clone(), tr];
        assert_eq!(cut_point(&msgs), 1); // never lands on a tool result
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
