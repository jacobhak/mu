//! Diff lines with line numbers and whole-line added/removed backgrounds; the review view and the transcript
//! both call `lines`.
use super::mono;
use crate::data::{DiffKind, DiffLine};
use crate::theme::{MONO, Theme};
use gpui::prelude::*;
use gpui::{AnyElement, StyledText, TextRun, div, font, px, relative};

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
    let (sign, bg, hi, sign_fg) = match l.kind {
        DiffKind::Context => (" ", None, t.hover, t.faint),
        DiffKind::Add => ("+", Some(t.diff_add_bg), t.diff_add_hi, t.ok),
        DiffKind::Del => ("−", Some(t.diff_del_bg), t.diff_del_hi, t.err),
    };
    let changed = if l.kind == DiffKind::Context { None } else { l.changed };
    let no = l.no.map(|n| n.to_string()).unwrap_or_default();
    mono(t)
        .flex()
        .flex_none()
        .w_full()
        .when_some(bg, |d, b| d.bg(b))
        .child(
            div()
                .flex()
                .flex_none()
                .justify_end()
                .w(px(t.ty.code * GUTTER_EM))
                .pr(px(t.ty.code * 0.6))
                .text_color(t.faint)
                .child(no),
        )
        .child(
            div().flex_none().w(px(t.ty.code * SIGN_EM)).pl(px(t.ty.code * 0.3)).text_color(sign_fg).child(sign),
        )
        .child(div().flex_1().min_w_0().truncate().child(text(t, l.text, changed, hi)))
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
