//! Terminal input: raw reads from the controlling tty, parsed into crossterm events, bracketed pastes
//! and kitty OSC 5522 clipboard-paste events. crossterm's own parser cannot read OSC input.

use std::collections::VecDeque;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
use crossterm::event::Event;
use tokio::io::unix::AsyncFd;
use tokio::signal::unix::{Signal, SignalKind, signal};

use super::term;
use crate::image;
use crate::message::Image;

const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";
/// An unterminated escape sequence or paste this large is garbage; drop it.
const MAX_PENDING: usize = 64 * 1024 * 1024;

pub enum Input {
    Term(Event),
    PasteText(String),
    PasteImage(Image),
    Status(String),
}

pub struct Reader {
    src: Source,
    parser: Parser,
    winch: Signal,
    queue: VecDeque<Input>,
}

/// How readiness of the tty is learned. epoll (Linux) lets tokio watch the fd; kqueue (macOS) cannot register
/// `/dev/tty` (EINVAL), so there a helper thread waits in select(2) and the reader drains on its signal.
enum Source {
    Async(AsyncFd<OwnedFd>),
    Select { fd: OwnedFd, ready: tokio::sync::mpsc::UnboundedReceiver<()>, ack: std::sync::mpsc::Sender<()> },
}

/// Blocks in select(2) until `fd` is readable, tells the reader, and waits for it to drain before selecting
/// again. It never reads, so keystrokes meant for a child process (editor, `omp login`) are not stolen.
fn select_thread(fd: RawFd, ready: tokio::sync::mpsc::UnboundedSender<()>, ack: std::sync::mpsc::Receiver<()>) {
    loop {
        // SAFETY: `set` is zeroed and initialised by FD_ZERO/FD_SET; `fd` is below FD_SETSIZE for a freshly opened tty.
        let r = unsafe {
            let mut set: libc::fd_set = std::mem::zeroed();
            libc::FD_ZERO(&mut set);
            libc::FD_SET(fd, &mut set);
            libc::select(fd + 1, &mut set, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut())
        };
        if r < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return;
        }
        if ready.send(()).is_err() || ack.recv().is_err() {
            return;
        }
    }
}

impl Reader {
    /// Reads from `/dev/tty` rather than fd 0: `O_NONBLOCK` is a property of the open file description,
    /// which stdout and stderr usually share with stdin, and non-blocking stdout would fail writes with
    /// `EAGAIN`. Children (editor, `omp login`) also keep a blocking stdin.
    pub fn new() -> io::Result<Self> {
        // SAFETY: plain open(2) with a NUL-terminated literal path.
        let raw: RawFd = unsafe { libc::open(c"/dev/tty".as_ptr(), libc::O_RDONLY | libc::O_NONBLOCK | libc::O_CLOEXEC) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `raw` is a freshly opened descriptor that nothing else owns, and `OwnedFd` keeps it open.
        let owned = unsafe { OwnedFd::from_raw_fd(raw) };
        // MU_INPUT_SELECT forces the select backend, so it can be tested on Linux.
        let src = if cfg!(target_os = "macos") || std::env::var_os("MU_INPUT_SELECT").is_some() {
            let (rtx, ready) = tokio::sync::mpsc::unbounded_channel();
            let (ack, arx) = std::sync::mpsc::channel();
            std::thread::Builder::new().name("tty-select".into()).spawn(move || select_thread(raw, rtx, arx))?;
            Source::Select { fd: owned, ready, ack }
        } else {
            // SAFETY: `owned` is the only owner of the descriptor and stays open as long as the `AsyncFd`.
            Source::Async(unsafe { AsyncFd::register_with_interest(owned, tokio::io::Interest::READABLE) }?)
        };
        Ok(Self { src, parser: Parser::default(), winch: signal(SignalKind::window_change())?, queue: VecDeque::new() })
    }

    fn consume(&mut self, bytes: &[u8]) {
        let inputs = self.parser.feed(bytes);
        for seq in self.parser.take_writes() {
            term::write_terminal_seq(&seq);
        }
        self.queue.extend(inputs);
    }

    /// The next input, or `None` when the terminal closed.
    pub async fn next(&mut self) -> Option<Input> {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            if let Some(i) = self.queue.pop_front() {
                return Some(i);
            }
            let winch = &mut self.winch;
            match &mut self.src {
                Source::Async(fd) => {
                    tokio::select! {
                        _ = winch.recv() => self.queue.push_back_resize(),
                        ready = fd.readable() => {
                            let mut guard = ready.ok()?;
                            let res = guard.try_io(|fd| read_fd(fd.as_raw_fd(), &mut buf));
                            match res {
                                Ok(Ok(0)) => return None,
                                Ok(Ok(n)) => self.consume(&buf[..n]),
                                Ok(Err(e)) if e.kind() == io::ErrorKind::Interrupted => {}
                                Ok(Err(_)) => return None,
                                // Spurious readiness: `try_io` cleared it.
                                Err(_) => {}
                            }
                        }
                    }
                }
                Source::Select { fd, ready, ack } => {
                    tokio::select! {
                        _ = winch.recv() => self.queue.push_back_resize(),
                        got = ready.recv() => {
                            got?;
                            let raw = fd.as_raw_fd();
                            let mut chunks: Vec<Vec<u8>> = Vec::new();
                            loop {
                                match read_fd(raw, &mut buf) {
                                    Ok(0) => return None,
                                    Ok(n) => chunks.push(buf[..n].to_vec()),
                                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                                    Err(_) => return None,
                                }
                            }
                            let _ = ack.send(());
                            for c in chunks {
                                self.consume(&c);
                            }
                        }
                    }
                }
            }
        }
    }
}

