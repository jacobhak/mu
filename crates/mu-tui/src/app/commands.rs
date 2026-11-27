//! The built-in slash commands, in one table: name, description, whether Enter in the completion
//! menu must wait for an argument, whether the command may run during a turn, the argument
//! completions and the handler. Help, completion and dispatch all read it.

use super::slash::HOTKEYS;
use super::*;
use mu_base::config::Configs;
use mu_base::models::Thinking;
use mu_base::permissions::PermissionMode;
use mu_provider::auth::chatgpt::PROVIDER_ID;
use mu_provider::provider::providers;
use mu_tools::context::Context;

/// What the completion lists of the built-in commands are built from.
pub(super) struct CompleteCtx<'a> {
    pub ctx: &'a Context,
    pub cfg: &'a Configs,
    pub custom_ids: &'a [String],
}

/// The argument names and what each does (parallel; the second may be empty).
type Completions = fn(&CompleteCtx) -> (Vec<String>, Vec<String>);

pub(super) struct Builtin {
    pub name: &'static str,
    pub desc: &'static str,
    /// Enter on the bare name in the completion menu completes it and waits, instead of running it.
    pub needs_arg: bool,
    /// May run while a turn is in progress.
    pub busy_ok: bool,
    pub complete: Option<Completions>,
    /// The argument is a directory path, completed from the host's file system (`Entry::paths`).
    pub paths: bool,
    pub run: fn(&mut App, &str),
}

const fn cmd(name: &'static str, desc: &'static str, run: fn(&mut App, &str)) -> Builtin {
    Builtin { name, desc, needs_arg: false, busy_ok: false, complete: None, paths: false, run }
}

impl Builtin {
    const fn needs_arg(mut self) -> Self {
        self.needs_arg = true;
        self
    }

    const fn busy_ok(mut self) -> Self {
        self.busy_ok = true;
        self
    }

    const fn complete(mut self, f: Completions) -> Self {
        self.complete = Some(f);
        self
    }

    const fn paths(mut self) -> Self {
        self.paths = true;
        self
    }
}

fn names(items: impl IntoIterator<Item = String>) -> (Vec<String>, Vec<String>) {
    (items.into_iter().collect(), vec![])
}

fn complete_model(c: &CompleteCtx) -> (Vec<String>, Vec<String>) {
    crate::host::models::role_views(c.cfg, c.ctx).into_iter().map(|r| (format!("@{}", r.name), r.row())).unzip()
}

fn complete_modelset(c: &CompleteCtx) -> (Vec<String>, Vec<String>) {
    let subs = [
        ("new", "Copy the active set into a new one and switch to it"),
        ("delete", "Remove a set"),
        ("rename", "Rename a set"),
        ("import", "Copy omp's model config"),
    ];
    let sets = c.cfg.set_names();
    let args = subs.iter().map(|s| s.0.to_string()).chain(sets.iter().cloned()).collect();
    let descs =
        subs.iter().map(|s| s.1.to_string()).chain(sets.iter().map(|_| "Switch to this set".to_string())).collect();
    (args, descs)
}

fn complete_login(c: &CompleteCtx) -> (Vec<String>, Vec<String>) {
    names(
        [PROVIDER_ID.to_string(), "custom".to_string()]
            .into_iter()
            .chain(providers::api_key_defs().map(|d| d.id.to_string()))
            .chain(c.custom_ids.iter().cloned()),
    )
}

fn complete_logout(c: &CompleteCtx) -> (Vec<String>, Vec<String>) {
    names(
        std::iter::once(PROVIDER_ID.to_string())
            .chain(providers::api_key_defs().map(|d| d.id.to_string()))
            .chain(c.custom_ids.iter().cloned()),
    )
}

fn strings(items: &[&str]) -> (Vec<String>, Vec<String>) {
    names(items.iter().map(|s| s.to_string()))
}

/// The verbs of `/agents` and `/remote` (`new` only for the latter), with what each does.
const AGENT_VERBS: [(&str, &str); 5] = [
    ("new", "Start an agent on a remote and attach to it"),
    ("stop", "End an agent"),
    ("cancel", "Cancel an agent's current turn"),
    ("notify", "Leave an agent a message"),
    ("attach", "Open an agent here"),
];

fn complete_agents(_: &CompleteCtx) -> (Vec<String>, Vec<String>) {
    AGENT_VERBS[1..].iter().map(|&(a, d)| (a.to_string(), d.to_string())).unzip()
}

fn complete_remote(_: &CompleteCtx) -> (Vec<String>, Vec<String>) {
    AGENT_VERBS.iter().map(|&(a, d)| (a.to_string(), d.to_string())).unzip()
}

fn complete_theme(_: &CompleteCtx) -> (Vec<String>, Vec<String>) {
    use crate::theme::Origin;
    crate::theme::available()
        .into_iter()
        .map(|e| {
            let tone = match e.dark {
                Some(true) => "dark",
                Some(false) => "light",
                None => "",
            };
            let from = match e.origin {
                Origin::Bundled => "",
                Origin::Custom => "~/.mu/themes",
                Origin::Terminal => "your terminal's colours",
                Origin::Omarchy => "omarchy's current theme",
            };
            (e.name, [tone, from].iter().filter(|s| !s.is_empty()).copied().collect::<Vec<_>>().join(" · "))
        })
        .unzip()
}

