//! Transcript blocks: user prompts, tool headers and framed tool cards, thinking markers, subagent rows.
use super::diffview;
use super::line::truncate_text as trunc;
use super::line::{self, Line, fill, span, truncate, wrap};
use super::markdown;
use super::theme::{self, Role};
use crate::config::Cards;
use crate::events::{SubagentState, SubagentStatus};
use crate::tools::{BashMeta, FileMeta, ToolMeta, ToolOutput};
use crossterm::style::{Attribute, Color, ContentStyle};
use serde_json::Value;

const BASH_TAIL: usize = 10;
const STREAM_TAIL: usize = 12;
const CMD_CUT: usize = 12;
const TEXT_CUT: usize = 10;
const ERR_LINES: usize = 10;
const DEFAULT_TIMEOUT_SECS: f64 = 300.0;

fn tfg(role: Role) -> ContentStyle {
    theme::get().fg(role)
}

fn tbg(role: Role) -> Option<Color> {
    theme::get().color(role)
}

fn with_bg(mut s: ContentStyle, bg: Option<Color>) -> ContentStyle {
    s.background_color = bg;
    s
}

fn bold(mut s: ContentStyle) -> ContentStyle {
    s.attributes.set(Attribute::Bold);
    s
}

fn more(n: usize) -> Line {
    vec![span(format!("… {n} more lines"), tfg(Role::Faint))]
}

/// Whole-line text in one style; tabs expanded so cell widths stay predictable.
fn styled_text(l: &str, style: ContentStyle) -> Line {
    vec![span(l, style)]
}

/// `user` block: a full-width lifted band, an accent bar on every row, the prompt rendered as
/// markdown, and one padding row above and below.
pub fn user_prompt(text: &str, width: usize) -> Vec<Line> {
    let bg = tbg(Role::Raised);
    let inner = width.saturating_sub(1).max(1);
    let bar = || span("▎", with_bg(tfg(Role::Accent), bg));
    let blank = || fill(&vec![bar()], width, bg);
    let mut out = vec![blank()];
    for l in markdown::render(text, inner) {
        let mut row = vec![bar()];
        row.extend(l.into_iter().map(|mut s| {
            if s.style.foreground_color.is_none() {
                s.style = theme::get().apply(Role::Strong, s.style);
            }
            s
        }));
        out.push(fill(&row, width, bg));
    }
    out.push(blank());
    out
}

fn name_style() -> ContentStyle {
    bold(tfg(Role::Strong))
}

/// Header target. A finished call shows the path its tool recorded; a call still running only has
/// the arguments, where edit's first input line is `[path#tag]` (the model's own text, not output).
fn summary(name: &str, args: &Value, meta: Option<&ToolMeta>) -> String {
    if let Some(ToolMeta::File(FileMeta { path, .. })) = meta
        && !path.is_empty()
    {
        return path.clone();
    }
    if name == "edit" {
        let first = args["input"].as_str().and_then(|s| s.lines().next()).unwrap_or("");
        let path = match first.strip_prefix("*** Edit File:") {
            Some(p) => p.trim().trim_end_matches(" all").trim_matches('"'),
            None => first.strip_prefix('[').unwrap_or(first).split(['#', ']']).next().unwrap_or("").trim(),
        };
        if !path.is_empty() {
            return path.to_string();
        }
    }
    crate::tools::summary(name, args)
}

