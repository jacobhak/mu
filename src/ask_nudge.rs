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
const OPTION_WINDOW: usize = 800;
const QUESTION_WINDOW: usize = 300;

/// Whether a final assistant message ends by asking the user something in prose: it ends with a
/// `?`, or it ends with at least two option lines and a `?` close to the end.
pub fn asks_in_prose(text: &str) -> bool {
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
            // Fires: option lines and a question near the end.
            ("Two options:\n1) Keep the cache\n2) Drop it\nWhat should I do? Tell me.", true),
            ("Options:\n**A.** Keep the cache\n**B.** Drop it\n\nWhich one fits?", true),
            ("Pick one:\nA. Keep it\nB. Drop it\nThe second is simpler, but which?\n", true),
            // Does not fire: empty, very short, or no question.
            ("", false),
            ("   \n", false),
            ("?", false),
            ("OK?", false),
            ("Done.", false),
            ("The build is green. Nothing else to do.", false),
            ("Why is this slow? Because the lock is held. The fix is in place.", false),
            ("Is the cache stale? I checked: it is. Fixed in lib.rs.", false),
            ("Did the tests pass? Yes, all 412 of them.", false),
            ("Steps:\n1. Build\n2. Test\n3. Commit\nAll done.", false),
            ("1. Keep it\n2. Drop it\nLet me know.", false),
            ("Two options: 1. A 2. B", false),
            // Does not fire: code, quotes and blockquotes.
            ("The build is green.\n```rust\nprintln!(\"why?\");\n```", false),
            ("Here is the list:\n```\n1. a\n2. b\nwhich one?", false),
            ("```\n1. a\n2. b\n```\nDone.", false),
            ("The spec says \"should we keep the cache?\"", false),
            ("Summary:\n> Should we keep it?", false),
            ("The user wrote \"Should I?\"\n", false),
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

    /// Dev-only: runs the detector over the user's own transcripts and prints what it fires on.
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
        let fired: Vec<&str> = finals.iter().map(|t| t.trim_end()).filter(|t| asks_in_prose(t)).collect();
        let near: Vec<&str> = finals
            .iter()
            .map(|t| t.trim_end())
            .filter(|t| !asks_in_prose(t) && tail(t, QUESTION_WINDOW).contains('?'))
            .collect();
        let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos() as u64 | 1;
        println!("turn-final assistant messages: {}", finals.len());
        println!("fired: {}", fired.len());
        println!("not fired, with `?` in the last 300 chars: {}", near.len());
        println!("sample seed: {seed}");
        for (label, pool) in [("FIRED", &fired), ("NOT FIRED", &near)] {
            for t in sample(pool, 25, seed) {
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
