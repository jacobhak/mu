//! The dock: approval (both steps), questions with a preview, the final review, document review.
use super::{ViewCtx, markdown, mono, ui};
use crate::data::{Approval, DocReview, Question, QuestionsReview, Request};
use crate::theme::{Lift, Theme, metrics};
use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, Div, FontWeight, InteractiveElement as _, IntoElement as _, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, div, px,
};

/// The document may take this much of the tile; the tile's height is unknown here, so a pixel cap stands in
/// for the wireframe's 55% (a percentage needs a definite parent height, which the dock's wrapper lacks).
const DOC_MAX_H: f32 = 340.0;
const PREVIEW_W: f32 = 232.0;
const MARKER_W: f32 = 16.0;
const KEY_W: f32 = 16.0;

/// The session's request docked between the transcript and the composer; `None` when nothing waits.
pub fn dock(c: &ViewCtx) -> Option<AnyElement> {
    let t = c.t;
    let body = match c.s().request.as_ref()? {
        Request::Approval(a) => approval(t, a),
        Request::Question(q) => question(t, q),
        Request::QuestionsReview(r) => questions_review(t, r),
        Request::Review(r) => review(c, r),
    };
    Some(
        // A raised card that interrupts the flow: lifted off the tile, with an accent edge on top.
        div().px(px(metrics::PAD)).pt(px(metrics::PAD_SM)).pb(px(metrics::S0_5)).child(
            ui(t)
                .flex()
                .flex_col()
                .gap(px(metrics::GAP_SM))
                .px(px(metrics::PAD_X))
                .py(px(metrics::PAD_Y))
                .rounded(px(metrics::RADIUS_LG))
                .bg(t.raised)
                .border_1()
                .border_color(t.accent_soft)
                .shadow(t.shadow(Lift::Card))
                .child(body),
        )
        .into_any_element(),
    )
}

/// A stack of text lines (a head): tight pairs.
fn col() -> Div {
    div().flex().flex_col().gap(px(metrics::GAP_SM))
}

/// A stack of blocks in the dock: head, command, options, hint.
fn blocks() -> Div {
    div().flex().flex_col().gap(px(metrics::GAP))
}

fn hint(t: &Theme, text: &'static str) -> Div {
    div().text_size(px(t.ty.small)).text_color(t.muted).child(text)
}

/// The `❯` the TUI puts before the highlighted row, in the accent.
fn marker(t: &Theme, selected: bool) -> Div {
    div().flex_none().w(px(MARKER_W)).text_color(t.accent).child(if selected { "❯" } else { " " })
}

/// A key as a small cap: filled and accented on the selected row.
fn key(t: &Theme, k: &'static str, selected: bool) -> Div {
    ui(t)
        .flex_none()
        .flex()
        .justify_center()
        .min_w(px(KEY_W))
        .px(px(metrics::CAP_PX))
        .py(px(metrics::CAP_PY))
        .rounded(px(metrics::RADIUS_SM))
        .border_1()
        .border_color(if selected { t.accent } else { t.line })
        .bg(if selected { t.selected } else { t.window })
        .text_size(px(t.ty.small))
        .text_color(if selected { t.accent } else { t.muted })
        .child(k)
}

/// A choice as a button: a bordered row, filled with the selected-row colour and edged in the accent when selected.
fn choice_row(t: &Theme, selected: bool, k: &'static str, label: &'static str, detail: &'static str) -> Div {
    let mut row = button(t, selected)
        .items_center()
        .gap(px(metrics::GAP))
        .child(marker(t, selected))
        .child(key(t, k, selected))
        .child(
            div()
                .flex_none()
                .text_color(t.text)
                .when(selected, |d| d.font_weight(FontWeight::SEMIBOLD))
                .child(label),
        );
    if !detail.is_empty() {
        row = row.child(mono(t).flex_1().min_w_0().truncate().text_right().text_color(t.muted).child(detail));
    }
    row
}

/// A button or option row: at least a control high, PAD_X across, the row's vertical padding.
fn button(t: &Theme, selected: bool) -> Div {
    div()
        .flex()
        .min_h(px(metrics::CONTROL_H))
        .px(px(metrics::PAD_X))
        .py(px(metrics::ROW_PY))
        .rounded(px(metrics::RADIUS))
        .border_1()
        .border_color(if selected { t.accent } else { t.line_faint })
        .bg(if selected { t.selected } else { t.window })
}

