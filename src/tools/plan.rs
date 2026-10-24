use super::{ToolCtx, ToolOutput, Tools};
use crate::events::{AgentEvent, PlanDecision};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::oneshot;

#[derive(Deserialize)]
pub struct Args {
    slug: String,
    title: String,
}

pub fn schema() -> Value {
    json!({"type":"object","properties":{
        "slug":{"type":"string","description":"matches local://<slug>-plan.md; letters, numbers, underscores, hyphens"},
        "title":{"type":"string"}},"required":["slug","title"]})
}

pub const DESCRIPTION: &str = "Submit the finished plan for user approval. The plan file local://<slug>-plan.md must exist. Only available in plan mode.";

impl Tools {
    pub async fn propose_plan(&self, a: Args, ctx: &ToolCtx) -> ToolOutput {
        let uri = format!("local://{}-plan.md", a.slug);
        if a.slug.is_empty() || !a.slug.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
            return ToolOutput::err(format!("Invalid slug `{}`: use letters, numbers, underscores, hyphens", a.slug));
        }
        let content = match self.resolve(&uri).ok().and_then(|r| std::fs::read_to_string(r.path).ok()) {
            Some(c) => c,
            None => return ToolOutput::err(format!("Plan file {uri} not found")),
        };
        let (tx, rx) = oneshot::channel();
        let _ = ctx.events.send(AgentEvent::PlanReview { title: a.title, path: uri, reply: tx });
        let decision = tokio::select! {
            d = rx => d.unwrap_or(PlanDecision::Refine(String::new())),
            _ = ctx.cancel.cancelled() => return ToolOutput::err("Plan review cancelled"),
        };
        match decision {
            PlanDecision::Execute => {
                let mut o = ToolOutput::ok("Plan approved. Executing in a fresh session.");
                o.details = Some(json!({"planDecision": "execute", "content": content}));
                o
            }
            PlanDecision::Keep => {
                let mut o = ToolOutput::ok("Plan approved. Implement it now.");
                o.details = Some(json!({"planDecision": "keep"}));
                o
            }
            PlanDecision::Refine(t) => ToolOutput::ok(format!("User requested changes:\n{t}")),
        }
    }
}
