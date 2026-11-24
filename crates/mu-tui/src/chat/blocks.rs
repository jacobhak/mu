//! Transcript blocks: user prompts, tool headers and framed tool cards, thinking markers, subagent rows.
use crate::text::diffview;
use crate::text::line::{self, Line, fill, span, truncate, wrap};
use crate::text::markdown;
use crate::theme::{self, Role};
use crossterm::style::{Attribute, Color, ContentStyle};
use mu_base::config::Cards;
use mu_base::events::{JobKind, SubagentState, SubagentStatus};
use mu_base::text::truncate_text as trunc;
use mu_base::tool::{BashMeta, FileMeta, ToolMeta, ToolOutput};
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
/// markdown, and one padding row above and below. A skill invocation shows as one header row
/// instead of its expanded body (see `skill_call`).
pub fn user_prompt(text: &str, width: usize) -> Vec<Line> {
    let bg = tbg(Role::Surface);
    let inner = width.saturating_sub(1).max(1);
    let bar = || span("▎", with_bg(tfg(Role::Accent), bg));
    let blank = || fill(&vec![bar()], width, bg);
    let mut out = vec![blank()];
    for l in prompt_rows(text, inner) {
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

/// Ends every `>` quote at the first following line without `>`, as chat apps do, instead of
/// markdown's lazy continuation: a blank line is inserted there. Fenced code is left alone.
fn end_quotes(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    let (mut fence, mut prev_quote) = (false, false);
    for (i, line) in text.split('\n').enumerate() {
        let t = line.trim_start();
        let quote = !fence && t.starts_with('>');
        if !quote && (t.starts_with("```") || t.starts_with("~~~")) {
            fence = !fence;
        }
        if prev_quote && !quote && !t.is_empty() {
            out.push('\n');
        }
        if i > 0 {
            out.push('\n');
        }
        out.push_str(line);
        prev_quote = quote;
    }
    out
}

/// The rows of a user prompt's text, before the band is drawn around them.
fn prompt_rows(text: &str, inner: usize) -> Vec<Line> {
    let Some(call) = skill_call(text) else {
        return markdown::render(&end_quotes(text), inner);
    };
    let home = crate::host::host_home();
    let head = vec![
        span(format!("/skill:{}", call.name), name_style()),
        span(format!(" · {}", tilde(call.path, home.as_deref())), tfg(Role::Faint)),
    ];
    let mut rows = vec![truncate(&head, inner)];
    if !call.args.is_empty() {
        rows.extend(markdown::render(call.args, inner));
    }
    rows
}

/// A `/skill:` invocation as `Context::skill_prompt` expands it: the envelope, then the user's args.
#[derive(Debug, PartialEq)]
pub struct SkillCall<'a> {
    pub name: &'a str,
    pub path: &'a str,
    pub args: &'a str,
}

/// Parses `<skill name="N" path="P">\n<body>\n</skill>` with optional `\n\n<args>` after it.
/// The path may contain quotes, since the writer does not escape it. The body ends at the first
/// `\n</skill>` that is followed by the end of the text or by the blank line before the args.
/// `None` for any other text, which then shows as it is.
pub fn skill_call(text: &str) -> Option<SkillCall<'_>> {
    const CLOSE: &str = "\n</skill>";
    let rest = text.strip_prefix("<skill name=\"")?;
    let (name, rest) = rest.split_once("\" path=\"")?;
    let (path, rest) = rest.split_once("\">\n")?;
    if name.is_empty() || path.is_empty() {
        return None;
    }
    let mut from = 0;
    while let Some(i) = rest[from..].find(CLOSE) {
        let after = &rest[from + i + CLOSE.len()..];
        if after.is_empty() {
            return Some(SkillCall { name, path, args: "" });
        }
        if let Some(args) = after.strip_prefix("\n\n").map(str::trim).filter(|a| !a.is_empty()) {
            return Some(SkillCall { name, path, args });
        }
        from += i + 1;
    }
    None
}

/// `p` with the home directory written as `~`.
fn tilde(p: &str, home: Option<&str>) -> String {
    match home.filter(|h| !h.is_empty()).and_then(|h| p.strip_prefix(h)) {
        Some(rest) if rest.starts_with('/') => format!("~{rest}"),
        _ => p.to_string(),
    }
}