fn approval(t: &Theme, a: &Approval) -> AnyElement {
    let mut head = col().child(div().text_color(t.text).font_weight(FontWeight::SEMIBOLD).child(a.title));
    if let Some(from) = a.from {
        head = head.child(div().text_color(t.text).child(format!("from {from}")));
    }
    head = head.child(div().text_color(t.text).child(a.why));
    let code = mono(t)
        .px(px(metrics::PAD_X))
        .py(px(metrics::PAD_Y))
        .rounded(px(metrics::RADIUS))
        .bg(t.window)
        .border_1()
        .border_color(t.line)
        .text_color(t.text)
        .child(a.body);
    let rows = div().flex().flex_col().gap(px(metrics::GAP_SM)).children(
        a.choices.iter().enumerate().map(|(i, ch)| choice_row(t, i == a.selected, ch.key, ch.label, ch.detail)),
    );
    blocks().child(head).child(code).child(rows).child(hint(t, a.hint)).into_any_element()
}

fn option_row(t: &Theme, q: &Question, i: usize) -> Div {
    let o = &q.options[i];
    let selected = i == q.selected;
    let number: &'static str = ["1", "2", "3", "4", "5", "6", "7", "8", "9"].get(i).copied().unwrap_or("");
    // The TUI appends the recommendation to the label, in the label's own style.
    let label = if o.recommended { format!("{} (Recommended)", o.label) } else { o.label.to_string() };
    let mut text = col().gap(px(0.)).child(
        div()
            .text_color(t.text)
            .when(selected, |d| d.font_weight(FontWeight::SEMIBOLD))
            .child(label),
    );
    if !o.desc.is_empty() {
        text = text.child(div().text_size(px(t.ty.small)).text_color(t.muted).child(o.desc));
    }
    button(t, selected)
        .items_start()
        .gap(px(metrics::GAP))
        .child(marker(t, selected))
        .child(key(t, number, selected))
        .child(text.flex_1().min_w_0())
}

/// What a preview's border and text take: the TUI's kinds, guessed from the text as
/// `widgets/select/preview.rs` does when the ask declares none (the sketch's data declares none).
#[derive(Clone, Copy, PartialEq)]
enum PreviewKind {
    Code,
    Diff,
    Layout,
    Text,
}

fn preview_kind(text: &str) -> PreviewKind {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let n = lines.len();
    let count = |p: &dyn Fn(&str) -> bool| lines.iter().filter(|l| p(l)).count();
    let add = count(&|l| l.starts_with('+'));
    let del = count(&|l| l.starts_with('-'));
    if lines.iter().any(|l| l.starts_with("@@") || l.starts_with("diff --git"))
        || (add > 0 && del > 0 && (add + del) * 2 >= n)
    {
        return PreviewKind::Diff;
    }
    if text.chars().any(|c| ('\u{2500}'..='\u{257f}').contains(&c)) || count(&|l| l.trim_start().starts_with('|')) >= 2
    {
        return PreviewKind::Layout;
    }
    const STARTS: [&str; 12] =
        ["fn ", "def ", "let ", "const ", "import ", "use ", "class ", "if ", "for ", "return ", "//", "#"];
    let code = count(&|l| {
        let t = l.trim();
        t.ends_with([';', '{', '}', '(', ')', ',']) || STARTS.iter().any(|s| t.starts_with(s))
    });
    if n > 0 && code * 2 >= n { PreviewKind::Code } else { PreviewKind::Text }
}

/// A preview in a bordered box, its kind in the top edge. Plain text in the muted role, diff rows in the
/// ok/err/accent roles, the rest in the text colour (the TUI's syntax colours are not drawn here).
fn preview_box(t: &Theme, text: &'static str) -> Div {
    let kind = preview_kind(text);
    let (role, label) = match kind {
        PreviewKind::Code => (t.syn_function, "code"),
        PreviewKind::Diff => (t.warn, "diff"),
        PreviewKind::Layout => (t.tool, "layout"),
        PreviewKind::Text => (t.muted, "text"),
    };
    let rows = text.trim_end_matches('\n').lines().map(|r| {
        let color = match kind {
            PreviewKind::Diff if r.starts_with('+') => t.ok,
            PreviewKind::Diff if r.starts_with('-') => t.err,
            PreviewKind::Diff if r.starts_with("@@") => t.accent,
            PreviewKind::Text => t.muted,
            _ => t.text,
        };
        div().text_color(color).child(r)
    });
    div()
        .flex_none()
        .w(px(PREVIEW_W))
        .self_start()
        .overflow_hidden()
        .rounded(px(metrics::RADIUS))
        .border_1()
        .border_color(role)
        .child(
            ui(t)
                .h(px(metrics::ROW_H))
                .flex()
                .items_center()
                .px(px(metrics::PAD_X))
                .border_b_1()
                .border_color(role)
                .text_size(px(t.ty.small))
                .text_color(role)
                .child(label),
        )
        .child(
            mono(t)
                .flex()
                .flex_col()
                .px(px(metrics::PAD_X))
                .py(px(metrics::PAD_Y))
                .children(rows),
        )
}