fn read_fd(fd: RawFd, buf: &mut [u8]) -> io::Result<usize> {
    // SAFETY: `buf` is valid for `buf.len()` bytes for the duration of the call.
    let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
    if n < 0 { Err(io::Error::last_os_error()) } else { Ok(n as usize) }
}

trait PushResize {
    fn push_back_resize(&mut self);
}

impl PushResize for VecDeque<Input> {
    fn push_back_resize(&mut self) {
        if let Ok((c, r)) = crossterm::terminal::size() {
            self.push_back(Input::Term(Event::Resize(c, r)));
        }
    }
}

#[derive(Default)]
struct Parser {
    buf: Vec<u8>,
    paste: Paste5522,
    writes: Vec<String>,
}

fn find(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    hay.get(from..)?.windows(needle.len()).position(|w| w == needle).map(|i| i + from)
}

/// Index just past the OSC terminator (BEL or ST) at or after byte 2, and the payload end.
fn osc_end(buf: &[u8]) -> Option<(usize, usize)> {
    let mut i = 2;
    while i < buf.len() {
        match buf[i] {
            0x07 => return Some((i, i + 1)),
            0x1b if buf.get(i + 1) == Some(&b'\\') => return Some((i, i + 2)),
            _ => i += 1,
        }
    }
    None
}

impl Parser {
    fn take_writes(&mut self) -> Vec<String> {
        std::mem::take(&mut self.writes)
    }

    /// Consumes `chunk` (one `read`), returning every complete input. Incomplete sequences stay
    /// buffered for the next chunk; a lone ESC at the end of the data is an Escape key press.
    fn feed(&mut self, chunk: &[u8]) -> Vec<Input> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        'next: while !self.buf.is_empty() {
            if self.buf.starts_with(b"\x1b]") {
                let Some((end, next)) = osc_end(&self.buf) else { break };
                let body = String::from_utf8_lossy(&self.buf[2..end]).into_owned();
                self.buf.drain(..next);
                if let Some(rest) = body.strip_prefix("5522;") {
                    let (input, writes) = self.paste.handle(rest);
                    self.writes.extend(writes);
                    out.extend(input);
                }
                continue;
            }
            if self.buf.starts_with(PASTE_START) {
                let Some(end) = find(&self.buf, PASTE_END, PASTE_START.len()) else { break };
                let text = String::from_utf8_lossy(&self.buf[PASTE_START.len()..end]).into_owned();
                self.buf.drain(..end + PASTE_END.len());
                out.push(Input::Term(Event::Paste(text)));
                continue;
            }
            let start = if self.buf[0] == 0x1b && self.buf.len() > 1 { 2 } else { 1 };
            for n in start..=self.buf.len() {
                match terminput::Event::parse_from(&self.buf[..n]) {
                    Ok(Some(ev)) => {
                        self.buf.drain(..n);
                        out.extend(terminput_crossterm::to_crossterm(ev).ok().map(Input::Term));
                        continue 'next;
                    }
                    Ok(None) => {}
                    Err(_) => {
                        self.buf.drain(..n);
                        continue 'next;
                    }
                }
            }
            break;
        }
        if self.buf.len() > MAX_PENDING {
            self.buf.clear();
        }
        out
    }
}

