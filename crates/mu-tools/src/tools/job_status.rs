//! `job_status`: what this session's background jobs are doing. Offered where background jobs exist.

use super::Tools;
use mu_base::tool::ToolOutput;
use serde::Deserialize;
use serde_json::{Value, json};

pub const DESCRIPTION: &str = "Show this session's background jobs: state, run time, tool calls, cost and last activity. With `id`, one job in detail: its task or command, recent tool calls of a subagent, or the last `tail` output lines of a bash job (default 40, at most 400).";

const DEFAULT_TAIL: usize = 40;
const MAX_TAIL: usize = 400;

#[derive(Deserialize)]
pub struct Args {
    id: Option<String>,
    tail: Option<usize>,
}

pub fn schema() -> Value {
    json!({"type":"object","properties":{
        "id":{"type":"string","description":"A job id; omit to list every job"},
        "tail":{"type":"integer","minimum":1,"maximum":MAX_TAIL,"description":"With `id` on a bash job: how many of its last output lines (default 40)"}}})
}

impl Tools {
    pub fn job_status(&self, a: Args) -> ToolOutput {
        let Some(jobs) = &self.jobs else {
            return ToolOutput::err("job_status: this session has no background jobs");
        };
        match jobs.status(a.id.as_deref(), a.tail.unwrap_or(DEFAULT_TAIL).clamp(1, MAX_TAIL)) {
            Ok(text) => ToolOutput::ok(text),
            Err(e) => ToolOutput::err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::JobsHandle;
    use mu_base::events::AgentEvent;
    use std::rc::Rc;

    fn with_jobs() -> (Tools, Rc<JobsHandle>) {
        let mut t = Tools::for_test();
        let (wake, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (events, _erx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
        let jobs = JobsHandle::new(wake, events);
        t.jobs = Some(jobs.clone());
        (t, jobs)
    }

    fn call(t: &Tools, args: Value) -> ToolOutput {
        t.job_status(serde_json::from_value(args).unwrap())
    }

    #[test]
    fn without_jobs_it_is_an_error() {
        let out = call(&Tools::for_test(), json!({}));
        assert!(out.is_error && out.text.contains("no background jobs"), "{}", out.text);
    }

    #[test]
    fn an_empty_session_says_so_and_an_unknown_id_lists_the_jobs() {
        let (t, jobs) = with_jobs();
        assert_eq!(call(&t, json!({})).text, "No background jobs in this session.");
        jobs.push_running("a");
        jobs.push_running("b");
        let out = call(&t, json!({"id": "zz"}));
        assert!(out.is_error && out.text == "job_status: no job `zz`; jobs: a, b", "{}", out.text);
    }

    #[test]
    fn a_subagent_is_listed_and_shown_with_its_recent_tool_calls() {
        let (t, jobs) = with_jobs();
        jobs.push_running("a");
        jobs.finish("a", true, "x");
        let list = call(&t, json!({})).text;
        assert_eq!(list.lines().count(), 1);
        assert!(list.starts_with("a · scout · done · "), "{list}");
        assert!(list.contains(" · 0 tools") && list.ends_with(" · result waiting"), "{list}");
        let detail = call(&t, json!({"id": "a"})).text;
        assert!(detail.starts_with("Job a (scout) — done, "), "{detail}");
        assert!(detail.contains("\nresult: waiting (use `wait`)"), "{detail}");
        assert!(detail.ends_with("Recent tool calls:\n- none yet"), "{detail}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_bash_job_shows_its_last_output_lines_and_ports() {
        let (t, jobs) = with_jobs();
        let sink = tokio::task::LocalSet::new()
            .run_until(async {
                let (id, sink) = jobs.spawn_bash("serve".into(), "/".into(), |cancel, _| async move {
                    cancel.cancelled().await;
                    crate::tools::subagent::BackgroundResult {
                        ok: false,
                        text: String::new(),
                        note: "cancelled".into(),
                        meta: None,
                    }
                });
                assert_eq!(id, "bg_1");
                sink.lines("one\ntwo\nthree\n");
                let list = call(&t, json!({})).text;
                assert!(list.starts_with("bg_1 · bash · running · "), "{list}");
                let detail = call(&t, json!({"id": "bg_1", "tail": 2})).text;
                assert!(detail.contains("command: serve"), "{detail}");
                assert!(detail.ends_with("Output (last 2 lines):\ntwo\nthree"), "{detail}");
                jobs.kill(None);
                sink
            })
            .await;
        drop(sink);
    }
}
