//! Transcript blocks: user prompts, tool headers and framed tool cards, thinking markers, subagent rows.
use super::line::{self, Line, fill, plain, span, truncate, wrap};
use super::markdown;
use super::theme;
use crate::events::{SubagentState, SubagentStatus};
use crate::tools::ToolOutput;
use crossterm::style::{Attribute, Color, ContentStyle};
use serde_json::Value;

const BASH_TAIL: usize = 10;
const STREAM_TAIL: usize = 12;
const CMD_CUT: usize = 12;
const DIFF_CUT: usize = 40;
const TEXT_CUT: usize = 10;
const ERR_LINES: usize = 10;
const DEFAULT_TIMEOUT_SECS: f64 = 300.0;

fn tfg(key: &str) -> ContentStyle {
    theme::get().fg(key)
}

fn tbg(key: &str) -> Option<Color> {
    theme::get().color(key)
}

fn with_bg(mut s: ContentStyle, bg: Option<Color>) -> ContentStyle {
    s.background_color = bg;
    s
}

fn bold(mut s: ContentStyle) -> ContentStyle {
    s.attributes.set(Attribute::Bold);
    s
}

fn trunc(s: &str, n: usize) -> String {
    if s.chars().count() > n { s.chars().take(n.saturating_sub(1)).chain(['…']).collect() } else { s.into() }
}

fn more(n: usize) -> Line {
    vec![span(format!("… {n} more lines"), tfg("toolOutput"))]
}

/// Whole-line text in one style; tabs expanded so cell widths stay predictable.
fn styled_text(l: &str, style: ContentStyle) -> Line {
    vec![span(l.replace('\t', "   "), style)]
}

/// `user` block: a full-width `userMessageBg` band with one padding row above and below.
pub fn user_prompt(text: &str, width: usize) -> Vec<Line> {
    let bg = tbg("userMessageBg");
    let inner = width.saturating_sub(2).max(1);
    let mut out = vec![fill(&vec![], width, bg)];
    for l in text.lines() {
        for w in wrap(&plain(l.replace('\t', "   ")), inner) {
            let mut row = vec![span(" ", line::bg(bg))];
            row.extend(w);
            out.push(fill(&row, width, bg));
        }
    }
    out.push(fill(&vec![], width, bg));
    out
}

fn name_style() -> ContentStyle {
    bold(tfg("toolTitle"))
}

/// Header target: edit's first input line is `[path#tag]`, shown as the bare path.
fn summary(name: &str, args: &Value) -> String {
    if name == "edit" {
        let first = args["input"].as_str().and_then(|s| s.lines().next()).unwrap_or("");
        let path = first.strip_prefix('[').unwrap_or(first).split(['#', ']']).next().unwrap_or("").trim();
        if !path.is_empty() {
            return path.to_string();
        }
    }
    crate::tools::summary(name, args)
}

