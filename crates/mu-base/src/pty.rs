//! Pseudo-terminal child processes for `bash` with `pty: true` (Unix only).
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::raw::c_int;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::io::unix::AsyncFd;

/// `$SHELL` when it is bash or zsh, else the first `bash` on `PATH`, else `/bin/sh`.
pub(crate) fn login_shell() -> String {
    if let Ok(sh) = std::env::var("SHELL")
        && matches!(Path::new(&sh).file_name().and_then(|n| n.to_str()), Some("bash" | "zsh"))
    {
        return sh;
    }
    if let Some(bash) = bash_on_path() {
        return bash.to_string_lossy().into_owned();
    }
    "/bin/sh".into()
}

/// The first `bash` on `PATH`; `None` when there is none, or `PATH` is unset.
pub fn bash_on_path() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join("bash")).find(|p| p.is_file())
}

/// The terminal side of a pty: I/O and signalling. Shared so reads can run while `child` is awaited.
pub struct PtyIo {
    master: AsyncFd<OwnedFd>,
    pid: i32,
}

pub struct Pty {
    pub io: std::rc::Rc<PtyIo>,
    pub child: tokio::process::Child,
}

impl std::ops::Deref for Pty {
    type Target = PtyIo;
    fn deref(&self) -> &PtyIo {
        &self.io
    }
}

