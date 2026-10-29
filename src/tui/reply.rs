//! The reply picker (Ctrl+R, `/reply`): choose sentences, list items, table rows, code lines or
//! word ranges of an assistant reply and quote them into the composer as markdown blockquotes.
//!
//! A reply becomes a list of units. Every word of every unit has one global index, and a marked
//! or selected span is a half-open range of those indices, so a selection can start and end
//! anywhere, including inside a sentence, and merging adjacent marks is plain interval merging.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::style::{Attribute, ContentStyle};
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::line::{self, Line, dim, span};
use super::theme::{self, Role};

/// Columns the marker gutter takes at the left of every body row.
const GUTTER: usize = 2;
/// List items longer than this (bytes) are split into sentences too.
const LONG_ITEM: usize = 120;
/// Words that end in a period without ending a sentence (compared lowercased).
const ABBREVIATIONS: &[&str] =
    &["vs", "etc", "mr", "mrs", "ms", "dr", "prof", "sr", "jr", "st", "no", "cf", "al", "approx", "fig", "vol"];
/// Separates the cells of a table row in the unit text (and the quoted text).
const CELL_SEP: &str = "·";
const OPTS: Options = Options::ENABLE_TABLES.union(Options::ENABLE_STRIKETHROUGH).union(Options::ENABLE_TASKLISTS);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Para,
    Item,
    Heading,
    Row,
    Code,
}

/// One logical piece of a reply: a sentence, a list item, a heading, a table row or a code line.
#[derive(Debug, PartialEq)]
pub struct Unit {
    /// Plain projection: no markup, inline code keeps its backticks, whitespace is single spaces
    /// (code lines keep their indentation).
    pub text: String,
    pub kind: Kind,
    /// Prefix of the first display row (bullet, number, indentation) and of the others.
    pub lead: String,
    pub cont: String,
    /// Units with the same block are one paragraph or item: they join with a space when quoted.
    block: usize,
    /// Byte ranges of the words in `text`.
    words: Vec<(usize, usize)>,
}

/// Byte ranges of the whitespace-separated runs of `s`.
fn runs(s: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut start = None;
    for (i, c) in s.char_indices() {
        match (c.is_whitespace(), start) {
            (true, Some(a)) => {
                out.push((a, i));
                start = None;
            }
            (false, None) => start = Some(i),
            _ => {}
        }
    }
    if let Some(a) = start {
        out.push((a, s.len()));
    }
    out
}

