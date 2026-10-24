use super::{ToolCtx, ToolOutput, Tools};
use crate::jobs::{WaitResult, result_text};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Deserialize)]
pub struct Args {
    #[serde(default)]
    ids: Vec<String>,
}

pub const DESCRIPTION: &str = "Block until a background job from `task` finishes. Use only when blocked with nothing else to do; results otherwise arrive automatically. Returns at once when no matching job is running.";

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
