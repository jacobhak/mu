//! Multi-line prompt buffer: editing keys, input history, paste markers, Ctrl-C state.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use unicode_width::UnicodeWidthChar;

use super::line::{Line, dim, plain, span};
use super::theme;
use crate::events::UserInput;
use crate::image;
use crate::message::Image;
use crossterm::style::ContentStyle;

const HISTORY_MAX: usize = 1000;
const PASTE_MAX_LINES: usize = 10;
const PASTE_MAX_CHARS: usize = 1000;
const PROMPT: &str = "μ ❯ ";
const INDENT: &str = "    ";
/// Display width of `PROMPT` and `INDENT`.
const PREFIX_W: usize = 4;
const CTRL_C_HINT: &str = "(press Ctrl-C again to exit)";
/// Blank cells left of the prompt.
const PAD: usize = 1;

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    None,
    Submit(UserInput),
    QueueFollowUp(UserInput),
    PasteImage,
    Abort,
    Exit,
    CycleThinking,
    ToggleThinking,
    ToggleExpanded,
    ModelPicker,
    EditExternal,
    PopQueue,
    Redraw,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Paste {
    marker: String,
    content: String,
}

/// Editable buffer. `lines` is never empty; `row`/`col` are char indices.
#[derive(Clone, Debug)]
struct Buf {
    lines: Vec<String>,
    row: usize,
    col: usize,
    pastes: Vec<Paste>,
    /// `(marker, image)` for each `[Image #n]` marker inserted.
    images: Vec<(String, Image)>,
}

impl Default for Buf {
    fn default() -> Self {
        Self { lines: vec![String::new()], row: 0, col: 0, pastes: vec![], images: vec![] }
    }
}

fn nch(s: &str) -> usize {
    s.chars().count()
}

