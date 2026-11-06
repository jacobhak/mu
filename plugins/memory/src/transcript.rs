//! What the review reads of a session: what the user typed, the assistant's text, and tool calls by name with a
//! short argument summary. Tool results, thinking and anything mu or a plugin injected (job notices,
//! reminders, `!cmd` output) stay out, so file, web and command output never reaches the memory model. A
//! slash command or skill shows as the command the user typed, not its expansion: the template's
//! instructions are not the user's words. The session's own memory edits show in full with whether they
//! were applied, so the review does not redo or undo them.

use mu_base::message::{Message, is_dismissal, is_injected, text_of, tool_calls};
use mu_session::session::format::Loaded;
use serde_json::Value;
use std::collections::HashMap;

/// Characters of transcript the review sends. The newest turns are kept.
pub const CAP: usize = 60_000;
const ARG_CHARS: usize = 120;
const OMITTED: &str = "[earlier turns omitted]\n\n";

#[derive(Debug, PartialEq)]
pub struct Transcript {
    pub text: String,
    /// Numbers of the first and last user turns in the text, counted from the session's first.
    pub first_turn: usize,
    pub last_turn: usize,
    /// The last loaded entry: the cursor moves here once the text is reviewed.
    pub last_entry: String,
}

struct Group {
    turn: usize,
    user: bool,
    text: String,
}

/// The entries after `cursor` (all of them when it is not among the loaded entries), rendered and capped
/// at `cap` characters. `None` when fewer than two user turns follow the cursor.
pub fn extract(loaded: &Loaded, cursor: Option<&str>, cap: usize) -> Option<Transcript> {
    let start = cursor.and_then(|c| loaded.entry_ids.iter().position(|id| id == c)).map_or(0, |i| i + 1);
    let mut turn = loaded.messages[..start].iter().filter(|m| is_user_turn(m)).count();
    let failed = memory_results(&loaded.messages);
    let mut user_turns = 0;
    let mut groups: Vec<Group> = Vec::new();
    for m in &loaded.messages[start..] {
        let user = is_user_turn(m);
        if user {
            turn += 1;
            user_turns += 1;
        }
        let lines = render(m, &failed);
        if lines.is_empty() {
            continue;
        }
        if user || groups.is_empty() {
            let text = if user { format!("### Turn {turn}\n") } else { String::new() };
            groups.push(Group { turn, user, text });
        }
        if let Some(g) = groups.last_mut() {
            for l in lines {
                g.text.push_str(&l);
                g.text.push('\n');
            }
            g.text.push('\n');
        }
    }
    if user_turns < 2 {
        return None;
    }
    let budget = cap.saturating_sub(OMITTED.len());
    let mut kept: Vec<Group> = Vec::new();
    let mut used = 0;
    let mut omitted = false;
    for g in groups.into_iter().rev() {
        let n = g.text.chars().count();
        if used + n > budget {
            omitted = true;
            if kept.is_empty() {
                let tail: String = g.text.chars().skip(n.saturating_sub(budget)).collect();
                kept.push(Group { turn: g.turn, user: g.user, text: tail });
            }
            break;
        }
        used += n;
        kept.push(g);
    }
    kept.reverse();
    let first_turn = kept.iter().find(|g| g.user).map_or(turn, |g| g.turn);
    let mut text = String::new();
    if omitted {
        text.push_str(OMITTED);
    }
    for g in &kept {
        text.push_str(&g.text);
    }
    Some(Transcript { text, first_turn, last_turn: turn, last_entry: loaded.entry_ids.last()?.clone() })
}

/// The line for what the user did in `m`, or `None` for a message mu or a plugin sent.
fn user_line(m: &Message) -> Option<String> {
    let Message::User { content, synthetic: false, command, .. } = m else { return None };
    if let Some(c) = command.as_deref().map(str::trim).filter(|c| !c.is_empty()) {
        return Some(format!("user ran: {c}"));
    }
    let t = text_of(content);
    let t = t.trim();
    (!t.is_empty() && !is_injected(t)).then(|| format!("user: {t}"))
}

