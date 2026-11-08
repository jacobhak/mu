//! What each model role is for, shown wherever a role is picked or assigned. The fallback chains
//! come from `models::role_chain`, so what is shown is what mu does.

use crate::config::Configs;
use crate::model_config::RoleMeta;
use crate::models::{role_chain, role_chain_for, role_selector, split_level};

struct Doc {
    name: &'static str,
    /// A few words, shown beside the role.
    purpose: &'static str,
    used_by: &'static str,
    /// What happens when neither the role nor its fallbacks are set.
    unset: &'static str,
}

/// Whether `name` is one of the roles mu itself uses.
pub fn is_builtin(name: &str) -> bool {
    DOCS.iter().any(|d| d.name == name)
}

const DOCS: [Doc; 8] = [
    Doc {
        name: "default",
        purpose: "main session",
        used_by: "The model a session starts with, and the keep-this-model choice when you approve a plan.",
        unset: "mu picks a model itself",
    },
    Doc {
        name: "plan",
        purpose: "plan mode",
        used_by: "The model while plan mode is on.",
        unset: "plan mode keeps the session's model",
    },
    Doc {
        name: "task",
        purpose: "subagents",
        used_by: "`task` subagents, which can edit and run commands, and any other agent whose `model:` names no model or role you have set.",
        unset: "subagents use the session's model",
    },
    Doc {
        name: "scout",
        purpose: "read-only scouts",
        used_by: "`scout` subagents: read-only research, everything `task` does in plan mode, and the scouts of /review.",
        unset: "scouts use the session's model",
    },
    Doc {
        name: "review",
        purpose: "/review lead",
        used_by: "The lead of /review and `mu review`. Its scouts run on @scout.",
        unset: "the review uses the session's model",
    },
    Doc {
        name: "smol",
        purpose: "titles, plugins",
        used_by: "Session titles, plugins that ask for a model, and a cheap choice when you approve a plan.",
        unset: "titles and plugin calls are skipped",
    },
    Doc {
        name: "memory",
        purpose: "memory plugin",
        used_by: "The memory plugin's model calls.",
        unset: "the memory plugin cannot ask a model",
    },
    Doc {
        name: "slow",
        purpose: "plan execution",
        used_by: "A stronger model offered when you approve a plan (`cycle_order` in models.toml lists the offered roles).",
        unset: "not offered when you approve a plan",
    },
];

/// The role an agent's `model:` entry names (`@slow`, `@slow:high`), if it is a role.
pub fn agent_role(entry: &str) -> Option<String> {
    entry.strip_prefix('@').map(|r| split_level(r).0)
}

/// A role as shown to the user.
#[derive(Clone, Debug, PartialEq)]
pub struct RoleView {
    pub name: String,
    pub purpose: String,
    pub used_by: String,
    /// The configured `provider/model[:level]`.
    pub value: Option<String>,
    /// When unset: the role mu uses instead, with its value.
    pub fallback: Option<(String, String)>,
    /// What happens when nothing in the chain is set.
    pub unset: String,
    /// Agents that list the role in `model:`.
    pub agents: Vec<String>,
    /// The role's whole `[roles.<name>].description`, for a custom role.
    pub description: Option<String>,
    /// The configured `[roles.<name>].fallback` list, in order.
    pub fallbacks: Vec<String>,
    /// The model may pick the role for a subagent (`[roles.<name>].delegate`).
    pub delegate: bool,
    /// Whether mu uses the role itself; only a custom role's settings can be edited.
    pub custom: bool,
    /// Sets other than the one being edited that also assign the role.
    pub other_sets: Vec<String>,
}

impl RoleView {
    /// One line beside the role name.
    pub fn row(&self) -> String {
        let state = match (&self.value, &self.fallback) {
            (Some(v), _) => v.clone(),
            (None, Some((f, _))) => format!("(unset → @{f})"),
            (None, None) => "(unset)".into(),
        };
        format!("{state} · {}", self.purpose)
    }

