//! Filterable list overlay drawn in the composer slot.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::rc::Rc;

use super::editor::{self, Editor};
use super::line::{self, Line, bold, dim, span};
use super::markdown::{chars_to_line, chs, wrap_chs};
use super::theme::{self, Role};
use crate::events::PreviewKind;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::style::ContentStyle;

fn accent_bold() -> ContentStyle {
    let mut s = theme::get().fg(Role::Accent);
    s.attributes.set(crossterm::style::Attribute::Bold);
    s
}

/// Rows the title may take; a longer one ends in `…`.
const TITLE_MAX_LINES: usize = 6;

/// Lines a description shows under its option, when the option is highlighted or the list fits.
const DESC_MAX_LINES: usize = 6;

/// Columns a description or preview is indented under its option.
const INDENT: usize = 4;

/// Rows a highlighted option's preview may take, the truncation marker included.
const PREVIEW_MAX_LINES: usize = 16;

const TEXT_HINT: &str = "Enter submit · Alt-Enter newline · Esc to options";

pub struct Item {
    pub label: String,
    pub detail: String,
}

pub enum SelectResult {
    Pending,
    Cancelled,
    /// Left (with the filter empty) or Shift-Tab, when `back_keys` is on.
    Back,
    /// Indices into the original items, ascending.
    Chosen(Vec<usize>),
    /// Like `Chosen`, with the note (only when the note is non-empty).
    Noted(Vec<usize>, String),
    Text(String),
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Row {
    Item(usize),
    Free,
}

pub struct Select {
    title: String,
    items: Vec<Item>,
    multi: bool,
    free: Option<String>,
    /// The filter line. Single line: Enter and newlines are the caller's.
    filter: Editor,
    /// Highlighted row position in the filtered row list.
    sel: usize,
    /// Original indices toggled (multi) or preselected.
    checked: BTreeSet<usize>,
    /// True while typing into the free-text row.
    editing: bool,
    /// The free-text answer. Kept after Esc, so the row shows it and Enter edits it again.
    text: Editor,
    /// Lowercased `label detail` of each item, built once.
    haystacks: Vec<String>,
    /// The filtered rows for the filter they were computed with; reused until the filter changes.
    rows_cache: RefCell<Option<(String, Rc<Vec<Row>>)>>,
    /// Note key enabled: Right (with the filter empty), or Tab in single-select, edits a note.
    notes: bool,
    /// True while typing the note.
    noting: bool,
    note: Editor,
    /// The note before the edit, restored by Esc.
    note_before: String,
    /// Keys that pick an item at once while the filter is empty (single-select): `(key, item index)`.
    quick: Vec<(char, usize)>,
    /// A dim line at the bottom naming the keys.
    hint: Option<String>,
    /// One entry per item (by item index): a monospace block shown in a box under the item.
    previews: Vec<Option<String>>,
    /// The content kind the model declared for each preview; `None` is guessed from the text.
    kinds: Vec<Option<PreviewKind>>,
    /// Ask layout: each option's description under its label, then its note and preview box. Otherwise a description sits inline after its label.
    described: bool,
    /// Left (with the filter empty) and Shift-Tab return `SelectResult::Back`.
    back: bool,
}

impl Select {
    pub fn new(title: &str, items: Vec<Item>) -> Self {
        let haystacks = items.iter().map(|it| format!("{} {}", it.label, it.detail).to_lowercase()).collect();
        Select {
            haystacks,
            rows_cache: Default::default(),
            title: title.to_string(),
            items,
            multi: false,
            free: None,
            filter: Editor::default(),
            sel: 0,
            checked: BTreeSet::new(),
            editing: false,
            text: Editor::default(),
            notes: false,
            noting: false,
            note: Editor::default(),
            note_before: String::new(),
            quick: Vec::new(),
            hint: None,
            previews: Vec::new(),
            kinds: Vec::new(),
            described: false,
            back: false,
        }
    }

    pub fn multi(mut self) -> Self {
        self.multi = true;
        self
    }

    /// The ask layout (see `described`).
    pub fn described(mut self) -> Self {
        self.described = true;
        self
    }

    /// Left (with the filter empty) and Shift-Tab go back.
    pub fn back_keys(mut self, on: bool) -> Self {
        self.back = on;
        self
    }

    pub fn free_text(mut self, label: &str) -> Self {
        self.free = Some(label.to_string());
        *self.rows_cache.borrow_mut() = None;
        self
    }

    /// Highlights `idx`. In multi mode it is also checked.
    pub fn preselect(mut self, idx: usize) -> Self {
        if idx < self.items.len() {
            self.sel = idx;
            self.checked.insert(idx);
        }
        self
    }

    /// Checks every index in `idxs` and highlights the first.
    pub fn preselect_all(mut self, idxs: &[usize]) -> Self {
        if let Some(&first) = idxs.iter().find(|&&i| i < self.items.len()) {
            self.sel = first;
        }
        self.checked.extend(idxs.iter().copied().filter(|&i| i < self.items.len()));
        self
    }

    /// Fills the free-text answer and highlights the free-text row.
    pub fn prefill_text(mut self, s: &str) -> Self {
        if self.free.is_some() {
            self.text = Editor::from_text(s);
            self.sel = self.items.len();
        }
        self
    }

    /// Sets the note that the next submit carries.
    pub fn note_text(mut self, s: &str) -> Self {
        self.note = Editor::from_text(s);
        self
    }

    /// Enables the note key (see `notes`).
    pub fn notes(mut self) -> Self {
        self.notes = true;
        self
    }

    pub fn quick_keys(mut self, keys: Vec<(char, usize)>) -> Self {
        self.quick = keys;
        self
    }

    pub fn hint(mut self, text: &str) -> Self {
        self.hint = Some(text.to_string());
        self
    }

    pub fn previews(mut self, previews: Vec<Option<String>>) -> Self {
        self.previews = previews;
        self
    }

    pub fn preview_kinds(mut self, kinds: Vec<Option<PreviewKind>>) -> Self {
        self.kinds = kinds;
        self
    }

    pub fn on_key(&mut self, key: KeyEvent) -> SelectResult {
        if key.kind == KeyEventKind::Release {
            return SelectResult::Pending;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        if ctrl && key.code == KeyCode::Char('c') {
            return SelectResult::Cancelled;
        }
        if self.editing {
            return self.text_key(key);
        }
        if self.noting {
            return self.note_key(key);
        }
        let rows = self.rows();
        let n = rows.len();
        self.sel = self.sel.min(n.saturating_sub(1));
        if let KeyCode::Char(c) = key.code
            && !ctrl
            && !alt
            && self.filter.is_empty()
            && !self.multi
            && let Some(i) = self.quick_item(c)
        {
            return self.finish(vec![i]);
        }
        match key.code {
            KeyCode::Esc => SelectResult::Cancelled,
            KeyCode::BackTab if self.back => SelectResult::Back,
            KeyCode::Left if self.back && self.filter.is_empty() => SelectResult::Back,
            KeyCode::Up => {
                self.step(-1, n);
                SelectResult::Pending
            }
            KeyCode::Down => {
                self.step(1, n);
                SelectResult::Pending
            }
            KeyCode::Char('p') if ctrl => {
                self.step(-1, n);
                SelectResult::Pending
            }
            KeyCode::Char('n') if ctrl => {
                self.step(1, n);
                SelectResult::Pending
            }
            KeyCode::Enter => self.choose(&rows),
            KeyCode::Right if self.notes && self.filter.is_empty() => self.start_note(&rows),
            KeyCode::Tab if self.notes && !self.multi => self.start_note(&rows),
            // Space toggles only while the filter is empty; with text in it, Space is part of the
            // filter. Tab always toggles.
            KeyCode::Tab | KeyCode::Char(' ')
                if self.multi && !ctrl && (key.code == KeyCode::Tab || self.filter.is_empty()) =>
            {
                if let Some(Row::Item(i)) = rows.get(self.sel)
                    && !self.checked.remove(i)
                {
                    self.checked.insert(*i);
                }
                SelectResult::Pending
            }
            _ => {
                if self.filter.key(key, false) == editor::Edit::Changed {
                    self.sel = 0;
                }
                SelectResult::Pending
            }
        }
    }

    /// Typing the free-text answer: Enter submits it, Esc goes back to the list.
    fn text_key(&mut self, key: KeyEvent) -> SelectResult {
        match key.code {
            KeyCode::Esc => {
                self.editing = false;
                SelectResult::Pending
            }
            KeyCode::Enter if !key.modifiers.intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) => {
                if self.text.is_blank() { SelectResult::Pending } else { SelectResult::Text(self.text.text()) }
            }
            _ => {
                self.text.key(key, true);
                SelectResult::Pending
            }
        }
    }

