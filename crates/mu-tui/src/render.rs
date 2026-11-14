//! Inline renderer: committed lines scroll into native scrollback; the viewport is redrawn in place.
//!
//! Every draw starts by moving the cursor to the top of the previously drawn viewport using the
//! physical row layout at the current width, so a resize shrink still lands correctly. Committed
//! lines are never re-wrapped.

use std::io::{self, Write};

use crossterm::cursor::{Hide, MoveDown, MoveToColumn, MoveUp, Show};
use crossterm::queue;
use crossterm::terminal::{self, BeginSynchronizedUpdate, Clear, ClearType, EndSynchronizedUpdate};

use crate::text::line::{self, Line};

#[derive(Default)]
pub struct Renderer {
    /// Display width of each viewport row as last printed.
    drawn: Vec<usize>,
    /// Cursor as (row index into `drawn`, column) as last placed.
    cursor: (usize, usize),
    /// What the last draw put on screen, to skip a redraw that would write the same thing.
    last: Option<Frame>,
    /// A stand-in for the terminal; `None` is stdout and the real size.
    fixed: Option<Fixed>,
}

/// An output and screen size that replace the terminal.
struct Fixed {
    out: Box<dyn Write + Send>,
    size: (usize, usize),
}

/// The viewport as last drawn: one entry per physical row (wrapped to the width), as ANSI text.
#[derive(PartialEq)]
struct Frame {
    size: (usize, usize),
    rows: Vec<Vec<u8>>,
    cursor: (usize, usize),
}

/// Rewrites only the rows of `new` that differ from `old` (same length), starting with the cursor
/// on row `from`, and ends with it at `to`. Moves stay inside the viewport.
fn diff_bytes(old: &[Vec<u8>], new: &[Vec<u8>], from: usize, to: (usize, usize)) -> io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    let mut at = from;
    let go = |buf: &mut Vec<u8>, row: usize, at: &mut usize| -> io::Result<()> {
        if row < *at {
            queue!(buf, MoveUp(u16_of(*at - row)))?;
        } else if row > *at {
            queue!(buf, MoveDown(u16_of(row - *at)))?;
        }
        *at = row;
        Ok(())
    };
    for (i, (o, n)) in old.iter().zip(new).enumerate() {
        if o != n {
            go(&mut buf, i, &mut at)?;
            buf.push(b'\r');
            queue!(buf, Clear(ClearType::CurrentLine))?;
            buf.extend_from_slice(n);
        }
    }
    go(&mut buf, to.0, &mut at)?;
    queue!(buf, MoveToColumn(to.1 as u16))?;
    Ok(buf)
}

impl Renderer {
    pub fn new() -> Self {
        Self::default()
    }

    /// A renderer that writes to `out` and sees a screen of `size` (cols, rows).
    #[cfg(test)]
    pub fn fixed(out: impl Write + Send + 'static, size: (usize, usize)) -> Self {
        Self { fixed: Some(Fixed { out: Box::new(out), size }), ..Self::default() }
    }

    /// (cols, rows), queried fresh.
    pub fn size(&self) -> (usize, usize) {
        if let Some(f) = &self.fixed {
            return f.size;
        }
        let (c, r) = terminal::size().unwrap_or((80, 24));
        (c.max(1) as usize, r.max(1) as usize)
    }

    fn emit(&mut self, buf: &[u8]) -> io::Result<()> {
        match &mut self.fixed {
            Some(f) => {
                f.out.write_all(buf)?;
                f.out.flush()
            }
            None => {
                let mut out = io::stdout().lock();
                out.write_all(buf)?;
                out.flush()
            }
        }
    }

    /// Tells the panic hook how far below the cursor the viewport reaches; a stand-in screen has no
    /// terminal for it to move in.
    fn set_rows_below(&self, n: usize) {
        if self.fixed.is_none() {
            crate::term::term::set_rows_below(n);
        }
    }