    /// What the role does and what it is now.
    pub fn detail(&self) -> String {
        let mut out = format!("Used by: {}", self.used_by);
        if !self.agents.is_empty() {
            out.push_str(&format!("\nAgents: {}", self.agents.join(", ")));
        }
        out.push_str(&match (&self.value, &self.fallback) {
            (Some(v), _) => format!("\nSet to {v}"),
            (None, Some((f, v))) => format!("\nNot set: uses @{f} ({v})."),
            (None, None) => format!("\nNot set: {}.", self.unset),
        });
        out
    }

    /// The `[roles.<name>]` settings this view shows.
    pub fn meta(&self) -> RoleMeta {
        RoleMeta { description: self.description.clone(), fallback: self.fallbacks.clone(), delegate: self.delegate }
    }
}

/// Shown for a custom role that nothing refers to yet.
const NOT_REFERENCED: &str = "nothing refers to it yet: /switch @<name>, --model @<name> or an agent's `model:`";

/// The roles mu uses, then every other role in `models.toml`, your agents or `cycle_order`.
pub fn views(cfg: &Configs, agents: &[(&str, &[String])]) -> Vec<RoleView> {
    let cycle = cfg.role_cycle();
    let using = |role: &str| -> Vec<String> {
        agents
            .iter()
            .filter(|(_, models)| models.iter().any(|m| agent_role(m).as_deref() == Some(role)))
            .map(|(name, _)| name.to_string())
            .collect()
    };
    let active = cfg.active_set();
    let other_sets = |name: &str| -> Vec<String> {
        cfg.sets_with_role(name).into_iter().filter(|s| Some(s.as_str()) != active.as_deref()).collect()
    };
    let view = |name: &str, purpose: &str, used_by: String, unset: &str, chain: Vec<String>| {
        let fallback = chain.into_iter().skip(1).find_map(|r| cfg.role(&r).map(|v| (r, v)));
        RoleView {
            name: name.into(),
            purpose: purpose.into(),
            used_by,
            value: cfg.role(name),
            fallback,
            unset: unset.into(),
            agents: using(name),
            description: None,
            fallbacks: Vec::new(),
            delegate: false,
            custom: false,
            other_sets: other_sets(name),
        }
    };
    let mut out: Vec<RoleView> =
        DOCS.iter().map(|d| view(d.name, d.purpose, d.used_by.into(), d.unset, role_chain(d.name))).collect();
    let mut custom: Vec<String> = agents
        .iter()
        .flat_map(|(_, models)| models.iter())
        .filter_map(|m| agent_role(m))
        .chain(cycle.iter().cloned())
        .chain(cfg.custom_role_names())
        .filter(|r| !DOCS.iter().any(|d| d.name == r))
        .collect();
    custom.sort_unstable();
    custom.dedup();
    for name in custom {
        let meta = cfg.role_meta(&name).unwrap_or_default();
        let purpose = meta
            .description
            .as_deref()
            .and_then(|d| d.lines().next())
            .map(str::trim)
            .filter(|d| !d.is_empty())
            .unwrap_or("custom role");
        let mut parts = Vec::new();
        if cycle.contains(&name) {
            parts.push("offered when you approve a plan (`cycle_order`)");
        }
        if !using(&name).is_empty() {
            parts.push("agents that list it in `model:`");
        }
        if meta.delegate {
            parts.push("the model may pick it for a subagent (`delegate = true`)");
        }
        let mut used_by =
            if parts.is_empty() { NOT_REFERENCED.replace("<name>", &name) } else { format!("{}.", parts.join("; ")) };
        if let Some(d) = meta.description.as_deref()
            && d.lines().count() > 1
        {
            used_by = format!("{}\n{used_by}", d.trim());
        }
        // An agent whose role is unset takes the `task` model.
        let mut chain = role_chain_for(&name, cfg);
        for r in role_chain("task") {
            if !chain.contains(&r) {
                chain.push(r);
            }
        }
        let mut v = view(&name, purpose, used_by, "agents use @task", chain);
        v.description = meta.description;
        v.fallbacks = meta.fallback;
        v.delegate = meta.delegate;
        v.custom = true;
        out.push(v);
    }
    out
}

