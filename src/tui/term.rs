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
        if fatal {
            let msg = PANIC.lock().ok().and_then(|mut g| g.take());
            eprint!("\r\n{}\r\n", msg.unwrap_or_else(|| "mu: internal error".into()));
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
    }
    write_terminal_seq("\x1b[?5522h");
    Ok(())
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
    off();
    Ok(())
}

/// Re-applies the modes after `suspend`. No-op if already applied.
pub fn resume() -> io::Result<()> {
    on()
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn panic_message_is_recorded_for_later() {
        install_panic_hook();
        let _ = std::panic::catch_unwind(|| panic!("boom-recorded"));
        let _ = std::panic::take_hook();
        assert!(take_panic().is_some_and(|m| m.contains("boom-recorded")));
    }
}
