//! When to ring the terminal. A turn that needs the user (`ask`, an approval, plan review) rings
//! once when that happens, whoever started it. A turn the user typed also rings when it ends,
//! unless a background command or subagent is still running or it already rang for a prompt. A
//! turn mu started itself (a `mu notify` message, a finished background job or subagent) rings
//! only for such a prompt, never because it finished. Pure state, so it is testable without a
//! terminal.

#[derive(Default)]
pub struct WokenBell {
    woken: bool,
    rang: bool,
}

impl WokenBell {
    /// A turn began: a prompt answered between turns does not silence it.
    pub fn on_turn_start(&mut self) {
        self.rang = false;
    }

    /// The running turn started without user input.
    pub fn on_woken(&mut self) {
        self.woken = true;
        self.rang = false;
    }

    /// The turn is waiting for the user (`ask`, an approval, plan review). True when it should
    /// ring now: once per turn, whether the user or mu started it.
    pub fn on_attention(&mut self) -> bool {
        let ring = !self.rang;
        if ring {
            self.rang = true;
        }
        ring
    }

    /// The turn ended. True when it should ring: the user started it, did not abort it, it never
    /// rang for a prompt and nothing runs in the background.
    pub fn on_turn_end(&mut self, aborted: bool, jobs_running: bool) -> bool {
        let ring = !self.woken && !aborted && !jobs_running && !self.rang;
        *self = Self::default();
        ring
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_turn_rings_once_when_it_needs_the_user() {
        let mut b = WokenBell::default();
        assert!(b.on_attention());
        assert!(!b.on_attention());
    }

    #[test]
    fn a_woken_turn_rings_once_when_it_needs_the_user() {
        let mut b = WokenBell::default();
        b.on_woken();
        assert!(b.on_attention());
        assert!(!b.on_attention());
    }

    #[test]
    fn a_user_turn_rings_at_the_end_when_nothing_runs() {
        let mut b = WokenBell::default();
        assert!(b.on_turn_end(false, false));
    }

    #[test]
    fn a_user_turn_stays_quiet_while_jobs_run_or_when_aborted() {
        let mut b = WokenBell::default();
        assert!(!b.on_turn_end(false, true));
        assert!(!b.on_turn_end(true, false));
    }

    #[test]
    fn a_woken_turn_does_not_ring_at_the_end() {
        let mut b = WokenBell::default();
        b.on_woken();
        assert!(!b.on_turn_end(false, false));
        assert!(b.on_turn_end(false, false), "the next turn is a user turn again");
    }

    #[test]
    fn a_user_turn_that_rang_for_a_prompt_does_not_ring_at_the_end() {
        let mut b = WokenBell::default();
        assert!(b.on_attention());
        assert!(!b.on_turn_end(false, false));
    }

    #[test]
    fn a_prompt_between_turns_does_not_silence_the_next_turn() {
        let mut b = WokenBell::default();
        assert!(b.on_attention(), "a prompt with no turn running still rings");
        b.on_turn_start();
        assert!(b.on_turn_end(false, false));
    }
}
