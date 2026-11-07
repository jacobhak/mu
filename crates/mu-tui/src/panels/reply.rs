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

use crate::text::accent;
use crate::text::line::{self, Line, dim, span};
use crate::theme::{self, Role};

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

    /// The markdown of words `[lo, hi)`. Pieces of one paragraph join with a space; list items
    /// keep their marker and break lines; other blocks are separated by a blank line and
    /// consecutive code lines sit in one fence, so the quote renders with its structure.
    fn quote_text(&self, lo: usize, hi: usize) -> String {
        let mut out = String::new();
        let mut prev: Option<&Unit> = None;
        for u in self.word_unit[lo]..=self.word_unit[hi - 1] {
            let unit = &self.units[u];
            let (ws, we) = self.unit_span[u];
            let (a, b) = (lo.max(ws), hi.min(we));
            let start = if a == ws { 0 } else { unit.words[a - ws].0 };
            let end = if b == we { unit.text.len() } else { unit.words[b - ws - 1].1 };
            let code = unit.kind == Kind::Code;
            match prev {
                None if code => out.push_str("```\n"),
                None => {}
                Some(p) => {
                    let pc = p.kind == Kind::Code;
                    out.push_str(match (pc, code) {
                        (true, true) => "\n",
                        (true, false) => "\n```\n\n",
                        (false, true) => "\n\n```\n",
                        (false, false) if p.block == unit.block => " ",
                        (false, false) if p.kind == Kind::Item && unit.kind == Kind::Item => "\n",
                        (false, false) => "\n\n",
                    });
                }
            }
            if unit.kind == Kind::Item && a == ws && !unit.lead.trim().is_empty() {
                out.push_str(&unit.lead.replace('•', "-").replace('☑', "- [x]").replace('☐', "- [ ]"));
            }
            out.push_str(&unit.text[start..end]);
            prev = Some(unit);
        }
        if prev.is_some_and(|p| p.kind == Kind::Code) {
            out.push_str("\n```");
        }
        out
    }

    /// The blockquote for words `[lo, hi)`: every line prefixed `> `.
    fn block(&self, lo: usize, hi: usize) -> String {
        let lines = self.quote_text(lo, hi);
        lines
            .lines()
            .map(|l| if l.is_empty() { ">".to_string() } else { format!("> {l}") })
            .collect::<Vec<_>>()
            .join("\n")
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
    /// Selection anchor; the selection runs between it and the cursor word, both included.
    anchor: Option<usize>,
    /// Whether the cursor word is shown: only after a left/right move, so plain sentence
    /// navigation highlights the unit, not a word.
    word_mode: bool,
    /// First body row shown.
    top: usize,
    /// Body rows of each unit and the row of each word, from the last render.
    unit_rows: Vec<(usize, usize)>,
    word_row: Vec<usize>,
    /// Body rows of the last render.
    body_h: usize,
    /// Vim count typed so far (`3j`); 0 when none.
    count: usize,
    /// A `g` was just pressed (`gg`).
    pending_g: bool,
}

