//! Every block type, at rest and streaming. The view is top-aligned: anchoring to the live end needs scroll
//! state the sketch does not keep.
use super::{ViewCtx, diff, markdown, mono, prose, ui};
use crate::data::{Bash, Block, Diff, Sub, TodoItem, Tool};
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
        .child(div().id(("transcript-scroll", c.session)).size_full().overflow_y_scroll().child(super::column(t, col)))
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
    matches!(
        b,
        Block::Thinking { .. }
            | Block::Tool(_)
            | Block::Bash(_)
            | Block::Retry(_)
            | Block::Notice(_)
            | Block::Compaction(_)
            | Block::Answered(_)
    )
}

fn block(t: &Theme, b: &Block) -> AnyElement {
    match b {
        Block::User { text, image } => user(t, *text, *image).into_any_element(),
        Block::Woken { text } => line(t, "✦", t.accent, t.muted, *text).italic().into_any_element(),
        Block::Thinking { time, live } => thinking(t, *time, *live).into_any_element(),
        Block::Tool(tool) => tool_row(t, tool).into_any_element(),
        Block::Prose(md) => div().child(markdown::render(t, md)).into_any_element(),
        Block::Diff(d) => diff_block(t, d).into_any_element(),
        Block::Bash(b) => bash(t, b).into_any_element(),
        Block::Subagents(subs) => subagents(t, subs).into_any_element(),
        Block::Todo(items) => todo(t, items).into_any_element(),
        Block::Retry(s) => line(t, "↻", t.warn, t.muted, *s).into_any_element(),
        Block::Notice(s) => line(t, "ℹ", t.faint, t.muted, *s).into_any_element(),
        Block::Error(s) => error(t, *s).into_any_element(),
        Block::Compaction(s) => line(t, "⊟", t.faint, t.muted, *s).into_any_element(),
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

fn user(t: &Theme, text: &'static str, image: Option<&'static str>) -> impl IntoElement {
    prose(t)
        .flex()
        .items_start()
        .gap(px(12.))
        .w_full()
        .px(px(14.))
        .py(px(10.))
        .bg(t.surface)
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
        None => ui(t).text_color(t.thinking).child(format!("▸ Thought for {time}")).into_any_element(),
        Some(tail) => div()
            .flex()
            .flex_col()
            .child(
                ui(t)
                    .flex()
                    .text_color(t.accent)
                    .child(div().flex_none().w(px(t.ty.code * ICON_W)).child(SPINNER))
                    .child(format!("Thinking · {time}")),
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

fn tool_row(t: &Theme, tool: &Tool) -> impl IntoElement {
    let facts = match (tool.facts.is_empty(), tool.streaming) {
        (_, true) if tool.facts.is_empty() => "…".to_string(),
        (_, true) => format!("{} …", tool.facts),
        _ => tool.facts.to_string(),
    };
    // The verb and the facts are interface text; only the arguments (a path, a pattern) are code.
    ui(t)
        .flex()
        .items_center()
        .whitespace_nowrap()
        .text_color(t.muted)
        .child(div().flex_none().w(px(t.ty.code * ICON_W)).text_color(t.tool).child(tool.kind.icon()))
        .child(div().flex_none().w(px(t.ty.code * VERB_EM)).child(tool.verb))
        .child(mono(t).min_w_0().truncate().text_color(t.text).child(tool.target))
        .when(!facts.is_empty(), |d| d.child(div().flex_none().pl(px(t.ty.code)).text_color(t.faint).child(facts)))
}

fn diff_block(t: &Theme, d: &Diff) -> impl IntoElement {
    ui(t)
        .flex()
        .flex_col()
        .gap(px(5.))
        .child(
            ui(t)
                .flex()
                .items_center()
                .whitespace_nowrap()
                .text_color(t.muted)
                .child(div().flex_none().w(px(t.ty.code * ICON_W)).text_color(t.tool).child("✎"))
                .child(div().flex_none().w(px(t.ty.code * VERB_EM)).child("Edit"))
                .child(mono(t).min_w_0().truncate().text_color(t.text).child(d.file))
                .child(div().flex_none().pl(px(t.ty.code)).text_color(t.ok).child(format!("+{}", d.plus)))
                .child(div().flex_none().pl(px(t.ty.code * 0.5)).text_color(t.err).child(format!("−{}", d.minus))),
        )
        .child(
            div()
                .w_full()
                .overflow_hidden()
                .border_1()
                .border_color(t.line_faint)
                .rounded(px(metrics::RADIUS))
                .child(diff::lines(t, &d.lines)),
        )
        .when(d.more > 0, |el| {
            el.child(
                ui(t)
                    .pl(px(t.ty.code * ICON_W))
                    .text_size(px(t.ty.small))
                    .text_color(t.faint)
                    .child(format!("{} more lines", d.more)),
            )
        })
}

fn bash(t: &Theme, b: &Bash) -> impl IntoElement {
    let failed = b.exit.is_some_and(|c| c != 0);
    let (status, status_fg) = match b.exit {
        None => (format!("{SPINNER} {}", b.time), t.accent),
        Some(0) => (format!("✓ 0 · {}", b.time), t.ok),
        Some(code) => (format!("✗ {code} · {}", b.time), t.err),
    };
    let mut tail = mono(t).flex().flex_col().pl(px(t.ty.code * ICON_W)).pt(px(2.)).text_color(t.muted);
    for l in &b.tail {
        tail = tail.child(div().flex_none().truncate().child(if l.is_empty() { " " } else { *l }));
    }
    if b.hidden > 0 {
        tail = tail.child(
            div().flex().justify_end().text_size(px(t.ty.small)).text_color(t.faint).child(format!("({} lines)", b.hidden)),
        );
    }
    ui(t)
        .flex()
        .flex_col()
        .border_l_2()
        .border_color(if failed { t.err } else { gpui::transparent_black() })
        .pl(px(if failed { 8. } else { 0. }))
        .child(
            ui(t)
                .flex()
                .items_center()
                .whitespace_nowrap()
                .child(mono(t).flex_none().w(px(t.ty.code * ICON_W)).text_color(t.tool).child("$"))
                .child(mono(t).flex_1().min_w_0().truncate().child(b.command))
                .child(div().flex_none().pl(px(t.ty.code)).text_color(status_fg).child(status)),
        )
        .when(!b.tail.is_empty() || b.hidden > 0, |d| d.child(tail))
}

fn subagents(t: &Theme, subs: &[Sub]) -> impl IntoElement {
    let done = subs.iter().filter(|s| s.done).count();
    let running = subs.len() - done;
    let head = if running == 0 { format!("{done} done") } else { format!("{done} done · {running} running") };
    let mut col = ui(t).flex().flex_col().gap(px(3.)).child(
        ui(t)
            .flex()
            .text_color(t.muted)
            .child(div().flex_none().w(px(t.ty.code * ICON_W)).text_color(t.tool).child("◆"))
            .child(div().text_color(t.text).child("Subagents"))
            .child(div().pl(px(t.ty.code)).child(head)),
    );
    for s in subs {
        let (icon, fg) = if s.done { ("✓", t.ok) } else { ("●", t.accent) };
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
                .child(div().flex_1().min_w_0().truncate().text_color(t.faint).child(format!("\"{}\"", s.summary))),
        );
    }
    col
}

fn todo(t: &Theme, items: &[TodoItem]) -> impl IntoElement {
    let done = items.iter().filter(|i| i.state == 2).count();
    let mut col = ui(t).flex().flex_col().gap(px(2.)).child(
        ui(t)
            .flex()
            .text_color(t.muted)
            .child(div().flex_none().w(px(t.ty.code * ICON_W)).text_color(t.tool).child("☰"))
            .child(div().text_color(t.text).child("Todo"))
            .child(div().pl(px(t.ty.code)).child(format!("{done}/{}", items.len()))),
    );
    for i in items {
        let (icon, icon_fg, fg) = match i.state {
            2 => ("✓", t.ok, t.faint),
            1 => ("▸", t.accent, t.text),
            _ => ("○", t.faint, t.muted),
        };
        col = col.child(
            ui(t)
                .flex()
                .pl(px(t.ty.code * ICON_W))
                .text_color(fg)
                .child(div().flex_none().w(px(t.ty.code * ICON_W)).text_color(icon_fg).child(icon))
                .child(div().flex_1().min_w_0().when(i.state == 2, |d| d.line_through()).child(i.text)),
        );
    }
    col
}

fn error(t: &Theme, msg: &'static str) -> impl IntoElement {
    ui(t)
        .flex()
        .items_start()
        .border_1()
        .border_color(t.err.opacity(0.4))
        .rounded(px(metrics::RADIUS))
        .bg(t.err.opacity(0.08))
        .px(px(12.))
        .py(px(7.))
        .child(div().flex_none().w(px(t.ty.ui * 1.5)).text_color(t.err).child("✗"))
        .child(div().flex_1().min_w_0().text_color(t.text).child(msg))
}

fn plan(t: &Theme, msg: &'static str) -> impl IntoElement {
    prose(t)
        .text_size(px(t.ty.ui))
        .flex()
        .items_start()
        .px(px(14.))
        .py(px(9.))
        .border_1()
        .border_color(t.accent_soft)
        .rounded(px(metrics::RADIUS_LG))
        .child(div().flex_none().w(px(t.ty.ui * 1.7)).text_color(t.accent).child("▣"))
        .child(div().flex_1().min_w_0().child(msg))
}
