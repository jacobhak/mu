//! One place that decides which tools a run may use. `Agent::tool_defs` (what the model is told)
//! and `Agent::run_tool` (what is executed) both ask it, so the two cannot drift apart.
//!
//! Bash is allowed unless a mode denies it, as in omp: the plan-mode prompt tells the model
//! not to change anything, but nothing sandboxes the shell.

#[derive(Debug, Clone, Default)]
pub struct ToolPolicy {
    /// When set, only these tools may run.
    pub allow: Option<Vec<String>>,
    /// Tools that never run (e.g. `ask` with no user to answer).
    pub excluded: Vec<&'static str>,
    /// Subagent nesting depth of this process.
    pub depth: u32,
    /// The active mode.
    pub mode: Option<ModePolicy>,
    /// Every mode's exit tool with the mode it belongs to.
    pub mode_tools: Vec<(String, String)>,
}

/// What the active mode lets through.
#[derive(Debug, Clone)]
pub struct ModePolicy {
    pub name: String,
    pub title: String,
    pub exit_tool: String,
    pub allow: Option<Vec<String>>,
    pub deny: Vec<String>,
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
        let active_exit = self.mode.as_ref().map(|m| m.exit_tool.as_str());
        if active_exit != Some(name)
            && let Some((_, mode)) = self.mode_tools.iter().find(|(t, _)| t == name)
        {
            return Err(format!("Tool `{name}` is only available in {mode} mode."));
        }
        if let Some(m) = &self.mode {
            if name == "enter_mode" {
                return Err(format!("{} mode is already on.", m.title));
            }
            let exempt = name == m.exit_tool || name == "yield";
            if !exempt
                && (m.deny.iter().any(|d| d == name) || m.allow.as_ref().is_some_and(|a| !a.iter().any(|x| x == name)))
            {
                return Err(format!("Tool `{name}` is not available in {} mode.", m.name));
            }
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

    fn plan() -> ToolPolicy {
        ToolPolicy {
            mode: Some(ModePolicy {
                name: "plan".into(),
                title: "Plan".into(),
                exit_tool: "propose_plan".into(),
                allow: None,
                deny: vec![],
            }),
            mode_tools: vec![("propose_plan".into(), "plan".into())],
            ..Default::default()
        }
    }

    #[test]
    fn scout_rejects_bash_and_depth_rejects_task() {
        let scout = ToolPolicy { allow: Some(["read", "grep"].map(String::from).to_vec()), ..Default::default() };
        assert!(scout.check("read").is_ok());
        assert!(scout.check("yield").is_ok(), "yield bypasses the allowlist");
        assert!(scout.check("bash").is_err());
        assert!(ToolPolicy { depth: 2, ..Default::default() }.check("task").is_err());
        assert!(ToolPolicy { depth: 1, ..Default::default() }.check("task").is_ok());
    }

    #[test]
    fn excluded_and_mode_only() {
        let p = ToolPolicy { excluded: vec!["ask"], mode_tools: plan().mode_tools, ..Default::default() };
        assert!(p.check("ask").is_err());
        assert_eq!(p.check("propose_plan"), Err("Tool `propose_plan` is only available in plan mode.".into()));
        assert!(plan().check("propose_plan").is_ok());
        assert!(plan().check("bash").is_ok());
    }

    #[test]
    fn enter_mode_only_outside_a_mode() {
        assert!(ToolPolicy::default().check("enter_mode").is_ok());
        assert_eq!(plan().check("enter_mode"), Err("Plan mode is already on.".into()));
        let excluded = ToolPolicy { excluded: vec!["enter_mode"], ..Default::default() };
        assert!(excluded.check("enter_mode").is_err());
    }

    #[test]
    fn a_mode_restricts_its_tools() {
        let mut p = plan();
        p.mode.as_mut().unwrap().deny = vec!["bash".into()];
        assert_eq!(p.check("bash"), Err("Tool `bash` is not available in plan mode.".into()));
        assert!(p.check("read").is_ok());
        p.mode.as_mut().unwrap().allow = Some(vec!["read".into()]);
        assert!(p.check("read").is_ok() && p.check("grep").is_err());
        assert!(p.check("propose_plan").is_ok() && p.check("yield").is_ok());
    }
}