/// A cursor move; the `bool` extends the selection.
#[derive(Clone, Copy)]
enum Mv {
    Up(bool),
    Down(bool),
    Left(bool),
    Right(bool),
    Page(isize, bool),
    Half(isize),
    Top(bool),
    Bottom(bool),
    UnitStart,
    UnitEnd,
    Block(isize),
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
            anchor: None,
            word_mode: false,
            top: 0,
            unit_rows: Vec::new(),
            word_row: Vec::new(),
            body_h: 1,
            count: 0,
            pending_g: false,
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
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let alt = k.modifiers.contains(KeyModifiers::ALT);
        let plain = !ctrl && !alt;
        let shift = k.modifiers.contains(KeyModifiers::SHIFT);
        let pending_g = std::mem::take(&mut self.pending_g);
        if plain
            && let KeyCode::Char(d @ '0'..='9') = k.code
            && (d != '0' || self.count > 0)
        {
            self.count = (self.count * 10 + (d as usize - '0' as usize)).min(999);
            return ReplyResult::Pending;
        }
        let had_count = self.count > 0;
        let n = std::mem::take(&mut self.count).max(1);
        let mv = match k.code {
            KeyCode::Esc => {
                // Esc first drops a pending count or selection, then closes.
                return if self.anchor.take().is_none() && !had_count {
                    ReplyResult::Close
                } else {
                    ReplyResult::Pending
                };
            }
            KeyCode::Char('g') if ctrl && !alt => {
                return if self.anchor.take().is_none() && !had_count {
                    ReplyResult::Close
                } else {
                    ReplyResult::Pending
                };
            }
            KeyCode::Up => Mv::Up(shift),
            KeyCode::Down => Mv::Down(shift),
            KeyCode::Left => Mv::Left(shift),
            KeyCode::Right => Mv::Right(shift),
            KeyCode::PageUp => Mv::Page(-1, shift),
            KeyCode::PageDown => Mv::Page(1, shift),
            KeyCode::Home if plain => Mv::Top(shift),
            KeyCode::End if plain => Mv::Bottom(shift),
            KeyCode::Enter => {
                self.take_selection();
                return ReplyResult::Quote(self.quotes());
            }
            KeyCode::Char(c) if plain => match c {
                'k' => Mv::Up(false),
                'K' => Mv::Up(true),
                'j' => Mv::Down(false),
                'J' => Mv::Down(true),
                'h' | 'b' => Mv::Left(false),
                'H' | 'B' => Mv::Left(true),
                'l' | 'w' | 'e' => Mv::Right(false),
                'L' | 'W' | 'E' => Mv::Right(true),
                '0' | '^' => Mv::UnitStart,
                '$' => Mv::UnitEnd,
                '{' => Mv::Block(-1),
                '}' => Mv::Block(1),
                'G' => Mv::Bottom(false),
                'g' if pending_g => Mv::Top(false),
                'g' => {
                    self.pending_g = true;
                    return ReplyResult::Pending;
                }
                ' ' => {
                    if !self.take_selection() {
                        self.toggle_unit();
                    }
                    return ReplyResult::Pending;
                }
                'c' => {
                    self.marks.clear();
                    self.anchor = None;
                    return ReplyResult::Pending;
                }
                'q' => return ReplyResult::Close,
                _ => return ReplyResult::Pending,
            },
            KeyCode::Char(c) if ctrl && !alt => match c.to_ascii_lowercase() {
                'n' => Mv::Down(false),
                'p' => Mv::Up(false),
                'f' => Mv::Right(false),
                'b' => Mv::Left(false),
                'a' => Mv::UnitStart,
                'e' => Mv::UnitEnd,
                'v' => Mv::Page(1, false),
                'd' => Mv::Half(1),
                'u' => Mv::Half(-1),
                _ => return ReplyResult::Pending,
            },
            KeyCode::Char(c) if alt && !ctrl => match c {
                'f' => Mv::Right(false),
                'b' => Mv::Left(false),
                'v' => Mv::Page(-1, false),
                '<' => Mv::Top(false),
                '>' => Mv::Bottom(false),
                _ => return ReplyResult::Pending,
            },
            _ => return ReplyResult::Pending,
        };
        // A counted word move moves at once instead of first revealing the cursor word.
        if n > 1 && matches!(mv, Mv::Left(false) | Mv::Right(false)) {
            self.word_mode = true;
        }
        for _ in 0..n {
            self.step(mv);
        }
        ReplyResult::Pending
    }

    fn step(&mut self, mv: Mv) {
        match mv {
            Mv::Up(x) => self.up(x),
            Mv::Down(x) => self.down(x),
            Mv::Left(x) => self.left(x),
            Mv::Right(x) => self.right(x),
            Mv::Page(d, x) => self.page(d, self.body_h, x),
            Mv::Half(d) => self.page(d, (self.body_h / 2).max(1), false),
            Mv::Top(x) => self.go(0, x),
            Mv::Bottom(x) => self.go(self.words() - 1, x),
            Mv::UnitStart => {
                self.go(self.span(self.unit(self.cur)).0, false);
                self.word_mode = true;
            }
            Mv::UnitEnd => {
                self.go(self.span(self.unit(self.cur)).1 - 1, false);
                self.word_mode = true;
            }
            Mv::Block(d) => self.block(d),
        }
    }

    /// The selected words `[start, end)`, if a selection is active.
    fn sel(&self) -> Option<(usize, usize)> {
        self.anchor.map(|a| (a.min(self.cur), a.max(self.cur) + 1))
    }

    /// Moves the cursor; `extend` keeps or starts a selection, otherwise it is dropped.
    fn go(&mut self, to: usize, extend: bool) {
        self.anchor = if extend { Some(self.anchor.unwrap_or(self.cur)) } else { None };
        self.cur = to;
        self.word_mode = false;
    }

    /// Turns the selection into a mark. Returns whether there was one.
    fn take_selection(&mut self) -> bool {
        let Some((s, e)) = self.sel() else { return false };
        self.anchor = None;
        add(&mut self.marks, s, e);
        true
    }

    /// Up and Down go to the neighbouring unit. Extending first goes to the edge of the current
    /// unit, then on by whole units.
    fn up(&mut self, extend: bool) {
        let u = self.unit(self.cur);
        let (s, _) = self.span(u);
        if extend && self.cur != s {
            self.go(s, true);
        } else if u > 0 {
            self.go(self.span(u - 1).0, extend);
        } else {
            self.go(self.cur, extend);
        }
    }

    fn down(&mut self, extend: bool) {
        let u = self.unit(self.cur);
        let (_, e) = self.span(u);
        if extend && self.cur != e - 1 {
            self.go(e - 1, true);
        } else if u + 1 < self.doc.units.len() {
            let (s, e) = self.span(u + 1);
            self.go(if extend { e - 1 } else { s }, extend);
        } else {
            self.go(self.cur, extend);
        }
    }

    /// The first plain Left/Right only reveals the cursor word; later ones move it.
    fn left(&mut self, extend: bool) {
        let reveal = !extend && !self.word_mode;
        self.go(if reveal { self.cur } else { self.cur.saturating_sub(1) }, extend);
        self.word_mode = true;
    }

    fn right(&mut self, extend: bool) {
        let reveal = !extend && !self.word_mode;
        self.go(if reveal { self.cur } else { (self.cur + 1).min(self.words() - 1) }, extend);
        self.word_mode = true;
    }

    /// Moves about `rows` body rows. Before the first render (no row map) it moves one unit.
    fn page(&mut self, dir: isize, rows: usize, extend: bool) {
        let units = self.doc.units.len();
        let from = self.unit(self.cur);
        let mut t = if self.unit_rows.len() == units {
            let row = self.unit_rows[from].0 as isize + dir * rows as isize;
            let row = row.max(0) as usize;
            self.unit_rows.partition_point(|r| r.0 <= row).saturating_sub(1)
        } else {
            (from as isize + dir).clamp(0, units as isize - 1) as usize
        };
        if dir > 0 && t <= from {
            t = (from + 1).min(units - 1);
        }
        if dir < 0 && t >= from {
            t = from.saturating_sub(1);
        }
        let (s, e) = self.span(t);
        self.go(if extend && dir > 0 { e - 1 } else { s }, extend);
    }

    /// The first unit of the block that unit `u` belongs to.
    fn block_start(&self, u: usize) -> usize {
        let b = self.doc.units[u].block;
        (0..=u).find(|&i| self.doc.units[i].block == b).unwrap_or(u)
    }

    /// `{` and `}`: to the start of the block (the first unit when already there, for `{`), or of
    /// the next one.
    fn block(&mut self, dir: isize) {
        let u = self.unit(self.cur);
        let first = self.block_start(u);
        let t = if dir > 0 {
            let b = self.doc.units[u].block;
            (u + 1..self.doc.units.len()).find(|&i| self.doc.units[i].block != b)
        } else if self.cur > self.span(first).0 {
            Some(first)
        } else {
            first.checked_sub(1).map(|p| self.block_start(p))
        };
        if let Some(t) = t {
            self.go(self.span(t).0, false);
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
        if self.sel().is_some_and(|(a, b)| a <= g && g < b) {
            s.attributes.set(Attribute::Reverse);
        }
        if self.word_mode && g == self.cur {
            s.attributes.set(Attribute::Reverse);
            s.attributes.set(Attribute::Bold);
        } else if !self.word_mode && self.unit(g) == self.unit(self.cur) {
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

    /// Renders the reply: a titled rule, the window of body rows that keeps the cursor's unit
    /// visible, a closing rule and a two-row footer when there is room. `which` is
    /// `(index, count)` for the title.
    fn render(&mut self, width: usize, max_rows: usize, which: (usize, usize)) -> (Vec<Line>, (usize, usize)) {
        let header = usize::from(max_rows >= 3);
        let bottom = usize::from(max_rows >= 5);
        let footer = if max_rows >= 9 { 2 } else { 0 };
        let h = max_rows.saturating_sub(header + bottom + footer).max(1);
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
            } else if self.sel().is_some_and(|(a, b)| a < e && s < b) {
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
            let selecting = if self.anchor.is_some() { " · selecting" } else { "" };
            let switch = if which.1 > 1 { " · [ ] Tab replies" } else { "" };
            let title = format!("reply {}/{} · {} marked{selecting}{switch}", which.0, which.1, self.marks.len());
            out.push(rule(Some(&title), width));
        }
        let end = (self.top + h).min(total);
        out.extend(all[self.top..end].iter().cloned());
        if bottom == 1 {
            out.push(rule(None, width));
        }
        if footer == 2 {
            let (a, b) = if self.anchor.is_some() {
                ("Shift+←→ word · Shift+↑↓ unit · Space mark", "Enter quote · Esc drop selection · Ctrl+G close")
            } else {
                (
                    "↑↓ jk unit · ←→ hl word · Shift+move select · gg G { } ^D ^U ^N ^P ^F ^B",
                    "Space mark · c clear · Enter quote · [ ] Tab reply (3[ jumps) · Esc close",
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

/// A full-width rule, with the title set into its left end.
pub(crate) fn rule(title: Option<&str>, width: usize) -> Line {
    let edge = theme::get().fg(Role::Line);
    let Some(t) = title else { return vec![span("─".repeat(width), edge)] };
    let used = 4 + UnicodeWidthStr::width(t);
    vec![
        span("── ", edge),
        span(t.to_string(), accent()),
        span(format!(" {}", "─".repeat(width.saturating_sub(used))), edge),
    ]
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
        let (ctrl, alt) = (k.modifiers.contains(KeyModifiers::CONTROL), k.modifiers.contains(KeyModifiers::ALT));
        // Ctrl+Alt is how some terminals report AltGr, which is how `[` and `]` are typed on many layouts.
        let bracket = (!ctrl && !alt) || (ctrl && alt);
        let dir: isize = match k.code {
            KeyCode::Char('[') if bracket => -1,
            KeyCode::Char(']') if bracket => 1,
            KeyCode::BackTab if !ctrl && !alt => -1,
            KeyCode::Tab if !ctrl && !alt => {
                if k.modifiers.contains(KeyModifiers::SHIFT) {
                    -1
                } else {
                    1
                }
            }
            _ => return self.views[self.at].key(k),
        };
        let view = &mut self.views[self.at];
        // A count (`3[`) jumps that many replies.
        let n = std::mem::take(&mut view.count).max(1) as isize;
        view.anchor = None;
        view.pending_g = false;
        self.at = (self.at as isize + dir * n).clamp(0, self.views.len() as isize - 1) as usize;
        ReplyResult::Pending
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
    use crate::text::line::line_text;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use mu_testutil::key;

    fn press(p: &mut ReplyPicker, c: KeyCode) -> ReplyResult {
        p.on_key(key(c))
    }

    fn shift(p: &mut ReplyPicker, c: KeyCode) -> ReplyResult {
        p.on_key(KeyEvent::new(c, KeyModifiers::SHIFT))
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
        assert_eq!(
            press(&mut p, KeyCode::Enter),
            ReplyResult::Quote(vec!["> Beta two.\n>\n> Gamma three.".into()]),
            "a blank quoted line keeps the paragraphs apart"
        );
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
        let mut p = picker("One two three.");
        press(&mut p, KeyCode::Right);
        press(&mut p, KeyCode::Right);
        shift(&mut p, KeyCode::Right);
        press(&mut p, KeyCode::Char(' '));
        assert_eq!(view(&p).marks, [(1, 3)], "a word mark from the selection");
        press(&mut p, KeyCode::Left);
        press(&mut p, KeyCode::Char(' '));
        assert_eq!(view(&p).marks, [(0, 3)]);
        press(&mut p, KeyCode::Char(' '));
        assert!(view(&p).marks.is_empty());
    }

    #[test]
    fn shift_left_and_right_grow_and_shrink_the_selection_by_word() {
        let mut p = picker(ALPHA);
        shift(&mut p, KeyCode::Right);
        assert_eq!(view(&p).sel(), Some((0, 2)));
        shift(&mut p, KeyCode::Right);
        assert_eq!(view(&p).sel(), Some((0, 3)), "a word past the unit joins the next one");
        shift(&mut p, KeyCode::Left);
        assert_eq!(view(&p).sel(), Some((0, 2)));
        shift(&mut p, KeyCode::Left);
        shift(&mut p, KeyCode::Left);
        assert_eq!(view(&p).sel(), Some((0, 1)), "never shrinks below one word");
    }

    #[test]
    fn selecting_backwards_works_and_a_plain_arrow_drops_the_selection() {
        let mut p = picker(ALPHA);
        press(&mut p, KeyCode::Right);
        press(&mut p, KeyCode::Right);
        press(&mut p, KeyCode::Right);
        shift(&mut p, KeyCode::Left);
        assert_eq!(view(&p).sel(), Some((1, 3)));
        press(&mut p, KeyCode::Left);
        assert!(view(&p).sel().is_none() && view(&p).marks.is_empty());
    }

    #[test]
    fn a_selection_crossing_units_quotes_the_words() {
        let mut p = picker(ALPHA);
        press(&mut p, KeyCode::Right);
        press(&mut p, KeyCode::Right);
        shift(&mut p, KeyCode::Right);
        assert_eq!(press(&mut p, KeyCode::Enter), ReplyResult::Quote(vec!["> one. Beta".into()]));
    }

    #[test]
    fn shift_up_and_down_go_to_the_unit_edge_then_by_whole_units() {
        let mut p = picker(ALPHA);
        shift(&mut p, KeyCode::Down);
        assert_eq!(view(&p).sel(), Some((0, 2)));
        shift(&mut p, KeyCode::Down);
        assert_eq!(view(&p).sel(), Some((0, 4)));
        shift(&mut p, KeyCode::Up);
        assert_eq!(view(&p).sel(), Some((0, 3)));
        shift(&mut p, KeyCode::Up);
        assert_eq!(view(&p).sel(), Some((0, 1)));
    }

    #[test]
    fn esc_drops_the_selection_without_marking_and_esc_again_closes() {
        let mut p = picker(ALPHA);
        shift(&mut p, KeyCode::Right);
        assert_eq!(press(&mut p, KeyCode::Esc), ReplyResult::Pending);
        assert!(view(&p).sel().is_none() && view(&p).marks.is_empty());
        assert_eq!(press(&mut p, KeyCode::Esc), ReplyResult::Close);
    }

    #[test]
    fn enter_quotes_the_selection_together_with_the_marks() {
        let mut p = picker(ALPHA);
        press(&mut p, KeyCode::Down);
        press(&mut p, KeyCode::Down);
        press(&mut p, KeyCode::Char(' '));
        press(&mut p, KeyCode::Up);
        press(&mut p, KeyCode::Up);
        shift(&mut p, KeyCode::Down);
        assert_eq!(
            press(&mut p, KeyCode::Enter),
            ReplyResult::Quote(vec!["> Alpha one.".into(), "> Gamma three.".into()])
        );
    }

    #[test]
    fn list_items_keep_their_markers_and_stay_on_separate_lines() {
        let mut p = picker("Intro here.\n\n- first item\n- second item\n\nOutro.");
        press(&mut p, KeyCode::Char('G'));
        for _ in 0..4 {
            shift(&mut p, KeyCode::Up);
        }
        assert_eq!(
            press(&mut p, KeyCode::Enter),
            ReplyResult::Quote(vec!["> Intro here.\n>\n> - first item\n> - second item\n>\n> Outro.".into()])
        );
    }

    #[test]
    fn code_lines_are_fenced_inside_the_quote() {
        let mut p = picker("Run:\n\n```sh\nls -l\ncd x\n```\n");
        shift(&mut p, KeyCode::Down);
        shift(&mut p, KeyCode::Down);
        shift(&mut p, KeyCode::Down);
        assert_eq!(
            press(&mut p, KeyCode::Enter),
            ReplyResult::Quote(vec!["> Run:\n>\n> ```\n> ls -l\n> cd x\n> ```".into()])
        );
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
        assert!(line_text(&lines[0]).starts_with("── reply 1/2 · 0 marked"), "{:?}", line_text(&lines[0]));
        press(&mut p, KeyCode::Char('['));
        assert_eq!(p.at, 0, "stops at the oldest");
        press(&mut p, KeyCode::Char(']'));
        press(&mut p, KeyCode::Char(']'));
        assert_eq!(p.at, 1, "stops at the newest");
        let (lines, _) = p.render(60, 10);
        assert!(line_text(&lines[0]).starts_with("── reply 2/2 · 1 marked"), "{:?}", line_text(&lines[0]));
        assert!(lines.iter().any(|l| line_text(l).contains("Enter quote")));
    }

    fn five() -> ReplyPicker {
        let r: Vec<String> = (1..=5).map(|i| format!("reply number {i}.")).collect();
        ReplyPicker::open(&r).unwrap()
    }

    #[test]
    fn tab_and_shift_tab_switch_replies() {
        let mut p = five();
        assert_eq!(p.at, 4);
        press(&mut p, KeyCode::BackTab);
        assert_eq!(p.at, 3);
        shift(&mut p, KeyCode::Tab);
        assert_eq!(p.at, 2);
        press(&mut p, KeyCode::Tab);
        assert_eq!(p.at, 3);
    }

    #[test]
    fn a_count_jumps_that_many_replies_and_clamps() {
        let mut p = five();
        press(&mut p, KeyCode::Char('3'));
        press(&mut p, KeyCode::Char('['));
        assert_eq!(p.at, 1);
        press(&mut p, KeyCode::Char('9'));
        press(&mut p, KeyCode::Char('['));
        assert_eq!(p.at, 0, "stops at the oldest");
        press(&mut p, KeyCode::Char('2'));
        press(&mut p, KeyCode::Char(']'));
        assert_eq!(p.at, 2);
        assert_eq!(view(&p).count, 0, "the count is spent");
    }

    #[test]
    fn altgr_brackets_switch_replies_but_ctrl_or_alt_alone_do_not() {
        let mut p = five();
        let both = KeyModifiers::CONTROL | KeyModifiers::ALT;
        p.on_key(KeyEvent::new(KeyCode::Char('['), both));
        assert_eq!(p.at, 3);
        p.on_key(KeyEvent::new(KeyCode::Char('['), KeyModifiers::ALT));
        p.on_key(KeyEvent::new(KeyCode::Char('['), KeyModifiers::CONTROL));
        assert_eq!(p.at, 3);
    }

    #[test]
    fn the_title_names_the_switch_keys_only_with_several_replies() {
        let (lines, _) = five().render(80, 10);
        assert!(line_text(&lines[0]).contains("[ ] Tab replies"), "{:?}", line_text(&lines[0]));
        let (lines, _) = picker(ALPHA).render(80, 10);
        assert!(!line_text(&lines[0]).contains("Tab"), "{:?}", line_text(&lines[0]));
    }

    fn many() -> ReplyPicker {
        picker("One. Two. Three.\n\nFour. Five.\n\nSix. Seven.")
    }

    fn ctrl(p: &mut ReplyPicker, c: char) -> ReplyResult {
        p.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL))
    }

    fn alt(p: &mut ReplyPicker, c: char) -> ReplyResult {
        p.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::ALT))
    }

    fn ch(p: &mut ReplyPicker, c: char) -> ReplyResult {
        press(p, KeyCode::Char(c))
    }

    fn at_unit(p: &ReplyPicker) -> usize {
        view(p).unit(view(p).cur)
    }

    #[test]
    fn emacs_keys_move_by_unit_word_and_edge() {
        let mut p = many();
        ctrl(&mut p, 'n');
        ctrl(&mut p, 'n');
        assert_eq!(at_unit(&p), 2);
        ctrl(&mut p, 'p');
        assert_eq!(at_unit(&p), 1);
        ctrl(&mut p, 'f');
        assert!(view(&p).word_mode && view(&p).cur == view(&p).span(1).0, "the first one reveals the word");
        ctrl(&mut p, 'f');
        assert_eq!(view(&p).cur, view(&p).span(1).0 + 1);
        alt(&mut p, 'b');
        assert_eq!(view(&p).cur, view(&p).span(1).0);
        ctrl(&mut p, 'e');
        assert_eq!(view(&p).cur, view(&p).span(1).1 - 1);
        ctrl(&mut p, 'a');
        assert_eq!(view(&p).cur, view(&p).span(1).0);
        alt(&mut p, '>');
        assert_eq!(view(&p).cur, view(&p).words() - 1);
        alt(&mut p, '<');
        assert_eq!(view(&p).cur, 0);
    }

    #[test]
    fn vim_keys_word_edge_top_bottom_and_blocks() {
        let mut p = many();
        ch(&mut p, 'w');
        ch(&mut p, 'w');
        assert_eq!(view(&p).cur, 1, "the first w reveals the word, the next moves");
        ch(&mut p, 'b');
        assert_eq!(view(&p).cur, 0);
        ch(&mut p, '$');
        assert_eq!(view(&p).cur, view(&p).span(0).1 - 1);
        ch(&mut p, '0');
        assert_eq!(view(&p).cur, 0);
        ch(&mut p, '}');
        assert_eq!(at_unit(&p), 3, "the next block starts at its first unit");
        ch(&mut p, '}');
        assert_eq!(at_unit(&p), 5);
        ch(&mut p, '{');
        assert_eq!(at_unit(&p), 3);
        ch(&mut p, 'G');
        assert_eq!(view(&p).cur, view(&p).words() - 1);
        ch(&mut p, 'g');
        assert_eq!(view(&p).cur, view(&p).words() - 1, "a single g waits for the second");
        ch(&mut p, 'g');
        assert_eq!(view(&p).cur, 0);
        ch(&mut p, 'g');
        ch(&mut p, 'j');
        assert_eq!(at_unit(&p), 1, "g then another key is not gg");
    }

    #[test]
    fn counts_repeat_a_motion_and_zero_without_a_count_is_the_unit_start() {
        let mut p = many();
        ch(&mut p, '3');
        ch(&mut p, 'j');
        assert_eq!(at_unit(&p), 3);
        ch(&mut p, '1');
        ch(&mut p, '0');
        ch(&mut p, 'k');
        assert_eq!(at_unit(&p), 0, "clamped at the top");
        ch(&mut p, '3');
        ch(&mut p, 'l');
        assert_eq!(view(&p).cur, 3, "a counted word move starts moving at once");
        ch(&mut p, '2');
        assert_eq!(press(&mut p, KeyCode::Esc), ReplyResult::Pending, "Esc drops a pending count first");
        ch(&mut p, 'j');
        assert_eq!(at_unit(&p), 4, "the dropped count does not apply");
    }

    #[test]
    fn q_and_ctrl_g_close_and_ctrl_g_drops_a_selection_first() {
        let mut p = many();
        shift(&mut p, KeyCode::Right);
        assert_eq!(ctrl(&mut p, 'g'), ReplyResult::Pending);
        assert!(view(&p).sel().is_none());
        assert_eq!(ctrl(&mut p, 'g'), ReplyResult::Close);
        assert_eq!(ch(&mut many(), 'q'), ReplyResult::Close);
    }

    #[test]
    fn half_and_full_pages_move_by_the_body_height() {
        let md: String = (1..=30).map(|k| format!("Para {k} sentence.\n\n")).collect();
        let mut p = picker(&md);
        p.render(40, 12);
        ctrl(&mut p, 'd');
        let half = at_unit(&p);
        assert!(half > 0, "{half}");
        ctrl(&mut p, 'u');
        assert_eq!(at_unit(&p), 0);
        ctrl(&mut p, 'v');
        assert!(at_unit(&p) > half);
        alt(&mut p, 'v');
        assert_eq!(at_unit(&p), 0);
    }

    #[test]
    fn the_window_gets_a_rule_above_and_below_and_uses_all_the_rows_it_is_given() {
        let md: String = (1..=40).map(|k| format!("Para {k} sentence.\n\n")).collect();
        let mut p = picker(&md);
        let (lines, _) = p.render(100, 30);
        let t: Vec<String> = lines.iter().map(line_text).collect();
        assert_eq!(t.len(), 30, "{t:?}");
        assert!(t[0].starts_with("── reply 1/1") && t[0].ends_with('─'), "{:?}", t[0]);
        assert!(t[27].chars().all(|c| c == '─'), "{:?}", t[27]);
        assert!(t[29].contains("Esc close"), "{t:?}");
    }
}
