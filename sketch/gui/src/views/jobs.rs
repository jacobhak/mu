//! The jobs view: running and finished jobs, with the selected one's details.
use super::{ViewCtx, mono, ui};
use crate::data::{JobRow, JobState};
use crate::theme::{Theme, metrics};
use gpui::prelude::*;
use gpui::{AnyElement, Div, FontWeight, Hsla, div, px};

const COL_GAP: f32 = 10.;
const COL_STATE: f32 = 18.;
const COL_AGENT: f32 = 56.;
const COL_TOOLS: f32 = 44.;
const COL_ELAPSED: f32 = 56.;
const COL_COST: f32 = 52.;
const ROW_PY: f32 = 3.;

fn small(t: &Theme) -> Div {
    ui(t).text_size(px(t.ty.small))
}

fn glyph(t: &Theme, s: JobState) -> (&'static str, Hsla) {
    match s {
        JobState::Running => ("●", t.accent),
        JobState::Done => ("✓", t.ok),
        JobState::Failed => ("✗", t.err),
        JobState::Cancelled => ("⊘", t.faint),
    }
}

fn cell(d: Div, w: Option<f32>) -> Div {
    match w {
        Some(w) => d.w(px(w)).flex_none().truncate(),
        None => d.flex_1().min_w_0().truncate(),
    }
}

pub fn view(c: &ViewCtx) -> AnyElement {
    let t = c.t;
    let d = c.data;
    let running = d.jobs.iter().filter(|j| j.state == JobState::Running).count();

    let header = ui(t)
        .flex()
        .items_center()
        .gap(px(metrics::PAD))
        .px(px(metrics::PAD))
        .py(px(metrics::PAD_SM + 2.))
        .border_b_1()
        .border_color(t.line)
        .child(ui(t).font_weight(FontWeight::SEMIBOLD).text_color(t.strong).child("Jobs"))
        .child(small(t).text_color(t.muted).child(format!("{running} running · {} total", d.jobs.len())));

    let mut table = div().flex().flex_col().px(px(metrics::PAD_SM)).py(px(metrics::PAD_SM)).child(head_row(t));
    for (i, j) in d.jobs.iter().enumerate() {
        table = table.child(job_row(t, j, i == d.job_selected));
    }

    let mut body = div()
        .id(("jobs-scroll", c.session))
        .flex()
        .flex_col()
        .flex_1()
        .min_h_0()
        .overflow_y_scroll()
        .child(table);
    if let Some(j) = d.jobs.get(d.job_selected) {
        body = body.child(details(t, j));
    }

    let footer = small(t)
        .px(px(metrics::PAD))
        .py(px(7.))
        .border_t_1()
        .border_color(t.line)
        .text_color(t.muted)
        .child("Enter open · x cancel · X cancel all · d result · a apply · o ports");

    div().flex().flex_col().size_full().bg(t.window).child(header).child(body).child(footer).into_any_element()
}

fn head_row(t: &Theme) -> Div {
    let h = |text: &'static str, w: Option<f32>, right: bool| {
        let d = cell(small(t).font_weight(FontWeight::SEMIBOLD).text_color(t.faint), w);
        (if right { d.text_right() } else { d }).child(text)
    };
    small(t)
        .flex()
        .gap(px(COL_GAP))
        .px(px(6.))
        .py(px(5.))
        .border_b_1()
        .border_color(t.line_faint)
        .child(h("", Some(COL_STATE), false))
        .child(h("NAME", None, false))
        .child(h("AGENT", Some(COL_AGENT), false))
        .child(h("TOOLS", Some(COL_TOOLS), true))
        .child(h("ELAPSED", Some(COL_ELAPSED), true))
        .child(h("COST", Some(COL_COST), true))
        .child(h("LAST ACTIVITY", None, false))
}

fn job_row(t: &Theme, j: &JobRow, selected: bool) -> Div {
    let (icon, color) = glyph(t, j.state);
    let muted = |d: Div| d.text_color(t.muted);
    let d = ui(t)
        .flex()
        .items_center()
        .gap(px(COL_GAP))
        .px(px(6.))
        .py(px(ROW_PY))
        .rounded(px(4.))
        .border_l_2()
        .border_color(gpui::transparent_black());
    let d = if selected { d.bg(t.selected).border_color(t.accent) } else { d };
    d.child(cell(ui(t).text_color(color), Some(COL_STATE)).child(icon))
        .child(cell(ui(t).text_color(if selected { t.strong } else { t.text }), None).child(j.name))
        .child(cell(muted(small(t)), Some(COL_AGENT)).child(j.agent))
        .child(cell(muted(small(t)), Some(COL_TOOLS)).text_right().child(j.tools.to_string()))
        .child(cell(muted(small(t)), Some(COL_ELAPSED)).text_right().child(j.elapsed))
        .child(cell(muted(small(t)), Some(COL_COST)).text_right().child(j.cost))
        .child(cell(muted(small(t)), None).child(j.last))
}

fn details(t: &Theme, j: &JobRow) -> Div {
    let (icon, color) = glyph(t, j.state);
    let mut out = mono(t)
        .flex()
        .flex_col()
        .px(px(metrics::PAD_SM + 2.))
        .py(px(metrics::PAD_SM))
        .rounded(px(metrics::RADIUS))
        .bg(t.surface)
        .text_size(px(t.ty.small + 0.5))
        .text_color(t.muted);
    if j.output.is_empty() {
        out = out.text_color(t.faint).child("(no output yet)");
    }
    for l in &j.output {
        out = out.child(div().min_w_0().child(*l));
    }
    div()
        .flex()
        .flex_col()
        .gap(px(metrics::PAD_SM))
        .mx(px(metrics::PAD_SM))
        .mt(px(metrics::PAD_SM))
        .mb(px(metrics::PAD))
        .p(px(metrics::PAD_SM))
        .border_t_1()
        .border_color(t.line)
        .child(
            ui(t)
                .flex()
                .gap(px(6.))
                .child(ui(t).text_color(color).child(icon))
                .child(ui(t).font_weight(FontWeight::SEMIBOLD).text_color(t.strong).child(j.name)),
        )
        .child(ui(t).text_color(t.text).child(j.task))
        .child(small(t).text_color(t.faint).child("OUTPUT"))
        .child(out)
}
