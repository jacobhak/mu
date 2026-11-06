use super::Tools;
use mu_base::events::SubagentState;
use mu_base::tool::ToolOutput;
use serde::Deserialize;
use serde_json::{Value, json};

pub const DESCRIPTION: &str = "Cancel a background job (a `task` subagent or a `bash` command run with `async`) by its id, or every running job with `all`. Stops the job; its result arrives as a system notice, like any finished job. Use this, not `kill`, to stop one. Cancelling a job that already finished does nothing.";

#[derive(Deserialize)]
pub struct Args {
    id: Option<String>,
    #[serde(default)]
    all: bool,
}

pub fn schema() -> Value {
    json!({"type":"object","properties":{
        "id":{"type":"string","description":"The job id, as returned by `task` or `bash` with `async`"},
        "all":{"type":"boolean","description":"Cancel every running job instead of one"}}})
}

impl Tools {
    pub fn job_cancel(&self, a: Args) -> ToolOutput {
        let Some(jobs) = &self.jobs else {
            return ToolOutput::err("job_cancel: this session has no background jobs");
        };
        let running = jobs.running_ids();
        match (a.id, a.all) {
            (Some(_), true) => ToolOutput::err("job_cancel: give `id` or `all`, not both"),
            (None, false) => ToolOutput::err("job_cancel: give the job's `id`, or `all: true`"),
            (None, true) if running.is_empty() => ToolOutput::ok("No jobs are running."),
            (None, true) => {
                jobs.kill(None);
                ToolOutput::ok(format!("Cancelled {} job(s): {}.", running.len(), running.join(", ")))
            }
            (Some(id), false) => match jobs.state_of(&id) {
                None if running.is_empty() => ToolOutput::err(format!("job_cancel: no job `{id}`; none are running")),
                None => ToolOutput::err(format!("job_cancel: no job `{id}`; running: {}", running.join(", "))),
                Some(SubagentState::Running) => {
                    jobs.kill(Some(&id));
                    ToolOutput::ok(format!("Cancelled job {id}."))
                }
                Some(state) => {
                    let state = if state == SubagentState::Done { "done" } else { "failed" };
                    ToolOutput::ok(format!("Job {id} had already finished ({state}); nothing to cancel."))
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(t: &Tools, args: Value) -> ToolOutput {
        t.job_cancel(serde_json::from_value(args).unwrap())
    }

    fn with_jobs() -> Tools {
        let mut t = Tools::for_test();
        let (wake, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (events, _erx) = tokio::sync::mpsc::unbounded_channel();
        t.jobs = Some(crate::jobs::JobsHandle::new(wake, events));
        t
    }

    #[test]
    fn without_jobs_it_is_an_error() {
        let out = call(&Tools::for_test(), json!({"id": "x"}));
        assert!(out.is_error && out.text.contains("no background jobs"), "{}", out.text);
    }

    #[test]
    fn needs_exactly_one_of_id_and_all() {
        let t = with_jobs();
        assert!(call(&t, json!({})).is_error);
        assert!(call(&t, json!({"id": "a", "all": true})).is_error);
    }

    #[test]
    fn cancels_a_running_job_by_id_and_names_the_running_ones_when_it_is_unknown() {
        let t = with_jobs();
        let jobs = t.jobs.clone().unwrap();
        jobs.push_running("a");
        jobs.push_running("b");
        let out = call(&t, json!({"id": "nope"}));
        assert!(out.is_error && out.text.contains("running: a, b"), "{}", out.text);
        let out = call(&t, json!({"id": "a"}));
        assert!(!out.is_error && out.text.contains("Cancelled job a"), "{}", out.text);
        assert!(jobs.is_cancelled("a") && !jobs.is_cancelled("b"));
    }

    #[test]
    fn all_cancels_every_running_job() {
        let t = with_jobs();
        let jobs = t.jobs.clone().unwrap();
        assert_eq!(call(&t, json!({"all": true})).text, "No jobs are running.");
        jobs.push_running("a");
        jobs.push_running("b");
        let out = call(&t, json!({"all": true}));
        assert!(out.text.contains("Cancelled 2 job(s): a, b"), "{}", out.text);
        assert!(jobs.is_cancelled("a") && jobs.is_cancelled("b"));
    }

    #[test]
    fn a_finished_job_is_left_alone() {
        let t = with_jobs();
        let jobs = t.jobs.clone().unwrap();
        jobs.push_running("a");
        jobs.finish("a", true, "x");
        let out = call(&t, json!({"id": "a"}));
        assert!(!out.is_error && out.text.contains("already finished (done)"), "{}", out.text);
        assert!(!jobs.is_cancelled("a"));
    }
}
