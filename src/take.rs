//! `mu take <session|id>`: stop a running `mu -p` at its next tool boundary, then resume the
//! session here, in the TUI. Only POSIX calls (`flock`, `kill`, `exec`), so it is portable.
use crate::session::{self, Owner};
use mu_common::paths;
use std::io::IsTerminal;
use std::path::Path;
use std::time::Duration;

pub fn run(arg: &Path, cwd: &Path) -> anyhow::Result<i32> {
    let path = match session::find(cwd, arg) {
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
    match session::owner(&path) {
        Owner::Free => eprintln!("not running; resuming"),
        Owner::Live { pid, print_run: false, .. } => {
            eprintln!("mu take: pid {pid} has it open in an interactive mu (or a subagent/ACP); close it there");
            return Ok(1);
        }
        Owner::Unknown => {
            eprintln!(
                "mu take: the session is locked by a process that left no owner record (an older mu, or another MU_HOME); stop it yourself"
            );
            return Ok(1);
        }
        Owner::Live { pid, print_run: true, started, .. } => {
            if let Some(code) = stop_and_wait(&path, pid, &started)? {
                return Ok(code);
            }
        }
    }
    resume(&path)
}

fn signal(pid: u32) -> Result<(), std::io::Error> {
    crate::proc::signal_pid(pid, libc::SIGUSR1)
}

/// `Ok(None)`: the session is free, go on. `Ok(Some(code))`: stop with that exit code.
fn stop_and_wait(path: &Path, pid: u32, started: &str) -> anyhow::Result<Option<i32>> {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    // Registered before signalling, so a Ctrl-C right after is not lost.
    let mut int = {
        let _g = rt.enter();
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?
    };
    // The record was read a moment ago; make sure it is still the same live run before the first kill.
    match verify(session::owner(path), pid, started) {
        Recheck::Same => {}
        Recheck::Free => return Ok(None),
        Recheck::Changed => {
            eprintln!("mu take: the session changed hands while starting; nothing was signalled, try again");
            return Ok(Some(1));
        }
    }
    match signal(pid) {
        Ok(()) => eprintln!("asked pid {pid} to stop after its current step; Ctrl-C to stop now"),
        Err(e) if e.raw_os_error() == Some(libc::ESRCH) => {}
        Err(e) => {
            eprintln!("mu take: cannot signal pid {pid}: {e}");
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
                    if session::is_free(path) {
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
                    if verify(session::owner(path), pid, started) == Recheck::Same {
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

#[derive(Debug, PartialEq, Eq)]
enum Recheck {
    /// Still the same `-p` run (pid and start time): safe to signal.
    Same,
    /// Nobody has the session any more.
    Free,
    /// Someone else (another pid, a reused pid, an interactive owner, no record): never signal.
    Changed,
}

/// Compares a fresh owner lookup with the run `mu take` decided to stop.
fn verify(now: Owner, pid: u32, started: &str) -> Recheck {
    match now {
        Owner::Live { pid: p, print_run: true, started: s, .. } if p == pid && s == started => Recheck::Same,
        Owner::Free => Recheck::Free,
        _ => Recheck::Changed,
    }
}

const GONE: &str = "it has already finished";
const CHANGED: &str = "the session changed hands; nothing was signalled";

/// `s` in the jobs panel: stops the run the user saw, without taking it over. A `-p` run gets SIGUSR1
/// and stops after its current step; a subagent's process group gets SIGTERM, which is how its parent
/// cancels it; an interactive mu that said it handles SIGUSR1 cancels its current turn and keeps
/// running. Nothing is signalled unless the session's owner is still that run (same pid and start
/// time).
pub fn stop_session(path: &Path, pid: u32, started: &str, kind: &str) -> Result<String, String> {
    match kind {
        "print" => match verify(session::owner(path), pid, started) {
            Recheck::Same => signal(pid)
                .map(|()| format!("asked pid {pid} to stop after its current step"))
                .map_err(|e| format!("cannot signal pid {pid}: {e}")),
            Recheck::Free => Err(GONE.into()),
            Recheck::Changed => Err(CHANGED.into()),
        },
        "subagent" => match session::owner(path) {
            Owner::Live { pid: p, print_run: false, started: s, .. } if p == pid && s == started => {
                terminate_leader(pid)
            }
            Owner::Free => Err(GONE.into()),
            _ => Err(CHANGED.into()),
        },
        "tui" => {
            let l = live_run(path, pid, started)?;
            if l.kind.as_deref() != Some("tui") || !l.abort {
                return Err("that mu does not take abort requests (an older version); use S to terminate it".into());
            }
            signal(pid)
                .map(|()| format!("asked pid {pid} to abort its current turn"))
                .map_err(|e| format!("cannot signal pid {pid}: {e}"))
        }
        _ => Err("only -p runs, subagents and interactive mu can be stopped from here".into()),
    }
}

/// `S` in the jobs panel, after a confirmation: asks an interactive mu to exit (SIGTERM; it restores
/// its terminal on the way out). Only for the run the user saw, and only an interactive mu.
pub fn terminate_session(path: &Path, pid: u32, started: &str) -> Result<String, String> {
    let l = live_run(path, pid, started)?;
    if l.kind.as_deref() != Some("tui") {
        return Err("only an interactive mu can be terminated from here".into());
    }
    crate::proc::signal_pid(pid, libc::SIGTERM)
        .map(|()| format!("asked pid {pid} to exit"))
        .map_err(|e| format!("cannot signal pid {pid}: {e}"))
}

/// The live owner record of `path`, if it is still the run (pid and start time) the caller saw.
fn live_run(path: &Path, pid: u32, started: &str) -> Result<session::LiveSession, String> {
    let abs = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    match session::live_sessions().into_iter().find(|l| l.path == abs) {
        Some(l) if l.pid == pid && l.started == started => Ok(l),
        Some(_) => Err(CHANGED.into()),
        None => Err(GONE.into()),
    }
}

/// SIGTERM to the process group `pid` leads. A pid that leads no group of its own is left alone: its
/// group is someone else's.
fn terminate_leader(pid: u32) -> Result<String, String> {
    let p = libc::pid_t::try_from(pid).map_err(|_| "bad pid".to_string())?;
    // SAFETY: plain syscalls on integers.
    if unsafe { libc::getpgid(p) } != p {
        return Err(format!("pid {pid} does not lead its own process group; nothing was signalled"));
    }
    // SAFETY: as above.
    if unsafe { libc::kill(-p, libc::SIGTERM) } == 0 {
        Ok(format!("asked pid {pid} to terminate"))
    } else {
        Err(format!("cannot signal pid {pid}: {}", std::io::Error::last_os_error()))
    }
}

fn resume(path: &Path) -> anyhow::Result<i32> {
    use std::os::unix::process::CommandExt;
    if let Some(dir) = session::header_cwd(path) {
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

    fn live(pid: u32, print_run: bool, started: &str) -> Owner {
        Owner::Live { pid, print_run, started: started.into(), inbox: false }
    }

    #[test]
    fn only_the_same_print_run_may_be_signalled() {
        let t = "2026-01-01T00:00:00.000Z";
        assert_eq!(verify(live(7, true, t), 7, t), Recheck::Same);
        // A reused pid starts later.
        assert_eq!(verify(live(7, true, "2026-01-01T00:00:01.000Z"), 7, t), Recheck::Changed);
        assert_eq!(verify(live(8, true, t), 7, t), Recheck::Changed);
        // The same pid reopened by an interactive mu.
        assert_eq!(verify(live(7, false, t), 7, t), Recheck::Changed);
        assert_eq!(verify(Owner::Unknown, 7, t), Recheck::Changed);
        assert_eq!(verify(Owner::Free, 7, t), Recheck::Free);
    }

    #[test]
    fn stopping_refuses_other_kinds_and_sessions_nobody_has() {
        let t = "2026-01-01T00:00:00.000Z";
        let missing = Path::new("/definitely/not/a/session_x.jsonl");
        assert!(stop_session(missing, 7, t, "acp").unwrap_err().contains("can be stopped from here"));
        assert!(stop_session(missing, 7, t, "tui").unwrap_err().contains("already finished"));
        assert!(terminate_session(missing, 7, t).unwrap_err().contains("already finished"));
        assert!(stop_session(missing, 7, t, "print").unwrap_err().contains("already finished"));
        assert!(stop_session(missing, 7, t, "subagent").unwrap_err().contains("already finished"));
    }

    #[test]
    fn an_interactive_run_is_only_signalled_while_it_is_the_one_listed() {
        crate::testutil::home();
        let s = session::SessionFile::create(Path::new("/opt/stop-tui"), None).unwrap();
        let me = std::process::id();
        // Another pid, or another start time: the record is somebody else's now. Never our own pid
        // with the right start time here, which would signal this test process.
        assert!(stop_session(&s.path, me + 1, "x", "tui").unwrap_err().contains("changed hands"));
        assert!(terminate_session(&s.path, me + 1, "x").unwrap_err().contains("changed hands"));
        assert!(stop_session(&s.path, me, "not the start time", "tui").unwrap_err().contains("changed hands"));
        drop(s);
    }

    #[test]
    fn a_group_leader_is_terminated_and_anyone_else_is_left_alone() {
        use std::os::unix::process::CommandExt;
        use std::os::unix::process::ExitStatusExt;
        let mut leader = std::process::Command::new("sleep").arg("30").process_group(0).spawn().unwrap();
        let msg = terminate_leader(leader.id()).unwrap();
        assert!(msg.contains("terminate"), "{msg}");
        assert_eq!(leader.wait().unwrap().signal(), Some(libc::SIGTERM));

        // Shares this test's group, so it is not a leader.
        let mut follower = std::process::Command::new("sleep").arg("30").spawn().unwrap();
        let err = terminate_leader(follower.id()).unwrap_err();
        assert!(err.contains("does not lead its own process group"), "{err}");
        assert!(follower.try_wait().unwrap().is_none(), "it was not signalled");
        let _ = follower.kill();
        let _ = follower.wait();
    }
}
