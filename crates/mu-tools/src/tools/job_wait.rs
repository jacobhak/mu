//! `wait`: block on background jobs until one finishes (or all do, or a bash job prints a line), the
//! time is up, the run is cancelled, or the user sends a new prompt. Offered where background jobs
//! exist (interactive sessions).

use super::{ToolCtx, Tools};
use crate::jobs::{self, JobsHandle, WaitReady, WaitSpec};
use mu_base::tool::{ToolDef, ToolOutput};
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Duration;

pub const DESCRIPTION: &str = "Wait for background jobs: the named ids, or any job when `ids` is omitted. Returns as soon as one of them finishes (all of them with `all`), with the finished jobs' results, so no notice follows for them. `until` returns when a named bash job prints a matching line; `timeout` bounds the wait. A new prompt from the user stops the wait; the jobs keep running.";

/// Longest `timeout`, in seconds.
const MAX_TIMEOUT: u64 = 3600;

#[derive(Deserialize)]
pub struct Args {
    #[serde(default)]
    ids: Vec<String>,
    #[serde(default)]
    all: bool,
    timeout: Option<u64>,
    until: Option<String>,
}

pub fn schema() -> Value {
    json!({"type":"object","properties":{
        "ids":{"type":"array","items":{"type":"string"},"description":"Job ids to wait for; omitted or empty means any job."},
        "all":{"type":"boolean","description":"Wait until every named job (every job when `ids` is empty) has finished, instead of the first"},
        "timeout":{"type":"integer","minimum":1,"maximum":MAX_TIMEOUT,"description":"Give up after this many seconds (1-3600); finished results are still returned"},
        "until":{"type":"string","description":"A regex (Rust syntax): return as soon as an output line of the named `bash` jobs matches, including output printed before the call; the jobs keep running"}}})
}

