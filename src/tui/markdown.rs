//! Markdown to styled transcript lines. Tolerates incomplete input (streaming).

use std::ops::Range;

use crossterm::style::{Attribute, ContentStyle};
use pulldown_cmark::{Alignment, CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use unicode_width::UnicodeWidthChar;

use super::highlight;
use super::line::{Line, bold, line_text, sanitize, span, wrap};
use super::theme::{self, Role};

/// Role `r` as a foreground style.
fn key(r: Role) -> ContentStyle {
    theme::get().fg(r)
}

fn heading_style() -> ContentStyle {
    let mut s = key(Role::Accent);
    s.attributes.set(Attribute::Bold);
    s
}

/// A styled character; the working unit before lines are formed.
type Ch = (char, ContentStyle);

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Para,
    /// Bare inline run inside a tight list item.
    Tight,
    List,
    Other,
}

const OPTS: Options = Options::ENABLE_TABLES.union(Options::ENABLE_STRIKETHROUGH).union(Options::ENABLE_TASKLISTS);

/// Renders `md` wrapped to `width` columns. Never ends with a blank line.
pub fn render(md: &str, width: usize) -> Vec<Line> {
    let opts = OPTS;
    let ev: Vec<(Event, Range<usize>)> = Parser::new_ext(md, opts).into_offset_iter().collect();
    let mut r = R { ev, pos: 0, src: md };
    let w = width.max(1);
    let mut out: Vec<Vec<Ch>> = Vec::new();
    while r.pos < r.ev.len() {
        let ls = r.blocks(w);
        if !ls.is_empty() {
            if !out.is_empty() {
                out.push(Vec::new());
            }
            out.extend(ls);
        }
        r.pos += 1; // stray End event
    }
    out.iter().map(|c| chars_to_line(c)).collect()
}

/// `(lang, body)` of each fenced or indented code block, in order; `lang` is the first info-string word.
pub fn code_blocks(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut cur: Option<(String, String)> = None;
    for ev in Parser::new(text) {
        match ev {
            Event::Start(Tag::CodeBlock(kind)) => {
                let lang = match kind {
                    CodeBlockKind::Fenced(info) => info.split_whitespace().next().unwrap_or("").to_string(),
                    CodeBlockKind::Indented => String::new(),
                };
                cur = Some((lang, String::new()));
            }
            Event::Text(t) => {
                if let Some((_, body)) = cur.as_mut() {
                    body.push_str(&t);
                }
            }
            Event::End(TagEnd::CodeBlock) => out.extend(cur.take()),
            _ => {}
        }
    }
    out
}

/// `http(s)://` URLs in `text`, minus trailing punctuation. A URL ends at whitespace or a quote or
/// bracket; parentheses belong to it only when balanced (`…/Rust_(language)`, not `(see …)`). A URL
/// with no host is not a link.
pub fn links(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(at) = ["http://", "https://"].iter().filter_map(|p| text[from..].find(p).map(|i| from + i)).min() {
        let mut depth = 0usize;
        let mut end = text.len();
        for (i, c) in text[at..].char_indices() {
            let stop = match c {
                '(' => {
                    depth += 1;
                    false
                }
                ')' => match depth {
                    0 => true,
                    _ => {
                        depth -= 1;
                        false
                    }
                },
                c => c.is_whitespace() || "<>[]\"'`".contains(c),
            };
            if stop {
                end = at + i;
                break;
            }
        }
        let url = text[at..end].trim_end_matches(['.', ',', ';', ':', '!', '?']);
        from = end.max(at + 1);
        let rest = &url[url.find("://").map_or(0, |i| i + 3)..];
        let host = rest.split(['/', '?', '#', ':']).next().unwrap_or("");
        if !host.is_empty() {
            out.push(url.to_string());
        }
    }
    out
}

