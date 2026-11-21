//! How a `Select` draws: title and context, the filtered list, the described layout.

use super::preview::{INDENT, PREVIEW_MAX_LINES, indented, preview_box};
use super::{Mode, Row, Select};
use crate::text::accent;
use crate::text::line::{self, Line, bold, dim, span};
use crate::text::markdown::{chars_to_line, chs, wrap_chs};
use crate::theme::{self, Role};
use crossterm::style::ContentStyle;

/// Rows the title may take; a longer one ends in `…`.
pub(super) const TITLE_MAX_LINES: usize = 6;

/// Lines a description shows under its option, when the option is highlighted or the list fits.
const DESC_MAX_LINES: usize = 6;

/// Rows the context above the title may take at once.
const CONTEXT_MAX_LINES: usize = 8;

/// Rows a highlighted item's detail text may take under the list.
const DETAIL_MAX_LINES: usize = 6;

const TEXT_HINT: &str = "Enter submit · Shift/Alt/Ctrl-Enter newline · Esc to options";

impl Select {
    /// Lines (title, input, rows) fitted to `max_rows`, and the cursor (row, col).
    pub fn render(&self, width: usize, max_rows: usize) -> (Vec<Line>, (usize, usize)) {
        let (lines, cursor, _) = self.render_inner(width, max_rows);
        (lines, cursor)
    }

    pub fn render_with_hint(&self, width: usize, max_rows: usize) -> (Vec<Line>, (usize, usize), Option<Line>) {
        let (mut lines, cursor, has_hint) = self.render_inner(width, max_rows);
        let hint = if has_hint { lines.pop() } else { None };
        (lines, cursor, hint)
    }

    fn render_inner(&self, width: usize, max_rows: usize) -> (Vec<Line>, (usize, usize), bool) {
        let width = width.max(1);
        if max_rows == 0 {
            self.context_scroll.set(0);
            self.context_scroll_max.set(0);
            return (Vec::new(), (0, 0), false);
        }
        let mut out = self.title_rows(width, max_rows);
        let rows = self.rows();
        let sel = self.sel.min(rows.len().saturating_sub(1));
        let room = max_rows - out.len();
        let mut cursor;
        let has_hint;
        match self.mode {
            Mode::Text => {
                let hint = room >= 2;
                has_hint = hint;
                let (lines, cur) = self.text.view("› ", width, room - usize::from(hint));
                cursor = (out.len() + cur.0, cur.1);
                out.extend(lines);
                if hint {
                    out.push(line::styled(TEXT_HINT, |_| dim()));
                }
            }
            Mode::Note => {
                let hint = room >= 3;
                has_hint = hint;
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
            }
            Mode::List => {
                // Without a filter the cursor rests at the start of the list.
                cursor = (out.len(), 0);
                if self.filterable {
                    let (line, col) = self.filter.view_line("› ", width);
                    cursor = (out.len(), col);
                    out.push(line);
                }
                // The list budget is the room after the filter line; the hint takes the last row only
                // when one list row is left under it.
                let list_room = room - usize::from(self.filterable);
                let hint = self.hint.as_ref().filter(|_| list_room >= 2);
                has_hint = hint.is_some();
                let avail = list_room - usize::from(hint.is_some());
                let detail = self.detail_rows(rows.get(sel).copied(), width, avail.saturating_sub(3));
                let avail = avail - detail.len();
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
                out.extend(detail);
                if let Some(h) = hint {
                    out.push(line::styled(format!("  {h}"), |_| dim()));
                }
            }
        }
        cursor.1 = cursor.1.min(width - 1);
        let out = out.iter().map(|l| line::truncate(l, width)).collect();
        (out, cursor, has_hint)
    }