/// The results of the finished jobs `ids`, taken: they need no notice.
fn finished_text(jobs: &JobsHandle, ids: &[String]) -> String {
    let done = jobs.take_undelivered_for(ids);
    let body: Vec<String> = done.iter().map(jobs::result_text).collect();
    format!("Finished: {}.\n\n{}", ids.join(", "), body.join("\n\n"))
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
        if a.timeout.is_some_and(|t| !(1..=MAX_TIMEOUT).contains(&t)) {
            return ToolOutput::err("wait: timeout must be 1-3600 seconds");
        }
        let until = match a.until.as_deref() {
            None => None,
            Some(_) if a.ids.is_empty() => {
                return ToolOutput::err("wait: `until` needs `ids` naming `bash` jobs");
            }
            Some(pattern) => {
                if a.ids.iter().any(|id| jobs.is_subagent(id)) {
                    return ToolOutput::err("wait: `until` needs `ids` naming `bash` jobs");
                }
                match regex::Regex::new(pattern) {
                    Ok(re) => Some(re),
                    Err(e) => return ToolOutput::err(format!("wait: until: invalid regex: {e}")),
                }
            }
        };
        let spec = WaitSpec {
            ids: &a.ids,
            all: a.all,
            until: until.as_ref(),
            deadline: a.timeout.map(|t| tokio::time::Instant::now() + Duration::from_secs(t)),
            on_match: true,
        };
        match jobs.wait_for(spec, &ctx.cancel, interrupt).await {
            WaitReady::Ready(ids) => ToolOutput::ok(finished_text(jobs, &ids)),
            WaitReady::TimedOut { finished, running } => {
                let secs = a.timeout.unwrap_or_default();
                let running = running.join(", ");
                if finished.is_empty() {
                    ToolOutput::ok(format!("Timed out after {secs}s; still running: {running}. Nothing finished."))
                } else {
                    ToolOutput::ok(format!(
                        "Timed out after {secs}s; still running: {running}.\n\n{}",
                        finished_text(jobs, &finished)
                    ))
                }
            }
            WaitReady::Matched(ids) => {
                let m = jobs.take_matches_for(&ids);
                ToolOutput::ok(format!("{}\nThe jobs keep running.", jobs::matches_body(&m)))
            }
            WaitReady::Until(m) => ToolOutput::ok(format!("{}\nThe jobs keep running.", jobs::matches_body(&m))),
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

    fn args(v: Value) -> Args {
        serde_json::from_value(v).unwrap()
    }

    #[tokio::test]
    async fn needs_jobs_and_an_active_wait() {
        let plain_tools = Tools::for_test();
        let (events, _rx) = tokio::sync::mpsc::unbounded_channel();
        let no_jobs =
            ToolCtx { events, cancel: CancellationToken::new(), call_id: "c".into(), mode: None, wait_interrupt: None };
        let out = plain_tools.job_wait(args(json!({})), &no_jobs).await;
        assert!(out.is_error && out.text.contains("unavailable without background jobs"), "{}", out.text);

        let (t, _jobs) = tools();
        let (events, _rx) = tokio::sync::mpsc::unbounded_channel();
        let plain =
            ToolCtx { events, cancel: CancellationToken::new(), call_id: "c".into(), mode: None, wait_interrupt: None };
        let out = t.job_wait(args(json!({})), &plain).await;
        assert!(out.is_error && out.text.contains("no active interruptible wait"), "{}", out.text);
    }

    #[tokio::test]
    async fn a_finished_job_is_returned_inline() {
        let (t, jobs) = tools();
        jobs.push_running("A");
        jobs.finish("A", true, "ALPHA");
        let (c, _wait) = ctx(&jobs, CancellationToken::new());
        let out = t.job_wait(args(json!({})), &c).await;
        assert!(!out.is_error && out.text.starts_with("Finished: A."), "{}", out.text);
        assert!(out.text.contains("## A (scout) — ok\nALPHA"), "{}", out.text);
        assert!(jobs.take_undelivered().is_empty(), "no notice follows for it");
    }

    #[tokio::test]
    async fn no_jobs_and_unknown_ids_report_plainly() {
        let (t, jobs) = tools();
        let (c, _wait) = ctx(&jobs, CancellationToken::new());
        let out = t.job_wait(args(json!({})), &c).await;
        assert!(!out.is_error && out.text == "No matching background jobs running.", "{}", out.text);
        let out = t.job_wait(args(json!({"ids": ["nope", "nope2"]})), &c).await;
        assert!(out.is_error && out.text == "wait: unknown job ids: nope, nope2", "{}", out.text);
    }

    #[tokio::test]
    async fn input_releases_the_wait_and_cancel_stops_it() {
        let (t, jobs) = tools();
        jobs.push_running("A");
        let (c, wait) = ctx(&jobs, CancellationToken::new());
        wait.token().cancel();
        let out = t.job_wait(args(json!({})), &c).await;
        assert!(!out.is_error && out.text.starts_with("Waiting stopped for your new prompt."), "{}", out.text);
        assert_eq!(jobs.state_of("A"), Some(mu_base::events::SubagentState::Running), "the job survives");

        let cancel = CancellationToken::new();
        cancel.cancel();
        let (c, _wait) = ctx(&jobs, cancel);
        let out = t.job_wait(args(json!({})), &c).await;
        assert!(out.is_error && out.text == "Wait cancelled", "{}", out.text);
        assert_eq!(jobs.state_of("A"), Some(mu_base::events::SubagentState::Running));
    }

    #[tokio::test]
    async fn the_arguments_are_checked() {
        let (t, jobs) = tools();
        jobs.push_running("A");
        let (c, _wait) = ctx(&jobs, CancellationToken::new());
        for bad in [0, 3601] {
            let out = t.job_wait(args(json!({"timeout": bad})), &c).await;
            assert!(out.is_error && out.text == "wait: timeout must be 1-3600 seconds", "{}", out.text);
        }
        let out = t.job_wait(args(json!({"until": "x"})), &c).await;
        assert!(out.is_error && out.text.contains("`until` needs `ids` naming `bash` jobs"), "{}", out.text);
        let out = t.job_wait(args(json!({"until": "x", "ids": ["A"]})), &c).await;
        assert!(out.is_error && out.text.contains("`until` needs `ids` naming `bash` jobs"), "{}", out.text);
    }

    #[tokio::test]
    async fn a_bad_regex_is_reported() {
        let (t, jobs) = tools();
        let (c, _wait) = ctx(&jobs, CancellationToken::new());
        let out = t.job_wait(args(json!({"until": "(", "ids": ["bg_1"]})), &c).await;
        assert!(out.is_error && out.text.starts_with("wait: until: invalid regex"), "{}", out.text);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_timeout_returns_what_finished_and_names_what_runs() {
        let (t, jobs) = tools();
        jobs.push_running("A");
        let (c, _wait) = ctx(&jobs, CancellationToken::new());
        let out = t.job_wait(args(json!({"timeout": 1})), &c).await;
        assert_eq!(out.text, "Timed out after 1s; still running: A. Nothing finished.");
        jobs.push_running("B");
        jobs.finish("B", true, "BETA");
        let out = t.job_wait(args(json!({"timeout": 1, "all": true})), &c).await;
        assert!(out.text.starts_with("Timed out after 1s; still running: A.\n\nFinished: B."), "{}", out.text);
        assert!(out.text.contains("## B (scout) — ok\nBETA"), "{}", out.text);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn all_waits_for_every_job() {
        let (t, jobs) = tools();
        jobs.push_running("A");
        jobs.push_running("B");
        let (c, _wait) = ctx(&jobs, CancellationToken::new());
        jobs.finish("A", true, "a");
        let waiting = t.job_wait(args(json!({"all": true})), &c);
        let finish = async {
            tokio::task::yield_now().await;
            jobs.finish("B", true, "b");
        };
        let (out, ()) = tokio::join!(waiting, finish);
        assert!(out.text.starts_with("Finished: A, B.") && out.text.contains("## B (scout) — ok\nb"), "{}", out.text);
    }

    #[test]
    fn summary_names_the_ids_or_any() {
        assert_eq!(summary("wait", &serde_json::json!({})), "any");
        assert_eq!(summary("wait", &serde_json::json!({"ids": ["a", "b"]})), "a, b");
    }
}
