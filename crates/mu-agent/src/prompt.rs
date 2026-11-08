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
    /// The `task` tool is offered to this run; the delegation rules are only stated then.
    pub task: bool,
    /// Custom roles the model may pick for a `task`'s `model` (name, description).
    pub roles: &'a [(String, String)],
    pub agent_prompt: Option<&'a str>,
    pub plugin_ctx: &'a std::collections::BTreeMap<String, String>,
    /// The MCP servers' instructions block (`PluginHost::mcp_instructions`).
    pub mcp_instructions: Option<&'a str>,
    /// The edit tool takes `[path#TAG]` hashline edits (`edit` in hashline mode) rather than `*** Find` blocks.
    pub hashline: bool,
    /// Background jobs exist (interactive sessions): an ordinary `bash` command may wait for its
    /// result instead of ending the turn (`wait` exists).
    pub jobs: bool,
    /// The `bash` tool is offered to this run; the shell rules are only stated then.
    pub bash: bool,
}

/// How a command that takes a while is run. Without background jobs the only way to wait is to end
/// the turn; with them an ordinary call may wait, and a new prompt releases the wait.
const BASH_ASYNC_RULE: &str = "- Commands that may run longer than ~5 seconds MUST use `bash` with `async: true`. It returns a job id at once; when the job finishes, mu automatically starts your next turn with its result, with no user input needed. Meanwhile do other work that does not depend on it; when your next step needs the result, end your turn with a short status and no tool call. Never wait for a job in the foreground.";

const BASH_WAIT_RULE: &str = "- `bash` runs every command in its own shell: pass `cwd` and the environment a command needs, and put setup that must carry over into the same command. Nothing persists between calls, and background descendants end with the call.\n- A command may run in the foreground until it finishes. When a result is needed now, use `wait` on a background job; it returns as soon as one of the jobs finishes, and a new prompt from the user releases the wait without stopping the work. Use `async: true` for independent concurrent work or a long-lived service: it returns a job id at once and its result arrives as a system notice, with no user input needed.";

