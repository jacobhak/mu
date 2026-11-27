//! The review view: the branch's diff by file. PLACEHOLDER: replaced by the agent list and jobs slice.
//! It follows `c.session`, or is locked to it when `c.pinned` (the header then names the session, with 📌).
use super::{ViewCtx, ui};
use gpui::{AnyElement, IntoElement as _, ParentElement as _, Styled as _, px};

pub fn view(c: &ViewCtx) -> AnyElement {
    ui(c.t).p(px(12.)).child(format!("Review · {} {}", c.s().title, if c.pinned { "📌" } else { "" })).into_any_element()
}