#[derive(Debug, PartialEq)]
enum Kind {
    Image,
    Text,
}

#[derive(Default)]
enum Paste5522 {
    #[default]
    Idle,
    Listing { mimes: Vec<String>, dot_payload: bool, pw: Option<String>, loc: Option<String> },
    Reading { kind: Kind, mime: String, bytes: Vec<u8> },
}

const IMAGE_MIMES: [&str; 4] = ["image/png", "image/jpeg", "image/webp", "image/gif"];

fn b64_text(v: &str) -> Option<String> {
    STANDARD.decode(v).ok().map(|b| String::from_utf8_lossy(&b).into_owned())
}

impl Paste5522 {
    /// Handles one packet (`type=read:status=…[;payload]`). Returns an input to emit and any
    /// terminal sequences to send back. A port of omp's `EnhancedPasteController`.
    fn handle(&mut self, body: &str) -> (Option<Input>, Vec<String>) {
        let (meta, payload) = body.split_once(';').unwrap_or((body, ""));
        let meta: Vec<(&str, &str)> = meta.split(':').filter_map(|p| p.split_once('=').filter(|(k, _)| !k.is_empty())).collect();
        let get = |k: &str| meta.iter().find(|(m, _)| *m == k).map(|(_, v)| *v);
        if get("type") != Some("read") {
            return (None, vec![]);
        }
        match get("status") {
            Some("OK") => {
                if !matches!(self, Paste5522::Reading { .. }) {
                    *self = Paste5522::Listing {
                        mimes: vec![],
                        dot_payload: false,
                        pw: get("pw").map(String::from),
                        loc: get("loc").filter(|l| *l == "primary").map(String::from),
                    };
                }
                (None, vec![])
            }
            Some("DATA") => {
                self.data(get("mime"), payload);
                (None, vec![])
            }
            Some("DONE") => self.done(),
            Some(status) => {
                *self = Paste5522::Idle;
                (Some(Input::Status(format!("Enhanced paste failed: {status}"))), vec![])
            }
            None => (None, vec![]),
        }
    }

    fn data(&mut self, encoded_mime: Option<&str>, payload: &str) {
        let Some(mime) = encoded_mime.and_then(b64_text).filter(|m| !m.is_empty()) else { return };
        match self {
            Paste5522::Idle => {}
            Paste5522::Listing { mimes, dot_payload, .. } => {
                // Kitty answers the listing with one `mime="."` packet whose payload is the
                // whitespace-separated type list; the spec form sends one empty-payload packet per type.
                if mime == "." {
                    if payload.is_empty() {
                        return;
                    }
                    let Some(listing) = b64_text(payload) else { return };
                    *dot_payload = true;
                    mimes.extend(listing.split_whitespace().filter(|c| *c != ".").map(String::from));
                } else {
                    mimes.push(mime);
                }
            }
            Paste5522::Reading { mime: want, bytes, .. } => {
                if *want == mime && !payload.is_empty() {
                    if let Ok(chunk) = STANDARD_NO_PAD.decode(payload.trim_end_matches('=')) {
                        bytes.extend(chunk);
                    }
                }
            }
        }
    }

