//! Multi-line prompt buffer: editing keys, input history, paste markers, Ctrl-C state.

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use unicode_width::UnicodeWidthChar;

use super::complete::{self, Entry};
use super::editor::{Edit, Editor, normalize_paste};
use super::line::{self, Line, plain, span};
use super::theme::{self, Role};
use crossterm::style::ContentStyle;
use mu_base::image;
use mu_base::message::{Image, UserInput};

const HISTORY_MAX: usize = 1000;
/// The history file is rewritten down to `HISTORY_MAX` lines once it passes this many.
const HISTORY_COMPACT_AT: usize = 4 * HISTORY_MAX;

/// The last `n` non-empty lines of a file, read by seeking back from the end.
fn tail_lines(path: &std::path::Path, n: usize) -> Vec<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(path) else { return vec![] };
    let Ok(mut pos) = f.seek(SeekFrom::End(0)) else { return vec![] };
    let mut data: Vec<u8> = Vec::new();
    // Need n+1 newlines (or the start of the file) so the first kept line is whole.
    while pos > 0 && data.iter().filter(|b| **b == b'\n').count() <= n {
        let step = pos.min(64 * 1024);
        pos -= step;
        let mut chunk = vec![0u8; step as usize];
        if f.seek(SeekFrom::Start(pos)).is_err() || f.read_exact(&mut chunk).is_err() {
            return vec![];
        }
        chunk.extend_from_slice(&data);
        data = chunk;
    }
    let mut lines: Vec<Vec<u8>> = data.split(|b| *b == b'\n').filter(|l| !l.is_empty()).map(<[u8]>::to_vec).collect();
    if pos > 0 && !lines.is_empty() {
        lines.remove(0); // the first line may be cut at the chunk boundary
    }
    let skip = lines.len().saturating_sub(n);
    lines.drain(..skip);
    lines
}

/// Lines that never go into the history file: `/mcp add` carries env values, headers and urls that
/// can hold tokens.
fn withheld(text: &str) -> bool {
    let mut words = text.split_whitespace();
    words.next() == Some("/mcp") && words.next() == Some("add")
}

fn slash_usage(history: &[String]) -> HashMap<String, u32> {
    let mut usage: HashMap<String, u32> = HashMap::new();
    for h in history {
        if let Some(name) = h.split_whitespace().next().and_then(|t| t.strip_prefix('/')) {
            *usage.entry(name.to_owned()).or_default() += 1;
        }
    }
    usage
}

fn count_lines(path: &std::path::Path) -> usize {
    use std::io::BufRead;
    let Ok(f) = std::fs::File::open(path) else { return 0 };
    let mut r = std::io::BufReader::new(f);
    let (mut n, mut buf) = (0, Vec::new());
    while r.read_until(b'\n', &mut buf).is_ok_and(|k| k > 0) {
        n += 1;
        buf.clear();
    }
    n
}

/// Reads the last `HISTORY_MAX` lines. A file that has grown past `HISTORY_COMPACT_AT` lines is
/// first rewritten, atomically, down to those lines.
fn load_history(path: &std::path::Path) -> Vec<Vec<u8>> {
    let lines = tail_lines(path, HISTORY_MAX);
    // Counting is only worth doing for a file big enough to plausibly exceed the limit.
    let big = std::fs::metadata(path).is_ok_and(|m| m.len() > (HISTORY_COMPACT_AT as u64) * 40);
    if big && count_lines(path) > HISTORY_COMPACT_AT {
        let mut out = Vec::new();
        for l in &lines {
            out.extend_from_slice(l);
            out.push(b'\n');
        }
        let _ = mu_common::fs::write_atomic(path, &out);
    }
    lines
}
const PASTE_MAX_LINES: usize = 10;
const PASTE_MAX_CHARS: usize = 1000;
const PROMPT: &str = "μ ❯ ";
const INDENT: &str = "    ";
/// Display width of `PROMPT` and `INDENT`.
const PREFIX_W: usize = 4;
const MENU_ROWS: usize = 8;
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
    Reply,
    Jobs,
    PopQueue,
    Redraw,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Paste {
    marker: String,
    content: String,
}

/// The composer's buffer: the shared editor's text plus the paste markers and images it carries.
#[derive(Clone, Debug, Default)]
struct Buf {
    ed: Editor,
    pastes: Vec<Paste>,
    /// `(marker, image)` for each `[Image #n]` marker inserted.
    images: Vec<(String, Image)>,
}

impl Deref for Buf {
    type Target = Editor;
    fn deref(&self) -> &Editor {
        &self.ed
    }
}

impl DerefMut for Buf {
    fn deref_mut(&mut self) -> &mut Editor {
        &mut self.ed
    }
}

impl Buf {
    fn from_text(s: &str) -> Self {
        Buf { ed: Editor::from_text(s), pastes: vec![], images: vec![] }
    }
}