/// A read-only agent (`scout`) has no shell: saying what to do instead beats naming a tool it cannot call.
const NO_SHELL_RULE: &str =
    "- This run has no shell: read with `read`, search with `grep`/`glob`, and say when only a shell could answer.";

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
    // The edit tool's own description covers its format; this adds only the cross-call rule.
    let edit_rule = if i.hashline {
        " Every edit changes a file's `[path#TAG]`: put all changes to one file in a single `edit` call."
    } else {
        ""
    };
    let bash_wait_rule = if !i.bash {
        NO_SHELL_RULE
    } else if i.jobs {
        BASH_WAIT_RULE
    } else {
        BASH_ASYNC_RULE
    };
    let mut s = SYSTEM
        .replace("{edit_rule}", edit_rule)
        .replace("{bash_wait_rule}", bash_wait_rule)
        .replace("{os}", format!("{} {}", std::env::consts::OS, os.trim()).trim_end())
        .replace("{arch}", std::env::consts::ARCH)
        .replace("{date}", &date.to_string())
        .replace("{model}", i.model)
        .replace("{cwd}", &i.cwd.display().to_string());
    if i.ask {
        s.push_str(
            "\n# Decisions\n\
A decision that belongs to the user (scope, design, trade-offs, anything ambiguous, risky or hard to undo) MUST be asked with `ask`; NEVER end a turn with a decision question or an option list in prose. Informational and rhetorical questions are fine in prose.\n",
        );
    }
    if i.task && !i.subagent {
        s.push_str(
            "\n# Delegation\n\
When the `task` tool is available, SHOULD use it instead of doing everything inline:\n\
- User says `parallel` or `parallelize` → MUST use `task` subagents; parallel tool calls are not enough.\n\
- Substantial work that splits into 2+ independent, self-contained slices (multi-file changes, refactors, features, tests, investigations) → fan the slices out in one `tasks[]` batch. Set shared interfaces in `context`; one agent owns any shared edit.\n\
- Unmapped code spanning several areas → `scout` agents rather than reading file after file yourself.\n\
- Do it yourself: a single small edit, a slice you already have open, a direct question, a prerequisite the others depend on.\n\
- Agents start blank: give each full requirements, never rely on conversation history. NEVER spawn then idle, and NEVER serialize independent work.\n",
        );
        if !i.roles.is_empty() {
            s.push_str("\n- A task runs on its agent's model. Set its `model` to one of these roles only when the role's description fits that task:\n");
            for (name, description) in i.roles {
                s.push_str(&format!("  - `@{name}`: {}\n", description.replace('\n', " ")));
            }
        }
    }
    if i.subagent {
        s.push_str("\nYou are a subagent. Your final message is returned verbatim to the parent agent as your result; make it complete and self-contained.\n");
    }
    if let Some(p) = i.agent_prompt {
        s.push_str(p);
    }
    if !i.ctx.files.is_empty() {
        s.push_str("\n<context-files>\nMUST follow these context files for all tasks:\n");
        for f in &i.ctx.files {
            s.push_str(&format!("<file path=\"{}\">\n{}\n</file>\n", f.path.display(), f.content.trim_end()));
        }
        s.push_str("</context-files>\n");
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
            jobs: false,
            bash: true,
            cwd: &proj,
            ctx: &ctx,
            plan: None,
            subagent: false,
            ask: false,
            task: false,
            agent_prompt: None,
            plugin_ctx: &Default::default(),
            mcp_instructions: None,
            roles: &[],
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
            jobs: false,
            bash: true,
            cwd: &proj,
            ctx: &ctx,
            plan: None,
            subagent: false,
            ask: false,
            task: false,
            agent_prompt: None,
            plugin_ctx: &Default::default(),
            mcp_instructions: None,
            roles: &[],
        });
        assert!(sp.contains("<dir-context>\nSome directories may have rules; deeper rules override higher ones.\nBefore changes in these directories, MUST read:\n- a/AGENTS.md\n- a/b/c/d/AGENTS.md\n</dir-context>"));
    }

    #[test]
    fn system_prompt_forbids_shell_file_edits_and_tracks_edit_mode() {
        let ctx = Context::default();
        let plugins = std::collections::BTreeMap::new();
        let input = |hashline| PromptInput {
            model: "m",
            hashline,
            jobs: false,
            bash: true,
            cwd: Path::new("/tmp"),
            ctx: &ctx,
            plan: None,
            subagent: false,
            ask: false,
            task: false,
            agent_prompt: None,
            plugin_ctx: &plugins,
            mcp_instructions: None,
            roles: &[],
        };
        let sp = system_prompt(&input(true));
        assert!(sp.contains("NEVER use `sed`, `perl`") && sp.contains("NEVER `cat`"));
        assert!(sp.contains("[path#TAG]`: put all changes") && !sp.contains("{edit_rule}"));
        let find = system_prompt(&input(false));
        assert!(!find.contains("[path#TAG]") && !find.contains("{edit_rule}"));
    }

    #[test]
    fn the_command_rule_follows_whether_jobs_exist() {
        let ctx = Context::default();
        let plugins = std::collections::BTreeMap::new();
        let input = |jobs| PromptInput {
            model: "m",
            hashline: true,
            jobs,
            bash: true,
            cwd: Path::new("/tmp"),
            ctx: &ctx,
            plan: None,
            subagent: false,
            ask: false,
            task: false,
            agent_prompt: None,
            plugin_ctx: &plugins,
            mcp_instructions: None,
            roles: &[],
        };
        let on = system_prompt(&input(true));
        assert!(on.contains("every command in its own shell") && on.contains("use `wait`"));
        assert!(!on.contains("Never wait for a job in the foreground"));
        assert!(!on.contains("MUST use `bash` with `async: true`"));
        assert!(!on.contains("{bash_wait_rule}"));
        let off = system_prompt(&input(false));
        assert!(off.contains("Never wait for a job in the foreground"));
        assert!(!off.contains("use `wait`"));
    }

    /// A run without `bash` must not be told how to call it: `scout` has no shell at all.
    #[test]
    fn no_shell_rule_replaces_the_bash_rules() {
        let ctx = Context::default();
        let plugins = std::collections::BTreeMap::new();
        let input = |bash| PromptInput {
            model: "m",
            hashline: true,
            jobs: false,
            bash,
            cwd: Path::new("/tmp"),
            ctx: &ctx,
            plan: None,
            subagent: true,
            ask: false,
            task: false,
            agent_prompt: None,
            plugin_ctx: &plugins,
            mcp_instructions: None,
            roles: &[],
        };
        let sp = system_prompt(&input(false));
        assert!(sp.contains("This run has no shell"), "{sp}");
        assert!(!sp.contains("MUST use `bash`"), "{sp}");
        assert!(!sp.contains("use `wait`") && !sp.contains("{bash_wait_rule}"), "{sp}");
        assert!(system_prompt(&input(true)).contains("MUST use `bash` with `async: true`"));
    }

    #[test]
    fn delegation_only_when_task_is_offered_to_a_parent() {
        let ctx = Context::default();
        let plugins = std::collections::BTreeMap::new();
        let input = |task, subagent| PromptInput {
            model: "m",
            hashline: true,
            jobs: false,
            bash: true,
            cwd: Path::new("/tmp"),
            ctx: &ctx,
            plan: None,
            subagent,
            ask: false,
            task,
            agent_prompt: None,
            plugin_ctx: &plugins,
            mcp_instructions: None,
            roles: &[],
        };
        assert!(system_prompt(&input(true, false)).contains("# Delegation"));
        assert!(!system_prompt(&input(false, false)).contains("# Delegation"));
        assert!(!system_prompt(&input(true, true)).contains("# Delegation"));
    }

    #[test]
    fn delegated_roles_are_listed_only_where_the_model_may_pick_them() {
        let ctx = Context::default();
        let plugins = std::collections::BTreeMap::new();
        let roles = [("w".to_string(), "Docs\nand prose".to_string())];
        let input = |task, subagent| PromptInput {
            model: "m",
            hashline: true,
            jobs: false,
            bash: true,
            cwd: Path::new("/tmp"),
            ctx: &ctx,
            plan: None,
            subagent,
            ask: false,
            task,
            roles: &roles,
            agent_prompt: None,
            plugin_ctx: &plugins,
            mcp_instructions: None,
        };
        assert!(system_prompt(&input(true, false)).contains("  - `@w`: Docs and prose\n"));
        assert!(!system_prompt(&input(false, false)).contains("- `@w`:"));
        assert!(!system_prompt(&input(true, true)).contains("- `@w`:"));
    }

    #[test]
    fn decision_rule_only_when_ask_is_offered() {
        let ctx = Context::default();
        let plugins = std::collections::BTreeMap::new();
        let input = |ask| PromptInput {
            model: "m",
            hashline: true,
            jobs: false,
            bash: true,
            cwd: Path::new("/tmp"),
            ctx: &ctx,
            plan: None,
            subagent: false,
            ask,
            task: false,
            agent_prompt: None,
            plugin_ctx: &plugins,
            mcp_instructions: None,
            roles: &[],
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
            jobs: false,
            bash: true,
            cwd: Path::new("/tmp"),
            ctx: &ctx,
            plan: None,
            subagent: true,
            ask: false,
            task: false,
            agent_prompt: Some(block),
            plugin_ctx: &Default::default(),
            mcp_instructions: None,
            roles: &[],
        });
        assert!(sp.contains("<agent name=\"x\">\nDo it.\n</agent>"));
    }
}
