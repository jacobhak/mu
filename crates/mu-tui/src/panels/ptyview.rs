//! Embedded terminal screen for a bash call running on a pty.
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use mu_base::events::PtyInput;
use std::num::NonZeroU16;
use tokio::sync::mpsc::UnboundedSender;

pub struct PtyView {
    pub id: String,
    parser: vt100::Parser,
    input: UnboundedSender<PtyInput>,
    /// (cols, rows)
    size: (u16, u16),
    /// The command is waiting for keyboard input (a prompt, a password).
    pub waiting: bool,
}

fn nz(n: u16) -> NonZeroU16 {
    NonZeroU16::new(n.max(1)).expect("max(1) is non-zero")
}

impl PtyView {
    /// Card inner width, and a row count that leaves room for the rest of the UI.
    pub fn size_for(width: usize, height: usize) -> (u16, u16) {
        let cols = width.saturating_sub(4).max(10).min(u16::MAX as usize) as u16;
        let rows = height.saturating_sub(12).clamp(4, 24) as u16;
        (cols, rows)
    }

    pub fn new(id: String, input: UnboundedSender<PtyInput>, size: (u16, u16)) -> Self {
        Self { id, parser: vt100::Parser::new(nz(size.1), nz(size.0), 0), input, size, waiting: false }
    }

    pub fn feed(&mut self, s: &str) {
        self.parser.process(s.as_bytes());
    }

    pub fn resize(&mut self, width: usize, height: usize) {
        let size = Self::size_for(width, height);
        if size != self.size {
            self.size = size;
            self.parser.screen_mut().set_size(nz(size.1), nz(size.0));
            let _ = self.input.send(PtyInput::Resize { cols: size.0, rows: size.1 });
        }
    }

    /// Screen rows (trailing blank rows trimmed, but kept through the cursor row) and the cursor `(row, col)`.
    pub fn screen(&self) -> (Vec<String>, (usize, usize)) {
        let screen = self.parser.screen();
        let mut rows: Vec<String> = screen.rows(0, self.size.0).collect();
        let (cr, cc) = screen.cursor_position();
        let (cr, cc) = (cr as usize, cc as usize);
        while rows.len() > 1 && rows.len() > cr + 1 && rows.last().is_some_and(|r| r.trim().is_empty()) {
            rows.pop();
        }
        if rows.is_empty() {
            rows.push(String::new());
        }
        (rows, (cr, cc))
    }

    pub fn key(&self, k: KeyEvent) {
        if k.kind == KeyEventKind::Release {
            return;
        }
        // Esc leaves the tool: the command is killed and the view closes when its call ends. Ctrl+C
        // never reaches the command; the app uses it to exit mu.
        if crate::keymap::TERMINAL_KILL.hit(&k) {
            let _ = self.input.send(PtyInput::Kill);
            return;
        }
        if let Some(b) = encode_key(k, self.parser.screen().application_cursor()) {
            let _ = self.input.send(PtyInput::Bytes(b));
        }
    }

    pub fn paste(&self, s: &str) {
        let mut b = Vec::new();
        let bracketed = self.parser.screen().bracketed_paste();
        if bracketed {
            b.extend_from_slice(b"\x1b[200~");
        }
        // An ESC in the text could end the bracketed paste early and smuggle in keystrokes.
        b.extend_from_slice(s.replace('\x1b', "").as_bytes());
        if bracketed {
            b.extend_from_slice(b"\x1b[201~");
        }
        let _ = self.input.send(PtyInput::Bytes(b));
    }
}

