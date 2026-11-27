//! Queue rows, the composer's states, the `/` menu, the session bar.
use super::{ViewCtx, mono, ui};
use crate::data::{Session, SlashMenu};
use crate::theme::{Theme, metrics};
use gpui::prelude::FluentBuilder as _;
use gpui::{AnyElement, Div, FontWeight, IntoElement as _, ParentElement as _, Styled as _, div, px};

const MENU_W: f32 = 380.0;
const CMD_W: f32 = 96.0;

/// One row per queued message above the composer; `None` when the queue is empty.
pub fn queue(c: &ViewCtx) -> Option<AnyElement> {
    let (t, s) = (c.t, c.s());
    if s.queue.is_empty() {
        return None;
    }
    let rows = s.queue.iter().map(|q| {
        mono(t)
            .flex()
            .items_center()
            .gap(px(8.))
            .text_size(px(t.ty.small + 0.5))
            .text_color(t.muted)
            .child(div().flex_1().min_w_0().truncate().child(format!("↳ {}: {}", q.kind, q.text)))
            .when(q.newest, |d| d.child(div().flex_none().text_color(t.faint).child("shift+↑ edit")))
    });
    Some(
        div()
            .flex()
            .flex_col()
            .gap(px(2.))
            .px(px(metrics::PAD))
            .pt(px(metrics::PAD_SM))
            .pb(px(2.))
            .children(rows)
            .into_any_element(),
    )
}

/// The input box: typed text, ghost text, image chip; the `/` menu opens above it.
pub fn input(c: &ViewCtx) -> AnyElement {
    let (t, s) = (c.t, c.s());
    let cm = &s.composer;
    let empty = cm.before.is_empty() && cm.after.is_empty();
    let cursor = div().text_color(if c.focused { t.accent } else { t.faint }).child("▌");
    let text = if empty && cm.ghost.is_empty() {
        // The cursor sits at the start; the placeholder follows it.
        mono(t).flex().flex_wrap().child(cursor).child(div().text_color(t.faint).child(cm.placeholder))
    } else {
        mono(t)
            .flex()
            .flex_wrap()
            .child(div().text_color(t.strong).child(cm.before))
            .child(cursor)
            .child(div().text_color(t.strong).child(cm.after))
            .child(div().text_color(t.faint).child(cm.ghost))
    };
    let mut boxed = div()
        .flex()
        .flex_col()
        .gap(px(6.))
        .px(px(metrics::PAD))
        .pt(px(metrics::PAD_SM + 2.))
        .pb(px(metrics::PAD_SM))
        .border_t_1()
        .border_color(t.line)
        .child(text);
    if let Some((name, size)) = cm.image {
        boxed = boxed.child(image_chip(t, name, size));
    }
    let mut wrap = div().relative().child(boxed);
    if let Some(menu) = &cm.slash {
        wrap = wrap.child(slash_menu(t, menu));
    }
    wrap.into_any_element()
}

fn image_chip(t: &Theme, name: &'static str, size: &'static str) -> Div {
    div().flex().child(
        mono(t)
            .px(px(8.))
            .py(px(1.))
            .rounded(px(10.))
            .bg(t.surface)
            .border_1()
            .border_color(t.line)
            .text_size(px(t.ty.small))
            .text_color(t.muted)
            .child(format!("[image 1 · {name} {size}]")),
    )
}

fn slash_menu(t: &Theme, m: &SlashMenu) -> Div {
    let rows = m.entries.iter().enumerate().map(|(i, (cmd, desc))| {
        let on = i == m.selected;
        mono(t)
            .flex()
            .gap(px(10.))
            .px(px(8.))
            .h(px(metrics::ROW_H))
            .items_center()
            .rounded(px(4.))
            .when(on, |d| d.bg(t.selected))
            .child(
                div()
                    .flex_none()
                    .w(px(CMD_W))
                    .text_color(if on { t.accent } else { t.strong })
                    .when(on, |d| d.font_weight(FontWeight::SEMIBOLD))
                    .child(*cmd),
            )
            .child(div().flex_1().min_w_0().truncate().text_color(t.muted).child(*desc))
    });
    div().absolute().bottom_full().left(px(metrics::PAD)).pb(px(4.)).child(
        div()
            .flex()
            .flex_col()
            .w(px(MENU_W))
            .p(px(4.))
            .rounded(px(metrics::RADIUS))
            .bg(t.raised)
            .border_1()
            .border_color(t.line)
            .shadow_md()
            .child(div().px(px(8.)).pb(px(2.)).text_size(px(t.ty.small)).text_color(t.faint).child("/"))
            .children(rows),
    )
}

fn chip(t: &Theme, text: String, fg: gpui::Hsla) -> Div {
    ui(t).flex_none().whitespace_nowrap().px(px(7.)).py(px(1.)).rounded(px(10.)).text_size(px(t.ty.small)).text_color(fg).child(text)
}

fn mode_colour(t: &Theme, mode: &str) -> gpui::Hsla {
    match mode {
        "ask" => t.warn,
        "plan" => t.accent,
        "yolo" => t.err,
        _ => t.muted,
    }
}

fn ctx_colour(t: &Theme, pct: u8) -> gpui::Hsla {
    match pct {
        90.. => t.err,
        75.. => t.warn,
        _ => t.muted,
    }
}

/// The status line's segments as chips: model, thinking, mode, branch, context, cost, turn timer, stop.
pub fn session_bar(c: &ViewCtx) -> AnyElement {
    let (t, s): (&Theme, &Session) = (c.t, c.s());
    let mode = mode_colour(t, s.mode);
    let bar = ui(t)
        .flex()
        .items_center()
        .gap(px(4.))
        .h(px(metrics::STATUS_BAR_H + 4.))
        .px(px(metrics::PAD - 7.))
        .pb(px(2.))
        .overflow_hidden()
        .when(!c.focused, |d| d.opacity(0.6))
        .child(chip(t, s.model.to_string(), t.strong).font_weight(FontWeight::MEDIUM))
        .child(chip(t, s.thinking.to_string(), t.muted))
        .child(chip(t, s.mode.to_string(), mode).bg(mode.opacity(0.14)))
        .child(chip(t, s.branch.to_string(), t.muted).min_w_0().truncate())
        .child(div().flex_1())
        .child(chip(t, format!("ctx {}%", s.ctx_pct), ctx_colour(t, s.ctx_pct)))
        .child(chip(t, s.cost.to_string(), t.muted));
    let bar = match s.turn_timer {
        Some(timer) => bar
            .child(chip(t, format!("⠋ {timer}"), t.accent))
            .child(
                chip(t, "■".to_string(), t.err)
                    .px(px(8.))
                    .bg(t.err.opacity(0.14))
                    .text_size(px(t.ty.ui)),
            ),
        None => bar,
    };
    bar.into_any_element()
}
