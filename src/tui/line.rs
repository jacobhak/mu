use crossterm::style::{Attribute, Color, ContentStyle, StyledContent};
use std::borrow::Cow;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthChar;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Span {
    pub text: String,
    pub style: ContentStyle,
}

pub type Line = Vec<Span>;

/// Makes text safe to print: C0/C1 controls other than `\n` become a visible glyph (`\x1b` → `␛`),
/// and tabs expand to the next 4-column stop. Nothing the terminal could act on gets through.
pub fn sanitize(s: &str) -> Cow<'_, str> {
    if !s.chars().any(|c| c != '\n' && (c.is_control())) {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len() + 8);
    let mut col = 0;
    for c in s.chars() {
        match c {
            '\n' => {
                out.push('\n');
                col = 0;
            }
            '\t' => {
                let n = 4 - col % 4;
                out.extend(std::iter::repeat_n(' ', n));
                col += n;
            }
            '\u{7f}' => {
                out.push('\u{2421}');
                col += 1;
            }
            c if (c as u32) < 0x20 => {
                out.push(char::from_u32(0x2400 + c as u32).unwrap_or('\u{fffd}'));
                col += 1;
            }
            c if c.is_control() => {
                out.push('\u{fffd}');
                col += 1;
            }
            c => {
                out.push(c);
                col += c.width().unwrap_or(0);
            }
        }
    }
    Cow::Owned(out)
}

pub fn span(text: impl Into<String>, style: ContentStyle) -> Span {
    let text: String = text.into();
    let text = match sanitize(&text) {
        Cow::Borrowed(_) => text,
        Cow::Owned(clean) => clean,
    };
    Span { text, style }
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

/// Display width of one grapheme cluster (a combining sequence or emoji counts as one cell run).
pub fn grapheme_width(g: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(g)
}

pub fn line_width(l: &Line) -> usize {
    l.iter().map(|s| s.text.graphemes(true).map(grapheme_width).sum::<usize>()).sum()
}

pub fn line_text(l: &Line) -> String {
    l.iter().map(|s| s.text.as_str()).collect()
}

/// One grapheme cluster of the source line, placed by `layout`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cell {
    /// Wrapped row and the column where the cluster starts.
    pub row: usize,
    pub col: usize,
    pub width: usize,
    /// Char offset of the cluster's first char in the source text.
    pub src: usize,
    /// Display column of the cluster's start in the *unwrapped* line.
    pub disp: usize,
}

/// A line hard-wrapped at a width, with where every cluster landed. The single source of wrap
/// geometry: drawing, cursor placement and the composer all read it.
#[derive(Clone, Debug)]
pub struct Layout {
    pub rows: Vec<Line>,
    pub cells: Vec<Cell>,
    /// Row and column just after the last cluster.
    pub end: (usize, usize),
    /// Number of chars in the source text.
    pub chars: usize,
}

impl Layout {
    /// Row and column of the cluster containing char offset `idx`; the end position for `idx` past the text.
    pub fn locate_char(&self, idx: usize) -> (usize, usize) {
        match self.cells.iter().rev().find(|c| c.src <= idx) {
            Some(c) if idx < self.chars => (c.row, c.col),
            _ => self.end,
        }
    }

    /// Where the cursor sits after the clusters that start before display column `target` of the
    /// unwrapped line.
    pub fn locate_col(&self, target: usize) -> (usize, usize) {
        match self.cells.iter().take_while(|c| c.disp < target).last() {
            Some(c) => (c.row, c.col + c.width),
            None => (0, 0),
        }
    }
}

/// Hard-wraps `l` at `width` columns by grapheme cluster, preserving span styles. A cluster wider
/// than `width` sits alone on its row. An empty line yields one empty row.
pub fn layout(l: &Line, width: usize) -> Layout {
    let width = width.max(1);
    let mut rows: Vec<Line> = vec![vec![]];
    let mut cells = Vec::new();
    let (mut row, mut col, mut disp, mut src) = (0, 0, 0, 0);
    for s in l {
        let mut cur = String::new();
        for g in s.text.graphemes(true) {
            let w = grapheme_width(g);
            if col > 0 && col + w > width {
                if !cur.is_empty() {
                    rows[row].push(span(std::mem::take(&mut cur), s.style));
                }
                rows.push(vec![]);
                row += 1;
                col = 0;
            }
            cells.push(Cell { row, col, width: w, src, disp });
            cur.push_str(g);
            col += w;
            disp += w;
            src += g.chars().count();
        }
        if !cur.is_empty() {
            rows[row].push(span(cur, s.style));
        }
    }
    Layout { rows, cells, end: (row, col), chars: src }
}

/// Hard-wraps at `width` columns, preserving span styles. An empty line yields one empty line.
pub fn wrap(l: &Line, width: usize) -> Vec<Line> {
    layout(l, width).rows
}

/// Cuts to `width` columns by grapheme cluster, ending with `…` when cut.
pub fn truncate(l: &Line, width: usize) -> Line {
    if line_width(l) <= width {
        return l.clone();
    }
    let mut out: Line = vec![];
    let mut col = 0;
    'outer: for s in l {
        let mut cur = String::new();
        for g in s.text.graphemes(true) {
            let w = grapheme_width(g);
            if col + w + 1 > width {
                if !cur.is_empty() {
                    out.push(span(cur, s.style));
                }
                out.push(span("…", s.style));
                break 'outer;
            }
            cur.push_str(g);
            col += w;
        }
        out.push(span(cur, s.style));
    }
    out
}

/// `s` cut to `width` columns, ending with `…` when cut.
pub fn truncate_text(s: &str, width: usize) -> String {
    line_text(&truncate(&plain(s), width))
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

    #[test]
    fn controls_become_glyphs_and_tabs_have_width() {
        let l = plain("a\x1b]52;c;aGk=\x07b\x1b[2Jc\u{9b}d\te");
        let t = line_text(&l);
        assert!(!t.chars().any(|c| c.is_control()), "{t:?}");
        assert!(t.starts_with("a\u{241b}]52"));
        assert_eq!(line_width(&l), t.chars().count());
        assert_eq!(sanitize("ab\tc"), "ab  c");
        assert_eq!(sanitize("\tx"), "    x");
        assert_eq!(sanitize("l1\nl2"), "l1\nl2");
    }
}