pub struct Composer {
    buf: Buf,
    history: Vec<String>,
    /// How often each slash command appears in `history`; rebuilt only when history changes.
    slash_usage: HashMap<String, u32>,
    history_path: Option<PathBuf>,
    /// History index being browsed, and the draft it replaces.
    browse: Option<(usize, Buf)>,
    paste_count: usize,
    image_count: usize,
    ctrl_c_armed: bool,
    cwd: String,
    ghost: Option<String>,
    menu: Option<Menu>,
    /// Text for which the user dismissed the menu with Esc.
    menu_closed_for: Option<String>,
    /// Nothing submitted is recorded in the history (the `/mcp add` wizard is collecting answers).
    private: bool,
    /// A command is being typed (see `set_closed`).
    closed: bool,
}

/// Slash-command menu. `items` are completion tokens (command or argument names).
struct Menu {
    items: Vec<String>,
    descs: Vec<String>,
    needs_arg: Vec<bool>,
    sel: usize,
    /// Completing an argument of `cmd`.
    arg: Option<String>,
    /// The typed token already equals an exact command (or argument).
    exact: bool,
}

impl Composer {
    pub fn new(history_path: Option<PathBuf>) -> Self {
        let mut history: Vec<String> = history_path
            .as_deref()
            .map(load_history)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|l| match serde_json::from_slice::<serde_json::Value>(&l).ok()? {
                serde_json::Value::String(s) => Some(s),
                serde_json::Value::Object(o) => o.get("text").and_then(|t| t.as_str()).map(String::from),
                _ => None,
            })
            .collect();
        let skip = history.len().saturating_sub(HISTORY_MAX);
        history.drain(..skip);
        Self {
            buf: Buf::default(),
            slash_usage: slash_usage(&history),
            history,
            history_path,
            browse: None,
            paste_count: 0,
            image_count: 0,
            ctrl_c_armed: false,
            cwd: String::new(),
            ghost: None,
            menu: None,
            menu_closed_for: None,
            private: false,
            closed: false,
        }
    }

    /// Records `cwd` with each prompt written to the history file.
    pub fn with_cwd(mut self, cwd: &Path) -> Self {
        self.cwd = cwd.to_string_lossy().into_owned();
        self
    }

    /// While on, submitted text stays out of the history, in memory and on disk.
    pub fn set_private(&mut self, on: bool) {
        self.private = on;
    }

    /// A command is being typed: its text stays out of history and prediction (see `set_private`), and
    /// the keys that send it elsewhere (external editor, clipboard, reply search, completion) are off.
    pub fn set_closed(&mut self, on: bool) {
        self.clear();
        self.closed = on;
        self.menu = None;
        self.menu_closed_for = None;
    }

    /// A key or command is being typed: the buffer must not take text from elsewhere.
    fn locked(&self) -> bool {
        self.buf.masked || self.closed
    }

    /// A secret is being typed: the buffer is drawn as bullets and nothing typed leaves the composer
    /// (no history, prediction, completion, external editor, clipboard or image paste).
    pub fn set_masked(&mut self, on: bool) {
        self.clear();
        self.buf.masked = on;
        self.menu = None;
        self.menu_closed_for = None;
    }

    /// The typed text with paste markers expanded (a secret is never a marker; a command may be).
    pub fn secret_text(&self) -> String {
        let mut text = self.buf.text();
        for p in &self.buf.pastes {
            text = text.replace(&p.marker, &p.content);
        }
        text
    }

    /// The typed text, cleared from the buffer without going to the history.
    pub fn take_secret(&mut self) -> String {
        let text = self.secret_text();
        self.clear();
        text
    }

    pub fn on_key(&mut self, key: KeyEvent, running: bool, commands: &[Entry]) -> Action {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return Action::None;
        }
        let plain_mods = !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT);
        let ctrl_only = key.modifiers == KeyModifiers::CONTROL;
        let ghost = self.ghost.take();
        let action = 'act: {
            if self.menu.is_some() {
                if let Some(a) = self.menu_key(&key, plain_mods, ctrl_only) {
                    break 'act a;
                }
            } else if key.code == KeyCode::Tab {
                self.ctrl_c_armed = false;
                break 'act match ghost {
                    Some(g) => self.accept_ghost(&g),
                    None => Action::None,
                };
            } else if let (KeyCode::Right, Some(g)) = (key.code, ghost.as_deref())
                && plain_mods
                && self.at_end()
            {
                self.ctrl_c_armed = false;
                break 'act self.accept_ghost(g);
            }
            let a = self.on_key_inner(key, running);
            if let (KeyCode::Char(c), Some(g)) = (key.code, ghost.as_deref())
                && !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                && let Some(rest) = g.strip_prefix(c).filter(|r| !r.is_empty())
            {
                self.ghost = Some(rest.to_owned());
            }
            a
        };
        self.refresh_menu(commands);
        action
    }

    fn at_end(&self) -> bool {
        self.buf.row + 1 == self.buf.lines.len() && self.buf.col == self.buf.cur_len()
    }

    fn accept_ghost(&mut self, g: &str) -> Action {
        self.edit(|b| {
            b.insert_str(g);
        })
    }

    /// Stores a prediction for `text_for` if the buffer still holds exactly that with the cursor at its end.
    pub fn set_ghost(&mut self, text_for: &str, g: String) {
        if !g.is_empty() && self.menu.is_none() && !self.locked() && self.at_end() && self.text() == text_for {
            self.ghost = Some(g);
        }
    }

    /// The buffer text worth predicting a continuation for, if any.
    pub fn predict_query(&self) -> Option<String> {
        let t = self.text();
        let ok = self.at_end()
            && !self.buf.is_blank()
            && !t.starts_with('/')
            && !t.starts_with('!')
            && self.browse.is_none()
            && self.menu.is_none()
            && !self.locked();
        ok.then_some(t)
    }

    pub fn has_ghost(&self) -> bool {
        self.ghost.is_some()
    }

    /// Menu keys; `None` falls through to normal handling.
    fn menu_key(&mut self, key: &KeyEvent, plain_mods: bool, ctrl_only: bool) -> Option<Action> {
        let m = self.menu.as_mut()?;
        let n = m.items.len();
        match key.code {
            KeyCode::Up if plain_mods => m.sel = (m.sel + n - 1) % n,
            KeyCode::Down if plain_mods => m.sel = (m.sel + 1) % n,
            KeyCode::Char('p') if ctrl_only => m.sel = (m.sel + n - 1) % n,
            KeyCode::Char('n') if ctrl_only => m.sel = (m.sel + 1) % n,
            KeyCode::Tab => {
                let text = self.menu_text(false);
                self.set_text(&text);
                self.menu_closed_for = Some(text);
            }
            KeyCode::Enter if plain_mods => {
                let (exact, arg, needs) = (m.exact, m.arg.is_some(), m.needs_arg[m.sel]);
                if exact {
                    return None;
                }
                if arg {
                    let text = self.menu_text(false);
                    self.set_text(&text);
                } else if needs {
                    let text = self.menu_text(true);
                    self.set_text(&text);
                    return Some(Action::Redraw);
                } else {
                    let text = self.menu_text(false);
                    self.set_text(&text);
                }
                return Some(self.submit(false));
            }
            KeyCode::Esc => {
                self.menu_closed_for = Some(self.text());
                self.menu = None;
            }
            _ => return None,
        }
        self.ctrl_c_armed = false;
        Some(Action::Redraw)
    }

    /// The selected completion as buffer text, optionally with a trailing space.
    fn menu_text(&self, space: bool) -> String {
        let m = self.menu.as_ref().expect("menu open");
        let item = &m.items[m.sel];
        let mut t = match &m.arg {
            Some(cmd) => format!("/{cmd} {item}"),
            None => format!("/{item}"),
        };
        if space {
            t.push(' ');
        }
        t
    }

    fn refresh_menu(&mut self, commands: &[Entry]) {
        let text = self.text();
        if self.menu_closed_for.as_ref().is_some_and(|c| *c != text) {
            self.menu_closed_for = None;
        }
        let new = self.build_menu(&text, commands);
        self.menu = match (new, self.menu.take()) {
            (Some(mut n), Some(old)) if old.items == n.items && old.arg == n.arg => {
                n.sel = old.sel.min(n.items.len() - 1);
                Some(n)
            }
            (n, _) => n,
        };
    }

    fn build_menu(&self, text: &str, commands: &[Entry]) -> Option<Menu> {
        if self.buf.lines.len() != 1
            || self.browse.is_some()
            || self.locked()
            || self.menu_closed_for.as_deref() == Some(text)
        {
            return None;
        }
        let body = text.strip_prefix('/')?;
        let (items, descs, needs_arg, arg, exact) = match body.split_once(char::is_whitespace) {
            None => {
                let idx = complete::rank(commands, body, &self.slash_usage);
                let pick = |f: fn(&Entry) -> String| idx.iter().map(|&i| f(&commands[i])).collect::<Vec<_>>();
                let exact = commands.iter().any(|c| c.name == body);
                (
                    pick(|e| e.name.clone()),
                    pick(|e| e.desc.clone()),
                    idx.iter().map(|&i| commands[i].needs_arg).collect::<Vec<_>>(),
                    None,
                    exact,
                )
            }
            Some((name, partial)) => {
                if partial.contains(char::is_whitespace) {
                    return None;
                }
                let e = commands.iter().find(|c| c.name == name && !c.args.is_empty())?;
                let lower = partial.to_lowercase();
                let hits: Vec<usize> =
                    (0..e.args.len()).filter(|&i| e.args[i].to_lowercase().starts_with(&lower)).collect();
                let items: Vec<String> = hits.iter().map(|&i| e.args[i].clone()).collect();
                let descs: Vec<String> =
                    hits.iter().map(|&i| e.arg_descs.get(i).cloned().unwrap_or_default()).collect();
                let n = items.len();
                let exact = e.args.iter().any(|a| a == partial);
                (items, descs, vec![false; n], Some(name.to_owned()), exact)
            }
        };
        (!items.is_empty()).then_some(Menu { items, descs, needs_arg, sel: 0, arg, exact })
    }

    fn on_key_inner(&mut self, key: KeyEvent, running: bool) -> Action {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return Action::None;
        }
        // Any key other than Ctrl-C disarms the exit hint.
        let armed = std::mem::take(&mut self.ctrl_c_armed);
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
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
            KeyCode::Char('d') if ctrl && self.is_empty() => Action::None,
            KeyCode::Enter if key.modifiers.intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) && !self.buf.masked => {
                self.edit(|b| b.newline())
            }
            KeyCode::Enter => self.submit(false),
            KeyCode::Char('q') if ctrl => self.submit(running),
            KeyCode::Up if alt => Action::PopQueue,
            KeyCode::Esc if self.buf.masked => {
                self.clear();
                Action::Redraw
            }
            KeyCode::Esc => {
                if running {
                    Action::Abort
                } else {
                    Action::None
                }
            }
            KeyCode::BackTab => Action::CycleThinking,
            KeyCode::Char('t') if ctrl => Action::ToggleThinking,
            KeyCode::Char('o') if ctrl => Action::ToggleExpanded,
            KeyCode::Char('m') if alt => Action::ModelPicker,
            KeyCode::Char('g') if ctrl && !self.locked() => Action::EditExternal,
            KeyCode::Char('r') if ctrl && self.menu.is_none() && !self.locked() => Action::Reply,
            KeyCode::Char('j') if alt && !ctrl => Action::Jobs,
            KeyCode::Up | KeyCode::Char('p') if !matches!(key.code, KeyCode::Char(_)) || ctrl => {
                if self.buf.row == 0 {
                    self.history_prev();
                } else {
                    self.buf.key(key, true);
                }
                Action::Redraw
            }
            KeyCode::Down | KeyCode::Char('n') if !matches!(key.code, KeyCode::Char(_)) || ctrl => {
                if self.buf.row + 1 == self.buf.lines.len() {
                    self.history_next();
                } else {
                    self.buf.key(key, true);
                }
                Action::Redraw
            }
            KeyCode::Char('v') if ctrl && !self.locked() => Action::PasteImage,
            _ => match self.buf.key(key, true) {
                Edit::Changed => self.edit(|_| {}),
                Edit::Moved => Action::Redraw,
                Edit::Unhandled => Action::None,
            },
        }
    }

    /// Pasted text. A paste made only of image-file paths attaches those images instead.
    pub fn on_paste(&mut self, s: &str, commands: &[Entry]) -> Result<(), String> {
        let r = self.paste_inner(s);
        self.refresh_menu(commands);
        r
    }

    fn paste_inner(&mut self, s: &str) -> Result<(), String> {
        self.ghost = None;
        self.ctrl_c_armed = false;
        self.browse = None;
        if self.locked() {
            self.buf.insert_str(&normalize_paste(s));
            return Ok(());
        }
        if let Some(paths) = image::paths_in_paste(s) {
            // Load everything first: a failure part-way must not leave some of the paste attached.
            let imgs = paths.iter().map(|p| image::load(p)).collect::<Result<Vec<_>, _>>()?;
            for img in imgs {
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
    /// An empty buffer shows a placeholder.
    pub fn render(&self, width: usize) -> (Vec<Line>, (usize, usize)) {
        let avail = width.saturating_sub(PAD + PREFIX_W).max(1);
        let (rows, cursor) = self.buf.wrap(avail);
        let pad = " ".repeat(PAD);
        let accent = theme::get().fg(Role::Accent);
        let mut body: Vec<Line> = rows
            .iter()
            .enumerate()
            .map(|(i, t)| {
                if i == 0 {
                    vec![
                        span(format!("{pad}{}", PROMPT.trim_end()), accent),
                        span(format!(" {t}"), ContentStyle::default()),
                    ]
                } else {
                    plain(format!("{pad}{INDENT}{t}"))
                }
            })
            .collect();
        let faint = theme::get().fg(Role::Faint);
        if let Some(g) = self.ghost.as_deref().filter(|_| !self.is_empty()) {
            let mut left = avail.saturating_sub(cursor.1);
            let mut shown = String::new();
            for ch in g.chars() {
                let w = ch.width().unwrap_or(0);
                if w > left {
                    break;
                }
                left -= w;
                shown.push(ch);
            }
            if let Some(row) = body.get_mut(cursor.0).filter(|_| !shown.is_empty()) {
                row.push(span(shown, faint));
            }
        }
        if self.is_empty() {
            body[0] = vec![
                span(format!("{pad}{}", PROMPT.trim_end()), accent),
                span(" ", ContentStyle::default()),
                span("type here…", theme::get().fg(Role::Faint)),
            ];
        }
        if self.ctrl_c_armed {
            body.push(vec![span(format!("{pad}{CTRL_C_HINT}"), theme::get().fg(Role::Faint))]);
        }
        if let Some(m) = &self.menu {
            let n = m.items.len();
            let start = m.sel.saturating_sub(MENU_ROWS - 1).min(n.saturating_sub(MENU_ROWS));
            for i in start..n.min(start + MENU_ROWS) {
                let (head, desc) = match &m.arg {
                    Some(_) => (format!("{pad}  {}", m.items[i]), &m.descs[i]),
                    None => (format!("{pad}  /{}", m.items[i]), &m.descs[i]),
                };
                let mut row: Line = vec![span(head, ContentStyle::default())];
                if !desc.is_empty() {
                    row.push(span(format!("  {desc}"), theme::get().fg(Role::Faint)));
                }
                row = line::truncate(&row, width);
                if i == m.sel {
                    row = line::fill(&row, width, theme::get().color(Role::Selected));
                }
                body.push(row);
            }
        }
        let mut out = Vec::with_capacity(body.len() + 1);
        out.push(vec![span("─".repeat(width), theme::get().fg(Role::LineFaint))]);
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
        self.ghost = None;
        let pastes = std::mem::take(&mut self.buf.pastes);
        let images = std::mem::take(&mut self.buf.images);
        let masked = self.buf.masked;
        self.buf = Buf::from_text(s);
        self.buf.masked = masked;
        self.buf.pastes = pastes;
        self.buf.images = images;
    }

    /// Appends quote blocks after the text (a blank line apart) and puts the cursor on the row
    /// right under the first one, ready for the comment. The transcript ends a quote at the first
    /// line without `>` (see `blocks::end_quotes`), so the comment is not part of it.
    pub fn append_quotes(&mut self, quotes: &[String]) {
        let old = self.text();
        let mut text = if old.trim().is_empty() { String::new() } else { format!("{}\n\n", old.trim_end()) };
        let base = text.matches('\n').count();
        let first = quotes.first().map_or(0, |q| q.matches('\n').count() + 1);
        for q in quotes {
            text.push_str(q);
            text.push_str("\n\n");
        }
        self.set_text(&text);
        self.menu = None;
        self.buf.row = base + first;
        self.buf.col = 0;
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
        let mut found: Vec<(usize, Image)> = std::mem::take(&mut self.buf.images)
            .into_iter()
            .filter_map(|(m, img)| text.find(&m).map(|at| (at, img)))
            .collect();
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
    /// `take_for_submit` produced them. Handed back, untouched, while the buffer is locked.
    pub fn restore(&mut self, items: Vec<UserInput>) -> Result<(), Vec<UserInput>> {
        if self.locked() {
            return Err(items);
        }
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
        Ok(())
    }

    fn clear(&mut self) {
        self.ghost = None;
        let masked = self.buf.masked;
        self.buf = Buf::default();
        self.buf.masked = masked;
        self.browse = None;
        self.image_count = 0;
    }

    /// Applies an edit: leaves history browsing.
    fn edit(&mut self, f: impl FnOnce(&mut Buf)) -> Action {
        self.browse = None;
        f(&mut self.buf);
        Action::Redraw
    }

    fn submit(&mut self, queue: bool) -> Action {
        if self.locked() {
            return Action::None;
        }
        if self.is_empty() {
            return Action::None;
        }
        let text = self.take_for_submit();
        if queue { Action::QueueFollowUp(text) } else { Action::Submit(text) }
    }

    fn push_history(&mut self, text: &str) {
        if self.private || withheld(text) || text.trim().is_empty() || self.history.last().is_some_and(|h| h == text) {
            return;
        }
        if let Some(path) = &self.history_path {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            // Best effort: history persistence must not block the prompt.
            if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
                let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
                let row = serde_json::json!({ "text": text, "cwd": self.cwd, "ts": ts });
                let _ = writeln!(f, "{row}");
            }
        }
        self.history.push(text.to_owned());
        if self.history.len() > HISTORY_MAX {
            self.history.remove(0);
        }
        self.slash_usage = slash_usage(&self.history);
    }

    fn history_prev(&mut self) {
        if self.locked() {
            return;
        }
        self.ghost = None;
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
        if self.locked() {
            return;
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use mu_testutil::key_mods as key;

    fn press(c: &mut Composer, code: KeyCode, mods: KeyModifiers) -> Action {
        c.on_key(key(code, mods), false, &[])
    }

    fn ctrl(c: &mut Composer, ch: char) -> Action {
        press(c, KeyCode::Char(ch), KeyModifiers::CONTROL)
    }

    #[test]
    fn alt_j_opens_the_jobs_panel_and_plain_or_ctrl_j_do_not() {
        let mut c = Composer::new(None);
        assert_eq!(press(&mut c, KeyCode::Char('j'), KeyModifiers::ALT), Action::Jobs);
        assert_ne!(press(&mut c, KeyCode::Char('j'), KeyModifiers::CONTROL), Action::Jobs);
        assert_ne!(press(&mut c, KeyCode::Char('j'), KeyModifiers::NONE), Action::Jobs);
        assert_eq!(c.text(), "\nj", "Ctrl+J is still a newline");
    }

    #[test]
    fn quotes_go_after_the_text_with_the_cursor_right_under_the_first() {
        let mut c = Composer::new(None);
        c.set_text("hi");
        c.append_quotes(&["> a".to_string(), "> b\n> c".to_string()]);
        assert_eq!(c.text(), "hi\n\n> a\n\n> b\n> c\n\n");
        assert_eq!((c.buf.row, c.buf.col), (3, 0));
        let mut e = Composer::new(None);
        e.append_quotes(&["> x".to_string()]);
        assert_eq!(e.text(), "> x\n\n");
        assert_eq!((e.buf.row, e.buf.col), (1, 0));
    }

    fn type_str(c: &mut Composer, s: &str) {
        for ch in s.chars() {
            press(c, KeyCode::Char(ch), KeyModifiers::NONE);
        }
    }

    fn entry(name: &str, needs_arg: bool, args: &[&str]) -> Entry {
        Entry {
            name: name.into(),
            desc: format!("{name} desc"),
            needs_arg,
            args: args.iter().map(|s| s.to_string()).collect(),
            arg_descs: vec![],
        }
    }

    fn cmds() -> Vec<Entry> {
        vec![
            entry("exit", false, &[]),
            entry("model", false, &[]),
            entry("rename", true, &[]),
            entry("thinking", false, &["off", "low", "medium", "high"]),
        ]
    }

    fn tab(c: &mut Composer) -> Action {
        c.on_key(key(KeyCode::Tab, KeyModifiers::NONE), false, &cmds())
    }

    fn pr(c: &mut Composer, code: KeyCode) -> Action {
        c.on_key(key(code, KeyModifiers::NONE), false, &cmds())
    }

    fn typ(c: &mut Composer, s: &str) {
        for ch in s.chars() {
            pr(c, KeyCode::Char(ch));
        }
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
    fn secrets_stay_out_of_the_history() {
        let path = temp_path("private");
        let mut c = Composer::new(Some(path.clone()));
        for line in ["/mcp add h --url http://x/mcp -H \"Authorization: Bearer s3cret\"", "  /mcp   add"] {
            type_str(&mut c, line);
            assert!(matches!(press(&mut c, KeyCode::Enter, KeyModifiers::NONE), Action::Submit(_)));
        }
        c.set_private(true);
        type_str(&mut c, "Bearer s3cret");
        assert!(matches!(press(&mut c, KeyCode::Enter, KeyModifiers::NONE), Action::Submit(_)));
        c.set_private(false);
        for line in ["/mcp list", "/mcp remove h", "hello"] {
            type_str(&mut c, line);
            press(&mut c, KeyCode::Enter, KeyModifiers::NONE);
        }
        assert_eq!(c.history, ["/mcp list", "/mcp remove h", "hello"]);
        let file = std::fs::read_to_string(&path).unwrap();
        assert!(!file.contains("s3cret") && !file.contains("add"), "{file}");
        assert_eq!(Composer::new(Some(path)).history.len(), 3);
    }

    #[test]
    fn slash_menu() {
        let mut c = Composer::new(None);
        typ(&mut c, "/mo");
        assert_eq!(c.menu.as_ref().unwrap().items[0], "model");
        let (lines, _) = c.render(40);
        assert!(crate::tui::line::line_text(&lines[lines.len() - 1]).contains("/model  model desc"));
        pr(&mut c, KeyCode::Down);
        assert_eq!(c.text(), "/mo");
        assert_eq!(pr(&mut c, KeyCode::Enter), Action::Submit("/model".into()));

        let mut c = Composer::new(None);
        typ(&mut c, "/ren");
        assert_eq!(pr(&mut c, KeyCode::Enter), Action::Redraw);
        assert_eq!(c.text(), "/rename ");

        let mut c = Composer::new(None);
        typ(&mut c, "/thinking h");
        tab(&mut c);
        assert_eq!(c.text(), "/thinking high");
        assert!(c.menu.is_none());

        let mut c = Composer::new(None);
        typ(&mut c, "/thin");
        tab(&mut c);
        assert_eq!(c.text(), "/thinking");
        assert!(c.menu.is_none());
        typ(&mut c, " ");
        assert_eq!(c.menu.as_ref().and_then(|m| m.arg.as_deref()), Some("thinking"));

        let mut c = Composer::new(None);
        typ(&mut c, "/thinking h");
        assert_eq!(pr(&mut c, KeyCode::Enter), Action::Submit("/thinking high".into()));

        let mut c = Composer::new(None);
        typ(&mut c, "/model");
        assert_eq!(pr(&mut c, KeyCode::Enter), Action::Submit("/model".into()), "exact name submits");

        let mut c = Composer::new(None);
        typ(&mut c, "/mo");
        assert_eq!(c.on_key(key(KeyCode::Esc, KeyModifiers::NONE), true, &cmds()), Action::Redraw);
        assert!(c.menu.is_none());
        c.on_key(key(KeyCode::Down, KeyModifiers::NONE), false, &cmds());
        assert!(c.menu.is_none(), "stays closed until an edit");
        typ(&mut c, "d");
        assert!(c.menu.is_some());

        let mut c = Composer::new(None);
        typ(&mut c, "hello");
        assert!(c.menu.is_none());
        assert_eq!(tab(&mut c), Action::None);
        assert_eq!(c.text(), "hello");
    }

    #[test]
    fn ghost_text() {
        let mut c = Composer::new(None);
        typ(&mut c, "fix the bu");
        c.set_ghost("fix the b", "ug".into());
        assert!(c.ghost.is_none(), "stale text_for is ignored");
        c.set_ghost("fix the bu", "g".into());
        typ(&mut c, "g");
        assert!(c.ghost.is_none());
        assert_eq!(c.text(), "fix the bug");

        c.set_text("fix the bu");
        c.set_ghost("fix the bu", "gfix".into());
        typ(&mut c, "g");
        assert_eq!(c.ghost.as_deref(), Some("fix"), "typing the ghost's first char projects it");
        typ(&mut c, "x");
        assert!(c.ghost.is_none());

        c.set_text("please com");
        c.set_ghost("please com", "mit".into());
        tab(&mut c);
        assert_eq!(c.text(), "please commit");

        c.set_text("please com");
        c.set_ghost("please com", "mit".into());
        pr(&mut c, KeyCode::Right);
        assert_eq!(c.text(), "please commit");

        c.set_text("a b");
        c.set_ghost("a b", " c d".into());
        let (lines, _) = c.render(40);
        assert!(crate::tui::line::line_text(&lines[1]).ends_with("a b c d"));
        tab(&mut c);
        assert_eq!(c.text(), "a b c d");

        let mut c = Composer::new(None);
        c.history = vec!["old".into()];
        typ(&mut c, "x");
        c.set_ghost("x", "yz".into());
        pr(&mut c, KeyCode::Up);
        assert!(c.ghost.is_none());
        assert_eq!(c.predict_query(), None, "browsing history");
        let mut c = Composer::new(None);
        typ(&mut c, "hi");
        assert_eq!(c.predict_query().as_deref(), Some("hi"));
        c.set_text("/x");
        assert_eq!(c.predict_query(), None);
    }

    #[test]
    fn masked_input_draws_bullets_and_never_leaves_the_composer() {
        let mut c = Composer::new(None);
        c.set_masked(true);
        type_str(&mut c, "sk-secret");
        let (lines, _) = c.render(60);
        let drawn: String = lines.iter().map(crate::tui::line::line_text).collect();
        assert!(drawn.contains(&"•".repeat(9)), "{drawn}");
        assert!(!drawn.contains("secret"), "{drawn}");
        assert_eq!(c.text(), "sk-secret", "the buffer keeps the text");
        assert_eq!(c.predict_query(), None, "nothing reaches the predict plugin");
        c.set_ghost("sk-secret", "x".into());
        assert!(!c.has_ghost());
        press(&mut c, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(c.text(), "sk-secret", "no history browsing");
        assert_eq!(press(&mut c, KeyCode::Enter, KeyModifiers::NONE), Action::None, "submitting is the app's");
        assert_eq!(ctrl(&mut c, 'g'), Action::None, "no external editor");
        assert_eq!(press(&mut c, KeyCode::Esc, KeyModifiers::NONE), Action::Redraw);
        assert!(c.is_empty(), "Esc clears the secret");
        type_str(&mut c, "k2");
        assert_eq!(c.take_secret(), "k2");
        assert!(c.is_empty());
    }

    #[test]
    fn history_loads_both_formats() {
        let path = temp_path("formats");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "\"legacy\"\n{\"text\":\"new\",\"cwd\":\"/x\",\"ts\":5}\n").unwrap();
        let c = Composer::new(Some(path.clone()));
        assert_eq!(c.history, ["legacy", "new"]);
        let mut c = c.with_cwd(Path::new("/w"));
        c.push_history("third");
        let last = std::fs::read_to_string(&path).unwrap().lines().last().unwrap().to_owned();
        let v: serde_json::Value = serde_json::from_str(&last).unwrap();
        assert_eq!((v["text"].as_str(), v["cwd"].as_str()), (Some("third"), Some("/w")));
        assert!(v["ts"].as_u64().unwrap() > 0);
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
        c.on_paste("y", &[]).unwrap();
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
        c.on_paste("a\r\nb\rc\td\u{7}e", &[]).unwrap();
        assert_eq!(c.text(), "a\nb\nc   de");

        let mut c = Composer::new(None);
        let body: Vec<String> = (0..12).map(|i| format!("line {i}")).collect();
        let original = body.join("\n");
        c.on_paste(&original, &[]).unwrap();
        assert_eq!(c.text(), "[Paste #1, +11 lines]");
        assert_eq!(press(&mut c, KeyCode::Enter, KeyModifiers::NONE), Action::Submit(original.into()));

        let mut c = Composer::new(None);
        c.on_paste(&"x".repeat(1500), &[]).unwrap();
        assert_eq!(c.text(), "[Paste #1, 1500 chars]");
        c.on_paste("tail", &[]).unwrap();
        assert_eq!(c.text(), "[Paste #1, 1500 chars]tail");
        // Two pastes get distinct numbers.
        c.on_paste(&"y".repeat(1500), &[]).unwrap();
        assert_eq!(c.text(), "[Paste #1, 1500 chars]tail[Paste #2, 1500 chars]");

        // Editing a marker makes it literal text; the broken marker is not expanded.
        let mut c = Composer::new(None);
        c.on_paste(&"z".repeat(1500), &[]).unwrap();
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
        assert_eq!(texts, ["─".repeat(11), " μ ❯ 日本語".into(), "     日本".into()]);
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
        assert_eq!(lines.len(), 2, "rule, one input row");
        assert_eq!(line::line_text(&lines[0]), "─".repeat(20));
        assert_eq!(cur, (1, 5));
        type_str(&mut c, "   ");
        assert_eq!(press(&mut c, KeyCode::Enter, KeyModifiers::NONE), Action::None);
        assert_eq!(c.text(), "   ");
    }

    #[test]
    fn queue_and_abort_keys() {
        let mut c = Composer::new(None);
        type_str(&mut c, "a");
        assert_eq!(
            c.on_key(key(KeyCode::Char('q'), KeyModifiers::CONTROL), true, &[]),
            Action::QueueFollowUp("a".into())
        );
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
        let queued =
            UserInput { text: "see [Image #1] and [Image #2]".into(), images: vec![other.clone(), other.clone()] };
        c.restore(vec![queued]).unwrap();
        assert_eq!(c.text(), "[Image #1]\n\nsee [Image #2] and [Image #3]");
        let input = c.take_for_submit();
        assert_eq!(input.images, vec![png(), other.clone(), other]);
    }

    #[test]
    fn restore_is_refused_while_a_key_or_command_is_typed() {
        let mut c = Composer::new(None);
        c.set_masked(true);
        type_str(&mut c, "sk-1");
        let back = c.restore(vec![UserInput::from("queued prompt")]).unwrap_err();
        assert_eq!(back[0].text, "queued prompt");
        assert_eq!(c.text(), "sk-1", "the typed key is untouched");
        c.set_masked(false);
        c.restore(back).unwrap();
        assert_eq!(c.text(), "queued prompt");
    }

    #[test]
    fn command_entry_takes_no_editor_clipboard_reply_completion_or_prediction() {
        let mut c = Composer::new(None);
        c.set_private(true);
        c.set_closed(true);
        type_str(&mut c, "!op read");
        assert_eq!(ctrl(&mut c, 'g'), Action::None, "no external editor");
        assert_eq!(ctrl(&mut c, 'r'), Action::None, "no reply search");
        assert_eq!(ctrl(&mut c, 'v'), Action::None, "no clipboard image");
        assert_eq!(ctrl(&mut c, 'q'), Action::None, "not submitted");
        press(&mut c, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(c.text(), "!op read", "no history browsing");
        assert_eq!(c.predict_query(), None);
        assert!(c.on_paste("/no/such/file.png", &[]).is_ok());
        assert_eq!(c.text(), "!op read/no/such/file.png", "pasted paths stay text");
        assert_eq!(c.take_secret(), "!op read/no/such/file.png");
        assert!(c.is_empty());
    }

    #[test]
    fn history_reads_only_the_tail_and_compacts() {
        let d = std::env::temp_dir().join(format!("mu-hist-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("h.jsonl");
        let body: String = (0..HISTORY_COMPACT_AT + 500)
            .map(|i| format!("{{\"text\":\"prompt number {i} padded padded\"}}\n"))
            .collect();
        std::fs::write(&p, body).unwrap();
        let c = Composer::new(Some(p.clone()));
        assert_eq!(c.history.len(), HISTORY_MAX);
        assert!(c.history.last().unwrap().contains(&format!("{}", HISTORY_COMPACT_AT + 499)));
        assert_eq!(count_lines(&p), HISTORY_MAX, "file rewritten to the tail");
        assert_eq!(tail_lines(&p, 3).len(), 3);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn wide_and_combining_text_wraps_by_width() {
        let mut c = Composer::new(None);
        c.set_text("日本語日本語日本語");
        let (rows, cur) = c.render(14);
        assert!(rows.len() > 2);
        assert!(cur.1 <= 14);
        let lay = line::layout(&plain("e\u{301}e\u{301}e\u{301}"), 2);
        assert_eq!(lay.rows.len(), 2, "each accented e is one column");
    }
}
