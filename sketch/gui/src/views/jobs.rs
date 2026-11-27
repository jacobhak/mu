//! The jobs view: running and finished jobs, with the selected one's details. PLACEHOLDER: replaced by the
//! agent list and jobs slice.
use super::{ViewCtx, ui};
use gpui::{AnyElement, IntoElement as _, ParentElement as _, Styled as _, px};

pub fn view(c: &ViewCtx) -> AnyElement {
    let mut col = ui(c.t).flex().flex_col().p(px(12.)).gap_1();
    for j in &c.data.jobs {
        col = col.child(format!("{} {}", j.name, j.elapsed));
    }
    col.into_any_element()
}
