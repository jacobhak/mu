//! Diff rows for edit and write cards: a `│` line-number gutter, coloured added/removed rows with
//! the changed part of a modified line reversed, and syntax-highlighted context. Reads both
//! pi-edit's numbered rows (`-8|code`, `+8|code`, ` 9|code`, an empty row for a gap) and plain unified diffs.
use super::highlight;
use super::line::{Line, span};
use super::theme::{self, Role};
use crossterm::style::{Attribute, ContentStyle};
use std::ops::Range;

/// Rows shown before the card cuts to `… n more lines` (unless expanded).
pub const DIFF_CUT: usize = 40;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Context,
    Added,
    Removed,
}

#[derive(Debug)]
enum Row<'a> {
    Code {
        kind: Kind,
        num: Option<usize>,
        code: &'a str,
    },
    /// A hunk boundary: `@@ … @@` or pi-edit's empty gap row.
    Gap,
    /// File headers, `\ No newline at end of file` and anything else that is not code.
    Note(&'a str),
}

/// Splits pi-edit's `12|code` into the number and the code.
fn numbered(rest: &str) -> Option<(usize, &str)> {
    let bar = rest.find('|')?;
    let n = rest[..bar].parse().ok()?;
    Some((n, &rest[bar + 1..]))
}

/// `@@ -a,b +c,d @@` → `(a, c)`.
fn hunk_start(l: &str) -> Option<(usize, usize)> {
    let mut parts = l.split_whitespace().skip(1);
    let num = |s: Option<&str>, sign: char| -> Option<usize> { s?.strip_prefix(sign)?.split(',').next()?.parse().ok() };
    Some((num(parts.next(), '-')?, num(parts.next(), '+')?))
}

fn parse(diff: &str) -> Vec<Row<'_>> {
    let mut rows = Vec::new();
    // Unified-diff line counters; `None` until a hunk header gives them.
    let mut at: Option<(usize, usize)> = None;
    let mut in_hunk = false;
    for l in diff.lines() {
        if l.starts_with("@@") {
            in_hunk = true;
            at = hunk_start(l);
            if rows.iter().any(|r| matches!(r, Row::Code { .. })) {
                rows.push(Row::Gap);
            }
            continue;
        }
        if l.is_empty() {
            rows.push(Row::Gap);
            continue;
        }
        if !in_hunk && (l.starts_with("+++") || l.starts_with("---")) {
            rows.push(Row::Note(l));
            continue;
        }
        let (sign, rest) = l.split_at(l.chars().next().map_or(0, char::len_utf8));
        let kind = match sign {
            " " => Kind::Context,
            "+" => Kind::Added,
            "-" => Kind::Removed,
            _ => {
                rows.push(Row::Note(l));
                continue;
            }
        };
        let (num, code) = match numbered(rest) {
            Some((n, code)) => (Some(n), code),
            None => {
                let n = at.map(|(old, new)| if kind == Kind::Removed { old } else { new });
                if let Some((old, new)) = at.as_mut() {
                    match kind {
                        Kind::Context => {
                            *old += 1;
                            *new += 1;
                        }
                        Kind::Added => *new += 1,
                        Kind::Removed => *old += 1,
                    }
                }
                (n, rest)
            }
        };
        rows.push(Row::Code { kind, num, code });
    }
    while matches!(rows.last(), Some(Row::Gap)) {
        rows.pop();
    }
    rows
}

/// Highlights the new side of the diff (context and added rows) as a whole file, so multi-line
/// constructs (strings, comments) colour correctly, and returns each such row's highlighted line.
fn highlighted(rows: &[Row], lang: &str) -> Vec<Option<Line>> {
    let (mut text, mut idx) = (String::new(), Vec::new());
    for (i, r) in rows.iter().enumerate() {
        if let Row::Code { kind, code, .. } = r
            && *kind != Kind::Removed
        {
            text.push_str(code);
            text.push('\n');
            idx.push(i);
        }
    }
    let mut out = vec![None; rows.len()];
    if lang.is_empty() {
        return out;
    }
    let Some(lines) = highlight::highlight(&text, lang) else { return out };
    if lines.len() != idx.len() {
        return out;
    }
    for (l, i) in lines.into_iter().zip(idx) {
        out[i] = Some(l);
    }
    out
}

