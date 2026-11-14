//! Preview boxes under an option in the ask layout.

use crate::text::line::{self, Line, dim, span};
use crate::theme::{self, Role};
use crossterm::style::ContentStyle;
use mu_base::events::PreviewKind;

/// Columns a description or preview is indented under its option.
pub(super) const INDENT: usize = 4;

/// Rows a highlighted option's preview may take, the truncation marker included.
pub(super) const PREVIEW_MAX_LINES: usize = 16;

/// `l` after `by` spaces.
pub(super) fn indented(l: Line, by: usize) -> Line {
    let mut row = vec![span(" ".repeat(by), ContentStyle::default())];
    row.extend(l);
    row
}

/// What a preview shows: the declared kind, else a guess from the text.
pub(super) fn preview_kind(text: &str, hint: Option<PreviewKind>) -> PreviewKind {
    if let Some(k) = hint {
        return k;
    }
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

fn preview_row_style(kind: PreviewKind, row: &str) -> ContentStyle {
    let t = theme::get();
    match kind {
        PreviewKind::Diff if row.starts_with('+') => t.fg(Role::Ok),
        PreviewKind::Diff if row.starts_with('-') => t.fg(Role::Err),
        PreviewKind::Diff if row.starts_with("@@") => t.fg(Role::Accent),
        PreviewKind::Text => t.fg(Role::Muted),
        _ => ContentStyle::default(),
    }
}

/// The tree-sitter language of a code preview, guessed from its text; only languages mu compiles in.
fn guess_lang(code: &str) -> Option<&'static str> {
    let has = |s: &str| code.contains(s);
    let starts = |ps: &[&str]| code.lines().any(|l| ps.iter().any(|p| l.trim_start().starts_with(p)));
    let t = code.trim_start();
    if (t.starts_with('{') || t.starts_with('[')) && has("\":") {
        Some("json")
    } else if starts(&["fn ", "pub fn ", "let mut ", "impl ", "struct ", "pub struct ", "enum ", "use std"])
        || has("println!")
    {
        Some("rust")
    } else if starts(&["def ", "import ", "elif ", "print("]) || (starts(&["from "]) && has(" import ")) {
        Some("python")
    } else if starts(&["function ", "const ", "let "]) || has("=>") || has("console.") {
        Some("javascript")
    } else if t.starts_with("#!") || starts(&["$ ", "cargo ", "git "]) {
        Some("bash")
    } else {
        None
    }
}

/// A preview in a labelled box `width` columns wide and at most `cap` rows tall, indented under its
/// option. Past the cap the last body row says how many lines were cut. Empty when it can't fit.
pub(super) fn preview_box(
    text: &str,
    hint: Option<PreviewKind>,
    width: usize,
    cap: usize,
    highlighted: bool,
) -> Vec<Line> {
    if cap < 3 || width < 6 {
        return Vec::new();
    }
    let kind = preview_kind(text, hint);
    let (role, label) = match kind {
        PreviewKind::Code => (Role::SynFunction, "code"),
        PreviewKind::Diff => (Role::Warn, "diff"),
        PreviewKind::Layout => (Role::Tool, "layout"),
        PreviewKind::Text => (Role::Muted, "text"),
    };
    let mut border = theme::get().fg(role);
    if highlighted {
        border.attributes.set(crossterm::style::Attribute::Bold);
    }
    let inner = width - 4;
    let clean = mu_base::text::sanitize(text.trim_end_matches('\n'));
    let src: Vec<&str> = clean.split('\n').collect();
    let max = cap - 2;
    let keep = if src.len() > max { max - 1 } else { src.len() };
    let hl = (kind == PreviewKind::Code)
        .then(|| guess_lang(&clean))
        .flatten()
        .and_then(|lang| crate::text::highlight::highlight(&clean, lang))
        .filter(|h| h.len() == src.len());
    let mut body: Vec<Line> = src[..keep]
        .iter()
        .enumerate()
        .map(|(i, r)| match &hl {
            Some(h) => line::truncate(&h[i], inner),
            None => vec![span(mu_base::text::truncate_text(r, inner), preview_row_style(kind, r))],
        })
        .collect();
    if keep < src.len() {
        body.push(line::styled(
            mu_base::text::truncate_text(&format!("… {} more lines", src.len() - keep), inner),
            |_| dim(),
        ));
    }
    let title = format!(" {label} ");
    let top = if title.len() + 3 <= width {
        format!("┌─{title}{}┐", "─".repeat(width - 3 - title.len()))
    } else {
        format!("┌{}┐", "─".repeat(width - 2))
    };
    let mut out = vec![indented(vec![span(top, border)], INDENT)];
    for l in body {
        let pad = inner.saturating_sub(line::line_width(&l));
        let mut row = vec![span("│ ", border)];
        row.extend(l);
        row.push(span(format!("{} │", " ".repeat(pad)), border));
        out.push(indented(row, INDENT));
    }
    out.push(indented(vec![span(format!("└{}┘", "─".repeat(width - 2)), border)], INDENT));
    out
}
