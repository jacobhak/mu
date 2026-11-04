//! The text buffer behind every line the user types into: the composer, the ask picker's free-text
//! answer, its note and its filter. It owns the editing shortcuts, so those cannot drift apart.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::line::{self, Line, plain};
use crate::text::grapheme_width;

/// What a key did to the text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edit {
    /// Not an editing key: the caller may handle it.
    Unhandled,
    Moved,
    Changed,
}

/// Lines of text with a cursor (`row`, `col` in chars) and a kill buffer that Ctrl-Y yanks.
/// `lines` is never empty.
#[derive(Clone, Debug)]
pub struct Editor {
    pub(super) lines: Vec<String>,
    pub(super) row: usize,
    pub(super) col: usize,
    kill: String,
    /// Drawn as one `•` per char (a secret being typed). The text is kept as typed.
    pub masked: bool,
}

impl Default for Editor {
    fn default() -> Self {
        Editor { lines: vec![String::new()], row: 0, col: 0, kill: String::new(), masked: false }
    }
}

/// CR and CRLF become LF, tabs become three spaces, other control characters are dropped.
pub fn normalize_paste(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push('\n');
            }
            '\n' => out.push('\n'),
            '\t' => out.push_str("   "),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

fn nch(s: &str) -> usize {
    s.chars().count()
}

fn bidx(s: &str, col: usize) -> usize {
    s.char_indices().nth(col).map_or(s.len(), |(b, _)| b)
}

/// Char index of the grapheme boundary before `col` (0 at the start).
fn prev_boundary(s: &str, col: usize) -> usize {
    let mut at = 0;
    for (b, g) in s.grapheme_indices(true) {
        let start = s[..b].chars().count();
        let end = start + g.chars().count();
        if end >= col {
            return if start < col { start } else { at };
        }
        at = start;
    }
    at.min(col)
}

/// Char index of the grapheme boundary after `col` (the line end at most).
fn next_boundary(s: &str, col: usize) -> usize {
    for (b, g) in s.grapheme_indices(true) {
        let start = s[..b].chars().count();
        let end = start + g.chars().count();
        if end > col {
            return end;
        }
    }
    nch(s)
}

