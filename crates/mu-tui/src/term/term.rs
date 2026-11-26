//! Terminal modes for the inline UI. No alternate screen.

use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering::SeqCst};

use crossterm::cursor;
use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::style::ResetColor;
use crossterm::terminal;

/// True while raw mode and the extra modes are applied.
static APPLIED: AtomicBool = AtomicBool::new(false);
/// Whether keyboard enhancement was pushed; queried once in `enter`.
static KBD: AtomicBool = AtomicBool::new(false);

/// RAII guard: restores the terminal on drop.
pub struct Term;

impl Term {
    pub fn enter() -> io::Result<Term> {
        terminal::enable_raw_mode()?;
        // The query needs raw mode to read the terminal's reply.
        KBD.store(matches!(terminal::supports_keyboard_enhancement(), Ok(true)), SeqCst);
        if let Err(e) = on() {
            off();
            let _ = terminal::disable_raw_mode();
            return Err(e);
        }
        Ok(Term)
    }
}

impl Drop for Term {
    fn drop(&mut self) {
        // Unwinding out of the UI: a fatal panic. Move below the viewport so the message does not
        // land on the composer, restore the terminal, then print it.
        let fatal = std::thread::panicking();
        if fatal {
            let below = ROWS_BELOW.load(SeqCst);
            if below > 0 && APPLIED.load(SeqCst) {
                let _ = execute!(io::stdout(), cursor::MoveDown(below.min(u16::MAX as usize) as u16));
            }
        }
        off();
        // Not `eprint!`: a second panic here aborts the process, and this runs exactly when the
        // terminal may have hung up.
        if fatal && !is_lost() {
            let msg = PANIC.lock().ok().and_then(|mut g| g.take());
            say_err(format_args!("\r\n{}\r\n", msg.unwrap_or_else(|| "mu: internal error".into())));
        }
    }
}

/// The controlling terminal, opened once and shared: the colour probe and the input reader use the
/// same open file description (so the same non-blocking flag), not two separate handles.
pub fn tty() -> Option<std::os::fd::RawFd> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    static TTY: std::sync::OnceLock<Option<OwnedFd>> = std::sync::OnceLock::new();
    TTY.get_or_init(|| {
        // SAFETY: plain open(2) with a NUL-terminated literal path.
        let fd = unsafe { libc::open(c"/dev/tty".as_ptr(), libc::O_RDWR | libc::O_NONBLOCK | libc::O_CLOEXEC) };
        // SAFETY: a fresh descriptor that nothing else owns.
        (fd >= 0).then(|| unsafe { OwnedFd::from_raw_fd(fd) })
    })
    .as_ref()
    .map(|f| f.as_raw_fd())
}

/// Rows between the cursor and the bottom of the drawn viewport; set by the renderer.
static ROWS_BELOW: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
/// The latest panic message not yet shown.
static PANIC: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

pub fn set_rows_below(n: usize) {
    ROWS_BELOW.store(n, SeqCst);
}

/// A panic recorded since the last call, to show as an error. Panics that are caught (a tool, a
/// background task) land here; a fatal one is printed by `Term::drop` instead.
pub fn take_panic() -> Option<String> {
    PANIC.lock().ok().and_then(|mut g| g.take())
}

/// True once the terminal hung up.
static LOST: AtomicBool = AtomicBool::new(false);

/// Writes a farewell to stderr without ever panicking. `eprint!` panics when the write fails, and
/// a write to a terminal that hung up fails with `EIO`; on the way out that panic is the crash.
pub fn say_err(args: std::fmt::Arguments<'_>) {
    let mut err = io::stderr().lock();
    let _ = err.write_fmt(args);
    let _ = err.flush();
}

/// Whether an output error means the terminal is gone: a pty whose master closed answers `EIO`.
pub fn hangup(e: &io::Error) -> bool {
    matches!(e.raw_os_error(), Some(libc::EIO) | Some(libc::EPIPE) | Some(libc::ENXIO))
}

/// Remembers the hangup, so the screen is not drawn to a terminal that cannot show it.
pub fn mark_lost() {
    LOST.store(true, SeqCst);
}

/// Whether the terminal hung up: nothing can be drawn or said to it any more.
pub fn is_lost() -> bool {
    LOST.load(SeqCst)
}

/// Waits for SIGTERM, SIGHUP or SIGQUIT, so the terminal can be restored on the way out.
pub struct Shutdown {
    term: tokio::signal::unix::Signal,
    hup: tokio::signal::unix::Signal,
    quit: tokio::signal::unix::Signal,
}

