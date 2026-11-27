//! Diff lines with line numbers and whole-line added/removed backgrounds; the review view and the transcript
//! both call `lines`.
use super::{mono, ui};
use crate::data::{Diff, DiffKind, DiffLine};
use crate::theme::{MONO, Theme, metrics};
use gpui::prelude::*;
use gpui::{AnyElement, FontWeight, StyledText, TextRun, div, font, px, relative};

/// Gutter and sign column widths, in code-size ems (Lilex advances ~0.6em per cell).
const GUTTER_EM: f32 = 3.6;
const SIGN_EM: f32 = 1.8;

/// The lines of one diff as a column (no header, no border: the caller frames it).
pub fn lines(t: &Theme, lines: &[DiffLine]) -> AnyElement {
    let mut col = mono(t).flex().flex_col().w_full().min_w_0().line_height(relative(t.ty.code_lh));
    for l in lines {
        col = col.child(row(t, l));
    }
    col.into_any_element()
}

fn row(t: &Theme, l: &DiffLine) -> impl IntoElement {
    let (sign, bg, gutter_bg, hi, sign_fg, no_fg) = match l.kind {
        DiffKind::Context => (" ", None, t.surface, t.hover, t.faint, t.faint),
        DiffKind::Add => ("+", Some(t.diff_add_bg), t.diff_add_gutter, t.diff_add_hi, t.ok, t.text),
        DiffKind::Del => ("−", Some(t.diff_del_bg), t.diff_del_gutter, t.diff_del_hi, t.err, t.text),
    };
    let changed = if l.kind == DiffKind::Context { None } else { l.changed };
    let no = l.no.map(|n| n.to_string()).unwrap_or_default();
    mono(t)
        .flex()
        .flex_none()
        .w_full()
        .when_some(bg, |d, b| d.bg(b))
        // The gutter is its own band, so the numbers read as a column apart from the code.
        .child(
            div()
                .flex()
                .flex_none()
                .justify_end()
                .w(px(t.ty.code * GUTTER_EM))
                .pr(px(t.ty.code * 0.6))
                .bg(gutter_bg)
                .text_color(no_fg)
                .child(no),
        )
        .child(
            div()
                .flex_none()
                .w(px(t.ty.code * SIGN_EM))
                .pl(px(t.ty.code * 0.4))
                .font_weight(FontWeight::BOLD)
                .text_color(sign_fg)
                .child(sign),
        )
        .child(div().flex_1().min_w_0().truncate().child(text(t, l.text, changed, hi)))
}

/// One edit as a card: a header bar (verb, file, counts), the lines, and a footer for the lines not shown.
pub fn card(t: &Theme, d: &Diff) -> gpui::Div {
    let header = ui(t)
        .flex()
        .items_center()
        .gap(px(8.))
        .px(px(12.))
        .py(px(6.))
        .bg(t.surface)
        .border_b_1()
        .border_color(t.line_faint)
        .child(div().flex_none().text_color(t.tool).child("✎"))
        .child(div().flex_none().text_color(t.muted).child("Edit"))
        .child(mono(t).flex_1().min_w_0().truncate().text_color(t.strong).child(d.file))
        .child(div().flex_none().text_color(t.ok).child(format!("+{}", d.plus)))
        .child(div().flex_none().text_color(t.err).child(format!("−{}", d.minus)));
    let footer = ui(t)
        .px(px(12.))
        .py(px(4.))
        .bg(t.surface)
        .border_t_1()
        .border_color(t.line_faint)
        .text_size(px(t.ty.small))
        .text_color(t.muted)
        .child(format!("{} more lines", d.more));
    div()
        .w_full()
        .overflow_hidden()
        .rounded(px(metrics::RADIUS))
        .border_1()
        .border_color(t.line)
        .child(header)
        .child(lines(t, &d.lines))
        .when(d.more > 0, |c| c.child(footer))
}

/// The line's text with the changed char range on a stronger background.
fn text(t: &Theme, text: &'static str, changed: Option<(usize, usize)>, hi: gpui::Hsla) -> StyledText {
    if text.is_empty() {
        return StyledText::new(" ");
    }
    let byte = |chars: usize| text.char_indices().nth(chars).map_or(text.len(), |(i, _)| i);
    let (a, b) = changed.map_or((0, 0), |(a, b)| (byte(a), byte(b.max(a))));
    let run = |len: usize, background: Option<gpui::Hsla>| TextRun {
        len,
        font: font(MONO),
        color: t.text,
        background_color: background,
        underline: None,
        strikethrough: None,
    };
    let mut runs = Vec::new();
    for (len, bg) in [(a, None), (b - a, Some(hi)), (text.len() - b, None)] {
        if len > 0 {
            runs.push(run(len, bg));
        }
    }
    StyledText::new(text).with_runs(runs)
}
