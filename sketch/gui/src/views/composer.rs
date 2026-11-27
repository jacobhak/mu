//! Queue rows, the composer's states, the `/` menu, the session bar.
use super::{ViewCtx, ui};
use crate::data::{ContextWindow, Session, SlashMenu};
use crate::theme::{Lift, Theme, metrics};
use gpui::prelude::FluentBuilder as _;
use gpui::{AnyElement, Div, IntoElement as _, ParentElement as _, Styled as _, div, px, relative};

const MENU_W: f32 = 380.0;
const CMD_W: f32 = 96.0;

/// One row per queued message above the composer; `None` when the queue is empty.
pub fn queue(c: &ViewCtx) -> Option<AnyElement> {
    let (t, s) = (c.t, c.s());
    if s.queue.is_empty() {
        return None;
    }
    let rows = s.queue.iter().map(|q| {
        ui(t)
            .flex()
            .items_center()
            .gap(px(metrics::GAP))
            .px(px(metrics::PAD_X))
            .py(px(metrics::ROW_PY))
            .text_size(px(t.ty.small))
            .text_color(t.muted)
            .child(div().flex_1().min_w_0().truncate().child(format!("↳ {}: {}", q.kind, q.text)))
            .when(q.newest, |d| d.child(div().flex_none().text_color(t.muted).child("shift+↑ edit")))
    });
    // Indented by the composer's own padding and the card's, so the queued text lines up with the typed text.
    Some(
        div()
            .flex()
            .flex_col()
            .gap(px(metrics::GAP_SM))
            .px(px(metrics::PAD))
            .pt(px(metrics::PAD_SM))
            .pb(px(metrics::S0_5))
            .children(rows)
            .into_any_element(),
    )
}

/// The input box: prompt, typed text, ghost text, image chip; the `/` menu opens above it.
pub fn input(c: &ViewCtx) -> AnyElement {
    let (t, s) = (c.t, c.s());
    let cm = &s.composer;
    let empty = cm.before.is_empty() && cm.after.is_empty();
    let caret = || div().flex_none().w(px(2.)).h(px(t.ty.ui)).bg(if c.focused { t.accent } else { t.faint });
    let prompt = || div().flex_none().pr(px(metrics::S1_5)).text_color(t.accent).child("μ ❯");
    let field = ui(t).line_height(relative(1.4)).flex().flex_wrap().items_center();
    let text = if empty && cm.ghost.is_empty() {
        // The cursor sits after the prompt; the placeholder follows it.
        field.child(prompt()).child(caret()).child(div().pl(px(metrics::S0_5)).text_color(t.faint).child(cm.placeholder))
    } else {
        field
            .child(prompt())
            .child(div().text_color(t.text).child(cm.before))
            .child(caret())
            .child(div().text_color(t.text).child(cm.after))
            .child(div().text_color(t.faint).child(cm.ghost))
    };
    // An input card: lifted off the tile. Its edge is the TUI's composer rule, which is LineFaint.
    let mut card = div()
        .flex()
        .flex_col()
        .gap(px(metrics::GAP_SM))
        .min_h(px(metrics::CONTROL_H))
        .px(px(metrics::PAD_X))
        .py(px(metrics::PAD_Y))
        .rounded(px(metrics::RADIUS))
        .bg(t.raised)
        .border_1()
        .border_color(t.line_faint)
        .shadow(t.shadow(Lift::Rest))
        .child(text);
    if let Some((name, size)) = cm.image {
        card = card.child(image_chip(t, name, size));
    }
    let mut wrap = div().relative().px(px(metrics::PAD)).pt(px(metrics::PAD_SM)).pb(px(metrics::S0_5)).child(card);
    if let Some(menu) = &cm.slash {
        wrap = wrap.child(slash_menu(t, menu));
    }
    wrap.into_any_element()
}

fn image_chip(t: &Theme, name: &'static str, size: &'static str) -> Div {
    div().flex().child(
        ui(t)
            .flex()
            .items_center()
            .min_h(px(metrics::CONTROL_H_SM))
            .px(px(metrics::PAD_X))
            .rounded(px(metrics::RADIUS_SM))
            .bg(t.surface)
            .border_1()
            .border_color(t.line)
            .text_size(px(t.ty.small))
            .text_color(t.text)
            .child(format!("[Image #1 · {name} {size}]")),
    )
}

fn slash_menu(t: &Theme, m: &SlashMenu) -> Div {
    let rows = m.entries.iter().enumerate().map(|(i, (cmd, desc))| {
        let on = i == m.selected;
        ui(t)
            .flex()
            .gap(px(metrics::ROW_GAP))
            .px(px(metrics::ROW_PX))
            .py(px(metrics::ROW_PY))
            .items_center()
            .rounded(px(metrics::RADIUS))
            .when(on, |d| d.bg(t.selected))
            .child(div().flex_none().w(px(CMD_W)).text_color(t.text).child(*cmd))
            .child(div().flex_1().min_w_0().truncate().text_color(t.faint).child(*desc))
    });
    div().absolute().bottom_full().left(px(metrics::PAD)).pb(px(metrics::GAP_SM)).child(
        div()
            .flex()
            .flex_col()
            .w(px(MENU_W))
            .p(px(metrics::ROW_PX))
            .rounded(px(metrics::RADIUS_LG))
            .bg(t.raised)
            .border_1()
            .border_color(t.line)
            .shadow(t.shadow(Lift::Float))
            .child(
                div()
                    .h(px(metrics::ROW_H))
                    .flex()
                    .items_center()
                    .px(px(metrics::ROW_PX))
                    .text_size(px(t.ty.small))
                    .text_color(t.faint)
                    .child("/"),
            )
            .children(rows),
    )
}

