//! Diff lines with line numbers and whole-line added/removed backgrounds. PLACEHOLDER: the transcript slice
//! owns this file; the review view and the transcript both call `lines`.
use super::mono;
use crate::data::{DiffKind, DiffLine};
use crate::theme::Theme;
use gpui::{AnyElement, IntoElement as _, ParentElement as _, Styled as _};

/// The lines of one diff as a column (no header, no border: the caller frames it).
pub fn lines(t: &Theme, lines: &[DiffLine]) -> AnyElement {
    let mut col = mono(t).flex().flex_col();
    for l in lines {
        let (sign, bg) = match l.kind {
            DiffKind::Context => (' ', t.window),
            DiffKind::Add => ('+', t.diff_add_bg),
            DiffKind::Del => ('−', t.diff_del_bg),
        };
        col = col.child(mono(t).bg(bg).child(format!("{:>4} {sign} {}", l.no.unwrap_or(0), l.text)));
    }
    col.into_any_element()
}
