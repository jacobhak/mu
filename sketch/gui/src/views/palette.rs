//! The agent palette, the full palette and the floating panel's frame. PLACEHOLDER: replaced by the palettes
//! and floating panel slice.
//!
//! The shell draws the scrim and centres what these return; they size themselves (a palette about 640 px wide).
use super::ui;
use crate::data::{Data, SessionId};
use crate::theme::Theme;
use gpui::{AnyElement, IntoElement as _, ParentElement as _, Styled as _, px};

/// Agents and sessions, most recent first. `selected` indexes `data.palette.agents`.
pub fn agent(t: &Theme, data: &Data, selected: usize) -> AnyElement {
    let _ = data;
    ui(t).w(px(640.)).p(px(12.)).bg(t.raised).child(format!("agent palette · {selected}")).into_any_element()
}

/// Everything: actions with their keys, slash commands, agents, layouts, views. `selected` indexes
/// `data.palette.entries`.
pub fn full(t: &Theme, data: &Data, selected: usize) -> AnyElement {
    let _ = data;
    ui(t).w(px(640.)).p(px(12.)).bg(t.raised).child(format!("full palette · {selected}")).into_any_element()
}

/// The floating panel for a jump to the next request: a header (title, host, "needs you"), then `body`, the
/// session's own tile content (transcript with its request docked), which the shell builds.
pub fn floating(t: &Theme, data: &Data, session: SessionId, body: AnyElement) -> AnyElement {
    ui(t).w(px(820.)).h(px(620.)).bg(t.window).flex().flex_col().child(data.session(session).title).child(body).into_any_element()
}