fn bidx(s: &str, col: usize) -> usize {
    s.char_indices().nth(col).map_or(s.len(), |(b, _)| b)
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

impl Buf {
    fn from_text(s: &str) -> Self {
        let lines: Vec<String> = s.split('\n').map(str::to_owned).collect();
        let row = lines.len() - 1;
        let col = nch(&lines[row]);
        Self { lines, row, col, pastes: vec![], images: vec![] }
    }

    fn text(&self) -> String {
        self.lines.join("\n")
    }

    fn is_blank(&self) -> bool {
        self.lines.iter().all(|l| l.trim().is_empty())
    }

    fn cur(&self) -> &str {
        &self.lines[self.row]
    }

    fn cur_mut(&mut self) -> &mut String {
        &mut self.lines[self.row]
    }

    fn cur_len(&self) -> usize {
        nch(self.cur())
    }

    fn insert_str(&mut self, s: &str) {
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

    fn newline(&mut self) {
        let b = bidx(self.cur(), self.col);
        let rest = self.cur_mut().split_off(b);
        self.lines.insert(self.row + 1, rest);
        self.row += 1;
        self.col = 0;
    }

    /// Removes chars `from..to` on the current line; cursor goes to `from`.
    fn cut(&mut self, from: usize, to: usize) {
        let line = self.cur_mut();
        let b0 = bidx(line, from);
        let b1 = bidx(line, to);
        line.replace_range(b0..b1, "");
        self.col = from;
    }

    fn backspace(&mut self) {
        if self.col > 0 {
            self.cut(self.col - 1, self.col);
        } else if self.row > 0 {
            let cur = self.lines.remove(self.row);
            self.row -= 1;
            self.col = nch(&self.lines[self.row]);
            self.lines[self.row].push_str(&cur);
        }
    }

    fn delete(&mut self) {
        if self.col < self.cur_len() {
            self.cut(self.col, self.col + 1);
        } else if self.row + 1 < self.lines.len() {
            let next = self.lines.remove(self.row + 1);
            self.cur_mut().push_str(&next);
        }
    }

    fn kill_eol(&mut self) {
        if self.col < self.cur_len() {
            self.cut(self.col, self.cur_len());
        } else {
            self.delete();
        }
    }

    fn kill_bol(&mut self) {
        if self.col > 0 {
            self.cut(0, self.col);
        }
    }

    fn delete_word_back(&mut self) {
        if self.col == 0 {
            self.backspace();
        } else {
            let chars: Vec<char> = self.cur().chars().collect();
            let start = word_left_col(&chars, self.col);
            self.cut(start, self.col);
        }
    }

    fn left(&mut self) {
        if self.col > 0 {
            self.col -= 1;
        } else if self.row > 0 {
            self.row -= 1;
            self.col = self.cur_len();
        }
    }

    fn right(&mut self) {
        if self.col < self.cur_len() {
            self.col += 1;
        } else if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = 0;
        }
    }

    /// Caller guarantees `row > 0`.
    fn up(&mut self) {
        self.row -= 1;
        self.col = self.col.min(self.cur_len());
    }

    /// Caller guarantees `row + 1 < lines.len()`.
    fn down(&mut self) {
        self.row += 1;
        self.col = self.col.min(self.cur_len());
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
}

pub struct Composer {
    buf: Buf,
    history: Vec<String>,
    history_path: Option<PathBuf>,
    /// History index being browsed, and the draft it replaces.
    browse: Option<(usize, Buf)>,
    paste_count: usize,
    image_count: usize,
    ctrl_c_armed: bool,
}

impl Composer {
    pub fn new(history_path: Option<PathBuf>) -> Self {
        let mut history: Vec<String> = history_path
            .as_deref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str::<String>(l).ok())
            .collect();
        let skip = history.len().saturating_sub(HISTORY_MAX);
        history.drain(..skip);
        Self {
            buf: Buf::default(),
            history,
            history_path,
            browse: None,
            paste_count: 0,
            image_count: 0,
            ctrl_c_armed: false,
        }
    }

    pub fn on_key(&mut self, key: KeyEvent, running: bool, commands: &[String]) -> Action {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return Action::None;
        }
        // Any key other than Ctrl-C disarms the exit hint.
        let armed = std::mem::take(&mut self.ctrl_c_armed);
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        match key.code {
            KeyCode::Char('c') if ctrl => {
                if !self.is_empty() {
                    self.clear();
                    Action::Redraw
                } else if running {
                    Action::Abort
                } else if armed {
                    Action::Exit
                } else {
                    self.ctrl_c_armed = true;
                    Action::Redraw
                }
            }
            KeyCode::Char('d') if ctrl => Action::None,
            KeyCode::Enter if shift || alt => self.edit(Buf::newline),
            KeyCode::Enter => self.submit(false),
            KeyCode::Char('j') if ctrl => self.edit(Buf::newline),
            KeyCode::Char('q') if ctrl => self.submit(running),
            KeyCode::Up if alt => Action::PopQueue,
            KeyCode::Esc => {
                if running {
                    Action::Abort
                } else {
                    Action::None
                }
            }
            KeyCode::Tab => self.complete(commands),
            KeyCode::BackTab => Action::CycleThinking,
            KeyCode::Char('t') if ctrl => Action::ToggleThinking,
            KeyCode::Char('o') if ctrl => Action::ToggleExpanded,
            KeyCode::Char('m') if alt => Action::ModelPicker,
            KeyCode::Char('g') if ctrl => Action::EditExternal,
            KeyCode::Up => {
                if self.buf.row == 0 {
                    self.history_prev();
                } else {
                    self.mv(Buf::up);
                }
                Action::Redraw
            }
            KeyCode::Down => {
                if self.buf.row + 1 == self.buf.lines.len() {
                    self.history_next();
                } else {
                    self.mv(Buf::down);
                }
                Action::Redraw
            }
            KeyCode::Left => self.mv(Buf::left),
            KeyCode::Right => self.mv(Buf::right),
            KeyCode::Home => self.mv(Buf::home),
            KeyCode::End => self.mv(Buf::end),
            KeyCode::Char('a') if ctrl => self.mv(Buf::home),
            KeyCode::Char('e') if ctrl => self.mv(Buf::end),
            KeyCode::Char('b') if alt => self.mv(Buf::word_left),
            KeyCode::Char('f') if alt => self.mv(Buf::word_right),
            KeyCode::Backspace => self.edit(Buf::backspace),
            KeyCode::Delete => self.edit(Buf::delete),
            KeyCode::Char('k') if ctrl => self.edit(Buf::kill_eol),
            KeyCode::Char('u') if ctrl => self.edit(Buf::kill_bol),
            KeyCode::Char('w') if ctrl => self.edit(Buf::delete_word_back),
            KeyCode::Char('v') if ctrl => Action::PasteImage,
            KeyCode::Char(c) if !ctrl && !alt => self.edit(|b| b.insert_str(&c.to_string())),
            _ => Action::None,
        }
    }

    /// Pasted text. A paste made only of image-file paths attaches those images instead.
    pub fn on_paste(&mut self, s: &str) -> Result<(), String> {
        self.ctrl_c_armed = false;
        self.browse = None;
        if let Some(paths) = image::paths_in_paste(s) {
            for p in paths {
                let img = image::load(&p)?;
                self.attach_image(img);
                self.buf.insert_str(" ");
            }
            return Ok(());
        }
        let text = normalize_paste(s);
        let lines = text.matches('\n').count() + 1;
        let chars = text.chars().count();
        if lines > PASTE_MAX_LINES || chars > PASTE_MAX_CHARS {
            self.paste_count += 1;
            let n = self.paste_count;
            let marker = if lines > 1 {
                format!("[Paste #{n}, +{} lines]", lines - 1)
            } else {
                format!("[Paste #{n}, {chars} chars]")
            };
            self.buf.insert_str(&marker);
            self.buf.pastes.push(Paste { marker, content: text });
        } else {
            self.buf.insert_str(&text);
        }
        Ok(())
    }

    /// The `mu` composer: a blank line, then the soft-wrapped input behind a `μ ❯` prompt; cursor `(row, col)` inside it.
    /// An empty buffer shows a placeholder and a right-aligned command hint.
    pub fn render(&self, width: usize) -> (Vec<Line>, (usize, usize)) {
        let hint = "/ commands · ⇧⇥ thinking";
        let avail = width.saturating_sub(PAD + PREFIX_W).max(1);
        let mut rows: Vec<String> = Vec::new();
        let mut cursor = (0, 0);
        for (r, line) in self.buf.lines.iter().enumerate() {
            let start = rows.len();
            let mut wrapped = vec![String::new()];
            let mut col = 0;
            let on_cursor_line = r == self.buf.row;
            for (ci, ch) in line.chars().enumerate() {
                let w = ch.width().unwrap_or(0);
                if col > 0 && col + w > avail {
                    wrapped.push(String::new());
                    col = 0;
                }
                if on_cursor_line && ci == self.buf.col {
                    cursor = (start + wrapped.len() - 1, col);
                }
                wrapped.last_mut().unwrap().push(ch);
                col += w;
            }
            if on_cursor_line && self.buf.col == nch(line) {
                // The cursor cell needs one column; wrap if the line is full.
                if col + 1 > avail {
                    wrapped.push(String::new());
                    col = 0;
                }
                cursor = (start + wrapped.len() - 1, col);
            }
            rows.extend(wrapped);
        }
        let pad = " ".repeat(PAD);
        let accent = theme::get().fg("accent");
        let mut body: Vec<Line> = rows
            .iter()
            .enumerate()
            .map(|(i, t)| {
                if i == 0 {
                    vec![span(format!("{pad}{}", PROMPT.trim_end()), accent), span(format!(" {t}"), ContentStyle::default())]
                } else {
                    plain(format!("{pad}{INDENT}{t}"))
                }
            })
            .collect();
        if self.is_empty() {
            let used = PAD + PREFIX_W;
            let placeholder = "type here…";
            let ph_w = unicode_width::UnicodeWidthStr::width(placeholder);
            let h_w = unicode_width::UnicodeWidthStr::width(hint);
            let mut row = vec![span(format!("{pad}{}", PROMPT.trim_end()), accent), span(" ", ContentStyle::default()), span(placeholder, dim())];
            if used + ph_w + 2 + h_w <= width {
                row.push(span(" ".repeat(width - used - ph_w - h_w), ContentStyle::default()));
                row.push(span(hint, dim()));
            }
            body[0] = row;
        }
        if self.ctrl_c_armed {
            body.push(vec![span(format!("{pad}{CTRL_C_HINT}"), dim())]);
        }
        let mut out = Vec::with_capacity(body.len() + 1);
        out.push(Vec::new());
        out.extend(body);
        (out, (cursor.0 + 1, cursor.1 + PAD + PREFIX_W))
    }

    /// Raw buffer text; paste markers are unexpanded.
    pub fn text(&self) -> String {
        self.buf.text()
    }

    /// Replaces the buffer and keeps the cursor at the end. Paste and image markers still in the
    /// buffer remain expandable.
    pub fn set_text(&mut self, s: &str) {
        self.browse = None;
        let pastes = std::mem::take(&mut self.buf.pastes);
        let images = std::mem::take(&mut self.buf.images);
        self.buf = Buf::from_text(s);
        self.buf.pastes = pastes;
        self.buf.images = images;
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_blank()
    }

    /// Returns the buffer with paste markers expanded and the images whose markers survive, in
    /// order of first appearance; records the text in history and clears the composer.
    pub fn take_for_submit(&mut self) -> UserInput {
        let mut text = self.buf.text();
        for p in &self.buf.pastes {
            text = text.replace(&p.marker, &p.content);
        }
        let mut found: Vec<(usize, Image)> =
            std::mem::take(&mut self.buf.images).into_iter().filter_map(|(m, img)| text.find(&m).map(|at| (at, img))).collect();
        found.sort_by_key(|(at, _)| *at);
        self.push_history(&text);
        self.clear();
        UserInput { text, images: found.into_iter().map(|(_, img)| img).collect() }
    }

    /// Inserts an `[Image #n]` marker at the cursor and keeps the image for submission.
    pub fn attach_image(&mut self, img: Image) {
        self.ctrl_c_armed = false;
        self.browse = None;
        self.image_count += 1;
        let marker = format!("[Image #{}]", self.image_count);
        self.buf.insert_str(&marker);
        self.buf.images.push((marker, img));
    }

    /// Appends queued inputs back into the buffer (blank-line separated), re-attaching their images
    /// under fresh markers. Images pair with the markers in each item's text in order, as
    /// `take_for_submit` produced them.
    pub fn restore(&mut self, items: Vec<UserInput>) {
        let mut text = self.buf.text();
        let mut attached = Vec::new();
        for item in items {
            let mut t = String::new();
            let mut at = 0;
            let mut images = item.images.into_iter();
            for span in image_marker_spans(&item.text) {
                let Some(img) = images.next() else { break };
                self.image_count += 1;
                let marker = format!("[Image #{}]", self.image_count);
                t.push_str(&item.text[at..span.start]);
                t.push_str(&marker);
                at = span.end;
                attached.push((marker, img));
            }
            t.push_str(&item.text[at..]);
            if text.trim().is_empty() {
                text = t;
            } else if !t.trim().is_empty() {
                text = format!("{text}\n\n{t}");
            }
        }
        self.set_text(&text);
        self.buf.images.extend(attached);
    }

    fn clear(&mut self) {
        self.buf = Buf::default();
        self.browse = None;
        self.image_count = 0;
    }

    /// Applies an edit: leaves history browsing.
    fn edit(&mut self, f: impl FnOnce(&mut Buf)) -> Action {
        self.browse = None;
        f(&mut self.buf);
        Action::Redraw
    }

    /// Moves the cursor; keeps history browsing.
    fn mv(&mut self, f: impl FnOnce(&mut Buf)) -> Action {
        f(&mut self.buf);
        Action::Redraw
    }

    fn submit(&mut self, queue: bool) -> Action {
        if self.is_empty() {
            return Action::None;
        }
        let text = self.take_for_submit();
        if queue { Action::QueueFollowUp(text) } else { Action::Submit(text) }
    }

    fn push_history(&mut self, text: &str) {
        if text.trim().is_empty() || self.history.last().is_some_and(|h| h == text) {
            return;
        }
        if let Some(path) = &self.history_path {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            // Best effort: history persistence must not block the prompt.
            if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
                let _ = writeln!(f, "{}", serde_json::to_string(text).unwrap_or_default());
            }
        }
        self.history.push(text.to_owned());
        if self.history.len() > HISTORY_MAX {
            self.history.remove(0);
        }
    }

    fn history_prev(&mut self) {
        match self.browse.take() {
            None => {
                if self.history.is_empty() {
                    return;
                }
                let draft = std::mem::take(&mut self.buf);
                self.browse = Some((self.history.len() - 1, draft));
            }
            Some((i, draft)) => {
                self.browse = Some((i.saturating_sub(1), draft));
            }
        }
        self.load_browsed();
    }

    fn history_next(&mut self) {
        let Some((i, draft)) = self.browse.take() else { return };
        if i + 1 < self.history.len() {
            self.browse = Some((i + 1, draft));
            self.load_browsed();
        } else {
            self.buf = draft;
        }
    }

    fn load_browsed(&mut self) {
        if let Some((i, _)) = &self.browse {
            let mut buf = Buf::from_text(&self.history[*i]);
            buf.pastes.clear();
            buf.images.clear();
            self.buf = buf;
        }
    }

    fn complete(&mut self, commands: &[String]) -> Action {
        let text = self.buf.text();
        let Some(prefix) = text.strip_prefix('/') else { return Action::None };
        if prefix.contains(char::is_whitespace) {
            return Action::None;
        }
        let matches: Vec<&str> = commands
            .iter()
            .map(String::as_str)
            .filter(|c| c.starts_with(prefix))
            .collect();
        let completed = match matches.as_slice() {
            [] => return Action::None,
            [only] => format!("/{only} "),
            many => {
                let common = common_prefix(many);
                if common.len() <= prefix.len() {
                    return Action::None;
                }
                format!("/{common}")
            }
        };
        self.set_text(&completed);
        Action::Redraw
    }
}