    /// Draws committed lines (scrolled into scrollback) followed by the viewport. The cursor is
    /// placed at `cursor` = (viewport row, display column) in the given viewport coordinates.
    pub fn draw(&mut self, committed: &[Line], viewport: &[Line], cursor: (usize, usize)) -> io::Result<()> {
        let (cols, rows) = self.size();
        let mut head = Vec::new();
        for l in committed {
            for w in line::wrap(l, cols) {
                queue!(head, Clear(ClearType::CurrentLine))?;
                head.extend_from_slice(line::to_ansi(&w).as_bytes());
                head.extend_from_slice(b"\r\n");
            }
        }

        let mut wrapped: Vec<Line> = Vec::new();
        let mut at = None;
        for (i, l) in viewport.iter().enumerate() {
            let base = wrapped.len();
            let lay = line::layout(l, cols);
            if i == cursor.0 {
                let (sub, col) = lay.locate_col(cursor.1);
                at = Some((base + sub, col));
            }
            wrapped.extend(lay.rows);
        }

        // Keep the last rows when the viewport is taller than the space left (rows - 1).
        let cap = rows.saturating_sub(1).max(1);
        let mut skip = wrapped.len().saturating_sub(cap);
        // The cursor's row must stay on screen: a window that would cut it off (the status bar
        // below the composer can crowd it out) moves to include it.
        if let Some((r, _)) = at {
            skip = skip.min(r);
            if r >= skip + cap {
                skip = r + 1 - cap;
            }
        }
        let shown = &wrapped[skip..(skip + cap).min(wrapped.len())];
        let (crow, ccol) = match at {
            Some((r, c)) if r >= skip => (r - skip, c),
            _ => (shown.len().saturating_sub(1), 0),
        };

        let widths: Vec<usize> = shown.iter().map(line::line_width).collect();
        let frame = Frame {
            size: (cols, rows),
            rows: shown.iter().map(|l| line::to_ansi(l).into_bytes()).collect(),
            cursor: (crow, ccol.min(cols - 1)),
        };

        // Nothing is scrolling into scrollback and the viewport has the same shape: touch only the
        // rows that changed (a running spinner is one row), or nothing at all.
        if head.is_empty()
            && let Some(prev) = self.last.as_ref().filter(|p| p.size == frame.size && p.rows.len() == frame.rows.len())
        {
            if *prev == frame {
                return Ok(());
            }
            let (cur, _) = self.positions(cols);
            let mut buf = Vec::new();
            queue!(buf, BeginSynchronizedUpdate, Hide)?;
            buf.extend(diff_bytes(&prev.rows, &frame.rows, cur, frame.cursor)?);
            queue!(buf, Show, EndSynchronizedUpdate)?;
            self.drawn = widths;
            self.cursor = frame.cursor;
            self.set_rows_below(frame.rows.len().saturating_sub(1).saturating_sub(frame.cursor.0));
            self.last = None;
            self.emit(&buf)?;
            self.last = Some(frame);
            return Ok(());
        }

        let (up, _) = self.positions(cols);
        let mut buf = Vec::new();
        queue!(buf, BeginSynchronizedUpdate)?;
        // The cursor travels through the viewport; hidden so a terminal that does not batch
        // synchronized updates (or draws cursor trails) never shows it moving.
        queue!(buf, Hide)?;
        if up > 0 {
            queue!(buf, MoveUp(u16_of(up)))?;
        }
        buf.push(b'\r');
        buf.extend_from_slice(&head);
        for (i, row) in frame.rows.iter().enumerate() {
            if i > 0 {
                buf.extend_from_slice(b"\r\n");
            }
            queue!(buf, Clear(ClearType::CurrentLine))?;
            buf.extend_from_slice(row);
        }
        self.drawn = widths;
        // Erase leftovers of a taller previous viewport. Per-line clears above avoid ED at the
        // top-left, which some terminals (tmux) treat as a full clear and archive to scrollback.
        queue!(buf, Clear(ClearType::FromCursorDown))?;
        self.cursor = frame.cursor;

        // The last viewport row has no trailing newline, so the cursor is on it now.
        let (cur, total) = self.positions(cols);
        if total > 0 && total - 1 > cur {
            queue!(buf, MoveUp(u16_of(total - 1 - cur)))?;
        }
        queue!(buf, MoveToColumn(self.cursor.1 as u16))?;
        queue!(buf, Show)?;
        queue!(buf, EndSynchronizedUpdate)?;
        self.set_rows_below(total.saturating_sub(1).saturating_sub(cur));

        self.last = None;
        self.emit(&buf)?;
        self.last = Some(frame);
        Ok(())
    }

    /// Clears the screen and its scrollback and forgets what was drawn, so the next draw starts at the
    /// top: for a client that is about to replay a whole conversation.
    pub fn clear_all(&mut self) -> io::Result<()> {
        self.emit(b"\x1b[3J\x1b[2J\x1b[H")?;
        self.drawn.clear();
        self.cursor = (0, 0);
        self.last = None;
        Ok(())
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
        self.emit(&buf)?;
        self.drawn.clear();
        self.cursor = (0, 0);
        self.last = None;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::line::plain;

    #[test]
    fn locate_follows_wrap() {
        let at = |t| line::layout(&plain("abcdef"), 4).locate_col(t);
        assert_eq!(at(5), (1, 1));
        assert_eq!(at(0), (0, 0));
        assert_eq!(at(4), (0, 4));
    }

    #[test]
    fn diff_rewrites_only_changed_rows() {
        let row = |s: &str| s.as_bytes().to_vec();
        let old = [row("a"), row("b"), row("c"), row("d")];
        let new = [row("a"), row("B"), row("c"), row("D")];
        let out = String::from_utf8(diff_bytes(&old, &new, 3, (3, 2)).unwrap()).unwrap();
        assert_eq!(out, "\x1b[2A\r\x1b[2KB\x1b[2B\r\x1b[2KD\x1b[3G");
        let same = diff_bytes(&old, &old, 2, (2, 0)).unwrap();
        assert_eq!(String::from_utf8(same).unwrap(), "\x1b[1G");
    }

    #[test]
    fn positions_account_for_reflow_on_shrink() {
        let r = Renderer { drawn: vec![10, 3], cursor: (1, 2), ..Renderer::default() };
        // At 4 cols the 10-wide row takes 3 physical rows; cursor row starts after it.
        assert_eq!(r.positions(4), (3, 4));
        assert_eq!(r.positions(20), (1, 2));
    }
}