    /// The highlighted item's detail text, wrapped to `width` and cut to `cap` rows (at most
    /// `DETAIL_MAX_LINES`).
    fn detail_rows(&self, row: Option<Row>, width: usize, cap: usize) -> Vec<Line> {
        let Some(Row::Item(i)) = row else { return Vec::new() };
        let Some(Some(text)) = self.details.get(i) else { return Vec::new() };
        let cap = cap.min(DETAIL_MAX_LINES);
        if cap == 0 {
            return Vec::new();
        }
        let mut rows: Vec<Line> =
            wrap_chs(&chs(text, dim()), width.saturating_sub(2)).iter().map(|r| chars_to_line(r)).collect();
        if rows.len() > cap {
            rows.truncate(cap);
            if let Some(last) = rows.last_mut() {
                last.push(span("…", dim()));
            }
        }
        rows.into_iter()
            .map(|mut l| {
                l.insert(0, span("  ", dim()));
                l
            })
            .collect()
    }

    /// The title wrapped to `width`, honouring its newlines. It takes at most half of `max_rows`
    /// (and `TITLE_MAX_LINES`), and always leaves two rows for the input and the highlighted row. A
    /// cut title ends in `…`.
    fn title_rows(&self, width: usize, max_rows: usize) -> Vec<Line> {
        let cap = (max_rows / 2).max(1).min(max_rows.saturating_sub(2)).min(TITLE_MAX_LINES);
        if cap == 0 {
            self.context_scroll.set(0);
            self.context_scroll_max.set(0);
            return Vec::new();
        }
        let title_style = if self.described { bold() } else { accent() };
        let mut rows: Vec<Line> =
            wrap_chs(&chs(&self.title, title_style), width).iter().map(|r| chars_to_line(r)).collect();
        if rows.is_empty() {
            rows.push(Vec::new());
        }
        if rows.len() > cap {
            rows.truncate(cap);
            if let Some(last) = rows.last_mut() {
                let mut row = last.clone();
                row.push(span("…", accent()));
                *last = line::truncate(&row, width);
            }
        }
        if self.context.is_empty() {
            return rows;
        }
        let room = max_rows.saturating_sub(rows.len() + 2);
        let ctx_cap = room.div_ceil(2).min(CONTEXT_MAX_LINES);
        if ctx_cap == 0 {
            self.context_scroll.set(0);
            self.context_scroll_max.set(0);
            if let Some(first) = rows.first_mut() {
                first.insert(0, span("↕ ", dim()));
            }
            return rows;
        }
        let ctx: Vec<Line> = wrap_chs(&chs(&self.context, ContentStyle::default()), width.saturating_sub(2).max(1))
            .iter()
            .map(|r| chars_to_line(r))
            .collect();
        let max_scroll = ctx.len().saturating_sub(ctx_cap);
        self.context_scroll_max.set(max_scroll);
        let start = self.context_scroll.get().min(max_scroll);
        self.context_scroll.set(start);
        let end = (start + ctx_cap).min(ctx.len());
        let mut visible = ctx[start..end].to_vec();
        if (start > 0 || end < ctx.len())
            && let Some(last) = visible.last_mut()
        {
            *last = line::truncate(last, width.saturating_sub(2));
            last.push(span(" ↕", dim()));
        }
        visible.extend(rows);
        visible
    }

    /// The described list: each option's label, its description under it, then its note (the
    /// highlighted one) and preview box. Heights are planned to fit `avail` rows. The highlighted
    /// option keeps its label, full description and as much preview as fits; the others' previews
    /// and descriptions give way first (shorter, then one line, then none).
    fn described_list(&self, rows: &[Row], sel: usize, width: usize, avail: usize) -> Vec<Line> {
        let n = rows.len();
        let detail = |pos: usize| match rows[pos] {
            Row::Item(i) => self.items[i].detail.as_str(),
            Row::Free | Row::Skip => "",
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
                    Row::Free | Row::Skip => Vec::new(),
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
            Row::Skip => {
                if self.multi {
                    l.push(span("    ", ContentStyle::default()));
                }
                l.push(span(self.skip.clone().unwrap_or_default(), if selected { emphasis } else { dim() }));
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
pub(super) fn window(n: usize, sel: usize, avail: usize) -> (usize, usize, bool) {
    window_by(&vec![1; n], sel, avail)
}

/// `window` for rows of differing heights `h`: the range around `sel` whose heights and indicators
/// fit `avail`. The selected row is always in it, so `h[sel]` must fit on its own.
pub(super) fn window_by(h: &[usize], sel: usize, avail: usize) -> (usize, usize, bool) {
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