/// Byte offset up to which `buf` is complete and can be committed; 0 if none.
///
/// Parses `buf` and returns the start of the last top-level block: everything before it is made of
/// blocks that already have a successor, so later text cannot change them. A list or quote that is
/// still open is the last block, so it is never cut in the middle, and an open code fence is too.
pub fn stable_prefix(buf: &str) -> usize {
    let mut depth = 0usize;
    let mut starts: Vec<usize> = Vec::new();
    for (ev, range) in Parser::new_ext(buf, OPTS).into_offset_iter() {
        match ev {
            Event::Start(_) => {
                if depth == 0 {
                    starts.push(range.start);
                }
                depth += 1;
            }
            Event::End(_) => depth = depth.saturating_sub(1),
            _ if depth == 0 => starts.push(range.start),
            _ => {}
        }
    }
    match starts.as_slice() {
        [.., last] if starts.len() >= 2 => *last,
        _ => 0,
    }
}

/// The reference definitions (`[label]: url`) in `text`, as source lines to prepend when rendering
/// later text that is committed separately.
pub fn reference_defs(text: &str) -> String {
    let p = Parser::new_ext(text, OPTS);
    let mut spans: Vec<Range<usize>> = p.reference_definitions().iter().map(|(_, d)| d.span.clone()).collect();
    spans.sort_by_key(|r| r.start);
    let mut out = String::new();
    for r in spans {
        if let Some(t) = text.get(r) {
            out.push_str(t.trim_end());
            out.push('\n');
        }
    }
    out
}

fn is_inline(tag: &Tag) -> bool {
    matches!(tag, Tag::Emphasis | Tag::Strong | Tag::Strikethrough | Tag::Link { .. } | Tag::Image { .. })
}

struct R<'a> {
    ev: Vec<(Event<'a>, Range<usize>)>,
    pos: usize,
    src: &'a str,
}

