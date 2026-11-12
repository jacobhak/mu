//! Tool permission types shared by the config, the gate, subagents and every front end.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// How much the agent may do without asking.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionMode {
    /// Ask for file edits and for commands that are not read-only.
    Ask,
    /// File edits inside the project run; other commands ask.
    Edits,
    /// Nothing asks. The default.
    #[default]
    Yolo,
}

/// What one classified call may do: the unit a mode runs without asking.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Capability {
    /// Read files inside the project.
    ReadProject,
    /// Change files inside the project.
    WriteProject,
    /// Read anything outside the project.
    ReadOutside,
    /// Run commands, and write outside the project.
    Exec,
}

impl PermissionMode {
    pub const NAMES: [&str; 3] = ["ask", "edits", "yolo"];

    /// What this mode runs without asking; every call outside the set prompts, unless a rule
    /// decided it first.
    pub fn capabilities(self) -> &'static [Capability] {
        const ASK: &[Capability] = &[Capability::ReadProject];
        const EDITS: &[Capability] = &[Capability::ReadProject, Capability::WriteProject];
        const YOLO: &[Capability] =
            &[Capability::ReadProject, Capability::WriteProject, Capability::ReadOutside, Capability::Exec];
        match self {
            Self::Ask => ASK,
            Self::Edits => EDITS,
            Self::Yolo => YOLO,
        }
    }

    pub fn allows(self, c: Capability) -> bool {
        self.capabilities().contains(&c)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::Edits => "edits",
            Self::Yolo => "yolo",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::NAMES.iter().position(|n| *n == s).map(|i| [Self::Ask, Self::Edits, Self::Yolo][i])
    }
}

/// What a rule decides for a matching call.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Allow,
    Ask,
    Deny,
}

impl Action {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Ask => "ask",
            Self::Deny => "deny",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        [Self::Allow, Self::Ask, Self::Deny].into_iter().find(|a| a.as_str() == s)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rule {
    pub pattern: String,
    pub action: Action,
}

/// Rules in source order: a tool-name glob and what it decides.
#[derive(Clone, Debug, Default)]
pub struct RuleSet(pub Vec<(String, Rule)>);

impl RuleSet {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn extend(&mut self, other: &RuleSet) {
        self.0.extend(other.0.iter().cloned());
    }

    /// A `[permissions.rules]` table: `tool = "action"` or `tool = { pattern = "action" }`.
    /// Invalid entries are dropped with a warning; they never become `allow`.
    pub fn parse(table: &toml::Table, origin: &str, warn: &mut Vec<String>) -> RuleSet {
        let mut rules = RuleSet::default();
        for (tool, value) in table {
            let why = |why: String| format!("{origin}: [permissions.rules] {tool}: {why}; ignored");
            match value {
                toml::Value::String(action) => match Action::parse(action) {
                    Some(action) => rules.0.push((tool.clone(), Rule { pattern: "*".into(), action })),
                    None => warn.push(why(format!("\"{action}\" is not allow, ask or deny"))),
                },
                toml::Value::Table(patterns) => {
                    for (pattern, value) in patterns {
                        match value.as_str() {
                            Some(s) => match Action::parse(s) {
                                Some(action) => rules.0.push((tool.clone(), Rule { pattern: pattern.clone(), action })),
                                None => warn.push(why(format!("\"{s}\" is not allow, ask or deny"))),
                            },
                            None => warn.push(why(format!("{pattern}: the action is not a string"))),
                        }
                    }
                }
                other => warn.push(why(format!("expected an action or a table, found {}", other.type_str()))),
            }
        }
        rules
    }

    /// `[{"tool":…, "pattern":…, "action":…}]`, for the subagent payload.
    pub fn to_json(&self) -> Value {
        Value::Array(
            self.0
                .iter()
                .map(|(tool, r)| serde_json::json!({"tool": tool, "pattern": r.pattern, "action": r.action.as_str()}))
                .collect(),
        )
    }

    pub fn from_json(v: &Value) -> RuleSet {
        let mut rules = RuleSet::default();
        for e in v.as_array().into_iter().flatten() {
            let (Some(tool), Some(pattern), Some(action)) =
                (e["tool"].as_str(), e["pattern"].as_str(), e["action"].as_str().and_then(Action::parse))
            else {
                continue;
            };
            rules.0.push((tool.to_string(), Rule { pattern: pattern.into(), action }));
        }
        rules
    }
}

/// What a prompt may grant for later calls.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Grant {
    pub tool: String,
    /// Subjects matched as-is; `["*"]` grants every call of the tool.
    pub patterns: Vec<String>,
    /// The widths above the exact one, narrowest first: the directory, the checkout it sits in,
    /// the checkouts beside it, `~`, and `/`. Empty when the grant cannot widen.
    #[serde(default)]
    pub wider: Vec<GrantWidth>,
}

