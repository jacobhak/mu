//! Every block type, at rest and streaming. The view is top-aligned: anchoring to the live end needs scroll
//! state the sketch does not keep.
use super::{ViewCtx, diff, markdown, mono, prose, ui};
use crate::data::{Bash, Block, Diff, Sub, SubState, TodoItem, Tool};
use crate::theme::{Theme, metrics};
use gpui::prelude::*;
use gpui::{AnyElement, FontWeight, div, px, relative};

/// Space between consecutive blocks: tight between one-line rows, open around prose, diffs and groups.
const ROW_GAP: f32 = 3.0;
const BLOCK_GAP: f32 = 14.0;
/// Column widths of the tool and subagent rows, in code-size ems.
const ICON_W: f32 = 1.7;
const VERB_EM: f32 = 4.4;
const SUB_NAME_EM: f32 = 11.0;
const SUB_KIND_EM: f32 = 5.0;
const SUB_TIME_EM: f32 = 7.0;
const SUB_COST_EM: f32 = 6.0;
const SPINNER: &str = "⠋";

/// The session's transcript as a scrollable column: blocks in order, "↓ new output" when `new_output`.
pub fn view(c: &ViewCtx) -> AnyElement {
    let (t, s) = (c.t, c.s());
    let mut col = ui(t).flex().flex_col().w_full().px(px(metrics::PAD)).py(px(metrics::PAD));
    let mut prev: Option<&Block> = None;
    for b in &s.blocks {
        let top = prev.map_or(0.0, |p| if row_like(p) && row_like(b) { ROW_GAP } else { BLOCK_GAP });
        col = col.child(div().flex_none().w_full().min_w_0().mt(px(top)).child(block(t, b)));
        prev = Some(b);
    }
    // Room under the last block so the pill never covers it.
    col = col.child(div().flex_none().h(px(metrics::PAD * 2.)));

    div()
        .size_full()
        .relative()
        .child(div().id(("transcript-scroll", c.session)).size_full().overflow_y_scroll().child(col))
        .when(s.new_output, |d| {
            d.child(
                div().absolute().bottom_3().left_0().right_0().flex().justify_center().child(
                    ui(t)
                        .px(px(12.))
                        .py(px(3.))
                        .rounded(px(999.))
                        .bg(t.accent)
                        .text_color(t.window)
                        .text_size(px(t.ty.small))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child("↓ new output"),
                ),
            )
        })
        .into_any_element()
}

fn row_like(b: &Block) -> bool {
    match b {
        // A failed call is a card under its header, not a row.
        Block::Tool(tool) => tool.error.is_empty(),
        _ => matches!(
            b,
            Block::Thinking { .. } | Block::Retry(_) | Block::Notice(_) | Block::Compaction(_) | Block::Answered(_)
        ),
    }
}

fn block(t: &Theme, b: &Block) -> AnyElement {
    match b {
        Block::User { text, image } => user(t, *text, *image).into_any_element(),
        Block::Woken { text } => line(t, "✦", t.accent, t.muted, *text).italic().into_any_element(),
        Block::Thinking { time, live } => thinking(t, *time, *live).into_any_element(),
        Block::Tool(tool) => tool_row(t, tool),
        Block::Prose(md) => div().child(markdown::render(t, md)).into_any_element(),
        Block::Diff(d) => diff_block(t, d).into_any_element(),
        Block::Bash(b) => bash(t, b).into_any_element(),
        Block::Subagents(subs) => boxed(t).child(subagents(t, subs)).into_any_element(),
        Block::Todo(items) => boxed(t).child(todo(t, items)).into_any_element(),
        Block::Retry(s) => note(t, format!("↻ {s}")).into_any_element(),
        Block::Notice(s) => note(t, *s).into_any_element(),
        Block::Error(s) => error(t, *s).into_any_element(),
        Block::Compaction(s) => note(t, format!("── {s}")).into_any_element(),
        Block::Answered(s) => ui(t).text_color(t.muted).truncate().child(*s).into_any_element(),
        Block::Plan(s) => plan(t, *s).into_any_element(),
    }
}