fn question(t: &Theme, q: &Question) -> AnyElement {
    // The context sits above the title in the TUI's default text colour; the title is bold.
    let head = col()
        .when(!q.context.is_empty(), |d| d.child(div().text_color(t.text).child(q.context)))
        .child(
            div()
                .text_color(t.text)
                .font_weight(FontWeight::SEMIBOLD)
                .child(format!("{}/{} · {}: {}", q.index, q.total, q.header, q.question)),
        );
    let list = div()
        .flex()
        .flex_col()
        .gap(px(metrics::GAP_SM))
        .flex_1()
        .min_w_0()
        .children((0..q.options.len()).map(|i| option_row(t, q, i)));
    let mut body = div().flex().gap(px(metrics::GAP)).child(list);
    if let Some(p) = q.options.get(q.selected).and_then(|o| o.preview) {
        body = body.child(preview_box(t, p));
    }
    blocks().child(head).child(body).child(hint(t, q.hint)).into_any_element()
}

fn questions_review(t: &Theme, r: &QuestionsReview) -> AnyElement {
    let pairs = r.answers.iter().enumerate().map(|(i, (q, a))| {
        col()
            .gap(px(0.))
            .px(px(metrics::PAD_X))
            .py(px(metrics::PAD_Y))
            .rounded(px(metrics::RADIUS))
            .bg(t.window)
            .border_1()
            .border_color(t.line_faint)
            .child(div().text_color(t.text).child(format!("{}. {}", i + 1, q)))
            .child(div().text_color(t.muted).child(*a))
    });
    // The TUI's review opens with "Submit answers" highlighted.
    let submit = button(t, true)
        .items_center()
        .gap(px(metrics::GAP))
        .child(marker(t, true))
        .child(div().flex_none().text_color(t.text).font_weight(FontWeight::SEMIBOLD).child("Submit answers"));
    let rows = div().flex().flex_col().gap(px(metrics::GAP_SM)).children(pairs).child(submit);
    blocks()
        .child(div().text_color(t.accent).font_weight(FontWeight::SEMIBOLD).child(r.header))
        .child(rows)
        .child(hint(t, r.hint))
        .into_any_element()
}

/// The model slider as the TUI's footer row: `continue with ◂ model ▸`, each arrow in the accent where it can move.
fn model_slider(t: &Theme, r: &DocReview) -> Div {
    let last = r.models.len().saturating_sub(1);
    let arrow = |on: bool| if on { t.accent } else { t.muted };
    div()
        .flex()
        .items_center()
        .gap(px(metrics::GAP_SM))
        .min_h(px(metrics::CONTROL_H_SM))
        .child(div().text_color(t.muted).child("continue with"))
        .child(div().text_color(arrow(r.model > 0)).child("◂"))
        .child(div().text_color(t.text).child(r.models.get(r.model).copied().unwrap_or_default()))
        .child(div().text_color(arrow(r.model < last)).child("▸"))
}

fn review(c: &ViewCtx, r: &DocReview) -> AnyElement {
    let t = c.t;
    let doc = div()
        .id(("review-doc-scroll", c.session))
        .max_h(px(DOC_MAX_H))
        .overflow_y_scroll()
        .px(px(metrics::PAD_X))
        .py(px(metrics::PAD_Y))
        .rounded(px(metrics::RADIUS))
        .bg(t.window)
        .border_1()
        .border_color(t.line)
        .shadow(t.shadow(Lift::Rest))
        .child(markdown::render(t, &r.doc));
    let actions = div()
        .flex()
        .flex_col()
        .gap(px(metrics::GAP_SM))
        .children(r.actions.iter().enumerate().map(|(i, a)| choice_row(t, i == r.selected, a.key, a.label, a.detail)));
    blocks()
        .child(div().text_color(t.accent).font_weight(FontWeight::SEMIBOLD).child(r.title))
        .child(doc)
        .child(actions)
        .children((r.models.len() > 1).then(|| model_slider(t, r)))
        .child(hint(t, r.hint))
        .into_any_element()
}
