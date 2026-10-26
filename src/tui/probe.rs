//! Asks the terminal for its colours with OSC 10/11/17/4 queries, ended by a DA1 request.
use std::time::{Duration, Instant};

pub type Rgb = (u8, u8, u8);

#[derive(Default, Debug, PartialEq)]
pub struct Answers {
    pub fg: Option<Rgb>,
    pub bg: Option<Rgb>,
    pub selection: Option<Rgb>,
    pub slots: [Option<Rgb>; 16],
}

/// Queries the controlling terminal; needs raw mode. Returns whatever arrived before `timeout`.
pub fn query(timeout: Duration) -> Answers {
    let Some(fd) = super::term::tty() else { return Answers::default() };
    let mut out = String::from("\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b]17;?\x1b\\");
    for i in 0..16 {
        out.push_str(&format!("\x1b]4;{i};?\x1b\\"));
    }
    out.push_str("\x1b[c");
    let deadline = Instant::now() + timeout;
    let mut buf: Vec<u8> = Vec::new();
    let b = out.as_bytes();
    let mut off = 0;
    while off < b.len() {
        let n = unsafe { libc::write(fd, b[off..].as_ptr().cast(), b.len() - off) };
        if n < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted || e.kind() == std::io::ErrorKind::WouldBlock {
                if Instant::now() >= deadline {
                    break;
                }
                continue;
            }
            break;
        }
        off += n as usize;
    }
    while !has_da1(&buf) {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        let ms = (deadline - now).as_millis().min(i32::MAX as u128) as i32;
        let mut p = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
        let r = unsafe { libc::poll(&mut p, 1, ms.max(1)) };
        if r < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        if r == 0 {
            break;
        }
        let mut chunk = [0u8; 1024];
        let n = unsafe { libc::read(fd, chunk.as_mut_ptr().cast(), chunk.len()) };
        if n < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::WouldBlock {
            continue; // the handle is non-blocking: poll said readable but another reader took it
        }
        if n <= 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n as usize]);
    }
    parse(&buf)
}

/// `ESC [ ? digits/; ... c`
fn has_da1(buf: &[u8]) -> bool {
    let mut i = 0;
    while i + 3 <= buf.len() {
        if buf[i..].starts_with(b"\x1b[?") {
            let rest = &buf[i + 3..];
            let n = rest.iter().take_while(|c| c.is_ascii_digit() || **c == b';').count();
            if n > 0 && rest.get(n) == Some(&b'c') {
                return true;
            }
        }
        i += 1;
    }
    false
}

fn parse_spec(s: &str) -> Option<Rgb> {
    let rest = s.strip_prefix("rgb:")?;
    let mut it = rest.split('/');
    let mut ch = || -> Option<u8> {
        let h = it.next()?;
        if h.is_empty() || h.len() > 4 {
            return None;
        }
        let v = u32::from_str_radix(h, 16).ok()?;
        let max = 16u32.pow(h.len() as u32) - 1;
        Some(((v * 255) as f64 / max as f64).round() as u8)
    };
    let (r, g, b) = (ch()?, ch()?, ch()?);
    if it.next().is_some() {
        return None;
    }
    Some((r, g, b))
}

fn parse(buf: &[u8]) -> Answers {
    let mut a = Answers::default();
    let mut i = 0;
    while i + 1 < buf.len() {
        if buf[i] != 0x1b || buf[i + 1] != b']' {
            i += 1;
            continue;
        }
        let start = i + 2;
        let mut j = start;
        let mut end = None;
        while j < buf.len() {
            if buf[j] == 0x07 {
                end = Some((j, j + 1));
                break;
            }
            if buf[j] == 0x1b && buf.get(j + 1) == Some(&b'\\') {
                end = Some((j, j + 2));
                break;
            }
            j += 1;
        }
        let Some((body_end, next)) = end else { break };
        i = next;
        let Ok(body) = std::str::from_utf8(&buf[start..body_end]) else { continue };
        if let Some(s) = body.strip_prefix("10;") {
            a.fg = parse_spec(s);
        } else if let Some(s) = body.strip_prefix("11;") {
            a.bg = parse_spec(s);
        } else if let Some(s) = body.strip_prefix("17;") {
            a.selection = parse_spec(s);
        } else if let Some(s) = body.strip_prefix("4;")
            && let Some((n, spec)) = s.split_once(';')
            && let Ok(n) = n.parse::<usize>()
            && n < 16
        {
            a.slots[n] = parse_spec(spec);
        }
    }
    a
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tmux_34_reply() {
        let a = parse(b"\x1b]10;rgb:ffff/ffff/ffff\x1b\\\x1b]11;rgb:1e1e/1e1e/1e1e\x1b\\\x1b[?1;2;4c");
        assert_eq!((a.fg, a.bg), (Some((255, 255, 255)), Some((30, 30, 30))));
        assert!(a.slots.iter().all(|s| s.is_none()) && a.selection.is_none());
        assert!(has_da1(b"\x1b]10;x\x07\x1b[?1;2;4c"));
        assert!(!has_da1(b"\x1b[?1;"));
    }

    #[test]
    fn slot_reply_with_short_channels() {
        let a = parse(b"\x1b]4;1;rgb:f7/76/8e\x07");
        assert_eq!(a.slots[1], Some((247, 118, 142)));
        assert_eq!(parse_spec("rgb:f/0/8"), Some((255, 0, 136)));
    }

    #[test]
    fn garbage_is_ignored() {
        let a =
            parse(b"junk\x1b]99;foo\x07\x1b]4;40;rgb:00/00/00\x07\x1b]11;nope\x07\x1b]4;3;rgb:1/2\x07\x1b]10;rgb:00");
        assert_eq!(a, Answers::default());
    }
}
