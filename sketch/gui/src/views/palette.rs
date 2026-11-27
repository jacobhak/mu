//! The agent palette, the full palette and the floating panel's frame.
//!
//! The shell draws the scrim and centres what these return; they size themselves (a palette about 640 px wide).
use super::ui;
use crate::data::{Data, EntryKind, PaletteEntry, SessionId};
use crate::theme::{Lift, Theme, metrics};
use gpui::prelude::*;
use gpui::{AnyElement, FontWeight, Hsla, StyledText, TextRun, div, font, px};

const PALETTE_W: f32 = 640.0;
const FLOAT_W: f32 = 820.0;
const FLOAT_H: f32 = 620.0;
const QUERY_H: f32 = 40.0;
const ROW_PAD_Y: f32 = 4.0;
const MARKER_W: f32 = 18.0;
const KIND_W: f32 = 64.0;
const GLYPH_W: f32 = 18.0;
const CAP_RADIUS: f32 = 6.0;
const HEADER_H: f32 = 38.0;
const BADGE_BG_OPACITY: f32 = 0.16;

/// Agents and sessions, most recent first. `selected` indexes `data.palette.agents`.
pub fn agent(t: &Theme, data: &Data, selected: usize) -> AnyElement {
    let rows = data.palette.agents.iter().enumerate().map(|(i, e)| agent_row(t, e, i == selected));
    shell(t)
        .child(query_row(t, ""))
        .child(rule(t))
        .child(div().flex().flex_col().py(px(metrics::PAD_SM)).children(rows))
        .child(rule(t))
        .child(footer(t))
        .into_any_element()
}

/// Everything: actions with their keys, slash commands, agents, layouts, views. `selected` indexes
/// `data.palette.entries`.
pub fn full(t: &Theme, data: &Data, selected: usize) -> AnyElement {
    let q = data.palette.query;
    let rows = data.palette.entries.iter().enumerate().map(|(i, e)| full_row(t, e, q, i == selected));
    shell(t)
        .child(query_row(t, q))
        .child(rule(t))
        .child(div().flex().flex_col().py(px(metrics::PAD_SM)).children(rows))
        .child(rule(t))
        .child(footer(t))
        .into_any_element()
}

/// The floating panel for a jump to the next request: a header (title, host, "needs you"), then `body`, the
/// session's own tile content (transcript with its request docked), which the shell builds.
pub fn floating(t: &Theme, data: &Data, session: SessionId, body: AnyElement) -> AnyElement {
    let s = data.session(session);
    let header = ui(t)
        .h(px(HEADER_H))
        .flex_none()
        .flex()
        .items_center()
        .gap(px(10.))
        .px(px(metrics::PAD))
        .bg(t.bar)
        .border_b_1()
        .border_color(t.line)
        .child(div().font_weight(FontWeight::SEMIBOLD).text_color(t.strong).truncate().child(s.title))
        .child(div().flex_none().text_color(t.faint).child(s.host))
        .child(
            div()
                .flex_none()
                .px(px(7.))
                .py(px(1.))
                .rounded(px(CAP_RADIUS))
                .bg(t.warn.opacity(BADGE_BG_OPACITY))
                .text_size(px(t.ty.small))
                .text_color(t.warn)
                .child("⚑ needs you"),
        )
        .child(div().flex_1())
        .child(hint(t, "Esc", "close"))
        .child(div().text_size(px(t.ty.small)).text_color(t.faint).child("·"))
        .child(hint(t, "o", "open in tile"));
    ui(t)
        .w(px(FLOAT_W))
        .h(px(FLOAT_H))
        .bg(t.window)
        .rounded(px(metrics::RADIUS_LG + 2.))
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
        .bg(t.raised)
        .rounded(px(metrics::RADIUS_LG + 2.))
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
        .gap(px(1.))
        .when(query.is_empty(), |d| d.child(cursor(t)).child(div().text_color(t.faint).child("Find agent…")))
        .when(!query.is_empty(), |d| d.child(div().text_color(t.strong).child(query.to_string())).child(cursor(t)));
    ui(t)
        .h(px(QUERY_H))
        .flex_none()
        .flex()
        .items_center()
        .px(px(14.))
        .gap(px(10.))
        .child(div().text_color(t.accent).font_weight(FontWeight::SEMIBOLD).child("›"))
        .child(typed)
}