impl Shutdown {
    pub fn new() -> io::Result<Self> {
        use tokio::signal::unix::{SignalKind, signal};
        Ok(Self {
            term: signal(SignalKind::terminate())?,
            hup: signal(SignalKind::hangup())?,
            quit: signal(SignalKind::quit())?,
        })
    }

    pub async fn next(&mut self) {
        tokio::select! {
            _ = self.term.recv() => {}
            _ = self.hup.recv() => {}
            _ = self.quit.recv() => {}
        }
    }
}

fn on() -> io::Result<()> {
    if APPLIED.load(SeqCst) {
        return Ok(());
    }
    terminal::enable_raw_mode()?;
    APPLIED.store(true, SeqCst);
    let mut out = io::stdout();
    execute!(out, EnableBracketedPaste)?;
    if KBD.load(SeqCst) {
        execute!(out, PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES))?;
    } else {
        // xterm modifyOtherKeys: how tmux (`extended-keys on`) and xterm tell Ctrl+Enter from Enter.
        out.write_all(b"\x1b[>4;2m")?;
    }
    write_terminal_seq(paste_events_seq(in_tmux()));
    Ok(())
}

fn in_tmux() -> bool {
    std::env::var("TMUX").is_ok_and(|v| !v.is_empty())
}

/// The sequence sent on entry for the terminal's paste events (mode 5522). tmux implements no part
/// of the protocol: with `extended-keys on` it breaks an event into keys, so the composer fills
/// with `5522;type=read:…`, and the mode belongs to the terminal's one surface, so while it is on
/// a paste into any other pane arrives there as typed text. Under tmux it is therefore turned off
/// (which also clears it after a mu that left it on); a paste then comes as bracketed paste.
fn paste_events_seq(tmux: bool) -> &'static str {
    if tmux { "\x1b[?5522l" } else { "\x1b[?5522h" }
}

/// Idempotent; errors are ignored so it is safe from panic hooks and drop.
fn off() {
    if !APPLIED.swap(false, SeqCst) {
        return;
    }
    let mut out = io::stdout();
    // Ends a synchronized update a panic may have left open.
    let _ = out.write_all(b"\x1b[?2026l");
    if KBD.load(SeqCst) {
        let _ = execute!(out, PopKeyboardEnhancementFlags);
    } else {
        let _ = out.write_all(b"\x1b[>4;0m");
    }
    let _ = execute!(out, DisableBracketedPaste, ResetColor, cursor::Show);
    let _ = terminal::disable_raw_mode();
    write_terminal_seq("\x1b[?5522l");
    let _ = out.flush();
}

/// Writes a control sequence to the terminal. Under tmux it is also sent as a DCS passthrough
/// (needs `allow-passthrough`), since tmux itself drops sequences it does not know.
pub fn write_terminal_seq(seq: &str) {
    let mut out = io::stdout();
    let _ = out.write_all(seq.as_bytes());
    if std::env::var("TMUX").is_ok_and(|v| !v.is_empty()) {
        let _ = write!(out, "\x1bPtmux;{}\x1b\\", seq.replace('\x1b', "\x1b\x1b"));
    }
    let _ = out.flush();
}

/// The bytes that tell the user something happened: always a BEL (the terminal or tmux flags the
/// pane), plus an OSC 9 toast under tmux (DCS passthrough) and in terminals known to show it.
pub fn notify_bytes(msg: &str, env: impl Fn(&str) -> Option<String>) -> String {
    let msg: String = msg.chars().filter(|c| !c.is_control()).take(100).collect();
    let osc = format!("\x1b]9;{msg}\x1b\\");
    let set = |k: &str| env(k).filter(|v| !v.is_empty());
    if set("TMUX").is_some() {
        return format!("\x1bPtmux;{}\x1b\\\x07", osc.replace('\x1b', "\x1b\x1b"));
    }
    let known = set("TERM_PROGRAM")
        .is_some_and(|t| matches!(t.as_str(), "ghostty" | "WezTerm" | "iTerm.app" | "WarpTerminal"))
        || set("LC_TERMINAL").is_some_and(|t| t == "iTerm2");
    if known { osc } else { "\x07".into() }
}

/// Rings the terminal bell (and shows a toast where supported).
pub fn notify(msg: &str) {
    let mut out = io::stdout();
    let _ = out.write_all(notify_bytes(msg, |k| std::env::var(k).ok()).as_bytes());
    let _ = out.flush();
}

