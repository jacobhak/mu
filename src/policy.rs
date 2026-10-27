//! One place that decides which tools a run may use. `Agent::tool_defs` (what the model is told)
//! and `Agent::run_tool` (what is executed) both ask it, so the two cannot drift apart.
//!
//! Bash is deliberately allowed in plan mode, as in omp: the plan-mode prompt tells the model
//! not to change anything, but nothing sandboxes the shell.

#[derive(Debug, Clone, Default)]
pub struct ToolPolicy {
    /// When set, only these tools may run.
    pub allow: Option<Vec<String>>,
    /// Tools that never run (e.g. `ask` with no user to answer).
    pub excluded: Vec<&'static str>,
    /// Subagent nesting depth of this process.
    pub depth: u32,
    /// Plan mode is active.
    pub plan: bool,
}

/// Subagents may not nest deeper than this.
pub const MAX_DEPTH: u32 = 2;

impl ToolPolicy {
    pub fn check(&self, name: &str) -> Result<(), String> {
        if self.excluded.contains(&name) {
            return Err(format!("Tool `{name}` is not available in this run."));
        }
        if let Some(allow) = &self.allow
            && name != "yield" // how a subagent with an output schema returns its result
            && !allow.iter().any(|a| a == name)
        {
            return Err(format!("Tool `{name}` is not allowed for this agent."));
        }
        if name == "task" && self.depth >= MAX_DEPTH {
            return Err("Tool `task` is not available: subagent depth limit reached.".into());
        }
        if name == "propose_plan" && !self.plan {
            return Err("Tool `propose_plan` is only available in plan mode.".into());
        }
        Ok(())
    }

    pub fn permits(&self, name: &str) -> bool {
        self.check(name).is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scout_rejects_bash_and_depth_rejects_task() {
        let scout = ToolPolicy { allow: Some(["read", "grep"].map(String::from).to_vec()), ..Default::default() };
        assert!(scout.check("read").is_ok());
        assert!(scout.check("bash").is_err());
        assert!(ToolPolicy { depth: 2, ..Default::default() }.check("task").is_err());
        assert!(ToolPolicy { depth: 1, ..Default::default() }.check("task").is_ok());
    }

    #[test]
    fn excluded_and_plan_only() {
        let p = ToolPolicy { excluded: vec!["ask"], ..Default::default() };
        assert!(p.check("ask").is_err());
        assert!(p.check("propose_plan").is_err());
        assert!(ToolPolicy { plan: true, ..Default::default() }.check("propose_plan").is_ok());
        assert!(ToolPolicy { plan: true, ..Default::default() }.check("bash").is_ok());
    }
}
