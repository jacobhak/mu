//! Parsed markdown in proportional type: headings, paragraphs with inline styles, lists, code blocks, quotes,
//! rules and tables, drawn with the TUI's roles. The request dock's plan review calls `render` too, so nothing
//! here assumes the transcript's width.
use super::{mono, prose};
use crate::data::{Align, Md, Span, SpanStyle, Table};
use crate::theme::{MONO, SANS, Theme, metrics};
use gpui::prelude::*;
use gpui::{
    AnyElement, FontWeight, Hsla, StrikethroughStyle, StyledText, TextRun, UnderlineStyle, div, font, px, relative,
};

/// The width of a list's marker column.
const MARKER_W: f32 = 24.0;
/// The `▏` bar of a quote, one per line; the quote clips the ones past its height.
const QUOTE_BAR_LINES: usize = 40;
/// The bar's column; the quote's text starts after it.
const QUOTE_BAR_W: f32 = metrics::PAD_X;
/// The `─` run of a rule; the block clips it to the width the transcript gives.
const RULE_GLYPHS: usize = 200;

pub fn render(t: &Theme, blocks: &[Md]) -> AnyElement {
    let mut col = prose(t).flex().flex_col().w_full().min_w_0().line_height(relative(t.ty.prose_lh));
    for (i, b) in blocks.iter().enumerate() {
        let el = match b {
            Md::Heading(level, s) => heading(t, *level, s).into_any_element(),
            Md::Para(spans) => para(t, spans).into_any_element(),
            Md::List { ordered, items } => list(t, *ordered, items).into_any_element(),
            Md::Code { lang, lines } => code(t, lang, lines).into_any_element(),
            Md::Quote(spans) => quote(t, spans).into_any_element(),
            Md::Rule => rule(t).into_any_element(),
            Md::Table(table) => table_block(t, table).into_any_element(),
        };
        // A heading opens a section; other blocks are set off by a GAP.
        let top = match (i, b) {
            (0, _) => 0.0,
            (_, Md::Heading(..)) => metrics::GAP_LG,
            _ => metrics::GAP,
        };
        col = col.child(div().flex_none().mt(px(top)).child(el));
    }
    col.into_any_element()
}

fn heading(t: &Theme, level: u8, text: &'static str) -> impl IntoElement {
    let level = level.clamp(1, 3);
    // As the TUI: the hashes in faint bold, the title in the tool role, bold.
    let prefix = format!("{} ", "#".repeat(level as usize));
    let face = || {
        let mut f = font(SANS);
        f.weight = FontWeight::SEMIBOLD;
        f
    };
    let run = |len: usize, color: Hsla| TextRun {
        len,
        font: face(),
        color,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let runs = vec![run(prefix.len(), t.faint), run(text.len(), t.tool)];
    div()
        .text_size(px(t.ty.prose))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(t.tool)
        .child(StyledText::new(format!("{prefix}{text}")).with_runs(runs))
}

fn para(t: &Theme, spans: &[Span]) -> impl IntoElement {
    div().child(inline(t, spans, Tone::Body))
}

/// How a run of text sits in its block: a quote's text is muted and italic, a table header's is bold, and
/// the rest is the body's.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tone {
    Body,
    Quote,
    Head,
}

/// One wrapping paragraph: the spans are runs of a single `StyledText`.
fn inline(t: &Theme, spans: &[Span], tone: Tone) -> StyledText {
    let mut text = String::new();
    let mut runs = Vec::new();
    // As the TUI: plain text takes the default foreground; a quote's text takes the muted tone.
    let base = if tone == Tone::Quote { t.muted } else { t.text };
    for s in spans {
        if s.text.is_empty() {
            continue;
        }
        text.push_str(s.text);
        let mut f = font(SANS);
        let mut color = base;
        let mut italic = tone == Tone::Quote;
        let mut bold = tone == Tone::Head;
        let mut underline = None;
        let mut strikethrough = None;
        match s.style {
            SpanStyle::Plain => {}
            SpanStyle::Code => {
                // The TUI draws inline code in `Strong`, no fill; only the face tells it from prose.
                f = font(MONO);
                color = t.strong;
            }
            // As the TUI: the bold attribute alone, in the surrounding colour.
            SpanStyle::Bold => bold = true,
            SpanStyle::Italic => italic = true,
            SpanStyle::Strike => {
                strikethrough = Some(StrikethroughStyle { thickness: px(1.), color: Some(color) });
            }
            SpanStyle::Link => {
                // As the TUI: underlined, in the `Literal` role.
                color = t.literal;
                underline = Some(UnderlineStyle { thickness: px(1.), color: Some(t.literal), wavy: false });
            }
        }
        if italic {
            f = f.italic();
        }
        if bold {
            f.weight = FontWeight::SEMIBOLD;
        }
        runs.push(TextRun { len: s.text.len(), font: f, color, background_color: None, underline, strikethrough });
    }
    if text.is_empty() {
        return StyledText::new(" ");
    }
    StyledText::new(text).with_runs(runs)
}

/// A quote: a `▏` bar down its left edge in the line role, the text muted and italic. As the TUI prefixes
/// every wrapped line, the bar runs the quote's full height, clipped by the block.
fn quote(t: &Theme, spans: &[Span]) -> impl IntoElement {
    let bars = (0..QUOTE_BAR_LINES).map(|_| div().flex_none().child("▏"));
    let bar = prose(t)
        .absolute()
        .top_0()
        .bottom_0()
        .left_0()
        .w(px(QUOTE_BAR_W))
        .overflow_hidden()
        .flex()
        .flex_col()
        .line_height(relative(t.ty.prose_lh))
        .text_color(t.line)
        .children(bars);
    div()
        .relative()
        .w_full()
        .min_w_0()
        .pl(px(QUOTE_BAR_W))
        .child(bar)
        .child(div().min_w_0().child(inline(t, spans, Tone::Quote)))
}

