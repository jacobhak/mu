//! The agent list: narrow (in a bar) and wide (as a table in a tile). `c.wide` selects the table form.
use super::{ViewCtx, ui};
use crate::data::{AgentRow, AgentState, RecentRow};
use crate::theme::{Theme, metrics};
use gpui::prelude::*;
use gpui::{AnyElement, Div, FontWeight, Hsla, div, px};

const ICON_W: f32 = 16.;
const SUB_INDENT: f32 = 14.;
const COL_GAP: f32 = 8.;
const COL_MARK: f32 = 16.;
const COL_STATE: f32 = 44.;
const COL_KIND: f32 = 76.;
const COL_HOST: f32 = 110.;
const COL_BRANCH: f32 = 110.;
const COL_ATTACHED: f32 = 80.;
const COL_RAN: f32 = 48.;
const COL_COST: f32 = 56.;
const ROW_PY: f32 = 3.;
const HEAD_PY: f32 = 5.;

pub fn view(c: &ViewCtx) -> AnyElement {
    if c.wide { wide(c) } else { narrow(c) }
}

/// The TUI's marks: a live session is `●` and a finished job `✓` (both ok), a failed one `✗` (err),
/// an idle one `○` (faint, as a not-started item). A session waiting for the user keeps the live
/// mark; the word `waiting` on its kind says so, as the TUI does.
fn state_mark(t: &Theme, s: AgentState) -> (&'static str, Hsla) {
    let icon = match s {
        AgentState::NeedsYou => "●",
        _ => s.icon(),
    };
    let color = match s {
        AgentState::NeedsYou | AgentState::Running | AgentState::Finished => t.ok,
        AgentState::Idle => t.faint,
        AgentState::Failed => t.err,
    };
    (icon, color)
}

fn small(t: &Theme) -> Div {
    ui(t).text_size(px(t.ty.small))
}

fn count(rows: &[AgentRow]) -> usize {
    rows.iter().map(|r| 1 + count(&r.subs)).sum()
}

fn find_field(t: &Theme) -> Div {
    ui(t)
        .flex()
        .items_center()
        .px(px(metrics::PAD_SM + 2.))
        .py(px(4.))
        .rounded(px(metrics::RADIUS))
        .bg(t.surface)
        .border_1()
        .border_color(t.line_faint)
        .text_color(t.muted)
        .child("› Find…")
}

fn shell_col(c: &ViewCtx, id: &'static str) -> gpui::Stateful<Div> {
    div_col().id((id, c.session)).flex_1().min_h_0().overflow_y_scroll()
}

fn div_col() -> Div {
    div().flex().flex_col()
}

// ---------------------------------------------------------------- narrow

fn narrow(c: &ViewCtx) -> AnyElement {
    let t = c.t;
    let mut body = shell_col(c, "agents-scroll").px(px(metrics::PAD_SM)).pb(px(metrics::PAD));
    if !c.data.needs_you.is_empty() {
        body = body.child(group_label(t, "NEEDS YOU", t.muted));
        for r in &c.data.needs_you {
            body = body.child(narrow_row(t, r, true));
        }
    }
    for g in &c.data.groups {
        body = body.child(group_label(t, g.name, t.muted));
        for r in &g.rows {
            body = body.child(narrow_row(t, r, false));
            for s in &r.subs {
                body = body.child(narrow_sub(t, s));
            }
        }
        if !g.recent.is_empty() {
            body = body.child(small(t).pl(px(ICON_W)).pt(px(6.)).text_color(t.faint).child("RECENT"));
            for r in &g.recent {
                body = body.child(narrow_recent(t, r));
            }
        }
    }
    div_col()
        .size_full()
        .bg(t.bar)
        .child(div_col().p(px(metrics::PAD_SM)).child(find_field(t)))
        .child(body)
        .into_any_element()
}

fn group_label(t: &Theme, text: &'static str, color: Hsla) -> Div {
    small(t)
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(color)
        .px(px(4.))
        .mt(px(12.))
        .pb(px(4.))
        .mb(px(4.))
        .border_b_1()
        .border_color(t.line_faint)
        .truncate()
        .child(text)
}

