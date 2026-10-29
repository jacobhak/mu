//! Spots a turn that ends by asking the user a decision in prose, so the agent can remind the
//! model to use the `ask` tool instead. Conservative on purpose: a miss costs nothing, a false
//! alarm costs one extra request.

/// Sent as a user message when `asks_in_prose` fires. `is_reminder` recognises it in the history.
pub const REMINDER: &str = "<system-reminder>\nYour last message ends with a question or option list in prose. If it asks the user to decide something (scope, design, trade-offs, anything ambiguous, risky or hard to undo), call `ask` now with 2-5 options and a recommendation instead of repeating the question. If it was not a decision for the user, do not ask again: reply with just `.`\n</system-reminder>";

pub fn is_reminder(text: &str) -> bool {
    text.trim() == REMINDER
}

/// Shorter messages are not questions worth a reminder ("OK?", "?").
const MIN_CHARS: usize = 8;
/// Short messages whose questions are all open are greetings or invitations, not decisions.
const SHORT_CHARS: usize = 160;
const OPTION_WINDOW: usize = 800;
const QUESTION_WINDOW: usize = 300;
/// Words that open an open question to the user ("What would you like to work on?").
const OPEN_WORDS: [&str; 7] = ["what", "how", "where", "who", "when", "why", "anything"];
/// A question that names the user or the assistant asks for a choice, not an explanation.
const NAMES: [&str; 8] = ["i", "me", "my", "you", "your", "we", "us", "our"];

/// Whether a final assistant message ends by asking the user something in prose: it ends with a
/// `?`, or it ends with at least two option lines and a `?` close to the end, or its last
/// paragraph asks a decision question. Greetings and short open invitations do not count.
pub fn asks_in_prose(text: &str) -> bool {
    let Some(prose) = prose(text) else { return false };
    let prose = prose.trim();
    if prose.chars().count() < MIN_CHARS {
        return false;
    }
    let ends_with_question =
        prose.trim_end_matches(|c: char| c.is_whitespace() || matches!(c, '*' | '_' | '`')).ends_with('?');
    let options = option_lines(tail(prose, OPTION_WINDOW)) >= 2 && tail(prose, QUESTION_WINDOW).contains('?');
    let asks = ends_with_question || options || asks_in_last_paragraph(prose);
    asks && !is_open_invitation(prose)
}

/// A question in the last paragraph that names the user or offers a choice. "Why is it slow?"
/// followed by its answer is an explanation, and "What would you like to do?" is an invitation:
/// neither counts.
fn asks_in_last_paragraph(prose: &str) -> bool {
    let clean = strip_quoted(last_paragraph(prose));
    questions(&clean).into_iter().any(|q| !is_open_question(q) && (has_word(q, &NAMES) || has_word(q, &["or"])))
}

/// A short message whose questions all open with `what`, `how`, ... and offer no choice or
/// proposal ("Hi! What would you like to work on?").
fn is_open_invitation(prose: &str) -> bool {
    if prose.chars().count() > SHORT_CHARS || option_lines(prose) > 0 {
        return false;
    }
    let clean = strip_quoted(prose);
    if has_word(&clean, &["or", "about"]) {
        return false;
    }
    let qs = questions(&clean);
    !qs.is_empty() && qs.iter().all(|q| is_open_question(q))
}

/// A question that opens with `what`, `how`, ... and offers no choice or proposal.
fn is_open_question(q: &str) -> bool {
    OPEN_WORDS.iter().any(|w| first_word(q).eq_ignore_ascii_case(w)) && !has_word(q, &["or", "about"])
}

/// The message without code fences and blockquotes: quoted or example text is not the model's
/// question. `None` while a code fence is still open at the end.
fn prose(text: &str) -> Option<String> {
    let mut out = String::new();
    let mut fenced = false;
    for line in text.lines() {
        let t = line.trim_start();
        if t.starts_with("```") || t.starts_with("~~~") {
            fenced = !fenced;
            continue;
        }
        if !fenced && !t.starts_with('>') {
            out.push_str(line);
            out.push('\n');
        }
    }
    (!fenced).then_some(out)
}

