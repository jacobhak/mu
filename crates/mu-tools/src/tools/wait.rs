use super::{ToolCtx, Tools};
use crate::jobs::{WaitResult, result_text};
use mu_base::tool::ToolOutput;
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Deserialize)]
pub struct Args {
    #[serde(default)]
    ids: Vec<String>,
}

pub const DESCRIPTION: &str = "Block until a background job from `task` or `bash` (`async: true`) finishes. Results arrive automatically without it, so end your turn instead of waiting; never poll with it. Returns at once when no matching job is running.";

pub fn schema() -> Value {
    json!({"type":"object","properties":{
        "ids":{"type":"array","items":{"type":"string"},"description":"job ids; default any"}}})
}

impl Tools {
    pub async fn wait(&self, a: Args, ctx: &ToolCtx) -> ToolOutput {
        let Some(jobs) = &self.jobs else { return ToolOutput::err("wait: background jobs are not enabled") };
        match jobs.wait(&a.ids, &ctx.cancel).await {
            WaitResult::Done(done) => ToolOutput::ok(done.iter().map(result_text).collect::<Vec<_>>().join("\n\n")),
            WaitResult::Nothing => ToolOutput::ok("No background jobs running."),
            WaitResult::Cancelled => ToolOutput::err("Wait cancelled"),
            WaitResult::Timeout(ids) => ToolOutput::ok(format!("Still running: {}", ids.join(", "))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn wait_returns_a_finished_bash_job() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let mut tools = Tools::for_test();
                tools.jobs = Some(crate::jobs::tests::handle());
                let (events, _rx) = tokio::sync::mpsc::unbounded_channel();
                let ctx = ToolCtx { events, cancel: Default::default(), call_id: "c".into(), plan: false };
                tools.call("bash", &json!({"command": "echo w", "async": true}), &ctx).await;
                let out = tools.call("wait", &json!({}), &ctx).await;
                assert!(out.text.contains("## bg_1 (bash: echo w) — ok, exit 0\nw"), "{}", out.text);
            })
            .await;
    }
}
