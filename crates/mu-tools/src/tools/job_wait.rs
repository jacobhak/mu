//! `wait`: block on background jobs until one finishes, the run is cancelled, or the user sends a
//! new prompt. Offered where background jobs exist (interactive sessions).

use super::{ToolCtx, Tools};
use crate::jobs::WaitReady;
use mu_base::tool::{ToolDef, ToolOutput};
use serde::Deserialize;
use serde_json::{Value, json};

pub const DESCRIPTION: &str = "Wait for background jobs: the named ids, or any job when `ids` is omitted. Returns as soon as one of them finishes (or prints a line matching its `notify` pattern), and the results follow in a system notice. A new prompt from the user stops the wait; the jobs keep running.";

#[derive(Deserialize)]
pub struct Args {
    #[serde(default)]
    ids: Vec<String>,
}

pub fn schema() -> Value {
    json!({"type":"object","properties":{"ids":{"type":"array","items":{"type":"string"},"description":"Job ids to wait for; omitted or empty means any job."}}})
}

impl Tools {
    pub fn wait_def() -> ToolDef {
        ToolDef { name: "wait".into(), description: DESCRIPTION.into(), schema: schema() }
    }

    pub async fn job_wait(&self, a: Args, ctx: &ToolCtx) -> ToolOutput {
        let Some(jobs) = &self.jobs else {
            return ToolOutput::err("wait: unavailable without background jobs");
        };
        let Some(interrupt) = &ctx.wait_interrupt else {
            return ToolOutput::err("wait: no active interruptible wait");
        };
        match jobs.wait_ready_or_matches(&a.ids, &ctx.cancel, interrupt).await {
            WaitReady::Ready(ids) => {
                ToolOutput::ok(format!("Finished jobs: {}. Their results follow in a system notice.", ids.join(", ")))
            }
            WaitReady::Matched(ids) => ToolOutput::ok(format!(
                "Output matching `notify` from jobs: {}. The matching lines follow in a system notice; the jobs keep running.",
                ids.join(", ")
            )),
            WaitReady::Nothing => ToolOutput::ok("No matching background jobs running."),
            WaitReady::Interrupted => ToolOutput::ok(
                "Waiting stopped for your new prompt. Jobs continue running; their results will arrive automatically.",
            ),
            WaitReady::Cancelled => ToolOutput::err("Wait cancelled"),
            WaitReady::Unknown(ids) => ToolOutput::err(format!("wait: unknown job ids: {}", ids.join(", "))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::JobsHandle;
    use crate::tools::summary;
    use mu_base::events::AgentEvent;
    use tokio_util::sync::CancellationToken;

    fn tools() -> (Tools, std::rc::Rc<JobsHandle>) {
        let mut t = Tools::for_test();
        let (wake, _wake_rx) = tokio::sync::mpsc::unbounded_channel();
        let (events, _events_rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
        let jobs = JobsHandle::new(wake, events);
        t.jobs = Some(jobs.clone());
        (t, jobs)
    }

    fn ctx(jobs: &std::rc::Rc<JobsHandle>, cancel: CancellationToken) -> (ToolCtx, crate::jobs::ActiveWait) {
        let (events, _rx) = tokio::sync::mpsc::unbounded_channel();
        let wait = jobs.begin_wait();
        (ToolCtx { events, cancel, call_id: "c".into(), mode: None, wait_interrupt: Some(wait.token()) }, wait)
    }

    #[tokio::test]
    async fn needs_jobs_and_an_active_wait() {
        let plain_tools = Tools::for_test();
        let (events, _rx) = tokio::sync::mpsc::unbounded_channel();
        let no_jobs =
            ToolCtx { events, cancel: CancellationToken::new(), call_id: "c".into(), mode: None, wait_interrupt: None };
        let out = plain_tools.job_wait(Args { ids: vec![] }, &no_jobs).await;
        assert!(out.is_error && out.text.contains("unavailable without background jobs"), "{}", out.text);

        let (t, _jobs) = tools();
        let (events, _rx) = tokio::sync::mpsc::unbounded_channel();
        let plain =
            ToolCtx { events, cancel: CancellationToken::new(), call_id: "c".into(), mode: None, wait_interrupt: None };
        let out = t.job_wait(Args { ids: vec![] }, &plain).await;
        assert!(out.is_error && out.text.contains("no active interruptible wait"), "{}", out.text);
    }

    #[tokio::test]
    async fn a_finished_job_is_named_and_left_for_the_notice() {
        let (t, jobs) = tools();
        jobs.push_running("A");
        jobs.finish("A", true, "ALPHA");
        let (c, _wait) = ctx(&jobs, CancellationToken::new());
        let out = t.job_wait(Args { ids: vec![] }, &c).await;
        assert!(
            !out.is_error && out.text == "Finished jobs: A. Their results follow in a system notice.",
            "{}",
            out.text
        );
        assert!(!out.text.contains("ALPHA"));
        assert_eq!(jobs.take_undelivered()[0].text, "ALPHA", "the result is not consumed by the wait");
    }

    #[tokio::test]
    async fn no_jobs_and_unknown_ids_report_plainly() {
        let (t, jobs) = tools();
        let (c, _wait) = ctx(&jobs, CancellationToken::new());
        let out = t.job_wait(Args { ids: vec![] }, &c).await;
        assert!(!out.is_error && out.text == "No matching background jobs running.", "{}", out.text);
        let out = t.job_wait(Args { ids: vec!["nope".into(), "nope2".into()] }, &c).await;
        assert!(out.is_error && out.text == "wait: unknown job ids: nope, nope2", "{}", out.text);
    }

    #[tokio::test]
    async fn input_releases_the_wait_and_cancel_stops_it() {
        let (t, jobs) = tools();
        jobs.push_running("A");
        let (c, wait) = ctx(&jobs, CancellationToken::new());
        wait.token().cancel();
        let out = t.job_wait(Args { ids: vec![] }, &c).await;
        assert!(!out.is_error && out.text.starts_with("Waiting stopped for your new prompt."), "{}", out.text);
        assert_eq!(jobs.state_of("A"), Some(mu_base::events::SubagentState::Running), "the job survives");

        let cancel = CancellationToken::new();
        cancel.cancel();
        let (c, _wait) = ctx(&jobs, cancel);
        let out = t.job_wait(Args { ids: vec![] }, &c).await;
        assert!(out.is_error && out.text == "Wait cancelled", "{}", out.text);
        assert_eq!(jobs.state_of("A"), Some(mu_base::events::SubagentState::Running));
    }

    #[test]
    fn summary_names_the_ids_or_any() {
        assert_eq!(summary("wait", &serde_json::json!({})), "any");
        assert_eq!(summary("wait", &serde_json::json!({"ids": ["a", "b"]})), "a, b");
    }
}