/// The text after the last blank line.
fn last_paragraph(prose: &str) -> &str {
    let (mut start, mut pos) = (0, 0);
    for line in prose.split_inclusive('\n') {
        pos += line.len();
        if line.trim().is_empty() {
            start = pos;
        }
    }
    &prose[start..]
}

/// `s` without URLs, inline code and double-quoted text: a `?` there is not the model asking.
fn strip_quoted(s: &str) -> String {
    let mut out = String::new();
    let (mut code, mut quote) = (false, false);
    for tok in s.split_inclusive(char::is_whitespace) {
        if tok.contains("://") {
            continue;
        }
        for c in tok.chars() {
            match c {
                '`' if !quote => code = !code,
                '"' if !code => quote = !quote,
                _ if code || quote => {}
                _ => out.push(c),
            }
        }
    }
    out
}

/// The sentences of `s` that end in a `?` (followed by whitespace, markup or the end), each up to
/// and including its `?`.
fn questions(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut chars = s.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        match c {
            '?' => {
                let ends = chars.peek().is_none_or(|&(_, n)| n.is_whitespace() || matches!(n, '*' | '_' | ')' | ']'));
                if ends {
                    out.push(s[start..i + 1].trim_start_matches(|c: char| !c.is_alphanumeric()));
                    start = i + 1;
                }
            }
            '.' | '!' | ':' | '\n' => start = i + 1,
            _ => {}
        }
    }
    out
}

fn has_word(s: &str, words: &[&str]) -> bool {
    s.split(|c: char| !c.is_alphanumeric()).any(|w| words.iter().any(|x| w.eq_ignore_ascii_case(x)))
}

fn first_word(s: &str) -> &str {
    s.split(|c: char| !c.is_alphanumeric()).find(|w| !w.is_empty()).unwrap_or("")
}

fn option_lines(s: &str) -> usize {
    s.lines().filter(|l| is_option(l)).count()
}

/// `1.`, `2)`, `A.` or `B)` at the start of a line, optionally bolded.
fn is_option(line: &str) -> bool {
    let t = line.trim_start().trim_start_matches('*');
    let digits = t.chars().take_while(char::is_ascii_digit).count();
    let marker = if (1..=2).contains(&digits) {
        digits
    } else if t.starts_with(|c: char| c.is_ascii_uppercase()) {
        1
    } else {
        return false;
    };
    let Some(sep) = t[marker..].strip_prefix(['.', ')']) else { return false };
    sep.starts_with([' ', '\t', '*'])
}

