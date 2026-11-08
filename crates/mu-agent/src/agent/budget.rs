//! A subagent's soft request budget, ported from omp: the request count is checked before every
//! request, the run is steered to wrap up at the budget, the free-running turn is stopped at 1.5x so
//! the child hands back what it has, and the run is ended five requests later if it never does.
//! A scout is capped tighter than a worker: it answers a slice, it does not survey.

/// Extra requests the forced report may take after the stop, before the run is ended anyway.
pub const GRACE_REQUESTS: u32 = 5;

/// State of one run's budget. `budget == 0` disables it.
pub struct Budget {
    budget: u32,
    notice: bool,
    requests: u32,
    noticed: bool,
    stopped: bool,
}

/// What the loop does before its next request.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Due {
    Nothing,
    /// The budget is crossed: ask the child to wrap up.
    Notice,
    /// 1.5x: stop the free-running turn and demand the report.
    Stop,
    /// The grace after the stop ran out: end the run with what it has.
    End,
}

impl Budget {
    pub fn new(budget: u32, notice: bool) -> Self {
        Self { budget, notice, requests: 0, noticed: false, stopped: false }
    }

    /// Counts the request the loop is about to send.
    pub fn step(&mut self) -> Due {
        if self.budget == 0 {
            return Due::Nothing;
        }
        self.requests += 1;
        let stop = stop_at(self.budget);
        if self.stopped {
            return if self.requests >= stop + GRACE_REQUESTS { Due::End } else { Due::Nothing };
        }
        if self.requests >= stop {
            self.stopped = true;
            return Due::Stop;
        }
        if self.notice && !self.noticed && self.requests >= self.budget {
            self.noticed = true;
            return Due::Notice;
        }
        Due::Nothing
    }

    /// The message this step earns, if any.
    pub fn message(&self, due: Due) -> Option<String> {
        match due {
            Due::Nothing | Due::End => None,
            Due::Notice => Some(notice_text(self.requests, self.budget)),
            Due::Stop => Some(stop_text(self.requests, self.budget)),
        }
    }

    /// Tool calls are refused once the stop fired: only the report may follow.
    pub fn stopped(&self) -> bool {
        self.stopped
    }
}

/// The request at which the run is stopped: 1.5x the budget, as in omp.
pub fn stop_at(budget: u32) -> u32 {
    budget + budget.div_ceil(2)
}

/// The notice at the budget.
pub fn notice_text(requests: u32, budget: u32) -> String {
    format!(
        "<system-reminder>\n[budget notice] You have used {requests} requests in this run (soft budget: {budget}). Wrap up now: finish the current step and deliver your final result. At {} requests the run is stopped and you will be asked for whatever you have.\n</system-reminder>",
        stop_at(budget)
    )
}

/// The demand at the stop. Its tool calls are refused, so the only way on is the result.
pub fn stop_text(requests: u32, budget: u32) -> String {
    format!(
        "<system-reminder>\n[budget notice] The run is stopped at {requests} requests (soft budget: {budget}); no more tools will run, only the result itself (`yield` where the task has a schema). Write the final result now from what you have, naming anything you did not get to and anything left unverified.\n</system-reminder>"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// omp's thresholds: the notice at the budget, the stop at 1.5x, the end five requests later.
    #[test]
    fn notice_then_stop_then_end() {
        let mut b = Budget::new(100, true);
        let dues: Vec<Due> = (1..=156).map(|_| b.step()).collect();
        assert_eq!(dues[98], Due::Nothing, "request 99 is still under the budget");
        assert_eq!(dues[99], Due::Notice, "the notice lands on the 100th request");
        assert_eq!(dues[148], Due::Nothing);
        assert_eq!(dues[149], Due::Stop, "the stop lands on the 150th request");
        assert!(dues[150..154].iter().all(|d| *d == Due::Nothing), "the forced report has its grace");
        assert!(dues[154..].iter().all(|d| *d == Due::End), "155 requests end the run, and it stays ended");
    }

    #[test]
    fn a_budget_of_zero_or_a_silent_notice() {
        let mut off = Budget::new(0, true);
        assert_eq!((1..500).map(|_| off.step()).filter(|d| *d != Due::Nothing).count(), 0);
        let mut silent = Budget::new(4, false);
        let dues: Vec<Due> = (1..=6).map(|_| silent.step()).collect();
        assert_eq!(dues[5], Due::Stop, "no notice, the stop still comes at 6");
        assert!(!dues[..4].contains(&Due::Notice));
        assert!(silent.stopped());
    }

    #[test]
    fn the_notice_names_the_stop() {
        let n = notice_text(100, 100);
        assert!(n.contains("100 requests") && n.contains("At 150 requests"), "{n}");
        let s = stop_text(150, 100);
        assert!(s.contains("no more tools") && s.contains("150 requests"), "{s}");
    }
}
