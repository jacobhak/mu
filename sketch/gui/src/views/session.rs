//! A session tile: header, transcript, the request dock, the queue, the composer and the session bar.
//! Owned by the shell; the pieces are the other views' functions.
use super::{ViewCtx, composer, request, transcript, ui};
use crate::theme::metrics;
use gpui::{AnyElement, InteractiveElement as _, IntoElement as _, ParentElement as _, Styled as _, div, px};

/// The widest a lone session tile grows, as a share of the window's width. Past it the shell centres the tile and
/// the chrome shows either side; the content inside always fills the tile.
const MAX_TILE_SHARE: f32 = 0.6;

pub fn max_width(window_w: f32) -> f32 {
    window_w * MAX_TILE_SHARE
}

pub fn view(c: &ViewCtx) -> AnyElement {
    let (t, s) = (c.t, c.s());
    let header = ui(t)
        .flex()
        .flex_none()
        .items_center()
        .gap(px(metrics::GAP))
        .h(px(metrics::ROW_H))
        .px(px(metrics::PAD_X))
        .bg(t.bar)
        .border_b_1()
        .border_color(t.line)
        .child(div().flex_1().truncate().text_color(t.tool).child(s.title))
        .child(div().text_color(t.faint).child(s.host))
        .child(div().text_color(t.faint).child("⋯"));

    let mut col = div().flex().flex_col().size_full().min_h_0().child(header);
    col = col.child(div().id(("transcript", c.session)).flex_1().min_h_0().overflow_hidden().child(transcript::view(c)));
    if let Some(dock) = request::dock(c) {
        col = col.child(div().flex_none().child(dock));
    }
    if let Some(q) = composer::queue(c) {
        col = col.child(div().flex_none().child(q));
    }
    col.child(div().flex_none().relative().child(composer::input(c)))
        .child(div().flex_none().child(composer::session_bar(c)))
        .into_any_element()
}