pub fn spawn(command: &str, cwd: &Path, cols: u16, rows: u16, env: &[(&str, String)]) -> io::Result<Pty> {
    let (mut m, mut s) = (-1 as c_int, -1 as c_int);
    let ws = libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
    // SAFETY: valid out-pointers; name and termios are null. macOS declares the winsize pointer
    // `*mut`, Linux `*const`; the cast is only read through.
    let r = unsafe {
        libc::openpty(
            &mut m,
            &mut s,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::addr_of!(ws).cast_mut().cast(),
        )
    };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: openpty returned two fresh descriptors that nothing else owns.
    let (master, slave) = unsafe { (OwnedFd::from_raw_fd(m), OwnedFd::from_raw_fd(s)) };
    // SAFETY: plain fcntl calls on a descriptor we own.
    unsafe {
        let fd = master.as_raw_fd();
        let fl = libc::fcntl(fd, libc::F_GETFL);
        if fl < 0 || libc::fcntl(fd, libc::F_SETFL, fl | libc::O_NONBLOCK) < 0 {
            return Err(io::Error::last_os_error());
        }
        let fdf = libc::fcntl(fd, libc::F_GETFD);
        if fdf < 0 || libc::fcntl(fd, libc::F_SETFD, fdf | libc::FD_CLOEXEC) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    let mut cmd = tokio::process::Command::new(login_shell());
    cmd.arg("-lc").arg(command).current_dir(cwd).env("TERM", "xterm-256color");
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.stdin(Stdio::from(slave.try_clone()?));
    cmd.stdout(Stdio::from(slave.try_clone()?));
    cmd.stderr(Stdio::from(slave));
    // SAFETY: only async-signal-safe calls (setsid, ioctl) between fork and exec.
    unsafe {
        cmd.pre_exec(|| {
            crate::process::new_session()?;
            if libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = cmd.spawn();
    // Close the parent's copies of the slave so reads see EOF/EIO when the child side closes.
    drop(cmd);
    let child = child?;
    let pid = child.id().map(|p| p as i32).unwrap_or(0);
    let interest = tokio::io::Interest::READABLE | tokio::io::Interest::WRITABLE;
    // SAFETY: `master` is an owned, non-blocking descriptor that stays open as long as the `AsyncFd`.
    let master = unsafe { AsyncFd::register_with_interest(master, interest) }?;
    Ok(Pty { io: std::rc::Rc::new(PtyIo { master, pid }), child })
}

impl PtyIo {
    /// The child's pid, which is also its process group id.
    pub fn pid(&self) -> i32 {
        self.pid
    }

    /// `Ok(0)` on EOF and on `EIO` (Linux: every slave fd closed).
    pub async fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            let mut guard = self.master.readable().await?;
            let res = guard.try_io(|fd| {
                // SAFETY: `buf` is valid for `buf.len()` bytes for the call.
                let n = unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
                if n < 0 { Err(io::Error::last_os_error()) } else { Ok(n as usize) }
            });
            match res {
                Ok(Ok(n)) => return Ok(n),
                Ok(Err(e)) if e.raw_os_error() == Some(libc::EIO) => return Ok(0),
                Ok(Err(e)) => return Err(e),
                Err(_) => continue,
            }
        }
    }

    /// Waits until the master is writable, then writes what fits. Returns the bytes written.
    pub async fn write_some(&self, data: &[u8]) -> io::Result<usize> {
        loop {
            let mut guard = self.master.writable().await?;
            let res = guard.try_io(|fd| {
                // SAFETY: `data` is valid for `data.len()` bytes for the call.
                let n = unsafe { libc::write(fd.as_raw_fd(), data.as_ptr().cast(), data.len()) };
                if n < 0 { Err(io::Error::last_os_error()) } else { Ok(n as usize) }
            });
            match res {
                Ok(r) => return r,
                Err(_) => continue,
            }
        }
    }

    pub fn resize(&self, cols: u16, rows: u16) {
        let ws = libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
        // SAFETY: valid winsize pointer, owned master fd.
        unsafe { libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ as _, &ws) };
    }

    /// Signals the whole process group; only call before the child has been reaped.
    pub fn signal(&self, sig: c_int) {
        if self.pid > 0 {
            // SAFETY: plain kill(2).
            unsafe { libc::kill(-self.pid, sig) };
        }
    }

    pub fn hangup(&self) {
        if self.pid > 0 {
            // SAFETY: plain kill(2).
            unsafe { libc::kill(self.pid, libc::SIGHUP) };
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum State {
    #[default]
    Ground,
    Esc,
    Csi,
    Str,
    StrEsc,
    Cr,
}

/// Turns raw terminal text into model-facing text, keeping state across chunks.
#[derive(Default)]
pub struct Cleaner {
    state: State,
    pub(crate) line: String,
}

const MAX_LINE: usize = 16 * 1024;

impl Cleaner {
    pub fn push(&mut self, s: &str) -> String {
        let mut out = String::new();
        for c in s.chars() {
            self.step(c, &mut out);
        }
        out
    }

    pub fn finish(&mut self) -> String {
        self.state = State::Ground;
        std::mem::take(&mut self.line)
    }

    fn step(&mut self, c: char, out: &mut String) {
        match self.state {
            State::Esc => {
                self.state = match c {
                    '[' => State::Csi,
                    ']' | 'P' | '_' | '^' => State::Str,
                    _ => State::Ground,
                };
            }
            State::Csi => {
                if ('\x40'..='\x7e').contains(&c) {
                    self.state = State::Ground;
                }
            }
            State::Str => match c {
                '\x07' => self.state = State::Ground,
                '\x1b' => self.state = State::StrEsc,
                _ => {}
            },
            State::StrEsc => {
                self.state = if c == '\\' { State::Ground } else { State::Str };
            }
            State::Cr => {
                if c == '\r' {
                    return; // `\r\r\n` is still a line ending
                }
                self.state = State::Ground;
                if c == '\n' {
                    self.newline(out);
                } else {
                    self.line.clear();
                    self.ground(c, out);
                }
            }
            State::Ground => self.ground(c, out),
        }
    }

    fn newline(&mut self, out: &mut String) {
        out.push_str(&self.line);
        out.push('\n');
        self.line.clear();
    }

    fn ground(&mut self, c: char, out: &mut String) {
        match c {
            '\x1b' => self.state = State::Esc,
            '\r' => self.state = State::Cr,
            '\n' => self.newline(out),
            '\x08' => {
                self.line.pop();
            }
            '\t' => self.line.push(c),
            c if c < ' ' || c == '\x7f' => {}
            c => {
                self.line.push(c);
                if self.line.len() > MAX_LINE {
                    out.push_str(&self.line);
                    self.line.clear();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clean(s: &str) -> String {
        let mut c = Cleaner::default();
        let mut o = c.push(s);
        o.push_str(&c.finish());
        o
    }

    #[test]
    fn double_cr_before_newline_keeps_the_line() {
        assert_eq!(clean("abc\r\r\n"), "abc\n");
        assert_eq!(clean("abc\rdef\n"), "def\n");
    }

    #[test]
    fn strips_csi() {
        assert_eq!(clean("\x1b[31mred\x1b[0m\r\n"), "red\n");
    }

    #[test]
    fn carriage_return_overwrites() {
        assert_eq!(clean("10%\r50%\r100%\r\ndone\n"), "100%\ndone\n");
    }

    #[test]
    fn strips_osc() {
        assert_eq!(clean("\x1b]0;title\x07x\n"), "x\n");
    }

    #[test]
    fn split_escape() {
        let mut c = Cleaner::default();
        let mut o = c.push("a\x1b[3");
        o.push_str(&c.push("1mb\n"));
        assert_eq!(o, "ab\n");
    }

    #[test]
    fn backspace() {
        assert_eq!(clean("ab\x08c\n"), "ac\n");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn spawn_echo() {
        let env = [("PI_X", "yes".to_string())];
        let mut p = spawn("[ -t 0 ] && echo tty; echo env:$PI_X; read x; echo got:$x", Path::new("/tmp"), 80, 24, &env)
            .unwrap();
        p.io.write_some(b"hi\r").await.unwrap();
        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let n = p.read(&mut buf).await.unwrap();
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
        let s = String::from_utf8_lossy(&out);
        assert!(s.contains("tty") && s.contains("got:hi") && s.contains("env:yes"), "{s:?}");
        assert_eq!(p.child.wait().await.unwrap().code(), Some(0));
    }
}