fn normalise(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

impl Unit {
    fn new(text: String, kind: Kind, lead: String, cont: String, block: usize) -> Unit {
        let words = runs(&text).into_iter().filter(|&(a, b)| kind != Kind::Row || &text[a..b] != CELL_SEP).collect();
        Unit { text, kind, lead, cont, block, words }
    }
}

fn starts_sentence(c: char) -> bool {
    c.is_uppercase() || c.is_ascii_digit() || matches!(c, '"' | '\'' | '“' | '‘' | '`')
}

/// Whether the word before a terminator lets a sentence end there. Dotted words (`e.g`, `v1.2`,
/// `Cargo.toml`), paths and URLs never end a sentence; a lone initial or a list number does not
/// either.
fn may_end(word: &str, single_dot: bool) -> bool {
    if word.is_empty() || word.contains(['.', '/', '\\', '@']) {
        return false;
    }
    let lower = word.to_lowercase();
    if lower.starts_with("www") {
        return false;
    }
    if single_dot {
        if ABBREVIATIONS.contains(&lower.as_str()) {
            return false;
        }
        if word.chars().count() == 1 && word.chars().all(char::is_alphabetic) {
            return false;
        }
        if word.len() <= 2 && word.chars().all(|c| c.is_ascii_digit()) {
            return false;
        }
    }
    true
}

/// Byte offsets where a sentence after the first one starts in `s` (whitespace already normalised).
/// A terminator (`.` `!` `?`, optionally followed by closing brackets or quotes) splits only when
/// whitespace and then an uppercase letter, digit, quote or backtick follow, and it is not inside
/// a code span.
fn sentence_starts(s: &str) -> Vec<usize> {
    let cs: Vec<(usize, char)> = s.char_indices().collect();
    let mut starts = Vec::new();
    let mut code = false;
    let mut i = 0;
    while i < cs.len() {
        let c = cs[i].1;
        if c == '`' {
            code = !code;
            i += 1;
            continue;
        }
        if code || !matches!(c, '.' | '!' | '?') {
            i += 1;
            continue;
        }
        let first = i;
        let mut j = i;
        while j + 1 < cs.len() && matches!(cs[j + 1].1, '.' | '!' | '?') {
            j += 1;
        }
        i = j + 1;
        let mut k = i;
        while k < cs.len() && matches!(cs[k].1, ')' | ']' | '"' | '\'' | '”' | '’') {
            k += 1;
        }
        if k >= cs.len() || !cs[k].1.is_whitespace() {
            continue;
        }
        let mut m = k;
        while m < cs.len() && cs[m].1.is_whitespace() {
            m += 1;
        }
        if m >= cs.len() || !starts_sentence(cs[m].1) {
            continue;
        }
        let before = &s[..cs[first].0];
        let word = before.rsplit(char::is_whitespace).next().unwrap_or("");
        let word = word.trim_start_matches(|c: char| !c.is_alphanumeric());
        if may_end(word, c == '.' && j == first) {
            starts.push(cs[m].0);
            i = m;
        }
    }
    starts
}

/// The sentences of a whitespace-normalised paragraph.
pub fn split_sentences(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut prev = 0;
    for st in sentence_starts(s) {
        out.push(s[prev..st].trim().to_string());
        prev = st;
    }
    out.push(s[prev..].trim().to_string());
    out.retain(|x| !x.is_empty());
    out
}

/// Turns the raw markdown of one reply into its units, in document order.
pub fn parse(md: &str) -> Vec<Unit> {
    let mut b = Builder::default();
    for ev in Parser::new_ext(md, OPTS) {
        b.event(ev);
    }
    b.finish()
}

/// A block still being collected. Inline text goes here until the block closes.
struct Pend {
    kind: Kind,
    lead: String,
    cont: String,
    text: String,
    cells: Vec<String>,
}

#[derive(Default)]
struct Builder {
    units: Vec<Unit>,
    cur: Option<Pend>,
    /// Continuation prefix of each open list item, innermost last.
    items: Vec<String>,
    /// One entry per open list: its next number, or `None` for a bullet list.
    lists: Vec<Option<u64>>,
    block: usize,
    /// Text of the open code block.
    code: Option<String>,
}

impl Builder {
    fn base(&self) -> String {
        self.items.last().cloned().unwrap_or_default()
    }

    fn open(&mut self, kind: Kind, lead: String, cont: String) {
        self.flush();
        self.cur = Some(Pend { kind, lead, cont, text: String::new(), cells: Vec::new() });
    }

    fn flush(&mut self) {
        let Some(p) = self.cur.take() else { return };
        let text = match p.kind {
            Kind::Row => {
                p.cells.iter().filter(|c| !c.is_empty()).cloned().collect::<Vec<_>>().join(&format!(" {CELL_SEP} "))
            }
            _ => normalise(&p.text),
        };
        if text.is_empty() {
            return;
        }
        self.block += 1;
        let pieces = match p.kind {
            Kind::Para => split_sentences(&text),
            Kind::Item if text.len() > LONG_ITEM => split_sentences(&text),
            _ => vec![text],
        };
        for (i, t) in pieces.into_iter().enumerate() {
            let lead = if i == 0 { p.lead.clone() } else { p.cont.clone() };
            self.units.push(Unit::new(t, p.kind, lead, p.cont.clone(), self.block));
        }
    }

    fn event(&mut self, ev: Event<'_>) {
        match ev {
            Event::Start(tag) => self.start(tag),
            Event::End(end) => self.end(end),
            Event::Text(t) | Event::Html(t) | Event::InlineHtml(t) | Event::FootnoteReference(t) => self.text(&t),
            Event::Code(t) => self.text(&format!("`{t}`")),
            Event::SoftBreak | Event::HardBreak => self.text(" "),
            Event::TaskListMarker(done) => {
                let parent = self.items.len().checked_sub(2).map(|i| self.items[i].clone()).unwrap_or_default();
                if let Some(p) = self.cur.as_mut()
                    && p.kind == Kind::Item
                {
                    p.lead = format!("{parent}{} ", if done { "☑" } else { "☐" });
                }
            }
            Event::Rule => self.flush(),
            _ => {}
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {
                // A loose item's first paragraph continues the item, so it keeps the bullet.
                let in_item = matches!(&self.cur, Some(p) if p.kind == Kind::Item && p.text.is_empty());
                if !in_item {
                    let b = self.base();
                    self.open(Kind::Para, b.clone(), b);
                }
            }
            Tag::Heading { .. } => self.open(Kind::Heading, String::new(), String::new()),
            Tag::CodeBlock(_) => {
                self.flush();
                self.code = Some(String::new());
            }
            Tag::List(start) => {
                self.flush();
                self.lists.push(start);
            }
            Tag::Item => {
                let parent = self.base();
                let marker = match self.lists.last_mut() {
                    Some(Some(n)) => {
                        let m = format!("{n}. ");
                        *n += 1;
                        m
                    }
                    _ => "• ".to_string(),
                };
                let cont = format!("{parent}{}", " ".repeat(UnicodeWidthStr::width(marker.as_str())));
                self.open(Kind::Item, format!("{parent}{marker}"), cont.clone());
                self.items.push(cont);
            }
            Tag::BlockQuote(_) | Tag::Table(_) => self.flush(),
            Tag::TableHead | Tag::TableRow => {
                let b = self.base();
                self.open(Kind::Row, b.clone(), b);
            }
            Tag::TableCell => {
                if let Some(p) = self.cur.as_mut() {
                    p.text.clear();
                }
            }
            _ => {}
        }
    }

    fn end(&mut self, end: TagEnd) {
        match end {
            TagEnd::Paragraph
            | TagEnd::Heading(_)
            | TagEnd::BlockQuote(_)
            | TagEnd::Table
            | TagEnd::TableHead
            | TagEnd::TableRow => self.flush(),
            TagEnd::Item => {
                self.flush();
                self.items.pop();
            }
            TagEnd::List(_) => {
                self.flush();
                self.lists.pop();
            }
            TagEnd::CodeBlock => {
                if let Some(body) = self.code.take() {
                    self.code_units(&body);
                }
            }
            TagEnd::TableCell => {
                if let Some(p) = self.cur.as_mut() {
                    let c = normalise(&p.text);
                    p.text.clear();
                    p.cells.push(c);
                }
            }
            _ => {}
        }
    }

    fn text(&mut self, t: &str) {
        if let Some(code) = self.code.as_mut() {
            code.push_str(t);
            return;
        }
        if self.cur.is_none() {
            let b = self.base();
            self.open(Kind::Para, b.clone(), b);
        }
        if let Some(p) = self.cur.as_mut() {
            p.text.push_str(t);
        }
    }

    /// Each non-blank line of a code block is a unit; the fence lines are not.
    fn code_units(&mut self, body: &str) {
        self.block += 1;
        let base = self.base();
        for line in body.lines() {
            let t = line.trim_end().replace('\t', "    ");
            if t.trim().is_empty() {
                continue;
            }
            self.units.push(Unit::new(t, Kind::Code, base.clone(), base.clone(), self.block));
        }
    }

    fn finish(mut self) -> Vec<Unit> {
        self.flush();
        self.units.retain(|u| !u.words.is_empty());
        self.units
    }
}

/// The units of one reply with the global word numbering the picker works in.
pub struct Doc {
    units: Vec<Unit>,
    /// Unit of each word.
    word_unit: Vec<usize>,
    /// Global word range `[first, end)` of each unit.
    unit_span: Vec<(usize, usize)>,
}

impl Doc {
    fn new(units: Vec<Unit>) -> Doc {
        let mut word_unit = Vec::new();
        let mut unit_span = Vec::new();
        for (u, unit) in units.iter().enumerate() {
            let s = word_unit.len();
            word_unit.extend(std::iter::repeat_n(u, unit.words.len()));
            unit_span.push((s, word_unit.len()));
        }
        Doc { units, word_unit, unit_span }
    }

    /// The text of words `[lo, hi)`. Pieces of one paragraph join with a space, pieces of different
    /// blocks and code lines join with a newline.
    fn quote_text(&self, lo: usize, hi: usize) -> String {
        let mut out = String::new();
        let mut prev: Option<&Unit> = None;
        for u in self.word_unit[lo]..=self.word_unit[hi - 1] {
            let unit = &self.units[u];
            let (ws, we) = self.unit_span[u];
            let (a, b) = (lo.max(ws), hi.min(we));
            let start = if a == ws { 0 } else { unit.words[a - ws].0 };
            let end = if b == we { unit.text.len() } else { unit.words[b - ws - 1].1 };
            if let Some(p) = prev {
                let joined = p.block == unit.block && p.kind != Kind::Code && unit.kind != Kind::Code;
                out.push(if joined { ' ' } else { '\n' });
            }
            out.push_str(&unit.text[start..end]);
            prev = Some(unit);
        }
        out
    }

    /// The blockquote for words `[lo, hi)`: every line prefixed `> `.
    fn block(&self, lo: usize, hi: usize) -> String {
        self.quote_text(lo, hi).lines().map(|l| format!("> {l}")).collect::<Vec<_>>().join("\n")
    }
}

/// Merges overlapping and touching ranges.
fn merge(mut v: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    v.sort_unstable();
    let mut out: Vec<(usize, usize)> = Vec::new();
    for (a, b) in v {
        match out.last_mut() {
            Some(last) if a <= last.1 => last.1 = last.1.max(b),
            _ => out.push((a, b)),
        }
    }
    out
}

fn add(v: &mut Vec<(usize, usize)>, lo: usize, hi: usize) {
    v.push((lo, hi));
    *v = merge(std::mem::take(v));
}

fn remove(v: &mut Vec<(usize, usize)>, lo: usize, hi: usize) {
    let mut out = Vec::new();
    for (a, b) in v.drain(..) {
        if b <= lo || a >= hi {
            out.push((a, b));
        } else {
            if a < lo {
                out.push((a, lo));
            }
            if b > hi {
                out.push((hi, b));
            }
        }
    }
    *v = out;
}

fn covered(v: &[(usize, usize)], lo: usize, hi: usize) -> bool {
    v.iter().any(|&(a, b)| a <= lo && hi <= b)
}

fn touches(v: &[(usize, usize)], lo: usize, hi: usize) -> bool {
    v.iter().any(|&(a, b)| a < hi && lo < b)
}

/// One reply with its own marks, cursor and scroll position.
struct View {
    doc: Doc,
    /// Marked word ranges, sorted and merged.
    marks: Vec<(usize, usize)>,
    /// Cursor word.
    cur: usize,
    /// Visual selection: `[start, end)` words, `start` fixed while `end` moves.
    vis: Option<(usize, usize)>,
    /// First body row shown.
    top: usize,
    /// Body rows of each unit and the row of each word, from the last render.
    unit_rows: Vec<(usize, usize)>,
    word_row: Vec<usize>,
    /// Body rows of the last render.
    body_h: usize,
}

/// What a key did to the picker.
#[derive(Debug, PartialEq)]
pub enum ReplyResult {
    Pending,
    /// Close without a change.
    Close,
    /// Insert these blockquotes into the composer.
    Quote(Vec<String>),
}

impl View {
    fn new(units: Vec<Unit>) -> View {
        View {
            doc: Doc::new(units),
            marks: Vec::new(),
            cur: 0,
            vis: None,
            top: 0,
            unit_rows: Vec::new(),
            word_row: Vec::new(),
            body_h: 1,
        }
    }

    fn words(&self) -> usize {
        self.doc.word_unit.len()
    }

    fn unit(&self, g: usize) -> usize {
        self.doc.word_unit[g]
    }

    fn span(&self, u: usize) -> (usize, usize) {
        self.doc.unit_span[u]
    }

    fn key(&mut self, k: KeyEvent) -> ReplyResult {
        let plain = !k.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        match k.code {
            KeyCode::Esc => {
                if self.vis.is_some() {
                    self.vis = None;
                } else {
                    return ReplyResult::Close;
                }
            }
            KeyCode::Up => self.up(),
            KeyCode::Char('k') if plain => self.up(),
            KeyCode::Down => self.down(),
            KeyCode::Char('j') if plain => self.down(),
            KeyCode::Left => self.left(),
            KeyCode::Char('h') if plain => self.left(),
            KeyCode::Right => self.right(),
            KeyCode::Char('l') if plain => self.right(),
            KeyCode::PageUp => self.page(-1),
            KeyCode::PageDown => self.page(1),
            KeyCode::Home | KeyCode::Char('g') if plain => {
                if self.vis.is_none() {
                    self.cur = 0;
                }
            }
            KeyCode::End | KeyCode::Char('G') if plain => {
                if self.vis.is_none() {
                    self.cur = self.words() - 1;
                }
            }
            KeyCode::Char(' ') if plain => {
                if self.vis.is_some() {
                    self.finish_visual();
                } else {
                    self.toggle_unit();
                }
            }
            KeyCode::Enter => {
                if self.vis.is_some() {
                    self.finish_visual();
                } else {
                    return ReplyResult::Quote(self.quotes());
                }
            }
            KeyCode::Char('v') if plain && self.vis.is_none() => self.start_visual(),
            KeyCode::Char('c') if plain => {
                self.marks.clear();
                self.vis = None;
            }
            _ => {}
        }
        ReplyResult::Pending
    }

    /// Up and Down go to the neighbouring unit; in visual mode they move the end by whole units.
    fn up(&mut self) {
        match self.vis {
            None => {
                let u = self.unit(self.cur);
                if u > 0 {
                    self.cur = self.span(u - 1).0;
                }
            }
            Some((s, e)) => {
                let u = self.unit(e - 1);
                if u > 0 {
                    self.vis = Some((s, self.span(u - 1).1.max(s + 1)));
                }
            }
        }
    }

    fn down(&mut self) {
        match self.vis {
            None => {
                let u = self.unit(self.cur);
                if u + 1 < self.doc.units.len() {
                    self.cur = self.span(u + 1).0;
                }
            }
            Some((s, e)) => {
                let u = self.unit(e - 1);
                if u + 1 < self.doc.units.len() {
                    self.vis = Some((s, self.span(u + 1).1));
                }
            }
        }
    }

    fn left(&mut self) {
        match self.vis {
            None => self.cur = self.cur.saturating_sub(1),
            Some((s, e)) => self.vis = Some((s, e.saturating_sub(1).max(s + 1))),
        }
    }

    fn right(&mut self) {
        let n = self.words();
        match self.vis {
            None => self.cur = (self.cur + 1).min(n - 1),
            Some((s, e)) => self.vis = Some((s, (e + 1).min(n))),
        }
    }

    /// Moves about one body height. Before the first render (no row map) it moves one unit.
    fn page(&mut self, dir: isize) {
        let units = self.doc.units.len();
        let (anchor, s) = match self.vis {
            None => (self.unit(self.cur), None),
            Some((s, e)) => (self.unit(e - 1), Some(s)),
        };
        let mut t = if self.unit_rows.len() == units {
            let row = self.unit_rows[anchor].0 as isize + dir * self.body_h as isize;
            let row = row.max(0) as usize;
            self.unit_rows.partition_point(|r| r.0 <= row).saturating_sub(1)
        } else {
            (anchor as isize + dir).clamp(0, units as isize - 1) as usize
        };
        if dir > 0 && t <= anchor {
            t = (anchor + 1).min(units - 1);
        }
        if dir < 0 && t >= anchor {
            t = anchor.saturating_sub(1);
        }
        match s {
            None => self.cur = self.span(t).0,
            Some(s) => {
                let end = if dir > 0 { self.span(t).1 } else { self.span(t).1.max(s + 1) };
                self.vis = Some((s, end));
            }
        }
    }

    fn toggle_unit(&mut self) {
        let (s, e) = self.span(self.unit(self.cur));
        if covered(&self.marks, s, e) {
            remove(&mut self.marks, s, e);
        } else {
            add(&mut self.marks, s, e);
        }
    }

    /// Anchored at the unit when the cursor is at its start, otherwise at the cursor word.
    fn start_visual(&mut self) {
        let (s, e) = self.span(self.unit(self.cur));
        self.vis = Some(if self.cur == s { (s, e) } else { (self.cur, self.cur + 1) });
    }

    fn finish_visual(&mut self) {
        if let Some((s, e)) = self.vis.take() {
            add(&mut self.marks, s, e);
        }
    }

    /// Marks if any exist, else the current unit.
    fn quotes(&self) -> Vec<String> {
        if self.marks.is_empty() {
            let (s, e) = self.span(self.unit(self.cur));
            vec![self.doc.block(s, e)]
        } else {
            self.marks.iter().map(|&(a, b)| self.doc.block(a, b)).collect()
        }
    }

    fn word_style(&self, g: usize, base: ContentStyle) -> ContentStyle {
        let mut s = base;
        if self.marks.iter().any(|&(a, b)| a <= g && g < b) {
            s = theme::get().fg(Role::Accent);
            s.attributes.set(Attribute::Underlined);
        }
        if self.vis.is_some_and(|(a, b)| a <= g && g < b) {
            s.attributes.set(Attribute::Reverse);
        }
        if g == self.cur {
            s.attributes.set(Attribute::Reverse);
            s.attributes.set(Attribute::Bold);
        }
        s
    }

    /// The styled words of a unit, with spaces between them and cell separators shown as `│`.
    fn pieces(&self, u: usize) -> Vec<Piece> {
        let unit = &self.doc.units[u];
        let ws = self.span(u).0;
        let base = match unit.kind {
            Kind::Heading => {
                let mut s = theme::get().fg(Role::Strong);
                s.attributes.set(Attribute::Bold);
                s
            }
            Kind::Code => theme::get().fg(Role::Literal),
            _ => ContentStyle::default(),
        };
        let rs = runs(&unit.text);
        let mut out = Vec::new();
        if unit.kind == Kind::Code
            && let Some(&(a, _)) = rs.first()
            && a > 0
        {
            out.push(Piece { text: unit.text[..a].to_string(), style: base, word: None, gap: false });
        }
        let mut k = 0;
        for (i, &(a, b)) in rs.iter().enumerate() {
            if i > 0 {
                out.push(Piece { text: " ".into(), style: base, word: None, gap: true });
            }
            let t = &unit.text[a..b];
            if unit.kind == Kind::Row && t == CELL_SEP {
                out.push(Piece { text: "│".into(), style: dim(), word: None, gap: false });
            } else {
                let g = ws + k;
                k += 1;
                out.push(Piece { text: t.to_string(), style: self.word_style(g, base), word: Some(g), gap: false });
            }
        }
        out
    }

    /// Wraps unit `u` to `w` columns. Returns its rows and the row of each word (offset in the unit).
    fn layout(&self, u: usize, w: usize) -> (Vec<Line>, Vec<(usize, usize)>) {
        let unit = &self.doc.units[u];
        let mut wr = Wrap {
            cont: &unit.cont,
            cont_w: UnicodeWidthStr::width(unit.cont.as_str()),
            rows: Vec::new(),
            cur: vec![span(unit.lead.clone(), dim())],
            cw: UnicodeWidthStr::width(unit.lead.as_str()),
            pw: UnicodeWidthStr::width(unit.lead.as_str()),
            words: Vec::new(),
        };
        let mut gap = false;
        for p in self.pieces(u) {
            if p.gap {
                gap = true;
                continue;
            }
            let tw = UnicodeWidthStr::width(p.text.as_str());
            let sp = usize::from(gap && wr.cw > wr.pw);
            gap = false;
            if wr.cw + sp + tw <= w {
                if sp == 1 {
                    wr.cur.push(span(" ", ContentStyle::default()));
                    wr.cw += 1;
                }
                if let Some(g) = p.word {
                    wr.words.push((g, wr.rows.len()));
                }
                wr.cur.push(span(p.text, p.style));
                wr.cw += tw;
                continue;
            }
            if wr.cw > wr.pw {
                wr.newline();
            }
            if tw <= w.saturating_sub(wr.pw) {
                if let Some(g) = p.word {
                    wr.words.push((g, wr.rows.len()));
                }
                wr.cur.push(span(p.text, p.style));
                wr.cw += tw;
                continue;
            }
            let mut first = true;
            for c in p.text.chars() {
                let cwid = UnicodeWidthChar::width(c).unwrap_or(0);
                if wr.cw + cwid > w && wr.cw > wr.pw {
                    wr.newline();
                }
                if first && let Some(g) = p.word {
                    wr.words.push((g, wr.rows.len()));
                }
                first = false;
                wr.cur.push(span(c.to_string(), p.style));
                wr.cw += cwid;
            }
        }
        wr.rows.push(wr.cur);
        (wr.rows, wr.words)
    }

    /// Renders the reply: header, the window of body rows that keeps the cursor's unit visible,
    /// and a two-row footer when there is room. `which` is `(index, count)` for the header.
    fn render(&mut self, width: usize, max_rows: usize, which: (usize, usize)) -> (Vec<Line>, (usize, usize)) {
        let header = usize::from(max_rows >= 3);
        let footer = if max_rows >= 8 { 2 } else { 0 };
        let h = max_rows.saturating_sub(header + footer).max(1);
        let w = width.saturating_sub(GUTTER).max(1);
        let n = self.doc.units.len();

        let mut all: Vec<Line> = Vec::new();
        // The unit each row belongs to; `usize::MAX` for the blank rows between blocks.
        let mut row_unit: Vec<usize> = Vec::new();
        let mut unit_rows = vec![(0, 0); n];
        let mut word_row = vec![0; self.words()];
        let mut prev_block = None;
        for u in 0..n {
            let block = self.doc.units[u].block;
            if prev_block.is_some_and(|b| b != block) {
                all.push(Vec::new());
                row_unit.push(usize::MAX);
            }
            prev_block = Some(block);
            let (rows, words) = self.layout(u, w);
            let start = all.len();
            unit_rows[u] = (start, start + rows.len());
            for (g, r) in words {
                word_row[g] = start + r;
            }
            for r in rows {
                all.push(r);
                row_unit.push(u);
            }
        }

        let cu = self.unit(self.cur);
        for (line, &u) in all.iter_mut().zip(&row_unit) {
            if u == usize::MAX {
                continue;
            }
            let (s, e) = self.span(u);
            let gutter = if u == cu {
                span("❯ ", accent())
            } else if touches(&self.marks, s, e) {
                span("▌ ", accent())
            } else if self.vis.is_some_and(|(a, b)| a < e && s < b) {
                span("┃ ", dim())
            } else {
                span("  ", ContentStyle::default())
            };
            line.insert(0, gutter);
        }

        let total = all.len();
        let cr = word_row[self.cur];
        let (s, e) = unit_rows[cu];
        // A unit that fits the window is kept whole; a taller one keeps its cursor row.
        let (lo, hi) = if e - s <= h { (s, e) } else { (cr, cr + 1) };
        if lo < self.top {
            self.top = lo;
        }
        if hi > self.top + h {
            self.top = hi - h;
        }
        self.top = self.top.min(total.saturating_sub(h));
        self.body_h = h;
        self.unit_rows = unit_rows;
        self.word_row = word_row;

        let mut out: Vec<Line> = Vec::new();
        if header == 1 {
            let visual = if self.vis.is_some() { " · VISUAL" } else { "" };
            out.push(vec![span(
                format!("reply {}/{} · {} marked{visual}", which.0, which.1, self.marks.len()),
                accent(),
            )]);
        }
        let end = (self.top + h).min(total);
        out.extend(all[self.top..end].iter().cloned());
        if footer == 2 {
            let (a, b) = if self.vis.is_some() {
                ("VISUAL  ←→ word · ↑↓ unit · Space/Enter mark", "Esc cancel visual · Ctrl+C close")
            } else {
                (
                    "↑↓ unit · ←→ word · PgUp/PgDn · Home/End · [ ] reply",
                    "Space mark · v visual · c clear · Enter quote · Esc close",
                )
            };
            out.push(line::styled(a, |_| dim()));
            out.push(line::styled(b, |_| dim()));
        }
        let cursor_row = if cr >= self.top && cr < self.top + h { header + cr - self.top } else { header };
        let out = out.iter().map(|l| line::truncate(l, width)).collect();
        (out, (cursor_row, 0))
    }
}

fn accent() -> ContentStyle {
    let mut s = theme::get().fg(Role::Accent);
    s.attributes.set(Attribute::Bold);
    s
}

/// A styled word or gap, before wrapping.
struct Piece {
    text: String,
    style: ContentStyle,
    word: Option<usize>,
    /// A space between words: it is dropped where a row breaks.
    gap: bool,
}

struct Wrap<'a> {
    cont: &'a str,
    cont_w: usize,
    rows: Vec<Line>,
    cur: Line,
    /// Width of the current row, and of its prefix.
    cw: usize,
    pw: usize,
    /// `(global word, row within the unit)` of each word placed.
    words: Vec<(usize, usize)>,
}

