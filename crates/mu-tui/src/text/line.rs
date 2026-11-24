use crossterm::style::{Attribute, Color, ContentStyle, StyledContent};
use mu_base::text::{grapheme_width, sanitize};
use std::borrow::Cow;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use unicode_segmentation::UnicodeSegmentation;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Span {
    pub text: String,
    pub style: ContentStyle,
    /// Where the text links to (an OSC 8 hyperlink). Never inline escape bytes in `text`: only a
    /// `link_ok` URL goes here, and `to_ansi` writes the escapes.
    pub link: Option<Arc<str>>,
}

pub type Line = Vec<Span>;

pub fn span(text: impl Into<String>, style: ContentStyle) -> Span {
    let text: String = text.into();
    let text = match sanitize(&text) {
        Cow::Borrowed(_) => text,
        Cow::Owned(clean) => clean,
    };
    Span { text, style, link: None }
}

/// A span with `s`'s style and link and other text.
fn like(s: &Span, text: impl Into<String>) -> Span {
    Span { link: s.link.clone(), ..span(text, s.style) }
}

/// Longest URL written as a hyperlink.
const LINK_MAX: usize = 2048;

/// Whether `url` is safe to put in an OSC 8 escape: an http(s) URL with no control character and no space.
pub fn link_ok(url: &str) -> bool {
    (url.starts_with("http://") || url.starts_with("https://"))
        && url.len() <= LINK_MAX
        && !url.chars().any(|c| c.is_control() || c == ' ')
}

/// A span's link, when it is one that can be written.
pub fn link(url: &str) -> Option<Arc<str>> {
    link_ok(url).then(|| Arc::from(url))
}

static HYPERLINKS: AtomicBool = AtomicBool::new(true);

/// `[display] hyperlinks`: whether `to_ansi` writes OSC 8 links.
pub fn set_hyperlinks(on: bool) {
    HYPERLINKS.store(on, Ordering::Relaxed);
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
                    rows[row].push(like(s, std::mem::take(&mut cur)));
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
            rows[row].push(like(s, cur));
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
                    out.push(like(s, cur));
                }
                out.push(like(s, "…"));
                break 'outer;
            }
            cur.push_str(g);
            col += w;
        }
        out.push(like(s, cur));
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
        .map(|s| {
            let style = ContentStyle { background_color: s.style.background_color.or(bg), ..s.style };
            Span { link: s.link.clone(), ..span(s.text.clone(), style) }
        })
        .collect();
    let pad = width.saturating_sub(line_width(l));
    if pad > 0 {
        out.push(span(" ".repeat(pad), self::bg(bg)));
    }
    out
}

/// Renders a line as an ANSI string without a trailing newline.
pub fn to_ansi(l: &Line) -> String {
    to_ansi_with(l, HYPERLINKS.load(Ordering::Relaxed))
}

/// `to_ansi`, with OSC 8 hyperlinks written or left out. Every call closes its own links, so a row
/// that ends inside a link does not carry it to the next.
pub fn to_ansi_with(l: &Line, hyperlinks: bool) -> String {
    let mut out = String::new();
    let mut open: Option<&str> = None;
    for s in l {
        let want = s.link.as_deref().filter(|u| hyperlinks && link_ok(u));
        if want != open {
            if open.is_some() {
                out.push_str("\x1b]8;;\x1b\\");
            }
            if let Some(url) = want {
                out.push_str("\x1b]8;;");
                out.push_str(url);
                out.push_str("\x1b\\");
            }
            open = want;
        }
        if s.style == ContentStyle::default() {
            out.push_str(&s.text);
        } else {
            out.push_str(&StyledContent::new(s.style, &s.text).to_string());
        }
    }
    if open.is_some() {
        out.push_str("\x1b]8;;\x1b\\");
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

    fn linked(text: &str, url: &str) -> Span {
        Span { link: link(url), ..span(text, ContentStyle::default()) }
    }

    #[test]
    fn a_linked_span_is_wrapped_in_osc_8() {
        let l = vec![
            span("see ", ContentStyle::default()),
            linked("here", "https://a.test/x"),
            span(" now", ContentStyle::default()),
        ];
        assert_eq!(to_ansi_with(&l, true), "see \x1b]8;;https://a.test/x\x1b\\here\x1b]8;;\x1b\\ now");
        assert_eq!(to_ansi_with(&l, false), "see here now");
    }

    #[test]
    fn a_link_that_wraps_closes_and_reopens_on_each_row() {
        let rows = wrap(&vec![linked("abcdef", "https://a.test/x")], 3);
        assert_eq!(rows.len(), 2);
        for (row, text) in rows.iter().zip(["abc", "def"]) {
            assert_eq!(to_ansi_with(row, true), format!("\x1b]8;;https://a.test/x\x1b\\{text}\x1b]8;;\x1b\\"));
        }
        let cut = truncate(&vec![linked("abcdef", "https://a.test/x")], 4);
        assert!(cut.iter().all(|s| s.link.is_some()), "{cut:?}");
        assert!(fill(&rows[0], 5, None).iter().filter(|s| s.link.is_some()).count() == 1);
    }

    #[test]
    fn adjacent_links_to_different_places_stay_apart() {
        let l = vec![linked("a", "https://a.test"), linked("b", "https://b.test")];
        assert_eq!(
            to_ansi_with(&l, true),
            "\x1b]8;;https://a.test\x1b\\a\x1b]8;;\x1b\\\x1b]8;;https://b.test\x1b\\b\x1b]8;;\x1b\\"
        );
    }

    #[test]
    fn only_plain_http_urls_become_links() {
        assert!(link_ok("https://a.test/x?y=1#z"));
        assert!(link_ok("http://a.test"));
        for bad in [
            "ftp://a.test",
            "file:///etc/passwd",
            "javascript:alert(1)",
            "https://a.test/\x1b]8;;x",
            "https://a b",
            "https://a.test/\x07",
            "https://a.test/\u{9c}",
            "https://a.test/\u{7f}",
        ] {
            assert!(!link_ok(bad), "{bad:?}");
        }
        assert!(!link_ok(&format!("https://a.test/{}", "a".repeat(LINK_MAX))));
        assert!(link("https://a b").is_none());
        // A span that carries a bad URL anyway is written without a link.
        let l = vec![Span { link: Some(Arc::from("https://a.test/\x1b[2J")), ..span("x", ContentStyle::default()) }];
        assert_eq!(to_ansi_with(&l, true), "x");
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