fn name_style() -> ContentStyle {
    bold(tfg(Role::Tool))
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
    mu_tools::tools::summary(name, args)
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
    let mut chars = name.chars();
    let title: String = chars.next().into_iter().flat_map(char::to_uppercase).chain(chars).collect();
    let mut l = vec![span(format!("{icon} "), icon_style), span(title, name_style())];
    if !summary.is_empty() {
        l.push(span(format!(" {summary}"), tfg(Role::Tool)));
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
    /// Label on the top rule.
    title: Option<Line>,
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
    let mut out = vec![rule('╭', '╮', card.title.clone())];
    let mut anchor = None;
    let mut first = true;
    for s in &card.sections {
        if s.rows.is_empty() && s.label.is_none() {
            continue;
        }
        if let Some(label) = s.label {
            out.push(rule('├', '┤', Some(vec![span(label, tfg(Role::Tool))])));
        } else if !first {
            out.push(rule('├', '┤', None));
        }
        first = false;
        if s.label == Some("Output") {
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
            Fact::KillHint => "Esc kills".into(),
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
        .map(|(i, l)| vec![span(if i == 0 { "$ " } else { "  " }, faint), span(*l, tfg(Role::Accent))])
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
        rows.extend(output[skip..].iter().map(|l| styled_text(l, tfg(Role::Muted))));
        sections.push(Section { label: Some("Output"), rows });
    }
    let at = footer.iter().position(|f| matches!(f, Fact::Wall(_) | Fact::Running(_))).map_or(0, |i| i + 1);
    if let Some(t) = timeout_part(args) {
        footer.insert(at, t);
    }
    let footer = (!footer.is_empty()).then(|| facts(&footer));
    frame_at(&Card { tone, title: None, sections, footer }, width)
}

/// How long a job has run, or ran, in seconds; a running one has run until `now`.
fn job_secs(r: &SubagentStatus, now: std::time::SystemTime) -> f64 {
    let end = r.ended.unwrap_or(now);
    end.duration_since(r.started).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

/// `3.4s`, or `3m04s` past a minute.
fn run_time(secs: f64) -> String {
    if secs >= 60.0 { format!("{}m{:02}s", secs as u64 / 60, secs as u64 % 60) } else { format!("{secs:.1}s") }
}

/// How a `bash` job ended, as a footer fact. A clean `exit 0` says nothing, as a foreground run's
/// footer does.
fn exit_fact(note: Option<&str>) -> Option<Fact> {
    let note = note?;
    if let Some(code) = note.strip_prefix("exit ") {
        return match code.trim().parse::<i32>() {
            Ok(0) => None,
            Ok(c) => Some(Fact::Exit(c)),
            Err(_) => Some(Fact::Note(note.to_string())),
        };
    }
    Some(match note {
        "timed out" => Fact::TimedOut,
        "cancelled" => Fact::Cancelled,
        other => Fact::Note(other.to_string()),
    })
}

/// The result of a finished `bash` job: its command, the newest of its output and a footer with
/// its run time and how it ended.
pub fn bash_job_result(r: &SubagentStatus, out: Option<&str>, expanded: bool, width: usize) -> Vec<Line> {
    let args = serde_json::json!({ "command": r.task });
    let output: Vec<&str> = out.unwrap_or("").lines().collect();
    let tone = if r.state == SubagentState::Failed { Tone::Error } else { Tone::Success };
    let mut footer = vec![Fact::Wall(job_secs(r, std::time::SystemTime::now()))];
    footer.extend(exit_fact(r.note.as_deref()));
    bash_box(&args, &output, expanded, tone, footer, width)
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
        if m.background.is_some() {
            // The call only started a job: it has no output of its own, and its result comes as a
            // card of its own when the job finishes. One line marks the call.
            return vec![header_line("●", tfg(Role::Faint), name, args, out.meta.as_ref(), width)];
        }
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
            title: None,
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
            let tool = tfg(Role::Tool);
            let faint = tfg(Role::Faint);
            let mut title = vec![
                span(if name == "edit" { "✎ Edit" } else { "✎ Write" }, tool),
                span(": ", faint),
                span(path, tool),
                span(" ", faint),
            ];
            if name == "edit" {
                title.extend([
                    span("⟦", faint),
                    span(format!("+{adds}"), tfg(Role::Ok)),
                    span("/", faint),
                    span(format!("-{dels}"), tfg(Role::Err)),
                    span("⟧", faint),
                ]);
            } else {
                title.push(span(format!("· {adds} lines"), faint));
            }
            let card = Card {
                tone: Tone::Success,
                title: Some(title),
                sections: vec![Section { label: None, rows }],
                footer: None,
            };
            lines.clear();
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
        "review" => {
            let tone = if out.text.starts_with("## Review: NEEDS FIXES") { Tone::Error } else { Tone::Success };
            let rows = markdown::render(&out.text, width.saturating_sub(4));
            let card = Card { tone, title: None, sections: vec![Section { label: None, rows }], footer: None };
            lines.extend(frame(&card, width));
        }
        "ask" => lines.extend(ask_rows(args, &out.text, width)),
        "task" => lines.extend(text_rows(&out.text, expanded, |l| {
            if l.starts_with("## ") { name_style() } else { tfg(Role::Muted) }
        })),
        "todo" => match todo_rows(out, width) {
            Some((rows, count)) => {
                lines[0].push(span(count, tfg(Role::Faint)));
                lines[0] = truncate(&lines[0], width);
                lines.extend(rows);
            }
            None => lines.extend(text_rows(&out.text, expanded, |_| tfg(Role::Muted))),
        },
        _ => lines.extend(text_rows(&out.text, expanded, |_| tfg(Role::Muted))),
    }
    lines
}

/// A todo result as one checklist: a mark per task, phase names only when there are several.
/// `None` when the result carries no tasks (a cleared list), so the plain text shows instead.
fn todo_rows(out: &ToolOutput, width: usize) -> Option<(Vec<Line>, String)> {
    use mu_tools::tools::todo::{Phase, Status};
    let phases: Vec<Phase> = serde_json::from_value(out.details.as_ref()?["phases"].clone()).ok()?;
    let phases: Vec<&Phase> = phases.iter().filter(|p| !p.tasks.is_empty()).collect();
    let total: usize = phases.iter().map(|p| p.tasks.len()).sum();
    if total == 0 {
        return None;
    }
    let done = phases
        .iter()
        .flat_map(|p| &p.tasks)
        .filter(|t| matches!(t.status, Status::Completed | Status::Abandoned))
        .count();
    let multi = phases.len() > 1;
    let indent = if multi { "    " } else { "  " };
    let mut rows = Vec::new();
    for p in &phases {
        if multi {
            rows.push(truncate(&vec![span(format!("  {}", p.name), tfg(Role::Muted))], width));
        }
        for t in &p.tasks {
            let (mark, mark_style, text_style) = match t.status {
                Status::Completed => ("✓", tfg(Role::Ok), tfg(Role::Faint)),
                Status::InProgress => ("▸", tfg(Role::Warn), tfg(Role::Warn)),
                Status::Pending => ("○", tfg(Role::Faint), tfg(Role::Muted)),
                Status::Abandoned => ("–", tfg(Role::Faint), tfg(Role::Faint)),
                Status::Blocked => ("!", tfg(Role::Err), tfg(Role::Muted)),
            };
            let mut row = vec![span(format!("{indent}{mark} "), mark_style), span(t.content.clone(), text_style)];
            if t.status == Status::Blocked
                && let Some(b) = &t.blocker
            {
                row.push(span(format!(" ({b})"), tfg(Role::Faint)));
            }
            rows.push(truncate(&row, width));
        }
    }
    Some((rows, format!(" · {done}/{total}")))
}

/// An answered `ask`: context and the emphasized question above the answer the tool returned for
/// it (`id: answer` lines after the `User answers:` header). The model still gets only the tool text.
fn ask_rows(args: &Value, text: &str, width: usize) -> Vec<Line> {
    let inner = width.saturating_sub(2).max(1);
    let questions: Vec<&Value> = args["questions"].as_array().into_iter().flatten().collect();
    let ids: Vec<&str> = questions.iter().map(|q| q["id"].as_str().unwrap_or("")).collect();
    let answers = answer_blocks(&ids, text);
    let mut rows = Vec::new();
    for (q, answer) in questions.iter().zip(answers) {
        let question = q["question"].as_str().unwrap_or("");
        let question = match q["header"].as_str().filter(|h| !h.is_empty()) {
            Some(h) => format!("{h}: {question}"),
            None => question.to_string(),
        };
        if let Some(c) = q["context"].as_str().map(str::trim).filter(|c| !c.is_empty()) {
            for r in markdown::wrap_chs(&markdown::chs(c, ContentStyle::default()), inner) {
                let mut row = vec![span("  ", line::dim())];
                row.extend(markdown::chars_to_line(&r));
                rows.push(row);
            }
        }
        for r in markdown::wrap_chs(&markdown::chs(&question, bold(ContentStyle::default())), inner) {
            let mut row = vec![span("  ", line::dim())];
            row.extend(markdown::chars_to_line(&r));
            rows.push(row);
        }
        if let Some(a) = answer {
            // Wrapped, not cut: a note after the choice, and every line of a multi-line answer, is part of it.
            for r in markdown::wrap_chs(&markdown::chs(&a, tfg(Role::Muted)), inner) {
                let mut row = vec![span("  ", line::dim())];
                row.extend(markdown::chars_to_line(&r));
                rows.push(row);
            }
        }
    }
    rows
}

/// Each question's answer from the tool text, in question order: the `id: ` line and the lines after
/// it, up to the next question's line. A custom answer or a note can span several lines.
fn answer_blocks(ids: &[&str], text: &str) -> Vec<Option<String>> {
    let mut out: Vec<Option<String>> = vec![None; ids.len()];
    let mut cur: Option<usize> = None;
    let mut next = 0;
    for line in text.lines() {
        if let Some(j) = (next..ids.len()).find(|&j| line.starts_with(&format!("{}: ", ids[j]))) {
            out[j] = Some(line.to_string());
            cur = Some(j);
            next = j + 1;
        } else if let Some(block) = cur.and_then(|j| out[j].as_mut()) {
            block.push('\n');
            block.push_str(line);
        }
    }
    out
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
    waiting: bool,
    width: usize,
) -> (Vec<Line>, Option<(usize, usize)>) {
    let body: Vec<&str> = rows.iter().map(String::as_str).collect();
    let mut footer = vec![Fact::Running(elapsed)];
    if waiting {
        footer.push(Fact::Note("waiting for input".into()));
    }
    footer.push(Fact::KillHint);
    let (lines, first) = bash_box_at(args, &body, true, Tone::Pending, footer, width);
    // Terminal rows never wrap (they are at most the inner width), so the screen row `r` is
    // `first + r`. With no output rows there is nowhere to put the cursor.
    let cur = first.map(|first| (first + cursor.0, 2 + cursor.1));
    (lines, cur)
}

/// Subagent rows: state mark, name, tool count, cost and the last activity. `cost` says what a row's
/// session has spent; none, or nothing yet, leaves the figure out (omp does the same).
pub fn subagent_rows(
    rows: &[SubagentStatus],
    now: std::time::SystemTime,
    spin: char,
    cost: impl Fn(&SubagentStatus) -> Option<f64>,
) -> Vec<Line> {
    job_rows(rows, now, spin, cost, true)
}

/// `subagent_rows`; `with_last: false` leaves the last activity out, for a caller that shows it in full below.
pub fn job_rows(
    rows: &[SubagentStatus],
    now: std::time::SystemTime,
    spin: char,
    cost: impl Fn(&SubagentStatus) -> Option<f64>,
    with_last: bool,
) -> Vec<Line> {
    rows.iter()
        .map(|r| {
            let (mark, style) = match r.state {
                SubagentState::Running => (spin, tfg(Role::Accent)),
                SubagentState::Done => ('✓', tfg(Role::Ok)),
                SubagentState::Failed => ('✗', tfg(Role::Err)),
            };
            let mut row = vec![span(format!("  {mark} "), style), span(r.name.clone(), ContentStyle::default())];
            match r.kind {
                // A bash job runs one command: its row says how long it has run, not what it called.
                JobKind::Bash => row.push(span(format!(" · {}", run_time(job_secs(r, now))), tfg(Role::Muted))),
                JobKind::Subagent => {
                    if let Some(m) = r.model.as_deref() {
                        row.push(span(format!(" · {m}"), tfg(Role::Muted)));
                    }
                    row.push(span(format!(" · {} tools", r.tools), tfg(Role::Muted)));
                }
            }
            if let Some(c) = cost(r).filter(|c| *c > 0.0) {
                row.push(span(" · ", tfg(Role::Muted)));
                row.push(span(format!("${c:.2}"), tfg(Role::SynType)));
            }
            if with_last {
                row.push(span(format!(" · {}", trunc(&r.last, 80)), tfg(Role::Muted)));
            }
            row
        })
        .collect()
}

/// A running job's row with the inline marker (`▸`, row highlighted), and the keys that act on it
/// in a row of its own.
pub fn focused_job(mut row: Line, width: usize) -> Vec<Line> {
    if let Some(first) = row.first_mut() {
        first.text = first.text.replacen("  ", "▸ ", 1);
    }
    let row = fill(&truncate(&row, width), width, theme::get().color(Role::Selected));
    let hint = "    k cancel · Esc unmark · Alt+↓ next · Alt+↑ previous";
    vec![row, vec![span(trunc(hint, width), tfg(Role::Faint))]]
}

/// Most lines of a bash job's newest output shown under its row when expanded.
const JOB_TAIL: usize = 8;

/// Rows under a job's row when expanded: its whole task, agent, run time and directory, the full
/// last activity and, for a bash job, the tail of its output. Long lines wrap, none is cut.
pub fn job_details(r: &SubagentStatus, now: std::time::SystemTime, out: Option<&str>, width: usize) -> Vec<Line> {
    let inner = width.saturating_sub(6);
    let mut rows = Vec::new();
    let mut row = |text: &str| {
        let body = vec![span(text.replace('\t', "    "), tfg(Role::Muted))];
        rows.extend(wrap(&body, inner).into_iter().map(|mut l| {
            l.insert(0, span("    │ ".to_string(), tfg(Role::Faint)));
            l
        }));
    };
    r.task.lines().filter(|t| !t.is_empty()).for_each(&mut row);
    row(&format!("{} · {} · {}", r.agent, run_time(job_secs(r, now)), r.cwd.display()));
    if r.kind == JobKind::Subagent && !r.last.is_empty() {
        row(&format!("last: {}", r.last));
    }
    if let Some(out) = out {
        let lines: Vec<&str> = out.lines().collect();
        lines[lines.len().saturating_sub(JOB_TAIL)..].iter().for_each(|l| row(l));
    }
    rows
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
            let mut out = vec![span(" ", ContentStyle::default())];
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
    vec![span(" ", ContentStyle::default())]
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
    out.extend(frame(
        &Card { tone: Tone::Pending, title: None, sections: vec![Section { label: None, rows }], footer },
        width,
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::line::line_text as text_of;
    use serde_json::json;

    #[test]
    fn job_details_show_task_and_the_output_tail() {
        let r = SubagentStatus {
            name: "b1".into(),
            agent: "bash".into(),
            tools: 0,
            last: String::new(),
            state: SubagentState::Running,
            kind: JobKind::Bash,
            cwd: "/w".into(),
            task: "build it".into(),
            started: std::time::SystemTime::now(),
            ended: None,
            delivered: false,
            note: None,
            session: None,
            model: None,
            changes: None,
            progress: None,
            ports: Vec::new(),
        };
        let out: String = (0..20).map(|i| format!("l{i}\n")).collect();
        let text: Vec<String> =
            job_details(&r, std::time::SystemTime::now(), Some(&out), 80).iter().map(text_of).collect();
        assert!(text[0].contains("build it"));
        assert!(text[1].contains("/w"));
        assert_eq!(text.len(), 2 + JOB_TAIL);
        assert!(text.last().unwrap().contains("l19"));
        assert!(!text.iter().any(|l| l.contains("l11")));
    }

    #[test]
    fn job_details_wrap_a_long_task_whole_and_every_task_line() {
        let mut r = bash_job(SubagentState::Running, 0.0);
        r.task = format!("cd /w && {}\nsecond line", "x".repeat(100));
        let text: Vec<String> = job_details(&r, fixed_now(), None, 40).iter().map(text_of).collect();
        assert!(
            text.iter().all(|l| l.starts_with("    │ ") && !l.contains('…') && l.chars().count() <= 40),
            "{text:?}"
        );
        let task: String =
            text.iter().take_while(|l| !l.contains("second")).map(|l| &l[l.find('│').unwrap() + 4..]).collect();
        assert_eq!(task, r.task.lines().next().unwrap());
        assert!(text.iter().any(|l| l.contains("second line")));
        let row = |with_last| text_of(&job_rows(&[r.clone()], fixed_now(), '⠋', |_| None, with_last)[0]);
        assert_eq!(row(false), "  ⠋ bg_1 · 0.0s");
    }

    #[test]
    fn a_focused_job_row_is_marked_and_followed_by_the_keys() {
        let row = vec![span("  ⠋ ", tfg(Role::Accent)), span("b1", ContentStyle::default())];
        let t = texts(&focused_job(row, 80));
        assert_eq!(t[0].trim_end(), "▸ ⠋ b1");
        assert!(t[1].contains("k cancel") && t[1].contains("Esc unmark"), "{t:?}");
        assert!(t.iter().all(|l| l.chars().count() <= 80));
    }

    #[test]
    fn subagent_rows_show_their_cost_only_when_there_is_some() {
        let row = |state| SubagentStatus {
            name: "scout-1".into(),
            agent: "scout".into(),
            tools: 4,
            last: "read a.rs".into(),
            state,
            kind: mu_base::events::JobKind::Subagent,
            cwd: std::path::PathBuf::new(),
            task: String::new(),
            started: std::time::SystemTime::UNIX_EPOCH,
            ended: None,
            delivered: false,
            note: None,
            session: None,
            model: Some("scout · anthropic/haiku".into()),
            changes: None,
            progress: None,
            ports: Vec::new(),
        };
        let text = |cost: Option<f64>, state| text_of(&subagent_rows(&[row(state)], fixed_now(), '⠋', |_| cost)[0]);
        assert_eq!(
            text(Some(0.123), SubagentState::Running),
            "  ⠋ scout-1 · scout · anthropic/haiku · 4 tools · $0.12 · read a.rs"
        );
        assert_eq!(
            text(Some(0.5), SubagentState::Done),
            "  ✓ scout-1 · scout · anthropic/haiku · 4 tools · $0.50 · read a.rs"
        );
        let plain = "  ⠋ scout-1 · scout · anthropic/haiku · 4 tools · read a.rs";
        assert_eq!(text(None, SubagentState::Running), plain, "no session, no figure");
        assert_eq!(text(Some(0.0), SubagentState::Running), plain, "unpriced is not free: no `$0.00`");
    }

    /// The clock every row is rendered at: no real time passes between building a job and showing it.
    fn fixed_now() -> std::time::SystemTime {
        std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000)
    }

    /// A `bash` job that has been running, or ran, for `secs` seconds at `fixed_now`.
    fn bash_job(state: SubagentState, secs: f64) -> SubagentStatus {
        let elapsed = std::time::Duration::from_secs_f64(secs);
        let running = state == SubagentState::Running;
        let now = fixed_now();
        let started = if running { now - elapsed } else { std::time::SystemTime::UNIX_EPOCH };
        let ended = (!running).then(|| started + elapsed);
        SubagentStatus {
            name: "bg_1".into(),
            agent: "bash".into(),
            tools: 0,
            last: "cargo build".into(),
            state,
            kind: JobKind::Bash,
            cwd: "/w".into(),
            task: "cargo build".into(),
            started,
            ended,
            delivered: true,
            note: Some("exit 0".into()),
            session: None,
            model: None,
            changes: None,
            progress: None,
            ports: Vec::new(),
        }
    }

    #[test]
    fn an_async_bash_call_is_one_line() {
        // The result text is for the model; the transcript marks the call and shows the job.
        let text = "Started background job bg_1.\nWhen it finishes, mu automatically starts your next turn…";
        let mut out = ToolOutput::ok(text);
        out.meta =
            Some(ToolMeta::Bash(BashMeta { output_end: 0, background: Some("bg_1".into()), ..Default::default() }));
        let t = texts(&tool_result("bash", &json!({"command": "cargo build"}), &out, false, 40));
        assert_eq!(t, ["● Bash cargo build"]);
    }

    #[test]
    fn a_bash_job_row_says_how_long_it_has_run() {
        let row = |state, secs| text_of(&subagent_rows(&[bash_job(state, secs)], fixed_now(), '⠋', |_| None)[0]);
        assert_eq!(row(SubagentState::Running, 0.0), "  ⠋ bg_1 · 0.0s · cargo build", "no tool count");
        assert_eq!(row(SubagentState::Running, 12.5), "  ⠋ bg_1 · 12.5s · cargo build");
        assert_eq!(row(SubagentState::Done, 3.4), "  ✓ bg_1 · 3.4s · cargo build");
        let long = bash_job(SubagentState::Done, 184.0);
        assert_eq!(text_of(&subagent_rows(&[long], fixed_now(), '⠋', |_| None)[0]), "  ✓ bg_1 · 3m04s · cargo build");
    }

    #[test]
    fn a_finished_bash_job_is_a_bash_card_with_its_run_time() {
        let out = "Compiling mu\nFinished dev profile";
        let t = texts(&bash_job_result(&bash_job(SubagentState::Done, 3.4), Some(out), false, 40));
        assert!(t[0].starts_with('╭') && t[0].ends_with('╮'), "{t:?}");
        assert!(t[1].starts_with("│ $ cargo build") && t[1].ends_with(" │"), "{t:?}");
        assert!(t[2].starts_with("├─ Output ") && t[2].ends_with('┤'), "{t:?}");
        assert!(t[3].starts_with("│ Compiling mu") && t[4].starts_with("│ Finished dev"), "{t:?}");
        assert!(t[5].starts_with("╰─ Wall: 3.4s ") && t[5].ends_with('╯'), "{}", t[5]);
        assert!(!t[5].contains("Exit"), "a clean exit says nothing: {}", t[5]);
        assert!(t.iter().all(|l| unicode_width_of(l) == 40), "every row is full width: {t:?}");
    }

    #[test]
    fn a_failed_bash_job_keeps_its_exit_and_its_output() {
        let mut r = bash_job(SubagentState::Failed, 1.5);
        r.note = Some("exit 2".into());
        let t = texts(&bash_job_result(&r, Some("boom"), false, 30));
        let last = t.last().unwrap();
        assert!(last.contains("Wall: 1.5s") && last.contains("Exit: 2"), "{last}");
        assert!(t.iter().any(|l| l.contains("boom")), "{t:?}");
        let top = &bash_job_result(&r, None, false, 30)[0];
        assert_eq!(top[0].style.foreground_color, theme::get().color(Role::ErrSoft), "the card takes the error tone");
        r.note = Some("timed out".into());
        assert!(texts(&bash_job_result(&r, None, false, 30)).last().unwrap().contains("timed out"));
    }

    #[test]
    fn pty_live_cursor() {
        let (lines, cur) =
            pty_live(&json!({"command":"sudo ls"}), &["[sudo] password: ".into()], (0, 17), 1, false, 40);
        let t: Vec<String> = lines.iter().map(text_of).collect();
        let cur = cur.expect("output rows exist");
        assert!(t[cur.0].starts_with("│ [sudo] password:"), "{t:?}");
        assert_eq!(cur.1, 19);
        let (_, none) = pty_live(&json!({"command":"x"}), &[], (0, 0), 1, false, 40);
        assert_eq!(none, None, "no output rows: the cursor is hidden");
        let (lines, _) = pty_live(&json!({"command":"sudo ls"}), &["[sudo] password: ".into()], (0, 17), 1, true, 60);
        assert!(lines.iter().map(text_of).any(|l| l.contains("waiting for input")));
    }

    #[test]
    fn streaming_write_tail() {
        let content: String = (1..=20).map(|i| format!("{i}\n")).collect();
        let t: Vec<String> = tool_streaming("write", &json!({"path": "/tmp/x", "content": content}), '⠋', 60)
            .iter()
            .map(text_of)
            .collect();
        assert!(t[0].contains("Write"));
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
        assert_eq!(texts(&tool_result("write", &args, &out, false, 60)), ["● Write local://x-plan.md · 3 lines"]);
    }

    #[test]
    fn todo_result_is_a_checklist() {
        let mut out = ToolOutput::ok("Remaining items (1): ...");
        out.details = Some(json!({"op": "done", "phases": [{"name": "Tasks", "tasks": [
            {"content": "one", "status": "completed"},
            {"content": "two", "status": "in_progress"},
            {"content": "three", "status": "blocked", "blocker": "waiting"},
            {"content": "four", "status": "pending"}]}]}));
        let t = texts(&tool_result("todo", &json!({"op": "done"}), &out, false, 40));
        assert_eq!(t, ["● Todo · 1/4", "  ✓ one", "  ▸ two", "  ! three (waiting)", "  ○ four"], "{t:?}");
        let mut cleared = ToolOutput::ok("Todo list cleared.");
        cleared.details = Some(json!({"op": "rm", "phases": []}));
        let t = texts(&tool_result("todo", &json!({"op": "rm"}), &cleared, false, 40));
        assert_eq!(t, ["● Todo", "  Todo list cleared."], "{t:?}");
    }

    #[test]
    fn answered_ask_shows_each_question_above_its_answer() {
        let args = json!({"questions": [
            {"id": "approve", "question": "Deploy the canary now?\nstatus: 3 services green", "header": "Release", "options": []},
            {"id": "tests", "question": "Which suite?", "options": []}
        ]});
        let out = ToolOutput::ok("User answers:\napprove: yes\ntests: \"custom text\"");
        let t = texts(&tool_result("ask", &args, &out, false, 24));
        assert_eq!(
            t[1..],
            [
                "  Release: Deploy the",
                "  canary now?",
                "  status: 3 services",
                "  green",
                "  approve: yes",
                "  Which suite?",
                "  tests: \"custom text\"",
            ],
            "{t:?}"
        );
        assert!(t.iter().all(|l| l.chars().count() <= 24), "{t:?}");
    }

    #[test]
    fn answered_ask_shows_its_context_above_the_question() {
        let args = json!({"questions": [
            {"id": "api", "question": "Keep it?", "context": "Three callers still use it.", "options": []}
        ]});
        let out = ToolOutput::ok("User answers:\napi: Keep");
        let rows = tool_result("ask", &args, &out, false, 40);
        let t = texts(&rows);
        assert_eq!(t[1..], ["  Three callers still use it.", "  Keep it?", "  api: Keep"], "{t:?}");
        assert_eq!(rows[1][1].style, ContentStyle::default());
        assert_eq!(rows[2][1].style, bold(ContentStyle::default()));
        assert_eq!(rows[3][1].style, tfg(Role::Muted));
    }

    #[test]
    fn noted_answer_wraps_so_the_note_stays_visible() {
        let args = json!({"questions": [{"id": "skip", "question": "Run the tests?", "options": []}]});
        let out =
            ToolOutput::ok("User answers:\nskip: Skip — note: \"skip the tests for now, I will run them tomorrow\"");
        let t = texts(&tool_result("ask", &args, &out, false, 30));
        assert!(t.iter().any(|l| l.contains("tomorrow")), "{t:?}");
        assert!(t.iter().any(|l| l.contains("note")), "{t:?}");
        assert!(t.iter().all(|l| l.chars().count() <= 30), "{t:?}");
    }

    #[test]
    fn multi_line_answers_stay_under_their_question() {
        let args = json!({"questions": [
            {"id": "why", "question": "Why?", "options": []},
            {"id": "next", "question": "Next?", "options": []}
        ]});
        let out = ToolOutput::ok("User answers:\nwhy: \"first line\nsecond line\"\nnext: Skip — note: \"a\nb\"");
        let t = texts(&tool_result("ask", &args, &out, false, 30));
        let at = |s: &str| t.iter().position(|l| l.contains(s)).unwrap_or_else(|| panic!("{s}: {t:?}"));
        assert!(at("first line") < at("second line\""), "{t:?}");
        assert!(at("second line\"") < at("Next?"), "the answer's last line stays with its question: {t:?}");
        assert!(at("b\"") > at("Next?"), "{t:?}");
        assert!(t.iter().all(|l| l.chars().count() <= 30), "{t:?}");
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
        assert!(t[2].starts_with("├─ Output ") && t[2].ends_with('┤'));
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
        // top rule (title), 40 diff rows, "more" row, bottom rule
        assert_eq!(l.len(), 1 + 40 + 1 + 1);
        assert!(text_of(&l[41]).starts_with("│ … 11 more lines"), "{}", text_of(&l[41]));
        assert_eq!(l[1][1].text, "- 1");
        assert_eq!(l[1][1].style.foreground_color, theme::get().color(Role::Err));
        assert_eq!(l[1][1].style.background_color, card_bg(), "fill applies the card surface (none when outlined)");
        assert!(text_of(&l[0]).contains("Edit: a.txt ⟦+50/-1⟧"), "{}", text_of(&l[0]));
    }

    #[test]
    fn error_box_cuts_at_ten_lines() {
        let body: String = (0..12).map(|i| format!("e{i}\n")).collect();
        let t = texts(&tool_result("read", &json!({"path":"x"}), &ToolOutput::err(body), false, 30));
        assert_eq!(t[0], "● Read x");
        assert!(t[1].starts_with('╭') && t[1].ends_with('╮'));
        assert!(t.iter().any(|l| l.contains("… 2 more lines")));
        assert!(t.last().unwrap().starts_with('╰'));
    }

    #[test]
    fn edit_header_shows_path_only() {
        let l = tool_result("edit", &json!({"input": "[src/a.rs#B6C1]\n@@"}), &ToolOutput::ok("ok"), false, 40);
        assert_eq!(text_of(&l[0]), "● Edit src/a.rs");
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

    #[test]
    fn a_comment_right_under_a_quote_is_not_part_of_it() {
        let rows: Vec<String> = user_prompt("> quoted\ncool", 30).iter().map(text_of).collect();
        assert!(rows.iter().any(|r| r.contains("▏ quoted")), "{rows:?}");
        assert!(rows.iter().any(|r| r.starts_with("▎cool")), "{rows:?}");
        assert_eq!(end_quotes("> a\n> b\nc\n\n```\n> x\ny\n```"), "> a\n> b\n\nc\n\n```\n> x\ny\n```");
    }

    #[test]
    fn skill_call_parses_envelope() {
        let text = "<skill name=\"steward\" path=\"/x/SKILL.md\">\n# Body\n\nmore\n</skill>";
        assert_eq!(skill_call(text), Some(SkillCall { name: "steward", path: "/x/SKILL.md", args: "" }));
        let with_args = format!("{text}\n\nnext keyboard");
        assert_eq!(
            skill_call(&with_args),
            Some(SkillCall { name: "steward", path: "/x/SKILL.md", args: "next keyboard" })
        );
    }

    #[test]
    fn skill_call_handles_odd_paths_and_bodies() {
        let quoted = "<skill name=\"a\" path=\"/tmp/my \"odd\" dir/SKILL.md\">\nbody\n</skill>\n\ngo \"now\"";
        assert_eq!(
            skill_call(quoted),
            Some(SkillCall { name: "a", path: "/tmp/my \"odd\" dir/SKILL.md", args: "go \"now\"" })
        );
        // A closer inside the body that is not at the end of the text does not end the body.
        let nested = "<skill name=\"n\" path=\"/p\">\nx\n</skill>\ny\n</skill>\n\nargs";
        assert_eq!(skill_call(nested), Some(SkillCall { name: "n", path: "/p", args: "args" }));
    }

    #[test]
    fn skill_call_rejects_other_text() {
        for text in [
            "plain prompt",
            "<skill name=\"x\">\nbody\n</skill>",
            "<skill name=\"\" path=\"/p\">\nbody\n</skill>",
            "<skill name=\"x\" path=\"/p\">\nbody",
            "<skill name=\"x\" path=\"/p\">\nbody\n</skill>tail",
            "<skill name=\"x\" path=\"/p\">\nbody\n</skill>\n\n   ",
        ] {
            assert_eq!(skill_call(text), None, "{text:?}");
        }
    }

    #[test]
    fn tilde_abbreviates_home_only_at_a_boundary() {
        assert_eq!(tilde("/home/u/.agents/s", Some("/home/u")), "~/.agents/s");
        assert_eq!(tilde("/home/user2/s", Some("/home/u")), "/home/user2/s");
        assert_eq!(tilde("/opt/s", None), "/opt/s");
    }

    #[test]
    fn skill_invocation_renders_one_header_and_args() {
        let body: String = (0..40).map(|i| format!("step {i}\n")).collect();
        let text =
            format!("<skill name=\"steward\" path=\"/opt/sk/steward/SKILL.md\">\n{body}</skill>\n\nnext keyboard");
        let l = user_prompt(&text, 60);
        let rows: Vec<String> = l.iter().map(text_of).collect();
        assert_eq!(rows.len(), 4, "{rows:?}");
        assert!(rows[1].starts_with("▎/skill:steward · /opt/sk/steward/SKILL.md"), "{rows:?}");
        assert!(rows[2].starts_with("▎next keyboard"), "{rows:?}");
        assert!(l.iter().all(|r| line::line_width(r) == 60));
        assert!(!rows.iter().any(|r| r.contains("step")));
    }

    #[test]
    fn skill_invocation_without_args_is_one_header() {
        let text = "<skill name=\"steward\" path=\"/opt/s/SKILL.md\">\nbody\n</skill>";
        let rows: Vec<String> = user_prompt(text, 60).iter().map(text_of).collect();
        assert_eq!(rows.len(), 3, "{rows:?}");
        assert!(rows[1].starts_with("▎/skill:steward"));
    }

    #[test]
    fn skill_header_truncates_to_the_band() {
        let text = "<skill name=\"s\" path=\"/opt/a/very/long/path/that/keeps/going/SKILL.md\">\nb\n</skill>";
        let l = user_prompt(text, 30);
        assert_eq!(l.len(), 3);
        assert!(l.iter().all(|r| line::line_width(r) == 30));
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
        assert_eq!(txt, [" a", " ", " b"]);
        assert_eq!(ls[0][0].style.foreground_color, None);
        assert_eq!(ls[0][1].style.foreground_color, t.color(Role::Thinking));
        assert!(ls[0][1].style.attributes.has(Attribute::Italic));
        assert_eq!(text_of(&thinking_gap()), " ");
    }

    #[test]
    fn tool_header_colours() {
        let t = theme::get();
        let ls = tool_result("read", &json!({"path":"x"}), &ToolOutput::ok("a\nb"), false, 40);
        let h = &ls[0];
        assert_eq!(h[0].style.foreground_color, t.color(Role::Faint));
        assert_eq!(h[1].style.foreground_color, t.color(Role::Tool));
        assert!(h[1].style.attributes.has(Attribute::Bold));
        assert_eq!(h[2].style.foreground_color, t.color(Role::Tool));
        assert_eq!(h[0].text, "● ");
        assert_eq!(h[2].text, " x");
    }
}
