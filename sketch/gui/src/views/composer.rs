//! Queue rows, the composer's states, the `/` menu, the session bar. PLACEHOLDER: replaced by the request and
//! composer slice.
use super::{ViewCtx, mono, ui};
use gpui::{AnyElement, IntoElement as _, ParentElement as _, Styled as _, px};

/// One row per queued message above the composer; `None` when the queue is empty.
pub fn queue(c: &ViewCtx) -> Option<AnyElement> {
    if c.s().queue.is_empty() {
        return None;
    }
    Some(ui(c.t).px(px(12.)).text_color(c.t.muted).child("queue").into_any_element())
}

/// The input box: typed text, ghost text, image chip; the `/` menu opens above it (position it absolutely
/// inside a `relative()` wrapper that the box itself provides).
pub fn input(c: &ViewCtx) -> AnyElement {
    let s = c.s();
    mono(c.t).p(px(12.)).child(format!("{}{}", s.composer.before, s.composer.placeholder)).into_any_element()
}

/// The status line's segments as chips: model, thinking, mode, branch, context, cost, turn timer, stop.
pub fn session_bar(c: &ViewCtx) -> AnyElement {
    let s = c.s();
    ui(c.t).px(px(12.)).pb(px(8.)).text_color(c.t.muted).child(format!("{} · {} · {}", s.model, s.thinking, s.mode)).into_any_element()
}
