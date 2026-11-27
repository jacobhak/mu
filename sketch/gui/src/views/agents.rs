//! The agent list: narrow (in a bar) and wide (as a table in a tile). PLACEHOLDER: replaced by the agent list
//! and jobs slice.
use super::{ViewCtx, ui};
use gpui::{AnyElement, IntoElement as _, ParentElement as _, Styled as _, px};

/// `c.wide` selects the table form.
pub fn view(c: &ViewCtx) -> AnyElement {
    let mut col = ui(c.t).flex().flex_col().p(px(12.)).gap_1();
    for r in &c.data.needs_you {
        col = col.child(format!("{} {}", r.state.icon(), r.title));
    }
    col.into_any_element()
}
