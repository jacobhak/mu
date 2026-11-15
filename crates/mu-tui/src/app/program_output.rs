//! What the client lets a host program's output do to this terminal. The host may be another
//! machine, so its bytes are not trusted: they could set the clipboard (OSC 52), name a file for a
//! click to open (OSC 7, OSC 8 to `file:`) or ask the terminal to answer on stdin (DCS and its kin).
//! Text, SGR, cursor movement and the window title (OSC 0, 1, 2) pass.

/// The longest OSC kept whole to be judged; a longer one is dropped.
const MAX_OSC: usize = 2048;

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum State {
    #[default]
    Ground,
    Esc,
    /// After `ESC` and an intermediate byte (a charset designation): the next byte ends it.
    EscInter,
    Csi,
    Osc,
    OscEsc,
    /// DCS, SOS, PM or APC: dropped up to its string terminator.
    Skip,
    SkipEsc,
}

/// Filters a stream chunk by chunk: a sequence split across chunks is judged whole.
#[derive(Default)]
pub(super) struct OutputFilter {
    state: State,
    osc: Vec<u8>,
    /// The OSC outgrew `MAX_OSC`.
    osc_long: bool,
}

impl OutputFilter {
    pub(super) fn push(&mut self, bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(bytes.len());
        for &b in bytes {
            self.step(b, &mut out);
        }
        out
    }

    fn step(&mut self, b: u8, out: &mut Vec<u8>) {
        // CAN and SUB cancel any sequence.
        if matches!(b, 0x18 | 0x1a) && self.state != State::Ground {
            self.end_osc();
            self.state = State::Ground;
            return;
        }
        match self.state {
            State::Ground => {
                if b == 0x1b {
                    self.state = State::Esc;
                } else {
                    out.push(b);
                }
            }
            State::Esc => match b {
                b']' => {
                    self.osc.clear();
                    self.osc_long = false;
                    self.state = State::Osc;
                }
                b'P' | b'X' | b'^' | b'_' => self.state = State::Skip,
                b'[' => {
                    out.extend_from_slice(b"\x1b[");
                    self.state = State::Csi;
                }
                0x1b => {}
                0x20..=0x2f => {
                    out.extend_from_slice(&[0x1b, b]);
                    self.state = State::EscInter;
                }
                _ => {
                    out.extend_from_slice(&[0x1b, b]);
                    self.state = State::Ground;
                }
            },
            State::EscInter => {
                out.push(b);
                if b >= 0x30 {
                    self.state = State::Ground;
                }
            }
            State::Csi => {
                out.push(b);
                if (0x40..=0x7e).contains(&b) {
                    self.state = State::Ground;
                }
            }
            State::Osc => match b {
                0x07 => self.finish_osc(b"\x07", out),
                0x1b => self.state = State::OscEsc,
                _ => self.keep(b),
            },
            State::OscEsc => {
                if b == b'\\' {
                    self.finish_osc(b"\x1b\\", out);
                } else {
                    // The ESC began a new sequence; the unfinished OSC is dropped.
                    self.end_osc();
                    self.state = State::Esc;
                    self.step(b, out);
                }
            }
            State::Skip => {
                match b {
                    0x1b => self.state = State::SkipEsc,
                    0x07 => self.state = State::Ground,
                    _ => {}
                };
            }
            State::SkipEsc => {
                if b == b'\\' {
                    self.state = State::Ground;
                } else {
                    self.state = State::Esc;
                    self.step(b, out);
                }
            }
        }
    }

    fn keep(&mut self, b: u8) {
        if self.osc.len() >= MAX_OSC {
            self.osc_long = true;
        } else {
            self.osc.push(b);
        }
    }

    fn end_osc(&mut self) {
        self.osc.clear();
        self.osc_long = false;
    }

    fn finish_osc(&mut self, terminator: &[u8], out: &mut Vec<u8>) {
        if !self.osc_long && osc_allowed(&self.osc) {
            out.extend_from_slice(b"\x1b]");
            out.extend_from_slice(&self.osc);
            out.extend_from_slice(terminator);
        }
        self.end_osc();
        self.state = State::Ground;
    }
}