/// `col` moved back to the start of the cluster it is inside.
fn snap_boundary(s: &str, col: usize) -> usize {
    let mut last = 0;
    for (b, g) in s.grapheme_indices(true) {
        let start = s[..b].chars().count();
        if start > col {
            break;
        }
        last = start;
        if start + g.chars().count() > col {
            return start;
        }
    }
    last.min(col)
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Start of the word left of `col`, not crossing line boundaries.
fn word_left_col(chars: &[char], col: usize) -> usize {
    let mut c = col;
    while c > 0 && !is_word(chars[c - 1]) {
        c -= 1;
    }
    while c > 0 && is_word(chars[c - 1]) {
        c -= 1;
    }
    c
}

fn word_right_col(chars: &[char], col: usize) -> usize {
    let n = chars.len();
    let mut c = col;
    while c < n && !is_word(chars[c]) {
        c += 1;
    }
    while c < n && is_word(chars[c]) {
        c += 1;
    }
    c
}

impl Editor {
    pub fn from_text(s: &str) -> Self {
        let lines: Vec<String> = s.split('\n').map(str::to_owned).collect();
        let row = lines.len() - 1;
        let col = nch(&lines[row]);
        Editor { lines, row, col, kill: String::new(), masked: false }
    }

    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    pub fn is_blank(&self) -> bool {
        self.lines.iter().all(|l| l.trim().is_empty())
    }

    pub fn is_empty(&self) -> bool {
        self.lines.len() == 1 && self.lines[0].is_empty()
    }

    pub fn cur(&self) -> &str {
        &self.lines[self.row]
    }

    fn cur_mut(&mut self) -> &mut String {
        &mut self.lines[self.row]
    }

    pub fn cur_len(&self) -> usize {
        nch(self.cur())
    }

    pub fn insert_str(&mut self, s: &str) {
        for (i, part) in s.split('\n').enumerate() {
            if i > 0 {
                self.newline();
            }
            let col = self.col;
            let line = self.cur_mut();
            let b = bidx(line, col);
            line.insert_str(b, part);
            self.col += nch(part);
        }
    }

    pub fn newline(&mut self) {
        let b = bidx(self.cur(), self.col);
        let rest = self.cur_mut().split_off(b);
        self.lines.insert(self.row + 1, rest);
        self.row += 1;
        self.col = 0;
    }

    /// Removes chars `from..to` on the current line, returns them; the cursor goes to `from`.
    fn cut(&mut self, from: usize, to: usize) -> String {
        let line = self.cur_mut();
        let b0 = bidx(line, from);
        let b1 = bidx(line, to);
        let removed = line[b0..b1].to_string();
        line.replace_range(b0..b1, "");
        self.col = from;
        removed
    }

    fn backspace(&mut self) {
        if self.col > 0 {
            let from = prev_boundary(self.cur(), self.col);
            self.cut(from, self.col);
        } else if self.row > 0 {
            let cur = self.lines.remove(self.row);
            self.row -= 1;
            self.col = nch(&self.lines[self.row]);
            self.lines[self.row].push_str(&cur);
        }
    }

    fn delete(&mut self) {
        if self.col < self.cur_len() {
            let to = next_boundary(self.cur(), self.col);
            self.cut(self.col, to);
        } else if self.row + 1 < self.lines.len() {
            let next = self.lines.remove(self.row + 1);
            self.cur_mut().push_str(&next);
        }
    }

    fn kill_eol(&mut self) {
        if self.col < self.cur_len() {
            self.kill = self.cut(self.col, self.cur_len());
        } else {
            self.delete();
        }
    }

    fn kill_bol(&mut self) {
        if self.col > 0 {
            self.kill = self.cut(0, self.col);
        }
    }

    fn delete_word_back(&mut self) {
        if self.col == 0 {
            self.backspace();
        } else {
            let chars: Vec<char> = self.cur().chars().collect();
            let start = word_left_col(&chars, self.col);
            self.kill = self.cut(start, self.col);
        }
    }

    fn delete_word_fwd(&mut self) {
        if self.col >= self.cur_len() {
            self.delete();
        } else {
            let chars: Vec<char> = self.cur().chars().collect();
            let end = word_right_col(&chars, self.col);
            self.kill = self.cut(self.col, end);
        }
    }

    fn yank(&mut self) {
        let k = self.kill.clone();
        self.insert_str(&k);
    }

    fn left(&mut self) {
        if self.col > 0 {
            self.col = prev_boundary(self.cur(), self.col);
        } else if self.row > 0 {
            self.row -= 1;
            self.col = self.cur_len();
        }
    }

    fn right(&mut self) {
        if self.col < self.cur_len() {
            self.col = next_boundary(self.cur(), self.col);
        } else if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = 0;
        }
    }

    /// Caller guarantees `row > 0`.
    fn up(&mut self) {
        self.row -= 1;
        self.col = snap_boundary(self.cur(), self.col.min(self.cur_len()));
    }

    /// Caller guarantees `row + 1 < lines.len()`.
    fn down(&mut self) {
        self.row += 1;
        self.col = snap_boundary(self.cur(), self.col.min(self.cur_len()));
    }

    fn home(&mut self) {
        self.col = 0;
    }

    fn end(&mut self) {
        self.col = self.cur_len();
    }

    fn word_left(&mut self) {
        while self.col == 0 && self.row > 0 {
            self.row -= 1;
            self.col = self.cur_len();
        }
        let chars: Vec<char> = self.cur().chars().collect();
        self.col = word_left_col(&chars, self.col);
    }

    fn word_right(&mut self) {
        while self.col == self.cur_len() && self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = 0;
        }
        let chars: Vec<char> = self.cur().chars().collect();
        self.col = word_right_col(&chars, self.col);
    }

    fn mv(&mut self, f: fn(&mut Self)) -> Edit {
        f(self);
        Edit::Moved
    }

    fn ed(&mut self, f: fn(&mut Self)) -> Edit {
        f(self);
        Edit::Changed
    }

    /// Applies an editing key. Enter (Alt/Shift) and Ctrl-J make a newline only when `multiline`;
    /// plain Enter is never handled here, the caller submits on it.
    pub fn key(&mut self, key: KeyEvent, multiline: bool) -> Edit {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        match key.code {
            KeyCode::Enter if multiline && (shift || alt) => self.ed(Self::newline),
            KeyCode::Char('j') if ctrl && multiline => self.ed(Self::newline),
            KeyCode::Char(c) if !ctrl && !alt => {
                self.insert_str(&c.to_string());
                Edit::Changed
            }
            KeyCode::Left => self.mv(Self::left),
            KeyCode::Char('b') if ctrl => self.mv(Self::left),
            KeyCode::Right => self.mv(Self::right),
            KeyCode::Char('f') if ctrl => self.mv(Self::right),
            KeyCode::Char('b') if alt => self.mv(Self::word_left),
            KeyCode::Char('f') if alt => self.mv(Self::word_right),
            KeyCode::Home => self.mv(Self::home),
            KeyCode::Char('a') if ctrl => self.mv(Self::home),
            KeyCode::End => self.mv(Self::end),
            KeyCode::Char('e') if ctrl => self.mv(Self::end),
            KeyCode::Up if self.row > 0 => self.mv(Self::up),
            KeyCode::Char('p') if ctrl && self.row > 0 => self.mv(Self::up),
            KeyCode::Down if self.row + 1 < self.lines.len() => self.mv(Self::down),
            KeyCode::Char('n') if ctrl && self.row + 1 < self.lines.len() => self.mv(Self::down),
            KeyCode::Backspace if alt => self.ed(Self::delete_word_back),
            KeyCode::Backspace => self.ed(Self::backspace),
            KeyCode::Char('h') if ctrl => self.ed(Self::backspace),
            KeyCode::Delete => self.ed(Self::delete),
            KeyCode::Char('d') if ctrl => self.ed(Self::delete),
            KeyCode::Char('d') if alt => self.ed(Self::delete_word_fwd),
            KeyCode::Char('w') if ctrl => self.ed(Self::delete_word_back),
            KeyCode::Char('k') if ctrl => self.ed(Self::kill_eol),
            KeyCode::Char('u') if ctrl => self.ed(Self::kill_bol),
            KeyCode::Char('y') if ctrl => self.ed(Self::yank),
            _ => Edit::Unhandled,
        }
    }

    /// The text as drawn: a masked buffer shows one `•` per char, so columns stay char indices.
    fn drawn(&self) -> std::borrow::Cow<'_, Editor> {
        if !self.masked {
            return std::borrow::Cow::Borrowed(self);
        }
        let lines = self.lines.iter().map(|l| "•".repeat(l.chars().count())).collect();
        std::borrow::Cow::Owned(Editor { lines, row: self.row, col: self.col, kill: String::new(), masked: false })
    }

    /// The text wrapped to `avail` columns, and the cursor (row, col) among the rows. A cursor past
    /// a full row takes a row of its own, so it always has a cell.
    pub fn wrap(&self, avail: usize) -> (Vec<String>, (usize, usize)) {
        let avail = avail.max(1);
        let mut rows: Vec<String> = Vec::new();
        let mut cursor = (0, 0);
        let d = self.drawn();
        for (r, text) in d.lines.iter().enumerate() {
            let start = rows.len();
            let lay = line::layout(&plain(text.as_str()), avail);
            let mut wrapped: Vec<String> = lay.rows.iter().map(line::line_text).collect();
            if r == d.row {
                cursor = lay.locate_char(d.col);
                if d.col >= lay.chars && cursor.1 >= avail {
                    wrapped.push(String::new());
                    cursor = (cursor.0 + 1, 0);
                }
                cursor.0 += start;
            }
            rows.extend(wrapped);
        }
        (rows, cursor)
    }

    /// The text as rows: `first` on the first row, the same width indented under it on the rest.
    /// Wrapped to `width`, and scrolled so the cursor sits in the `max_rows` shown (at least one).
    /// Returns the rows and the cursor within them.
    pub fn view(&self, first: &str, width: usize, max_rows: usize) -> (Vec<Line>, (usize, usize)) {
        let pw = UnicodeWidthStr::width(first).min(width.saturating_sub(1));
        let (rows, cur) = self.wrap(width.saturating_sub(pw).max(1));
        let max = max_rows.max(1);
        let start = cur.0.saturating_sub(max - 1).min(rows.len().saturating_sub(max));
        let end = (start + max).min(rows.len());
        let indent = " ".repeat(pw);
        let out = rows[start..end]
            .iter()
            .enumerate()
            .map(|(i, t)| plain(format!("{}{t}", if start + i == 0 { first } else { indent.as_str() })))
            .collect();
        (out, (cur.0 - start, (pw + cur.1).min(width.saturating_sub(1))))
    }

    /// The first line as one row after `first`, scrolled sideways so the cursor stays in view.
    /// For single-line fields: only the first line is shown.
    pub fn view_line(&self, first: &str, width: usize) -> (Line, usize) {
        let pw = UnicodeWidthStr::width(first).min(width.saturating_sub(1));
        let avail = width.saturating_sub(pw).max(1);
        let d = self.drawn();
        let text = d.lines[0].as_str();
        let cur = line::layout(&plain(text), usize::MAX).locate_char(d.col).1;
        let scroll_from = (cur + 1).saturating_sub(avail);
        let mut disp = 0;
        let mut base = None;
        let mut shown = String::new();
        let mut used = 0;
        for g in text.graphemes(true) {
            let w = grapheme_width(g);
            if disp < scroll_from {
                disp += w;
                continue;
            }
            if used + w > avail {
                break;
            }
            base.get_or_insert(disp);
            shown.push_str(g);
            used += w;
            disp += w;
        }
        let base = base.unwrap_or(scroll_from);
        (plain(format!("{first}{shown}")), pw + (cur - base).min(avail - 1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEvent;
    use mu_testutil::key_mods as key;

    fn ctrl(c: char) -> KeyEvent {
        key(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn alt(code: KeyCode) -> KeyEvent {
        key(code, KeyModifiers::ALT)
    }

    /// Feeds `keys` to `e` as a multi-line field.
    fn press(e: &mut Editor, keys: &[KeyEvent]) {
        for k in keys {
            e.key(*k, true);
        }
    }

    fn at(text: &str) -> Editor {
        Editor::from_text(text)
    }

    #[test]
    fn editing_moves_by_grapheme_cluster() {
        // "e" + combining acute, a family emoji (ZWJ sequence), a flag.
        let mut b = at("e\u{301}x👨‍👩‍👧🇸🇪");
        b.backspace();
        assert_eq!(b.cur(), "e\u{301}x👨‍👩‍👧");
        b.left();
        assert_eq!(b.col, 3, "left lands before the whole family emoji");
        b.left();
        assert_eq!(b.col, 2);
        b.left();
        assert_eq!(b.col, 0, "the accented e is one step");
        b.delete();
        assert_eq!(b.cur(), "x👨‍👩‍👧");
        b.end();
        b.backspace();
        assert_eq!(b.cur(), "x");
    }

    #[test]
    fn movement_keys_cover_the_composer_set() {
        let mut e = at("ab cd\nef");
        press(&mut e, &[key(KeyCode::Left, KeyModifiers::NONE), key(KeyCode::Left, KeyModifiers::NONE)]);
        assert_eq!((e.row, e.col), (1, 0));
        press(&mut e, &[ctrl('e')]);
        assert_eq!((e.row, e.col), (1, 2));
        press(&mut e, &[ctrl('a')]);
        assert_eq!((e.row, e.col), (1, 0));
        press(&mut e, &[ctrl('f')]);
        assert_eq!((e.row, e.col), (1, 1));
        press(&mut e, &[ctrl('b')]);
        press(&mut e, &[key(KeyCode::Right, KeyModifiers::NONE)]);
        press(&mut e, &[key(KeyCode::End, KeyModifiers::NONE), key(KeyCode::Home, KeyModifiers::NONE)]);
        assert_eq!((e.row, e.col), (1, 0));
        press(&mut e, &[key(KeyCode::Up, KeyModifiers::NONE)]);
        assert_eq!((e.row, e.col), (0, 0), "up moves between lines");
        press(&mut e, &[key(KeyCode::Up, KeyModifiers::NONE)]);
        assert_eq!((e.row, e.col), (0, 0), "up on the first line does nothing");
        press(&mut e, &[ctrl('n')]);
        assert_eq!(e.row, 1);
    }

    #[test]
    fn word_jumps_stop_at_word_edges() {
        let mut e = at("alpha beta, gamma");
        press(&mut e, &[alt(KeyCode::Char('b'))]);
        assert_eq!(e.col, 12, "before gamma");
        press(&mut e, &[alt(KeyCode::Char('b'))]);
        assert_eq!(e.col, 6, "before beta");
        press(&mut e, &[alt(KeyCode::Char('f'))]);
        assert_eq!(e.col, 10, "after beta");
        press(&mut e, &[alt(KeyCode::Char('f'))]);
        assert_eq!(e.col, 17, "the end");
    }

    #[test]
    fn word_jumps_cross_line_breaks() {
        let mut e = at("one\ntwo");
        press(&mut e, &[alt(KeyCode::Char('b'))]);
        assert_eq!((e.row, e.col), (1, 0));
        press(&mut e, &[alt(KeyCode::Char('b'))]);
        assert_eq!((e.row, e.col), (0, 0));
        press(&mut e, &[alt(KeyCode::Char('f'))]);
        assert_eq!((e.row, e.col), (0, 3), "forward lands at the end of one");
        press(&mut e, &[alt(KeyCode::Char('f'))]);
        assert_eq!((e.row, e.col), (1, 3), "and crosses the break to the end of two");
    }

    #[test]
    fn delete_keys_and_kill_yank() {
        let mut e = at("alpha beta gamma");
        press(&mut e, &[alt(KeyCode::Backspace)]);
        assert_eq!(e.text(), "alpha beta ");
        press(&mut e, &[ctrl('w')]);
        assert_eq!(e.text(), "alpha ");
        press(&mut e, &[ctrl('y')]);
        assert_eq!(e.text(), "alpha beta ", "yank brings back the last kill");
        press(&mut e, &[ctrl('u')]);
        assert_eq!(e.text(), "", "Ctrl-U kills to the line start");
        press(&mut e, &[ctrl('y')]);
        assert_eq!(e.text(), "alpha beta ");
        let mut e = at("keep cut tail");
        press(&mut e, &[key(KeyCode::Home, KeyModifiers::NONE), alt(KeyCode::Char('d'))]);
        assert_eq!(e.text(), " cut tail");
        press(&mut e, &[ctrl('k')]);
        assert_eq!(e.text(), "", "Ctrl-K kills to the line end");
        press(&mut e, &[ctrl('y')]);
        assert_eq!(e.text(), " cut tail");
        let mut e = at("ab");
        press(&mut e, &[key(KeyCode::Left, KeyModifiers::NONE), key(KeyCode::Delete, KeyModifiers::NONE)]);
        assert_eq!(e.text(), "a");
        press(&mut e, &[ctrl('h')]);
        assert_eq!(e.text(), "", "Ctrl-H after Delete removes the char before the cursor");
    }

    #[test]
    fn newlines_only_where_multi_line() {
        let mut e = at("a");
        assert_eq!(e.key(alt(KeyCode::Enter), true), Edit::Changed);
        assert_eq!(e.key(key(KeyCode::Enter, KeyModifiers::SHIFT), true), Edit::Changed);
        assert_eq!(e.key(ctrl('j'), true), Edit::Changed);
        assert_eq!(e.text(), "a\n\n\n");
        assert_eq!(
            e.key(key(KeyCode::Enter, KeyModifiers::NONE), true),
            Edit::Unhandled,
            "plain Enter is the caller's"
        );
        let mut single = at("a");
        assert_eq!(single.key(alt(KeyCode::Enter), false), Edit::Unhandled);
        assert_eq!(single.text(), "a");
    }

    #[test]
    fn paste_keeps_line_breaks_as_text() {
        let mut e = at("x");
        e.insert_str(&normalize_paste("a\r\nb\rc\td"));
        assert_eq!(e.text(), "xa\nb\nc   d");
        assert_eq!(normalize_paste("p\u{7}q"), "pq", "control characters are dropped");
    }

    #[test]
    fn wrap_places_the_cursor_after_wide_characters() {
        let e = at("ab字字");
        let (rows, cur) = e.wrap(3);
        assert_eq!(rows, ["ab", "字", "字"]);
        assert_eq!(cur, (2, 2), "the cursor is after the last wide char");
        let (rows, cur) = e.wrap(1);
        assert_eq!(rows, ["a", "b", "字", "字", ""]);
        assert_eq!(cur, (4, 0), "a cursor past a full row takes a row; a wide char alone at width 1");
    }

    #[test]
    fn wrap_gives_a_cursor_past_a_full_row_its_own_row() {
        let e = at("abcd");
        assert_eq!(e.wrap(4), (vec!["abcd".to_string(), String::new()], (1, 0)));
        let mut e = at("abcd");
        e.key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE), true);
        assert_eq!(e.wrap(4).1, (0, 3));
    }

    #[test]
    fn wrap_at_one_and_two_columns_never_loses_the_cursor() {
        for width in [1, 2] {
            for text in ["", "a", "ab", "字", "a字b字", "e\u{301}e\u{301}"] {
                let mut e = at(text);
                for _ in 0..=text.chars().count() {
                    let (rows, cur) = e.wrap(width);
                    assert!(cur.0 < rows.len(), "width {width} text {text:?} cursor {cur:?} rows {rows:?}");
                    e.key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE), true);
                }
            }
        }
    }

    #[test]
    fn view_wraps_under_the_prompt_and_scrolls_to_the_cursor() {
        let mut e = at("abcdefghijklmnop");
        let (lines, cur) = e.view("› ", 8, 10);
        let t: Vec<String> = lines.iter().map(line::line_text).collect();
        assert_eq!(t, ["› abcdef", "  ghijkl", "  mnop"]);
        assert_eq!(cur, (2, 6));
        let (lines, cur) = e.view("› ", 8, 2);
        let t: Vec<String> = lines.iter().map(line::line_text).collect();
        assert_eq!(t, ["  ghijkl", "  mnop"], "the cursor's row stays in the window");
        assert_eq!(cur, (1, 6));
        e.key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE), true);
        let (lines, cur) = e.view("› ", 8, 2);
        assert_eq!(line::line_text(&lines[0]), "› abcdef", "scrolling back up shows the top");
        assert_eq!(cur, (0, 2));
    }

    #[test]
    fn view_line_scrolls_sideways_to_keep_the_cursor() {
        let mut e = at("0123456789");
        let (line, col) = e.view_line("› ", 6);
        assert_eq!(line::line_text(&line), "› 789", "the cursor at the end sits on the last column");
        assert_eq!(col, 5);
        e.key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE), true);
        let (line, col) = e.view_line("› ", 6);
        assert_eq!(line::line_text(&line), "› 0123");
        assert_eq!(col, 2);
        let e = at("字字字字");
        let (line, col) = e.view_line("› ", 6);
        assert!(col < 6 && line::line_width(&line) <= 6, "{col} {:?}", line::line_text(&line));
    }

    #[test]
    fn masked_buffer_draws_bullets_and_keeps_the_text() {
        let mut e = at("sk-ant-123");
        e.masked = true;
        let (rows, cur) = e.wrap(40);
        assert_eq!(rows, ["•".repeat(10)], "one bullet per char");
        assert_eq!(cur, (0, 10));
        let (line, col) = e.view_line("› ", 40);
        assert_eq!(line::line_text(&line), format!("› {}", "•".repeat(10)));
        assert_eq!(col, 12);
        assert_eq!(e.text(), "sk-ant-123", "the buffer keeps the text");
    }
}
