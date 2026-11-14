//! The frame every panel shares: header rows, a body, footer rows. The body gets the rows the
//! header and footer leave, so no panel counts its chrome by hand.

use crate::text::line::{self, Line};

/// Rows an overlay may use below the blank spacer row and the `used` rows of transcript and status,
/// leaving one for the cursor line.
pub fn free_rows(rows: usize, used: usize) -> usize {
    rows.saturating_sub(1).saturating_sub(used).saturating_sub(1)
}

#[derive(Default)]
pub struct Frame {
    pub header: Vec<Line>,
    pub footer: Vec<Line>,
}

impl Frame {
    /// Rows left for the body in `max_rows`, and never fewer than `min`.
    pub fn body_rows(&self, max_rows: usize, min: usize) -> usize {
        max_rows.saturating_sub(self.header.len() + self.footer.len()).max(min)
    }

    /// Header, `body` and footer in order, each cut to `width`.
    pub fn finish(self, body: Vec<Line>, width: usize) -> Vec<Line> {
        self.header.into_iter().chain(body).chain(self.footer).map(|l| line::truncate(&l, width)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::line::{line_text, styled};

    fn row(s: &str) -> Line {
        styled(s, |_| Default::default())
    }

    #[test]
    fn the_body_gets_what_the_header_and_footer_leave() {
        let f = Frame { header: vec![row("h")], footer: vec![row("a"), row("b")] };
        assert_eq!(f.body_rows(10, 1), 7);
        assert_eq!(f.body_rows(2, 1), 1, "never below the minimum");
        let out = f.finish(vec![row("body")], 3);
        assert_eq!(out.iter().map(line_text).collect::<Vec<_>>(), ["h", "bo…", "a", "b"]);
    }

    #[test]
    fn free_rows_leave_the_spacer_the_used_rows_and_the_cursor_line() {
        assert_eq!(free_rows(30, 10), 18);
        assert_eq!(free_rows(5, 10), 0);
    }
}