fn narrow_row(t: &Theme, r: &AgentRow, pinned: bool) -> Div {
    let (icon, color) = state_mark(t, r.state);
    let meta = if pinned {
        format!("{} waiting · {} · {}", r.kind, r.host, r.ran)
    } else {
        format!("{} · {} · {}", r.kind, r.host, r.branch)
    };
    let title = ui(t)
        .flex_1()
        .min_w_0()
        .truncate()
        .text_color(t.strong)
        .when(r.current, |d| d.font_weight(FontWeight::BOLD))
        .child(r.title);
    let head = ui(t)
        .flex()
        .items_center()
        .child(ui(t).w(px(ICON_W)).flex_none().text_color(color).child(icon))
        .child(title)
        .when(r.current, |d| d.child(ui(t).flex_none().pl(px(4.)).text_color(t.accent).child("❯")));
    let meta_line = small(t)
        .flex()
        .pl(px(ICON_W))
        .text_color(t.muted)
        .child(div_col().flex_1().min_w_0().truncate().child(meta));
    let cost_line = (!pinned).then(|| {
        small(t)
            .flex()
            .justify_between()
            .pl(px(ICON_W))
            .text_color(t.muted)
            .child(r.ran)
            .child(r.cost)
    });
    row_box(t, r.current)
        .child(head)
        .child(meta_line)
        .when(pinned, |d| d.child(small(t).flex().justify_end().pl(px(ICON_W)).text_color(t.muted).child(r.cost)))
        .children(cost_line)
}

fn row_box(t: &Theme, current: bool) -> Div {
    let d = div_col()
        .px(px(4.))
        .py(px(ROW_PY + 1.))
        .rounded(px(metrics::RADIUS_SM))
        .border_l_2()
        .border_color(gpui::transparent_black());
    if current { d.bg(t.selected) } else { d }
}

fn narrow_sub(t: &Theme, s: &AgentRow) -> Div {
    let (icon, color) = state_mark(t, s.state);
    small(t)
        .flex()
        .items_center()
        .px(px(4.))
        .pl(px(4. + SUB_INDENT))
        .py(px(2.))
        .text_color(t.muted)
        .child(small(t).w(px(ICON_W - 2.)).flex_none().text_color(color).child(icon))
        .child(small(t).flex_1().min_w_0().truncate().text_color(t.strong).child(s.title))
        .child(small(t).flex_none().pl(px(6.)).child(s.kind))
}

fn narrow_recent(t: &Theme, r: &RecentRow) -> Div {
    small(t)
        .flex()
        .px(px(4.))
        .pl(px(4. + ICON_W))
        .py(px(2.))
        .text_color(t.muted)
        .child(div_col().flex_1().min_w_0().truncate().child(r.title))
        .child(small(t).flex_none().pl(px(6.)).child(r.age))
        .child(small(t).flex_none().pl(px(8.)).w(px(44.)).text_right().child(r.cost))
}

// ---------------------------------------------------------------- wide

fn wide(c: &ViewCtx) -> AnyElement {
    let t = c.t;
    let total = count(&c.data.needs_you) + c.data.groups.iter().map(|g| count(&g.rows)).sum::<usize>();
    let top = ui(t)
        .flex()
        .items_center()
        .gap(px(metrics::PAD))
        .p(px(metrics::PAD_SM))
        .border_b_1()
        .border_color(t.line)
        .child(find_field(t).flex_1().min_w_0())
        .child(small(t).flex_none().text_color(t.muted).child("group: project ▾"))
        .child(small(t).flex_none().text_color(t.muted).child(format!("sessions · {total} live")));

    let mut body = shell_col(c, "agents-table-scroll").px(px(metrics::PAD_SM)).pb(px(metrics::PAD_SM));
    body = body.child(header_row(t));
    if !c.data.needs_you.is_empty() {
        body = body.child(table_label(t, "NEEDS YOU", t.muted));
        for r in &c.data.needs_you {
            body = body.child(table_row(t, r, false));
        }
    }
    for g in &c.data.groups {
        body = body.child(table_label(t, g.name, t.muted));
        for r in &g.rows {
            body = body.child(table_row(t, r, false));
            for s in &r.subs {
                body = body.child(table_row(t, s, true));
            }
        }
        if !g.recent.is_empty() {
            body = body.child(table_label(t, "RECENT", t.faint));
            for r in &g.recent {
                body = body.child(recent_row(t, r));
            }
        }
    }

    let footer = small(t)
        .px(px(metrics::PAD))
        .py(px(HEAD_PY + 1.))
        .border_t_1()
        .border_color(t.line)
        .text_color(t.muted)
        .child("Enter peek · n message · s stop · S terminate · t take over · / find");

    div_col().size_full().bg(t.window).child(top).child(body).child(footer).into_any_element()
}

