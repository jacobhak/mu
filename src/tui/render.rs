//! Inline renderer: committed lines scroll into native scrollback; the viewport is redrawn in place.
//!
//! Every draw starts by moving the cursor to the top of the previously drawn viewport using the
//! physical row layout at the current width, so a resize shrink still lands correctly. Committed
//! lines are never re-wrapped.

use std::io::{self, Write};

use crossterm::cursor::{MoveDown, MoveToColumn, MoveUp};
use crossterm::queue;
use crossterm::terminal::{self, BeginSynchronizedUpdate, Clear, ClearType, EndSynchronizedUpdate};

use super::line::{self, Line};

#[derive(Default)]
pub struct Renderer {
    /// Display width of each viewport row as last printed.
    drawn: Vec<usize>,
    /// Cursor as (row index into `drawn`, column) as last placed.
    cursor: (usize, usize),
}

impl Renderer {
    pub fn new() -> Self {
        Self::default()
    }

    /// (cols, rows), queried fresh.
    pub fn size(&self) -> (usize, usize) {
        let (c, r) = terminal::size().unwrap_or((80, 24));
        (c.max(1) as usize, r.max(1) as usize)
    }

    /// Draws committed lines (scrolled into scrollback) followed by the viewport. The cursor is
    /// placed at `cursor` = (viewport row, display column) in the given viewport coordinates.
    pub fn draw(
        &mut self,
        committed: &[Line],
        viewport: &[Line],
        cursor: (usize, usize),
    ) -> io::Result<()> {
        let (cols, rows) = self.size();
        let (up, _) = self.positions(cols);
        let mut buf = Vec::new();
        queue!(buf, BeginSynchronizedUpdate)?;
        if up > 0 {
            queue!(buf, MoveUp(u16_of(up)))?;
        }
        buf.push(b'\r');
        for l in committed {
            for w in line::wrap(l, cols) {
                queue!(buf, Clear(ClearType::CurrentLine))?;
                buf.extend_from_slice(line::to_ansi(&w).as_bytes());
                buf.extend_from_slice(b"\r\n");
            }
        }

        let mut wrapped: Vec<Line> = Vec::new();
        let mut at = None;
        for (i, l) in viewport.iter().enumerate() {
            let base = wrapped.len();
            if i == cursor.0 {
                let (sub, col) = locate(l, cols, cursor.1);
                at = Some((base + sub, col));
            }
            wrapped.extend(line::wrap(l, cols));
        }

        // Keep the last rows when the viewport is taller than the space left (rows - 1).
        let cap = rows.saturating_sub(1).max(1);
        let skip = wrapped.len().saturating_sub(cap);
        let shown = &wrapped[skip..];
        let (crow, ccol) = match at {
            Some((r, c)) if r >= skip => (r - skip, c),
            _ => (shown.len().saturating_sub(1), 0),
        };

        self.drawn.clear();
        for (i, l) in shown.iter().enumerate() {
            if i > 0 {
                buf.extend_from_slice(b"\r\n");
            }
            queue!(buf, Clear(ClearType::CurrentLine))?;
            buf.extend_from_slice(line::to_ansi(l).as_bytes());
            self.drawn.push(line::line_width(l));
        }
        // Erase leftovers of a taller previous viewport. Per-line clears above avoid ED at the
        // top-left, which some terminals (tmux) treat as a full clear and archive to scrollback.
        queue!(buf, Clear(ClearType::FromCursorDown))?;
        self.cursor = (crow, ccol.min(cols - 1));

        // The last viewport row has no trailing newline, so the cursor is on it now.
        let (cur, total) = self.positions(cols);
        if total > 0 && total - 1 > cur {
            queue!(buf, MoveUp(u16_of(total - 1 - cur)))?;
        }
        queue!(buf, MoveToColumn(self.cursor.1 as u16))?;
        queue!(buf, EndSynchronizedUpdate)?;

        let mut out = io::stdout().lock();
        out.write_all(&buf)?;
        out.flush()
    }

    /// Moves below the viewport and ends the line so the shell prompt resumes cleanly.
    pub fn finish(&mut self) -> io::Result<()> {
        let (cols, _) = self.size();
        let (cur, total) = self.positions(cols);
        let mut buf = Vec::new();
        if total > 0 {
            if total - 1 > cur {
                queue!(buf, MoveDown(u16_of(total - 1 - cur)))?;
            }
            buf.extend_from_slice(b"\r\n");
        } else {
            buf.push(b'\r');
        }
        let mut out = io::stdout().lock();
        out.write_all(&buf)?;
        out.flush()?;
        self.drawn.clear();
        self.cursor = (0, 0);
        Ok(())
    }

    /// (physical rows from the viewport top to the cursor, total physical rows) at `cols`.
    fn positions(&self, cols: usize) -> (usize, usize) {
        let mut before = 0;
        let mut total = 0;
        for (i, &w) in self.drawn.iter().enumerate() {
            let p = w.div_ceil(cols).max(1);
            if i < self.cursor.0 {
                before += p;
            }
            total += p;
        }
        let cur = match self.drawn.get(self.cursor.0) {
            Some(&w) => before + self.cursor.1.min(w.saturating_sub(1)) / cols,
            None => 0,
        };
        (cur, total)
    }
}

fn u16_of(n: usize) -> u16 {
    n.min(u16::MAX as usize) as u16
}

/// Maps display column `target` of an unwrapped line to (sub-row, column) under `wrap` at `cols`.
fn locate(l: &Line, cols: usize, target: usize) -> (usize, usize) {
    use unicode_width::UnicodeWidthChar;
    let (mut row, mut col, mut disp) = (0, 0, 0);
    for ch in l.iter().flat_map(|s| s.text.chars()) {
        if disp >= target {
            break;
        }
        let w = ch.width().unwrap_or(0);
        if col + w > cols {
            row += 1;
            col = 0;
        }
        col += w;
        disp += w;
    }
    (row, col)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::line::plain;

    #[test]
    fn locate_follows_wrap() {
        assert_eq!(locate(&plain("abcdef"), 4, 5), (1, 1));
        assert_eq!(locate(&plain("abcdef"), 4, 0), (0, 0));
        assert_eq!(locate(&plain("abcdef"), 4, 4), (0, 4));
    }

    #[test]
    fn positions_account_for_reflow_on_shrink() {
        let r = Renderer { drawn: vec![10, 3], cursor: (1, 2) };
        // At 4 cols the 10-wide row takes 3 physical rows; cursor row starts after it.
        assert_eq!(r.positions(4), (3, 4));
        assert_eq!(r.positions(20), (1, 2));
    }
}