impl Wrap<'_> {
    fn newline(&mut self) {
        let next = vec![span(self.cont, dim())];
        self.rows.push(std::mem::replace(&mut self.cur, next));
        self.cw = self.cont_w;
        self.pw = self.cont_w;
    }
}

/// The reply picker overlay: one view per reply that has any units, the newest selected first.
pub struct ReplyPicker {
    views: Vec<View>,
    at: usize,
}

impl ReplyPicker {
    /// Opens on the newest reply that has text. `None` when there is nothing to quote.
    pub fn open(replies: &[String]) -> Option<ReplyPicker> {
        let views: Vec<View> = replies.iter().map(|md| parse(md)).filter(|u| !u.is_empty()).map(View::new).collect();
        if views.is_empty() {
            return None;
        }
        let at = views.len() - 1;
        Some(ReplyPicker { views, at })
    }

    pub fn on_key(&mut self, k: KeyEvent) -> ReplyResult {
        if k.kind == KeyEventKind::Release {
            return ReplyResult::Pending;
        }
        if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') {
            return ReplyResult::Close;
        }
        let plain = !k.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        if plain {
            let last = self.views.len() - 1;
            let to = match k.code {
                KeyCode::Char('[') => self.at.saturating_sub(1),
                KeyCode::Char(']') => (self.at + 1).min(last),
                _ => return self.views[self.at].key(k),
            };
            self.views[self.at].vis = None;
            self.at = to;
            return ReplyResult::Pending;
        }
        self.views[self.at].key(k)
    }

    pub fn render(&mut self, width: usize, max_rows: usize) -> (Vec<Line>, (usize, usize)) {
        let n = self.views.len();
        let at = self.at;
        self.views[at].render(width, max_rows, (at + 1, n))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::line::line_text;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    fn press(p: &mut ReplyPicker, c: KeyCode) -> ReplyResult {
        p.on_key(key(c))
    }

    fn picker(md: &str) -> ReplyPicker {
        ReplyPicker::open(&[md.to_string()]).expect("has units")
    }

    fn view(p: &ReplyPicker) -> &View {
        &p.views[p.at]
    }

    const ALPHA: &str = "Alpha one. Beta two.\n\nGamma three.";

    #[test]
    fn sentence_splitting_is_conservative() {
        let cases: &[(&str, &[&str])] = &[
            ("Use e.g. this and i.e. that. Done.", &["Use e.g. this and i.e. that.", "Done."]),
            ("Ask vs. the rest. Next one.", &["Ask vs. the rest.", "Next one."]),
            ("Mr. Smith arrived. He sat.", &["Mr. Smith arrived.", "He sat."]),
            ("The value is 78.97% today. Then 78.97 more.", &["The value is 78.97% today.", "Then 78.97 more."]),
            ("Edit src/tui/x.rs. Then run it.", &["Edit src/tui/x.rs. Then run it."]),
            ("Open https://example.com. Then read.", &["Open https://example.com. Then read."]),
            ("Bump to v1.2. Next release is later.", &["Bump to v1.2. Next release is later."]),
            ("It works! Really? Yes. Done", &["It works!", "Really?", "Yes.", "Done"]),
            ("He said \"stop.\" Then left.", &["He said \"stop.\"", "Then left."]),
            ("Done. the next is lower.", &["Done. the next is lower."]),
            ("Costs 3. Then 4.", &["Costs 3. Then 4."]),
            ("J. Smith wrote it. Yes.", &["J. Smith wrote it.", "Yes."]),
            ("Wait... really? Yes.", &["Wait... really?", "Yes."]),
            ("Run `a. B` now. Ok.", &["Run `a. B` now.", "Ok."]),
            ("A list: first item (see docs.) Second.", &["A list: first item (see docs.)", "Second."]),
            ("No terminator here", &["No terminator here"]),
            ("It ran. 2 tests failed.", &["It ran.", "2 tests failed."]),
        ];
        for (s, want) in cases {
            assert_eq!(split_sentences(s), *want, "{s}");
        }
    }

    #[test]
    fn units_cover_lists_headings_tables_and_code() {
        let md = "Intro sentence. Second *one*.\n\n- First item\n  - nested **bold** item\n- Second `code.` item\n\n1. one\n2. [two](http://x.y)\n\n## Heading *here*\n\n| a | b |\n|---|---|\n| 1 | [link](http://x.y) |\n\n```rust\nfn main() {\n\n    let x = 1;\n}\n```\n";
        let got: Vec<(String, Kind, String, String)> =
            parse(md).into_iter().map(|u| (u.text, u.kind, u.lead, u.cont)).collect();
        let s = |a: &str, k, l: &str, c: &str| (a.to_string(), k, l.to_string(), c.to_string());
        assert_eq!(
            got,
            vec![
                s("Intro sentence.", Kind::Para, "", ""),
                s("Second one.", Kind::Para, "", ""),
                s("First item", Kind::Item, "• ", "  "),
                s("nested bold item", Kind::Item, "  • ", "    "),
                s("Second `code.` item", Kind::Item, "• ", "  "),
                s("one", Kind::Item, "1. ", "   "),
                s("two", Kind::Item, "2. ", "   "),
                s("Heading here", Kind::Heading, "", ""),
                s("a · b", Kind::Row, "", ""),
                s("1 · link", Kind::Row, "", ""),
                s("fn main() {", Kind::Code, "", ""),
                s("    let x = 1;", Kind::Code, "", ""),
                s("}", Kind::Code, "", ""),
            ]
        );
    }

    #[test]
    fn long_items_split_into_sentences_and_short_ones_do_not() {
        let long = "This list item goes on for quite a while so that it passes the length limit for splitting. \
                    It has a second sentence that carries on for a good while longer than the first one did.";
        let units = parse(&format!("- {long}\n- Short one. Two sentences.\n"));
        let got: Vec<&str> = units.iter().map(|u| u.lead.as_str()).collect();
        assert_eq!(got.len(), 3, "{got:?}");
        assert_eq!(got[0], "• ");
        assert_eq!(got[1], "  ", "a long item's second sentence hangs under the bullet");
        assert_eq!(units[2].text, "Short one. Two sentences.");
    }

    #[test]
    fn adjacent_marks_merge_into_one_quote() {
        let mut p = picker(ALPHA);
        press(&mut p, KeyCode::Char(' '));
        press(&mut p, KeyCode::Down);
        press(&mut p, KeyCode::Char(' '));
        assert_eq!(view(&p).marks, [(0, 4)]);
        assert_eq!(press(&mut p, KeyCode::Enter), ReplyResult::Quote(vec!["> Alpha one. Beta two.".into()]));
    }

    #[test]
    fn marks_quote_in_document_order_each_as_a_blockquote() {
        let mut p = picker(ALPHA);
        press(&mut p, KeyCode::Down);
        press(&mut p, KeyCode::Down);
        press(&mut p, KeyCode::Char(' '));
        press(&mut p, KeyCode::Up);
        press(&mut p, KeyCode::Up);
        press(&mut p, KeyCode::Char(' '));
        assert_eq!(
            press(&mut p, KeyCode::Enter),
            ReplyResult::Quote(vec!["> Alpha one.".into(), "> Gamma three.".into()])
        );
    }

    #[test]
    fn a_selection_across_blocks_quotes_each_line_with_its_own_prefix() {
        let mut p = picker(ALPHA);
        press(&mut p, KeyCode::Down);
        press(&mut p, KeyCode::Char(' '));
        press(&mut p, KeyCode::Down);
        press(&mut p, KeyCode::Char(' '));
        assert_eq!(press(&mut p, KeyCode::Enter), ReplyResult::Quote(vec!["> Beta two.\n> Gamma three.".into()]));
    }

    #[test]
    fn enter_without_marks_quotes_the_current_unit() {
        let mut p = picker(ALPHA);
        press(&mut p, KeyCode::Down);
        assert_eq!(press(&mut p, KeyCode::Enter), ReplyResult::Quote(vec!["> Beta two.".into()]));
    }

    #[test]
    fn space_toggles_a_unit_mark_and_c_clears_all() {
        let mut p = picker(ALPHA);
        press(&mut p, KeyCode::Char(' '));
        assert_eq!(view(&p).marks, [(0, 2)]);
        press(&mut p, KeyCode::Char(' '));
        assert!(view(&p).marks.is_empty());
        press(&mut p, KeyCode::Char(' '));
        press(&mut p, KeyCode::Down);
        press(&mut p, KeyCode::Char(' '));
        press(&mut p, KeyCode::Char('c'));
        assert!(view(&p).marks.is_empty());
    }

    #[test]
    fn space_on_a_partly_marked_unit_completes_it_and_a_second_space_clears_it() {
        let mut p = picker(ALPHA);
        press(&mut p, KeyCode::Right);
        press(&mut p, KeyCode::Char('v'));
        press(&mut p, KeyCode::Enter);
        assert_eq!(view(&p).marks, [(1, 2)], "a word mark from the cursor word");
        press(&mut p, KeyCode::Left);
        press(&mut p, KeyCode::Char(' '));
        assert_eq!(view(&p).marks, [(0, 2)]);
        press(&mut p, KeyCode::Char(' '));
        assert!(view(&p).marks.is_empty());
    }

    #[test]
    fn visual_mode_grows_and_shrinks_by_word() {
        let mut p = picker(ALPHA);
        press(&mut p, KeyCode::Char('v'));
        assert_eq!(view(&p).vis, Some((0, 2)), "starts with the whole current unit");
        press(&mut p, KeyCode::Right);
        assert_eq!(view(&p).vis, Some((0, 3)));
        press(&mut p, KeyCode::Right);
        assert_eq!(view(&p).vis, Some((0, 4)), "a word past the unit joins the next one");
        press(&mut p, KeyCode::Left);
        assert_eq!(view(&p).vis, Some((0, 3)));
        press(&mut p, KeyCode::Left);
        press(&mut p, KeyCode::Left);
        assert_eq!(view(&p).vis, Some((0, 1)), "never shrinks below one word");
    }

    #[test]
    fn visual_starting_mid_unit_crosses_units_and_quotes_the_words() {
        let mut p = picker(ALPHA);
        press(&mut p, KeyCode::Right);
        press(&mut p, KeyCode::Char('v'));
        assert_eq!(view(&p).vis, Some((1, 2)));
        press(&mut p, KeyCode::Right);
        assert_eq!(press(&mut p, KeyCode::Enter), ReplyResult::Pending);
        assert_eq!(view(&p).marks, [(1, 3)]);
        assert_eq!(press(&mut p, KeyCode::Enter), ReplyResult::Quote(vec!["> one. Beta".into()]));
    }

    #[test]
    fn visual_up_and_down_move_the_end_by_whole_units() {
        let mut p = picker(ALPHA);
        press(&mut p, KeyCode::Char('v'));
        press(&mut p, KeyCode::Down);
        assert_eq!(view(&p).vis, Some((0, 4)));
        press(&mut p, KeyCode::Up);
        assert_eq!(view(&p).vis, Some((0, 2)));
        press(&mut p, KeyCode::Char(' '));
        assert_eq!(view(&p).marks, [(0, 2)], "space finishes the selection into a mark");
        assert!(view(&p).vis.is_none());
    }

    #[test]
    fn esc_leaves_visual_without_marking_and_esc_again_closes() {
        let mut p = picker(ALPHA);
        press(&mut p, KeyCode::Char('v'));
        press(&mut p, KeyCode::Right);
        assert_eq!(press(&mut p, KeyCode::Esc), ReplyResult::Pending);
        assert!(view(&p).vis.is_none() && view(&p).marks.is_empty());
        assert_eq!(press(&mut p, KeyCode::Esc), ReplyResult::Close);
    }

    #[test]
    fn ctrl_c_closes_from_anywhere() {
        let mut p = picker(ALPHA);
        press(&mut p, KeyCode::Char('v'));
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(p.on_key(ctrl_c), ReplyResult::Close);
    }

    #[test]
    fn enter_in_visual_marks_instead_of_quoting() {
        let mut p = picker(ALPHA);
        press(&mut p, KeyCode::Char('v'));
        press(&mut p, KeyCode::Down);
        assert_eq!(press(&mut p, KeyCode::Enter), ReplyResult::Pending);
        assert_eq!(view(&p).marks, [(0, 4)]);
    }

    #[test]
    fn the_window_keeps_the_current_unit_visible_at_tiny_heights() {
        let md: String = (1..=10).map(|k| format!("Para {k} sentence.\n\n")).collect();
        let mut p = picker(&md);
        for _ in 0..7 {
            press(&mut p, KeyCode::Down);
        }
        for h in 1..=12 {
            let (lines, cur) = p.render(40, h);
            let t: Vec<String> = lines.iter().map(line_text).collect();
            assert!(lines.len() <= h, "h={h}: {t:?}");
            assert!(t.iter().any(|l| l.starts_with("❯ ") && l.contains("Para 8")), "h={h}: {t:?}");
            assert!(cur.0 < h, "h={h}: {cur:?}");
        }
    }

    #[test]
    fn a_unit_taller_than_the_window_keeps_the_cursor_row() {
        let md = vec!["word"; 60].join(" ");
        let mut p = picker(&md);
        for _ in 0..30 {
            press(&mut p, KeyCode::Right);
        }
        let (lines, cur) = p.render(20, 4);
        assert!(lines.len() <= 4);
        assert!(cur.0 < 4, "{cur:?}");
        assert!(lines.iter().any(|l| line_text(l).starts_with("❯ ")));
    }

    #[test]
    fn wide_characters_wrap_within_the_width() {
        let md = "日本語の文章です。次の文章です。最後の文です。";
        let mut p = picker(md);
        let (lines, _) = p.render(12, 12);
        assert!(
            lines.iter().all(|l| line::line_width(l) <= 12),
            "{:?}",
            lines.iter().map(line_text).collect::<Vec<_>>()
        );
        assert!(lines.iter().any(|l| line_text(l).contains("日本語")));
    }

    #[test]
    fn opens_on_the_newest_reply_with_text_or_not_at_all() {
        assert!(ReplyPicker::open(&[]).is_none());
        assert!(ReplyPicker::open(&["```\n```".into(), "   ".into()]).is_none());
        let p = ReplyPicker::open(&["first one.".into(), "second one.".into(), String::new()]).unwrap();
        assert_eq!(p.at, 1);
        assert_eq!(view(&p).doc.units[0].text, "second one.");
    }

    #[test]
    fn brackets_switch_replies_and_keep_each_replies_marks() {
        let mut p = ReplyPicker::open(&["first one.".into(), "second one. more.".into()]).unwrap();
        press(&mut p, KeyCode::Char(' '));
        assert_eq!(press(&mut p, KeyCode::Char('[')), ReplyResult::Pending);
        assert_eq!(p.at, 0);
        let (lines, _) = p.render(60, 10);
        assert!(line_text(&lines[0]).starts_with("reply 1/2 · 0 marked"), "{:?}", line_text(&lines[0]));
        press(&mut p, KeyCode::Char('['));
        assert_eq!(p.at, 0, "stops at the oldest");
        press(&mut p, KeyCode::Char(']'));
        press(&mut p, KeyCode::Char(']'));
        assert_eq!(p.at, 1, "stops at the newest");
        let (lines, _) = p.render(60, 10);
        assert!(line_text(&lines[0]).starts_with("reply 2/2 · 1 marked"), "{:?}", line_text(&lines[0]));
        assert!(lines.iter().any(|l| line_text(l).contains("Enter quote")));
    }
}
