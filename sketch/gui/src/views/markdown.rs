//! Parsed markdown in proportional type: headings, paragraphs with inline styles, lists, code blocks.
//! The request dock's plan review calls `render` too, so nothing here assumes the transcript's width.
use super::{mono, prose};
use crate::data::{Md, Span, SpanStyle};
use crate::theme::{MONO, SANS, Theme};
use gpui::prelude::*;
use gpui::{AnyElement, FontWeight, Hsla, StyledText, TextRun, UnderlineStyle, div, font, px, relative};

/// Heading sizes as multiples of the prose size, levels 1..=3.
const HEADING_SCALE: [f32; 3] = [1.4, 1.2, 1.06];
/// Space between blocks, and the extra space above a heading, in px.
const BLOCK_GAP: f32 = 10.0;
const HEADING_TOP: f32 = 8.0;
const LIST_GAP: f32 = 4.0;
const MARKER_W: f32 = 22.0;

pub fn render(t: &Theme, blocks: &[Md]) -> AnyElement {
    let mut col = prose(t).flex().flex_col().w_full().min_w_0().line_height(relative(t.ty.prose_lh));
    for (i, b) in blocks.iter().enumerate() {
        let el = match b {
            Md::Heading(level, s) => heading(t, *level, s).into_any_element(),
            Md::Para(spans) => para(t, spans).into_any_element(),
            Md::List { ordered, items } => list(t, *ordered, items).into_any_element(),
            Md::Code { lang, lines } => code(t, lang, lines).into_any_element(),
        };
        let top = match (i, b) {
            (0, _) => 0.0,
            (_, Md::Heading(..)) => BLOCK_GAP + HEADING_TOP,
            _ => BLOCK_GAP,
        };
        col = col.child(div().flex_none().mt(px(top)).child(el));
    }
    col.into_any_element()
}

fn heading(t: &Theme, level: u8, text: &'static str) -> impl IntoElement {
    let scale = HEADING_SCALE[(level.clamp(1, 3) - 1) as usize];
    div()
        .text_size(px(t.ty.prose * scale))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(t.strong)
        .line_height(relative(1.3))
        .child(text)
}

fn para(t: &Theme, spans: &[Span]) -> impl IntoElement {
    div().child(inline(t, spans))
}

/// One wrapping paragraph: the spans are runs of a single `StyledText`.
fn inline(t: &Theme, spans: &[Span]) -> StyledText {
    let mut text = String::new();
    let mut runs = Vec::new();
    for s in spans {
        if s.text.is_empty() {
            continue;
        }
        text.push_str(s.text);
        let mut f = font(SANS);
        let mut run = TextRun {
            len: s.text.len(),
            font: font(SANS),
            color: t.text,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        match s.style {
            SpanStyle::Plain => {}
            SpanStyle::Code => {
                f = font(MONO);
                run.color = t.literal;
            }
            SpanStyle::Bold => {
                f.weight = FontWeight::SEMIBOLD;
                run.color = t.strong;
            }
            SpanStyle::Italic => f = f.italic(),
            SpanStyle::Link => {
                run.color = t.accent;
                run.underline = Some(UnderlineStyle { thickness: px(1.), color: Some(t.accent), wavy: false });
            }
        }
        run.font = f;
        runs.push(run);
    }
    if text.is_empty() {
        return StyledText::new(" ");
    }
    StyledText::new(text).with_runs(runs)
}

fn list(t: &Theme, ordered: bool, items: &[Vec<Span>]) -> impl IntoElement {
    let mut col = div().flex().flex_col().gap(px(LIST_GAP)).w_full().min_w_0();
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
                        .pr(px(8.))
                        .text_color(t.muted)
                        .child(marker),
                )
                .child(div().flex_1().min_w_0().child(inline(t, item))),
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
        .rounded(px(crate::theme::metrics::RADIUS))
        .when(!lang.is_empty(), |d| {
            d.child(
                div()
                    .px(px(12.))
                    .py(px(4.))
                    .bg(t.bar)
                    .border_b_1()
                    .border_color(t.line_faint)
                    .text_size(px(t.ty.small))
                    .text_color(t.muted)
                    .child(lang),
            )
        })
        .child(div().bg(t.surface).px(px(12.)).py(px(8.)).child(body))
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
        return StyledText::new(line.to_string()).with_runs(vec![colour_of(t.syn_comment, line.len())]);
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
