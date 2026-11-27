//! The agent palette, the full palette and the floating panel's frame.
//!
//! The shell draws the scrim and centres what these return; they size themselves (a palette about 640 px wide).
use super::ui;
use crate::data::{Data, EntryKind, PaletteEntry, SessionId};
use crate::theme::{Lift, Theme, metrics};
use gpui::prelude::*;
use gpui::{AnyElement, FontWeight, Hsla, div, px};

const PALETTE_W: f32 = 640.0;
const FLOAT_W: f32 = 820.0;
const FLOAT_H: f32 = 620.0;
const QUERY_H: f32 = 40.0;
const MARKER_W: f32 = 16.0;
const KIND_W: f32 = 72.0;
const GLYPH_W: f32 = 16.0;

/// Agents and sessions, most recent first. `selected` indexes `data.palette.agents`.
pub fn agent(t: &Theme, data: &Data, selected: usize) -> AnyElement {
    let rows = data.palette.agents.iter().enumerate().map(|(i, e)| agent_row(t, e, i == selected));
    shell(t)
        .child(query_row(t, ""))
        .child(rule(t))
        .child(div().flex().flex_col().py(px(metrics::PAD_Y)).children(rows))
        .child(rule(t))
        .child(footer(t))
        .into_any_element()
}

/// Everything: actions with their keys, slash commands, agents, layouts, views. `selected` indexes
/// `data.palette.entries`.
pub fn full(t: &Theme, data: &Data, selected: usize) -> AnyElement {
    let q = data.palette.query;
    let rows = data.palette.entries.iter().enumerate().map(|(i, e)| full_row(t, e, i == selected));
    shell(t)
        .child(query_row(t, q))
        .child(rule(t))
        .child(div().flex().flex_col().py(px(metrics::PAD_Y)).children(rows))
        .child(rule(t))
        .child(footer(t))
        .into_any_element()
}

/// The floating panel for a jump to the next request: a header (title, host, "needs you"), then `body`, the
/// session's own tile content (transcript with its request docked), which the shell builds.
pub fn floating(t: &Theme, data: &Data, session: SessionId, body: AnyElement) -> AnyElement {
    let s = data.session(session);
    let header = ui(t)
        .h(px(metrics::ROW_H))
        .flex_none()
        .flex()
        .items_center()
        .gap(px(metrics::GAP))
        .px(px(metrics::PAD_X))
        .py(px(metrics::PAD_Y))
        .bg(t.surface)
        .border_b_1()
        .border_color(t.line)
        .child(div().font_weight(FontWeight::BOLD).text_color(t.accent).truncate().child(s.title))
        .child(div().flex_none().text_color(t.muted).child(s.host))
        .child(
            div()
                .flex_none()
                .px(px(metrics::GAP))
                .rounded(px(999.))
                .border_1()
                .border_color(t.line)
                .bg(t.window)
                .text_size(px(t.ty.small))
                .text_color(t.warn)
                .child("⚑ needs you"),
        )
        .child(div().flex_1())
        .child(hint(t, "Esc", "close"))
        .child(sep(t))
        .child(hint(t, "o", "open in tile"));
    ui(t)
        .w(px(FLOAT_W))
        .h(px(FLOAT_H))
        .bg(t.window)
        .rounded(px(metrics::RADIUS_LG))
        .border_1()
        .border_color(t.focus_ring)
        .shadow(t.shadow(Lift::Float))
        .overflow_hidden()
        .flex()
        .flex_col()
        .child(header)
        .child(div().flex_1().min_h_0().overflow_hidden().child(body))
        .into_any_element()
}

// ---------------------------------------------------------------- shared

fn shell(t: &Theme) -> gpui::Div {
    ui(t)
        .w(px(PALETTE_W))
        .bg(t.window)
        .rounded(px(metrics::RADIUS_LG))
        .border_1()
        .border_color(t.line)
        .shadow(t.shadow(Lift::Float))
        .overflow_hidden()
        .flex()
        .flex_col()
}

fn rule(t: &Theme) -> gpui::Div {
    div().h(px(1.)).w_full().flex_none().bg(t.line)
}

fn query_row(t: &Theme, query: &str) -> gpui::Div {
    let typed = div()
        .flex()
        .items_center()
        .when(query.is_empty(), |d| d.child(cursor(t)).child(div().text_color(t.faint).child("Find agent…")))
        .when(!query.is_empty(), |d| d.child(div().text_color(t.text).child(query.to_string())).child(cursor(t)));
    // The input sits on the bar layer, apart from the list.
    ui(t)
        .h(px(QUERY_H))
        .flex_none()
        .flex()
        .items_center()
        .px(px(metrics::PAD_X))
        .gap(px(metrics::GAP_SM))
        .bg(t.surface)
        .child(div().text_color(t.text).child("›"))
        .child(typed)
}

fn cursor(t: &Theme) -> gpui::Div {
    div().w(px(2.)).h(px(t.ty.ui)).bg(t.accent)
}