/// Bytes a terminal would send for a key, following omp's `normalizeInputForPty`.
pub fn encode_key(k: KeyEvent, app_cursor: bool) -> Option<Vec<u8>> {
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    let alt = k.modifiers.contains(KeyModifiers::ALT);
    let mut out: Vec<u8> = Vec::new();
    let esc_if_alt = |out: &mut Vec<u8>| {
        if alt {
            out.push(0x1b);
        }
    };
    match k.code {
        KeyCode::Char(c) => {
            esc_if_alt(&mut out);
            if ctrl {
                let b = match c {
                    'a'..='z' | 'A'..='Z' => (c as u8) & 0x1f,
                    '@' | ' ' => 0,
                    '[' => 0x1b,
                    '\\' => 0x1c,
                    ']' => 0x1d,
                    '^' => 0x1e,
                    '_' => 0x1f,
                    '?' => 0x7f,
                    _ => return None,
                };
                out.push(b);
            } else {
                let mut buf = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
        }
        KeyCode::Enter => out.push(b'\r'),
        KeyCode::Tab => out.push(b'\t'),
        KeyCode::BackTab => out.extend_from_slice(b"\x1b[Z"),
        KeyCode::Backspace => {
            esc_if_alt(&mut out);
            out.push(0x7f);
        }
        KeyCode::Up | KeyCode::Down | KeyCode::Right | KeyCode::Left => {
            let d = match k.code {
                KeyCode::Up => b'A',
                KeyCode::Down => b'B',
                KeyCode::Right => b'C',
                _ => b'D',
            };
            out.extend_from_slice(if app_cursor { b"\x1bO" } else { b"\x1b[" });
            out.push(d);
        }
        KeyCode::Home => out.extend_from_slice(if app_cursor { b"\x1bOH" } else { b"\x1b[H" }),
        KeyCode::End => out.extend_from_slice(if app_cursor { b"\x1bOF" } else { b"\x1b[F" }),
        KeyCode::PageUp => out.extend_from_slice(b"\x1b[5~"),
        KeyCode::PageDown => out.extend_from_slice(b"\x1b[6~"),
        KeyCode::Insert => out.extend_from_slice(b"\x1b[2~"),
        KeyCode::Delete => out.extend_from_slice(b"\x1b[3~"),
        KeyCode::F(n @ 1..=4) => out.extend_from_slice(&[0x1b, b'O', b'P' + (n - 1)]),
        KeyCode::F(n @ 5..=12) => {
            let code = [15, 17, 18, 19, 20, 21, 23, 24][(n - 5) as usize];
            out.extend_from_slice(format!("\x1b[{code}~").as_bytes());
        }
        _ => return None,
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(code: KeyCode, m: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, m)
    }

    #[test]
    fn esc_kills_and_other_keys_go_to_the_child() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let v = PtyView::new("x".into(), tx, (80, 24));
        v.key(k(KeyCode::Char('a'), KeyModifiers::NONE));
        assert!(matches!(rx.try_recv(), Ok(PtyInput::Bytes(b)) if b == b"a"));
        v.key(k(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(rx.try_recv(), Ok(PtyInput::Kill)));
        assert!(encode_key(k(KeyCode::Esc, KeyModifiers::NONE), false).is_none(), "Esc is not a keystroke");
    }

    #[test]
    fn the_kill_key_can_be_rebound() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let v = PtyView::new("x".into(), tx, (80, 24));
        crate::keymap::with_keys(&[("terminal", "kill", &["f5"])], || {
            v.key(k(KeyCode::Esc, KeyModifiers::NONE));
            assert!(rx.try_recv().is_err(), "Esc no longer kills, and is no keystroke");
            v.key(k(KeyCode::F(5), KeyModifiers::NONE));
            assert!(matches!(rx.try_recv(), Ok(PtyInput::Kill)));
        });
    }

    #[test]
    fn keys() {
        let n = KeyModifiers::NONE;
        assert_eq!(encode_key(k(KeyCode::Char('c'), KeyModifiers::CONTROL), false), Some(vec![3]));
        assert_eq!(encode_key(k(KeyCode::Up, n), true), Some(b"\x1bOA".to_vec()));
        assert_eq!(encode_key(k(KeyCode::Up, n), false), Some(b"\x1b[A".to_vec()));
        assert_eq!(encode_key(k(KeyCode::Char('b'), KeyModifiers::ALT), false), Some(b"\x1bb".to_vec()));
        assert_eq!(encode_key(k(KeyCode::Enter, n), false), Some(b"\r".to_vec()));
        assert_eq!(encode_key(k(KeyCode::F(5), n), false), Some(b"\x1b[15~".to_vec()));
    }
}