    /// Typing the note on the highlighted row: Enter submits it with the picks, Esc restores the
    /// note as it was and goes back to the list.
    fn note_key(&mut self, key: KeyEvent) -> SelectResult {
        match key.code {
            KeyCode::Esc => {
                self.noting = false;
                self.note = Editor::from_text(&self.note_before);
                SelectResult::Pending
            }
            KeyCode::Enter if !key.modifiers.intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) => {
                self.noting = false;
                let rows = self.rows();
                self.choose(&rows)
            }
            _ => {
                self.note.key(key, true);
                SelectResult::Pending
            }
        }
    }

    fn start_note(&mut self, rows: &[Row]) -> SelectResult {
        if matches!(rows.get(self.sel), Some(Row::Item(_))) {
            self.noting = true;
            self.note_before = self.note.text();
        }
        SelectResult::Pending
    }

    fn quick_item(&self, c: char) -> Option<usize> {
        let c = c.to_ascii_lowercase();
        self.quick.iter().find(|(k, _)| *k == c).map(|(_, i)| *i)
    }

    /// Pasted text. The free-text answer and the note keep line breaks; the filter takes spaces.
    pub fn on_paste(&mut self, s: &str) {
        let text = editor::normalize_paste(s);
        if self.editing {
            self.text.insert_str(&text);
        } else if self.noting {
            self.note.insert_str(&text);
        } else {
            self.filter.insert_str(&text.replace('\n', " "));
            self.sel = 0;
        }
    }

    /// Lines (title, input, rows) fitted to `max_rows`, and the cursor (row, col).
    pub fn render(&self, width: usize, max_rows: usize) -> (Vec<Line>, (usize, usize)) {
        let width = width.max(1);
        if max_rows == 0 {
            return (Vec::new(), (0, 0));
        }
        let mut out = self.title_rows(width, max_rows);
        let rows = self.rows();
        let sel = self.sel.min(rows.len().saturating_sub(1));
        let room = max_rows - out.len();
        let mut cursor;
        if self.editing {
            let hint = room >= 2;
            let (lines, cur) = self.text.view("› ", width, room - usize::from(hint));
            cursor = (out.len() + cur.0, cur.1);
            out.extend(lines);
            if hint {
                out.push(line::styled(TEXT_HINT, |_| dim()));
            }
        } else if self.noting {
            let hint = room >= 3;
            let body = room - usize::from(hint);
            let hl = usize::from(body >= 2 && rows.get(sel).is_some());
            if let (1, Some(&row)) = (hl, rows.get(sel)) {
                out.push(self.row_line(row, true, width));
            }
            let (lines, cur) = self.note.view("note › ", width, body - hl);
            cursor = (out.len() + cur.0, cur.1);
            out.extend(lines);
            if hint {
                out.push(line::styled(TEXT_HINT, |_| dim()));
            }
        } else {
            let (line, col) = self.filter.view_line("› ", width);
            cursor = (out.len(), col);
            out.push(line);
            // The list budget is the room after the filter line; the hint takes the last row only
            // when one list row is left under it.
            let list_room = room - 1;
            let hint = self.hint.as_ref().filter(|_| list_room >= 2);
            let avail = list_room - usize::from(hint.is_some());
            if rows.is_empty() {
                if avail > 0 {
                    out.push(line::styled("  no matches", |_| dim()));
                }
            } else if avail > 0 {
                if self.described {
                    out.extend(self.described_list(&rows, sel, width, avail));
                } else {
                    let n = rows.len();
                    let (a, b, ind) = window(n, sel, avail);
                    if ind && a > 0 {
                        out.push(line::styled(format!("  … {a} more"), |_| dim()));
                    }
                    for (pos, &row) in rows.iter().enumerate().take(b).skip(a) {
                        out.push(self.row_line(row, pos == sel, width));
                    }
                    if ind && b < n {
                        out.push(line::styled(format!("  … {} more", n - b), |_| dim()));
                    }
                }
            }
            if let Some(h) = hint {
                out.push(line::styled(format!("  {h}"), |_| dim()));
            }
        }
        cursor.1 = cursor.1.min(width - 1);
        let out = out.iter().map(|l| line::truncate(l, width)).collect();
        (out, cursor)
    }

    /// The title wrapped to `width`, honouring its newlines. It takes at most half of `max_rows`
    /// (and `TITLE_MAX_LINES`), and always leaves two rows for the input and the highlighted row. A
    /// cut title ends in `…`.
    fn title_rows(&self, width: usize, max_rows: usize) -> Vec<Line> {
        let cap = (max_rows / 2).max(1).min(max_rows.saturating_sub(2)).min(TITLE_MAX_LINES);
        if cap == 0 {
            return Vec::new();
        }
        let mut rows: Vec<Line> =
            wrap_chs(&chs(&self.title, accent_bold()), width).iter().map(|r| chars_to_line(r)).collect();
        if rows.is_empty() {
            rows.push(Vec::new());
        }
        if rows.len() > cap {
            rows.truncate(cap);
            if let Some(last) = rows.last_mut() {
                let mut row = last.clone();
                row.push(span("…", accent_bold()));
                *last = line::truncate(&row, width);
            }
        }
        rows
    }

    /// The rows for the current filter, recomputed only when the filter changed.
    fn rows(&self) -> Rc<Vec<Row>> {
        let filter = self.filter.text();
        let mut cache = self.rows_cache.borrow_mut();
        if let Some((f, rows)) = cache.as_ref()
            && *f == filter
        {
            return rows.clone();
        }
        let q = filter.to_lowercase();
        let mut rows: Vec<Row> =
            (0..self.items.len()).filter(|&i| q.is_empty() || self.haystacks[i].contains(&q)).map(Row::Item).collect();
        if self.free.is_some() {
            rows.push(Row::Free);
        }
        let rows = Rc::new(rows);
        *cache = Some((filter, rows.clone()));
        rows
    }