fn is_user_turn(m: &Message) -> bool {
    user_line(m).is_some()
}

/// Whether each `memory` tool call failed, by call id.
fn memory_results(messages: &[Message]) -> HashMap<String, bool> {
    messages
        .iter()
        .filter_map(|m| match m {
            Message::ToolResult { tool_call_id, tool_name, is_error, .. } if tool_name == "memory" => {
                Some((tool_call_id.clone(), *is_error))
            }
            _ => None,
        })
        .collect()
}

fn render(m: &Message, failed: &HashMap<String, bool>) -> Vec<String> {
    match m {
        Message::User { .. } => user_line(m).into_iter().collect(),
        Message::Assistant { content, .. } if !is_dismissal(m) => {
            let mut out = Vec::new();
            let t = text_of(content);
            if !t.trim().is_empty() {
                out.push(format!("assistant: {}", t.trim()));
            }
            for (id, name, args) in tool_calls(m) {
                out.push(match memory_edit(&name, &args) {
                    Some(ops) => {
                        let status = match failed.get(&id) {
                            Some(false) => "applied: already in the files above",
                            Some(true) => "rejected: not applied",
                            None => "no result",
                        };
                        format!("memory edit by the session ({status}): {ops}")
                    }
                    None => format!("tool call: {name} {}", summary(&args.to_string())),
                });
            }
            out
        }
        _ => Vec::new(),
    }
}

/// The ops of a `memory` edit call, in full.
fn memory_edit(name: &str, args: &Value) -> Option<String> {
    (name == "memory" && args["action"] == "edit").then(|| args["ops"].to_string())
}

fn summary(args: &str) -> String {
    if args.chars().count() <= ARG_CHARS {
        return args.to_string();
    }
    let cut: String = args.chars().take(ARG_CHARS).collect();
    format!("{cut}…")
}

#[cfg(test)]
pub(crate) mod fixture {
    use serde_json::{Value, json};
    use std::path::Path;

    /// A v3 session file. `entries` are `(id, message-or-custom)` pairs in order; each entry's parent is the one before it.
    pub fn write(path: &Path, entries: &[Value]) {
        let mut lines = vec![
            json!({"type": "title", "title": "fixture"}),
            json!({"type": "session", "version": 3, "id": "fixture", "timestamp": "t", "cwd": "/"}),
        ];
        let mut parent: Option<String> = None;
        for (i, e) in entries.iter().enumerate() {
            let id = format!("e{}", i + 1);
            let mut v = e.clone();
            v["id"] = json!(id);
            v["parentId"] = json!(parent);
            parent = Some(id);
            lines.push(v);
        }
        let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
        std::fs::write(path, body).unwrap();
    }

    pub fn user(text: &str) -> Value {
        json!({"type": "message", "message": {"role": "user", "content": text, "timestamp": 1}})
    }

    pub fn assistant(blocks: Value) -> Value {
        json!({"type": "message", "message": {
            "role": "assistant", "content": blocks, "api": "x", "provider": "x", "model": "m",
            "usage": {}, "stopReason": "stop", "timestamp": 1
        }})
    }

    pub fn tool_result(text: &str) -> Value {
        json!({"type": "message", "message": {
            "role": "toolResult", "toolCallId": "c1", "toolName": "bash",
            "content": [{"type": "text", "text": text}], "isError": false, "timestamp": 1
        }})
    }

    pub fn custom_message(text: &str) -> Value {
        json!({"type": "custom_message", "content": text})
    }

    /// A prompt the TUI expanded from the slash command `command`.
    pub fn user_command(text: &str, command: &str) -> Value {
        json!({"type": "message", "message": {"role": "user", "content": text, "timestamp": 1, "command": command}})
    }