/// Char ranges that differ between a removed line and the added line replacing it: the common
/// prefix and suffix are trimmed back to word boundaries. `None` when the lines share too little
/// for a highlight to help.
fn changed(old: &str, new: &str) -> Option<(Range<usize>, Range<usize>)> {
    let (a, b): (Vec<char>, Vec<char>) = (old.chars().collect(), new.chars().collect());
    let word = |c: char| c.is_alphanumeric() || c == '_';
    let mut p = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    while p > 0 && p < a.len().min(b.len()) && word(a[p]) && word(a[p - 1]) {
        p -= 1;
    }
    let max_s = a.len().min(b.len()) - p;
    let mut s = a.iter().rev().zip(b.iter().rev()).take(max_s).take_while(|(x, y)| x == y).count();
    while s > 0 && s < max_s && word(a[a.len() - s]) && word(a[a.len() - s - 1]) {
        s -= 1;
    }
    let (ra, rb) = (p..a.len() - s, p..b.len() - s);
    let kept = p + s;
    let longest = a.len().max(b.len());
    // Mostly rewritten (or identical): tint the whole row instead.
    if (ra.is_empty() && rb.is_empty()) || kept * 2 < longest {
        return None;
    }
    Some((ra, rb))
}

/// Pairs each run of `k` removed rows directly followed by `k` added rows.
fn emphasis(rows: &[Row]) -> Vec<Option<Range<usize>>> {
    let mut out = vec![None; rows.len()];
    let kind_at = |i: usize| match rows.get(i) {
        Some(Row::Code { kind, .. }) => Some(*kind),
        _ => None,
    };
    let code_at = |i: usize| match &rows[i] {
        Row::Code { code, .. } => *code,
        _ => "",
    };
    let mut i = 0;
    while i < rows.len() {
        if kind_at(i) != Some(Kind::Removed) {
            i += 1;
            continue;
        }
        let dels = (i..).take_while(|&j| kind_at(j) == Some(Kind::Removed)).count();
        let adds = (i + dels..).take_while(|&j| kind_at(j) == Some(Kind::Added)).count();
        if dels == adds {
            for k in 0..dels {
                if let Some((ra, rb)) = changed(code_at(i + k), code_at(i + dels + k)) {
                    out[i + k] = Some(ra);
                    out[i + dels + k] = Some(rb);
                }
            }
        }
        i += dels + adds;
    }
    out
}

/// `code` in `style`, with the chars in `range` reversed (the card fill later turns that into the
/// hue as background and the surface as text).
fn emphasize(code: &str, style: ContentStyle, range: Option<&Range<usize>>) -> Line {
    let Some(r) = range else { return vec![span(code, style)] };
    let chars: Vec<char> = code.chars().collect();
    let (a, b) = (r.start.min(chars.len()), r.end.min(chars.len()));
    let mut rev = style;
    rev.attributes.set(Attribute::Reverse);
    let mut out = Line::new();
    for (text, st) in [(&chars[..a], style), (&chars[a..b], rev), (&chars[b..], style)] {
        if !text.is_empty() {
            out.push(span(text.iter().collect::<String>(), st));
        }
    }
    out
}

