//! `mu take <session|id>`, and `mu --resume` / `mu -c`: stop a running `mu -p` at its next tool
//! boundary, then resume the session here, in the TUI. Only POSIX calls (`flock`, `kill`, `exec`),
//! so it is portable.
use mu_common::paths;
use mu_session::session::control::{Recheck, Want, recheck, signal};
use mu_session::session::list::{find, header_cwd};
use mu_session::session::owner::{Owner, is_free, owner};
use std::io::IsTerminal;
use std::path::Path;
use std::time::Duration;

pub fn run(arg: &Path, cwd: &Path) -> anyhow::Result<i32> {
    let path = match find(cwd, arg) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("mu take: {e}");
            return Ok(1);
        }
    };
    // The cwd changes before the exec, so a relative path must not be left relative.
    let path = std::fs::canonicalize(&path).unwrap_or(path);
    // Checked before anything is signalled: a stopped run with nothing to open it is a loss.
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        eprintln!("mu take: needs a terminal on stdin and stdout; nothing was signalled");
        return Ok(1);
    }
    if let Some(code) = release(&path, "mu take", false)? {
        return Ok(code);
    }
    resume(&path)
}

/// Makes `path` free to open: a running `-p` is stopped after its current step and waited for.
/// With `ask`, stopping it needs a yes on the terminal; without a terminal it is refused.
/// `Ok(Some(code))`: stop with that exit code (the reason is printed, prefixed with `who`).
pub fn release(path: &Path, who: &str, ask: bool) -> anyhow::Result<Option<i32>> {
    match owner(path) {
        Owner::Free => Ok(None),
        Owner::Live { pid, print_run: false, .. } => {
            eprintln!("{who}: pid {pid} has it open in an interactive mu (or a subagent/ACP); close it there");
            Ok(Some(1))
        }
        Owner::Unknown => {
            eprintln!(
                "{who}: the session is locked by a process that left no owner record (an older mu, another MU_HOME, or a mirror of a remote subagent's session that is still running); stop it yourself or wait for it"
            );
            Ok(Some(1))
        }
        Owner::Live { pid, print_run: true, started, .. } => {
            if ask {
                let tty = std::io::stdin().is_terminal();
                if let Some(code) = confirm_stop(who, pid, tty, &mut std::io::stdin().lock()) {
                    return Ok(Some(code));
                }
            }
            stop_and_wait(path, pid, &started)
        }
    }
}

/// `release` for a caller with no terminal and no stderr anyone reads (the agent serving an attached client):
/// the reason a session cannot be had comes back as the error. Never asks.
pub fn release_quiet(path: &Path) -> Result<(), String> {
    match owner(path) {
        Owner::Free => Ok(()),
        Owner::Live { pid, print_run: false, .. } => {
            Err(format!("pid {pid} has the session open in an interactive mu (or a subagent/ACP); close it there"))
        }
        Owner::Unknown => Err("the session is locked by a process that left no owner record (an older mu, another \
                               MU_HOME, or a mirror of a remote subagent's session that is still running); stop it \
                               yourself or wait for it"
            .into()),
        Owner::Live { pid, print_run: true, started, .. } => match stop_and_wait(path, pid, &started) {
            Ok(None) => Ok(()),
            Ok(Some(_)) => Err(format!("could not stop the `mu -p` run (pid {pid}); see the agent's log")),
            Err(e) => Err(e.to_string()),
        },
    }
}

/// `None`: the user agreed to stop the run. `Some(code)`: leave it running and exit with `code`.
fn confirm_stop(who: &str, pid: u32, tty: bool, input: &mut impl std::io::BufRead) -> Option<i32> {
    if !tty {
        eprintln!(
            "{who}: pid {pid} is a running `mu -p` on this session; stopping it needs a yes on a terminal, and stdin is not one. Nothing was signalled; `mu take <session>` stops it without asking"
        );
        return Some(1);
    }
    eprint!(
        "{who}: pid {pid} is running `mu -p` on this session. Stop it after its current step and take over? [y/N] "
    );
    let mut line = String::new();
    let _ = input.read_line(&mut line);
    if matches!(line.trim(), "y" | "Y" | "yes") {
        return None;
    }
    eprintln!("{who}: not stopped; the run continues");
    Some(1)
}