    pub fn memory_result(call: &str, is_error: bool) -> Value {
        json!({"type": "message", "message": {
            "role": "toolResult", "toolCallId": call, "toolName": "memory",
            "content": [{"type": "text", "text": "result"}], "isError": is_error, "timestamp": 1
        }})
    }
}

#[cfg(test)]
mod tests {
    use super::fixture::*;
    use super::*;
    use mu_session::session::format::load;
    use serde_json::json;

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("mu-memory-transcript-{name}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn session(name: &str, entries: &[serde_json::Value]) -> Loaded {
        let p = tmp(name).join("s.jsonl");
        write(&p, entries);
        load(&p).unwrap()
    }

    #[test]
    fn tool_results_thinking_and_injected_text_stay_out() {
        let l = session(
            "filter",
            &[
                user("Please use tabs"),
                assistant(json!([
                    {"type": "thinking", "thinking": "private musing"},
                    {"type": "text", "text": "Done, tabs it is."},
                    {"type": "toolCall", "id": "c1", "name": "bash", "arguments": {"command": "cargo fmt"}}
                ])),
                tool_result("SECRET_OUTPUT"),
                json!({"type": "message", "message": {"role": "user", "content": "plan approved", "timestamp": 1, "synthetic": true}}),
                custom_message("plugin context"),
                user("Remember the release tag rule"),
            ],
        );
        let t = extract(&l, None, CAP).unwrap();
        assert!(t.text.contains("user: Please use tabs"), "{}", t.text);
        assert!(t.text.contains("assistant: Done, tabs it is."));
        assert!(t.text.contains(r#"tool call: bash {"command":"cargo fmt"}"#));
        assert!(t.text.contains("user: Remember the release tag rule"));
        for hidden in ["SECRET_OUTPUT", "private musing", "plan approved", "plugin context", "system-reminder"] {
            assert!(!t.text.contains(hidden), "{hidden} leaked: {}", t.text);
        }
        assert_eq!((t.first_turn, t.last_turn), (1, 2));
        assert_eq!(t.last_entry, "e6");
    }

    #[test]
    fn only_entries_after_the_cursor_count() {
        let entries = [
            user("one"),
            assistant(json!([{"type": "text", "text": "a"}])),
            user("two"),
            assistant(json!([{"type": "text", "text": "b"}])),
            user("three"),
            assistant(json!([{"type": "text", "text": "c"}])),
        ];
        let l = session("cursor", &entries);
        let t = extract(&l, Some("e2"), CAP).unwrap();
        assert!(!t.text.contains("one"));
        assert!(t.text.contains("user: two") && t.text.contains("user: three"));
        assert_eq!((t.first_turn, t.last_turn), (2, 3));
        // One user turn after the cursor is below the threshold.
        assert!(extract(&l, Some("e4"), CAP).is_none());
        // A cursor that is not among the loaded entries means everything loaded.
        assert!(extract(&l, Some("gone"), CAP).unwrap().text.contains("user: one"));
    }

    #[test]
    fn fewer_than_two_user_turns_is_skipped() {
        let l = session("one", &[user("only"), assistant(json!([{"type": "text", "text": "ok"}]))]);
        assert!(extract(&l, None, CAP).is_none());
    }

    #[test]
    fn the_cap_keeps_the_newest_turns() {
        let entries: Vec<_> = (1..=30)
            .flat_map(|n| {
                let body = format!("turn-{n:02} {}", "x".repeat(3000));
                [user(&format!("u{n}")), assistant(json!([{"type": "text", "text": body}]))]
            })
            .collect();
        let l = session("cap", &entries);
        let t = extract(&l, None, CAP).unwrap();
        assert!(t.text.chars().count() <= CAP);
        assert!(t.text.contains("turn-30"));
        assert!(!t.text.contains("turn-01"));
        assert!(t.text.starts_with(OMITTED));
        assert!(t.first_turn > 1 && t.first_turn < 30);
    }

    #[test]
    fn a_newest_turn_larger_than_the_cap_keeps_its_tail() {
        let l = session(
            "tail",
            &[
                user("first"),
                assistant(json!([{"type": "text", "text": "old"}])),
                user("second"),
                assistant(json!([{"type": "text", "text": format!("{}END", "y".repeat(500))}])),
            ],
        );
        let t = extract(&l, None, 200).unwrap();
        assert!(t.text.chars().count() <= 200);
        assert!(t.text.contains("END"));
        assert_eq!(t.last_turn, 2);
    }

    #[test]
    fn long_arguments_are_cut() {
        let long = "z".repeat(400);
        let l = session(
            "args",
            &[
                user("a"),
                assistant(json!([{"type": "toolCall", "id": "c", "name": "write", "arguments": {"text": long}}])),
                user("b"),
            ],
        );
        let t = extract(&l, None, CAP).unwrap();
        let line = t.text.lines().find(|l| l.starts_with("tool call: write")).unwrap();
        assert!(line.ends_with('…') && line.chars().count() < 160, "{line}");
    }

    #[test]
    fn notices_and_command_output_are_not_user_turns() {
        let notice = "<system-notice>\nBackground job A has completed.\n## A (task) — ok\nJOB_REPORT\n</system-notice>";
        let entries = [
            user(notice),
            user("I ran `ls`:\n```\nCMD_OUTPUT\n```"),
            user("<system-reminder>\nYou stopped with 1 open todo item(s)\n</system-reminder>"),
            assistant(json!([{"type": "text", "text": "a"}])),
            user("only one"),
        ];
        assert!(extract(&session("notices", &entries), None, CAP).is_none());
        let mut entries = entries.to_vec();
        entries.push(user("second"));
        let t = extract(&session("notices2", &entries), None, CAP).unwrap();
        for hidden in ["JOB_REPORT", "CMD_OUTPUT", "open todo"] {
            assert!(!t.text.contains(hidden), "{hidden} leaked: {}", t.text);
        }
        assert_eq!((t.first_turn, t.last_turn), (1, 2));
    }

    #[test]
    fn a_slash_command_shows_as_typed_not_expanded() {
        let l = session(
            "command",
            &[
                user_command("Ship the current work to main. Never stop to confirm.", "/ship --keep"),
                assistant(json!([{"type": "text", "text": "Shipped."}])),
                user("thanks"),
            ],
        );
        let t = extract(&l, None, CAP).unwrap();
        assert!(t.text.contains("user ran: /ship --keep\n"), "{}", t.text);
        assert!(!t.text.contains("Never stop to confirm"), "{}", t.text);
        assert_eq!(t.last_turn, 2);
    }

    #[test]
    fn the_sessions_memory_edits_show_in_full_with_their_outcome() {
        let long = format!("Release flow: {}", "step ".repeat(60));
        let edit = |id: &str, text: &str| {
            json!({"type": "toolCall", "id": id, "name": "memory",
                "arguments": {"action": "edit", "ops": [{"op": "add", "file": "project", "text": text}]}})
        };
        let l = session(
            "memory-edits",
            &[
                user("remember the release flow"),
                assistant(json!([edit("m1", &long), edit("m2", "too big")])),
                memory_result("m1", false),
                memory_result("m2", true),
                assistant(json!([{"type": "toolCall", "id": "m3", "name": "memory", "arguments": {"action": "read"}}])),
                user("ok"),
            ],
        );
        let t = extract(&l, None, CAP).unwrap();
        let applied = t.text.lines().find(|l| l.contains("applied: already in the files above")).unwrap();
        assert!(applied.starts_with("memory edit by the session") && applied.contains(long.trim_end()), "{applied}");
        assert!(t.text.contains(
            r#"memory edit by the session (rejected: not applied): [{"op":"add","file":"project","text":"too big"}]"#
        ));
        assert!(t.text.contains(r#"tool call: memory {"action":"read"}"#), "{}", t.text);
    }
}
