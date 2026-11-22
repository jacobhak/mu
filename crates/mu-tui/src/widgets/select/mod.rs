//! Filterable list overlay drawn in the composer slot.

mod layout;
mod preview;

use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;
use std::rc::Rc;

use crate::keymap;
use crate::widgets::editor::{self, Editor};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use mu_base::events::PreviewKind;

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
    /// The skip row was chosen (see `skip_row`).
    Skipped,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Row {
    Item(usize),
    Free,
    Skip,
}

/// What the keys go to.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Mode {
    /// The filter and the list.
    List,
    /// The free-text answer.
    Text,
    /// The note on the highlighted row.
    Note,
}

pub struct Select {
    title: String,
    /// Paragraph above the title: what the question is about.
    context: String,
    context_scroll: Cell<usize>,
    context_scroll_max: Cell<usize>,
    items: Vec<Item>,
    multi: bool,
    free: Option<String>,
    /// Label of the last row, which declines the question: Enter on it returns `SelectResult::Skipped`.
    skip: Option<String>,
    /// The filter line. Single line: Enter and newlines are the caller's.
    filter: Editor,
    /// Whether typing filters the list. Off, the filter line is not drawn and typed text is ignored.
    filterable: bool,
    /// Highlighted row position in the filtered row list.
    sel: usize,
    /// Original indices toggled (multi) or preselected.
    checked: BTreeSet<usize>,
    mode: Mode,
    /// The free-text answer. Kept after Esc, so the row shows it and Enter edits it again.
    text: Editor,
    /// Lowercased `label detail` of each item, built once.
    haystacks: Vec<String>,
    /// The filtered rows for the filter they were computed with; reused until the filter changes.
    rows_cache: RefCell<Option<(String, Rc<Vec<Row>>)>>,
    /// Note key enabled: Tab, or Right (with the filter empty), edits a note.
    notes: bool,
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
    /// One entry per item: dim lines under the list while that item is highlighted (compact layout).
    details: Vec<Option<String>>,
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
            context: String::new(),
            context_scroll: Cell::new(0),
            context_scroll_max: Cell::new(0),
            items,
            multi: false,
            free: None,
            skip: None,
            filter: Editor::default(),
            filterable: true,
            sel: 0,
            checked: BTreeSet::new(),
            mode: Mode::List,
            text: Editor::default(),
            notes: false,
            note: Editor::default(),
            note_before: String::new(),
            quick: Vec::new(),
            hint: None,
            previews: Vec::new(),
            kinds: Vec::new(),
            details: Vec::new(),
            described: false,
            back: false,
        }
    }

    pub fn multi(mut self) -> Self {
        self.multi = true;
        self
    }

    /// No filter: the list is not narrowed by typing, so letters, and Space in multi-select, are the caller's keys.
    pub fn unfiltered(mut self) -> Self {
        self.filterable = false;
        self
    }

    /// The ask layout (see `described`).
    pub fn described(mut self) -> Self {
        self.described = true;
        self
    }

    /// A paragraph above the title that says what the question is about.
    pub fn context(mut self, text: &str) -> Self {
        self.context = text.trim().to_string();
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

    /// Adds a last row that declines the question.
    pub fn skip_row(mut self, label: &str) -> Self {
        self.skip = Some(label.to_string());
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

    /// Opens the free-text row for editing with `s` already typed, and its cursor at the end. Unlike
    /// `prefill_text` the row is live, so Enter submits and Esc returns to the list.
    pub fn edit_text(mut self, s: &str) -> Self {
        if self.free.is_some() {
            self.text = Editor::from_text(s);
            self.sel = self.items.len();
            self.mode = Mode::Text;
        }
        self
    }

    /// Highlights the skip row.
    pub fn prefill_skip(mut self) -> Self {
        if self.skip.is_some() {
            self.sel = self.items.len() + usize::from(self.free.is_some());
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

    /// Text shown under the list while the item at the same index is highlighted.
    pub fn details(mut self, details: Vec<Option<String>>) -> Self {
        self.details = details;
        self
    }

    pub fn preview_kinds(mut self, kinds: Vec<Option<PreviewKind>>) -> Self {
        self.kinds = kinds;
        self
    }

    /// The original index of the highlighted item, or `None` when the highlight is on the free-text row.
    pub fn highlighted(&self) -> Option<usize> {
        match self.rows().get(self.sel) {
            Some(Row::Item(i)) => Some(*i),
            _ => None,
        }
    }

    /// Whether the filter holds text, so that typed keys are the filter's rather than the caller's.
    pub fn is_filtering(&self) -> bool {
        !self.filter.is_empty()
    }

    /// Whether the free-text row is open for typing.
    pub fn is_editing(&self) -> bool {
        self.mode == Mode::Text
    }

    /// The note or the free-text row takes keystrokes, so they are not shortcuts.
    pub fn is_typing(&self) -> bool {
        self.mode != Mode::List
    }

    /// Moves the highlight to item `idx`; dialogs that use it keep the filter empty.
    pub fn select_row(&mut self, idx: usize) {
        self.sel = idx.min(self.items.len().saturating_sub(1));
    }

    pub fn on_key(&mut self, key: KeyEvent) -> SelectResult {
        if key.kind == KeyEventKind::Release {
            return SelectResult::Pending;
        }
        if !self.context.is_empty() {
            if keymap::SELECT_CONTEXT_UP.hit(&key) {
                self.context_scroll.set(self.context_scroll.get().saturating_sub(1));
                return SelectResult::Pending;
            }
            if keymap::SELECT_CONTEXT_DOWN.hit(&key) {
                self.context_scroll.set(self.context_scroll.get().saturating_add(1).min(self.context_scroll_max.get()));
                return SelectResult::Pending;
            }
        }
        match self.mode {
            Mode::Text => return self.text_key(key),
            Mode::Note => return self.note_key(key),
            Mode::List => {}
        }
        let rows = self.rows();
        let n = rows.len();
        self.sel = self.sel.min(n.saturating_sub(1));
        if let KeyCode::Char(c) = key.code
            && !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
            && self.filter.is_empty()
            && !self.multi
            && let Some(i) = self.quick_item(c)
        {
            return self.finish(vec![i]);
        }
        if keymap::SELECT_CANCEL.hit(&key) {
            SelectResult::Cancelled
        } else if self.back && keymap::SELECT_BACK.hit(&key)
            || self.back && self.filter.is_empty() && keymap::SELECT_BACK_EMPTY.hit(&key)
        {
            SelectResult::Back
        } else if keymap::SELECT_UP.hit(&key) {
            self.step(-1, n);
            SelectResult::Pending
        } else if keymap::SELECT_DOWN.hit(&key) {
            self.step(1, n);
            SelectResult::Pending
        } else if keymap::SELECT_CHOOSE.hit(&key) {
            self.choose(&rows)
        } else if self.notes && self.filter.is_empty() && keymap::SELECT_NOTE_EMPTY.hit(&key)
            || self.notes && keymap::SELECT_NOTE.hit(&key)
        {
            self.start_note(&rows)
        } else if self.multi && self.filter.is_empty() && keymap::SELECT_TOGGLE.hit(&key) {
            // Space toggles only while the filter is empty (always, when unfiltered); with text in
            // it, Space is part of the filter.
            if let Some(Row::Item(i)) = rows.get(self.sel)
                && !self.checked.remove(i)
            {
                self.checked.insert(*i);
            }
            SelectResult::Pending
        } else {
            if self.filterable && self.filter.key(key, false) == editor::Edit::Changed {
                self.sel = 0;
            }
            SelectResult::Pending
        }
    }

    /// Typing the free-text answer: Enter submits it, Esc goes back to the list.
    fn text_key(&mut self, key: KeyEvent) -> SelectResult {
        if keymap::SELECT_CANCEL.hit(&key) {
            self.mode = Mode::List;
            SelectResult::Pending
        } else if keymap::SUBMIT.hit(&key) {
            if self.text.is_blank() {
                SelectResult::Pending
            } else if self.multi && !self.checked.is_empty() {
                SelectResult::Noted(self.checked.iter().copied().collect(), self.text.text())
            } else {
                SelectResult::Text(self.text.text())
            }
        } else {
            self.text.key(key, true);
            SelectResult::Pending
        }
    }

    /// Typing the note on the highlighted row: Enter submits it with the picks, Esc restores the
    /// note as it was and goes back to the list.
    fn note_key(&mut self, key: KeyEvent) -> SelectResult {
        if keymap::SELECT_CANCEL.hit(&key) {
            self.mode = Mode::List;
            self.note = Editor::from_text(&self.note_before);
            SelectResult::Pending
        } else if keymap::SUBMIT.hit(&key) {
            self.mode = Mode::List;
            let rows = self.rows();
            self.choose(&rows)
        } else {
            self.note.key(key, true);
            SelectResult::Pending
        }
    }

    fn start_note(&mut self, rows: &[Row]) -> SelectResult {
        if matches!(rows.get(self.sel), Some(Row::Item(_))) {
            self.mode = Mode::Note;
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
        match self.mode {
            Mode::Text => self.text.insert_str(&text),
            Mode::Note => self.note.insert_str(&text),
            Mode::List if self.filterable => {
                self.filter.insert_str(&text.replace('\n', " "));
                self.sel = 0;
            }
            Mode::List => {}
        }
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
        if self.skip.is_some() {
            rows.push(Row::Skip);
        }
        let rows = Rc::new(rows);
        *cache = Some((filter, rows.clone()));
        rows
    }

    fn step(&mut self, delta: isize, n: usize) {
        if n > 0 {
            self.sel = (self.sel as isize + delta).rem_euclid(n as isize) as usize;
        }
    }

    /// Picks the highlighted row, or the checked rows in multi mode. The free-text row opens its
    /// editor, even with rows checked: they are then submitted with the typed text as its note.
    fn choose(&mut self, rows: &[Row]) -> SelectResult {
        if rows.get(self.sel) == Some(&Row::Skip) {
            return SelectResult::Skipped;
        }
        if self.multi && !self.checked.is_empty() && rows.get(self.sel) != Some(&Row::Free) {
            return self.finish(self.checked.iter().copied().collect());
        }
        match rows.get(self.sel) {
            Some(Row::Item(i)) => self.finish(vec![*i]),
            Some(Row::Free) => {
                self.mode = Mode::Text;
                SelectResult::Pending
            }
            Some(Row::Skip) | None => SelectResult::Pending,
        }
    }

    /// The picks, with the note when there is one.
    fn finish(&self, picks: Vec<usize>) -> SelectResult {
        let note = self.note.text().trim().to_string();
        if self.notes && !note.is_empty() { SelectResult::Noted(picks, note) } else { SelectResult::Chosen(picks) }
    }
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
    use super::layout::{TITLE_MAX_LINES, window};
    use super::preview::{PREVIEW_MAX_LINES, preview_kind};
    use super::*;
    use crate::text::line::{self, Line};
    use crate::theme::{self, Role};
    use mu_testutil::key;

    #[test]
    fn context_scroll_clamps_and_reverses_after_resize() {
        let context = (0..12).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        let mut s = Select::new("title", items(&["a"])).context(&context);
        s.render(40, 10);
        for _ in 0..30 {
            s.on_key(key(KeyCode::PageDown));
        }
        assert_eq!(s.context_scroll.get(), 8);
        assert!(texts(&s.render(40, 10).0)[0].contains("line 8"));
        s.on_key(key(KeyCode::PageUp));
        assert_eq!(s.context_scroll.get(), 7);
        for _ in 0..30 {
            s.on_key(key(KeyCode::PageUp));
        }
        s.on_key(key(KeyCode::PageDown));
        assert_eq!(s.context_scroll.get(), 1);
        for _ in 0..30 {
            s.on_key(key(KeyCode::PageDown));
        }
        s.render(40, 20);
        assert_eq!(s.context_scroll.get(), 4);
        s.on_key(key(KeyCode::PageUp));
        assert_eq!(s.context_scroll.get(), 3);
        s.render(40, 3);
        assert_eq!(s.context_scroll.get(), 0);
        s.on_key(key(KeyCode::PageDown));
        assert_eq!(s.context_scroll.get(), 0);
        s.render(40, 10);
        s.on_key(key(KeyCode::PageDown));
        assert_eq!(s.context_scroll.get(), 1);
    }

    #[test]
    fn context_scroll_reclamps_when_width_changes() {
        let mut s = Select::new("title", items(&["a"])).context("abcdefghij abcdefghij abcdefghij abcdefghij");
        s.render(8, 6);
        for _ in 0..30 {
            s.on_key(key(KeyCode::PageDown));
        }
        assert!(s.context_scroll.get() > 0);
        s.render(80, 6);
        assert_eq!(s.context_scroll.get(), 0);
        s.on_key(key(KeyCode::PageDown));
        assert_eq!(s.context_scroll.get(), 0);
        s.render(8, 6);
        s.on_key(key(KeyCode::PageDown));
        assert_eq!(s.context_scroll.get(), 1);
    }

    #[test]
    fn context_scroll_works_while_editing_text_and_notes() {
        let context = (0..12).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        let mut text = Select::new("title", items(&["a"])).context(&context).free_text("other").edit_text("answer");
        let mut note = Select::new("title", items(&["a"])).context(&context).notes();
        note.on_key(key(KeyCode::Right));
        type_str(&mut note, "note");
        for s in [&mut text, &mut note] {
            s.render(40, 10);
            for _ in 0..30 {
                s.on_key(key(KeyCode::PageDown));
            }
            assert_eq!(s.context_scroll.get(), 8);
            s.on_key(key(KeyCode::PageUp));
            assert_eq!(s.context_scroll.get(), 7);
            assert!(texts(&s.render(40, 10).0)[0].contains("line 7"));
        }
        assert_eq!(text.text.text(), "answer");
        assert_eq!(note.note.text(), "note");
        assert!(text.mode == Mode::Text && note.mode == Mode::Note);
    }

    #[test]
    fn code_previews_are_syntax_highlighted() {
        let s = described(&[("a", "")]).previews(vec![Some("fn main() {\n    run();\n}".into())]);
        let (lines, _) = s.render(40, 30);
        let kw = theme::get().color(Role::SynKeyword);
        let row = lines.iter().find(|l| line::line_text(l).contains("fn main")).expect("code row");
        assert!(row.iter().any(|sp| sp.text.contains("fn") && sp.style.foreground_color == kw), "{row:?}");
        assert!(lines.iter().all(|l| line::line_width(l) <= 40));
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
    fn rebinding_select_keys_replaces_the_defaults() {
        let f5 = key(KeyCode::F(5));
        let f6 = key(KeyCode::F(6));
        let f7 = key(KeyCode::F(7));
        crate::keymap::with_keys(
            &[("select", "move_down", &["f5"]), ("select", "choose", &["f6"]), ("select", "cancel", &["f7"])],
            || {
                let mut s = Select::new("t", items(&["a", "b", "c"]));
                s.on_key(key(KeyCode::Down));
                assert_eq!(chosen(s.on_key(key(KeyCode::Enter))), None, "Enter no longer chooses");
                s.on_key(f5);
                assert_eq!(chosen(s.on_key(f6)), Some(vec![1]));
                assert!(matches!(s.on_key(key(KeyCode::Esc)), SelectResult::Pending));
                assert!(matches!(s.on_key(f7), SelectResult::Cancelled));
            },
        );
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
    fn multi_other_with_rows_checked_opens_the_editor_and_keeps_the_picks() {
        let mut s = Select::new("t", items(&["a", "b"])).multi().free_text("Other");
        s.on_key(key(KeyCode::Char(' ')));
        s.on_key(key(KeyCode::Down));
        s.on_key(key(KeyCode::Down));
        assert!(matches!(s.on_key(key(KeyCode::Enter)), SelectResult::Pending));
        type_str(&mut s, "hi");
        match s.on_key(key(KeyCode::Enter)) {
            SelectResult::Noted(picks, t) => assert_eq!((picks, t.as_str()), (vec![0], "hi")),
            _ => panic!("expected the picks with the text"),
        }
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
    fn edit_text_opens_the_free_row_on_its_value() {
        // A field: no rows of its own, its value typed in place, Enter submits it.
        let mut s = Select::new("Provider id", Vec::new()).free_text("Provider id").edit_text("modal");
        assert!(s.is_editing(), "the row is live from the start");
        let (rows, _) = s.render(20, 6);
        let drawn: Vec<String> = rows.iter().map(line::line_text).collect();
        assert!(drawn.iter().any(|l| l.contains("› modal")), "{drawn:?}");
        s.on_key(key(KeyCode::Char('!')));
        match s.on_key(key(KeyCode::Enter)) {
            SelectResult::Text(t) => assert_eq!(t, "modal!"),
            _ => panic!("expected the typed value"),
        }
        // A list with no free row has nothing to open.
        assert!(!Select::new("t", items(&["a"])).edit_text("x").is_editing());
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
    fn space_toggles_only_while_the_filter_is_empty() {
        let mut s = Select::new("t", items(&["new york", "newark"])).multi();
        s.on_key(key(KeyCode::Char(' ')));
        s.on_key(key(KeyCode::Down));
        assert_eq!(chosen(s.on_key(key(KeyCode::Enter))), Some(vec![0]), "empty filter: space toggles");
        let mut s = Select::new("t", items(&["new york", "newark"])).multi();
        type_str(&mut s, "new y");
        s.on_key(key(KeyCode::Char(' ')));
        s.on_key(key(KeyCode::Backspace));
        assert_eq!(
            chosen(s.on_key(key(KeyCode::Enter))),
            Some(vec![0]),
            "the space went into the filter, nothing toggled"
        );
    }

    #[test]
    fn unfiltered_has_no_filter_line_and_space_always_marks() {
        let mut s = Select::new("t", items(&["new york", "newark"])).multi().unfiltered();
        type_str(&mut s, "ne w");
        s.on_key(key(KeyCode::Down));
        s.on_key(key(KeyCode::Char(' ')));
        assert_eq!(texts(&s.render(40, 10).0), ["t", "  [x] new york", "❯ [x] newark"].map(String::from)[..]);
        assert_eq!(chosen(s.on_key(key(KeyCode::Enter))), Some(vec![0, 1]), "typing filtered nothing");
        s.on_paste("zzz");
        assert_eq!(s.rows().len(), 2, "paste does not filter either");
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
    fn the_highlighted_items_details_show_under_the_list_and_give_way_to_it() {
        let mut s =
            Select::new("Pick", items(&["a", "b"])).details(vec![Some("Used by: x\nNot set: uses @y".into()), None]);
        let (lines, _) = s.render(40, 10);
        assert_eq!(texts(&lines), ["Pick", "›", "❯ a", "  b", "  Used by: x", "  Not set: uses @y"]);
        s.on_key(key(KeyCode::Down));
        let (lines, _) = s.render(40, 10);
        assert_eq!(texts(&lines), ["Pick", "›", "  a", "❯ b"], "an item without details shows none");
        s.on_key(key(KeyCode::Up));
        let (lines, _) = s.render(40, 6);
        assert_eq!(texts(&lines), ["Pick", "›", "❯ a", "  b", "  Used by: x…"], "cut to the room");
        let (lines, _) = s.render(14, 20);
        assert!(texts(&lines).iter().all(|l| l.chars().count() <= 14), "wrapped to the width");
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
    fn tab_opens_a_note_and_in_multi_space_marks() {
        let mut s = Select::new("t", ask_items()).notes();
        s.on_key(key(KeyCode::Tab));
        type_str(&mut s, "z");
        assert!(matches!(s.on_key(key(KeyCode::Enter)), SelectResult::Noted(p, n) if p == vec![0] && n == "z"));
        let mut s = Select::new("t", ask_items()).notes().multi();
        s.on_key(key(KeyCode::Char(' ')));
        s.on_key(key(KeyCode::Down));
        s.on_key(key(KeyCode::Char(' ')));
        s.on_key(key(KeyCode::Tab));
        type_str(&mut s, "z");
        assert!(
            matches!(s.on_key(key(KeyCode::Enter)), SelectResult::Noted(p, n) if p == vec![0, 1] && n == "z"),
            "in multi, Tab adds a note to the marked picks"
        );
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