/// `Ok(None)`: the session is free, go on. `Ok(Some(code))`: stop with that exit code.
fn stop_and_wait(path: &Path, pid: u32, started: &str) -> anyhow::Result<Option<i32>> {
    let rt = mu_base::runtime::current_thread_runtime()?;
    // Registered before signalling, so a Ctrl-C right after is not lost.
    let mut int = {
        let _g = rt.enter();
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?
    };
    // The record was read a moment ago; make sure it is still the same live run before the first kill.
    match recheck(owner(path), pid, started, Want::PrintRun) {
        Recheck::Same => {}
        Recheck::Free => return Ok(None),
        Recheck::Changed => {
            eprintln!("mu: the session changed hands while starting; nothing was signalled, try again");
            return Ok(Some(1));
        }
    }
    match signal(pid) {
        Ok(()) => eprintln!("asked pid {pid} to stop after its current step; Ctrl-C to stop now"),
        Err(e) if e.raw_os_error() == Some(libc::ESRCH) => {}
        Err(e) => {
            eprintln!("mu: cannot signal pid {pid}: {e}");
            return Ok(Some(1));
        }
    }
    let code = rt.block_on(async {
        // (borrowed from the enclosing scope: `int` outlives this block)
        let mut tick = tokio::time::interval(Duration::from_millis(200));
        let (mut waited, mut interrupts) = (0u32, 0u32);
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    if is_free(path) {
                        return None;
                    }
                    waited += 1;
                    if waited % 150 == 0 {
                        eprintln!("still waiting for pid {pid} to finish its current step ({}s)", waited / 5);
                    }
                }
                _ = int.recv() => {
                    interrupts += 1;
                    if interrupts >= 2 {
                        eprintln!("giving up; the run will stop on its own");
                        return Some(130);
                    }
                    // Only signal again while it is still the same live run: the pid may have
                    // been reused after it exited.
                    if recheck(owner(path), pid, started, Want::PrintRun) == Recheck::Same {
                        let _ = signal(pid);
                        eprintln!("asked pid {pid} to stop now");
                    }
                }
            }
        }
    });
    drop(rt);
    Ok(code)
}

fn resume(path: &Path) -> anyhow::Result<i32> {
    use std::os::unix::process::CommandExt;
    if let Some(dir) = header_cwd(path) {
        if dir.is_dir() {
            std::env::set_current_dir(&dir)?;
        } else {
            eprintln!("mu take: {} no longer exists; staying in the current directory", dir.display());
        }
    }
    let exe = paths::self_exe()?;
    let err = std::process::Command::new(exe).arg("--resume").arg(path).exec();
    eprintln!("mu take: cannot exec mu: {err}");
    Ok(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mu_session::session::file::SessionFile;
    use mu_testutil::home;

    #[test]
    fn an_unknown_session_is_refused_before_anything_is_signalled() {
        home();
        assert_eq!(run(Path::new("no-such-session-zzz"), &std::env::temp_dir()).unwrap(), 1);
    }

    #[test]
    fn a_free_session_needs_no_release() {
        home();
        let s = SessionFile::create(Path::new("/opt/take-free"), None).unwrap();
        let path = s.path.clone();
        drop(s);
        assert_eq!(release(&path, "mu take", true).unwrap(), None, "nothing to ask about");
    }

    #[test]
    fn stopping_a_live_run_needs_a_yes_and_a_terminal() {
        let ask = |tty: bool, answer: &str| confirm_stop("mu", 7, tty, &mut answer.as_bytes());
        assert_eq!(ask(true, "y\n"), None);
        assert_eq!(ask(true, "yes\n"), None);
        assert_eq!(ask(true, "\n"), Some(1), "the default is no");
        assert_eq!(ask(true, ""), Some(1), "end of input is no");
        assert_eq!(ask(false, "y\n"), Some(1), "no terminal: refused whatever is piped in");
    }

    /// This process holds the session without being a `-p` run: `release` refuses and, being
    /// ourselves, would have signalled this test had it tried.
    #[test]
    fn a_session_open_in_an_interactive_mu_is_refused() {
        home();
        let s = SessionFile::create(Path::new("/opt/take-held"), None).unwrap();
        assert_eq!(release(&s.path, "mu take", true).unwrap(), Some(1));
        drop(s);
    }
}