/// The window title and icon (0, 1, 2), and hyperlinks (8) unless they point at a file.
fn osc_allowed(payload: &[u8]) -> bool {
    let mut parts = payload.splitn(2, |&b| b == b';');
    let code = parts.next().unwrap_or_default();
    let rest = parts.next().unwrap_or_default();
    match code {
        b"0" | b"1" | b"2" => true,
        b"8" => {
            let uri = rest.splitn(2, |&b| b == b';').nth(1).unwrap_or_default();
            !uri.get(..5).is_some_and(|s| s.eq_ignore_ascii_case(b"file:"))
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(chunks: &[&[u8]]) -> Vec<u8> {
        let mut f = OutputFilter::default();
        chunks.iter().flat_map(|c| f.push(c)).collect()
    }

    #[test]
    fn text_sgr_cursor_and_titles_pass() {
        for s in [
            &b"plain \xe2\x9c\x93 text\r\n"[..],
            b"\x1b[1;31mred\x1b[0m",
            b"\x1b[2J\x1b[H\x1b[10;5H\x1b[?25l",
            b"\x1b(B\x1b)0",
            b"\x1b]0;a title\x07",
            b"\x1b]2;a title\x1b\\",
            b"\x1b]8;;https://example.com\x07link\x1b]8;;\x07",
        ] {
            assert_eq!(run(&[s]), s, "{}", String::from_utf8_lossy(s));
        }
    }

    #[test]
    fn the_clipboard_file_links_and_dcs_are_dropped() {
        assert_eq!(run(&[b"a\x1b]52;c;aGk=\x07b"]), b"ab");
        assert_eq!(run(&[b"a\x1b]52;c;aGk=\x1b\\b"]), b"ab");
        assert_eq!(run(&[b"a\x1b]7;file://host/tmp\x07b"]), b"ab");
        assert_eq!(run(&[b"a\x1b]8;;file:///etc/passwd\x07x\x1b]8;;\x07b"]), b"ax\x1b]8;;\x07b");
        assert_eq!(run(&[b"a\x1b]8;id=1;FILE:///x\x07b"]), b"ab");
        assert_eq!(run(&[b"a\x1bP+q544e\x1b\\b"]), b"ab");
        assert_eq!(run(&[b"a\x1b_Gf=24;payload\x1b\\b"]), b"ab");
        assert_eq!(run(&[b"a\x1b]4;1;?\x07b"]), b"ab");
    }

    #[test]
    fn a_sequence_split_across_chunks_is_judged_whole() {
        assert_eq!(run(&[b"a\x1b]5", b"2;c;aGk", b"=\x07b"]), b"ab");
        assert_eq!(run(&[b"a\x1b", b"]52;c;x\x1b", b"\\b"]), b"ab");
        assert_eq!(run(&[b"\x1b]0;ti", b"tle\x07"]), b"\x1b]0;title\x07");
        assert_eq!(run(&[b"\x1bP1$", b"r\x1b", b"\\ok"]), b"ok");
        assert_eq!(run(&[b"\x1b[3", b"1mred"]), b"\x1b[31mred");
    }

    #[test]
    fn an_unterminated_or_aborted_sequence_does_not_swallow_what_follows() {
        assert_eq!(run(&[b"\x1b]52;c;x\x1b[31mred"]), b"\x1b[31mred");
        assert_eq!(run(&[b"\x1b]52;c;x\x18ok"]), b"ok");
        assert_eq!(run(&[b"\x1bPdata\x1b[1mok"]), b"\x1b[1mok");
    }

    #[test]
    fn an_oversize_title_is_dropped() {
        let mut s = b"\x1b]0;".to_vec();
        s.extend(std::iter::repeat_n(b'x', MAX_OSC + 10));
        s.push(0x07);
        s.extend_from_slice(b"after");
        assert_eq!(run(&[&s]), b"after");
    }
}