fn cursor(t: &Theme) -> gpui::Div {
    div().w(px(2.)).h(px(t.ty.ui + 3.)).bg(t.accent)
}

fn footer(t: &Theme) -> gpui::Div {
    ui(t)
        .flex_none()
        .flex()
        .items_center()
        .justify_between()
        .px(px(14.))
        .py(px(7.))
        .text_size(px(t.ty.small))
        .text_color(t.faint)
        .child("actions · /commands · agents · layouts")
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(5.))
                .child(cap(t, "Enter"))
                .child("run ·")
                .child(cap(t, "Esc")),
        )
}

/// A key as a small rounded cap.
fn cap(t: &Theme, key: &str) -> gpui::Div {
    ui(t)
        .flex_none()
        .px(px(6.))
        .py(px(1.))
        .rounded(px(CAP_RADIUS))
        .border_1()
        .border_color(t.line)
        .bg(t.surface)
        .text_size(px(t.ty.small))
        .text_color(t.muted)
        .child(key.to_string())
}

/// `cap` followed by its meaning, for the floating header.
fn hint(t: &Theme, key: &str, what: &str) -> gpui::Div {
    div()
        .flex_none()
        .flex()
        .items_center()
        .gap(px(5.))
        .text_size(px(t.ty.small))
        .text_color(t.faint)
        .child(cap(t, key))
        .child(what.to_string())
}

/// The row's frame: the `▸` marker column and the selected background.
fn row(t: &Theme, selected: bool) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .gap(px(8.))
        .mx(px(6.))
        .px(px(8.))
        .py(px(ROW_PAD_Y))
        .rounded(px(CAP_RADIUS))
        .when(selected, |d| d.bg(t.selected))
        .child(
            div()
                .w(px(MARKER_W - 8.))
                .flex_none()
                .text_color(t.accent)
                .child(if selected { "▸" } else { "" }),
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
                .text_color(if selected { t.strong } else { t.text })
                .child(e.label),
        )
        .child(div().flex_1().min_w_0().truncate().text_color(t.faint).child(e.detail))
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

fn full_row(t: &Theme, e: &PaletteEntry, query: &str, selected: bool) -> AnyElement {
    let keys = e.key.split(' ').filter(|k| !k.is_empty()).map(|k| cap(t, k));
    row(t, selected)
        .child(
            div()
                .w(px(KIND_W))
                .flex_none()
                .text_size(px(t.ty.small))
                .text_color(t.faint)
                .child(kind_tag(e.kind)),
        )
        .child(div().flex_none().child(label(t, e.label, query, selected)))
        .child(div().flex_1().min_w_0().truncate().text_color(t.faint).child(e.detail))
        .child(div().flex_none().flex().items_center().gap(px(3.)).children(keys))
        .into_any_element()
}

/// The label with the characters matched by `query` (a case-insensitive subsequence, spaces ignored) in the
/// accent colour.
fn label(t: &Theme, text: &str, query: &str, selected: bool) -> StyledText {
    let base = if selected { t.strong } else { t.text };
    let mut want = query.chars().filter(|c| !c.is_whitespace()).flat_map(|c| c.to_lowercase());
    let mut next = want.next();
    let mut runs: Vec<TextRun> = Vec::new();
    for ch in text.chars() {
        let hit = next.is_some_and(|n| ch.to_lowercase().next() == Some(n));
        if hit {
            next = want.next();
        }
        let mut f = font(crate::theme::SANS);
        if hit {
            f.weight = FontWeight::SEMIBOLD;
        }
        let colour = if hit { t.accent } else { base };
        match runs.last_mut() {
            Some(r) if r.font == f && r.color == colour => r.len += ch.len_utf8(),
            _ => runs.push(TextRun {
                len: ch.len_utf8(),
                font: f,
                color: colour,
                background_color: None,
                underline: None,
                strikethrough: None,
            }),
        }
    }
    StyledText::new(text.to_string()).with_runs(runs)
}