/// A role being created: nothing is saved for it yet.
pub fn unsaved(name: &str) -> RoleView {
    RoleView {
        name: name.into(),
        purpose: "custom role".into(),
        used_by: NOT_REFERENCED.replace("<name>", name),
        value: None,
        fallback: None,
        unset: "agents use @task".into(),
        agents: vec![],
        description: None,
        fallbacks: Vec::new(),
        delegate: false,
        custom: true,
        other_sets: Vec::new(),
    }
}

/// Custom roles the model may pick for a subagent: `delegate = true` and something in the chain is
/// set. (name, description)
pub fn delegated(cfg: &Configs) -> Vec<(String, String)> {
    cfg.custom_role_names()
        .into_iter()
        .filter_map(|name| {
            let meta = cfg.role_meta(&name).unwrap_or_default();
            if !meta.delegate || role_selector(&name, cfg).is_none() {
                return None;
            }
            Some((name, meta.description.unwrap_or_else(|| "(no description)".into())))
        })
        .collect()
}

/// `@role` has no model: what it is for, and where mu goes instead.
pub fn not_configured(role: &str, cfg: &Configs) -> String {
    let mut s = format!("model role @{role} is not configured");
    if let Some(d) = DOCS.iter().find(|d| d.name == role) {
        let chain = role_chain(role);
        s.push_str(&format!(" ({}", d.purpose));
        if chain.len() > 1 {
            let rest: Vec<String> = chain[1..].iter().map(|r| format!("@{r}")).collect();
            s.push_str(&format!("; when unset mu falls back to {}", rest.join(" then ")));
        }
        s.push(')');
        return s;
    }
    let meta = cfg.role_meta(role).unwrap_or_default();
    let mut parts: Vec<String> = Vec::new();
    if let Some(d) = meta.description.as_deref().map(str::trim).filter(|d| !d.is_empty()) {
        parts.push(d.to_string());
    }
    if !meta.fallback.is_empty() {
        let rest: Vec<String> = meta.fallback.iter().map(|r| format!("@{r}")).collect();
        parts.push(format!("when unset mu falls back to {}", rest.join(" then ")));
    }
    if !parts.is_empty() {
        s.push_str(&format!(" ({})", parts.join("; ")));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(toml: &str) -> Configs {
        Configs::from_models_toml(toml)
    }

    fn find<'a>(v: &'a [RoleView], name: &str) -> &'a RoleView {
        v.iter().find(|r| r.name == name).unwrap_or_else(|| panic!("no {name} in {v:?}"))
    }

    #[test]
    fn every_role_mu_uses_is_listed_set_or_not_with_its_fallback() {
        let v = views(&cfg("[sets.main]\ndefault = \"a/b\"\ncommit = \"c/d\"\n"), &[]);
        let names: Vec<&str> = v.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(
            names,
            ["default", "plan", "task", "scout", "review", "smol", "memory", "slow", "commit"],
            "commit is listed after the roles mu uses"
        );
        assert_eq!(find(&v, "default").row(), "a/b · main session");
        assert_eq!(find(&v, "review").row(), "(unset → @default) · /review lead");
        assert_eq!(find(&v, "slow").row(), "(unset) · plan execution");
        let c = find(&v, "commit");
        assert_eq!(c.row(), "c/d · custom role");
        assert!(c.used_by.contains("nothing refers to it yet"), "{}", c.used_by);
        let d = find(&v, "review").detail();
        assert!(d.contains("Not set: uses @default (a/b)"), "{d}");
        assert!(!d.contains("Enter"), "{d}");
        assert!(find(&v, "slow").detail().contains("not offered when you approve a plan"));
        let v = views(&cfg("[sets.main]\ndefault = \"a/b\"\nsmol = \"s/m\"\n"), &[]);
        assert_eq!(find(&v, "scout").row(), "(unset → @smol) · read-only scouts");
        assert_eq!(find(&v, "task").row(), "(unset → @default) · subagents");
    }

    #[test]
    fn role_metadata_describes_a_custom_role_and_offers_delegation() {
        let c = cfg("[sets.main]\nsmol = \"s/m\"\ncommit = \"anthropic/y\"\n\
             [roles.writer]\ndescription = \"Long-form docs\"\nfallback = [\"smol\"]\ndelegate = true\n\
             [roles.commit]\ndescription = \"Commit messages\"\n\
             [roles.ghost]\ndescription = \"Stale\"\ndelegate = true\n\
             [roles.multi]\ndescription = \"Short\\nlonger text\"\n");
        let v = views(&c, &[]);
        let w = find(&v, "writer");
        assert_eq!((w.purpose.as_str(), w.row().as_str()), ("Long-form docs", "(unset → @smol) · Long-form docs"));
        assert!(w.used_by.contains("delegate = true"), "{}", w.used_by);
        assert!(w.custom && w.delegate && !find(&v, "default").custom);
        assert_eq!(
            (w.description.as_deref(), w.fallbacks.as_slice()),
            (Some("Long-form docs"), &["smol".to_string()][..])
        );
        assert_eq!(
            w.meta(),
            RoleMeta { description: Some("Long-form docs".into()), fallback: vec!["smol".into()], delegate: true }
        );
        assert_eq!(find(&v, "commit").row(), "anthropic/y · Commit messages");
        assert_eq!(find(&v, "ghost").row(), "(unset) · Stale");
        assert!(find(&v, "multi").used_by.starts_with("Short\nlonger text\n"), "{:?}", find(&v, "multi").used_by);
        assert_eq!(delegated(&c), vec![("writer".to_string(), "Long-form docs".to_string())]);
        assert!(delegated(&cfg("")).is_empty());
        let u = unsaved("writer");
        assert_eq!((u.purpose.as_str(), u.value, u.agents.len()), ("custom role", None, 0));
        assert!(u.used_by.contains("nothing refers to it yet"));
    }

    #[test]
    fn a_role_set_in_other_sets_says_where_else_it_is() {
        let c =
            cfg("set = \"main\"\n[sets.main]\nwriter = \"a/b\"\n[sets.work]\nwriter = \"c/d\"\ndefault = \"e/f\"\n");
        let v = views(&c, &[]);
        assert_eq!(find(&v, "writer").other_sets, ["work"]);
        assert_eq!(find(&v, "default").other_sets, ["work"]);
        assert!(find(&v, "writer").custom);
    }

    #[test]
    fn agents_and_cycle_order_bring_in_custom_roles() {
        let agents = [
            ("docs", &["@writer:high".to_string(), "a/b".to_string()][..]),
            ("lint", &["@writer".to_string(), "@smol".to_string()][..]),
        ];
        let v =
            views(&cfg("cycle_order = [\"smol\", \"fast\"]\n[sets.main]\nsmol = \"a/b\"\nwriter = \"c/d\"\n"), &agents);
        assert_eq!(find(&v, "smol").agents, ["lint"]);
        let w = find(&v, "writer");
        assert_eq!((w.agents.clone(), w.value.as_deref()), (vec!["docs".into(), "lint".into()], Some("c/d")));
        assert_eq!(w.purpose, "custom role");
        let f = find(&v, "fast");
        assert!(f.used_by.contains("cycle_order") && f.agents.is_empty(), "{f:?}");
        assert_eq!(f.row(), "(unset) · custom role");
        assert!(f.detail().contains("agents use @task"));
    }

    #[test]
    fn the_error_names_what_the_role_is_for_and_its_chain() {
        let c = cfg("[sets.main]\nsmol = \"s/m\"\n[roles.writer]\ndescription = \"Docs\"\nfallback = [\"smol\"]\n");
        assert_eq!(
            not_configured("review", &c),
            "model role @review is not configured (/review lead; when unset mu falls back to @plan then @default)"
        );
        assert_eq!(not_configured("slow", &c), "model role @slow is not configured (plan execution)");
        assert_eq!(not_configured("mine", &c), "model role @mine is not configured");
        assert_eq!(
            not_configured("writer", &c),
            "model role @writer is not configured (Docs; when unset mu falls back to @smol)"
        );
    }
}
