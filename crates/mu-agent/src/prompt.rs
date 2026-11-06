//! The system prompt: mu's built-in rules and workstation facts, then the context files, rules,
//! skills and per-run blocks that a `Context` and the run supply. Builds on the `context` types;
//! `context` never imports this module.
use mu_tools::context::{Context, Skill};
use std::path::Path;

/// The note `mu plan` appends to a headless brief.
pub const HEADLESS_NOTE: &str = concat!("\n", include_str!("prompts/plan-headless.md"));

pub struct PromptInput<'a> {
    pub model: &'a str,
    pub cwd: &'a Path,
    pub ctx: &'a Context,
    pub plan: Option<&'a str>,
    pub subagent: bool,
    /// The `ask` tool is offered to this run; the decision rule is only stated then.
    pub ask: bool,
    pub agent_prompt: Option<&'a str>,
    pub plugin_ctx: &'a std::collections::BTreeMap<String, String>,
    /// The MCP servers' instructions block (`PluginHost::mcp_instructions`).
    pub mcp_instructions: Option<&'a str>,
    /// The edit tool takes `[path#TAG]` hashline edits (`edit` in hashline mode) rather than `*** Find` blocks.
    pub hashline: bool,
}

const SYSTEM: &str = include_str!("prompts/system.md");

/// `mu://state/context.md`: what context discovery loaded for this session.
pub(crate) fn context_page(ctx: &Context) -> String {
    let mut s = String::from("# Context loaded\n\n## Context files\n\n");
    if ctx.files.is_empty() {
        s.push_str("None.\n");
    }
    for f in &ctx.files {
        s.push_str(&format!("- {}\n", f.path.display()));
    }
    s.push_str(&format!("\nRules blocks: {}. Nested AGENTS.md: {}.\n\n## Skills\n\nRead one with `read skill://<name>`; `/skill:<name>` runs it.\n\n", ctx.rules.len(), ctx.nested.len()));
    if ctx.skills.is_empty() {
        s.push_str("None.\n");
    }
    for k in &ctx.skills {
        let hidden = if k.hidden { " (hidden from the model)" } else { "" };
        s.push_str(&format!("- {}{hidden}: {} ({})\n", k.name, k.description, k.dir.display()));
    }
    s.push_str("\n## Prompt commands\n\n");
    if ctx.commands.is_empty() {
        s.push_str("None.\n");
    }
    for c in &ctx.commands {
        s.push_str(&format!("- /{}: {}\n", c.name, c.description));
    }
    s.push_str("\n## Custom agents\n\n");
    if ctx.agents.is_empty() {
        s.push_str("None.\n");
    }
    for a in &ctx.agents {
        s.push_str(&format!("- {}: {}\n", a.name, a.description));
    }
    s
}