impl<'a> R<'a> {
    fn cur(&self) -> Option<Event<'a>> {
        self.ev.get(self.pos).map(|(e, _)| e.clone())
    }

    /// Consumes the End event closing the current container, if present.
    fn close(&mut self) {
        if matches!(self.ev.get(self.pos), Some((Event::End(_), _))) {
            self.pos += 1;
        }
    }

    /// Block sequence up to a container End (not consumed) or end of input.
    fn blocks(&mut self, w: usize) -> Vec<Vec<Ch>> {
        let mut out = Vec::new();
        let mut prev: Option<Kind> = None;
        while let Some(ev) = self.cur() {
            if matches!(ev, Event::End(_)) {
                break;
            }
            let (lines, kind) = self.block(ev, w);
            if lines.is_empty() {
                continue;
            }
            if let Some(p) = prev {
                // Tight list items and nested lists stay adjacent.
                let no_sep = kind == Kind::List && matches!(p, Kind::Tight | Kind::List);
                if !no_sep {
                    out.push(Vec::new());
                }
            }
            out.extend(lines);
            prev = Some(kind);
        }
        out
    }

    fn block(&mut self, ev: Event<'a>, w: usize) -> (Vec<Vec<Ch>>, Kind) {
        let start = self.pos;
        match ev {
            Event::Start(Tag::Paragraph) => {
                self.pos += 1;
                let segs = self.inline(ContentStyle::default());
                self.close();
                (wrap_chs(&segs, w), Kind::Para)
            }
            Event::Start(Tag::Heading { level, .. }) => {
                self.pos += 1;
                let hs = heading_style();
                let mut prefix = key(Role::AccentSoft);
                prefix.attributes.set(Attribute::Bold);
                let mut segs = chs(&format!("{} ", "#".repeat(level as usize)), prefix);
                segs.extend(self.inline(hs));
                self.close();
                (wrap_chs(&segs, w), Kind::Para)
            }
            Event::Start(Tag::BlockQuote(_)) => {
                self.pos += 1;
                let inner = self.blocks(w.saturating_sub(2).max(1));
                self.close();
                let lines = inner
                    .into_iter()
                    .map(|l| {
                        let mut v = chs("│ ", key(Role::Line));
                        v.extend(l.into_iter().map(|(c, mut st)| {
                            if st.foreground_color.is_none() {
                                st = theme::get().apply(Role::Muted, st);
                            }
                            st.attributes.set(Attribute::Italic);
                            (c, st)
                        }));
                        v
                    })
                    .collect();
                (lines, Kind::Other)
            }
            Event::Start(Tag::CodeBlock(kind)) => {
                self.pos += 1;
                let lang = match kind {
                    CodeBlockKind::Fenced(info) => info.split_whitespace().next().unwrap_or("").to_string(),
                    CodeBlockKind::Indented => String::new(),
                };
                let mut body = String::new();
                loop {
                    match self.cur() {
                        Some(Event::Text(s)) => {
                            body.push_str(&s);
                            self.pos += 1;
                        }
                        Some(Event::End(_)) => {
                            self.pos += 1;
                            break;
                        }
                        Some(_) => self.pos += 1,
                        None => break,
                    }
                }
                (code_lines(&lang, &body, w), Kind::Other)
            }
            Event::Start(Tag::List(start)) => {
                self.pos += 1;
                self.list(start, w)
            }
            Event::Start(Tag::Table(aligns)) => {
                let range = self.ev[self.pos].1.clone();
                self.pos += 1;
                (self.table(aligns, range, w), Kind::Other)
            }
            Event::Rule => {
                self.pos += 1;
                (vec![chs(&"─".repeat(w.min(80)), key(Role::Line))], Kind::Other)
            }
            Event::Start(tag) if !is_inline(&tag) => {
                // Unknown container (html block, footnote, ...): render its children.
                self.pos += 1;
                let inner = self.blocks(w);
                self.close();
                (inner, Kind::Other)
            }
            Event::End(_) => {
                self.pos += 1;
                (Vec::new(), Kind::Other)
            }
            _ => {
                let segs = self.inline(ContentStyle::default());
                if self.pos == start {
                    self.pos += 1; // unknown inline event: skip to guarantee progress
                }
                (wrap_chs(&segs, w), Kind::Tight)
            }
        }
    }

    /// Inline run until the first non-inline event (not consumed).
    fn inline(&mut self, base: ContentStyle) -> Vec<Ch> {
        let mut out = Vec::new();
        let mut st = base;
        let mut stack: Vec<ContentStyle> = Vec::new();
        let mut links: Vec<(usize, String)> = Vec::new();
        while let Some(ev) = self.cur() {
            match ev {
                Event::Text(s) | Event::Html(s) | Event::InlineHtml(s) => push_str(&mut out, &s, st),
                Event::Code(s) => push_str(&mut out, &s, st),
                Event::SoftBreak => out.push((' ', st)),
                Event::HardBreak => out.push(('\n', st)),
                Event::TaskListMarker(done) => push_str(&mut out, if done { "☑ " } else { "☐ " }, st),
                Event::Start(Tag::Emphasis) => {
                    stack.push(st);
                    st.attributes.set(Attribute::Italic);
                }
                Event::Start(Tag::Strong) => {
                    stack.push(st);
                    st.attributes.set(Attribute::Bold);
                }
                Event::Start(Tag::Strikethrough) => {
                    stack.push(st);
                    st.attributes.set(Attribute::CrossedOut);
                }
                Event::Start(Tag::Link { dest_url, .. }) => {
                    stack.push(st);
                    st.attributes.set(Attribute::Underlined);
                    st.foreground_color = theme::get().color(Role::Accent);
                    links.push((out.len(), dest_url.to_string()));
                }
                Event::Start(Tag::Image { .. }) => stack.push(st),
                Event::End(TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough | TagEnd::Image) => {
                    st = stack.pop().unwrap_or(base);
                }
                Event::End(TagEnd::Link) => {
                    st = stack.pop().unwrap_or(base);
                    if let Some((start, url)) = links.pop() {
                        let text: String = out[start..].iter().map(|c| c.0).collect();
                        if !url.is_empty() && text != url {
                            push_str(&mut out, &format!(" ({url})"), key(Role::Faint));
                        }
                    }
                }
                _ => break,
            }
            self.pos += 1;
        }
        out
    }

    fn list(&mut self, start: Option<u64>, w: usize) -> (Vec<Vec<Ch>>, Kind) {
        let mut out: Vec<Vec<Ch>> = Vec::new();
        let mut n = start;
        while let Some(ev) = self.cur() {
            match ev {
                Event::Start(Tag::Item) => {
                    self.pos += 1;
                    let marker = match n {
                        Some(k) => {
                            n = Some(k + 1);
                            format!("{k}. ")
                        }
                        None => "• ".to_string(),
                    };
                    let mw = marker.chars().count();
                    let first_is_list = matches!(self.cur(), Some(Event::Start(Tag::List(_))));
                    let child = self.blocks(w.saturating_sub(mw).max(1));
                    self.close();
                    let pad = " ".repeat(mw);
                    if child.is_empty() {
                        out.push(chs(marker.trim_end(), key(Role::Accent)));
                    } else if first_is_list {
                        out.push(chs(marker.trim_end(), key(Role::Accent)));
                        for l in child {
                            let mut v = chs(&pad, Default::default());
                            v.extend(l);
                            out.push(v);
                        }
                    } else {
                        for (i, l) in child.into_iter().enumerate() {
                            let mut v =
                                if i == 0 { chs(&marker, key(Role::Accent)) } else { chs(&pad, Default::default()) };
                            v.extend(l);
                            out.push(v);
                        }
                    }
                }
                Event::End(TagEnd::List(_)) => {
                    self.pos += 1;
                    break;
                }
                _ => self.pos += 1,
            }
        }
        (out, Kind::List)
    }

    fn table(&mut self, aligns: Vec<Alignment>, range: Range<usize>, w: usize) -> Vec<Vec<Ch>> {
        let mut rows: Vec<(bool, Vec<Vec<Ch>>)> = Vec::new();
        let mut in_head = false;
        while let Some(ev) = self.cur() {
            match ev {
                Event::End(TagEnd::Table) => {
                    self.pos += 1;
                    break;
                }
                Event::Start(Tag::TableHead) => {
                    self.pos += 1;
                    in_head = true;
                    rows.push((true, Vec::new()));
                }
                Event::End(TagEnd::TableHead) => {
                    self.pos += 1;
                    in_head = false;
                }
                Event::Start(Tag::TableRow) => {
                    self.pos += 1;
                    rows.push((false, Vec::new()));
                }
                Event::Start(Tag::TableCell) => {
                    self.pos += 1;
                    let base = if in_head { bold() } else { ContentStyle::default() };
                    let mut c = self.inline(base);
                    for ch in &mut c {
                        if ch.0 == '\n' {
                            ch.0 = ' ';
                        }
                    }
                    self.close();
                    if rows.is_empty() {
                        rows.push((false, Vec::new()));
                    }
                    rows.last_mut().unwrap().1.push(c);
                }
                _ => self.pos += 1,
            }
        }
        let ncols = rows.iter().map(|r| r.1.len()).max().unwrap_or(0).max(aligns.len());
        if ncols == 0 {
            return Vec::new();
        }
        let mut colw = vec![1usize; ncols];
        for (_, cells) in &rows {
            for (i, c) in cells.iter().enumerate() {
                colw[i] = colw[i].max(text_width(c));
            }
        }
        let total: usize = colw.iter().map(|c| c + 3).sum::<usize>() + 1;
        if total > w {
            // Too wide: show the source markdown instead.
            return self.src[range].trim_end().lines().flat_map(|l| wrap_chs(&chs(l, Default::default()), w)).collect();
        }

        let bdr = key(Role::Line);
        let border = |l: char, m: char, r: char| -> Vec<Ch> {
            let mut v = vec![(l, bdr)];
            for (i, cw) in colw.iter().enumerate() {
                v.extend(std::iter::repeat_n(('─', bdr), cw + 2));
                v.push((if i + 1 < ncols { m } else { r }, bdr));
            }
            v
        };
        let mut out = vec![border('┌', '┬', '┐')];
        let n_rows = rows.len();
        for (ri, (is_head, cells)) in rows.iter().enumerate() {
            let mut v = vec![('│', bdr)];
            for (i, &cw) in colw.iter().enumerate().take(ncols) {
                let cell: &[Ch] = cells.get(i).map(Vec::as_slice).unwrap_or(&[]);
                let pad = cw - text_width(cell);
                let (l, r) = match aligns.get(i).copied().unwrap_or(Alignment::None) {
                    Alignment::Right => (pad, 0),
                    Alignment::Center => (pad / 2, pad - pad / 2),
                    _ => (0, pad),
                };
                v.push((' ', Default::default()));
                v.extend(std::iter::repeat_n((' ', Default::default()), l));
                v.extend_from_slice(cell);
                v.extend(std::iter::repeat_n((' ', Default::default()), r));
                v.push((' ', Default::default()));
                v.push(('│', bdr));
            }
            out.push(v);
            if ri == 0 && *is_head && n_rows > 1 {
                out.push(border('├', '┼', '┤'));
            }
        }
        out.push(border('└', '┴', '┘'));
        out
    }
}