fn header_line(
    icon: &str,
    icon_style: ContentStyle,
    name: &str,
    args: &Value,
    meta: Option<&ToolMeta>,
    width: usize,
) -> Line {
    let summary = summary(name, args, meta);
    let mut l = vec![span(format!("{icon} "), icon_style), span(name.to_string(), name_style())];
    if !summary.is_empty() {
        l.push(span(format!(" {summary}"), tfg(Role::Muted)));
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
    fn border(self) -> ContentStyle {
        tfg(match self {
            Tone::Pending => Role::AccentSoft,
            Tone::Success => Role::Line,
            Tone::Error => Role::ErrSoft,
        })
    }
}

/// Card background: the recessed surface when cards are filled, else the terminal's.
fn card_bg() -> Option<Color> {
    (theme::get().cards() == Cards::Filled).then(|| tbg(Role::Surface)).flatten()
}

/// One body section of a card. A labelled section draws a titled separator above it.
struct Section {
    label: Option<&'static str>,
    rows: Vec<Line>,
}

struct Card {
    tone: Tone,
    sections: Vec<Section>,
    footer: Option<Line>,
}

/// Rounded frame with `│` sides, an optional card fill, and footer facts on the bottom rule.
fn frame(card: &Card, width: usize) -> Vec<Line> {
    frame_at(card, width).0
}

/// `frame`, plus the index of the first row of the `output` section (`None` if there is none).
fn frame_at(card: &Card, width: usize) -> (Vec<Line>, Option<usize>) {
    let w = width.max(6);
    let cw = w - 4;
    let bg = card_bg();
    let bd = with_bg(card.tone.border(), bg);
    let rule = |l: char, r: char, label: Option<Line>| -> Line {
        let mut out = vec![span(format!("{l}─"), bd)];
        let mut used = 3;
        if let Some(label) = label {
            let t = truncate(&label, w.saturating_sub(used + 2));
            used += line::line_width(&t) + 2;
            out.push(span(" ", with_bg(ContentStyle::default(), bg)));
            out.extend(t);
            out.push(span(" ", with_bg(ContentStyle::default(), bg)));
        }
        out.push(span("─".repeat(w.saturating_sub(used)), bd));
        out.push(span(r, bd));
        fill(&out, w, bg)
    };
    let mut out = vec![rule('╭', '╮', None)];
    let mut anchor = None;
    let mut first = true;
    for s in &card.sections {
        if s.rows.is_empty() && s.label.is_none() {
            continue;
        }
        if let Some(label) = s.label {
            out.push(rule('├', '┤', Some(vec![span(label, tfg(Role::Faint))])));
        } else if !first {
            out.push(rule('├', '┤', None));
        }
        first = false;
        if s.label == Some("output") {
            anchor = Some(out.len());
        }
        for row in &s.rows {
            for w_line in wrap(row, cw) {
                let row_bg = w_line.first().and_then(|s| s.style.background_color).or(bg);
                let mut l = vec![span("│ ", bd)];
                l.extend(fill(&w_line, cw, row_bg));
                l.push(span(" │", bd));
                out.push(l);
            }
        }
    }
    out.push(rule('╰', '╯', card.footer.clone()));
    (out, anchor)
}

/// One footer fact: what it says and how loudly.
#[derive(Clone, Debug, PartialEq)]
enum Fact {
    Wall(f64),
    Exit(i32),
    TimedOut,
    Cancelled,
    Timeout(String),
    Artifact(u32),
    Running(u64),
    KillHint,
    Lines(usize),
    Note(String),
}

impl Fact {
    fn label(&self) -> String {
        match self {
            Fact::Wall(s) => format!("Wall: {s:.1}s"),
            Fact::Exit(c) => format!("Exit: {c}"),
            Fact::TimedOut => "timed out".into(),
            Fact::Cancelled => "cancelled".into(),
            Fact::Timeout(t) => t.clone(),
            Fact::Artifact(n) => format!("artifact://{n}"),
            Fact::Running(s) => format!("running {s}s"),
            Fact::KillHint => "ctrl+c twice kills".into(),
            Fact::Lines(n) => format!("{n} lines"),
            Fact::Note(n) => n.clone(),
        }
    }

    fn role(&self) -> Role {
        match self {
            Fact::Exit(_) | Fact::TimedOut => Role::Err,
            _ => Role::Faint,
        }
    }
}

/// Footer facts joined by faint dots; failures stand out.
fn facts(parts: &[Fact]) -> Line {
    let mut out = Line::new();
    for (i, p) in parts.iter().enumerate() {
        if i > 0 {
            out.push(span(" · ", tfg(Role::Line)));
        }
        out.push(span(p.label(), tfg(p.role())));
    }
    out
}

/// Timeout footer part from the call's `timeout` argument (seconds; 0 disables it). `None` when
/// it is absent or the default.
fn timeout_part(args: &Value) -> Option<Fact> {
    let t = args["timeout"].as_f64().filter(|t| *t != DEFAULT_TIMEOUT_SECS)?;
    Some(Fact::Timeout(match t {
        0.0 => "Timeout: disabled".into(),
        t if t.fract() == 0.0 => format!("Timeout: {}s", t as u64),
        t => format!("Timeout: {t}s"),
    }))
}

/// The footer facts of a finished bash call, from what the tool recorded.
fn bash_facts(m: &BashMeta) -> Vec<Fact> {
    let mut f = Vec::new();
    if let Some(n) = m.artifact {
        f.push(Fact::Artifact(n));
    }
    if let Some(n) = &m.spill_note {
        f.push(Fact::Note(n.clone()));
    }
    if let Some(w) = m.wall_secs {
        f.push(Fact::Wall(w));
    }
    if m.cancelled {
        f.push(Fact::Cancelled);
    } else if m.timed_out {
        f.push(Fact::TimedOut);
    } else if let Some(c) = m.exit_code.filter(|c| *c != 0) {
        f.push(Fact::Exit(c));
    }
    f
}

/// Bash box: `$ command`, an `Output` section (last `BASH_TAIL` lines unless expanded), and the footer facts.
fn bash_box(args: &Value, output: &[&str], expanded: bool, tone: Tone, footer: Vec<Fact>, width: usize) -> Vec<Line> {
    bash_box_at(args, output, expanded, tone, footer, width).0
}

/// `bash_box`, plus the index of the first output row among the lines (`None` when there is no output).
fn bash_box_at(
    args: &Value,
    output: &[&str],
    expanded: bool,
    tone: Tone,
    mut footer: Vec<Fact>,
    width: usize,
) -> (Vec<Line>, Option<usize>) {
    let cmd_lines: Vec<&str> = args["command"].as_str().unwrap_or("").lines().collect();
    let faint = tfg(Role::Faint);
    let mut cmd_rows: Vec<Line> = cmd_lines
        .iter()
        .take(CMD_CUT)
        .enumerate()
        .map(|(i, l)| vec![span(if i == 0 { "$ " } else { "  " }, faint), span(*l, tfg(Role::Strong))])
        .collect();
    if cmd_lines.len() > CMD_CUT {
        cmd_rows.push(more(cmd_lines.len() - CMD_CUT));
    }
    if cmd_rows.is_empty() {
        cmd_rows.push(vec![span("$ ", faint)]);
    }
    let mut sections = vec![Section { label: None, rows: cmd_rows }];
    if !output.is_empty() {
        let skip = if expanded { 0 } else { output.len().saturating_sub(BASH_TAIL) };
        let mut rows = Vec::new();
        if skip > 0 {
            rows.push(more(skip));
        }
        rows.extend(output[skip..].iter().enumerate().map(|(i, l)| {
            let role = if i == 0 && tone == Tone::Error { Role::Err } else { Role::Muted };
            styled_text(l, tfg(role))
        }));
        sections.push(Section { label: Some("output"), rows });
    }
    let at = footer.iter().position(|f| matches!(f, Fact::Wall(_) | Fact::Running(_))).map_or(0, |i| i + 1);
    if let Some(t) = timeout_part(args) {
        footer.insert(at, t);
    }
    let footer = (!footer.is_empty()).then(|| facts(&footer));
    frame_at(&Card { tone, sections, footer }, width)
}

/// Plain indented result rows, cut at `TEXT_CUT` unless expanded.
fn text_rows(text: &str, expanded: bool, style: impl Fn(&str) -> ContentStyle) -> Vec<Line> {
    let lines: Vec<&str> = text.lines().collect();
    let shown = if expanded { lines.len() } else { lines.len().min(TEXT_CUT) };
    let mut rows: Vec<Line> = lines[..shown].iter().map(|l| styled_text(&format!("  {l}"), style(l))).collect();
    if shown < lines.len() {
        rows.push(vec![span(format!("  … {} more lines", lines.len() - shown), tfg(Role::Faint))]);
    }
    rows
}

fn error_rows(text: &str) -> Vec<Line> {
    let lines: Vec<&str> = text.lines().collect();
    let mut rows: Vec<Line> = lines[..lines.len().min(ERR_LINES)]
        .iter()
        .enumerate()
        .map(|(i, l)| styled_text(l, tfg(if i == 0 { Role::Err } else { Role::Muted })))
        .collect();
    if lines.len() > ERR_LINES {
        rows.push(more(lines.len() - ERR_LINES));
    }
    rows
}

/// Committed result of one tool call: header plus whatever box or summary the tool uses.
pub fn tool_result(name: &str, args: &Value, out: &ToolOutput, expanded: bool, width: usize) -> Vec<Line> {
    if name == "bash"
        && let Some(ToolMeta::Bash(m)) = &out.meta
    {
        // The command's own output ends where the tool said; nothing after it is parsed.
        let end = m.output_end.min(out.text.len());
        let mut body: Vec<&str> = out.text.get(..end).unwrap_or_default().lines().collect();
        while body.last().is_some_and(|l| l.trim().is_empty()) {
            body.pop();
        }
        let tone = if out.is_error || m.failed() { Tone::Error } else { Tone::Success };
        return bash_box(args, &body, expanded, tone, bash_facts(m), width);
    }
    let icon_style = if out.is_error { tfg(Role::Err) } else { tfg(Role::Faint) };
    let mut lines = vec![header_line("●", icon_style, name, args, out.meta.as_ref(), width)];
    if out.is_error {
        let card = Card {
            tone: Tone::Error,
            sections: vec![Section { label: None, rows: error_rows(&out.text) }],
            footer: None,
        };
        lines.extend(frame(&card, width));
        return lines;
    }
    match name {
        "write" if matches!(&out.meta, Some(ToolMeta::File(FileMeta { plan: true, .. }))) => {
            let n = args["content"].as_str().map_or(0, |c| c.lines().count());
            lines[0].push(span(format!(" · {n} lines"), tfg(Role::Faint)));
            lines[0] = truncate(&lines[0], width);
        }
        "edit" | "write" if out.display.is_some() => {
            let path = summary(name, args, out.meta.as_ref());
            let lang = std::path::Path::new(&path).extension().and_then(|e| e.to_str()).unwrap_or("");
            let (rows, adds, dels) = diffview::rows(out.display.as_deref().unwrap_or(""), lang, expanded);
            let card = Card {
                tone: Tone::Success,
                sections: vec![Section { label: None, rows }],
                footer: Some(vec![
                    span(format!("+{adds}"), tfg(Role::Ok)),
                    span(" ", ContentStyle::default()),
                    span(format!("-{dels}"), tfg(Role::Err)),
                ]),
            };
            lines.extend(frame(&card, width));
        }
        "read" => {
            // An image read is one line: `Image: path (mime, n bytes)`.
            let what = match out.text.strip_prefix("Image: ").and_then(|t| t.rsplit_once(" (")) {
                Some((_, kind)) => format!("  image ({kind}"),
                None => format!("  {} lines", out.text.lines().count().saturating_sub(1)),
            };
            lines.push(vec![span(what, tfg(Role::Muted))]);
        }
        "grep" => lines.push(vec![span(format!("  {} matches", out.text.lines().count()), tfg(Role::Muted))]),
        "glob" => lines.push(vec![span(format!("  {} files", out.text.lines().count()), tfg(Role::Muted))]),
        "task" => lines.extend(text_rows(&out.text, expanded, |l| {
            if l.starts_with("## ") { name_style() } else { tfg(Role::Muted) }
        })),
        _ => lines.extend(text_rows(&out.text, expanded, |_| tfg(Role::Muted))),
    }
    lines
}

/// Viewport rows for a running tool: the bash box with its streaming tail, or a spinner header.
pub fn tool_live(name: &str, args: &Value, tail: &[String], spin: char, elapsed: u64, width: usize) -> Vec<Line> {
    if name == "bash" {
        let body: Vec<&str> = tail.iter().map(String::as_str).collect();
        let footer = vec![Fact::Running(elapsed)];
        return bash_box(args, &body, false, Tone::Pending, footer, width);
    }
    vec![header_line(&spin.to_string(), tfg(Role::Accent), name, args, None, width)]
}

/// Live bash card around an embedded terminal screen. Returns the lines and the cursor `(row, col)` within them.
pub fn pty_live(
    args: &Value,
    rows: &[String],
    cursor: (usize, usize),
    elapsed: u64,
    width: usize,
) -> (Vec<Line>, Option<(usize, usize)>) {
    let body: Vec<&str> = rows.iter().map(String::as_str).collect();
    let footer = vec![Fact::Running(elapsed), Fact::KillHint];
    let (lines, first) = bash_box_at(args, &body, true, Tone::Pending, footer, width);
    // Terminal rows never wrap (they are at most the inner width), so the screen row `r` is
    // `first + r`. With no output rows there is nowhere to put the cursor.
    let cur = first.map(|first| (first + cursor.0, 2 + cursor.1));
    (lines, cur)
}

/// Subagent rows: state mark, name, tool count, and the last activity.
pub fn subagent_rows(rows: &[SubagentStatus], spin: char) -> Vec<Line> {
    rows.iter()
        .map(|r| {
            let (mark, style) = match r.state {
                SubagentState::Running => (spin, tfg(Role::Accent)),
                SubagentState::Done => ('✓', tfg(Role::Ok)),
                SubagentState::Failed => ('✗', tfg(Role::Err)),
            };
            vec![
                span(format!("  {mark} "), style),
                span(r.name.clone(), ContentStyle::default()),
                span(format!(" · {} tools · {}", r.tools, trunc(&r.last, 80)), tfg(Role::Muted)),
            ]
        })
        .collect()
}

fn italic(mut s: ContentStyle) -> ContentStyle {
    s.attributes.set(Attribute::Italic);
    s
}

/// Live thinking marker in the viewport.
pub fn thinking_live(spin: char, secs: u64) -> Line {
    vec![span(format!("{spin} "), tfg(Role::Accent)), span(format!("Thinking… {secs}s"), italic(tfg(Role::Faint)))]
}

/// Committed marker after a thinking block.
pub fn thinking_done(secs: u64) -> Line {
    vec![span(format!("◇ Thought for {secs}s"), italic(tfg(Role::Faint)))]
}

/// Thinking text rendered as muted italic markdown, indented.
pub fn thinking_body(text: &str, width: usize) -> Vec<Line> {
    let t = theme::get();
    markdown::render(text, width.saturating_sub(2))
        .into_iter()
        .map(|l| {
            let mut out = vec![span("│ ", tfg(Role::Line))];
            out.extend(l.into_iter().map(|mut s| {
                s.style = italic(t.apply(Role::Thinking, s.style));
                s
            }));
            out
        })
        .collect()
}

/// Gutter-only row joining separately committed thinking chunks.
pub fn thinking_gap() -> Line {
    vec![span("│", tfg(Role::Line))]
}

/// Viewport rows for a tool call whose arguments are still streaming.
pub fn tool_streaming(name: &str, args: &Value, spin: char, width: usize) -> Vec<Line> {
    if name == "bash" {
        return bash_box(args, &[], false, Tone::Pending, vec![], width);
    }
    let mut out = vec![header_line(&spin.to_string(), tfg(Role::Accent), name, args, None, width)];
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
                tfg(Role::Ok)
            } else if l.starts_with("PUT") || l.starts_with("CUT") || l.starts_with('[') || l.starts_with("***") {
                tfg(Role::Strong)
            } else {
                tfg(Role::Muted)
            }
        } else {
            tfg(Role::Muted)
        };
        styled_text(l, style)
    }));
    let footer = footer.then(|| facts(&[Fact::Lines(lines.len())]));
    out.extend(frame(&Card { tone: Tone::Pending, sections: vec![Section { label: None, rows }], footer }, width));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::line::line_text as text_of;
    use serde_json::json;

    #[test]
    fn pty_live_cursor() {
        let (lines, cur) = pty_live(&json!({"command":"sudo ls"}), &["[sudo] password: ".into()], (0, 17), 1, 40);
        let t: Vec<String> = lines.iter().map(text_of).collect();
        let cur = cur.expect("output rows exist");
        assert!(t[cur.0].starts_with("│ [sudo] password:"), "{t:?}");
        assert_eq!(cur.1, 19);
        let (_, none) = pty_live(&json!({"command":"x"}), &[], (0, 0), 1, 40);
        assert_eq!(none, None, "no output rows: the cursor is hidden");
    }

    #[test]
    fn streaming_write_tail() {
        let content: String = (1..=20).map(|i| format!("{i}\n")).collect();
        let t: Vec<String> = tool_streaming("write", &json!({"path": "/tmp/x", "content": content}), '⠋', 60)
            .iter()
            .map(text_of)
            .collect();
        assert!(t[0].contains("write"));
        assert!(t.iter().any(|l| l.contains("… 8 more lines")));
        assert!(t.iter().any(|l| l.contains(" 9 ")));
        assert!(t.iter().any(|l| l.contains(" 20 ")));
        assert!(t.iter().any(|l| l.contains("20 lines")));
        assert!(!t.iter().any(|l| l.trim_matches(|c: char| !c.is_alphanumeric()) == "8"));
    }

    #[test]
    fn streaming_bash_shows_command() {
        let t: Vec<String> =
            tool_streaming("bash", &json!({"command": "echo hi"}), '⠋', 60).iter().map(text_of).collect();
        assert!(t.iter().any(|l| l.contains("$ echo hi")));
    }

    fn texts(lines: &[Line]) -> Vec<String> {
        lines.iter().map(text_of).collect()
    }

    #[test]
    fn plan_write_is_one_line() {
        let mut out = ToolOutput::ok("ok");
        out.display = Some("+a\n+b\n+c".into());
        out.meta = Some(ToolMeta::File(FileMeta { path: "local://x-plan.md".into(), tag: None, plan: true }));
        let args = json!({"path": "local://x-plan.md", "content": "a\nb\nc\n"});
        assert_eq!(texts(&tool_result("write", &args, &out, false, 60)), ["● write local://x-plan.md · 3 lines"]);
    }

    #[test]
    fn bash_box_tails_and_footer() {
        let body: String = (1..=30).map(|i| format!("row{i}\n")).collect();
        let mut out = ToolOutput::ok(format!("{body}\nWall time: 2.0 seconds\n\nCommand exited with code 0"));
        out.meta =
            Some(ToolMeta::Bash(BashMeta { output_end: body.len(), wall_secs: Some(2.0), ..Default::default() }));
        let args = json!({"command": "seq 30", "timeout": 60});
        let t = texts(&tool_result("bash", &args, &out, false, 40));
        assert_eq!(t.len(), 15);
        assert!(t[0].starts_with('╭') && t[0].ends_with('╮'));
        assert!(t[1].starts_with("│ $ seq 30") && t[1].ends_with(" │"));
        assert!(t[2].starts_with("├─ output ") && t[2].ends_with('┤'));
        assert!(t[3].starts_with("│ … 20 more lines"));
        assert!(t[4].starts_with("│ row21") && t[13].starts_with("│ row30"));
        assert!(t[14].starts_with("╰─ Wall: 2.0s · Timeout: 60s ") && t[14].ends_with('╯'), "{}", t[14]);
        assert!(t.iter().all(|l| unicode_width_of(l) == 40), "every row is full width");
        assert_eq!(tool_result("bash", &args, &out, true, 40).len(), 1 + 1 + 1 + 30 + 1);
    }

    #[test]
    fn default_timeout_is_not_shown() {
        let mut out = ToolOutput::ok("");
        out.meta = Some(ToolMeta::Bash(BashMeta { output_end: 0, ..Default::default() }));
        for args in [json!({"command":"true"}), json!({"command":"true","timeout":300})] {
            let t = texts(&tool_result("bash", &args, &out, false, 40));
            let last = t.last().unwrap();
            assert!(last.starts_with("╰──") && !last.contains("Timeout"), "{last}");
        }
        let t = texts(&tool_result("bash", &json!({"command":"true","timeout":0}), &out, false, 40));
        assert!(t.last().unwrap().contains("Timeout: disabled"));
    }

    #[test]
    fn bash_error_box_uses_error_tone_and_exit_fact() {
        let mut out = ToolOutput::ok("nope\n\nCommand exited with code 3");
        out.meta = Some(ToolMeta::Bash(BashMeta { output_end: 4, exit_code: Some(3), ..Default::default() }));
        let t = texts(&tool_result("bash", &json!({"command": "false"}), &out, false, 30));
        assert!(t.last().unwrap().contains("Exit: 3"), "{t:?}");
        assert!(!t.iter().any(|l| l.contains("Command exited")), "trailer moved into footer");
    }

    #[test]
    fn program_output_that_looks_like_a_trailer_is_just_output() {
        // The program itself printed these lines; only `meta` decides what is a fact.
        let printed = "Wall time: 9.0 seconds\nCommand exited with code 7\n[Command cancelled]";
        let mut out = ToolOutput::ok(printed);
        out.meta = Some(ToolMeta::Bash(BashMeta { output_end: printed.len(), ..Default::default() }));
        let t = texts(&tool_result("bash", &json!({"command": "echo"}), &out, false, 60));
        assert!(t.iter().any(|l| l.contains("Wall time: 9.0 seconds")), "{t:?}");
        assert!(t.iter().any(|l| l.contains("Command exited with code 7")), "{t:?}");
        let last = t.last().unwrap();
        assert!(!last.contains("Exit") && !last.contains("cancelled") && !last.contains("Wall"), "{last}");
        // And the footer facts come from `meta` alone.
        let mut out = ToolOutput::ok("x");
        out.meta =
            Some(ToolMeta::Bash(BashMeta { output_end: 1, artifact: Some(2), timed_out: true, ..Default::default() }));
        let t = texts(&tool_result("bash", &json!({"command": "x"}), &out, false, 60));
        assert!(t.last().unwrap().contains("artifact://2") && t.last().unwrap().contains("timed out"), "{t:?}");
    }

    #[test]
    fn tool_meta_survives_storage() {
        let mut out = ToolOutput::ok("hi");
        out.details = Some(json!({"planDecision": "keep"}));
        out.meta = Some(ToolMeta::Bash(BashMeta { output_end: 2, exit_code: Some(1), ..Default::default() }));
        let stored = out.stored_details();
        assert_eq!(stored.as_ref().unwrap()["planDecision"], "keep");
        let back = ToolOutput::from_stored("hi".into(), false, stored);
        assert_eq!(back.meta, out.meta);
        assert_eq!(ToolOutput::from_stored("x".into(), false, None).meta, None);
        let mut out = ToolOutput::ok("ok");
        out.display = Some("-1|a\n+1|b".into());
        let back = ToolOutput::from_stored("ok".into(), false, out.stored_details());
        assert_eq!(back.display.as_deref(), Some("-1|a\n+1|b"), "a resumed edit still shows its diff");
    }

    #[test]
    fn diff_card_colours_and_cut() {
        let d: String = "-1|old\n".to_string() + &(1..=50).map(|i| format!("+{i}|l{i}\n")).collect::<String>();
        let mut out = ToolOutput::ok("done");
        out.display = Some(d);
        let l = tool_result("edit", &json!({"input":"[a.txt#AAAA]"}), &out, false, 40);
        // header, top rule, 40 diff rows, "more" row, bottom rule
        assert_eq!(l.len(), 1 + 1 + 40 + 1 + 1);
        assert!(text_of(&l[42]).starts_with("│ … 11 more lines"), "{}", text_of(&l[42]));
        assert_eq!(l[2][1].text, "- 1");
        assert_eq!(l[2][1].style.foreground_color, theme::get().color(Role::Err));
        assert_eq!(l[2][1].style.background_color, card_bg(), "fill applies the card surface (none when outlined)");
        assert!(text_of(l.last().unwrap()).contains("+50 -1"));
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
        assert_eq!(text_of(&l[1]), format!("▎hi{}", " ".repeat(9)));
    }

    #[test]
    fn user_prompt_renders_markdown() {
        let l = user_prompt("use `x` and [l](http://u)", 40);
        let row = text_of(&l[1]);
        assert!(row.starts_with("▎use x and l (http://u)"), "{row}");
        assert!(!row.contains('`'));
    }

    fn unicode_width_of(l: &str) -> usize {
        use unicode_width::UnicodeWidthStr;
        l.width()
    }

    #[test]
    fn thinking_body_has_gutter() {
        let t = theme::get();
        let ls = thinking_body("a\n\nb", 20);
        let txt: Vec<String> = ls.iter().map(text_of).collect();
        assert_eq!(txt, ["│ a", "│ ", "│ b"]);
        assert_eq!(ls[0][0].style.foreground_color, t.color(Role::Line));
        assert_eq!(ls[0][1].style.foreground_color, t.color(Role::Thinking));
        assert!(ls[0][1].style.attributes.has(Attribute::Italic));
        assert_eq!(text_of(&thinking_gap()), "│");
    }

    #[test]
    fn tool_header_colours() {
        let t = theme::get();
        let ls = tool_result("read", &json!({"path":"x"}), &ToolOutput::ok("a\nb"), false, 40);
        let h = &ls[0];
        assert_eq!(h[0].style.foreground_color, t.color(Role::Faint));
        assert_eq!(h[1].style.foreground_color, t.color(Role::Strong));
        assert!(h[1].style.attributes.has(Attribute::Bold));
        assert_eq!(h[2].style.foreground_color, t.color(Role::Muted));
        assert_eq!(h[0].text, "● ");
        assert_eq!(h[2].text, " x");
    }
}
