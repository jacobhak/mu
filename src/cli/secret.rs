//! Reading a secret from stdin without echo, for `mu auth add` and `mu providers add`.

use anyhow::{Result, anyhow};
use std::io::{IsTerminal, Read, Write};

/// A secret from stdin: a no-echo prompt on a terminal (after `hint`, when given), else the piped
/// text. Shared by `mu auth add` and `mu providers add`; a key is never a command argument.
pub(crate) fn read_secret(hint: &str, prompt: &str) -> Result<String> {
    let stdin = std::io::stdin();
    if !stdin.is_terminal() {
        let mut s = String::new();
        stdin.lock().read_to_string(&mut s)?;
        return Ok(s.trim().to_string());
    }
    if !hint.is_empty() {
        eprintln!("{hint}");
    }
    eprint!("{prompt} (input is hidden): ");
    std::io::stderr().flush()?;
    let key = with_echo_off(|| {
        let mut lock = stdin.lock();
        Ok(collect_key(|| {
            let mut b = [0u8];
            loop {
                match lock.read(&mut b) {
                    Ok(1) => return Some(b[0]),
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    _ => return None,
                }
            }
        }))
    })?;
    eprintln!();
    key.ok_or_else(|| anyhow!("cancelled; nothing saved"))
}

/// Reads a typed key up to a newline, one byte at a time (echo and line editing are off). `None`
/// when cancelled: Ctrl-C, or Ctrl-D on an empty line.
fn collect_key(mut next: impl FnMut() -> Option<u8>) -> Option<String> {
    let mut key = Vec::new();
    while let Some(b) = next() {
        match b {
            b'\n' | b'\r' => break,
            0x03 => return None,
            0x04 if key.is_empty() => return None,
            0x04 => {}
            0x08 | 0x7f => {
                key.pop();
            }
            b => key.push(b),
        }
    }
    Some(String::from_utf8_lossy(&key).trim().to_string())
}

/// The termios state to put back if a signal ends the process while echo is off.
static SAVED_TERMIOS: std::sync::OnceLock<libc::termios> = std::sync::OnceLock::new();

/// Signals that would otherwise leave the terminal without echo.
const RESTORE_ON: [libc::c_int; 3] = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP];

extern "C" fn restore_and_reraise(sig: libc::c_int) {
    if let Some(t) = SAVED_TERMIOS.get() {
        // SAFETY: tcsetattr is async-signal-safe, and the saved state is never changed once set.
        unsafe { libc::tcsetattr(0, libc::TCSANOW, t) };
    }
    // SAFETY: default action, then re-raise so the exit status still reports the signal.
    unsafe {
        libc::signal(sig, libc::SIG_DFL);
        libc::raise(sig);
    }
}

/// Echo and signal keys are off on stdin while this lives. Dropping it, or one of `RESTORE_ON`
/// arriving, puts the terminal back.
struct NoEcho {
    saved: libc::termios,
    handlers: Vec<(libc::c_int, libc::sighandler_t)>,
}

impl NoEcho {
    fn start() -> std::io::Result<Self> {
        // SAFETY: tcgetattr fills the zeroed struct when it succeeds.
        let saved: libc::termios = unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut t) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            t
        };
        let _ = SAVED_TERMIOS.set(saved);
        let mut quiet = saved;
        // Byte at a time with no signals from keys: Ctrl-C and Ctrl-D reach `collect_key` as bytes.
        quiet.c_lflag &= !(libc::ECHO | libc::ECHONL | libc::ICANON | libc::ISIG);
        quiet.c_cc[libc::VMIN] = 1;
        quiet.c_cc[libc::VTIME] = 0;
        // SAFETY: `quiet` is `saved` with these flags changed.
        if unsafe { libc::tcsetattr(0, libc::TCSANOW, &quiet) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut guard = Self { saved, handlers: Vec::new() };
        let handler = restore_and_reraise as extern "C" fn(libc::c_int) as libc::sighandler_t;
        for sig in RESTORE_ON {
            // SAFETY: the handler only makes async-signal-safe calls.
            let prev = unsafe { libc::signal(sig, handler) };
            if prev != libc::SIG_ERR {
                guard.handlers.push((sig, prev));
            }
        }
        Ok(guard)
    }
}

impl Drop for NoEcho {
    fn drop(&mut self) {
        for (sig, prev) in self.handlers.drain(..) {
            // SAFETY: reinstalls the handler that was there before.
            unsafe { libc::signal(sig, prev) };
        }
        // SAFETY: the state tcgetattr returned at start.
        if unsafe { libc::tcsetattr(0, libc::TCSANOW, &self.saved) } != 0 {
            eprintln!("warning: could not restore terminal echo; run `stty echo`");
        }
    }
}

/// Runs `f` with terminal echo off on stdin, restored afterwards (also on error and on signals).
fn with_echo_off<T>(f: impl FnOnce() -> std::io::Result<T>) -> std::io::Result<T> {
    let _guard = NoEcho::start()?;
    f()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(s: &'static [u8]) -> impl FnMut() -> Option<u8> {
        let mut it = s.iter().copied();
        move || it.next()
    }

    #[test]
    fn typed_keys_are_read_bytewise_and_ctrl_c_cancels() {
        assert_eq!(collect_key(feed(b"sk-abc\r")).as_deref(), Some("sk-abc"));
        assert_eq!(collect_key(feed(b"sk\x7f\x7fx-1\n")).as_deref(), Some("x-1"));
        assert_eq!(collect_key(feed(b"ab\x04\n")).as_deref(), Some("ab"));
        assert_eq!(collect_key(feed(b"sk-ab\x03\n")), None);
        assert_eq!(collect_key(feed(b"\x04")), None);
    }
}