    /// The described list: each option's label, its description under it, then its note (the
    /// highlighted one) and preview box. Heights are planned to fit `avail` rows. The highlighted
    /// option keeps its label, full description and as much preview as fits; the others' previews
    /// and descriptions give way first (shorter, then one line, then none).
    fn described_list(&self, rows: &[Row], sel: usize, width: usize, avail: usize) -> Vec<Line> {
        let n = rows.len();
        let detail = |pos: usize| match rows[pos] {
            Row::Item(i) => self.items[i].detail.as_str(),
            Row::Free => "",
        };
        let note = (!self.note.is_blank()).then(|| self.note.text().replace('\n', " "));
        let note_h = usize::from(note.is_some() && avail >= 2);
        let hl_cap = DESC_MAX_LINES.min(avail.saturating_sub(1 + note_h));
        let full: Vec<Vec<Line>> =
            (0..n).map(|pos| desc_rows(detail(pos), width, if pos == sel { hl_cap } else { DESC_MAX_LINES })).collect();
        let short: Vec<Vec<Line>> =
            (0..n).map(|pos| if pos == sel { full[pos].clone() } else { desc_rows(detail(pos), width, 1) }).collect();
        let hl_base = 1 + full.get(sel).map_or(0, Vec::len) + note_h;
        let hl_box_cap = (PREVIEW_MAX_LINES + 2).min(avail.saturating_sub(hl_base));
        let boxes = |other_cap: usize| -> Vec<Vec<Line>> {
            (0..n)
                .map(|pos| match rows[pos] {
                    Row::Item(i) => match self.previews.get(i) {
                        Some(Some(text)) => preview_box(
                            text,
                            self.kinds.get(i).copied().flatten(),
                            width.saturating_sub(INDENT),
                            if pos == sel { hl_box_cap } else { other_cap },
                            pos == sel,
                        ),
                        _ => Vec::new(),
                    },
                    Row::Free => Vec::new(),
                })
                .collect()
        };
        let desc_of = |mode: usize, pos: usize| -> usize {
            match mode {
                _ if pos == sel => full[pos].len(),
                0 => full[pos].len(),
                1 => short[pos].len(),
                _ => 0,
            }
        };
        let heights = |mode: usize, bx: &[Vec<Line>]| -> Vec<usize> {
            (0..n)
                .map(|pos| {
                    1 + desc_of(mode, pos) + if pos == sel { note_h } else { 0 } + bx.get(pos).map_or(0, Vec::len)
                })
                .collect()
        };
        // (description mode, rows the other options' preview boxes may take); the last always applies.
        let plans = [(0, PREVIEW_MAX_LINES + 2), (0, 8), (1, 8), (1, 4), (2, 4), (2, 0)];
        let (mode, bx, h) = plans
            .iter()
            .enumerate()
            .find_map(|(k, &(mode, cap))| {
                let bx = boxes(cap);
                let h = heights(mode, &bx);
                (h.iter().sum::<usize>() <= avail || k + 1 == plans.len()).then_some((mode, bx, h))
            })
            .unwrap_or_else(|| (2, Vec::new(), heights(2, &[])));
        let (a, b, ind) = window_by(&h, sel, avail);
        let mut out: Vec<Line> = Vec::new();
        if ind && a > 0 {
            out.push(line::styled(format!("  … {a} more"), |_| dim()));
        }
        for pos in a..b {
            out.push(self.row_line(rows[pos], pos == sel, width));
            let d: &[Line] = match mode {
                _ if pos == sel => &full[pos],
                0 => &full[pos],
                1 => &short[pos],
                _ => &[],
            };
            out.extend(d.iter().cloned());
            if pos == sel
                && let Some(text) = &note
            {
                out.push(line::styled(format!("{}note: {text}", " ".repeat(INDENT)), |_| dim()));
            }
            if let Some(block) = bx.get(pos) {
                out.extend(block.iter().cloned());
            }
        }
        if ind && b < n {
            out.push(line::styled(format!("  … {} more", n - b), |_| dim()));
        }
        out
    }

    fn step(&mut self, delta: isize, n: usize) {
        if n > 0 {
            self.sel = (self.sel as isize + delta).rem_euclid(n as isize) as usize;
        }
    }

    /// Picks the highlighted row, or the checked rows in multi mode. The free-text row opens its
    /// editor.
    fn choose(&mut self, rows: &[Row]) -> SelectResult {
        if self.multi && !self.checked.is_empty() {
            return self.finish(self.checked.iter().copied().collect());
        }
        match rows.get(self.sel) {
            Some(Row::Item(i)) => self.finish(vec![*i]),
            Some(Row::Free) => {
                self.editing = true;
                SelectResult::Pending
            }
            None => SelectResult::Pending,
        }
    }

    /// The picks, with the note when there is one.
    fn finish(&self, picks: Vec<usize>) -> SelectResult {
        let note = self.note.text().trim().to_string();
        if self.notes && !note.is_empty() { SelectResult::Noted(picks, note) } else { SelectResult::Chosen(picks) }
    }

    /// The free row's label: its own label, or the answer typed so far.
    fn free_label(&self) -> String {
        if self.text.is_blank() { self.free.clone().unwrap_or_default() } else { self.text.text().replace('\n', " ") }
    }

    fn row_line(&self, row: Row, selected: bool, width: usize) -> Line {
        let emphasis = if selected { bold() } else { ContentStyle::default() };
        let marker = if selected { theme::get().fg(Role::Accent) } else { ContentStyle::default() };
        let mut l: Line = vec![span(if selected { "❯ " } else { "  " }, marker)];
        match row {
            Row::Item(i) => {
                let it = &self.items[i];
                if self.multi {
                    let on = self.checked.contains(&i);
                    l.push(span(if on { "[x] " } else { "[ ] " }, ContentStyle::default()));
                }
                l.push(span(it.label.clone(), emphasis));
                if !it.detail.is_empty() && !self.described {
                    l.push(span("  ", ContentStyle::default()));
                    l.push(span(it.detail.clone(), dim()));
                }
            }
            Row::Free => {
                if self.multi {
                    l.push(span("    ", ContentStyle::default()));
                }
                l.push(span(self.free_label(), emphasis));
            }
        }
        if selected {
            l = line::fill(&l, width, theme::get().color(Role::Selected));
        }
        l
    }
}

/// Item range `[a, b)` around `sel` that fits `avail` rows including "… N more" indicators.
/// The flag is false when indicators are omitted (only the selected row fits).
fn window(n: usize, sel: usize, avail: usize) -> (usize, usize, bool) {
    window_by(&vec![1; n], sel, avail)
}

/// `window` for rows of differing heights `h`: the range around `sel` whose heights and indicators
/// fit `avail`. The selected row is always in it, so `h[sel]` must fit on its own.
fn window_by(h: &[usize], sel: usize, avail: usize) -> (usize, usize, bool) {
    let n = h.len();
    if h.iter().sum::<usize>() <= avail {
        return (0, n, false);
    }
    let cost = |a: usize, b: usize| h[a..b].iter().sum::<usize>() + (a > 0) as usize + (b < n) as usize;
    if cost(sel, sel + 1) > avail {
        return (sel, sel + 1, false);
    }
    let (mut a, mut b) = (sel, sel + 1);
    loop {
        let mut grew = false;
        if b < n && cost(a, b + 1) <= avail {
            b += 1;
            grew = true;
        }
        if a > 0 && cost(a - 1, b) <= avail {
            a -= 1;
            grew = true;
        }
        if !grew {
            break;
        }
    }
    (a, b, true)
}

