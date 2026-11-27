//! Parsed markdown in proportional type: headings, paragraphs with inline styles, lists, code blocks.
//! PLACEHOLDER: the transcript slice owns this file; the request dock's plan review calls `render` too.
use super::prose;
use crate::data::Md;
use crate::theme::Theme;
use gpui::{AnyElement, IntoElement as _, ParentElement as _, Styled as _};

pub fn render(t: &Theme, blocks: &[Md]) -> AnyElement {
    let mut col = prose(t).flex().flex_col().gap_2();
    for b in blocks {
        col = col.child(match b {
            Md::Heading(_, s) => (*s).to_string(),
            Md::Para(spans) => spans.iter().map(|s| s.text).collect::<String>(),
            Md::List { items, .. } => items.iter().map(|i| i.iter().map(|s| s.text).collect::<String>()).collect::<Vec<_>>().join("\n"),
            Md::Code { lines, .. } => lines.join("\n"),
        });
    }
    col.into_any_element()
}
