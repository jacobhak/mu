//! When to ring the terminal for a turn nobody typed (a `mu notify` message or a finished
//! background job woke the agent). Pure state, so it is testable without a terminal.

#[derive(Default)]
pub struct WokenBell {
    woken: bool,
    rang: bool,
}

impl WokenBell {
    /// The running turn started without user input.
    pub fn on_woken(&mut self) {
        self.woken = true;
        self.rang = false;
    }

    /// The turn is waiting for the user (`ask`, plan review). True when it should ring now.
    pub fn on_attention(&mut self) -> bool {
        let ring = self.woken && !self.rang;
        if ring {
            self.rang = true;
        }
        ring
    }

    /// The turn ended. True when it should ring: a woken turn that did not already ring and was
    /// not aborted by the user.
    pub fn on_turn_end(&mut self, aborted: bool) -> bool {
        let ring = self.woken && !self.rang && !aborted;
        *self = Self::default();
        ring
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_woken_turn_rings_once_at_the_end() {
        let mut b = WokenBell::default();
        b.on_woken();
        assert!(b.on_turn_end(false));
        assert!(!b.on_turn_end(false));
    }

    #[test]
    fn a_user_turn_never_rings() {
        let mut b = WokenBell::default();
        assert!(!b.on_attention());
        assert!(!b.on_turn_end(false));
    }

    #[test]
    fn an_aborted_woken_turn_does_not_ring() {
        let mut b = WokenBell::default();
        b.on_woken();
        assert!(!b.on_turn_end(true));
    }

    #[test]
    fn attention_then_end_rings_once() {
        let mut b = WokenBell::default();
        b.on_woken();
        assert!(b.on_attention());
        assert!(!b.on_attention());
        assert!(!b.on_turn_end(false));
    }
}