/// A description as rows under an option, wrapped to the width past the indent, at most `max`
/// rows. A cut row ends in `…` within the width.
fn desc_rows(detail: &str, width: usize, max: usize) -> Vec<Line> {
    if detail.trim().is_empty() || max == 0 {
        return Vec::new();
    }
    let tw = width.saturating_sub(INDENT).max(1);
    let rows = wrap_chs(&chs(detail, dim()), tw);
    let mut out: Vec<Line> = rows.iter().map(|r| chars_to_line(r)).collect();
    if out.len() > max {
        out.truncate(max);
        if let Some(last) = out.last_mut() {
            // `truncate` supplies its own `…` when it cuts, so append it first and cut to the width.
            let mut cut = last.clone();
            cut.push(span("…", dim()));
            *last = line::truncate(&cut, tw);
        }
    }
    out.into_iter().map(|l| indented(l, INDENT)).collect()
}

/// `l` after `by` spaces.
fn indented(l: Line, by: usize) -> Line {
    let mut row = vec![span(" ".repeat(by), ContentStyle::default())];
    row.extend(l);
    row
}

/// What a preview shows: the declared kind, else a guess from the text.
fn preview_kind(text: &str, hint: Option<PreviewKind>) -> PreviewKind {
    if let Some(k) = hint {
        return k;
    }
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let n = lines.len();
    let count = |p: &dyn Fn(&str) -> bool| lines.iter().filter(|l| p(l)).count();
    let add = count(&|l| l.starts_with('+'));
    let del = count(&|l| l.starts_with('-'));
    if lines.iter().any(|l| l.starts_with("@@") || l.starts_with("diff --git"))
        || (add > 0 && del > 0 && (add + del) * 2 >= n)
    {
        return PreviewKind::Diff;
    }
    if text.chars().any(|c| ('\u{2500}'..='\u{257f}').contains(&c)) || count(&|l| l.trim_start().starts_with('|')) >= 2
    {
        return PreviewKind::Layout;
    }
    const STARTS: [&str; 12] =
        ["fn ", "def ", "let ", "const ", "import ", "use ", "class ", "if ", "for ", "return ", "//", "#"];
    let code = count(&|l| {
        let t = l.trim();
        t.ends_with([';', '{', '}', '(', ')', ',']) || STARTS.iter().any(|s| t.starts_with(s))
    });
    if n > 0 && code * 2 >= n { PreviewKind::Code } else { PreviewKind::Text }
}

fn preview_row_style(kind: PreviewKind, row: &str) -> ContentStyle {
    let t = theme::get();
    match kind {
        PreviewKind::Diff if row.starts_with('+') => t.fg(Role::Ok),
        PreviewKind::Diff if row.starts_with('-') => t.fg(Role::Err),
        PreviewKind::Diff if row.starts_with("@@") => t.fg(Role::Accent),
        PreviewKind::Text => t.fg(Role::Muted),
        _ => ContentStyle::default(),
    }
}

/// The tree-sitter language of a code preview, guessed from its text; only languages mu compiles in.
fn guess_lang(code: &str) -> Option<&'static str> {
    let has = |s: &str| code.contains(s);
    let starts = |ps: &[&str]| code.lines().any(|l| ps.iter().any(|p| l.trim_start().starts_with(p)));
    let t = code.trim_start();
    if (t.starts_with('{') || t.starts_with('[')) && has("\":") {
        Some("json")
    } else if starts(&["fn ", "pub fn ", "let mut ", "impl ", "struct ", "pub struct ", "enum ", "use std"])
        || has("println!")
    {
        Some("rust")
    } else if starts(&["def ", "import ", "elif ", "print("]) || (starts(&["from "]) && has(" import ")) {
        Some("python")
    } else if starts(&["function ", "const ", "let "]) || has("=>") || has("console.") {
        Some("javascript")
    } else if t.starts_with("#!") || starts(&["$ ", "cargo ", "git "]) {
        Some("bash")
    } else {
        None
    }
}

/// A preview in a labelled box `width` columns wide and at most `cap` rows tall, indented under its
/// option. Past the cap the last body row says how many lines were cut. Empty when it can't fit.
fn preview_box(text: &str, hint: Option<PreviewKind>, width: usize, cap: usize, highlighted: bool) -> Vec<Line> {
    if cap < 3 || width < 6 {
        return Vec::new();
    }
    let kind = preview_kind(text, hint);
    let (role, label) = match kind {
        PreviewKind::Code => (Role::SynFunction, "code"),
        PreviewKind::Diff => (Role::Warn, "diff"),
        PreviewKind::Layout => (Role::Tool, "layout"),
        PreviewKind::Text => (Role::Muted, "text"),
    };
    let mut border = theme::get().fg(role);
    if highlighted {
        border.attributes.set(crossterm::style::Attribute::Bold);
    }
    let inner = width - 4;
    let clean = line::sanitize(text.trim_end_matches('\n'));
    let src: Vec<&str> = clean.split('\n').collect();
    let max = cap - 2;
    let keep = if src.len() > max { max - 1 } else { src.len() };
    let hl = (kind == PreviewKind::Code)
        .then(|| guess_lang(&clean))
        .flatten()
        .and_then(|lang| super::highlight::highlight(&clean, lang))
        .filter(|h| h.len() == src.len());
    let mut body: Vec<Line> = src[..keep]
        .iter()
        .enumerate()
        .map(|(i, r)| match &hl {
            Some(h) => line::truncate(&h[i], inner),
            None => vec![span(line::truncate_text(r, inner), preview_row_style(kind, r))],
        })
        .collect();
    if keep < src.len() {
        body.push(line::styled(line::truncate_text(&format!("… {} more lines", src.len() - keep), inner), |_| dim()));
    }
    let title = format!(" {label} ");
    let top = if title.len() + 3 <= width {
        format!("┌─{title}{}┐", "─".repeat(width - 3 - title.len()))
    } else {
        format!("┌{}┐", "─".repeat(width - 2))
    };
    let mut out = vec![indented(vec![span(top, border)], INDENT)];
    for l in body {
        let pad = inner.saturating_sub(line::line_width(&l));
        let mut row = vec![span("│ ", border)];
        row.extend(l);
        row.push(span(format!("{} │", " ".repeat(pad)), border));
        out.push(indented(row, INDENT));
    }
    out.push(indented(vec![span(format!("└{}┘", "─".repeat(width - 2)), border)], INDENT));
    out
}

