//! `job_send`: message a running subagent. Offered where background jobs exist.

use super::Tools;
use crate::jobs::Sent;
use mu_base::tool::ToolOutput;
use serde::Deserialize;
use serde_json::{Value, json};

pub const DESCRIPTION: &str = "Send a message to a running subagent started with `task`. It reads the message at its next tool boundary, like a user steering a turn. `urgent` also cancels the tool call it is running so it reads the message now. A queued subagent gets the message added to its task. A finished subagent cannot be messaged: continue it with `task` and `resume`.";

#[derive(Deserialize)]
pub struct Args {
    id: String,
    message: String,
    #[serde(default)]
    urgent: bool,
}

pub fn schema() -> Value {
    json!({"type":"object","properties":{
        "id":{"type":"string","description":"The subagent job's id"},
        "message":{"type":"string"},
        "urgent":{"type":"boolean","description":"Cancel the tool call it is running, so it reads the message now"}},
        "required":["id","message"]})
}

impl Tools {
    pub fn job_send(&self, a: Args) -> ToolOutput {
        let Some(jobs) = &self.jobs else {
            return ToolOutput::err("job_send: this session has no background jobs");
        };
        if a.message.trim().is_empty() {
            return ToolOutput::err("job_send: message is empty");
        }
        let id = &a.id;
        match jobs.send(id, &a.message, a.urgent) {
            Ok(Sent::Live) if a.urgent => ToolOutput::ok(format!(
                "Sent to {id} as urgent: its running tool call is cancelled and it reads the message now."
            )),
            Ok(Sent::Live) => ToolOutput::ok(format!("Sent to {id}; it reads the message at its next tool boundary.")),
            Ok(Sent::Queued) => ToolOutput::ok(format!("{id} has not started yet; the message was added to its task.")),
            Err(e) => ToolOutput::err(format!("job_send: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::JobsHandle;
    use mu_base::events::AgentEvent;

    fn with_jobs() -> (Tools, std::rc::Rc<JobsHandle>) {
        let mut t = Tools::for_test();
        let (wake, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (events, _erx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
        let jobs = JobsHandle::new(wake, events);
        t.jobs = Some(jobs.clone());
        (t, jobs)
    }

    fn call(t: &Tools, args: Value) -> ToolOutput {
        t.job_send(serde_json::from_value(args).unwrap())
    }

    #[test]
    fn it_needs_jobs_a_message_and_a_known_subagent() {
        let out = call(&Tools::for_test(), json!({"id": "a", "message": "hi"}));
        assert!(out.is_error && out.text.contains("no background jobs"), "{}", out.text);
        let (t, jobs) = with_jobs();
        assert_eq!(call(&t, json!({"id": "a", "message": " "})).text, "job_send: message is empty");
        assert_eq!(call(&t, json!({"id": "a", "message": "hi"})).text, "job_send: no job `a`");
        jobs.push_running("a");
        jobs.finish("a", true, "x");
        let out = call(&t, json!({"id": "a", "message": "hi"}));
        assert!(out.is_error && out.text.contains("has finished; continue it with `task` and `resume: \"a\"`"));
    }

    #[test]
    fn a_running_subagent_gets_the_message_and_urgent_says_so() {
        let (t, jobs) = with_jobs();
        let mut lines = jobs.push_steerable("a");
        let out = call(&t, json!({"id": "a", "message": "look at b.rs"}));
        assert_eq!(out.text, "Sent to a; it reads the message at its next tool boundary.");
        let out = call(&t, json!({"id": "a", "message": "stop", "urgent": true}));
        assert!(out.text.starts_with("Sent to a as urgent:"), "{}", out.text);
        let first: Value = serde_json::from_str(&lines.try_recv().unwrap()).unwrap();
        assert_eq!(first["type"], "steer");
        assert_eq!(first["urgent"], false);
        assert!(first["text"].as_str().unwrap().ends_with("look at b.rs"));
        let second: Value = serde_json::from_str(&lines.try_recv().unwrap()).unwrap();
        assert_eq!(second["urgent"], true);
    }
}