/// Records panics instead of printing them over the UI. Many panics are survivable (a tool or
/// task that is caught); the terminal is restored only if one unwinds out of the UI, in
/// `Term::drop`, which then prints the recorded message.
pub fn install_panic_hook() {
    std::panic::set_hook(Box::new(move |info| {
        let msg = format!("mu: internal error: {info}");
        if let Ok(dir) = std::fs::create_dir_all(mu_common::paths::mu_home().join("logs"))
            .map(|()| mu_common::paths::mu_home().join("logs"))
        {
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("panic.log")) {
                let _ = writeln!(f, "{msg}\n{}", std::backtrace::Backtrace::capture());
            }
        }
        if let Ok(mut g) = PANIC.lock() {
            *g = Some(msg);
        }
    }));
}

/// Leaves raw mode and the extra modes, e.g. around an external editor.
pub fn suspend() -> io::Result<()> {
    // A test never owns the terminal: raw mode on the developer's tty stops a background test run.
    if !cfg!(test) {
        off();
    }
    Ok(())
}

/// Re-applies the modes after `suspend`. No-op if already applied.
pub fn resume() -> io::Result<()> {
    if cfg!(test) { Ok(()) } else { on() }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn env(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |k| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string())
    }

    #[test]
    fn paste_events_are_asked_for_outside_tmux_only() {
        assert_eq!(paste_events_seq(false), "\x1b[?5522h");
        assert_eq!(paste_events_seq(true), "\x1b[?5522l");
    }

    #[test]
    fn notify_bytes_per_terminal() {
        assert_eq!(notify_bytes("hi", env(&[])), "\x07");
        assert_eq!(notify_bytes("hi", env(&[("TERM_PROGRAM", "vscode")])), "\x07");
        assert_eq!(notify_bytes("hi", env(&[("TERM_PROGRAM", "ghostty")])), "\x1b]9;hi\x1b\\");
        assert_eq!(notify_bytes("hi", env(&[("LC_TERMINAL", "iTerm2")])), "\x1b]9;hi\x1b\\");
        assert_eq!(
            notify_bytes("hi", env(&[("TMUX", "/tmp/t,1,0"), ("TERM_PROGRAM", "ghostty")])),
            "\x1bPtmux;\x1b\x1b]9;hi\x1b\x1b\\\x1b\\\x07"
        );
        assert_eq!(notify_bytes("a\x1b]b\nc", env(&[])), "\x07");
        assert_eq!(notify_bytes("a\x1bb\nc", env(&[("TERM_PROGRAM", "WezTerm")])), "\x1b]9;abc\x1b\\");
        assert!(notify_bytes(&"x".repeat(500), env(&[("TERM_PROGRAM", "WezTerm")])).len() < 110);
    }

    #[test]
    fn a_hangup_is_caught_not_fatal() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let mut s = Shutdown::new().unwrap();
            // SAFETY: raising a signal that now has a handler installed.
            unsafe { libc::raise(libc::SIGHUP) };
            tokio::time::timeout(std::time::Duration::from_secs(2), s.next()).await.expect("signal delivered");
        });
    }

    #[test]
    fn a_hung_up_terminal_is_recognised() {
        for code in [libc::EIO, libc::EPIPE, libc::ENXIO] {
            assert!(hangup(&io::Error::from_raw_os_error(code)), "os error {code} is a hangup");
        }
        assert!(!hangup(&io::Error::from_raw_os_error(libc::EAGAIN)));
        assert!(!hangup(&io::Error::from(io::ErrorKind::Other)));
    }

    #[test]
    fn a_write_after_the_master_closed_is_a_hangup() {
        use std::os::fd::{FromRawFd, OwnedFd};
        let (mut master, mut slave) = (-1 as libc::c_int, -1 as libc::c_int);
        let ws = libc::winsize { ws_row: 24, ws_col: 80, ws_xpixel: 0, ws_ypixel: 0 };
        // SAFETY: valid out-pointers; name and termios are null. macOS declares the winsize pointer
        // `*mut`, Linux `*const`; the cast is only read through.
        let r = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::addr_of!(ws).cast_mut().cast(),
            )
        };
        assert_eq!(r, 0, "openpty: {}", io::Error::last_os_error());
        // SAFETY: openpty returned two fresh descriptors that nothing else owns.
        let (master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
        drop(master);
        let e = std::fs::File::from(slave).write_all(b"x").expect_err("a hung-up pty takes no writes");
        assert!(hangup(&e), "the write reported {e:?}");
    }

    #[test]
    fn panic_message_is_recorded_for_later() {
        install_panic_hook();
        let _ = std::panic::catch_unwind(|| panic!("boom-recorded"));
        let _ = std::panic::take_hook();
        assert!(take_panic().is_some_and(|m| m.contains("boom-recorded")));
    }
}