pub fn clean_paste(s: &str) -> String {
    s.chars()
        .filter_map(|c| match c {
            '\r' | '\n' | '\t' => Some(' '),
            c if c.is_control() => None,
            c => Some(c),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_previews_are_syntax_highlighted() {
        let s = described(&[("a", "")]).previews(vec![Some("fn main() {\n    run();\n}".into())]);
        let (lines, _) = s.render(40, 30);
        let kw = theme::get().color(Role::SynKeyword);
        let row = lines.iter().find(|l| line::line_text(l).contains("fn main")).expect("code row");
        assert!(row.iter().any(|sp| sp.text.contains("fn") && sp.style.foreground_color == kw), "{row:?}");
        assert!(lines.iter().all(|l| line::line_width(l) <= 40));
    }

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn alt(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::ALT)
    }

    fn type_str(s: &mut Select, t: &str) {
        for c in t.chars() {
            s.on_key(key(KeyCode::Char(c)));
        }
    }

    fn items(labels: &[&str]) -> Vec<Item> {
        labels.iter().map(|l| Item { label: l.to_string(), detail: String::new() }).collect()
    }

    fn described_items(spec: &[(&str, &str)]) -> Vec<Item> {
        spec.iter().map(|(l, d)| Item { label: l.to_string(), detail: d.to_string() }).collect()
    }

    fn texts(lines: &[Line]) -> Vec<String> {
        lines.iter().map(|l| line::line_text(l).trim_end().to_string()).collect()
    }

    fn chosen(r: SelectResult) -> Option<Vec<usize>> {
        match r {
            SelectResult::Chosen(v) => Some(v),
            _ => None,
        }
    }

    #[test]
    fn filter_maps_back_to_original_indices() {
        let mut s = Select::new("t", items(&["apple", "banana", "Cherry"]));
        type_str(&mut s, "AN");
        assert_eq!(chosen(s.on_key(key(KeyCode::Enter))), Some(vec![1]));
    }

    #[test]
    fn filter_matches_detail() {
        let mut s = Select::new(
            "t",
            vec![Item { label: "a".into(), detail: "fast".into() }, Item { label: "b".into(), detail: "slow".into() }],
        );
        type_str(&mut s, "SLOW");
        assert_eq!(chosen(s.on_key(key(KeyCode::Enter))), Some(vec![1]));
    }

    #[test]
    fn wrap_around_and_cancel() {
        let mut s = Select::new("t", items(&["a", "b", "c"]));
        s.on_key(key(KeyCode::Up));
        assert_eq!(chosen(s.on_key(key(KeyCode::Enter))), Some(vec![2]));
        let mut s = Select::new("t", items(&["a"]));
        assert!(matches!(s.on_key(key(KeyCode::Esc)), SelectResult::Cancelled));
    }

    #[test]
    fn multi_toggle_and_enter() {
        let mut s = Select::new("t", items(&["a", "b", "c"])).multi();
        s.on_key(key(KeyCode::Char(' ')));
        s.on_key(key(KeyCode::Down));
        s.on_key(key(KeyCode::Down));
        s.on_key(key(KeyCode::Char(' ')));
        s.on_key(key(KeyCode::Char(' ')));
        s.on_key(key(KeyCode::Char(' ')));
        s.on_key(key(KeyCode::Up));
        s.on_key(key(KeyCode::Up));
        assert_eq!(chosen(s.on_key(key(KeyCode::Enter))), Some(vec![0, 2]));
    }

    #[test]
    fn multi_enter_without_toggle_returns_highlighted() {
        let mut s = Select::new("t", items(&["a", "b", "c"])).multi();
        s.on_key(key(KeyCode::Down));
        assert_eq!(chosen(s.on_key(key(KeyCode::Enter))), Some(vec![1]));
    }

    #[test]
    fn preselect_checks_and_highlights() {
        let mut s = Select::new("t", items(&["a", "b", "c"])).multi().preselect(2);
        assert_eq!(chosen(s.on_key(key(KeyCode::Enter))), Some(vec![2]));
        let mut s = Select::new("t", items(&["a", "b", "c"])).preselect(1);
        assert_eq!(chosen(s.on_key(key(KeyCode::Enter))), Some(vec![1]));
        let mut s = Select::new("t", items(&["a", "b", "c"])).multi().preselect_all(&[0, 2]);
        assert_eq!(chosen(s.on_key(key(KeyCode::Enter))), Some(vec![0, 2]));
    }

    #[test]
    fn free_text_flow() {
        let mut s = Select::new("t", items(&["a"])).free_text("Other (type your own)");
        s.on_key(key(KeyCode::Down));
        assert!(matches!(s.on_key(key(KeyCode::Enter)), SelectResult::Pending));
        assert!(matches!(s.on_key(key(KeyCode::Enter)), SelectResult::Pending));
        type_str(&mut s, "hi");
        match s.on_key(key(KeyCode::Enter)) {
            SelectResult::Text(t) => assert_eq!(t, "hi"),
            _ => panic!("expected text"),
        }
    }

    #[test]
    fn esc_in_text_returns_to_list_then_cancels() {
        let mut s = Select::new("t", items(&["a"])).free_text("Other");
        s.on_key(key(KeyCode::Down));
        s.on_key(key(KeyCode::Enter));
        type_str(&mut s, "x");
        assert!(matches!(s.on_key(key(KeyCode::Esc)), SelectResult::Pending));
        assert!(matches!(s.on_key(key(KeyCode::Esc)), SelectResult::Cancelled));
    }

    #[test]
    fn free_row_stays_visible_under_filter() {
        let mut s = Select::new("t", items(&["apple"])).free_text("Other");
        type_str(&mut s, "zzz");
        let (lines, _) = s.render(40, 10);
        assert!(texts(&lines).iter().any(|t| t.contains("Other")));
        s.on_key(key(KeyCode::Enter));
        type_str(&mut s, "ok");
        assert!(matches!(s.on_key(key(KeyCode::Enter)), SelectResult::Text(t) if t == "ok"));
    }

    #[test]
    fn render_cursor_and_truncation() {
        let mut s = Select::new("title", vec![Item { label: "x".into(), detail: "d".repeat(100) }]);
        type_str(&mut s, "ab");
        let (lines, cur) = s.render(20, 10);
        assert_eq!(cur, (1, 4));
        assert!(lines.iter().all(|l| line::line_width(l) <= 20));
    }

    #[test]
    fn windowing_keeps_selection_visible_and_fits() {
        let labels: Vec<String> = (0..50).map(|i| format!("item{i:02}")).collect();
        let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
        let mut s = Select::new("t", items(&refs));
        for _ in 0..40 {
            s.on_key(key(KeyCode::Down));
        }
        let (lines, _) = s.render(40, 10);
        assert!(lines.len() <= 10);
        let t = texts(&lines);
        assert!(t.iter().any(|l| l.starts_with("❯ item40")));
        assert!(t.iter().any(|l| l.contains("more")));
    }

    #[test]
    fn window_invariants() {
        for n in 0..30 {
            for sel in 0..n.max(1) {
                for avail in 1..12 {
                    if n == 0 {
                        continue;
                    }
                    let (a, b, ind) = window(n, sel, avail);
                    assert!(a <= sel && sel < b, "n={n} sel={sel} avail={avail}");
                    let indicators = (a > 0) as usize + (b < n) as usize;
                    let used = (b - a) + if ind { indicators } else { 0 };
                    assert!(used <= avail, "n={n} sel={sel} avail={avail} used={used}");
                }
            }
        }
    }

    #[test]
    fn space_types_in_the_filter_and_tab_toggles() {
        let mut s = Select::new("t", items(&["new york", "newark"])).multi();
        s.on_key(key(KeyCode::Char(' ')));
        assert_eq!(chosen(s.on_key(key(KeyCode::Enter))), Some(vec![0]), "empty filter: space toggles");
        let mut s = Select::new("t", items(&["new york", "newark"])).multi();
        type_str(&mut s, "new y");
        s.on_key(key(KeyCode::Tab));
        assert_eq!(chosen(s.on_key(key(KeyCode::Enter))), Some(vec![0]), "the filter kept its space");
    }

    #[test]
    fn detail_stays_inline_outside_the_ask_layout() {
        let s = Select::new("t", vec![Item { label: "a".into(), detail: "fast".into() }]);
        let (lines, _) = s.render(40, 10);
        assert_eq!(texts(&lines)[2].trim_end(), "❯ a  fast");
    }

    #[test]
    fn plain_picker_renders_as_before() {
        // A model-style picker: inline details, a filter line, no hint.
        let items = vec![
            Item { label: "anthropic/claude".into(), detail: "fast".into() },
            Item { label: "openai/gpt".into(), detail: String::new() },
        ];
        let mut s = Select::new("Pick a model", items);
        s.on_key(key(KeyCode::Down));
        let (lines, cur) = s.render(40, 10);
        assert_eq!(texts(&lines), ["Pick a model", "›", "  anthropic/claude  fast", "❯ openai/gpt"]);
        assert_eq!(cur, (1, 2));
        type_str(&mut s, "gpt");
        let (lines, cur) = s.render(40, 10);
        assert_eq!(texts(&lines), ["Pick a model", "› gpt", "❯ openai/gpt"]);
        assert_eq!(cur, (1, 5));
    }

    #[test]
    fn left_and_shift_tab_go_back_only_with_the_back_keys_and_an_empty_filter() {
        let mut s = Select::new("t", items(&["a", "b"]));
        assert!(matches!(s.on_key(key(KeyCode::Left)), SelectResult::Pending), "off by default");
        let mut s = Select::new("t", items(&["a", "b"])).back_keys(true);
        assert!(matches!(s.on_key(key(KeyCode::Left)), SelectResult::Back));
        assert!(matches!(s.on_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT)), SelectResult::Back));
        type_str(&mut s, "b");
        assert!(matches!(s.on_key(key(KeyCode::Left)), SelectResult::Pending), "with a filter, Left moves the cursor");
    }

    #[test]
    fn filter_edits_with_the_shared_shortcuts() {
        let mut s = Select::new("t", items(&["alpha", "beta"]));
        type_str(&mut s, "Xbeta");
        s.on_key(key(KeyCode::Home));
        s.on_key(key(KeyCode::Delete));
        let (lines, cur) = s.render(40, 10);
        assert_eq!(texts(&lines)[1], "› beta");
        assert_eq!(cur, (1, 2));
        assert_eq!(chosen(s.on_key(key(KeyCode::Enter))), Some(vec![1]));
    }

    #[test]
    fn note_submits_the_highlighted_pick_with_the_text() {
        let mut s = Select::new("t", ask_items()).notes();
        s.on_key(key(KeyCode::Down));
        s.on_key(key(KeyCode::Right));
        type_str(&mut s, " later ");
        assert!(matches!(s.on_key(key(KeyCode::Enter)), SelectResult::Noted(p, n) if p == vec![1] && n == "later"));
    }

    #[test]
    fn note_needs_the_note_key_and_an_item() {
        let mut s = Select::new("t", ask_items());
        assert_eq!(chosen(s.on_key(key(KeyCode::Right))), None);
        assert_eq!(chosen(s.on_key(key(KeyCode::Enter))), Some(vec![0]), "Right does nothing without notes()");
        let mut s = Select::new("t", ask_items()).notes().free_text("Other");
        s.on_key(key(KeyCode::Up));
        s.on_key(key(KeyCode::Right));
        type_str(&mut s, "x");
        assert!(matches!(s.on_key(key(KeyCode::Enter)), SelectResult::Pending), "no note on the free-text row");
    }

    #[test]
    fn tab_opens_a_note_only_in_single_select() {
        let mut s = Select::new("t", ask_items()).notes();
        s.on_key(key(KeyCode::Tab));
        type_str(&mut s, "z");
        assert!(matches!(s.on_key(key(KeyCode::Enter)), SelectResult::Noted(p, n) if p == vec![0] && n == "z"));
        let mut s = Select::new("t", ask_items()).notes().multi();
        s.on_key(key(KeyCode::Tab));
        assert_eq!(chosen(s.on_key(key(KeyCode::Enter))), Some(vec![0]), "in multi, Tab toggles");
    }

    #[test]
    fn note_render_shows_the_pick_and_the_input() {
        let mut s = Select::new("t", ask_items()).notes();
        s.on_key(key(KeyCode::Down));
        s.on_key(key(KeyCode::Right));
        type_str(&mut s, "ok");
        let (lines, cur) = s.render(40, 10);
        let t = texts(&lines);
        assert_eq!(t[1].trim_end(), "❯ b", "{t:?}");
        assert_eq!(t[2], "note › ok", "{t:?}");
        assert_eq!(cur, (2, 9));
    }

    #[test]
    fn esc_in_the_note_restores_the_earlier_note() {
        let mut s = Select::new("t", ask_items()).notes().note_text("first");
        s.on_key(key(KeyCode::Right));
        type_str(&mut s, " second");
        assert!(matches!(s.on_key(key(KeyCode::Esc)), SelectResult::Pending));
        assert!(matches!(s.on_key(key(KeyCode::Enter)), SelectResult::Noted(p, n) if p == vec![0] && n == "first"));
    }

    #[test]
    fn quick_keys_pick_at_once_while_the_filter_is_empty() {
        let mut s = Select::new("t", ask_items()).quick_keys(vec![('y', 0), ('n', 2)]);
        assert_eq!(chosen(s.on_key(key(KeyCode::Char('N')))), Some(vec![2]));
        let mut s = Select::new("t", ask_items()).quick_keys(vec![('y', 0)]);
        type_str(&mut s, "a");
        assert_eq!(chosen(s.on_key(key(KeyCode::Char('y')))), None, "with a filter, y types");
    }

    #[test]
    fn hint_is_the_last_row() {
        let s = Select::new("t", ask_items()).hint("keys");
        let (lines, _) = s.render(40, 10);
        assert_eq!(texts(&lines).last().map(String::as_str), Some("  keys"));
    }

    #[test]
    fn title_wraps_to_the_width_and_honours_newlines() {
        let title = "Deploy?\nstatus: 3 services\nthe canary has been running for a long while now";
        let s = Select::new(title, items(&["yes", "no"]));
        let (lines, cur) = s.render(20, 20);
        let t = texts(&lines);
        assert_eq!(t[..5], ["Deploy?", "status: 3 services", "the canary has been", "running for a long", "while now"]);
        assert_eq!(cur, (5, 2), "the cursor sits on the input row under the title");
        assert!(t[6].starts_with("❯ yes"), "{t:?}");
        assert!(lines.iter().all(|l| line::line_width(l) <= 20), "{t:?}");
    }

    #[test]
    fn title_is_capped_with_an_ellipsis_and_the_list_keeps_rows() {
        let title = (0..20).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        let s = Select::new(&title, items(&["yes", "no"]));
        let (lines, cur) = s.render(30, 20);
        let t = texts(&lines);
        assert_eq!(cur.0, TITLE_MAX_LINES, "{t:?}");
        assert_eq!(t[..TITLE_MAX_LINES], ["line 0", "line 1", "line 2", "line 3", "line 4", "line 5…"], "{t:?}");
        assert!(t[TITLE_MAX_LINES + 1].starts_with("❯ yes"), "{t:?}");
        // Short heights: the title shrinks further and the list still shows its selected row.
        let (lines, _) = s.render(30, 3);
        let t = texts(&lines);
        assert_eq!(t.len(), 3, "{t:?}");
        assert_eq!(t[0], "line 0…");
        assert!(t[2].starts_with("❯ yes"), "{t:?}");
        let (lines, _) = s.render(30, 4);
        let t = texts(&lines);
        assert_eq!(t.len(), 4, "{t:?}");
        assert_eq!(t[1], "line 1…");
        assert!(t[3].starts_with("❯ yes"), "{t:?}");
    }

    #[test]
    fn title_lines_never_exceed_the_width() {
        let title = "日本語のタイトルは幅が二倍です、長いのでいくつにも折り返されます\n\nshort\tab";
        let s = Select::new(title, items(&["a", "b"]));
        for width in [1, 3, 8, 20] {
            for max_rows in [3, 4, 8, 20] {
                let (lines, _) = s.render(width, max_rows);
                assert!(lines.len() <= max_rows, "width {width}, rows {max_rows}: {:?}", texts(&lines));
                for l in &lines {
                    assert!(line::line_width(l) <= width, "width {width}: {:?}", line::line_text(l));
                }
            }
        }
    }

    fn ask_items() -> Vec<Item> {
        items(&["a", "b", "c"])
    }

    /// A described picker with a filter-free list.
    fn described(spec: &[(&str, &str)]) -> Select {
        Select::new("Q?", described_items(spec)).described()
    }

    #[test]
    fn described_layout_puts_each_description_under_its_label() {
        let s = described(&[("Yes", "apply it now"), ("No", ""), ("Later", "wait until tomorrow morning")]);
        let (lines, _) = s.render(40, 20);
        let t = texts(&lines);
        assert_eq!(t[2], "❯ Yes", "{t:?}");
        assert_eq!(t[3], "    apply it now", "{t:?}");
        assert_eq!(t[4], "  No", "{t:?}");
        assert_eq!(t[5], "  Later", "{t:?}");
        assert_eq!(t[6], "    wait until tomorrow morning", "a non-highlighted description shows in full: {t:?}");
    }

    #[test]
    fn described_layout_wraps_the_highlighted_description_to_the_width() {
        let text = "the first option has a long description that wraps over several rows of the picker";
        let s = described(&[("a", text), ("b", "short")]);
        let (lines, _) = s.render(30, 20);
        let t = texts(&lines);
        assert_eq!(t[2], "❯ a", "{t:?}");
        assert_eq!(t[3], "    the first option has a", "{t:?}");
        assert_eq!(t[4], "    long description that", "{t:?}");
        assert_eq!(t[5], "    wraps over several rows of", "{t:?}");
        assert_eq!(t[6], "    the picker", "{t:?}");
        assert_eq!(t[7], "  b", "{t:?}");
        assert!(lines.iter().all(|l| line::line_width(l) <= 30), "{t:?}");
    }

    #[test]
    fn described_layout_collapses_other_descriptions_first() {
        let long = "word ".repeat(30);
        let s = described(&[("a", &long), ("b", &long), ("c", &long), ("d", "tail")]);
        // Full: the others' descriptions do not fit, so one line each; a tighter list drops them.
        let (lines, _) = s.render(30, 16);
        let t = texts(&lines);
        assert!(t.iter().any(|l| l == "  b") && t.iter().any(|l| l == "  c"), "{t:?}");
        let truncated = t.iter().filter(|l| l.starts_with("    word") && l.ends_with('…')).count();
        assert_eq!(truncated, 2, "b and c keep one truncated line each, a its six: {t:?}");
        assert_eq!(t.iter().filter(|l| l.starts_with("    word")).count(), 8, "{t:?}");
        let (lines, _) = s.render(30, 14);
        let t = texts(&lines);
        assert!(t.iter().any(|l| l == "❯ a") && t.iter().any(|l| l == "  d"), "{t:?}");
        assert_eq!(t.iter().filter(|l| l.starts_with("    ")).count(), 6, "only the highlighted description: {t:?}");
        assert!(lines.iter().all(|l| line::line_width(l) <= 30), "{t:?}");
        assert!(lines.len() <= 14, "{t:?}");
    }

    #[test]
    fn described_highlighted_option_keeps_its_description_and_moves_with_the_cursor() {
        let long = "word ".repeat(40);
        let mut s = described(&[("a", &long), ("b", "bee"), ("c", "sea")]);
        s.on_key(key(KeyCode::Down));
        s.on_key(key(KeyCode::Down));
        let (lines, _) = s.render(30, 8);
        let t = texts(&lines);
        assert!(t.iter().any(|l| l.starts_with("❯ c")), "{t:?}");
        assert!(t.iter().any(|l| l == "    sea"), "the highlighted option's description is whole: {t:?}");
        assert!(lines.len() <= 8, "{t:?}");
    }

    fn box_rows(t: &[String]) -> Vec<&str> {
        t.iter().map(String::as_str).filter(|l| l.trim_start().starts_with('│')).collect()
    }

    #[test]
    fn described_previews_show_in_labelled_boxes_under_every_option() {
        let mut s = described(&[("A", "first"), ("B", "second")])
            .previews(vec![Some("fn a() {}\nfn a2() {}".into()), Some("fn b() {}".into())]);
        let (lines, _) = s.render(40, 30);
        let t = texts(&lines);
        assert_eq!(t[2], "❯ A", "{t:?}");
        assert_eq!(t[3], "    first", "{t:?}");
        assert!(t[4].starts_with("    ┌─ code ─") && t[4].ends_with('┐'), "{t:?}");
        assert!(t[5].starts_with("    │ fn a() {}") && t[5].ends_with(" │"), "{t:?}");
        assert!(t[6].starts_with("    │ fn a2() {}"), "{t:?}");
        assert!(t[7].starts_with("    └─") && t[7].ends_with('┘'), "{t:?}");
        assert_eq!(t[8], "  B", "{t:?}");
        assert!(t.iter().any(|l| l.starts_with("    │ fn b() {}")), "B's preview shows without highlight: {t:?}");
        s.on_key(key(KeyCode::Down));
        let (lines, _) = s.render(40, 30);
        let t = texts(&lines);
        assert!(t.iter().any(|l| l.contains("fn a() {}")) && t.iter().any(|l| l.contains("fn b() {}")), "{t:?}");
    }

    #[test]
    fn described_preview_is_bounded_and_marked_and_clipped() {
        let long: String = (1..=40).map(|i| format!("line {i}\n")).collect();
        let s = described(&[("a", "")]).previews(vec![Some(long)]);
        let (lines, _) = s.render(60, 40);
        let t = texts(&lines);
        let body = box_rows(&t);
        assert_eq!(body.len(), PREVIEW_MAX_LINES, "{t:?}");
        assert!(body[0].starts_with("│ line 1") || t.iter().any(|l| l.starts_with("    │ line 1 ")), "{t:?}");
        assert!(t.iter().any(|l| l.starts_with("    │ … 25 more lines")), "{t:?}");
        let s = described(&[("x", "")]).previews(vec![Some("x".repeat(100))]);
        let (lines, _) = s.render(20, 30);
        let t = texts(&lines);
        assert!(t.iter().any(|l| l.contains("x…")), "{t:?}");
        assert!(lines.iter().all(|l| line::line_width(l) <= 20), "{t:?}");
    }

    #[test]
    fn described_preview_keeps_its_leading_spaces_and_drops_tabs_to_spaces() {
        let s = described(&[("a", "")]).previews(vec![Some("if x {\n\treturn 1;\n}".into())]);
        let (lines, _) = s.render(40, 30);
        let t = texts(&lines);
        assert!(t.iter().any(|l| l.starts_with("    │ if x {")), "{t:?}");
        assert!(t.iter().any(|l| l.starts_with("    │     return 1;")), "{t:?}");
    }

    #[test]
    fn preview_kind_is_declared_or_guessed() {
        let k = |t: &str| preview_kind(t, None);
        assert_eq!(k("@@ -1 +1 @@\n-a\n+b"), PreviewKind::Diff);
        assert_eq!(k("-old\n+new\n context"), PreviewKind::Diff);
        assert_eq!(k("┌──┐\n│  │\n└──┘"), PreviewKind::Layout);
        assert_eq!(k("| a | b |\n|---|---|\n| 1 | 2 |"), PreviewKind::Layout);
        assert_eq!(k("fn main() {\n    run();\n}"), PreviewKind::Code);
        assert_eq!(k("A plain sentence\nwith no syntax"), PreviewKind::Text);
        assert_eq!(preview_kind("A plain sentence", Some(PreviewKind::Code)), PreviewKind::Code);
    }

    #[test]
    fn described_previews_squeeze_others_before_the_highlighted_one() {
        let body: String = (1..=10).map(|i| format!("l{i}\n")).collect();
        let s = described(&[("A", ""), ("B", ""), ("C", "")]).previews(vec![
            Some(body.clone()),
            Some(body.clone()),
            Some(body),
        ]);
        let (lines, _) = s.render(40, 22);
        let t = texts(&lines);
        assert!(lines.len() <= 22, "{t:?}");
        assert!(t.iter().any(|l| l.starts_with("    │ l10 ")), "the highlighted preview is whole: {t:?}");
        assert!(t.iter().any(|l| l.starts_with("❯ A")) && t.iter().any(|l| l.starts_with("  C")), "{t:?}");
    }

    #[test]
    fn described_note_shows_under_the_highlighted_option() {
        let mut s = described(&[("a", "one"), ("b", "two")]).notes().note_text("later");
        let (lines, _) = s.render(40, 20);
        let t = texts(&lines);
        assert_eq!(t[3], "    one", "{t:?}");
        assert_eq!(t[4], "    note: later", "{t:?}");
        s.on_key(key(KeyCode::Down));
        let (lines, _) = s.render(40, 20);
        let t = texts(&lines);
        assert!(t.iter().any(|l| l == "    note: later"), "the note stays on the picked option's block: {t:?}");
    }

    #[test]
    fn described_tiny_heights_keep_the_input_and_the_highlighted_option() {
        let long = "word ".repeat(40);
        let mut s =
            described(&[("a", &long), ("b", &long), ("c", &long)]).previews(vec![Some("x\ny\nz".into()), None, None]);
        let labels = ["a", "b", "c"];
        for _ in 0..3 {
            for max_rows in 1..=8 {
                for width in [1, 2, 3, 8, 30] {
                    let (lines, cur) = s.render(width, max_rows);
                    let t = texts(&lines);
                    assert!(lines.len() <= max_rows, "rows {max_rows}: {t:?}");
                    assert!(lines.iter().all(|l| line::line_width(l) <= width), "width {width}: {t:?}");
                    assert!(cur.0 < max_rows.max(1) && cur.1 < width.max(1), "cursor {cur:?}: {t:?}");
                    if max_rows >= 2 && width >= 3 {
                        let hl = labels[s.sel.min(2)];
                        assert!(t.iter().any(|l| l.starts_with(&format!("❯ {hl}"))), "rows {max_rows}: {t:?}");
                        assert!(t.iter().any(|l| l.starts_with('›')), "the filter line shows: {t:?}");
                    }
                }
            }
            s.on_key(key(KeyCode::Down));
        }
    }

    #[test]
    fn free_text_wraps_over_rows_and_the_cursor_follows_it() {
        let mut s = Select::new("t", items(&["a"])).free_text("Other");
        s.on_key(key(KeyCode::Down));
        s.on_key(key(KeyCode::Enter));
        type_str(&mut s, &"abcdefghij".repeat(3));
        let (lines, cur) = s.render(12, 20);
        let t = texts(&lines);
        assert_eq!(t[1], "› abcdefghij", "{t:?}");
        assert_eq!(t[2], "  abcdefghij", "{t:?}");
        assert_eq!(t[3], "  abcdefghij", "{t:?}");
        assert_eq!(t[4], "", "the cursor's own row after a full one: {t:?}");
        assert_eq!(cur, (4, 2), "{t:?}");
        assert!(lines.iter().all(|l| line::line_width(l) <= 12));
    }

    #[test]
    fn free_text_scrolls_to_keep_the_cursor_visible_when_short_of_rows() {
        let mut s = Select::new("t", items(&["a"])).free_text("Other");
        s.on_key(key(KeyCode::Down));
        s.on_key(key(KeyCode::Enter));
        type_str(&mut s, &format!("{}abcde", "abcdefghij".repeat(3)));
        let (lines, cur) = s.render(12, 3);
        let t = texts(&lines);
        assert_eq!(lines.len(), 3, "{t:?}");
        assert_eq!(t[1], "  abcde", "the cursor's row is the one shown: {t:?}");
        assert_eq!(cur, (1, 7), "{t:?}");
    }

    #[test]
    fn free_text_newlines_come_from_alt_enter_and_paste() {
        let mut s = Select::new("t", items(&["a"])).free_text("Other");
        s.on_key(key(KeyCode::Down));
        s.on_key(key(KeyCode::Enter));
        type_str(&mut s, "one");
        s.on_key(alt(KeyCode::Enter));
        type_str(&mut s, "two");
        s.on_paste("\nthree\r\nfour");
        let (lines, _) = s.render(40, 20);
        let t = texts(&lines);
        assert_eq!(t[1], "› one", "{t:?}");
        assert_eq!(t[2], "  two", "{t:?}");
        assert_eq!(t[3], "  three", "{t:?}");
        assert_eq!(t[4], "  four", "{t:?}");
        match s.on_key(key(KeyCode::Enter)) {
            SelectResult::Text(text) => assert_eq!(text, "one\ntwo\nthree\nfour"),
            _ => panic!("expected text"),
        }
    }

    #[test]
    fn free_text_takes_the_word_jumps_and_kill_keys() {
        let mut s = Select::new("t", items(&["a"])).free_text("Other");
        s.on_key(key(KeyCode::Down));
        s.on_key(key(KeyCode::Enter));
        type_str(&mut s, "alpha beta gamma");
        s.on_key(alt(KeyCode::Char('b')));
        s.on_key(ctrl('w'));
        type_str(&mut s, "X");
        match s.on_key(key(KeyCode::Enter)) {
            SelectResult::Text(text) => assert_eq!(text, "alpha Xgamma"),
            _ => panic!("expected text"),
        }
    }

    #[test]
    fn free_text_draft_stays_on_the_row_after_esc() {
        let mut s = Select::new("t", items(&["a"])).free_text("Other");
        s.on_key(key(KeyCode::Down));
        s.on_key(key(KeyCode::Enter));
        type_str(&mut s, "draft");
        s.on_key(key(KeyCode::Esc));
        let (lines, _) = s.render(40, 20);
        assert!(texts(&lines).iter().any(|t| t.starts_with("❯ draft")), "{:?}", texts(&lines));
        s.on_key(key(KeyCode::Enter));
        type_str(&mut s, "!");
        assert!(matches!(s.on_key(key(KeyCode::Enter)), SelectResult::Text(t) if t == "draft!"));
    }

    #[test]
    fn prefilled_text_opens_on_the_free_row_with_its_answer() {
        let mut s = Select::new("t", items(&["a", "b"])).free_text("Other").prefill_text("kept");
        let (lines, _) = s.render(40, 20);
        assert!(texts(&lines).iter().any(|t| t.starts_with("❯ kept")), "{:?}", texts(&lines));
        s.on_key(key(KeyCode::Enter));
        assert!(matches!(s.on_key(key(KeyCode::Enter)), SelectResult::Text(t) if t == "kept"));
    }

    #[test]
    fn free_text_paste_in_the_filter_becomes_one_line() {
        let mut s = Select::new("t", items(&["new york", "newark"]));
        s.on_paste("new\nyork");
        assert_eq!(chosen(s.on_key(key(KeyCode::Enter))), Some(vec![0]));
    }
}
