use crossterm::style::{Attribute, Color, ContentStyle, StyledContent};
use unicode_width::UnicodeWidthChar;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Span {
    pub text: String,
    pub style: ContentStyle,
}

pub type Line = Vec<Span>;

pub fn span(text: impl Into<String>, style: ContentStyle) -> Span {
    Span { text: text.into(), style }
}

pub fn plain(text: impl Into<String>) -> Line {
    vec![span(text, ContentStyle::default())]
}

pub fn styled(text: impl Into<String>, f: impl FnOnce(ContentStyle) -> ContentStyle) -> Line {
    vec![span(text, f(ContentStyle::default()))]
}

pub fn dim() -> ContentStyle {
    let mut s = ContentStyle::default();
    s.attributes.set(Attribute::Dim);
    s
}

pub fn bold() -> ContentStyle {
    let mut s = ContentStyle::default();
    s.attributes.set(Attribute::Bold);
    s
}

pub fn line_width(l: &Line) -> usize {
    l.iter().map(|s| s.text.chars().map(|c| c.width().unwrap_or(0)).sum::<usize>()).sum()
}

pub fn line_text(l: &Line) -> String {
    l.iter().map(|s| s.text.as_str()).collect()
}

/// Hard-wraps at `width` columns, preserving span styles. An empty line yields one empty line.
pub fn wrap(l: &Line, width: usize) -> Vec<Line> {
    let width = width.max(1);
    let mut out: Vec<Line> = vec![vec![]];
    let mut col = 0;
    for s in l {
        let mut cur = String::new();
        for c in s.text.chars() {
            let w = c.width().unwrap_or(0);
            if col + w > width {
                if !cur.is_empty() {
                    out.last_mut().unwrap().push(span(std::mem::take(&mut cur), s.style));
                }
                out.push(vec![]);
                col = 0;
            }
            cur.push(c);
            col += w;
        }
        if !cur.is_empty() {
            out.last_mut().unwrap().push(span(cur, s.style));
        }
    }
    out
}

/// Cuts to `width` columns, ending with `…` when cut.
pub fn truncate(l: &Line, width: usize) -> Line {
    if line_width(l) <= width {
        return l.clone();
    }
    let mut out: Line = vec![];
    let mut col = 0;
    'outer: for s in l {
        let mut cur = String::new();
        for c in s.text.chars() {
            let w = c.width().unwrap_or(0);
            if col + w + 1 > width {
                if !cur.is_empty() {
                    out.push(span(cur, s.style));
                }
                out.push(span("…", s.style));
                break 'outer;
            }
            cur.push(c);
            col += w;
        }
        out.push(span(cur, s.style));
    }
    out
}

/// Background-only style; `None` is the terminal default.
pub fn bg(c: Option<Color>) -> ContentStyle {
    ContentStyle { background_color: c, ..Default::default() }
}

/// Pads `l` with spaces to `width` columns and gives every span without its own background `bg`.
/// Call after wrapping: a line wider than `width` is returned unpadded.
pub fn fill(l: &Line, width: usize, bg: Option<Color>) -> Line {
    let mut out: Line = l
        .iter()
        .map(|s| span(s.text.clone(), ContentStyle { background_color: s.style.background_color.or(bg), ..s.style }))
        .collect();
    let pad = width.saturating_sub(line_width(l));
    if pad > 0 {
        out.push(span(" ".repeat(pad), self::bg(bg)));
    }
    out
}

/// Renders a line as an ANSI string without a trailing newline.
pub fn to_ansi(l: &Line) -> String {
    let mut out = String::new();
    for s in l {
        if s.style == ContentStyle::default() {
            out.push_str(&s.text);
        } else {
            out.push_str(&StyledContent::new(s.style, &s.text).to_string());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_and_truncate_by_display_width() {
        let l = plain("ab日本cd");
        let w = wrap(&l, 4);
        assert_eq!(w.iter().map(line_text).collect::<Vec<_>>(), ["ab日", "本cd"]);
        assert_eq!(line_text(&truncate(&l, 5)), "ab日…");
        assert_eq!(wrap(&vec![], 4).len(), 1);
    }
}
