use crate::theme::{self, Role};
use crossterm::style::{Attribute, ContentStyle};

pub(crate) mod chart;
pub(crate) mod diffview;
pub(crate) mod highlight;
pub(crate) mod line;
pub(crate) mod markdown;

pub(crate) fn accent() -> ContentStyle {
    let mut s = theme::get().fg(Role::Accent);
    s.attributes.set(Attribute::Bold);
    s
}

pub(crate) fn pad(s: &str, w: usize) -> String {
    let t = mu_base::text::truncate_text(s, w);
    let used = t.chars().count();
    format!("{t}{}", " ".repeat(w.saturating_sub(used)))
}

/// Wraps `text` to `width` columns on spaces; words longer than a row are cut by `truncate`.
pub(crate) fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut rows = Vec::new();
    let mut cur = String::new();
    for w in text.split_whitespace() {
        let fits = cur.is_empty() || cur.chars().count() + 1 + w.chars().count() <= width;
        if !fits {
            rows.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(w);
    }
    if !cur.is_empty() {
        rows.push(cur);
    }
    rows
}