fn chip(t: &Theme, text: String, fg: gpui::Hsla) -> Div {
    ui(t).flex_none().whitespace_nowrap().text_size(px(t.ty.small)).text_color(fg).child(text)
}

/// The TUI's " · " between status segments.
fn sep(t: &Theme) -> Div {
    ui(t).flex_none().text_color(t.line).child("·")
}

/// The `[status]` defaults (`MuStatus`). A level is reached at its percent or its token count, whichever is
/// lower, as the TUI's `context_color` does.
const WARN_PERCENT: f64 = 50.0;
const WARN_TOKENS: u64 = 270_000;
const ERR_PERCENT: f64 = 70.0;
const ERR_TOKENS: u64 = 500_000;

/// Context share in warn or err once it reaches that level, else accent (the TUI's muted level shows as accent).
fn ctx_colour(t: &Theme, c: &ContextWindow) -> gpui::Hsla {
    let share = c.share();
    let reaches = |percent: f64, tokens: u64| share >= percent.min(tokens as f64 / c.size as f64 * 100.0);
    if reaches(ERR_PERCENT, ERR_TOKENS) {
        t.err
    } else if reaches(WARN_PERCENT, WARN_TOKENS) {
        t.warn
    } else {
        t.accent
    }
}

/// The branch in the ok tone when clean, warn when dirty, then the counts the TUI's git segment prints after it:
/// `*unstaged` in warn, `+staged` in ok, `?untracked` in accent.
fn branch(t: &Theme, s: &Session) -> Div {
    let g = s.git;
    let mut d = ui(t)
        .flex()
        .items_center()
        .gap(px(metrics::GAP_SM))
        .min_w_0()
        .overflow_hidden()
        .text_size(px(t.ty.small))
        .child(div().min_w_0().truncate().text_color(if g.is_clean() { t.ok } else { t.warn }).child(s.branch));
    for (n, mark, fg) in [(g.dirty, "*", t.warn), (g.staged, "+", t.ok), (g.untracked, "?", t.accent)] {
        if n > 0 {
            d = d.child(div().flex_none().text_color(fg).child(format!("{mark}{n}")));
        }
    }
    d
}

/// Context share as the TUI's context segment: the share in its level's colour, then the window's size.
fn context(t: &Theme, s: &Session) -> Div {
    ui(t)
        .flex()
        .flex_none()
        .whitespace_nowrap()
        .text_size(px(t.ty.small))
        .child(div().text_color(ctx_colour(t, &s.context)).child(format!("{:.1}%", s.context.share())))
        .child(div().text_color(t.faint).child(format!(" of {}", s.context.size_label)))
}

/// The status line's segments in the TUI's order: turn glyph and timer, model and thinking level, mode,
/// branch, then the context share, cost and the stop control at the right.
pub fn session_bar(c: &ViewCtx) -> AnyElement {
    let (t, s): (&Theme, &Session) = (c.t, c.s());
    let pi = match s.turn_timer {
        Some(timer) => div()
            .flex_none()
            .flex()
            .gap(px(metrics::GAP_SM))
            .child(chip(t, "⠋".into(), t.accent))
            .child(chip(t, timer.to_string(), t.muted)),
        None => chip(t, "μ".into(), t.muted),
    };
    let model = ui(t)
        .flex_none()
        .whitespace_nowrap()
        .flex()
        .text_size(px(t.ty.small))
        .child(div().text_color(t.tool).child(s.model))
        .child(div().text_color(t.muted).child(format!(" ({})", s.thinking)));
    let bar = ui(t)
        .flex()
        .items_center()
        .gap(px(metrics::GAP_SM))
        .h(px(metrics::STATUS_BAR_H))
        .px(px(metrics::PAD_X))
        .overflow_hidden()
        .child(pi)
        .child(sep(t))
        .child(model)
        .child(sep(t))
        .child(chip(t, s.mode.to_string(), t.accent))
        .child(sep(t))
        .child(branch(t, s))
        .child(div().flex_1())
        .child(context(t, s))
        .child(sep(t))
        .child(chip(t, s.cost.to_string(), t.syn_type));
    let bar = match s.turn_timer {
        Some(_) => bar.child(
            ui(t)
                .flex()
                .items_center()
                .flex_none()
                .min_h(px(metrics::CONTROL_H_SM))
                .px(px(metrics::PAD_X))
                .rounded(px(metrics::RADIUS_SM))
                .border_1()
                .border_color(t.err_soft)
                .text_color(t.err)
                .child("■"),
        ),
        None => bar,
    };
    bar.into_any_element()
}
