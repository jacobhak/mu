//! A subagent's soft request budget, ported from omp: the request count is checked before every
//! request, the run is steered to wrap up at the budget, the free-running turn is stopped at 1.5x so
//! the child hands back what it has, and the run is ended five requests later if it never does.
//! A scout is capped tighter than a worker: it answers a slice, it does not survey.
//! A cost cap works the same way in dollars: the notice at 80 %, the stop at the cap.

/// Extra requests the forced report may take after the stop, before the run is ended anyway.
pub const GRACE_REQUESTS: u32 = 5;

/// What a cost notice fires at, as a share of the cap.
const COST_NOTICE_AT: f64 = 0.8;

/// State of one run's budget. A budget of 0 and a cost cap of 0 disable the respective limit.
pub struct Budget {
    budget: u32,
    cost_cap: f64,
    notice: bool,
    requests: u32,
    noticed: bool,
    stopped_at: Option<u32>,
    why: Why,
    spent: f64,
}

/// Which limit the latest notice or stop came from.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Why {
    Requests,
    Cost,
}

/// What the loop does before its next request.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Due {
    Nothing,
    /// The budget is crossed: ask the child to wrap up.
    Notice,
    /// 1.5x, or the cost cap: stop the free-running turn and demand the report.
    Stop,
    /// The grace after the stop ran out: end the run with what it has.
    End,
}

impl Budget {
    pub fn new(budget: u32, notice: bool, cost_cap: f64) -> Self {
        Self { budget, cost_cap, notice, requests: 0, noticed: false, stopped_at: None, why: Why::Requests, spent: 0.0 }
    }

    /// Counts the request the loop is about to send; `spent` is the run's cost so far in USD.
    pub fn step(&mut self, spent: f64) -> Due {
        if self.budget == 0 && self.cost_cap <= 0.0 {
            return Due::Nothing;
        }
        self.requests += 1;
        self.spent = spent;
        if let Some(at) = self.stopped_at {
            return if self.requests >= at + GRACE_REQUESTS { Due::End } else { Due::Nothing };
        }
        if self.cost_cap > 0.0 && spent >= self.cost_cap {
            self.stopped_at = Some(self.requests);
            self.why = Why::Cost;
            return Due::Stop;
        }
        if self.budget > 0 && self.requests >= stop_at(self.budget) {
            self.stopped_at = Some(self.requests);
            self.why = Why::Requests;
            return Due::Stop;
        }
        if self.notice && !self.noticed {
            if self.cost_cap > 0.0 && spent >= COST_NOTICE_AT * self.cost_cap {
                self.noticed = true;
                self.why = Why::Cost;
                return Due::Notice;
            }
            if self.budget > 0 && self.requests >= self.budget {
                self.noticed = true;
                self.why = Why::Requests;
                return Due::Notice;
            }
        }
        Due::Nothing
    }

    /// The message this step earns, if any.
    pub fn message(&self, due: Due) -> Option<String> {
        match (due, self.why) {
            (Due::Nothing | Due::End, _) => None,
            (Due::Notice, Why::Requests) => Some(notice_text(self.requests, self.budget)),
            (Due::Stop, Why::Requests) => Some(stop_text(self.requests, self.budget)),
            (Due::Notice, Why::Cost) => Some(cost_notice_text(self.spent, self.cost_cap)),
            (Due::Stop, Why::Cost) => Some(cost_stop_text(self.spent, self.cost_cap)),
        }
    }

    /// Tool calls are refused once the stop fired: only the report may follow.
    pub fn stopped(&self) -> bool {
        self.stopped_at.is_some()
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

/// The notice at 80 % of the cost cap.
pub fn cost_notice_text(spent: f64, cap: f64) -> String {
    format!(
        "<system-reminder>\n[budget notice] This run has spent ${spent:.2} of its ${cap:.2} cost limit. Wrap up now: finish the current step and deliver your final result. At ${cap:.2} the run is stopped and you will be asked for whatever you have.\n</system-reminder>"
    )
}

/// The demand at the cost cap.
pub fn cost_stop_text(spent: f64, cap: f64) -> String {
    format!(
        "<system-reminder>\n[budget notice] The run is stopped at ${spent:.2} (cost limit: ${cap:.2}); no more tools will run, only the result itself (`yield` where the task has a schema). Write the final result now from what you have, naming anything you did not get to and anything left unverified.\n</system-reminder>"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// omp's thresholds: the notice at the budget, the stop at 1.5x, the end five requests later.
    #[test]
    fn notice_then_stop_then_end() {
        let mut b = Budget::new(100, true, 0.0);
        let dues: Vec<Due> = (1..=156).map(|_| b.step(0.0)).collect();
        assert_eq!(dues[98], Due::Nothing, "request 99 is still under the budget");
        assert_eq!(dues[99], Due::Notice, "the notice lands on the 100th request");
        assert_eq!(dues[148], Due::Nothing);
        assert_eq!(dues[149], Due::Stop, "the stop lands on the 150th request");
        assert!(dues[150..154].iter().all(|d| *d == Due::Nothing), "the forced report has its grace");
        assert!(dues[154..].iter().all(|d| *d == Due::End), "155 requests end the run, and it stays ended");
    }

    #[test]
    fn a_budget_of_zero_or_a_silent_notice() {
        let mut off = Budget::new(0, true, 0.0);
        assert_eq!((1..500).map(|_| off.step(0.0)).filter(|d| *d != Due::Nothing).count(), 0);
        let mut silent = Budget::new(4, false, 0.0);
        let dues: Vec<Due> = (1..=6).map(|_| silent.step(0.0)).collect();
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

    #[test]
    fn cost_cap_notices_at_80_percent_and_stops_at_the_cap() {
        let mut b = Budget::new(0, true, 1.0);
        assert_eq!(b.step(0.5), Due::Nothing);
        assert_eq!(b.step(0.8), Due::Notice);
        assert!(b.message(Due::Notice).unwrap().contains("$0.80 of its $1.00 cost limit"));
        assert_eq!(b.step(1.0), Due::Stop);
        assert!(b.stopped());
        assert!(b.message(Due::Stop).unwrap().contains("cost limit: $1.00"));
        let later: Vec<Due> = (0..5).map(|_| b.step(1.2)).collect();
        assert_eq!(later[..4], [Due::Nothing; 4]);
        assert_eq!(later[4], Due::End);
    }

    #[test]
    fn the_cost_cap_and_the_request_budget_work_together() {
        let mut b = Budget::new(10, true, 1.0);
        assert_eq!(b.step(1.5), Due::Stop, "the cost cap stops a run well inside its request budget");
        assert!(b.message(Due::Stop).unwrap().contains("cost limit"));
    }
}