    fn done(&mut self) -> (Option<Input>, Vec<String>) {
        match std::mem::take(self) {
            Paste5522::Idle => (None, vec![]),
            Paste5522::Listing { mimes, dot_payload, pw, loc } => {
                let chosen = IMAGE_MIMES
                    .iter()
                    .find(|m| mimes.iter().any(|x| x == *m))
                    .map(|m| (Kind::Image, *m))
                    .or_else(|| mimes.iter().any(|x| x == "text/plain").then_some((Kind::Text, "text/plain")));
                let Some((kind, mime)) = chosen else {
                    return (Some(Input::Status("Clipboard paste has no supported text or image data".into())), vec![]);
                };
                let encoded = STANDARD.encode(mime);
                let mut meta = vec!["type=read".to_string()];
                if let Some(l) = loc {
                    meta.push(format!("loc={l}"));
                }
                if let Some(p) = pw {
                    meta.push(format!("pw={p}"));
                    meta.push(format!("name={}", STANDARD.encode("Paste event")));
                }
                let seq = if dot_payload {
                    format!("\x1b]5522;{};{encoded}\x07", meta.join(":"))
                } else {
                    meta.push(format!("mime={encoded}"));
                    format!("\x1b]5522;{}\x07", meta.join(":"))
                };
                *self = Paste5522::Reading { kind, mime: mime.into(), bytes: Vec::new() };
                (None, vec![seq])
            }
            Paste5522::Reading { kind, bytes, .. } => {
                let input = if bytes.is_empty() {
                    Input::Status("Clipboard paste was empty".into())
                } else if kind == Kind::Text {
                    Input::PasteText(String::from_utf8_lossy(&bytes).into_owned())
                } else {
                    match image::from_bytes(bytes) {
                        Ok(img) => Input::PasteImage(img),
                        Err(e) => Input::Status(e),
                    }
                };
                (Some(input), vec![])
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(c: KeyCode) -> Input {
        Input::Term(Event::Key(KeyEvent::new(c, KeyModifiers::NONE)))
    }

    fn same(a: &[Input], b: &[Input]) -> bool {
        a.len() == b.len()
            && a.iter().zip(b).all(|(x, y)| match (x, y) {
                (Input::Term(x), Input::Term(y)) => x == y,
                _ => false,
            })
    }

    #[test]
    fn keys_escape_and_bracketed_paste() {
        let mut p = Parser::default();
        assert!(same(&p.feed(b"\x1b[A"), &[key(KeyCode::Up)]));
        assert!(same(&p.feed(b"\x1b"), &[key(KeyCode::Esc)]));
        let got = p.feed(b"a\x1b[200~x\ny\x1b[201~b");
        assert!(same(&got, &[key(KeyCode::Char('a')), Input::Term(Event::Paste("x\ny".into())), key(KeyCode::Char('b'))]));
    }

    #[test]
    fn split_sequences_wait_for_the_rest() {
        let mut p = Parser::default();
        assert!(p.feed(b"\x1b[").is_empty());
        assert!(same(&p.feed(b"B"), &[key(KeyCode::Down)]));
        assert!(p.feed(b"\x1b[200~par").is_empty());
        assert!(same(&p.feed(b"t\x1b[201~"), &[Input::Term(Event::Paste("part".into()))]));
    }

    fn osc(body: &str) -> Vec<u8> {
        format!("\x1b]5522;{body}\x07").into_bytes()
    }

    #[test]
    fn osc_5522_image_paste_round_trip() {
        let mut p = Parser::default();
        let png_b64 = STANDARD.encode(b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR");
        let mime = STANDARD.encode("image/png");
        assert!(p.feed(&osc("type=read:status=OK")).is_empty());
        // The OSC arrives split across two reads.
        let data = osc(&format!("type=read:status=DATA:mime={mime}"));
        assert!(p.feed(&data[..10]).is_empty());
        assert!(p.feed(&data[10..]).is_empty());
        assert!(p.feed(&osc("type=read:status=DONE")).is_empty());
        let writes = p.take_writes();
        assert_eq!(writes, [format!("\x1b]5522;type=read:mime={mime}\x07")]);

        assert!(p.feed(&osc("type=read:status=OK")).is_empty());
        assert!(p.feed(&osc(&format!("type=read:status=DATA:mime={mime};{png_b64}"))).is_empty());
        let got = p.feed(&osc("type=read:status=DONE"));
        assert_eq!(got.len(), 1);
        let Input::PasteImage(img) = &got[0] else { panic!("expected an image paste") };
        assert_eq!(img.mime_type, "image/png");
        assert!(p.buf.is_empty());
    }

    #[test]
    fn osc_5522_kitty_dot_listing_prefers_image_over_text() {
        let mut p = Parser::default();
        let dot = STANDARD.encode(".");
        let list = STANDARD.encode("text/plain image/jpeg");
        p.feed(&osc("type=read:status=OK:pw=abc"));
        p.feed(&osc(&format!("type=read:status=DATA:mime={dot};{list}")));
        p.feed(&osc("type=read:status=DONE"));
        let jpeg = STANDARD.encode("image/jpeg");
        let name = STANDARD.encode("Paste event");
        assert_eq!(p.take_writes(), [format!("\x1b]5522;type=read:pw=abc:name={name};{jpeg}\x07")]);
    }

    #[test]
    fn osc_5522_errors_and_empty_pastes_surface_as_status() {
        let mut p = Parser::default();
        let got = p.feed(&osc("type=read:status=EPERM"));
        assert!(matches!(&got[..], [Input::Status(s)] if s == "Enhanced paste failed: EPERM"));
        p.feed(&osc("type=read:status=OK"));
        let got = p.feed(&osc("type=read:status=DONE"));
        assert!(matches!(&got[..], [Input::Status(s)] if s == "Clipboard paste has no supported text or image data"));
    }
}