pub(super) static BUILTIN: [Builtin; 41] = [
    cmd("exit", "Exit mu", |a, _| a.quit()),
    cmd("quit", "Exit mu", |a, _| a.quit()),
    cmd("detach", "Leave the agent running and close this client", |a, _| a.cmd_detach()).busy_ok(),
    cmd("reconnect", "Connect to the agent again now, without waiting out the pause", |a, _| a.cmd_reconnect())
        .busy_ok(),
    cmd("new", "Start a new session", App::cmd_new),
    cmd("resume", "Resume a session from this directory", |a, _| a.open_session_picker()),
    cmd("cd", "Change the working directory [dir]; shows it without one", App::cmd_cd).paths(),
    cmd("fork", "Continue in a copy of this session; the original stays as it is", |a, _| a.send(Command::Fork)),
    cmd("rewind", "Go back to before an earlier prompt, or to after an earlier reply", |a, _| {
        a.send(Command::ListRewind)
    }),
    cmd("retry", "Resend the last request after a failed or aborted turn", App::cmd_retry),
    cmd("model", "Edit saved model roles [@role]", App::model_cmd).complete(complete_model),
    cmd("switch", "Switch this session's model", App::cmd_switch),
    cmd("modelset", "Switch model sets [name | new | delete | rename | import]", App::modelset_cmd)
        .complete(complete_modelset),
    cmd("login", "Sign in with ChatGPT, add an API key, or add a custom endpoint", App::login).complete(complete_login),
    cmd("logout", "Remove a stored API key, or sign out of ChatGPT", App::logout_cmd).complete(complete_logout),
    cmd("web", "Choose the web search backend", |a, _| a.start_web_wizard(None)),
    cmd("thinking", "Set the thinking level", App::cmd_thinking)
        .busy_ok()
        .complete(|_| names(Thinking::NAMES.iter().map(|s| s.to_string()))),
    cmd("permissions", "Show or set tool permissions: ask, edits or yolo", App::cmd_permissions)
        .busy_ok()
        .complete(|_| names(PermissionMode::NAMES.iter().map(|s| s.to_string()))),
    cmd("compact", "Compact the context [focus]; waits for a running turn", App::cmd_compact).busy_ok(),
    cmd("review", "Review changes against the approved plan [focus]", App::cmd_review),
    cmd("refresh", "Refresh Git and gateway quota status", App::cmd_refresh).busy_ok(),
    cmd("session", "Show the session file, title and local:// directory", App::cmd_session).busy_ok(),
    cmd("rename", "Rename this session", App::cmd_rename).needs_arg(),
    cmd("todo", "Show the todo list", |a, _| a.show_todo()).busy_ok(),
    cmd("skills", "List skills", App::cmd_skills).busy_ok(),
    cmd("plugins", "List plugins", App::cmd_plugins).busy_ok(),
    cmd("theme", "Pick a colour theme, previewed live, and save it [name]", App::cmd_theme)
        .busy_ok()
        .complete(complete_theme),
    cmd("mcp", "Manage MCP servers (picker; or add, import, remove, enable, test…)", App::mcp_cmd)
        .complete(|_| MCP_SUBCOMMANDS.iter().map(|&(a, d)| (a.to_string(), d.to_string())).unzip()),
    cmd("hotkeys", "Show key bindings", |a, _| a.notice(HOTKEYS)),
    cmd("copy", "Copy a reply, code block or command", App::copy_cmd)
        .busy_ok()
        .complete(|_| strings(&["code", "cmd", "link", "local"])),
    cmd("reply", "Quote parts of the last reply into the composer", |a, _| a.open_reply()).busy_ok(),
    cmd(
        "open",
        "Open a file of the agent's machine here [path]; no path lists touched or mentioned files",
        App::open_cmd,
    )
    .busy_ok(),
    cmd("image", "Attach an image file", App::image_cmd).needs_arg().busy_ok(),
    cmd("upload", "Send files of this machine to the agent's session (when attached)", App::upload_cmd)
        .needs_arg()
        .busy_ok(),
    cmd("jobs", "Show background jobs and other mu sessions", App::jobs_cmd).busy_ok().complete(|_| strings(&["kill"])),
    cmd(
        "agents",
        "List mu agents here and on other machines [stop | cancel | notify | attach <id> …]",
        App::agents_cmd,
    )
    .busy_ok()
    .complete(complete_agents),
    cmd(
        "remote",
        "List agents on your remotes, start one or act on one [new <remote> | stop | cancel | notify | attach <id> …]",
        App::remote_cmd,
    )
    .busy_ok()
    .complete(complete_remote),
    cmd("schedule", "Send a prompt later [<delay> <prompt> | list | cancel [N|all]]", App::schedule_cmd)
        .needs_arg()
        .busy_ok()
        .complete(|_| strings(&["list", "cancel"])),
    cmd("plans", "Approve, refine or drop a parked plan", |a, _| a.send(Command::ListPlans)),
    cmd("memory", "Show and edit memory: files, notes and history; revert a commit", |a, _| a.open_memory()),
    cmd("deliver", "Implement, review and commit a parked plan in a worktree [id]", App::cmd_deliver),
];

