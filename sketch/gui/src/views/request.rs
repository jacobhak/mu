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
const PREVIEW_W: f32 = 230.0;
const MARKER_W: f32 = 14.0;
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
        div().px(px(metrics::PAD)).pt(px(metrics::PAD_SM)).pb(px(2.)).child(
            ui(t)
                .flex()
                .flex_col()
                .gap(px(4.))
                .px(px(metrics::PAD))
                .py(px(metrics::PAD_SM + 4.))
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

fn col() -> Div {
    div().flex().flex_col().gap(px(2.))
}

fn hint(t: &Theme, text: &'static str) -> Div {
    div().pt(px(6.)).text_size(px(t.ty.small)).text_color(t.muted).child(text)
}

/// A key as a small cap: filled and accented on the selected row.
fn key(t: &Theme, k: &'static str, selected: bool) -> Div {
    ui(t)
        .flex_none()
        .flex()
        .justify_center()
        .min_w(px(KEY_W))
        .px(px(5.))
        .rounded(px(metrics::RADIUS_SM - 2.))
        .border_1()
        .border_color(if selected { t.accent } else { t.line })
        .bg(if selected { t.selected } else { t.window })
        .text_size(px(t.ty.small))
        .text_color(if selected { t.accent } else { t.muted })
        .child(k)
}

/// A choice as a button: a bordered row, filled with the accent tint and edged in the accent when selected.
fn choice_row(t: &Theme, selected: bool, k: &'static str, label: &'static str, detail: &'static str) -> Div {
    let mut row = button(t, selected)
        .items_center()
        .gap(px(8.))
        .h(px(metrics::ROW_H + 2.))
        .child(key(t, k, selected))
        .child(
            div()
                .flex_none()
                .text_color(if selected { t.strong } else { t.text })
                .when(selected, |d| d.font_weight(FontWeight::SEMIBOLD))
                .child(label),
        );
    if !detail.is_empty() {
        row = row.child(
            mono(t)
                .flex_1()
                .min_w_0()
                .truncate()
                .text_right()
                .text_size(px(t.ty.small))
                .text_color(t.muted)
                .child(detail),
        );
    }
    row
}

fn button(t: &Theme, selected: bool) -> Div {
    div()
        .flex()
        .px(px(8.))
        .rounded(px(metrics::RADIUS_SM))
        .border_1()
        .border_color(if selected { t.accent } else { t.line_faint })
        .bg(if selected { t.selected } else { t.window })
}


fn approval(t: &Theme, a: &Approval) -> AnyElement {
    let mut head = col().child(div().text_color(t.strong).font_weight(FontWeight::SEMIBOLD).child(a.title));
    if let Some(from) = a.from {
        head = head.child(
            div().flex().gap(px(4.)).text_color(t.muted).child("from").child(div().text_color(t.text).child(from)),
        );
    }
    head = head.child(div().text_color(t.muted).child(a.why));
    let code = mono(t)
        .my(px(6.))
        .px(px(12.))
        .py(px(8.))
        .rounded(px(metrics::RADIUS))
        .bg(t.surface)
        .border_1()
        .border_color(t.line)
        .text_color(t.strong)
        .child(a.body);
    let rows = div().flex().flex_col().gap(px(4.)).children(
        a.choices.iter().enumerate().map(|(i, ch)| choice_row(t, i == a.selected, ch.key, ch.label, ch.detail)),
    );
    col().child(head).child(code).child(rows).child(hint(t, a.hint)).into_any_element()
}

fn option_row(t: &Theme, q: &Question, i: usize) -> Div {
    let o = &q.options[i];
    let selected = i == q.selected;
    let number: &'static str = ["1", "2", "3", "4", "5", "6", "7", "8", "9"].get(i).copied().unwrap_or("");
    let mut title = div()
        .flex()
        .items_baseline()
        .gap(px(6.))
        .child(
            div()
                .text_color(if selected { t.strong } else { t.text })
                .when(selected, |d| d.font_weight(FontWeight::SEMIBOLD))
                .child(o.label),
        );
    if o.recommended {
        title = title.child(
            div()
                .px(px(6.))
                .rounded(px(999.))
                .bg(t.surface)
                .text_size(px(t.ty.small))
                .text_color(t.ok)
                .child("recommended"),
        );
    }
    let mut text = col().gap(px(0.)).child(title);
    if !o.desc.is_empty() {
        text = text.child(div().text_size(px(t.ty.small)).text_color(t.muted).child(o.desc));
    }
    button(t, selected).items_start().gap(px(8.)).py(px(5.)).child(key(t, number, selected)).child(text.flex_1().min_w_0())
}

fn preview_box(t: &Theme, text: &'static str) -> Div {
    div()
        .flex_none()
        .w(px(PREVIEW_W))
        .self_start()
        .overflow_hidden()
        .rounded(px(metrics::RADIUS))
        .border_1()
        .border_color(t.line)
        .child(
            ui(t)
                .px(px(10.))
                .py(px(4.))
                .bg(t.bar)
                .border_b_1()
                .border_color(t.line_faint)
                .text_size(px(t.ty.small))
                .text_color(t.muted)
                .child("preview"),
        )
        .child(mono(t).px(px(10.)).py(px(8.)).bg(t.surface).text_size(px(t.ty.small)).text_color(t.text).child(text))
}

fn question(t: &Theme, q: &Question) -> AnyElement {
    let head = col()
        .child(div().text_color(t.muted).child(format!("{} · question {} of {}", q.header, q.index, q.total)))
        .child(div().text_color(t.muted).child(q.context))
        .child(div().text_color(t.strong).font_weight(FontWeight::SEMIBOLD).child(q.question));
    let list = div()
        .flex()
        .flex_col()
        .gap(px(4.))
        .flex_1()
        .min_w_0()
        .pt(px(6.))
        .children((0..q.options.len()).map(|i| option_row(t, q, i)));
    let mut body = div().flex().gap(px(12.)).child(list);
    if let Some(p) = q.options.get(q.selected).and_then(|o| o.preview) {
        body = body.child(preview_box(t, p).mt(px(6.)));
    }
    col().child(head).child(body).child(hint(t, q.hint)).into_any_element()
}

fn questions_review(t: &Theme, r: &QuestionsReview) -> AnyElement {
    let pairs = div().flex().flex_col().gap(px(4.)).pt(px(6.)).children(r.answers.iter().map(|(q, a)| {
        col()
            .gap(px(0.))
            .px(px(10.))
            .py(px(5.))
            .rounded(px(metrics::RADIUS_SM))
            .bg(t.surface)
            .border_1()
            .border_color(t.line_faint)
            .child(div().text_size(px(t.ty.small)).text_color(t.muted).child(*q))
            .child(div().text_color(t.strong).font_weight(FontWeight::SEMIBOLD).child(*a))
    }));
    col()
        .child(div().text_color(t.muted).child(r.header))
        .child(pairs)
        .child(hint(t, r.hint))
        .into_any_element()
}

fn model_slider(t: &Theme, r: &DocReview) -> Div {
    let segs = r.models.iter().enumerate().map(|(i, m)| {
        let on = i == r.model;
        div()
            .px(px(10.))
            .py(px(3.))
            .rounded(px(999.))
            .text_size(px(t.ty.small))
            .when(on, |d| d.bg(t.selected).border_color(t.accent).text_color(t.strong).font_weight(FontWeight::SEMIBOLD))
            .when(!on, |d| d.border_color(t.line).bg(t.window).text_color(t.muted))
            .border_1()
            .child(*m)
    });
    div()
        .flex()
        .items_center()
        .gap(px(6.))
        .pt(px(4.))
        .child(div().text_size(px(t.ty.small)).text_color(t.muted).child("continue with"))
        .children(segs)
}

fn review(c: &ViewCtx, r: &DocReview) -> AnyElement {
    let t = c.t;
    let doc = div()
        .id(("review-doc-scroll", c.session))
        .max_h(px(DOC_MAX_H))
        .overflow_y_scroll()
        .mt(px(6.))
        .px(px(16.))
        .py(px(12.))
        .rounded(px(metrics::RADIUS))
        .bg(t.window)
        .border_1()
        .border_color(t.line)
        .shadow(t.shadow(Lift::Rest))
        .child(markdown::render(t, &r.doc));
    let actions = div().flex().flex_col().gap(px(4.)).pt(px(6.)).children(
        r.actions.iter().enumerate().map(|(i, a)| choice_row(t, i == r.selected, a.key, a.label, a.detail)),
    );
    col()
        .child(div().text_color(t.strong).font_weight(FontWeight::SEMIBOLD).child(r.title))
        .child(doc)
        .child(actions)
        .child(model_slider(t, r))
        .child(hint(t, r.hint))
        .into_any_element()
}
