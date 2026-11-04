//! A stored conversation as transcript lines, for the jobs panel's view of a subagent's session. It
//! reuses the live transcript's blocks (prompt band, markdown, tool results), so a subagent reads
//! like the main session.

use serde_json::Value;

use super::blocks;
use super::line::{Line, dim, styled};
use super::markdown;
use crate::events::AgentEvent;
use crate::message::{Message, history_events};

/// `messages` drawn at `width` columns. A tool call without a result yet (the subagent is still
/// running it) shows as a call in progress.
pub fn lines(messages: &[Message], width: usize) -> Vec<Line> {
    let mut out: Vec<Line> = Vec::new();
    let mut open: Vec<(String, String, Value)> = Vec::new();
    for e in history_events(messages) {
        match e {
            AgentEvent::UserPrompt(t) => {
                out.push(Vec::new());
                out.extend(blocks::user_prompt(&t, width));
                out.push(Vec::new());
            }
            AgentEvent::Text(t) => out.extend(markdown::render(&t, width)),
            AgentEvent::BlockEnd => out.push(Vec::new()),
            AgentEvent::Thinking(_) => out.push(styled("◇ thinking", |_| dim())),
            AgentEvent::Notice(s) => out.push(styled(s, |_| dim())),
            AgentEvent::ToolStart { id, name, args } => open.push((id, name, args)),
            AgentEvent::ToolEnd { id, name, output } => {
                let args = match open.iter().position(|(i, ..)| *i == id) {
                    Some(at) => open.remove(at).2,
                    None => Value::Null,
                };
                out.extend(blocks::tool_result(&name, &args, &output, false, width));
            }
            _ => {}
        }
    }
    for (_, name, args) in open {
        out.extend(blocks::tool_streaming(&name, &args, '◌', width));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::line::{line_text, line_width};
    use serde::Deserialize;
    use serde_json::json;

    fn msg(v: Value) -> Message {
        Message::deserialize(v).unwrap()
    }

    fn user(t: &str) -> Message {
        msg(json!({"role":"user","content":t,"timestamp":1}))
    }

    fn assistant(content: Value) -> Message {
        msg(json!({"role":"assistant","content":content,"api":"a","provider":"p","model":"m",
            "usage":{"input":1,"output":1,"cacheRead":0,"cacheWrite":0,"totalTokens":2,"cost":{"total":0.0}},
            "stopReason":"stop","timestamp":2}))
    }

    fn call(id: &str, path: &str) -> Value {
        json!({"type":"toolCall","id":id,"name":"read","arguments":{"path":path}})
    }

    fn result(id: &str, text: &str) -> Message {
        msg(json!({"role":"toolResult","toolCallId":id,"toolName":"read",
            "content":[{"type":"text","text":text}],"isError":false,"timestamp":3}))
    }

    fn text(ls: &[Line]) -> String {
        ls.iter().map(line_text).collect::<Vec<_>>().join("\n")
    }

    #[test]
    fn prompt_reply_and_a_finished_tool_call_are_all_drawn() {
        let ms = vec![
            user("Find the bug"),
            assistant(json!([{"type":"text","text":"Looking."}, call("c1", "src/a.rs")])),
            result("c1", "fn main() {}"),
            assistant(json!([{"type":"text","text":"Found **it**."}])),
        ];
        let t = text(&lines(&ms, 80));
        assert!(t.contains("Find the bug"), "{t}");
        assert!(t.contains("Looking."), "{t}");
        assert!(t.contains("Read src/a.rs"), "{t}");
        assert!(t.contains("Found it."), "markdown is rendered: {t}");
    }

    #[test]
    fn a_call_without_a_result_is_shown_as_running() {
        let ms = vec![user("go"), assistant(json!([call("c1", "src/b.rs")]))];
        let t = text(&lines(&ms, 80));
        assert!(t.contains("◌") && t.contains("src/b.rs"), "{t}");
    }

    #[test]
    fn thinking_is_a_marker_not_the_text() {
        let ms = vec![assistant(json!([{"type":"thinking","thinking":"secret plans"},{"type":"text","text":"ok"}]))];
        let t = text(&lines(&ms, 80));
        assert!(t.contains("◇ thinking") && !t.contains("secret plans"), "{t}");
    }

    #[test]
    fn nothing_is_wider_than_the_width_and_empty_input_is_empty() {
        assert!(lines(&[], 40).is_empty());
        let ms = vec![user(&"word ".repeat(50)), assistant(json!([{"type":"text","text":"x ".repeat(80)}]))];
        for w in [10, 30, 80] {
            for l in lines(&ms, w) {
                assert!(line_width(&l) <= w, "width {w}: {:?}", line_text(&l));
            }
        }
    }
}