/// `/mcp` subcommands for the completion menu, with what each does. The names that need a server
/// open the server picker when typed bare.
const MCP_SUBCOMMANDS: [(&str, &str); 13] = [
    ("add", "Add a server: discovered, popular or custom"),
    ("import", "Copy servers other tools have configured"),
    ("list", "Show servers and their state"),
    ("enable", "Turn a server on"),
    ("disable", "Turn a server off"),
    ("remove", "Delete a server from the config"),
    ("test", "Reconnect a server and list its tools"),
    ("reconnect", "Restart a server and refresh its tools"),
    ("login", "Sign in to a server that uses OAuth"),
    ("reload", "Re-read the config files"),
    ("trust", "Start this project's MCP servers"),
    ("untrust", "Stop trusting this project's MCP servers"),
    ("help", "Show the syntax"),
];

pub(super) fn find(name: &str) -> Option<&'static Builtin> {
    BUILTIN.iter().find(|c| c.name == name)
}

pub(crate) fn is_builtin(name: &str) -> bool {
    find(name).is_some()
}

/// `mu://reference/interactive.md`: the built-in slash commands and the key bindings.
pub fn interactive_reference() -> String {
    let mut s = String::from(
        "# Interactive use\n\n## Slash commands\n\nBuilt in; commands, skills (`/skill:<name>`) and agents you add are listed in `mu://state/context.md`.\n\n",
    );
    for c in &BUILTIN {
        let arg = if c.needs_arg { " <arg>" } else { "" };
        s.push_str(&format!("- `/{}{arg}`: {}\n", c.name, c.desc));
    }
    s.push_str("\n### `/mcp` subcommands\n\n");
    for (name, desc) in MCP_SUBCOMMANDS {
        s.push_str(&format!("- `/mcp {name}`: {desc}\n"));
    }
    s.push_str("\n## Key bindings\n\n```\n");
    s.push_str(HOTKEYS);
    s.push_str("\n```\n");
    s
}

/// The completion list of the composer: the built-in commands with their arguments, then the
/// commands and skills that come from the context.
pub(crate) fn command_entries(ctx: &Context, cfg: &Configs, custom_ids: &[String]) -> Vec<Entry> {
    let cc = CompleteCtx { ctx, cfg, custom_ids };
    let mut commands: Vec<Entry> = BUILTIN
        .iter()
        .map(|c| {
            let (args, arg_descs) = c.complete.map(|f| f(&cc)).unwrap_or_default();
            Entry {
                name: c.name.into(),
                desc: c.desc.into(),
                needs_arg: c.needs_arg,
                args,
                arg_descs,
                paths: c.paths,
                sub: vec![],
            }
        })
        .collect();
    commands.extend(ctx.commands.iter().map(|c| Entry {
        name: c.name.clone(),
        desc: c.description.clone(),
        needs_arg: false,
        args: vec![],
        arg_descs: vec![],
        paths: false,
        sub: vec![],
    }));
    commands.extend(ctx.modes.iter().filter(|m| m.command && find(&m.name).is_none()).map(|m| Entry {
        name: m.name.clone(),
        desc: m.description.clone(),
        needs_arg: false,
        args: vec![],
        arg_descs: vec![],
        paths: false,
        sub: vec![],
    }));
    commands.extend(ctx.skills.iter().map(|s| Entry {
        name: format!("skill:{}", s.name),
        desc: s.description.clone(),
        needs_arg: false,
        args: vec![],
        arg_descs: vec![],
        paths: false,
        sub: vec![],
    }));
    commands
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for c in &BUILTIN {
            assert!(seen.insert(c.name), "{} is listed twice", c.name);
        }
    }

    #[test]
    fn commands_that_work_bare_do_not_wait_for_an_argument() {
        for name in ["permissions", "mcp", "model", "jobs", "copy", "theme", "open"] {
            assert!(!find(name).unwrap().needs_arg, "/{name} runs without an argument");
        }
        for name in ["rename", "image", "upload", "schedule"] {
            assert!(find(name).unwrap().needs_arg, "/{name} needs an argument");
        }
    }

    #[test]
    fn the_commands_that_run_during_a_turn() {
        let busy: Vec<&str> = BUILTIN.iter().filter(|c| c.busy_ok).map(|c| c.name).collect();
        assert_eq!(
            busy,
            [
                "detach",
                "reconnect",
                "thinking",
                "permissions",
                "compact",
                "refresh",
                "session",
                "todo",
                "skills",
                "plugins",
                "theme",
                "copy",
                "reply",
                "open",
                "image",
                "upload",
                "jobs",
                "agents",
                "remote",
                "schedule"
            ]
        );
    }

    #[test]
    fn the_reference_lists_every_command() {
        let r = interactive_reference();
        for c in &BUILTIN {
            assert!(r.contains(&format!("`/{}", c.name)), "/{} is missing", c.name);
        }
    }
}