fn footer(t: &Theme) -> gpui::Div {
    ui(t)
        .flex_none()
        .flex()
        .items_center()
        .justify_between()
        .px(px(metrics::PAD_X))
        .py(px(metrics::PAD_Y))
        .bg(t.surface)
        .text_size(px(t.ty.small))
        .text_color(t.muted)
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(metrics::GAP))
                .child("actions")
                .child(sep(t))
                .child("/commands")
                .child(sep(t))
                .child("agents")
                .child(sep(t))
                .child("layouts"),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(metrics::GAP))
                .child(div().flex().items_center().gap(px(metrics::GAP_SM)).child(cap(t, "Enter")).child("run"))
                .child(sep(t))
                .child(cap(t, "Esc")),
        )
}

/// A key as a small rounded cap.
fn cap(t: &Theme, key: &str) -> gpui::Div {
    ui(t)
        .flex_none()
        .px(px(metrics::CAP_PX))
        .py(px(metrics::CAP_PY))
        .rounded(px(metrics::RADIUS_SM))
        .border_1()
        .border_color(t.line)
        .bg(t.window)
        .text_size(px(t.ty.small))
        .text_color(t.text)
        .child(key.to_string())
}

/// `cap` followed by its meaning, for the floating header.
fn hint(t: &Theme, key: &str, what: &str) -> gpui::Div {
    div()
        .flex_none()
        .flex()
        .items_center()
        .gap(px(metrics::GAP_SM))
        .text_size(px(t.ty.small))
        .text_color(t.muted)
        .child(cap(t, key))
        .child(what.to_string())
}

/// A ` · ` separator: the line role, as the TUI's status line draws its own.
fn sep(t: &Theme) -> gpui::Div {
    div().text_size(px(t.ty.small)).text_color(t.line).child("·")
}

/// The row's frame: the `❯` marker column and the selected background. Its content starts at `PAD_X`, as the
/// query row's does.
fn row(t: &Theme, selected: bool) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .gap(px(metrics::ROW_GAP))
        .mx(px(metrics::GAP_SM))
        .px(px(metrics::ROW_PX))
        .py(px(metrics::ROW_PY))
        .rounded(px(metrics::RADIUS))
        .border_1()
        .border_color(if selected { t.selected } else { gpui::transparent_black() })
        .when(selected, |d| d.bg(t.selected))
        .child(
            div()
                .w(px(MARKER_W))
                .flex_none()
                .text_color(t.accent)
                .child(if selected { "❯" } else { "" }),
        )
}

/// The agent-state glyph and its colour, read from the detail text.
fn state_glyph(t: &Theme, detail: &str) -> (&'static str, Hsla) {
    if detail.contains('⚑') {
        ("⚑", t.warn)
    } else if detail.contains('✓') {
        ("✓", t.ok)
    } else if detail.contains('✗') {
        ("✗", t.err)
    } else if detail.contains("running") {
        ("●", t.ok)
    } else if detail.contains("idle") {
        ("○", t.muted)
    } else {
        ("·", t.faint)
    }
}

fn agent_row(t: &Theme, e: &PaletteEntry, selected: bool) -> AnyElement {
    let (glyph, colour) = state_glyph(t, e.detail);
    row(t, selected)
        .child(div().w(px(GLYPH_W)).flex_none().text_color(colour).child(glyph))
        .child(
            div()
                .flex_none()
                .text_color(t.strong)
                .when(selected, |d| d.font_weight(FontWeight::BOLD))
                .child(e.label),
        )
        .child(div().flex_1().min_w_0().truncate().text_color(t.muted).child(e.detail))
        .into_any_element()
}

fn kind_tag(kind: EntryKind) -> &'static str {
    match kind {
        EntryKind::Action => "action",
        EntryKind::Command => "command",
        EntryKind::Agent => "agent",
        EntryKind::Layout => "layout",
        EntryKind::View => "view",
    }
}

fn full_row(t: &Theme, e: &PaletteEntry, selected: bool) -> AnyElement {
    let keys = e.key.split(' ').filter(|k| !k.is_empty()).map(|k| cap(t, k));
    row(t, selected)
        .child(
            div().w(px(KIND_W)).flex_none().flex().child(
                div()
                    .min_h(px(metrics::CONTROL_H_SM))
                    .flex()
                    .items_center()
                    .px(px(metrics::PAD_X))
                    .rounded(px(metrics::RADIUS_SM))
                    .bg(t.surface)
                    .text_size(px(t.ty.small))
                    .text_color(t.muted)
                    .child(kind_tag(e.kind)),
            ),
        )
        .child(
            div()
                .flex_none()
                .text_color(t.text)
                .when(selected, |d| d.font_weight(FontWeight::BOLD))
                .child(e.label),
        )
        .child(div().flex_1().min_w_0().truncate().text_color(t.muted).child(e.detail))
        .child(div().flex_none().flex().items_center().gap(px(metrics::GAP_SM)).children(keys))
        .into_any_element()
}
