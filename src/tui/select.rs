//! Filterable list overlay drawn in the composer slot.

use std::collections::BTreeSet;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::style::ContentStyle;
use unicode_width::UnicodeWidthStr;

use super::line::{self, Line, bold, dim, span};
use super::theme::{self, Role};

fn accent_bold() -> ContentStyle {
    let mut s = theme::get().fg(Role::Accent);
    s.attributes.set(crossterm::style::Attribute::Bold);
    s
}

pub struct Item {
    pub label: String,
    pub detail: String,
}

pub enum SelectResult {
    Pending,
    Cancelled,
    /// Indices into the original items, ascending.
    Chosen(Vec<usize>),
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
    filter: String,
    /// Highlighted row position in the filtered row list.
    sel: usize,
    /// Original indices toggled (multi) or preselected.
    checked: BTreeSet<usize>,
    /// True while typing into the free-text row.
    editing: bool,
    text: String,
    /// Lowercased `label detail` of each item, built once.
    haystacks: Vec<String>,
    /// The filtered rows for the filter they were computed with; reused until the filter changes.
    rows_cache: std::cell::RefCell<Option<(String, std::rc::Rc<Vec<Row>>)>>,
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
            filter: String::new(),
            sel: 0,
            checked: BTreeSet::new(),
            editing: false,
            text: String::new(),
        }
    }

    pub fn multi(mut self) -> Self {
        self.multi = true;
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

    pub fn on_key(&mut self, key: KeyEvent) -> SelectResult {
        if key.kind == KeyEventKind::Release {
            return SelectResult::Pending;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('c') {
            return SelectResult::Cancelled;
        }
        if self.editing {
            return self.text_key(key, ctrl);
        }
        let rows = self.rows();
        let n = rows.len();
        self.sel = self.sel.min(n.saturating_sub(1));
        match key.code {
            KeyCode::Esc => SelectResult::Cancelled,
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
            KeyCode::Backspace => {
                self.filter.pop();
                self.sel = 0;
                SelectResult::Pending
            }
            KeyCode::Char(c) if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => {
                self.filter.push(c);
                self.sel = 0;
                SelectResult::Pending
            }
            _ => SelectResult::Pending,
        }
    }

    fn text_key(&mut self, key: KeyEvent, ctrl: bool) -> SelectResult {
        match key.code {
            KeyCode::Esc => {
                self.editing = false;
                self.text.clear();
                SelectResult::Pending
            }
            KeyCode::Enter => {
                if self.text.trim().is_empty() {
                    SelectResult::Pending
                } else {
                    SelectResult::Text(std::mem::take(&mut self.text))
                }
            }
            KeyCode::Backspace => {
                self.text.pop();
                SelectResult::Pending
            }
            KeyCode::Char('u') if ctrl => {
                self.text.clear();
                SelectResult::Pending
            }
            KeyCode::Char(c) if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => {
                self.text.push(c);
                SelectResult::Pending
            }
            _ => SelectResult::Pending,
        }
    }

    /// Replaces line breaks and tabs with spaces and drops other control characters.
    pub fn on_paste(&mut self, s: &str) {
        let clean = clean_paste(s);
        if self.editing {
            self.text.push_str(&clean);
        } else {
            self.filter.push_str(&clean);
            self.sel = 0;
        }
    }

    /// Lines (title, filter/input, rows) fitted to `max_rows`, and the cursor (row, col).
    pub fn render(&self, width: usize, max_rows: usize) -> (Vec<Line>, (usize, usize)) {
        let width = width.max(1);
        let input = if self.editing { &self.text } else { &self.filter };
        let mut out: Vec<Line> =
            vec![line::styled(self.title.clone(), |_| accent_bold()), line::plain(format!("› {input}"))];
        let cursor = (1, (2 + input.width()).min(width - 1));

        if self.editing {
            if out.len() < max_rows {
                out.push(line::styled("Enter submit · Esc back", |_| dim()));
            }
        } else {
            let rows = self.rows();
            let n = rows.len();
            let avail = max_rows.saturating_sub(out.len());
            if n == 0 {
                if avail > 0 {
                    out.push(line::styled("  no matches", |_| dim()));
                }
            } else if avail > 0 {
                let sel = self.sel.min(n - 1);
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

        let out = out.iter().map(|l| line::truncate(l, width)).collect();
        (out, cursor)
    }

    /// The rows for the current filter, recomputed only when the filter changed.
    fn rows(&self) -> std::rc::Rc<Vec<Row>> {
        let mut cache = self.rows_cache.borrow_mut();
        if let Some((f, rows)) = cache.as_ref()
            && *f == self.filter
        {
            return rows.clone();
        }
        let q = self.filter.to_lowercase();
        let mut rows: Vec<Row> =
            (0..self.items.len()).filter(|&i| q.is_empty() || self.haystacks[i].contains(&q)).map(Row::Item).collect();
        if self.free.is_some() {
            rows.push(Row::Free);
        }
        let rows = std::rc::Rc::new(rows);
        *cache = Some((self.filter.clone(), rows.clone()));
        rows
    }

    fn step(&mut self, delta: isize, n: usize) {
        if n > 0 {
            self.sel = (self.sel as isize + delta).rem_euclid(n as isize) as usize;
        }
    }

    fn choose(&mut self, rows: &[Row]) -> SelectResult {
        if self.multi && !self.checked.is_empty() {
            return SelectResult::Chosen(self.checked.iter().copied().collect());
        }
        match rows.get(self.sel) {
            Some(Row::Item(i)) => SelectResult::Chosen(vec![*i]),
            Some(Row::Free) => {
                self.editing = true;
                SelectResult::Pending
            }
            None => SelectResult::Pending,
        }
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
                if !it.detail.is_empty() {
                    l.push(span("  ", ContentStyle::default()));
                    l.push(span(it.detail.clone(), dim()));
                }
            }
            Row::Free => {
                if self.multi {
                    l.push(span("    ", ContentStyle::default()));
                }
                l.push(span(self.free.clone().unwrap_or_default(), emphasis));
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
    let cost = |a: usize, b: usize| (b - a) + (a > 0) as usize + (b < n) as usize;
    if n <= avail {
        return (0, n, false);
    }
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

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    fn type_str(s: &mut Select, t: &str) {
        for c in t.chars() {
            s.on_key(key(KeyCode::Char(c)));
        }
    }

    fn items(labels: &[&str]) -> Vec<Item> {
        labels.iter().map(|l| Item { label: l.to_string(), detail: String::new() }).collect()
    }

    fn texts(lines: &[Line]) -> Vec<String> {
        lines.iter().map(line::line_text).collect()
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
}