/// Fenced code as omp shows it: fence lines in the border colour, body indented 2 columns.
/// The body is syntax-highlighted when the language is known, else the default style.
fn code_lines(lang: &str, body: &str, w: usize) -> Vec<Vec<Ch>> {
    let mut open = chs("```", key(Role::Line));
    open.extend(chs(lang, key(Role::Faint)));
    let mut out = vec![open];
    let body = sanitize(body);
    let src: Vec<Line> = highlight::highlight(&body, lang)
        .unwrap_or_else(|| body.lines().map(|l| vec![span(l, ContentStyle::default())]).collect());
    for line in &src {
        if line_text(line).is_empty() {
            out.push(Vec::new());
            continue;
        }
        for part in wrap(line, w.saturating_sub(2).max(1)) {
            let mut p = chs("  ", ContentStyle::default());
            p.extend(part.iter().flat_map(|s| s.text.chars().map(move |c| (c, s.style))));
            out.push(p);
        }
    }
    out.push(chs("```", key(Role::Line)));
    out
}

/// Word-wraps at spaces, hard-splitting words wider than `width`. `\n` forces a break.
pub(super) fn wrap_chs(chs: &[Ch], width: usize) -> Vec<Vec<Ch>> {
    let width = width.max(1);
    let mut out: Vec<Vec<Ch>> = Vec::new();
    let mut cur: Vec<Ch> = Vec::new();
    let mut col = 0;
    let mut pending: Vec<Ch> = Vec::new();
    let mut i = 0;
    while i < chs.len() {
        let c = chs[i].0;
        if c == '\n' {
            out.push(std::mem::take(&mut cur));
            col = 0;
            pending.clear();
            i += 1;
            continue;
        }
        if c == ' ' {
            if col > 0 {
                pending.push(chs[i]);
            }
            i += 1;
            continue;
        }
        let start = i;
        while i < chs.len() && chs[i].0 != ' ' && chs[i].0 != '\n' {
            i += 1;
        }
        let word = &chs[start..i];
        let ww = text_width(word);
        let pw = pending.len();
        if col + pw + ww <= width {
            cur.append(&mut pending);
            cur.extend_from_slice(word);
            col += pw + ww;
        } else {
            pending.clear();
            if col > 0 {
                out.push(std::mem::take(&mut cur));
                col = 0;
            }
            if ww <= width {
                cur.extend_from_slice(word);
                col = ww;
            } else {
                for &ch in word {
                    let cw = char_w(ch.0);
                    if col + cw > width && col > 0 {
                        out.push(std::mem::take(&mut cur));
                        col = 0;
                    }
                    cur.push(ch);
                    col += cw;
                }
            }
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn char_w(c: char) -> usize {
    UnicodeWidthChar::width(c).unwrap_or(0)
}

fn text_width(chs: &[Ch]) -> usize {
    chs.iter().map(|c| char_w(c.0)).sum()
}

pub(super) fn chs(s: &str, st: ContentStyle) -> Vec<Ch> {
    sanitize(s).chars().map(|c| (c, st)).collect()
}

fn push_str(out: &mut Vec<Ch>, s: &str, st: ContentStyle) {
    out.extend(sanitize(s).chars().map(|c| (c, st)));
}

pub(super) fn chars_to_line(cs: &[Ch]) -> Line {
    let mut out: Line = Vec::new();
    for &(c, st) in cs {
        match out.last_mut() {
            Some(s) if s.style == st => s.text.push(c),
            _ => out.push(span(c.to_string(), st)),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::line::{line_text, line_width};
    use super::*;

    fn texts(md: &str, w: usize) -> Vec<String> {
        render(md, w).iter().map(line_text).collect()
    }

    #[test]
    fn heading_is_bold_with_hashes() {
        let ls = render("## Title", 40);
        assert_eq!(line_text(&ls[0]), "## Title");
        assert!(ls[0].iter().all(|s| s.style.attributes.has(Attribute::Bold)));
        assert_eq!(ls[0][0].style.foreground_color, theme::get().color(Role::AccentSoft));
        assert_eq!(ls[0][1].style.foreground_color, theme::get().color(Role::Accent));
    }

    #[test]
    fn paragraphs_are_separated_by_one_blank_line() {
        assert_eq!(texts("a\n\nb\n\n", 40), vec!["a", "", "b"]);
    }

    #[test]
    fn soft_and_hard_breaks() {
        assert_eq!(texts("a\nb", 40), vec!["a b"]);
        assert_eq!(texts("a  \nb", 40), vec!["a", "b"]);
    }

    #[test]
    fn word_wrap_at_spaces_and_hard_split() {
        assert_eq!(texts("aaa bbb ccc", 7), vec!["aaa bbb", "ccc"]);
        assert_eq!(texts("abcdefgh", 3), vec!["abc", "def", "gh"]);
    }

    #[test]
    fn lists_nest_with_hanging_indent() {
        assert_eq!(texts("- a\n- b\n  - c", 40), vec!["• a", "• b", "  • c"]);
        assert_eq!(texts("1. x\n2. y", 40), vec!["1. x", "2. y"]);
        assert_eq!(texts("- aaa bbb ccc", 7), vec!["• aaa", "  bbb", "  ccc"]);
    }

    #[test]
    fn blockquote_prefix() {
        assert_eq!(texts("> q", 40), vec!["│ q"]);
    }

    #[test]
    fn code_fence_is_raw_and_indented() {
        assert_eq!(texts("```rust\nfn main() {}\n```", 40), vec!["```rust", "  fn main() {}", "```"]);
        assert_eq!(texts("```\n*x*\n", 40), vec!["```", "  *x*", "```"]);
    }

    #[test]
    fn code_lines_hard_wrap() {
        assert_eq!(texts("```\nabcdef\n```", 5), vec!["```", "  abc", "  def", "```"]);
    }

    #[test]
    fn table_aligned() {
        assert_eq!(
            texts("| a | b |\n|---|---|\n| 1 | 2 |", 40),
            vec!["┌───┬───┐", "│ a │ b │", "├───┼───┤", "│ 1 │ 2 │", "└───┴───┘"]
        );
    }

    #[test]
    fn narrow_table_falls_back_to_source() {
        let ls = texts("| a | b |\n|---|---|\n| 1 | 2 |", 5);
        assert!(ls.iter().all(|l| l.chars().count() <= 5));
        let joined: String = ls.concat().chars().filter(|c| *c != ' ').collect();
        assert!(joined.contains("|a|b|"), "{joined}");
        assert!(!ls.iter().any(|l| l.contains('┌')));
    }

    #[test]
    fn rule_and_links() {
        assert_eq!(texts("---", 20), vec!["─".repeat(20)]);
        assert_eq!(texts("[x](http://u)", 40), vec!["x (http://u)"]);
        assert_eq!(texts("[http://u](http://u)", 40), vec!["http://u"]);
    }

    #[test]
    fn inline_code_has_no_hue() {
        let ls = render("use `x`", 40);
        assert_eq!(ls[0].last().unwrap().style.foreground_color, None);
    }

    #[test]
    fn highlighted_fence_wraps_within_width() {
        let md = "```rust\nlet s = \"a very long string literal that runs past the block width\";\n```";
        for w in [10, 20, 40] {
            for l in render(md, w) {
                assert!(line_width(&l) <= w, "w={w} line={:?}", line_text(&l));
            }
        }
    }

    #[test]
    #[cfg(feature = "lang-rust")]
    fn fence_body_uses_syntax_colours() {
        let ls = render("```rust\nfn f() {}\n```", 40);
        assert_eq!(line_text(&ls[1]), "  fn f() {}");
        assert!(ls[1].iter().any(|s| s.style.foreground_color == theme::get().color(Role::SynKeyword)));
    }

    #[test]
    fn unclosed_fence_renders_as_code_so_far() {
        assert_eq!(texts("```\nab", 40), vec!["```", "  ab", "```"]);
    }

    #[test]
    fn widths_never_exceed_width() {
        let md = "# 标题很长的中文标题用于测试换行和宽度计算是否正确\n\n\
            这是一个很长的段落 with some english words 中英混合 文字 wrapping test.\n\n\
            - 列表项一个很长很长的中文内容需要换行处理\n  - 嵌套 nested item with words\n\n\
            > 引用块里的中文内容也要正确换行 quote text\n\n\
            ```\n代码行很长很长很长很长很长很长很长很长很长很长\n```\n\n\
            | 列 | b |\n|---|---|\n| 中文单元格 | x |\n\n\
            | a long header | another long header | third column header |\n|---|---|---|\n| cell | cell | cell |\n\n\
            ---\n\n1. ordered 中文 item\n2. second\n";
        for w in [8, 10, 20, 40] {
            for l in render(md, w) {
                assert!(line_width(&l) <= w, "w={w} line={:?}", line_text(&l));
            }
        }
    }

    #[test]
    fn no_trailing_blank_line() {
        let ls = render("p\n\n", 40);
        assert_eq!(ls.len(), 1);
    }

    #[test]
    fn stable_prefix_open_fence() {
        let b = "p1\n\n```rust\nlet x;\n\nmore";
        assert_eq!(stable_prefix(b), "p1\n\n".len());
    }

    #[test]
    fn stable_prefix_two_paragraphs() {
        let b = "a\n\nb\n\nc";
        assert_eq!(stable_prefix(b), "a\n\nb\n\n".len());
    }

    #[test]
    fn a_list_is_never_cut_in_the_middle() {
        let b = "intro\n\n- a\n  - nested\n\n  more of a\n- b\n\n";
        let n = stable_prefix(b);
        assert_eq!(&b[..n], "intro\n\n", "the open list is the last block");
        let b = "- a\n- b\n\nafter";
        assert_eq!(&b[..stable_prefix(b)], "- a\n- b\n\n");
        let q = "> quote\n> more\n\ntail";
        assert_eq!(&q[..stable_prefix(q)], "> quote\n> more\n\n");
    }

    #[test]
    fn reference_definitions_survive_commits() {
        let first = "see [docs] here\n\n[docs]: https://example.com/d\n\nnext";
        let cut = stable_prefix(first);
        let refs = reference_defs(&first[..cut]);
        assert!(refs.contains("[docs]: https://example.com/d"));
        // A later chunk that uses the label still resolves it.
        let later = render(&format!("{refs}\nuse [docs] again"), 80);
        let text: String = later.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(text.contains("https://example.com/d"), "{text}");
    }

    #[test]
    fn task_lists_render_boxes() {
        let text: String = render("- [x] done\n- [ ] todo", 40).iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(text.contains("☑ done") && text.contains("☐ todo"), "{text}");
    }

    #[test]
    fn stable_prefix_none() {
        assert_eq!(stable_prefix("abc"), 0);
        assert_eq!(stable_prefix("abc\n"), 0);
        assert_eq!(stable_prefix("```\nx\n\n"), 0);
    }

    #[test]
    fn stable_prefix_after_closed_fence() {
        let b = "```\nx\n```\n\nafter";
        assert_eq!(stable_prefix(b), "```\nx\n```\n\n".len());
    }

    #[test]
    fn code_blocks_and_links_are_extracted() {
        let md = "text\n\n```rust\nfn a() {}\n```\n\n    indented\n\nsee (https://x.io/a?b=1). and <http://y.z>, `https://q.r;`";
        assert_eq!(
            code_blocks(md),
            [("rust".to_string(), "fn a() {}\n".to_string()), (String::new(), "indented\n".to_string())]
        );
        assert_eq!(links(md), ["https://x.io/a?b=1", "http://y.z", "https://q.r"]);
    }

    #[test]
    fn links_keep_balanced_parentheses_and_need_a_host() {
        assert_eq!(
            links("see https://en.wikipedia.org/wiki/Rust_(programming_language)."),
            ["https://en.wikipedia.org/wiki/Rust_(programming_language)"]
        );
        assert_eq!(links("(see https://x.io/a)"), ["https://x.io/a"]);
        assert!(links("http:// and https:///path and https://").is_empty());
        assert_eq!(
            links("a https://a.b/c, b http://d.e:8080/f?x=1&y=2;"),
            ["https://a.b/c", "http://d.e:8080/f?x=1&y=2"]
        );
    }

    #[test]
    fn list_markers_use_accent() {
        assert_eq!(render("- a\n- b", 20)[0][0].style.foreground_color, theme::get().color(Role::Accent));
    }
}
