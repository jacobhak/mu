//! The review view: the branch's diff by file. It follows `c.session`, or is locked to it when `c.pinned`
//! (the header then names the session, with 📌).
use super::{ViewCtx, diff, mono, ui};
use crate::data::ReviewFile;
use crate::theme::{Theme, metrics};
use gpui::prelude::*;
use gpui::{AnyElement, Div, FontWeight, div, px};

fn small(t: &Theme) -> Div {
    ui(t).text_size(px(t.ty.small))
}

fn counts(t: &Theme, plus: u32, minus: u32) -> Div {
    small(t)
        .flex()
        .flex_none()
        .gap(px(6.))
        .child(div().text_color(t.ok).child(format!("+{plus}")))
        .child(div().text_color(t.err).child(format!("−{minus}")))
}

pub fn view(c: &ViewCtx) -> AnyElement {
    let t = c.t;
    let r = &c.data.review;

    let title = if c.pinned {
        format!("Review · {}  📌", c.s().title)
    } else {
        "Review · following".to_string()
    };
    let header = div()
        .flex()
        .flex_col()
        .gap(px(2.))
        .px(px(metrics::PAD))
        .py(px(metrics::PAD_SM + 2.))
        .bg(t.bar)
        .border_b_1()
        .border_color(t.line)
        .child(ui(t).font_weight(FontWeight::SEMIBOLD).text_color(t.strong).truncate().child(title))
        .child(
            small(t)
                .flex()
                .gap(px(10.))
                .child(div().min_w_0().truncate().text_color(t.muted).child(r.scope))
                .child(counts(t, r.plus, r.minus)),
        );

    let mut body = div()
        .id(("review-scroll", c.session))
        .flex()
        .flex_col()
        .flex_1()
        .min_h_0()
        .overflow_y_scroll();
    for f in &r.files {
        body = body.child(file(t, f));
    }

    div().flex().flex_col().size_full().bg(t.window).child(header).child(body).into_any_element()
}

fn file(t: &Theme, f: &ReviewFile) -> Div {
    let head = mono(t)
        .flex()
        .items_center()
        .gap(px(6.))
        .px(px(metrics::PAD))
        .py(px(4.))
        .py(px(6.))
        .bg(t.surface)
        .border_y_1()
        .border_color(t.line)
        .child(ui(t).flex_none().text_color(t.muted).child(if f.expanded { "▾" } else { "▸" }))
        .child(div().flex_1().min_w_0().truncate().text_color(t.strong).child(f.path))
        .child(counts(t, f.plus, f.minus));
    let d = div().flex().flex_col().child(head);
    if f.expanded {
        d.child(div().min_w_0().overflow_hidden().child(diff::lines(t, &f.lines)))
    } else {
        d
    }
}
