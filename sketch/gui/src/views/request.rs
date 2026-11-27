//! The dock: approval (both steps), questions with a preview, the final review, document review.
//! PLACEHOLDER: replaced by the request and composer slice.
use super::{ViewCtx, ui};
use gpui::{AnyElement, IntoElement as _, ParentElement as _, Styled as _, px};

/// The session's request docked between the transcript and the composer; `None` when nothing waits.
pub fn dock(c: &ViewCtx) -> Option<AnyElement> {
    c.s().request.as_ref()?;
    Some(ui(c.t).p(px(12.)).bg(c.t.raised).border_t_1().border_color(c.t.line).child("request dock").into_any_element())
}
