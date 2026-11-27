//! Diff lines with line numbers and whole-line added/removed backgrounds; the review view and the transcript
//! both call `lines`.
use super::{mono, ui};
use crate::data::{Diff, DiffKind, DiffLine};
use crate::theme::{MONO, Theme, metrics};
use gpui::prelude::*;
use gpui::{AnyElement, StyledText, TextRun, div, font, px, relative};

/// The lines of one diff as a column (no header, no border: the caller frames it).
pub fn lines(t: &Theme, lines: &[DiffLine]) -> AnyElement {
    // As the TUI's `diffview`: all-added rows read as a listing (faint numbers, no signs); otherwise the sign and
    // number are one column in the line's role colour, then a `│`.
    let listing = !lines.is_empty() && lines.iter().all(|l| l.kind == DiffKind::Add);
    let w = lines.iter().filter_map(|l| l.no).map(|n| n.to_string().len()).max().unwrap_or(0);
    let mut col = mono(t).flex().flex_col().w_full().min_w_0().line_height(relative(t.ty.code_lh));
    for l in lines {
        col = col.child(row(t, l, listing, w));
    }
    col.into_any_element()
}

fn row(t: &Theme, l: &DiffLine, listing: bool, w: usize) -> impl IntoElement {
    let (sign, role) = match l.kind {
        DiffKind::Context => (' ', t.faint),
        DiffKind::Add => ('+', t.ok),
        DiffKind::Del => ('-', t.err),
    };
    let n = l.no.map(|n| n.to_string()).unwrap_or_default();
    let changed = if l.kind == DiffKind::Context || listing { None } else { l.changed };
    let code_fg = match l.kind {
        DiffKind::Context => t.muted,
        _ if listing => t.text,
        _ => role,
    };
    let gutter = if listing { format!("{n:>w$} ") } else { format!("{sign}{n:>w$}") };
    mono(t)
        .flex()
        .flex_none()
        .w_full()
        .whitespace_nowrap()
        .child(
            div()
                .flex_none()
                .px(px(metrics::GAP))
                .text_color(if listing { t.faint } else { role })
                .child(gutter),
        )
        .when(!listing, |d| {
            d.child(div().flex_none().pr(px(metrics::GAP)).text_color(t.line).child("│"))
        })
        .child(div().flex_1().min_w_0().truncate().child(text(t, l.text, changed, code_fg)))
}

/// One edit as a card: a header bar (verb, file, counts), the lines, and a footer for the lines not shown.
pub fn card(t: &Theme, d: &Diff) -> gpui::Div {
    // As the TUI's edit card: `✎ Edit: path ⟦+adds/-dels⟧` in the tool role, the counts in ok and err, the rule
    // in the card's tone (line), `… n more lines` faint.
    let header = ui(t)
        .flex()
        .items_center()
        .gap(px(metrics::GAP))
        .px(px(metrics::PAD_X))
        .py(px(metrics::PAD_Y))
        .bg(t.surface)
        .border_b_1()
        .border_color(t.line)
        .child(div().flex_none().text_color(t.tool).child("✎ Edit"))
        .child(mono(t).flex_1().min_w_0().truncate().text_color(t.tool).child(d.file))
        .child(
            ui(t)
                .flex()
                .flex_none()
                .text_color(t.faint)
                .child("⟦")
                .child(div().text_color(t.ok).child(format!("+{}", d.plus)))
                .child("/")
                .child(div().text_color(t.err).child(format!("-{}", d.minus)))
                .child("⟧"),
        );
    let footer = ui(t)
        .px(px(metrics::PAD_X))
        .py(px(metrics::PAD_Y))
        .bg(t.surface)
        .border_t_1()
        .border_color(t.line)
        .text_size(px(t.ty.small))
        .text_color(t.faint)
        .child(format!("… {} more lines", d.more));
    div()
        .w_full()
        .overflow_hidden()
        .rounded(px(metrics::RADIUS))
        .border_1()
        .border_color(t.line)
        .child(header)
        .child(div().px(px(metrics::PAD_X)).py(px(metrics::PAD_Y)).child(lines(t, &d.lines)))
        .when(d.more > 0, |c| c.child(footer))
}

/// The line's text with the changed char range on a stronger background.
fn text(t: &Theme, text: &'static str, changed: Option<(usize, usize)>, fg: gpui::Hsla) -> StyledText {
    if text.is_empty() {
        return StyledText::new(" ");
    }
    let byte = |chars: usize| text.char_indices().nth(chars).map_or(text.len(), |(i, _)| i);
    let (a, b) = changed.map_or((0, 0), |(a, b)| (byte(a), byte(b.max(a))));
    // The changed part is reversed, as in the TUI: the line's hue as background, the surface as text.
    let run = |len: usize, changed: bool| TextRun {
        len,
        font: font(MONO),
        color: if changed { t.surface } else { fg },
        background_color: changed.then_some(fg),
        underline: None,
        strikethrough: None,
    };
    let mut runs = Vec::new();
    for (len, changed) in [(a, false), (b - a, true), (text.len() - b, false)] {
        if len > 0 {
            runs.push(run(len, changed));
        }
    }
    StyledText::new(text).with_runs(runs)
}