/// A muted one-line row with an icon in its own colour.
fn line(t: &Theme, icon: &'static str, icon_fg: gpui::Hsla, fg: gpui::Hsla, text: &'static str) -> gpui::Div {
    ui(t)
        .flex()
        .items_start()
        .text_color(fg)
        .child(div().flex_none().w(px(t.ty.code * ICON_W)).text_color(icon_fg).child(icon))
        .child(div().flex_1().min_w_0().child(text))
}

/// A dim row, as the TUI's notice lines: the whole line in the muted tone, its glyph included.
fn note(t: &Theme, text: impl IntoElement) -> gpui::Div {
    ui(t).text_color(t.muted).child(text)
}

fn user(t: &Theme, text: &'static str, image: Option<&'static str>) -> impl IntoElement {
    prose(t)
        .flex()
        .items_start()
        .gap(px(12.))
        .w_full()
        .px(px(14.))
        .py(px(10.))
        .bg(t.surface)
        // The TUI's user band has an accent bar down its edge and no outline.
        .border_l_2()
        .border_color(t.accent)
        .rounded(px(metrics::RADIUS_LG))
        .line_height(relative(t.ty.prose_lh))
        .child(div().flex_1().min_w_0().text_color(t.strong).child(text))
        .when_some(image, |d, label| {
            d.child(
                ui(t)
                    .flex_none()
                    .px(px(8.))
                    .py(px(1.))
                    .rounded(px(metrics::RADIUS_SM))
                    .border_1()
                    .border_color(t.accent_soft)
                    .text_size(px(t.ty.small))
                    .text_color(t.accent)
                    .child(format!("[{label}]")),
            )
        })
}

fn thinking(t: &Theme, time: &'static str, live: Option<&'static str>) -> impl IntoElement {
    match live {
        None => ui(t).italic().text_color(t.faint).child(format!("◇ Thought for {time}")).into_any_element(),
        Some(tail) => div()
            .flex()
            .flex_col()
            .child(
                ui(t)
                    .flex()
                    .text_color(t.accent)
                    .child(div().flex_none().w(px(t.ty.code * ICON_W)).child(SPINNER))
                    .child(div().italic().text_color(t.faint).child(format!("Thinking… {time}"))),
            )
            .child(
                ui(t)
                    .pl(px(t.ty.code * ICON_W))
                    .pt(px(2.))
                    .italic()
                    .text_color(t.thinking)
                    .line_height(relative(1.45))
                    .child(tail),
            )
            .into_any_element(),
    }
}

fn tool_row(t: &Theme, tool: &Tool) -> AnyElement {
    let facts = match (tool.facts.is_empty(), tool.streaming) {
        (_, true) if tool.facts.is_empty() => "…".to_string(),
        (_, true) => format!("{} …", tool.facts),
        _ => tool.facts.to_string(),
    };
    // As the TUI's tool header: a bullet (a spinner while the arguments stream), the verb in bold, the target
    // in the tool role, the facts faint. The verb and the facts are interface text; only the target is code.
    // A failed call's bullet is err, and its output sits under the header in a card on the err_soft edge.
    let failed = !tool.error.is_empty();
    let (icon, icon_fg) = if tool.streaming {
        (SPINNER, t.accent)
    } else if failed {
        ("●", t.err)
    } else {
        ("●", t.faint)
    };
    let header = ui(t)
        .flex()
        .items_center()
        .whitespace_nowrap()
        .text_color(t.muted)
        .child(div().flex_none().w(px(t.ty.code * ICON_W)).text_color(icon_fg).child(icon))
        .child(div().flex_none().w(px(t.ty.code * VERB_EM)).font_weight(FontWeight::BOLD).text_color(t.tool).child(tool.verb))
        .child(mono(t).min_w_0().truncate().text_color(t.tool).child(tool.target))
        .when(!facts.is_empty(), |d| d.child(div().flex_none().pl(px(t.ty.code)).text_color(t.faint).child(facts)));
    if !failed {
        return header.into_any_element();
    }
    // As the TUI's error rows: the first line in err, the rest muted.
    let mut card = ui(t)
        .w_full()
        .min_w_0()
        .mt(px(3.))
        .px(px(12.))
        .py(px(7.))
        .rounded(px(metrics::RADIUS))
        .border_1()
        .border_color(t.err_soft)
        .bg(t.surface);
    for (i, l) in tool.error.iter().enumerate() {
        card = card.child(div().min_w_0().text_color(if i == 0 { t.err } else { t.muted }).child(*l));
    }
    div().flex().flex_col().w_full().child(header).child(card).into_any_element()
}