/// A thematic break: a `─` run in the line role.
fn rule(t: &Theme) -> impl IntoElement {
    prose(t)
        .w_full()
        .min_w_0()
        .overflow_hidden()
        .whitespace_nowrap()
        .text_color(t.line)
        .child("─".repeat(RULE_GLYPHS))
}

/// A table: borders in the line role, the header bold, each cell aligned as its column says.
fn table_block(t: &Theme, tb: &Table) -> impl IntoElement {
    let row = |cells: &[Vec<Span>], tone: Tone| {
        let mut r = div().flex().w_full().min_w_0();
        for (i, cell) in cells.iter().enumerate() {
            let mut c = div()
                .flex_1()
                .min_w_0()
                .px(px(metrics::ROW_PX))
                .py(px(metrics::ROW_PY))
                .border_color(t.line);
            c = match tb.aligns.get(i).copied().unwrap_or(Align::Left) {
                Align::Left => c,
                Align::Center => c.text_center(),
                Align::Right => c.text_right(),
            };
            if i > 0 {
                c = c.border_l_1();
            }
            r = r.child(c.child(inline(t, cell, tone)));
        }
        r
    };
    let mut col = prose(t)
        .flex()
        .flex_col()
        .w_full()
        .min_w_0()
        .overflow_hidden()
        .rounded(px(metrics::RADIUS))
        .border_1()
        .border_color(t.line)
        .child(row(tb.head.as_slice(), Tone::Head));
    for r in &tb.rows {
        col = col.child(row(r.as_slice(), Tone::Body).border_t_1().border_color(t.line));
    }
    col
}

fn list(t: &Theme, ordered: bool, items: &[Vec<Span>]) -> impl IntoElement {
    let mut col = div().flex().flex_col().gap(px(metrics::GAP_SM)).w_full().min_w_0();
    for (i, item) in items.iter().enumerate() {
        let marker = if ordered { format!("{}.", i + 1) } else { "•".to_string() };
        col = col.child(
            div()
                .flex()
                .items_start()
                .child(
                    div()
                        .flex()
                        .flex_none()
                        .justify_end()
                        .w(px(MARKER_W))
                        .pr(px(metrics::GAP))
                        .text_color(t.literal)
                        .child(marker),
                )
                .child(div().flex_1().min_w_0().child(inline(t, item, Tone::Body))),
        );
    }
    col
}

fn code(t: &Theme, lang: &'static str, lines: &[&'static str]) -> impl IntoElement {
    let mut body = mono(t).flex().flex_col().line_height(relative(t.ty.code_lh));
    for l in lines {
        body = body.child(div().flex_none().truncate().child(highlight(t, l)));
    }
    // A block: a label strip (the language) over the code on the darkest band.
    div()
        .flex()
        .flex_col()
        .w_full()
        .min_w_0()
        .overflow_hidden()
        .border_1()
        .border_color(t.line)
        .rounded(px(metrics::RADIUS))
        .when(!lang.is_empty(), |d| {
            d.child(
                div()
                    .px(px(metrics::PAD_X))
                    .py(px(metrics::PAD_Y))
                    .bg(t.bar)
                    .border_b_1()
                    .border_color(t.line)
                    .text_size(px(t.ty.small))
                    .text_color(t.faint)
                    .child(lang),
            )
        })
        .child(div().bg(t.surface).px(px(metrics::PAD_X)).py(px(metrics::PAD_Y)).child(body))
}

const KEYWORDS: &[&str] = &[
    "fn", "let", "mut", "if", "else", "match", "for", "while", "loop", "return", "use", "pub", "struct", "enum",
    "impl", "trait", "mod", "const", "static", "async", "await", "self", "Self", "true", "false", "None", "Some",
    "def", "class", "import", "from", "function", "var", "in", "as", "where", "type",
];

/// A cheap line tokenizer: comments, strings, numbers and keywords; everything else is plain.
fn highlight(t: &Theme, line: &str) -> StyledText {
    if line.is_empty() {
        return StyledText::new(" ");
    }
    let colour_of = |c: Hsla, len: usize| TextRun {
        len,
        font: font(MONO),
        color: c,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let trimmed = line.trim_start();
    if trimmed.starts_with("//") || trimmed.starts_with('#') {
        // Comments are italic in the TUI's highlighter.
        let mut run = colour_of(t.syn_comment, line.len());
        run.font = font(MONO).italic();
        return StyledText::new(line.to_string()).with_runs(vec![run]);
    }
    let mut runs: Vec<TextRun> = Vec::new();
    let mut push = |c: Hsla, len: usize| match runs.last_mut() {
        Some(r) if r.color == c => r.len += len,
        _ => runs.push(colour_of(c, len)),
    };
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let ch = line[i..].chars().next().unwrap_or(' ');
        let start = i;
        if ch == '"' {
            i += 1;
            while i < bytes.len() && bytes[i] != b'"' {
                i += 1;
            }
            i = (i + 1).min(bytes.len());
            push(t.syn_string, i - start);
        } else if ch.is_ascii_alphanumeric() || ch == '_' {
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            let word = &line[start..i];
            let c = if KEYWORDS.contains(&word) {
                t.syn_keyword
            } else if word.bytes().next().is_some_and(|b| b.is_ascii_digit()) {
                t.syn_number
            } else {
                t.text
            };
            push(c, i - start);
        } else {
            i += ch.len_utf8();
            push(if ch.is_whitespace() { t.text } else { t.syn_punct }, i - start);
        }
    }
    StyledText::new(line.to_string()).with_runs(runs)
}