fn header_line(icon: &str, icon_style: ContentStyle, name: &str, args: &Value, width: usize) -> Line {
    let summary = summary(name, args);
    let mut l = vec![span(format!("{icon} "), icon_style), span(name.to_string(), name_style())];
    if !summary.is_empty() {
        l.push(span(format!(" {summary}"), tfg("toolTitle")));
    }
    truncate(&l, width)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tone {
    Pending,
    Success,
    Error,
}

impl Tone {
    fn bg(self) -> Option<Color> {
        tbg(match self {
            Tone::Pending => "toolPendingBg",
            Tone::Success => "toolSuccessBg",
            Tone::Error => "toolErrorBg",
        })
    }

    fn border(self) -> ContentStyle {
        tfg(match self {
            Tone::Pending => "accent",
            Tone::Success => "dim",
            Tone::Error => "error",
        })
    }
}

/// One body section of a card. A labelled section draws a titled separator above it.
struct Section {
    label: Option<&'static str>,
    rows: Vec<Line>,
}

struct Card {
    tone: Tone,
    sections: Vec<Section>,
    footer: Option<String>,
}

/// Rounded frame with `│` sides, a full-width background, and footer facts on the bottom rule.
fn frame(card: &Card, width: usize) -> Vec<Line> {
    let w = width.max(6);
    let cw = w - 4;
    let bg = card.tone.bg();
    let bd = with_bg(card.tone.border(), bg);
    let rule = |l: char, r: char, label: Option<(&str, ContentStyle)>| -> Line {
        let mut out = vec![span(format!("{l}───"), bd)];
        let mut used = 5;
        if let Some((text, style)) = label {
            let t = trunc(text, w.saturating_sub(used + 2));
            used += t.chars().count() + 2;
            out.push(span(format!(" {t} "), with_bg(style, bg)));
        }
        out.push(span("─".repeat(w.saturating_sub(used)), bd));
        out.push(span(r, bd));
        fill(&out, w, bg)
    };
    let mut out = vec![rule('╭', '╮', None)];
    let mut first = true;
    for s in &card.sections {
        if s.rows.is_empty() && s.label.is_none() {
            continue;
        }
        if let Some(label) = s.label {
            out.push(rule('├', '┤', Some((label, with_bg(bold(tfg("toolTitle")), bg)))));
        } else if !first {
            out.push(rule('├', '┤', None));
        }
        first = false;
        for row in &s.rows {
            for w_line in wrap(row, cw) {
                let mut l = vec![span("│ ", bd)];
                l.extend(fill(&w_line, cw, bg));
                l.push(span(" │", bd));
                out.push(l);
            }
        }
    }
    out.push(rule('╰', '╯', card.footer.as_deref().map(|f| (f, bd))));
    out
}

/// Timeout footer part from the call's `timeout` argument (seconds; 0 disables it).
fn timeout_part(args: &Value) -> String {
    match args["timeout"].as_f64() {
        Some(t) if t == 0.0 => "Timeout: disabled".into(),
        Some(t) if t.fract() == 0.0 => format!("Timeout: {}s", t as u64),
        Some(t) => format!("Timeout: {t}s"),
        None => format!("Timeout: {}s", DEFAULT_TIMEOUT_SECS as u64),
    }
}

/// Splits a bash result into body lines and footer facts. Trailer notices are removed from the body.
fn bash_trailer(text: &str) -> (Vec<&str>, Vec<String>, bool) {
    let mut body = Vec::new();
    let mut facts = Vec::new();
    let mut failed = false;
    for l in text.lines() {
        if let Some(rest) = l.strip_prefix("Wall time: ") {
            facts.push(format!("Wall: {}s", rest.trim_end_matches(" seconds").trim()));
        } else if let Some(code) = l.strip_prefix("Command exited with code ") {
            failed |= code.trim() != "0";
            if code.trim() != "0" {
                facts.push(format!("Exit: {}", code.trim()));
            }
        } else if l.starts_with("[Command timed out") {
            facts.push("timed out".into());
            failed = true;
        } else if l.contains("[Command cancelled]") {
            facts.push("cancelled".into());
            failed = true;
        } else if let Some(rest) = l.strip_prefix("[full output: ") {
            facts.push(rest.trim_end_matches(']').to_string());
        } else if l != "(no output)" {
            body.push(l);
        }
    }
    while body.last().is_some_and(|l| l.trim().is_empty()) {
        body.pop();
    }
    (body, facts, failed)
}

/// Bash box: `$ command`, an `Output` section (last `BASH_TAIL` lines unless expanded), and the footer facts.
fn bash_box(args: &Value, output: &[&str], expanded: bool, tone: Tone, mut footer: Vec<String>, width: usize) -> Vec<Line> {
    let cmd_lines: Vec<&str> = args["command"].as_str().unwrap_or("").lines().collect();
    let dim = tfg("toolOutput");
    let mut cmd_rows: Vec<Line> = cmd_lines
        .iter()
        .take(CMD_CUT)
        .enumerate()
        .map(|(i, l)| vec![span(if i == 0 { "$ " } else { "  " }, dim), span(l.replace('\t', "   "), tfg("mdCode"))])
        .collect();
    if cmd_lines.len() > CMD_CUT {
        cmd_rows.push(more(cmd_lines.len() - CMD_CUT));
    }
    if cmd_rows.is_empty() {
        cmd_rows.push(vec![span("$ ", dim)]);
    }
    let mut sections = vec![Section { label: None, rows: cmd_rows }];
    if !output.is_empty() {
        let skip = if expanded { 0 } else { output.len().saturating_sub(BASH_TAIL) };
        let mut rows = Vec::new();
        if skip > 0 {
            rows.push(more(skip));
        }
        rows.extend(output[skip..].iter().map(|l| styled_text(l, dim)));
        sections.push(Section { label: Some("Output"), rows });
    }
    let at = footer.iter().position(|f| f.starts_with("Wall:") || f.starts_with("running")).map_or(0, |i| i + 1);
    footer.insert(at, timeout_part(args));
    frame(&Card { tone, sections, footer: Some(footer.join(" · ")) }, width)
}

/// Lines of a `+`/`-`/`@@` unified diff, with the add/remove counts.
fn diff_rows(diff: &str, expanded: bool) -> (Vec<Line>, usize, usize) {
    let all: Vec<&str> = diff.lines().collect();
    let (mut adds, mut dels) = (0, 0);
    for l in &all {
        if l.starts_with('+') && !l.starts_with("+++") {
            adds += 1;
        } else if l.starts_with('-') && !l.starts_with("---") {
            dels += 1;
        }
    }
    let shown = if expanded { all.len() } else { all.len().min(DIFF_CUT) };
    let mut rows: Vec<Line> = all[..shown]
        .iter()
        .map(|l| {
            let key = match l.chars().next() {
                Some('+') => "toolDiffAdded",
                Some('-') => "toolDiffRemoved",
                Some('@') => "toolOutput",
                _ => "toolDiffContext",
            };
            styled_text(l, tfg(key))
        })
        .collect();
    if shown < all.len() {
        rows.push(more(all.len() - shown));
    }
    (rows, adds, dels)
}

/// Plain indented result rows, cut at `TEXT_CUT` unless expanded.
fn text_rows(text: &str, expanded: bool, style: impl Fn(&str) -> ContentStyle) -> Vec<Line> {
    let lines: Vec<&str> = text.lines().collect();
    let shown = if expanded { lines.len() } else { lines.len().min(TEXT_CUT) };
    let mut rows: Vec<Line> = lines[..shown].iter().map(|l| styled_text(&format!("  {l}"), style(l))).collect();
    if shown < lines.len() {
        rows.push(vec![span(format!("  … {} more lines", lines.len() - shown), tfg("toolOutput"))]);
    }
    rows
}

fn error_rows(text: &str) -> Vec<Line> {
    let lines: Vec<&str> = text.lines().collect();
    let mut rows: Vec<Line> = lines[..lines.len().min(ERR_LINES)].iter().map(|l| styled_text(l, tfg("error"))).collect();
    if lines.len() > ERR_LINES {
        rows.push(more(lines.len() - ERR_LINES));
    }
    rows
}

/// Committed result of one tool call: header plus whatever box or summary the tool uses.
pub fn tool_result(name: &str, args: &Value, out: &ToolOutput, expanded: bool, width: usize) -> Vec<Line> {
    if name == "bash" {
        let (body, facts, failed) = bash_trailer(&out.text);
        let tone = if out.is_error || failed { Tone::Error } else { Tone::Success };
        return bash_box(args, &body, expanded, tone, facts, width);
    }
    let icon_style = if out.is_error { tfg("error") } else { ContentStyle::default() };
    let mut lines = vec![header_line("●", icon_style, name, args, width)];
    if out.is_error {
        let card = Card { tone: Tone::Error, sections: vec![Section { label: None, rows: error_rows(&out.text) }], footer: None };
        lines.extend(frame(&card, width));
        return lines;
    }
    match name {
        "write" if args["path"].as_str().is_some_and(|p| p.starts_with("local://") && p.ends_with("-plan.md")) => {
            let n = args["content"].as_str().map_or(0, |c| c.lines().count());
            lines[0].push(span(format!(" · {n} lines"), tfg("toolOutput")));
            lines[0] = truncate(&lines[0], width);
        }
        "edit" | "write" if out.display.is_some() => {
            let (rows, adds, dels) = diff_rows(out.display.as_deref().unwrap_or(""), expanded);
            let card = Card {
                tone: Tone::Success,
                sections: vec![Section { label: None, rows }],
                footer: Some(format!("+{adds} -{dels}")),
            };
            lines.extend(frame(&card, width));
        }
        "read" => {
            let n = out.text.lines().count().saturating_sub(1);
            lines.push(vec![span(format!("  {n} lines"), tfg("toolOutput"))]);
        }
        "grep" => lines.push(vec![span(format!("  {} matches", out.text.lines().count()), tfg("toolOutput"))]),
        "glob" => lines.push(vec![span(format!("  {} files", out.text.lines().count()), tfg("toolOutput"))]),
        "task" => lines.extend(text_rows(&out.text, expanded, |l| if l.starts_with("## ") { name_style() } else { tfg("toolOutput") })),
        _ => lines.extend(text_rows(&out.text, expanded, |_| tfg("toolOutput"))),
    }
    lines
}

/// Viewport rows for a running tool: the bash box with its streaming tail, or a spinner header.
pub fn tool_live(name: &str, args: &Value, tail: &[String], spin: char, elapsed: u64, width: usize) -> Vec<Line> {
    if name == "bash" {
        let body: Vec<&str> = tail.iter().map(String::as_str).collect();
        let footer = vec![format!("running {elapsed}s")];
        return bash_box(args, &body, false, Tone::Pending, footer, width);
    }
    vec![header_line(&spin.to_string(), tfg("accent"), name, args, width)]
}

/// Subagent rows: state mark, name, tool count, and the last activity.
pub fn subagent_rows(rows: &[SubagentStatus], spin: char) -> Vec<Line> {
    rows.iter()
        .map(|r| {
            let (mark, style) = match r.state {
                SubagentState::Running => (spin, tfg("accent")),
                SubagentState::Done => ('✓', tfg("success")),
                SubagentState::Failed => ('✗', tfg("error")),
            };
            vec![
                span(format!("  {mark} "), style),
                span(format!("{} · {} tools · {}", r.name, r.tools, trunc(&r.last, 80)), tfg("toolOutput")),
            ]
        })
        .collect()
}

/// Live thinking marker in the viewport.
pub fn thinking_live(spin: char, secs: u64) -> Line {
    vec![span(format!("{spin} Thinking… {secs}s"), tfg("thinkingText"))]
}

/// Committed marker after a thinking block.
pub fn thinking_done(secs: u64) -> Line {
    vec![span(format!("◇ Thought for {secs}s"), tfg("thinkingText"))]
}

/// Thinking text rendered as markdown in the thinking colour, indented.
pub fn thinking_body(text: &str, width: usize) -> Vec<Line> {
    let style = tfg("thinkingText");
    markdown::render(text, width.saturating_sub(2))
        .into_iter()
        .map(|l| {
            let mut out = vec![span("  ", ContentStyle::default())];
            out.extend(l.into_iter().map(|mut s| {
                s.style.foreground_color = style.foreground_color;
                s.style.attributes.set(Attribute::Italic);
                s
            }));
            out
        })
        .collect()
}

/// Viewport rows for a tool call whose arguments are still streaming.
pub fn tool_streaming(name: &str, args: &Value, spin: char, width: usize) -> Vec<Line> {
    if name == "bash" {
        return bash_box(args, &[], false, Tone::Pending, vec![], width);
    }
    let mut out = vec![header_line(&spin.to_string(), tfg("accent"), name, args, width)];
    let (body, footer) = match name {
        "write" => (args["content"].as_str(), true),
        "edit" => (args["input"].as_str(), false),
        _ => (None, false),
    };
    let Some(body) = body.filter(|b| !b.is_empty()) else { return out };
    let lines: Vec<&str> = body.lines().collect();
    let skip = lines.len().saturating_sub(STREAM_TAIL);
    let mut rows = Vec::new();
    if skip > 0 {
        rows.push(more(skip));
    }
    rows.extend(lines[skip..].iter().map(|l| {
        let style = if name == "edit" {
            if l.starts_with('+') {
                tfg("toolDiffAdded")
            } else if l.starts_with("PUT") || l.starts_with("CUT") || l.starts_with('[') {
                tfg("toolTitle")
            } else {
                tfg("toolOutput")
            }
        } else {
            tfg("toolOutput")
        };
        styled_text(l, style)
    }));
    let footer = footer.then(|| format!("{} lines", lines.len()));
    out.extend(frame(&Card { tone: Tone::Pending, sections: vec![Section { label: None, rows }], footer }, width));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::line::line_text as text_of;
    use serde_json::json;

    #[test]
    fn streaming_write_tail() {
        let content: String = (1..=20).map(|i| format!("{i}\n")).collect();
        let t: Vec<String> = tool_streaming("write", &json!({"path": "/tmp/x", "content": content}), '⠋', 60).iter().map(text_of).collect();
        assert!(t[0].contains("write"));
        assert!(t.iter().any(|l| l.contains("… 8 more lines")));
        assert!(t.iter().any(|l| l.contains(" 9 ")));
        assert!(t.iter().any(|l| l.contains(" 20 ")));
        assert!(t.iter().any(|l| l.contains("20 lines")));
        assert!(!t.iter().any(|l| l.trim_matches(|c: char| !c.is_alphanumeric()) == "8"));
    }

    #[test]
    fn streaming_bash_shows_command() {
        let t: Vec<String> = tool_streaming("bash", &json!({"command": "echo hi"}), '⠋', 60).iter().map(text_of).collect();
        assert!(t.iter().any(|l| l.contains("$ echo hi")));
    }

    fn texts(lines: &[Line]) -> Vec<String> {
        lines.iter().map(text_of).collect()
    }

    #[test]
    fn plan_write_is_one_line() {
        let mut out = ToolOutput::ok("ok");
        out.display = Some("+a\n+b\n+c".into());
        let args = json!({"path": "local://x-plan.md", "content": "a\nb\nc\n"});
        assert_eq!(texts(&tool_result("write", &args, &out, false, 60)), ["● write local://x-plan.md · 3 lines"]);
    }

    #[test]
    fn bash_box_tails_and_footer() {
        let body: String = (1..=30).map(|i| format!("row{i}\n")).collect();
        let out = ToolOutput::ok(format!("{body}\nWall time: 2.0 seconds\n\nCommand exited with code 0"));
        let args = json!({"command": "seq 30", "timeout": 60});
        let t = texts(&tool_result("bash", &args, &out, false, 40));
        assert_eq!(t.len(), 15);
        assert!(t[0].starts_with('╭') && t[0].ends_with('╮'));
        assert!(t[1].starts_with("│ $ seq 30") && t[1].ends_with(" │"));
        assert!(t[2].starts_with("├─── Output ") && t[2].ends_with('┤'));
        assert!(t[3].starts_with("│ … 20 more lines"));
        assert!(t[4].starts_with("│ row21") && t[13].starts_with("│ row30"));
        assert!(t[14].starts_with("╰─── Wall: 2.0s · Timeout: 60s ") && t[14].ends_with('╯'), "{}", t[14]);
        assert!(t.iter().all(|l| unicode_width_of(l) == 40), "every row is full width");
        assert_eq!(tool_result("bash", &args, &out, true, 40).len(), 1 + 1 + 1 + 30 + 1);
    }

    #[test]
    fn bash_error_box_uses_error_tone_and_exit_fact() {
        let out = ToolOutput::err("nope\n\nCommand exited with code 3");
        let t = texts(&tool_result("bash", &json!({"command": "false"}), &out, false, 30));
        assert!(t.last().unwrap().contains("Exit: 3"), "{t:?}");
        assert!(!t.iter().any(|l| l.contains("Command exited")), "trailer moved into footer");
    }

    #[test]
    fn diff_colors_and_cut() {
        let d: String = (0..50).map(|i| format!("+l{i}\n")).collect();
        let mut out = ToolOutput::ok("done");
        out.display = Some(d);
        let l = tool_result("edit", &json!({"input":"x"}), &out, false, 40);
        // header, top rule, 40 diff rows, "more" row, bottom rule
        assert_eq!(l.len(), 1 + 1 + 40 + 1 + 1);
        assert!(text_of(&l[42]).starts_with("│ … 10 more lines"));
        assert_eq!(l[2][1].style.foreground_color, theme::get().color("toolDiffAdded"));
        assert!(text_of(l.last().unwrap()).contains("+50 -0"));
    }

    #[test]
    fn error_box_cuts_at_ten_lines() {
        let body: String = (0..12).map(|i| format!("e{i}\n")).collect();
        let t = texts(&tool_result("read", &json!({"path":"x"}), &ToolOutput::err(body), false, 30));
        assert_eq!(t[0], "● read x");
        assert!(t[1].starts_with('╭') && t[1].ends_with('╮'));
        assert!(t.iter().any(|l| l.contains("… 2 more lines")));
        assert!(t.last().unwrap().starts_with('╰'));
    }

    #[test]
    fn edit_header_shows_path_only() {
        let l = tool_result("edit", &json!({"input": "[src/a.rs#B6C1]\n@@"}), &ToolOutput::ok("ok"), false, 40);
        assert_eq!(text_of(&l[0]), "● edit src/a.rs");
    }

    #[test]
    fn user_prompt_is_full_width_band() {
        let l = user_prompt("hi", 12);
        assert_eq!(l.len(), 3);
        assert!(l.iter().all(|r| line::line_width(r) == 12));
        assert_eq!(text_of(&l[1]), format!(" hi{}", " ".repeat(9)));
    }

    fn unicode_width_of(l: &str) -> usize {
        use unicode_width::UnicodeWidthStr;
        l.width()
    }
}