fn diff_block(t: &Theme, d: &Diff) -> impl IntoElement {
    diff::card(t, d)
}

fn bash(t: &Theme, b: &Bash) -> impl IntoElement {
    // As the TUI's bash card: the command behind a faint `$`, the output muted, the footer facts faint (a failed
    // exit in err). The edge takes the tone: pending accent_soft, success line, error err_soft.
    let (edge, status) = match b.exit {
        None => (t.accent_soft, ui(t).flex_none().text_color(t.faint).child(format!("running {}", b.time))),
        Some(0) => (t.line, ui(t).flex_none().text_color(t.faint).child(format!("Wall: {}", b.time))),
        Some(code) => (
            t.err_soft,
            ui(t)
                .flex()
                .flex_none()
                .text_color(t.faint)
                .child(format!("Wall: {}", b.time))
                .child(div().text_color(t.line).child(" · "))
                .child(div().text_color(t.err).child(format!("Exit: {code}"))),
        ),
    };
    // A block: the command on a header bar, the output under it.
    let header = ui(t)
        .flex()
        .items_center()
        .gap(px(8.))
        .px(px(12.))
        .py(px(6.))
        .whitespace_nowrap()
        .bg(t.surface)
        .child(mono(t).flex_none().text_color(t.faint).child("$"))
        .child(mono(t).flex_1().min_w_0().truncate().text_color(t.accent).child(b.command))
        .child(status);
    let mut tail = mono(t).flex().flex_col().px(px(12.)).py(px(6.)).text_color(t.muted);
    if b.hidden > 0 {
        tail = tail.child(div().flex_none().text_color(t.faint).child(format!("… {} more lines", b.hidden)));
    }
    for l in &b.tail {
        tail = tail.child(div().flex_none().truncate().child(if l.is_empty() { " " } else { *l }));
    }
    div()
        .w_full()
        .overflow_hidden()
        .rounded(px(metrics::RADIUS))
        .border_1()
        .border_color(edge)
        .child(header)
        .when(!b.tail.is_empty() || b.hidden > 0, |d| d.child(div().border_t_1().border_color(edge).child(tail)))
}

/// A bordered band for a group of rows (todo, subagents).
fn boxed(t: &Theme) -> gpui::Div {
    div().w_full().rounded(px(metrics::RADIUS)).bg(t.surface).border_1().border_color(t.line).px(px(12.)).py(px(8.))
}