fn cell(d: Div, w: Option<f32>) -> Div {
    match w {
        Some(w) => d.w(px(w)).flex_none().truncate(),
        None => d.flex_1().min_w_0().truncate(),
    }
}

fn header_row(t: &Theme) -> Div {
    let h = |text: &'static str, w: Option<f32>, right: bool| {
        let d = cell(small(t).text_color(t.faint), w);
        (if right { d.text_right() } else { d }).child(text)
    };
    small(t)
        .flex()
        .gap(px(COL_GAP))
        .px(px(4.))
        .py(px(HEAD_PY))
        .border_b_1()
        .border_color(t.line_faint)
        .child(cell(small(t), Some(COL_MARK)))
        .child(h("STATE", Some(COL_STATE), false))
        .child(h("TITLE / FIRST PROMPT", None, false))
        .child(h("KIND", Some(COL_KIND), false))
        .child(h("HOST", Some(COL_HOST), false))
        .child(h("BRANCH", Some(COL_BRANCH), false))
        .child(h("ATTACHED", Some(COL_ATTACHED), false))
        .child(h("RAN", Some(COL_RAN), true))
        .child(h("COST", Some(COL_COST), true))
}

fn table_label(t: &Theme, text: &'static str, color: Hsla) -> Div {
    small(t).font_weight(FontWeight::SEMIBOLD).text_color(color).px(px(4.)).pt(px(10.)).pb(px(3.)).truncate().child(text)
}

fn table_row(t: &Theme, r: &AgentRow, sub: bool) -> Div {
    let (icon, color) = state_mark(t, r.state);
    let selected = r.current;
    let kind = if r.state == AgentState::NeedsYou { format!("{} waiting", r.kind) } else { r.kind.to_string() };
    let muted = |d: Div| d.text_color(t.muted);
    let mut d = ui(t)
        .flex()
        .items_center()
        .gap(px(COL_GAP))
        .px(px(4.))
        .py(px(ROW_PY))
        .rounded(px(metrics::RADIUS_SM))
        .border_l_2()
        .border_color(gpui::transparent_black());
    if selected {
        d = d.bg(t.selected);
    }
    d.child(cell(ui(t).text_color(t.accent), Some(COL_MARK)).child(if selected { "❯" } else { "" }))
        .child(cell(ui(t).text_color(color), Some(COL_STATE)).pl(px(if sub { SUB_INDENT } else { 0. })).child(icon))
        .child(
            cell(ui(t).text_color(t.strong), None)
                .when(selected, |d| d.font_weight(FontWeight::BOLD))
                .child(r.title),
        )
        .child(cell(muted(small(t)), Some(COL_KIND)).child(kind))
        .child(cell(muted(small(t)), Some(COL_HOST)).child(r.host))
        .child(cell(muted(small(t)), Some(COL_BRANCH)).child(r.branch))
        .child(cell(muted(small(t)), Some(COL_ATTACHED)).child(r.attached))
        .child(cell(muted(small(t)), Some(COL_RAN)).text_right().child(r.ran))
        .child(cell(muted(small(t)), Some(COL_COST)).text_right().child(r.cost))
}

fn recent_row(t: &Theme, r: &RecentRow) -> Div {
    let span = COL_KIND + COL_HOST + COL_GAP;
    let skip = COL_BRANCH + COL_ATTACHED + COL_GAP;
    small(t)
        .flex()
        .items_center()
        .gap(px(COL_GAP))
        .px(px(4.))
        .py(px(ROW_PY))
        .text_color(t.muted)
        .child(cell(small(t), Some(COL_MARK)))
        .child(cell(small(t), Some(COL_STATE)))
        .child(cell(ui(t).text_color(t.muted), None).child(r.title))
        .child(cell(small(t).text_color(t.muted), Some(span)).child(format!("ended {} ago", r.age)))
        .child(cell(small(t), Some(skip)))
        .child(cell(small(t), Some(COL_RAN)))
        .child(cell(small(t), Some(COL_COST)).text_right().child(r.cost))
}
