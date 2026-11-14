use super::Tools;
use mu_base::tool::ToolOutput;
use serde::Deserialize;
use serde_json::{Value, json};

pub const DESCRIPTION: &str = "Ask the user to switch this session to plan mode. Use it when the user asks for a plan, or before a large or risky change that should be planned first. The user approves each call. Not available once plan mode is on.";

#[derive(Deserialize)]
pub struct Args {
    #[allow(dead_code)]
    reason: String,
}

pub fn schema() -> Value {
    json!({"type":"object","properties":{
        "reason":{"type":"string","description":"One sentence on why a plan is needed; the user reads it in the approval prompt"}},"required":["reason"]})
}

impl Tools {
    /// Changes nothing itself: the agent enters plan mode when it sees `enterPlan` in the details,
    /// the way it acts on a `propose_plan` decision.
    pub fn enter_plan(&self, _a: Args) -> ToolOutput {
        let mut o = ToolOutput::ok("Plan mode is on. Follow the plan-mode instructions in the system prompt.");
        o.details = Some(json!({"enterPlan": true}));
        o
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hands_the_switch_to_the_agent() {
        let out = Tools::for_test().enter_plan(serde_json::from_value(json!({"reason": "big change"})).unwrap());
        assert!(!out.is_error);
        assert_eq!(out.details, Some(json!({"enterPlan": true})));
    }
}