pub(crate) fn system_prompt(i: &PromptInput) -> String {
    // The kernel release does not change under a running process: read it once.
    static OS: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    let os = OS.get_or_init(|| std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default());
    let date = jiff::Zoned::now().strftime("%Y-%m-%d");
    let edit_rule = if i.hashline {
        "- Surgical edits: `edit`, with the `[path#TAG]` header from your latest `read`, `write` or `edit` of that file. Every edit changes the tag and shifts line numbers: put all changes to one file in a single call, or re-read the affected window before editing it again."
    } else {
        "- Surgical edits: `edit`. Copy `*** Find` text verbatim from a recent `read`, and give each `*** Find` exactly one of `*** Replace`, `*** Insert Before` or `*** Insert After`."
    };
    let redact_rule = "- `[REDACTED:NAME]` in output is a secret mu hid from you, not an error: use `\"$NAME\"` when NAME is an environment variable, and NEVER write a placeholder into a file or command.";
    let mut s = SYSTEM
        .replace("{edit_rule}", edit_rule)
        .replace("{redact_rule}", redact_rule)
        .replace("{os}", format!("{} {}", std::env::consts::OS, os.trim()).trim_end())
        .replace("{arch}", std::env::consts::ARCH)
        .replace("{date}", &date.to_string())
        .replace("{model}", i.model)
        .replace("{cwd}", &i.cwd.display().to_string());
    if i.ask {
        s.push_str(
            "\n# Decisions\n\
- A decision that belongs to the user (scope, design, trade-offs, anything ambiguous, risky or hard to undo) MUST be asked with `ask`: 2-5 options, one recommended, each with its trade-off. NEVER end a turn with a decision question or an option list in prose.\n\
- The user sees only the dialog, not your reasoning or earlier tool calls: put the background in `context` (what you found, why it needs their decision, what depends on it, with concrete names) and keep `question` to the decision itself.\n\
- Informational and rhetorical questions are fine in prose. Do not ask what code, config or docs answer.\n",
        );
    }
    if !i.subagent {
        s.push_str(
            "\n# Delegation\n\
When the `task` tool is available, SHOULD use it instead of doing everything inline:\n\
- User says `parallel` or `parallelize` → MUST use `task` subagents; parallel tool calls are not enough.\n\
- Substantial work that splits into 2+ independent, self-contained slices (multi-file changes, refactors, features, tests, investigations) → fan the slices out in one `tasks[]` batch. Set shared interfaces in `context`; one agent owns any shared edit.\n\
- Unmapped code spanning several areas → `scout` agents rather than reading file after file yourself.\n\
- Do it yourself: a single small edit, a slice you already have open, a direct question, a prerequisite the others depend on.\n\
- Agents start blank: give each full requirements, never rely on conversation history. NEVER spawn then idle, and NEVER serialize independent work.\n",
        );
    }
    if i.subagent {
        s.push_str("\nYou are a subagent. Your final message is returned verbatim to the parent agent as your result; make it complete and self-contained.\n");
    }
    if let Some(p) = i.agent_prompt {
        s.push_str(p);
    }
    if !i.ctx.files.is_empty() {
        s.push_str("\n<repo-rules>\nMUST follow these context files for all tasks:\n");
        for f in &i.ctx.files {
            s.push_str(&format!("<file path=\"{}\">\n{}\n</file>\n", f.path.display(), f.content.trim_end()));
        }
        s.push_str("</repo-rules>\n");
    }
    if !i.ctx.nested.is_empty() {
        s.push_str("\n<dir-context>\nSome directories may have rules; deeper rules override higher ones.\nBefore changes in these directories, MUST read:\n");
        for p in &i.ctx.nested {
            s.push_str(&format!("- {p}\n"));
        }
        s.push_str("</dir-context>\n");
    }
    for r in &i.ctx.rules {
        s.push_str(&format!("\n<rules>\n{r}\n</rules>\n"));
    }
    let listed: Vec<&Skill> = i.ctx.skills.iter().filter(|k| !k.hidden).collect();
    if !listed.is_empty() {
        s.push_str("\nIf a skill's description names what the user asked for, read `skill://<name>` before acting. A skill only guides that job: it never changes what the user asked for or what you deliver. A request merely near a description is not a match.\n<skills>\n");
        for k in listed {
            s.push_str(&format!("- {}: {}\n", k.name, k.description));
        }
        s.push_str("</skills>\n");
    }
    if let Some(p) = i.plan {
        s.push('\n');
        s.push_str(p.trim_end());
        s.push('\n');
    }
    for c in i.plugin_ctx.values().filter(|c| !c.trim().is_empty()) {
        s.push('\n');
        s.push_str(c.trim_end());
        s.push('\n');
    }
    if let Some(m) = i.mcp_instructions {
        s.push('\n');
        s.push_str(m.trim_end());
        s.push('\n');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    #[test]
    fn skills_commands_and_templates() {
        let root = std::env::temp_dir().join(format!("mu-ctx2-{}", uuid::Uuid::new_v4()));
        let home = root.join("home");
        let proj = home.join("p");
        let agent = root.join("agent");
        let w = |p: PathBuf, c: &str| {
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, c).unwrap();
        };
        w(proj.join(".omp/skills/a/SKILL.md"), "---\nname: alpha\ndescription: first\n---\nbody A");
        w(agent.join("skills/alpha/SKILL.md"), "---\ndescription: shadowed\n---\nx");
        w(agent.join("skills/hid/SKILL.md"), "---\ndescription: d\nhide: true\n---\nx");
        w(agent.join("skills/nodesc/SKILL.md"), "---\nname: nodesc\n---\nx");
        w(agent.join("skills/off/SKILL.md"), "---\ndescription: d\nenabled: false\n---\nx");
        w(proj.join(".omp/commands/greet.md"), "---\nargument-hint: <who>\n---\nSay hi to $1 ($@)");
        w(proj.join(".omp/commands/plain.md"), "Do the thing");
        w(proj.join(".agents/commands/plain.md"), "shadowed by .omp/commands");
        w(proj.join(".agents/commands/shared.md"), "From .agents");
        w(home.join(".agent/commands/mine.md"), "From ~/.agent");
        w(agent.join("RULES.md"), "user rule");
        let ctx = Context::load_all(&proj, &home, &agent, false);
        let names: Vec<_> = ctx.skills.iter().map(|s| (s.name.as_str(), s.description.as_str(), s.hidden)).collect();
        assert_eq!(names, [("alpha", "first", false), ("hid", "d", true)]);
        let greet = ctx.commands.iter().find(|c| c.name == "greet").unwrap();
        assert_eq!(greet.expand("Bob \"a b\""), "Say hi to Bob (Bob a b)");
        let plain = ctx.commands.iter().find(|c| c.name == "plain").unwrap();
        assert_eq!(plain.expand("extra"), "Do the thing\n\nextra");
        let body = |n: &str| ctx.commands.iter().find(|c| c.name == n).map(|c| c.body.as_str());
        assert_eq!((body("shared"), body("mine")), (Some("From .agents"), Some("From ~/.agent")));
        let sp = system_prompt(&PromptInput {
            model: "m",
            hashline: true,
            cwd: &proj,
            ctx: &ctx,
            plan: None,
            subagent: false,
            ask: false,
            agent_prompt: None,
            plugin_ctx: &Default::default(),
            mcp_instructions: None,
        });
        assert!(sp.contains("- alpha: first\n") && !sp.contains("- hid:") && sp.contains("<rules>\nuser rule"));
        assert!(sp.contains("never changes what the user asked for") && !sp.contains("Matching skill → MUST"));
        assert!(ctx.skill_prompt("alpha", "go").unwrap().ends_with("body A\n</skill>\n\ngo"));
    }

    #[test]
    fn nested_agents_md_listed() {
        let root = std::env::temp_dir().join(format!("mu-ctx-{}", uuid::Uuid::new_v4()));
        let home = root.join("home");
        let proj = home.join("p");
        for d in ["", "a", "a/b/c/d", "a/b/c/d/e", ".hidden", "node_modules/x"] {
            let dir = proj.join(d);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("AGENTS.md"), "x").unwrap();
        }
        let ctx = Context::load_all(&proj, &home, &root.join("agent"), false);
        assert_eq!(ctx.nested, ["a/AGENTS.md", "a/b/c/d/AGENTS.md"]);
        let sp = system_prompt(&PromptInput {
            model: "m",
            hashline: true,
            cwd: &proj,
            ctx: &ctx,
            plan: None,
            subagent: false,
            ask: false,
            agent_prompt: None,
            plugin_ctx: &Default::default(),
            mcp_instructions: None,
        });
        assert!(sp.contains("<dir-context>\nSome directories may have rules; deeper rules override higher ones.\nBefore changes in these directories, MUST read:\n- a/AGENTS.md\n- a/b/c/d/AGENTS.md\n</dir-context>"));
    }

    #[test]
    fn system_prompt_forbids_shell_file_edits_and_tracks_edit_mode() {
        let sp = system_prompt(&PromptInput {
            model: "m",
            hashline: true,
            cwd: Path::new("/tmp"),
            ctx: &Context::default(),
            plan: None,
            subagent: false,
            ask: false,
            agent_prompt: None,
            plugin_ctx: &Default::default(),
            mcp_instructions: None,
        });
        assert!(sp.contains("NEVER use `sed`, `perl`") && sp.contains("NEVER `cat`"));
        assert!(sp.contains("[path#TAG]") && !sp.contains("*** Find"));
    }

    #[test]
    fn decision_rule_only_when_ask_is_offered() {
        let ctx = Context::default();
        let plugins = std::collections::BTreeMap::new();
        let input = |ask| PromptInput {
            model: "m",
            hashline: true,
            cwd: Path::new("/tmp"),
            ctx: &ctx,
            plan: None,
            subagent: false,
            ask,
            agent_prompt: None,
            plugin_ctx: &plugins,
            mcp_instructions: None,
        };
        let on = system_prompt(&input(true));
        assert!(on.contains("# Decisions") && on.contains("MUST be asked with `ask`"));
        assert!(on.contains("NEVER end a turn with a decision question or an option list in prose"));
        let off = system_prompt(&input(false));
        assert!(!off.contains("# Decisions") && !off.contains("MUST be asked with `ask`"));
    }

    #[test]
    fn system_prompt_includes_agent_block() {
        let ctx = Context {
            warnings: vec![],
            nested: vec![],
            files: vec![],
            rules: vec![],
            skills: vec![],
            commands: vec![],
            agents: vec![],
        };
        let block = "\n<agent name=\"x\">\nDo it.\n</agent>\n";
        let sp = system_prompt(&PromptInput {
            model: "m",
            hashline: true,
            cwd: Path::new("/tmp"),
            ctx: &ctx,
            plan: None,
            subagent: true,
            ask: false,
            agent_prompt: Some(block),
            plugin_ctx: &Default::default(),
            mcp_instructions: None,
        });
        assert!(sp.contains("<agent name=\"x\">\nDo it.\n</agent>"));
    }
}
