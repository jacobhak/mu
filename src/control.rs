//! Signalling a session's run only while it is still the run the caller saw: same pid and start
//! time, and the owner record still promises the signal. Used by `mu take` (stop a `-p` run) and
//! `mu notify` (wake an interactive owner).
//!
//! Still duplicated elsewhere (Phase 4 dedupes these): the subagent arm of `take::stop_session`
//! (checks `print_run: false` inline) and `take::live_run` (looks the run up in
//! `session::live_sessions()` instead of the owner record).
use crate::session::Owner;

/// What the owner record must promise before a signal is sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Want {
    /// A `-p` run: stops at its next tool boundary on SIGUSR1.
    PrintRun,
    /// An interactive owner that installed the SIGUSR2 handler (`inbox: true`).
    Inbox,
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
                } =>
        {
            Recheck::Same
        }
        Owner::Free => Recheck::Free,
        _ => Recheck::Changed,
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
}
