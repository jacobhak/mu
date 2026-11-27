//! Every block type, at rest and streaming. PLACEHOLDER: replaced by the transcript slice.
use super::{ViewCtx, ui};
use gpui::{AnyElement, IntoElement as _, ParentElement as _, Styled as _, px};

/// The session's transcript as a scrollable column: blocks in order, "↓ new output" when `new_output`.
pub fn view(c: &ViewCtx) -> AnyElement {
    ui(c.t)
        .p(px(12.))
        .text_color(c.t.muted)
        .child(format!("transcript: {} blocks of {}", c.s().blocks.len(), c.s().title))
        .into_any_element()
}
