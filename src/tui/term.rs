//! Terminal modes for the inline UI. No alternate screen.

use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering::SeqCst};

use crossterm::cursor;
use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
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
        off();
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
        execute!(
            out,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )?;
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

/// Restores the terminal, then runs the previous hook so the message prints in normal mode.
pub fn install_panic_hook() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        off();
        prev(info);
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
