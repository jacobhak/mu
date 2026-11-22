use super::Tools;
use crate::context::capitalize;
use mu_base::tool::{ToolDef, ToolOutput};
use serde::Deserialize;
use serde_json::json;

#[derive(Deserialize)]
pub struct Args {
    mode: String,
    #[allow(dead_code)]
    reason: String,
}

/// `modes`: name and description of each mode the model may ask for.
pub fn def(modes: &[(String, String)]) -> ToolDef {
    let list: Vec<String> = modes.iter().map(|(n, d)| format!("- {n}: {d}")).collect();
    let names: Vec<&str> = modes.iter().map(|(n, _)| n.as_str()).collect();
    ToolDef {
        name: "enter_mode".into(),
        description: format!(
            "Ask to switch this session to a mode:\n{}\nThe user may have to approve each call. Not available once a mode is on.",
            list.join("\n")
        ),
        schema: json!({"type":"object","properties":{
            "mode":{"type":"string","enum":names},
            "reason":{"type":"string","description":"One sentence on why the mode fits; the user reads it in the approval prompt"}},
            "required":["mode","reason"]}),
    }
}

impl Tools {
    /// Changes nothing itself: the agent enters the mode when it sees `enterMode` in the details,
    /// the way it acts on an exit tool's decision.
    pub fn enter_mode(&self, a: Args) -> ToolOutput {
        if !self.mode_entry.borrow().iter().any(|(n, ..)| *n == a.mode) {
            return ToolOutput::err(format!("unknown mode {}", a.mode));
        }
        let mut o = ToolOutput::ok(format!(
            "{} mode is on. Follow the {}-mode instructions in the system prompt.",
            capitalize(&a.mode),
            a.mode
        ));
        o.details = Some(json!({"enterMode": a.mode}));
        o
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::ModelEntry;

    fn tools() -> Tools {
        let t = Tools::for_test();
        t.mode_entry.borrow_mut().push(("plan".into(), ModelEntry::Ask, "d".into(), true));
        t
    }

    #[test]
    fn hands_the_switch_to_the_agent() {
        let out = tools().enter_mode(serde_json::from_value(json!({"mode": "plan", "reason": "big change"})).unwrap());
        assert!(!out.is_error);
        assert_eq!(out.details, Some(json!({"enterMode": "plan"})));
        assert!(out.text.starts_with("Plan mode is on."));
    }

    #[test]
    fn an_unknown_mode_is_an_error() {
        let out = tools().enter_mode(serde_json::from_value(json!({"mode": "ghost", "reason": "x"})).unwrap());
        assert!(out.is_error && out.text == "unknown mode ghost");
    }

    #[test]
    fn the_schema_lists_the_modes() {
        let s = def(&[("plan".into(), "d".into())]).schema;
        assert_eq!(s["properties"]["mode"]["enum"], json!(["plan"]));
    }
}
