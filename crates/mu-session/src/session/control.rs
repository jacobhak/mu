//! Signalling a session's run only while it is still the run the caller saw: same pid and start
//! time, and the owner record still promises the signal. Used by `mu take` (stop a `-p` run),
//! `mu notify` (wake an interactive owner) and the jobs panel (stop or terminate a run).
use super::owner::{LiveSession, Owner, live_sessions, owner};
use std::path::Path;

/// What the owner record must promise before a signal is sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Want {
    /// A `-p` run: stops at its next tool boundary on SIGUSR1.
    PrintRun,
    /// An interactive owner that installed the SIGUSR2 handler (`inbox: true`).
    Inbox,
    /// Any run that is not `-p` (a subagent's process group gets SIGTERM, which is how its parent cancels it).
    NotPrint,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Recheck {
    /// Still the same run (pid and start time), promising what was asked: safe to signal.
    Same,
    /// Nobody has the session any more.
    Free,
    /// Someone else (another pid, a reused pid, a run that no longer promises the signal, no
    /// record): never signal.
    Changed,
}

/// Compares a fresh owner lookup with the run the caller decided to signal.
pub fn recheck(now: Owner, pid: u32, started: &str, want: Want) -> Recheck {
    match now {
        Owner::Live { pid: p, print_run, inbox, started: s, .. }
            if p == pid
                && s == started
                && match want {
                    Want::PrintRun => print_run,
                    Want::Inbox => inbox,
                    Want::NotPrint => !print_run,
                } =>
        {
            Recheck::Same
        }
        Owner::Free => Recheck::Free,
        _ => Recheck::Changed,
    }
}

pub fn signal(pid: u32) -> Result<(), std::io::Error> {
    mu_base::process::signal_pid(pid, libc::SIGUSR1)
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
        "print" => match recheck(owner(path), pid, started, Want::PrintRun) {
            Recheck::Same => signal(pid)
                .map(|()| format!("asked pid {pid} to stop after its current step"))
                .map_err(|e| format!("cannot signal pid {pid}: {e}")),
            Recheck::Free => Err(GONE.into()),
            Recheck::Changed => Err(CHANGED.into()),
        },
        "subagent" => match recheck(owner(path), pid, started, Want::NotPrint) {
            Recheck::Same => terminate_leader(pid),
            Recheck::Free => Err(GONE.into()),
            Recheck::Changed => Err(CHANGED.into()),
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
    mu_base::process::signal_pid(pid, libc::SIGTERM)
        .map(|()| format!("asked pid {pid} to exit"))
        .map_err(|e| format!("cannot signal pid {pid}: {e}"))
}

/// The live owner record of `path`, if it is still the run (pid and start time) the caller saw.
fn live_run(path: &Path, pid: u32, started: &str) -> Result<LiveSession, String> {
    let abs = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    match live_sessions().into_iter().find(|l| l.path == abs) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inbox_wants_an_owner_that_promises_messages() {
        let t = "2026-01-01T00:00:00.000Z";
        let live = |print_run, inbox| Owner::Live { pid: 7, print_run, started: t.into(), inbox };
        assert_eq!(recheck(live(false, true), 7, t, Want::Inbox), Recheck::Same);
        assert_eq!(recheck(live(false, false), 7, t, Want::Inbox), Recheck::Changed);
        assert_eq!(recheck(live(true, false), 7, t, Want::Inbox), Recheck::Changed);
        assert_eq!(recheck(live(false, true), 7, "other", Want::Inbox), Recheck::Changed);
        assert_eq!(recheck(Owner::Free, 7, t, Want::Inbox), Recheck::Free);
    }

    fn live(pid: u32, print_run: bool, started: &str) -> Owner {
        Owner::Live { pid, print_run, started: started.into(), inbox: false }
    }

    #[test]
    fn only_the_same_print_run_may_be_signalled() {
        let t = "2026-01-01T00:00:00.000Z";
        assert_eq!(recheck(live(7, true, t), 7, t, Want::PrintRun), Recheck::Same);
        // A reused pid starts later.
        assert_eq!(recheck(live(7, true, "2026-01-01T00:00:01.000Z"), 7, t, Want::PrintRun), Recheck::Changed);
        assert_eq!(recheck(live(8, true, t), 7, t, Want::PrintRun), Recheck::Changed);
        // The same pid reopened by an interactive mu.
        assert_eq!(recheck(live(7, false, t), 7, t, Want::PrintRun), Recheck::Changed);
        assert_eq!(recheck(Owner::Unknown, 7, t, Want::PrintRun), Recheck::Changed);
        assert_eq!(recheck(Owner::Free, 7, t, Want::PrintRun), Recheck::Free);
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
        mu_testutil::home();
        let s = crate::session::file::SessionFile::create(Path::new("/opt/stop-tui"), None).unwrap();
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