/// One width a grant can widen to, named so the prompt reads as a choice of scope.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrantWidth {
    /// How the prompt names it: "its directory", "this checkout", "all of ~".
    pub label: String,
    /// What it would remember: `<dir>/*` for each path, or the rebuilt command line.
    pub patterns: Vec<String>,
}

impl Grant {
    /// This grant with the patterns an answer picked, or with its own when it named none.
    pub fn picked(&self, patterns: Option<Vec<String>>) -> Grant {
        Grant {
            tool: self.tool.clone(),
            wider: self.wider.clone(),
            patterns: patterns.unwrap_or_else(|| self.patterns.clone()),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ApprovalRequest {
    pub tool: String,
    pub call_id: String,
    /// `<tool>: <summary>`.
    pub title: String,
    /// Why this call needs approval.
    pub reason: String,
    /// The command, the content or the pretty-printed arguments.
    pub body: String,
    /// The subagent the prompt was forwarded from.
    pub from: Option<String>,
    /// What "don't ask again" would remember; `None` when that cannot be offered.
    pub grant: Option<Grant>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalScope {
    Once,
    Session,
    Project,
    Global,
}

impl ApprovalScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Once => "once",
            Self::Session => "session",
            Self::Project => "project",
            Self::Global => "global",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "lowercase")]
pub enum ApprovalDecision {
    Allow {
        scope: ApprovalScope,
        note: Option<String>,
        /// The patterns the answer picked from the wider levels; `None` for the request's own.
        #[serde(default)]
        patterns: Option<Vec<String>>,
    },
    Deny {
        note: Option<String>,
    },
    /// Nobody can answer: the call does not run.
    Unavailable,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_round_trip() {
        for m in [PermissionMode::Ask, PermissionMode::Edits, PermissionMode::Yolo] {
            assert_eq!(PermissionMode::parse(m.as_str()), Some(m));
        }
        assert_eq!(PermissionMode::parse("nope"), None);
        assert_eq!(PermissionMode::default(), PermissionMode::Yolo);
    }

    #[test]
    fn each_mode_grants_a_superset_of_the_one_below() {
        use Capability::*;
        let ask = PermissionMode::Ask.capabilities();
        let edits = PermissionMode::Edits.capabilities();
        let yolo = PermissionMode::Yolo.capabilities();
        assert_eq!(ask, [ReadProject]);
        assert_eq!(edits, [ReadProject, WriteProject]);
        assert_eq!(yolo, [ReadProject, WriteProject, ReadOutside, Exec]);
        assert!(ask.iter().all(|c| edits.contains(c)), "edits adds to ask");
        assert!(edits.iter().all(|c| yolo.contains(c)), "yolo adds to edits");
        assert!(!PermissionMode::Ask.allows(ReadOutside));
        assert!(!PermissionMode::Edits.allows(ReadOutside), "edits reads outside the project only when asked");
        assert!(!PermissionMode::Edits.allows(Exec));
        assert!(PermissionMode::Yolo.allows(ReadOutside) && PermissionMode::Yolo.allows(Exec));
    }

    #[test]
    fn rules_parse_strings_and_tables_and_drop_invalid_entries() {
        let text = r#"
            bash = { "cargo test *" = "allow", "git push *" = "deny" }
            "mcp__github_*" = "allow"
            edit = { "*.lock" = "nope", "*.md" = 3 }
            fetch = 7
        "#;
        let table: toml::Table = text.parse().unwrap();
        let mut warn = Vec::new();
        let rules = RuleSet::parse(&table, "config.toml", &mut warn);
        let names: Vec<(&str, &str, Action)> =
            rules.0.iter().map(|(t, r)| (t.as_str(), r.pattern.as_str(), r.action)).collect();
        assert_eq!(
            names,
            [
                ("bash", "cargo test *", Action::Allow),
                ("bash", "git push *", Action::Deny),
                ("mcp__github_*", "*", Action::Allow),
            ]
        );
        assert_eq!(warn.len(), 3);
        assert!(warn[0].contains("nope"));
        assert!(warn[1].contains("not a string"));
        assert!(warn[2].contains("expected an action"));
        assert!(rules.0.iter().all(|(_, r)| r.action != Action::Ask || r.pattern != "*.lock"));
    }

    #[test]
    fn rules_round_trip_through_json() {
        let mut rules = RuleSet::default();
        rules.0.push(("bash".into(), Rule { pattern: "git push *".into(), action: Action::Deny }));
        let json = rules.to_json();
        let back = RuleSet::from_json(&json);
        assert_eq!(back.0, rules.0);
    }
}
