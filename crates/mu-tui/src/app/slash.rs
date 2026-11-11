//! What the composer submits: prompts, `!` shell commands, slash commands and their completion
//! list, plus the copy and todo commands.

use super::*;
use crate::term::clipboard;
use crate::text::markdown;
use mu_base::config::Configs;
use mu_base::image;
use mu_base::models::Thinking;
use mu_base::permissions::PermissionMode;
use mu_base::roles::{self, RoleView};
use mu_provider::auth::chatgpt::PROVIDER_ID;
use mu_provider::provider::providers;
use mu_tools::tools::todo::Status;

const BUILTIN_DESC: [(&str, &str, bool); 32] = [
    ("exit", "Exit mu", false),
    ("quit", "Exit mu", false),
    ("new", "Start a new session", false),
    ("resume", "Resume a session from this directory", false),
    ("fork", "Continue in a copy of this session; the original stays as it is", false),
    ("rewind", "Go back to before an earlier prompt, or to after an earlier reply", false),
    ("model", "Edit saved model roles [@role]", false),
    ("switch", "Switch this session's model", false),
    ("modelset", "Switch model sets [name | new | delete | rename | import]", false),
    ("login", "Sign in with ChatGPT, add an API key, or log in to a provider or MCP server", false),
    ("logout", "Remove a stored API key, or sign out of ChatGPT", false),
    ("web", "Choose the web search backend", false),
    ("thinking", "Set the thinking level", false),
    ("permissions", "Show or set tool permissions: ask, edits or yolo", true),
    ("compact", "Compact the context [focus]", false),
    ("plan", "Toggle plan mode", false),
    ("review", "Review changes against the approved plan [focus]", false),
    ("session", "Show the session file and title", false),
    ("rename", "Rename this session", true),
    ("todo", "Show the todo list", false),
    ("skills", "List skills", false),
    ("plugins", "List plugins", false),
    ("mcp", "Manage MCP servers (picker; or add, import, remove, enable, test…)", true),
    ("hotkeys", "Show key bindings", false),
    ("copy", "Copy a reply, code block or command", false),
    ("reply", "Quote parts of the last reply into the composer", false),
    ("image", "Attach an image file", true),
    ("jobs", "Show background jobs and other mu sessions", false),
    ("schedule", "Send a prompt later [<delay> <prompt> | list | cancel [N|all]]", true),
    ("plans", "Approve, refine or drop a parked plan", false),
    ("memory", "Show and edit memory: files, notes and history; revert a commit", false),
    ("deliver", "Implement, review and commit a parked plan in a worktree [id]", false),
];

/// `/mcp` subcommands for the completion menu, with what each does. The names that need a server
/// open the server picker when typed bare.
const MCP_SUBCOMMANDS: [(&str, &str); 12] = [
    ("add", "Add a server: discovered, popular or custom"),
    ("import", "Copy servers other tools have configured"),
    ("list", "Show servers and their state"),
    ("enable", "Turn a server on"),
    ("disable", "Turn a server off"),
    ("remove", "Delete a server from the config"),
    ("test", "Reconnect a server and list its tools"),
    ("reconnect", "Restart a server and refresh its tools"),
    ("reload", "Re-read the config files"),
    ("trust", "Start this project's MCP servers"),
    ("untrust", "Stop trusting this project's MCP servers"),
    ("help", "Show the syntax"),
];

pub(super) const HOTKEYS: &str = "Enter  submit (steer while a turn runs)      Ctrl+Q  queue follow-up      Shift+↑  edit last queued
Shift+Enter / Ctrl+Enter / Ctrl+J / Alt+Enter  newline      Esc  abort turn · leave a pty command (kills it)      Ctrl+C  twice exits mu, from any state
Shift+Tab  cycle thinking      Ctrl+T  toggle thinking      Ctrl+O  expand jobs and output      Alt+M  model roles      Alt+J  background jobs      Alt+↓/↑  mark a running job (then k cancels it, Esc unmarks)      Alt+K  cancel the marked or only job      Ctrl+R  quote parts of a reply
Ctrl+G  edit in $EDITOR      Ctrl+V  paste clipboard image      Tab  accept suggestion / complete /command      →  accept suggestion      ↑/↓  history      Ctrl+A/E/K/U/W  line editing
!cmd  run shell command and send output      !!cmd  run without sending";

