//! The jobs view: running and finished jobs, with the selected one's details.
use super::{ViewCtx, mono, ui};
use crate::data::{JobRow, JobState};
use crate::theme::{Theme, metrics};
use gpui::prelude::*;
use gpui::{AnyElement, Div, FontWeight, Hsla, div, px};

const COL_MARK: f32 = 12.;
const COL_STATE: f32 = 16.;
const COL_AGENT: f32 = 56.;
const COL_TOOLS: f32 = 44.;
const COL_ELAPSED: f32 = 56.;
const COL_COST: f32 = 56.;
const COL_PORTS: f32 = 96.;

fn small(t: &Theme) -> Div {
    ui(t).text_size(px(t.ty.small))
}

/// The TUI's job marks: running `◌` (accent), done `✓` (ok), failed `✗` (err). A cancelled job has
/// no mark in the TUI, which says `cancelled` in faint; `⊘` stays faint for it.
fn glyph(t: &Theme, s: JobState) -> (&'static str, Hsla) {
    match s {
        JobState::Running => ("◌", t.accent),
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
    let finished = d.jobs.len() - running;

    let header = ui(t)
        .flex()
        .items_center()
        .gap(px(metrics::GAP))
        .h(px(metrics::ROW_H))
        .px(px(metrics::PAD_X))
        .bg(t.surface)
        .border_b_1()
        .border_color(t.line)
        .child(ui(t).text_color(t.accent).child("Jobs"))
        .child(small(t).text_color(t.muted).child(format!("{running} running · {finished} finished")));

    let mut table = div().flex().flex_col().flex_none().px(px(metrics::PAD_SM)).py(px(metrics::PAD_SM)).child(head_row(t));
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
        body = body.child(details(t, j).flex_none());
    }

    let footer = small(t)
        .px(px(metrics::PAD_X))
        .py(px(metrics::PAD_Y))
        .bg(t.surface)
        .border_t_1()
        .border_color(t.line)
        .text_color(t.muted)
        .child("Enter transcript · x kill · X kill all · d diff · a apply · o ports");

    div().flex().flex_col().size_full().bg(t.window).child(header).child(body).child(footer).into_any_element()
}

fn head_row(t: &Theme) -> Div {
    let h = |text: &'static str, w: Option<f32>, right: bool| {
        let d = cell(small(t).text_color(t.faint), w);
        (if right { d.text_right() } else { d }).child(text)
    };
    small(t)
        .flex()
        .items_center()
        .h(px(metrics::ROW_H))
        .gap(px(metrics::GAP))
        .px(px(metrics::ROW_PX))
        .child(cell(small(t), Some(COL_MARK)))
        .child(h("", Some(COL_STATE), false))
        .child(h("NAME", None, false))
        .child(h("AGENT", Some(COL_AGENT), false))
        .child(h("TOOLS", Some(COL_TOOLS), true))
        .child(h("ELAPSED", Some(COL_ELAPSED), true))
        .child(h("COST", Some(COL_COST), true))
        .child(h("", Some(COL_PORTS), false))
        .child(h("LAST ACTIVITY", None, false))
}

fn job_row(t: &Theme, j: &JobRow, selected: bool) -> Div {
    let (icon, color) = glyph(t, j.state);
    let muted = |d: Div| d.text_color(t.muted);
    // Where the job listens, as the TUI's jobs panel prints it after the cost.
    let ports = j.ports.iter().map(|p| format!(":{p}")).collect::<Vec<_>>().join(" ");
    let d = ui(t)
        .flex()
        .items_center()
        .gap(px(metrics::GAP))
        .px(px(metrics::ROW_PX))
        .py(px(metrics::ROW_PY))
        .rounded(px(metrics::RADIUS));
    let d = if selected { d.bg(t.selected) } else { d };
    d.child(cell(ui(t).text_color(t.accent), Some(COL_MARK)).child(if selected { "❯" } else { "" }))
        .child(cell(ui(t).text_color(color), Some(COL_STATE)).child(icon))
        .child(
            cell(ui(t).text_color(t.strong), None)
                .when(selected, |d| d.font_weight(FontWeight::BOLD))
                .child(j.name),
        )
        .child(cell(muted(small(t)), Some(COL_AGENT)).child(j.agent))
        .child(cell(muted(small(t)), Some(COL_TOOLS)).text_right().child(j.tools.to_string()))
        .child(cell(muted(small(t)), Some(COL_ELAPSED)).text_right().child(j.elapsed))
        .child(cell(muted(small(t)), Some(COL_COST)).text_right().child(j.cost))
        .child(cell(muted(small(t)), Some(COL_PORTS)).child(ports))
        .child(cell(muted(small(t)), None).child(j.last))
}

fn details(t: &Theme, j: &JobRow) -> Div {
    let (icon, color) = glyph(t, j.state);
    // The output box takes the TUI's card tone: pending while running, error when failed, else success.
    let tone = match j.state {
        JobState::Running => t.accent_soft,
        JobState::Failed => t.err_soft,
        JobState::Done | JobState::Cancelled => t.line,
    };
    let mut out = mono(t)
        .flex()
        .flex_col()
        .px(px(metrics::PAD_X))
        .py(px(metrics::PAD_Y))
        .rounded(px(metrics::RADIUS))
        .bg(t.surface)
        .border_1()
        .border_color(tone)
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
        .gap(px(metrics::GAP))
        .mt(px(metrics::GAP))
        .mb(px(metrics::GAP_LG))
        .px(px(metrics::PAD_X))
        .py(px(metrics::PAD_Y))
        .border_t_1()
        .border_color(t.line)
        .child(
            ui(t)
                .flex()
                .gap(px(metrics::GAP_SM))
                .child(ui(t).text_color(color).child(icon))
                .child(ui(t).text_color(t.accent).child(j.name)),
        )
        .child(
            ui(t)
                .flex()
                .child(ui(t).text_color(t.faint).child("task "))
                .child(ui(t).text_color(t.strong).child(j.task)),
        )
        .child(small(t).text_color(t.tool).child("Output"))
        .child(out)
}