/// The last `n` characters of `s`.
fn tail(s: &str, n: usize) -> &str {
    match s.char_indices().rev().nth(n - 1) {
        Some((i, _)) => &s[i..],
        None => s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    #[test]
    fn detector_table() {
        let cases: &[(&str, bool)] = &[
            // Fires: ends with a question.
            ("Should I use approach A or B?", true),
            ("Proceed?", true),
            ("The fix is in place. Want me to also update the docs?", true),
            ("- Should I also update the docs?", true),
            ("**Which one do you want?**", true),
            ("Two ways.\n\n1. Rewrite the parser\n2. Patch the existing one\n\nWhich do you want?", true),
            ("Done. What about the docs? Want me to update them?", true),
            ("Should I run it?", true),
            ("Pushed. Want me to land it?", true),
            ("What about the docs?", true),
            ("Found it. Keep the cache or drop it?", true),
            // Fires: option lines and a question near the end.
            ("Two options:\n1) Keep the cache\n2) Drop it\nWhat should I do? Tell me.", true),
            ("Options:\n**A.** Keep the cache\n**B.** Drop it\n\nWhich one fits?", true),
            ("Pick one:\nA. Keep it\nB. Drop it\nThe second is simpler, but which?\n", true),
            // Fires: the last paragraph asks the user a decision in the middle of the paragraph.
            ("Done with the refactor.\n\nWant me to land it? It's based on the current `main`.", true),
            ("Two paths are open.\n\nKeep the cache or drop it? Say which.", true),
            ("Tests are green. Should I also update the docs? Nothing else is pending.", true),
            ("The fix is in.\n\nFirst paragraph.\n\nThen: should we ship it? Tell me.", true),
            // Does not fire: empty, very short, or no question.
            ("", false),
            ("   \n", false),
            ("?", false),
            ("OK?", false),
            ("Done.", false),
            ("The build is green. Nothing else to do.", false),
            // Does not fire: explanations, answered questions.
            ("Why is this slow? Because the lock is held. The fix is in place.", false),
            (
                "Why does the cache miss? Mostly because the keys include the timestamp. I changed that to a content hash, so rebuilds hit.",
                false,
            ),
            ("Is the cache stale? I checked: it is. Fixed in lib.rs.", false),
            ("Did the tests pass? Yes, all 412 of them.", false),
            ("Steps:\n1. Build\n2. Test\n3. Commit\nAll done.", false),
            ("1. Keep it\n2. Drop it\nLet me know.", false),
            ("Two options: 1. A 2. B", false),
            // Does not fire: code, quotes, URLs and blockquotes.
            ("The build is green.\n```rust\nprintln!(\"why?\");\n```", false),
            ("Here is the list:\n```\n1. a\n2. b\nwhich one?", false),
            ("```\n1. a\n2. b\n```\nDone.", false),
            ("The spec says \"should we keep the cache?\"", false),
            ("The spec says \"should we keep the cache?\" Then it is settled.", false),
            ("The probe is `is_ready(you?)` and it works.", false),
            ("The build is green.\n\nIs `cfg.mu.ask.nudge` on? It is, by default.", false),
            ("The docs are at https://example.com/a?b=1 and the build is green.", false),
            ("See https://example.com/x? for details.\n\nAll done here, nothing else to do.", false),
            ("Summary:\n> Should we keep it?", false),
            ("The user wrote \"Should I?\"\n", false),
            // Does not fire: greetings and short open invitations.
            ("Beep! I'm here. What do you want to work on in `/home/jacob/code/private`?", false),
            ("Hi! What would you like to work on?", false),
            ("How can I help?", false),
            ("Anything else?", false),
            (
                "Hi! What would you like to work on? I'm running in `/tmp`, so point me at a repo or file, or describe the task.",
                false,
            ),
            (
                "Done with the port. The tests pass and nothing else is pending.\n\nWhat would you like to do next? I can take any of the open items, or describe something new.",
                false,
            ),
        ];
        for (text, want) in cases {
            assert_eq!(asks_in_prose(text), *want, "{text:?}");
        }
    }

    #[test]
    fn options_need_a_question_close_to_the_end() {
        let filler = "x".repeat(400);
        let far = format!("1. Keep it\n2. Drop it\nWhich?\n{filler}");
        assert!(!asks_in_prose(&far), "the question is more than 300 characters from the end");
        let near = format!("1. Keep it\n2. Drop it\nWhich?\n{filler}.");
        assert!(!asks_in_prose(&near), "no `?` in the last 300 characters");
    }

    #[test]
    fn open_fence_suppresses_everything() {
        assert!(!asks_in_prose("Would you like this?\n```\nfn f() {}"));
        assert!(asks_in_prose("```\nfn f() {}\n```\nWould you like this?"));
    }

    #[test]
    fn reminder_is_recognised_exactly() {
        assert!(is_reminder(REMINDER));
        assert!(is_reminder(&format!("{REMINDER}\n")));
        assert!(!is_reminder("<system-reminder>\nYou stopped with 1 open todo item(s)."));
    }

    #[test]
    fn tail_counts_characters() {
        assert_eq!(tail("héllo", 3), "llo");
        assert_eq!(tail("ab", 5), "ab");
    }

    /// The rules before the last-paragraph and open-invitation changes, for the before/after counts.
    fn legacy_asks_in_prose(text: &str) -> bool {
        let Some(prose) = prose(text) else { return false };
        let prose = prose.trim();
        if prose.chars().count() < MIN_CHARS {
            return false;
        }
        if prose.trim_end_matches(|c: char| c.is_whitespace() || matches!(c, '*' | '_' | '`')).ends_with('?') {
            return true;
        }
        option_lines(tail(prose, OPTION_WINDOW)) >= 2 && tail(prose, QUESTION_WINDOW).contains('?')
    }

    /// Dev-only: runs the detector over the user's own transcripts and prints what it fires on,
    /// before (legacy rules) and after the current rules.
    /// `cargo test ask_nudge::tests::transcripts -- --ignored --nocapture`
    #[test]
    #[ignore = "reads ~/.mu and ~/.omp transcripts; run by hand"]
    fn transcripts() {
        let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
        let roots = [home.join(".omp/profiles/private/agent/sessions"), home.join(".mu/sessions")];
        let mut finals: Vec<String> = Vec::new();
        for root in roots {
            for dir in entries(&root) {
                for file in entries(&dir) {
                    if file.extension().is_some_and(|e| e == "jsonl") {
                        finals.extend(turn_finals(&file));
                    }
                }
            }
        }
        let texts: Vec<&str> = finals.iter().map(|t| t.trim_end()).collect();
        let fired_before = |t: &str| legacy_asks_in_prose(t);
        let fired_after = |t: &str| asks_in_prose(t);
        let newly_fired: Vec<&str> = texts.iter().copied().filter(|t| fired_after(t) && !fired_before(t)).collect();
        let both: Vec<&str> = texts.iter().copied().filter(|t| fired_after(t) && fired_before(t)).collect();
        let newly_skipped: Vec<&str> = texts.iter().copied().filter(|t| fired_before(t) && !fired_after(t)).collect();
        let quiet: Vec<&str> =
            texts.iter().copied().filter(|t| !fired_after(t) && tail(t, QUESTION_WINDOW).contains('?')).collect();
        let seed: u64 = 20_261_029;
        println!("turn-final assistant messages: {}", finals.len());
        println!("fired before: {}", texts.iter().filter(|t| fired_before(t)).count());
        println!("fired after: {}", texts.iter().filter(|t| fired_after(t)).count());
        println!("newly fired: {}", newly_fired.len());
        println!("newly skipped: {}", newly_skipped.len());
        println!("not fired after, with `?` in the last 300 chars: {}", quiet.len());
        let before = texts.iter().filter(|t| !fired_before(t) && tail(t, QUESTION_WINDOW).contains('?')).count();
        println!("not fired before, with `?` in the last 300 chars: {before}");
        println!("sample seed: {seed}");
        for (label, pool, n) in [
            ("NEWLY FIRED", &newly_fired, 25),
            ("FIRED BOTH", &both, 25),
            ("NEWLY SKIPPED", &newly_skipped, 15),
            ("NOT FIRED", &quiet, 25),
        ] {
            for t in sample(pool, n, seed) {
                println!("[{label}] ...{:?}", tail(t, 200));
            }
        }
    }

    fn entries(dir: &Path) -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> =
            std::fs::read_dir(dir).into_iter().flatten().filter_map(|e| e.ok()).map(|e| e.path()).collect();
        v.sort();
        v
    }

    /// Text of each turn-final assistant message in a session file: the last assistant message with
    /// visible text before the next user message (or the end of the file).
    fn turn_finals(path: &Path) -> Vec<String> {
        let Ok(text) = std::fs::read_to_string(path) else { return vec![] };
        let mut out = Vec::new();
        let mut last: Option<String> = None;
        for line in text.lines() {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
            if v["type"] != "message" {
                continue;
            }
            match v["message"]["role"].as_str() {
                Some("assistant") => {
                    let parts: Vec<&str> = v["message"]["content"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter(|b| b["type"] == "text")
                        .filter_map(|b| b["text"].as_str())
                        .collect();
                    let t = parts.join("\n");
                    if !t.trim().is_empty() {
                        last = Some(t);
                    }
                }
                Some("user") => out.extend(last.take()),
                _ => {}
            }
        }
        out.extend(last);
        out
    }

    /// Up to `n` items of `pool` in a seeded random order (xorshift).
    fn sample<'a>(pool: &[&'a str], n: usize, mut seed: u64) -> Vec<&'a str> {
        let mut idx: Vec<usize> = (0..pool.len()).collect();
        for i in (1..idx.len()).rev() {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            idx.swap(i, (seed % (i as u64 + 1)) as usize);
        }
        idx.into_iter().take(n).map(|i| pool[i]).collect()
    }
}
