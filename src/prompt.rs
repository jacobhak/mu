//! The system prompt: mu's built-in rules and workstation facts, then the context files, rules,
//! skills and per-run blocks that a `Context` and the run supply. Builds on the `context` types;
//! `context` never imports this module.
use crate::context::{Context, Skill};
use std::path::Path;

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

pub fn system_prompt(i: &PromptInput) -> String {
    // The kernel release does not change under a running process: read it once.
    static OS: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    let os = OS.get_or_init(|| std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default());
    let date = jiff::Zoned::now().strftime("%Y-%m-%d");
    let edit_rule = if i.hashline {
        "- Surgical edits: `edit`, with the `[path#TAG]` header from your latest `read`, `write` or `edit` of that file. Every edit changes the tag and shifts line numbers: put all changes to one file in a single call, or re-read the affected window before editing it again.\n"
    } else {
        "- Surgical edits: `edit`. Copy `*** Find` text verbatim from a recent `read`, and give each `*** Find` exactly one of `*** Replace`, `*** Insert Before` or `*** Insert After`.\n"
    };
    let redact_rule = "- `[REDACTED:NAME]` in output is a secret mu hid from you, not an error: use `\"$NAME\"` when NAME is an environment variable, and NEVER write a placeholder into a file or command.\n";
    let mut s = format!(
        "You are mu, a coding agent running in the user's terminal. Work in the current directory with the provided tools.\n\
RFC 2119 keywords: MUST, SHOULD, MAY. `NEVER` = `MUST NOT`.\n\n\
# Tool policy\n\
MUST use the specialized tool over its shell equivalent:\n\
- File and directory reads: `read` (a directory lists its entries). Use selector ranges, not whole files. NEVER `cat`, `head`, `tail`, `sed -n`, `awk`, `ls` or Python to read.\n\
{edit_rule}\
- Create or overwrite a file: `write`. Use it only for new files or complete rewrites.\n\
- Content search: `grep`, NEVER shell `grep`/`rg`/`awk`. File names and structure: `glob`, NEVER `find`/`fd`/`ls **/*.ext`.\n\
- `bash`: real binaries (builds, tests, git, package managers) and short fact pipelines such as counts or checksums. NEVER specialized-tool work, and NEVER paging, moving or trimming bytes you could `read`.\n\
{redact_rule}\
<critical>\n\
NEVER use `sed`, `perl`, `awk`, `python`, `node`, heredocs or `echo >` through `bash` to change files, singly or in bulk; MUST use `edit` or `write`. If `edit` fails, read its error, fix the payload and retry with `edit`. A failed edit is NEVER a reason to switch to the shell.\n\
</critical>\n\n\
# Editing\n\
- Read the relevant sections before editing; reuse existing patterns rather than starting a second convention.\n\
- After a tool failure, or if a file may have changed, re-read before acting.\n\
- Touch only the lines the task needs; NEVER restyle unrelated code.\n\
- Resolve prerequisites first and parallelize independent calls. NEVER open guessed files.\n\
- Tool results are verification: NEVER re-read a file just to check an applied edit.\n\
- Be concise; the user sees each tool call.\n\
- For work with 3+ steps, keep the `todo` list current as you go: `start` an item before working on it and mark it `done` right after finishing that one item, never several at once and never all at the end. NEVER finish with items still pending.\n\n\
<workstation>\n- OS: {}\n- Arch: {}\n- Model: {}\n- Date: {date}\n- Working directory: {}\n</workstation>\n",
        format!("{} {}", std::env::consts::OS, os.trim()).trim_end(),
        std::env::consts::ARCH,
        i.model,
        i.cwd.display()
    );
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