/// Byte ranges of every `[Image #n]` marker in `text`.
fn image_marker_spans(text: &str) -> Vec<std::ops::Range<usize>> {
    let mut out = Vec::new();
    let mut at = 0;
    while let Some(i) = text[at..].find("[Image #") {
        let start = at + i;
        let digits = &text[start + 8..];
        let n = digits.bytes().take_while(u8::is_ascii_digit).count();
        if n > 0 && digits.as_bytes().get(n) == Some(&b']') {
            let end = start + 8 + n + 1;
            out.push(start..end);
            at = end;
        } else {
            at = start + 1;
        }
    }
    out
}

/// CR and CRLF become LF, tabs become three spaces, other control characters are dropped.
fn normalize_paste(s: &str) -> String {
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

/// Longest common char prefix of a non-empty slice.
fn common_prefix(xs: &[&str]) -> String {
    let mut acc = xs[0].to_owned();
    for x in &xs[1..] {
        let n = acc.chars().zip(x.chars()).take_while(|(a, b)| a == b).count();
        acc = acc.chars().take(n).collect();
    }
    acc
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEvent;

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    fn press(c: &mut Composer, code: KeyCode, mods: KeyModifiers) -> Action {
        c.on_key(key(code, mods), false, &[])
    }

    fn ctrl(c: &mut Composer, ch: char) -> Action {
        press(c, KeyCode::Char(ch), KeyModifiers::CONTROL)
    }

    fn type_str(c: &mut Composer, s: &str) {
        for ch in s.chars() {
            press(c, KeyCode::Char(ch), KeyModifiers::NONE);
        }
    }

    fn cmds() -> Vec<String> {
        ["exit", "exec", "model"].map(str::to_owned).to_vec()
    }

    fn temp_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mu-composer-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("history.jsonl")
    }

    #[test]
    fn word_kill_and_join_edits() {
        let mut c = Composer::new(None);
        type_str(&mut c, "foo bar baz");
        ctrl(&mut c, 'w');
        assert_eq!(c.text(), "foo bar ");
        ctrl(&mut c, 'w');
        assert_eq!(c.text(), "foo ");

        let mut c = Composer::new(None);
        type_str(&mut c, "x");
        press(&mut c, KeyCode::Enter, KeyModifiers::SHIFT);
        type_str(&mut c, "y");
        assert_eq!(c.text(), "x\ny");
        press(&mut c, KeyCode::Backspace, KeyModifiers::NONE);
        press(&mut c, KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(c.text(), "x");

        let mut c = Composer::new(None);
        c.set_text("ab\ncd");
        press(&mut c, KeyCode::Char('a'), KeyModifiers::CONTROL);
        ctrl(&mut c, 'k');
        assert_eq!(c.text(), "ab\n", "Ctrl+K at col 0 kills the line");

        let mut c = Composer::new(None);
        c.set_text("ab\ncd");
        for _ in 0..4 {
            press(&mut c, KeyCode::Left, KeyModifiers::NONE);
        }
        assert_eq!((c.buf.row, c.buf.col), (0, 1));
        ctrl(&mut c, 'k');
        assert_eq!(c.text(), "a\ncd", "Ctrl+K mid-line kills to end of line");
        ctrl(&mut c, 'k');
        assert_eq!(c.text(), "acd", "Ctrl+K at EOL joins the next line");

        let mut c = Composer::new(None);
        c.set_text("hello world");
        ctrl(&mut c, 'u');
        assert_eq!(c.text(), "", "Ctrl+U kills to line start");

        let mut c = Composer::new(None);
        c.set_text("one two");
        ctrl(&mut c, 'a');
        press(&mut c, KeyCode::Char('f'), KeyModifiers::ALT);
        type_str(&mut c, "X");
        assert_eq!(c.text(), "oneX two", "Alt+F stops at word end");
    }

    #[test]
    fn history_browse_restores_draft() {
        let path = temp_path("browse");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "\"first\"\n\"second\"\nnot json\n").unwrap();
        let mut c = Composer::new(Some(path));
        type_str(&mut c, "dra");
        press(&mut c, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(c.text(), "second");
        press(&mut c, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(c.text(), "first");
        press(&mut c, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(c.text(), "first", "stops at oldest");
        press(&mut c, KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(c.text(), "second");
        press(&mut c, KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(c.text(), "dra", "past newest restores draft");
        press(&mut c, KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(c.text(), "dra");
    }

    #[test]
    fn edit_exits_history_browsing() {
        let mut c = Composer::new(None);
        c.history = vec!["old".into()];
        type_str(&mut c, "dr");
        press(&mut c, KeyCode::Up, KeyModifiers::NONE);
        type_str(&mut c, "!");
        assert_eq!(c.text(), "old!");
        press(&mut c, KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(c.text(), "old!", "Down is a cursor move, not history, once edited");
    }

    #[test]
    fn submit_appends_history_file_and_reloads() {
        let path = temp_path("submit");
        let mut c = Composer::new(Some(path.clone()));
        type_str(&mut c, "hello \"there\"");
        assert_eq!(press(&mut c, KeyCode::Enter, KeyModifiers::NONE), Action::Submit("hello \"there\"".into()));
        assert!(c.is_empty());
        let c2 = Composer::new(Some(path));
        assert_eq!(c2.history, vec!["hello \"there\"".to_owned()]);
    }

    #[test]
    fn tab_completes_commands() {
        let mut c = Composer::new(None);
        let cm = cmds();
        type_str(&mut c, "/");
        assert_eq!(c.on_key(key(KeyCode::Tab, KeyModifiers::NONE), false, &cm), Action::None);
        assert_eq!(c.text(), "/");

        let mut c = Composer::new(None);
        type_str(&mut c, "/mo");
        c.on_key(key(KeyCode::Tab, KeyModifiers::NONE), false, &cm);
        assert_eq!(c.text(), "/model ");

        let mut c = Composer::new(None);
        type_str(&mut c, "/e");
        c.on_key(key(KeyCode::Tab, KeyModifiers::NONE), false, &cm);
        assert_eq!(c.text(), "/ex", "extends to common prefix without a space");
        assert_eq!(c.on_key(key(KeyCode::Tab, KeyModifiers::NONE), false, &cm), Action::None);
        assert_eq!(c.text(), "/ex");

        let mut c = Composer::new(None);
        type_str(&mut c, "/x");
        assert_eq!(c.on_key(key(KeyCode::Tab, KeyModifiers::NONE), false, &cm), Action::None);

        let mut c = Composer::new(None);
        type_str(&mut c, "/model arg");
        assert_eq!(c.on_key(key(KeyCode::Tab, KeyModifiers::NONE), false, &cm), Action::None);
        assert_eq!(c.text(), "/model arg");

        let mut c = Composer::new(None);
        type_str(&mut c, "hi");
        assert_eq!(c.on_key(key(KeyCode::Tab, KeyModifiers::NONE), false, &cm), Action::None);
        assert_eq!(c.text(), "hi");
    }

    #[test]
    fn ctrl_c_exit_state_machine() {
        let mut c = Composer::new(None);
        assert_eq!(ctrl(&mut c, 'c'), Action::Redraw);
        let (lines, _) = c.render(40);
        assert!(crate::tui::line::line_text(&lines[lines.len() - 1]).contains("press Ctrl-C again"));
        assert_eq!(ctrl(&mut c, 'c'), Action::Exit);

        let mut c = Composer::new(None);
        ctrl(&mut c, 'c');
        press(&mut c, KeyCode::Left, KeyModifiers::NONE);
        assert_eq!(ctrl(&mut c, 'c'), Action::Redraw, "other key resets the armed state");
        assert_eq!(ctrl(&mut c, 'c'), Action::Exit);

        let mut c = Composer::new(None);
        ctrl(&mut c, 'c');
        c.on_paste("y").unwrap();
        assert_eq!(ctrl(&mut c, 'c'), Action::Redraw, "paste resets the armed state");
        assert!(c.is_empty());

        let mut c = Composer::new(None);
        type_str(&mut c, "x");
        assert_eq!(ctrl(&mut c, 'c'), Action::Redraw);
        assert!(c.is_empty());
        assert_eq!(ctrl(&mut c, 'c'), Action::Redraw, "first press on empty only arms");

        let mut c = Composer::new(None);
        assert_eq!(c.on_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL), true, &[]), Action::Abort);
        type_str(&mut c, "x");
        assert_eq!(c.on_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL), true, &[]), Action::Redraw);
        assert!(c.is_empty());
    }

    #[test]
    fn paste_normalizes_and_marks_large_pastes() {
        let mut c = Composer::new(None);
        c.on_paste("a\r\nb\rc\td\u{7}e").unwrap();
        assert_eq!(c.text(), "a\nb\nc   de");

        let mut c = Composer::new(None);
        let body: Vec<String> = (0..12).map(|i| format!("line {i}")).collect();
        let original = body.join("\n");
        c.on_paste(&original).unwrap();
        assert_eq!(c.text(), "[Paste #1, +11 lines]");
        assert_eq!(press(&mut c, KeyCode::Enter, KeyModifiers::NONE), Action::Submit(original.into()));

        let mut c = Composer::new(None);
        c.on_paste(&"x".repeat(1500)).unwrap();
        assert_eq!(c.text(), "[Paste #1, 1500 chars]");
        c.on_paste("tail").unwrap();
        assert_eq!(c.text(), "[Paste #1, 1500 chars]tail");
        // Two pastes get distinct numbers.
        c.on_paste(&"y".repeat(1500)).unwrap();
        assert_eq!(c.text(), "[Paste #1, 1500 chars]tail[Paste #2, 1500 chars]");

        // Editing a marker makes it literal text; the broken marker is not expanded.
        let mut c = Composer::new(None);
        c.on_paste(&"z".repeat(1500)).unwrap();
        press(&mut c, KeyCode::Backspace, KeyModifiers::NONE);
        match press(&mut c, KeyCode::Enter, KeyModifiers::NONE) {
            Action::Submit(s) => assert_eq!(s.text, "[Paste #1, 1500 chars"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn soft_wrap_cursor_with_wide_chars() {
        let mut c = Composer::new(None);
        type_str(&mut c, "日本語日本");
        // width 11 => 6 cells of text per row after the 1-cell pad and 4-cell prompt.
        let (lines, cur) = c.render(11);
        let texts: Vec<String> = lines.iter().map(crate::tui::line::line_text).collect();
        assert_eq!(texts, ["".to_string(), " μ ❯ 日本語".into(), "     日本".into()]);
        assert_eq!(cur, (2, 9));

        press(&mut c, KeyCode::Left, KeyModifiers::NONE);
        press(&mut c, KeyCode::Left, KeyModifiers::NONE);
        assert_eq!(c.render(11).1, (2, 5), "before 日 at char index 3");

        press(&mut c, KeyCode::Right, KeyModifiers::NONE);
        assert_eq!(c.render(11).1, (2, 7), "after 日");
    }

    #[test]
    fn render_empty_and_enter_guard() {
        let mut c = Composer::new(None);
        let (lines, cur) = c.render(20);
        assert_eq!(lines.len(), 2, "blank line, one input row");
        assert_eq!(cur, (1, 5));
        type_str(&mut c, "   ");
        assert_eq!(press(&mut c, KeyCode::Enter, KeyModifiers::NONE), Action::None);
        assert_eq!(c.text(), "   ");
    }

    #[test]
    fn queue_and_abort_keys() {
        let mut c = Composer::new(None);
        type_str(&mut c, "a");
        assert_eq!(c.on_key(key(KeyCode::Char('q'), KeyModifiers::CONTROL), true, &[]), Action::QueueFollowUp("a".into()));
        type_str(&mut c, "b");
        assert_eq!(c.on_key(key(KeyCode::Char('q'), KeyModifiers::CONTROL), false, &[]), Action::Submit("b".into()));
        assert_eq!(c.on_key(key(KeyCode::Up, KeyModifiers::ALT), true, &[]), Action::PopQueue);
        assert_eq!(c.on_key(key(KeyCode::Esc, KeyModifiers::NONE), true, &[]), Action::Abort);
        assert_eq!(c.on_key(key(KeyCode::Esc, KeyModifiers::NONE), false, &[]), Action::None);
        assert_eq!(c.on_key(key(KeyCode::BackTab, KeyModifiers::SHIFT), false, &[]), Action::CycleThinking);
    }

    fn png() -> Image {
        Image { data: "QUJD".into(), mime_type: "image/png".into() }
    }

    #[test]
    fn submit_keeps_only_images_whose_markers_remain_in_order() {
        let mut c = Composer::new(None);
        c.attach_image(png());
        type_str(&mut c, " mid ");
        let mut second = png();
        second.data = "REVG".into();
        c.attach_image(second.clone());
        c.attach_image(png());
        // Delete the third marker by editing it.
        press(&mut c, KeyCode::Backspace, KeyModifiers::NONE);
        let Action::Submit(input) = press(&mut c, KeyCode::Enter, KeyModifiers::NONE) else { panic!() };
        assert_eq!(input.text, "[Image #1] mid [Image #2][Image #3");
        assert_eq!(input.images, vec![png(), second]);
        assert!(c.is_empty());
    }

    #[test]
    fn restore_renumbers_images_against_existing_buffer() {
        let mut c = Composer::new(None);
        c.attach_image(png());
        let mut other = png();
        other.data = "REVG".into();
        let queued = UserInput { text: "see [Image #1] and [Image #2]".into(), images: vec![other.clone(), other.clone()] };
        c.restore(vec![queued]);
        assert_eq!(c.text(), "[Image #1]\n\nsee [Image #2] and [Image #3]");
        let input = c.take_for_submit();
        assert_eq!(input.images, vec![png(), other.clone(), other]);
    }
}