/// `mu://reference/interactive.md`: the built-in slash commands and the key bindings.
pub fn interactive_reference() -> String {
    let mut s = String::from(
        "# Interactive use\n\n## Slash commands\n\nBuilt in; commands, skills (`/skill:<name>`) and agents you add are listed in `mu://state/context.md`.\n\n",
    );
    for (name, desc, needs_arg) in BUILTIN_DESC {
        let arg = if needs_arg { " <arg>" } else { "" };
        s.push_str(&format!("- `/{name}{arg}`: {desc}\n"));
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

pub(super) fn role_views(cfg: &Configs, ctx: &Context) -> Vec<RoleView> {
    let agents: Vec<(&str, &[String])> = ctx.agents.iter().map(|a| (a.name.as_str(), a.model.as_slice())).collect();
    roles::views(cfg, &agents)
}

/// The custom endpoint ids in `providers.toml`, for completion. A file that does not read is its error.
pub(super) fn custom_completion_ids(providers: &Providers) -> Result<Vec<String>, String> {
    providers.custom_registry().snapshot().map(|c| c.into_keys().collect()).map_err(|e| format!("{e:#}"))
}

/// The completion list of the composer: the built-in commands, `/mcp` subcommands, the login and
/// logout targets (with the custom endpoint ids), then the commands and skills that come from the context.
pub(super) fn command_entries(plugins: &PluginHost, ctx: &Context, cfg: &Configs, custom_ids: &[String]) -> Vec<Entry> {
    let mut commands: Vec<Entry> = BUILTIN_DESC
        .iter()
        .map(|&(name, desc, needs_arg)| {
            let (args, arg_descs): (Vec<String>, Vec<String>) = match name {
                "thinking" => (Thinking::NAMES.iter().map(|s| s.to_string()).collect(), vec![]),
                "permissions" => (PermissionMode::NAMES.iter().map(|s| s.to_string()).collect(), vec![]),
                "copy" => (["code", "cmd", "link"].map(String::from).to_vec(), vec![]),
                "jobs" => (vec!["kill".to_string()], vec![]),
                "schedule" => (vec!["list".to_string(), "cancel".to_string()], vec![]),
                "model" => role_views(cfg, ctx).into_iter().map(|r| (format!("@{}", r.name), r.row())).unzip(),
                "modelset" => {
                    let subs = [
                        ("new", "Copy the active set into a new one and switch to it"),
                        ("delete", "Remove a set"),
                        ("rename", "Rename a set"),
                        ("import", "Copy omp's model config"),
                    ];
                    let sets = cfg.set_names();
                    let args = subs.iter().map(|s| s.0.to_string()).chain(sets.iter().cloned()).collect();
                    let descs = subs
                        .iter()
                        .map(|s| s.1.to_string())
                        .chain(sets.iter().map(|_| "Switch to this set".to_string()))
                        .collect();
                    (args, descs)
                }
                "mcp" => MCP_SUBCOMMANDS.iter().map(|&(a, d)| (a.to_string(), d.to_string())).unzip(),
                "login" => (
                    std::iter::once(PROVIDER_ID.to_string())
                        .chain(std::iter::once("custom".to_string()))
                        .chain(providers::api_key_defs().map(|d| d.id.to_string()))
                        .chain(custom_ids.iter().cloned())
                        .chain(plugins.mcp_names().iter().map(|n| format!("mcp:{n}")))
                        .collect(),
                    vec![],
                ),
                "logout" => (
                    std::iter::once(PROVIDER_ID.to_string())
                        .chain(providers::api_key_defs().map(|d| d.id.to_string()))
                        .chain(custom_ids.iter().cloned())
                        .collect(),
                    vec![],
                ),
                _ => (vec![], vec![]),
            };
            Entry { name: name.into(), desc: desc.into(), needs_arg, args, arg_descs }
        })
        .collect();
    commands.extend(ctx.commands.iter().map(|c| Entry {
        name: c.name.clone(),
        desc: c.description.clone(),
        needs_arg: false,
        args: vec![],
        arg_descs: vec![],
    }));
    commands.extend(ctx.skills.iter().map(|s| Entry {
        name: format!("skill:{}", s.name),
        desc: s.description.clone(),
        needs_arg: false,
        args: vec![],
        arg_descs: vec![],
    }));
    commands
}

impl App {
    /// Rebuilds the completion list, which names the model sets and the custom endpoints.
    pub(super) fn refresh_commands(&mut self) {
        let ids = match custom_completion_ids(&self.services.providers) {
            Ok(ids) => ids,
            Err(e) => {
                self.error(&e);
                Vec::new()
            }
        };
        self.prompt.commands = command_entries(&self.plugins, &self.ctx, &self.services.providers.cfg, &ids);
    }

    pub(super) fn submit(&mut self, mut input: UserInput, follow_up: bool) {
        if self.flows.mcp_wizard.is_some() && input.images.is_empty() {
            let line = input.text.trim().to_string();
            return self.mcp_wizard_line(&line);
        }
        input.text = input.text.trim().to_string();
        if input.text.is_empty() && input.images.is_empty() {
            return;
        }
        if input.images.is_empty() {
            let t = input.text.as_str();
            if !t.starts_with(['/', '!']) && self.agent.status.model_id.is_empty() {
                self.notice("no model chosen: sign in with /login if you have not, then pick one with /switch");
                return self.restore_queued(vec![input]);
            }
            if let Some(cmd) = t.strip_prefix("!!") {
                return self.user_bash(cmd.trim(), false);
            }
            if let Some(cmd) = t.strip_prefix('!') {
                return self.user_bash(cmd.trim(), true);
            }
            if let Some(cmd) = t.strip_prefix('/') {
                return self.slash(cmd);
            }
            let t = t.to_string();
            self.observe_prompt(&t);
        }
        self.send_prompt(input, follow_up);
    }

    pub(super) fn model_cmd(&mut self, arg: &str) {
        if self.agent.running {
            return self.notice("busy — Esc to abort first");
        }
        if arg.is_empty() {
            self.open_model_roles(None);
        } else if let Some(role) = arg
            .strip_prefix('@')
            .filter(|r| !r.is_empty() && !r.contains(char::is_whitespace) && !r.contains(':') && !r.starts_with('@'))
        {
            self.open_model_roles(Some(role.to_string()));
        } else {
            self.error("usage: /model [@role]; use /switch <provider/model or @role> to change this session");
        }
    }

    fn user_bash(&mut self, cmd: &str, include: bool) {
        if self.agent.running {
            return self.notice("busy — Esc to abort first");
        }
        if cmd.is_empty() {
            return;
        }
        self.agent.running = true;
        self.send(Command::Bash { cmd: cmd.into(), include });
    }

    pub(super) fn send_prompt(&mut self, input: impl Into<UserInput>, follow_up: bool) {
        let input = input.into();
        if self.agent.running {
            self.send(if follow_up { Command::FollowUp(input) } else { Command::Steer(input) });
        } else {
            self.agent.running = true;
            self.send(Command::Prompt(input));
        }
    }

    fn observe_prompt(&self, text: &str) {
        if !self.services.providers.cfg.mu.predict.enabled {
            return;
        }
        let params = json!({ "text": text, "cwd": self.cwd });
        let plugins = self.plugins.clone();
        tokio::task::spawn_local(async move {
            let _ = plugins.request("predict", "predict/observe", params, Duration::from_secs(2)).await;
        });
    }

    fn slash(&mut self, cmd: &str) {
        let (name, arg) = cmd.split_once(char::is_whitespace).map_or((cmd, ""), |(n, a)| (n, a.trim()));
        let allowed_busy = matches!(
            name,
            "thinking"
                | "permissions"
                | "todo"
                | "skills"
                | "plugins"
                | "session"
                | "copy"
                | "reply"
                | "image"
                | "jobs"
                | "schedule"
        );
        if self.agent.running && !allowed_busy {
            return self.notice("busy — Esc to abort first");
        }
        match name {
            "exit" | "quit" => self.quit(),
            "new" => {
                self.agent.todo.clear();
                self.send(Command::NewSession);
                self.commit(vec![styled("── new session".to_string(), |_| dim())]);
            }
            "resume" => self.open_session_picker(),
            "fork" => self.send(Command::Fork),
            "rewind" => self.send(Command::ListRewind),
            "model" => self.model_cmd(arg),
            "switch" if arg.is_empty() => self.request_models(),
            "switch" => self.send(Command::SetModel(arg.into())),
            "login" => self.login(arg),
            "logout" => self.logout_cmd(arg),
            "web" => self.start_web_wizard(None),
            "modelset" => self.modelset_cmd(arg),
            "mcp" => self.mcp_cmd(arg),
            "thinking" => match Thinking::parse(arg) {
                Some(t) => self.send(Command::SetThinking(t)),
                None => self.error("usage: /thinking off|low|medium|high|xhigh|max"),
            },
            "permissions" if arg.is_empty() => {
                self.notice(&format!("permissions: {} (ask | edits | yolo)", self.agent.status.permissions.as_str()))
            }
            "permissions" => match PermissionMode::parse(arg) {
                Some(m) => self.send(Command::SetPermissionMode(m)),
                None => self.error("usage: /permissions ask|edits|yolo"),
            },
            "plan" => self.send(Command::TogglePlan((!arg.is_empty()).then(|| arg.to_string()))),
            "plans" => self.send(Command::ListPlans),
            "deliver" if arg.is_empty() => {
                self.flows.pick_deliver = true;
                self.send(Command::ListPlans);
            }
            "deliver" => self.send(Command::Deliver(arg.into())),
            "compact" => {
                self.agent.running = true;
                self.send(Command::Compact((!arg.is_empty()).then(|| arg.to_string())));
            }
            "review" => {
                self.agent.running = true;
                self.send(Command::Review((!arg.is_empty()).then(|| arg.to_string())));
            }
            "session" => {
                let p = self.agent.status.session_path.clone().unwrap_or_else(|| "no session yet".into());
                let t = self.agent.status.title.clone().unwrap_or_else(|| "(untitled)".into());
                let cache = self
                    .agent
                    .status
                    .cache
                    .as_ref()
                    .map(|c| format!("\n{}", crate::status::cache_summary(c, mu_base::message::now_ms())))
                    .unwrap_or_default();
                self.notice(&format!("{p}\n{t}{cache}"));
            }
            "rename" if !arg.is_empty() => self.send(Command::Rename(arg.into())),
            "rename" => self.error("usage: /rename <title>"),
            "todo" => self.show_todo(),
            "skills" => {
                let s: Vec<String> = self
                    .ctx
                    .skills
                    .iter()
                    .map(|s| format!("{}{}: {}", s.name, if s.hidden { " (hidden)" } else { "" }, s.description))
                    .collect();
                self.notice(&if s.is_empty() { "no skills".into() } else { s.join("\n") });
            }
            "plugins" => {
                let p: Vec<String> = self
                    .plugins
                    .status()
                    .into_iter()
                    .map(|(n, l, up, extra)| {
                        let extra = if extra.is_empty() { extra } else { format!(" · {extra}") };
                        format!("{n} · {l} · {}{extra}", if up { "running" } else { "stopped" })
                    })
                    .collect();
                self.notice(&if p.is_empty() { "no plugins".into() } else { p.join("\n") });
            }
            "hotkeys" => self.notice(HOTKEYS),
            "copy" => self.copy_cmd(arg),
            "reply" => self.open_reply(),
            "image" => self.image_cmd(arg),
            "jobs" => self.jobs_cmd(arg),
            "schedule" => self.schedule_cmd(arg),
            "memory" => self.open_memory(),
            _ => {
                let typed = |text: String| UserInput { text, images: Vec::new(), command: Some(format!("/{cmd}")) };
                if let Some(sk) = name.strip_prefix("skill:") {
                    match self.ctx.skill_prompt(sk, arg) {
                        Some(p) => self.send_prompt(typed(p), false),
                        None => self.error(&format!("unknown skill {sk}")),
                    }
                } else if let Some(c) = self.ctx.commands.iter().find(|c| c.name == name) {
                    let p = c.expand(arg);
                    self.send_prompt(typed(p), false);
                } else {
                    self.error(&format!("unknown command /{name}"));
                }
            }
        }
    }

    fn jobs_cmd(&mut self, arg: &str) {
        let mut words = arg.split_whitespace();
        match (words.next(), words.next(), words.next()) {
            (None, ..) => self.open_jobs(),
            (Some("kill"), Some("all"), None) => self.send(Command::KillJobs(None)),
            (Some("kill"), Some(id), None) => self.send(Command::KillJobs(Some(id.into()))),
            _ => self.error("usage: /jobs [kill <id|all>]"),
        }
    }

    fn image_cmd(&mut self, arg: &str) {
        if arg.is_empty() {
            return self.error("usage: /image <path>");
        }
        let path = match arg.strip_prefix("~/") {
            Some(rest) => PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(rest),
            None => self.cwd.join(arg),
        };
        match image::load(&path) {
            Ok(img) => self.prompt.composer.attach_image(img),
            Err(e) => self.error(&format!("image: {e}")),
        }
    }

    fn copy_cmd(&mut self, arg: &str) {
        match arg {
            "" => self.open_copy_picker(),
            "code" => {
                let t = self.agent.tr.replies.iter().rev().find_map(|r| markdown::code_blocks(r).pop().map(|(_, b)| b));
                self.copy_text(t, "No code block to copy.");
            }
            "cmd" | "command" => {
                let t = self.agent.bash_cmds.last().cloned();
                self.copy_text(t, "No command to copy.");
            }
            "link" | "url" => {
                let t = self.agent.tr.replies.iter().rev().find_map(|r| markdown::links(r).pop());
                self.copy_text(t, "No link to copy.");
            }
            _ => self.error("usage: /copy [code|cmd|link]"),
        }
    }

    pub(super) fn copy_text(&mut self, text: Option<String>, none: &str) {
        let Some(t) = text else { return self.notice(none) };
        let done = format!("copied {} chars", t.chars().count());
        self.copy_async(t, done);
    }

    /// Copies on a blocking thread with a 2 s limit, so a stuck clipboard helper cannot freeze the UI.
    pub(super) fn copy_async(&mut self, text: String, done: String) {
        let tx = self.ui_tx.clone();
        tokio::task::spawn_local(async move {
            let r = match tokio::time::timeout(
                Duration::from_secs(2),
                tokio::task::spawn_blocking(move || clipboard::copy(&text)),
            )
            .await
            {
                Ok(Ok(r)) => r,
                Ok(Err(e)) => Err(e.to_string()),
                Err(_) => Err("the clipboard helper did not answer in 2 s".into()),
            };
            let _ = tx.send(UiMsg::Copied(r.map(|()| done)));
        });
    }

    fn show_todo(&mut self) {
        if self.agent.todo.iter().all(|p| p.tasks.is_empty()) {
            return self.notice("todo list is empty");
        }
        let mut out = Vec::new();
        for p in &self.agent.todo {
            out.push(p.name.clone());
            for t in &p.tasks {
                let mark = match t.status {
                    Status::Completed => "[x]",
                    Status::InProgress => "[/]",
                    Status::Abandoned => "[-]",
                    Status::Blocked => "[!]",
                    Status::Pending => "[ ]",
                };
                out.push(format!("  {mark} {}", t.content));
            }
        }
        self.notice(&out.join("\n"));
    }
}