fn subagents(t: &Theme, subs: &[Sub]) -> impl IntoElement {
    let count = |state: SubState| subs.iter().filter(|s| s.state == state).count();
    let (done, failed, running) = (count(SubState::Done), count(SubState::Failed), count(SubState::Running));
    let mut parts = vec![format!("{done} done")];
    if failed > 0 {
        parts.push(format!("{failed} failed"));
    }
    if running > 0 {
        parts.push(format!("{running} running"));
    }
    let head = parts.join(" · ");
    // As the TUI's job rows: a state mark (the spinner while running, ✓ when done, ✗ when failed), the name in the default text,
    // the kind and time muted, the spend in syn_type, the last activity muted; the counts in the header are faint,
    // as the todo's are.
    let mut col = ui(t).flex().flex_col().gap(px(3.)).child(
        ui(t)
            .flex()
            .child(div().flex_none().w(px(t.ty.code * ICON_W)).text_color(t.faint).child("●"))
            .child(div().font_weight(FontWeight::BOLD).text_color(t.tool).child("Subagents"))
            .child(div().pl(px(t.ty.code)).text_color(t.faint).child(head)),
    );
    for s in subs {
        let (icon, fg) = match s.state {
            SubState::Running => (SPINNER, t.accent),
            SubState::Done => ("✓", t.ok),
            SubState::Failed => ("✗", t.err),
        };
        col = col.child(
            ui(t)
                .flex()
                .items_center()
                .whitespace_nowrap()
                .pl(px(t.ty.code * ICON_W))
                .child(div().flex_none().w(px(t.ty.code * ICON_W)).text_color(fg).child(icon))
                .child(div().flex_none().w(px(t.ty.ui * SUB_NAME_EM * 0.6 + 8.)).child(s.name))
                .child(div().flex_none().w(px(t.ty.ui * SUB_KIND_EM * 0.6 + 8.)).text_color(t.muted).child(s.kind))
                .child(div().flex_none().w(px(t.ty.ui * SUB_TIME_EM * 0.6 + 8.)).text_color(t.muted).child(s.time))
                .child(div().flex_none().w(px(t.ty.ui * SUB_COST_EM * 0.6 + 8.)).text_color(t.syn_type).child(s.cost))
                .child(div().flex_1().min_w_0().truncate().text_color(t.muted).child(s.summary)),
        );
    }
    col
}

fn todo(t: &Theme, items: &[TodoItem]) -> impl IntoElement {
    let done = items.iter().filter(|i| i.state == 2).count();
    // As the TUI's todo result: a bullet header, `Todo · done/total`, then a mark per task.
    let mut col = ui(t).flex().flex_col().gap(px(2.)).child(
        ui(t)
            .flex()
            .child(div().flex_none().w(px(t.ty.code * ICON_W)).text_color(t.faint).child("●"))
            .child(div().font_weight(FontWeight::BOLD).text_color(t.tool).child("Todo"))
            .child(div().text_color(t.faint).child(format!(" · {done}/{}", items.len()))),
    );
    for i in items {
        let (icon, icon_fg, fg) = match i.state {
            2 => ("✓", t.ok, t.faint),
            1 => ("▸", t.warn, t.warn),
            _ => ("○", t.faint, t.muted),
        };
        col = col.child(
            ui(t)
                .flex()
                .pl(px(t.ty.code * ICON_W))
                .text_color(fg)
                .child(div().flex_none().w(px(t.ty.code * ICON_W)).text_color(icon_fg).child(icon))
                .child(div().flex_1().min_w_0().child(i.text)),
        );
    }
    col
}

fn error(t: &Theme, msg: &'static str) -> impl IntoElement {
    // As the TUI's error lines: the whole text in the err role.
    ui(t)
        .border_1()
        .border_color(t.err_soft)
        .rounded(px(metrics::RADIUS))
        .bg(t.surface)
        .px(px(12.))
        .py(px(7.))
        .text_color(t.err)
        .child(msg)
}

fn plan(t: &Theme, msg: &'static str) -> impl IntoElement {
    // The TUI prints a parked plan as a notice: muted text, no glyph; the outline is the pending tone.
    prose(t)
        .text_size(px(t.ty.ui))
        .px(px(14.))
        .py(px(9.))
        .border_1()
        .border_color(t.accent_soft)
        .rounded(px(metrics::RADIUS_LG))
        .text_color(t.muted)
        .child(msg)
}