/// Card rows for `diff`, highlighted as `lang` (a name or file extension; empty for none), with the
/// added and removed line counts.
pub fn rows(diff: &str, lang: &str, expanded: bool) -> (Vec<Line>, usize, usize) {
    let t = theme::get();
    let parsed = parse(diff);
    let count = |k: Kind| parsed.iter().filter(|r| matches!(r, Row::Code { kind, .. } if *kind == k)).count();
    let (adds, dels) = (count(Kind::Added), count(Kind::Removed));
    // A new file (only added rows) reads as a listing: numbers and code, no signs.
    let listing = adds > 0 && adds == parsed.iter().filter(|r| matches!(r, Row::Code { .. })).count();
    let hl = highlighted(&parsed, lang);
    let emph = emphasis(&parsed);
    let width = parsed
        .iter()
        .filter_map(|r| match r {
            Row::Code { num: Some(n), .. } => Some(n.to_string().len()),
            _ => None,
        })
        .max();
    let w = width.unwrap_or(0);
    let faint = t.fg(Role::Faint);
    let mut out: Vec<Line> = Vec::new();
    for (i, r) in parsed.iter().enumerate() {
        let (kind, num, code) = match r {
            Row::Gap => {
                out.push(vec![span(format!("{}⋯", " ".repeat(w + 1)), faint)]);
                continue;
            }
            Row::Note(n) => {
                out.push(vec![span(*n, faint)]);
                continue;
            }
            Row::Code { kind, num, code } => (*kind, *num, *code),
        };
        let (sign, role) = match kind {
            Kind::Context => (' ', Role::Faint),
            Kind::Added => ('+', Role::Ok),
            Kind::Removed => ('-', Role::Err),
        };
        let n = num.map(|n| n.to_string()).unwrap_or_default();
        let mut row = Line::new();
        if listing {
            if width.is_some() {
                row.push(span(format!("{n:>w$} "), faint));
            }
        } else {
            row.push(span(format!("{sign}{n:>w$}"), t.fg(role)));
            row.push(span("│", t.fg(Role::Line)));
        }
        if !listing && kind != Kind::Context {
            row.extend(emphasize(code, t.fg(role), emph[i].as_ref()));
        } else {
            let code_line = hl[i].clone().unwrap_or_else(|| {
                let style = if kind == Kind::Context && !listing { t.fg(Role::Muted) } else { ContentStyle::default() };
                vec![span(code, style)]
            });
            row.extend(code_line);
        }
        out.push(row);
    }
    if !expanded && out.len() > DIFF_CUT {
        let more = out.len() - DIFF_CUT;
        out.truncate(DIFF_CUT);
        out.push(vec![span(format!("… {more} more lines"), faint)]);
    }
    (out, adds, dels)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::line::line_text;

    fn texts(ls: &[Line]) -> Vec<String> {
        ls.iter().map(line_text).collect()
    }

    #[test]
    fn numbered_rows_get_a_gutter() {
        let (ls, adds, dels) = rows(" 9|a\n-10|b\n+10|c\n 11|d\n\n 40|e", "", true);
        assert_eq!((adds, dels), (1, 1));
        assert_eq!(texts(&ls), ["  9│a", "-10│b", "+10│c", " 11│d", "   ⋯", " 40│e"]);
    }

    #[test]
    fn unified_rows_are_numbered_from_the_hunk() {
        let d = "--- a/f\n+++ b/f\n@@ -3,2 +3,2 @@\n x\n-y\n+z\n@@ -20 +20 @@\n w\n";
        let (ls, adds, dels) = rows(d, "", true);
        assert_eq!((adds, dels), (1, 1));
        assert_eq!(texts(&ls), ["--- a/f", "+++ b/f", "  3│x", "- 4│y", "+ 4│z", "   ⋯", " 20│w"]);
    }

    #[test]
    fn content_that_looks_like_a_header_counts_inside_a_hunk() {
        let (_, adds, dels) = rows("--- a/f\n+++ b/f\n@@ -1,2 +1,2 @@\n--- removed\n+++ added\n", "", true);
        assert_eq!((adds, dels), (1, 1));
        let (_, adds, dels) = rows("+a\n+b\n-c\n", "", true);
        assert_eq!((adds, dels), (2, 1), "a headerless diff counts every line");
    }

    #[test]
    fn a_new_file_is_a_listing() {
        let (ls, adds, _) = rows("@@ -1,0 +1,2 @@\n+a\n+b\n", "", true);
        assert_eq!(adds, 2);
        assert_eq!(texts(&ls), ["1 a", "2 b"]);
        assert!(ls.iter().flatten().all(|s| s.style.background_color.is_none()), "no tint");
    }

    #[test]
    fn modified_lines_mark_the_changed_words() {
        assert_eq!(changed("    total += x", "    acc += x"), Some((4..9, 4..7)));
        assert_eq!(changed("return total / len(x)", "return acc / len(xs)"), None, "too much changed");
        assert_eq!(changed("foo(bar)", "foo(baz)"), Some((4..7, 4..7)), "whole words, not shared letters");
        assert_eq!(changed("same", "same"), None);
    }

    #[test]
    fn emphasis_paints_only_the_change() {
        let (ls, ..) = rows("-1|    total += x\n+1|    acc += x", "", true);
        let added = &ls[1];
        let emph: String =
            added.iter().filter(|s| s.style.attributes.has(Attribute::Reverse)).map(|s| s.text.as_str()).collect();
        assert_eq!(emph, "acc");
        assert!(added.iter().all(|s| s.style.background_color.is_none()), "no row tint");
    }

    #[test]
    fn long_diffs_are_cut() {
        let d: String = (0..50).map(|i| format!("+{i}|l\n")).collect::<String>() + "-1|x\n";
        let (ls, adds, dels) = rows(&d, "", false);
        assert_eq!((adds, dels), (50, 1));
        assert_eq!(ls.len(), DIFF_CUT + 1);
        assert_eq!(line_text(ls.last().unwrap()), "… 11 more lines");
    }

    #[cfg(feature = "lang-python")]
    #[test]
    fn code_is_highlighted_by_extension() {
        let t = theme::get();
        let (ls, ..) = rows(" 1|def f():\n-2|    return 1\n+2|    return 2", "py", true);
        let kw = ls[0].iter().find(|s| s.text == "def").expect("keyword span");
        assert_eq!(kw.style.foreground_color, t.color(Role::SynKeyword));
        let ret = ls[2].iter().find(|s| s.text == "    return ").expect("plain code span");
        assert_eq!(ret.style.foreground_color, t.color(Role::Ok));
        assert!(ret.style.background_color.is_none());
    }
}
