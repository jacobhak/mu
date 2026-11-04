//! What the composer submits: prompts, `!` shell commands, slash commands and their completion
//! list, plus the copy and todo commands.

use super::*;
use crate::term::clipboard;
use crate::text::markdown;
use mu_base::image;
use mu_base::models::{self, Thinking};
use mu_tools::tools::todo::Status;

const BUILTIN_DESC: [(&str, &str, bool); 25] = [
    ("exit", "Exit mu", false),
    ("quit", "Exit mu", false),
    ("new", "Start a new session", false),
    ("resume", "Resume a session from this directory", false),
    ("model", "Pick a model and assign a role", false),
    ("switch", "Switch this session's model", false),
    ("login", "Add an API key, or log in to a provider or MCP server", false),
    ("logout", "Remove a stored API key", false),
    ("thinking", "Set the thinking level", false),
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
    ("plans", "Approve, refine or drop a parked plan", false),
    ("deliver", "Implement, review and commit a parked plan in a worktree [id]", false),
];

/// `/mcp` subcommands for the completion menu, with what each does. The names that need a server
/// open the server picker when typed bare.
const MCP_SUBCOMMANDS: [(&str, &str); 10] = [
    ("add", "Add a server: discovered, popular or custom"),
    ("import", "Copy servers other tools have configured"),
    ("list", "Show servers and their state"),
    ("enable", "Turn a server on"),
    ("disable", "Turn a server off"),
    ("remove", "Delete a server from the config"),
    ("test", "Reconnect a server and list its tools"),
    ("reconnect", "Restart a server and refresh its tools"),
    ("reload", "Re-read the config files"),
    ("help", "Show the syntax"),
];

pub(super) const HOTKEYS: &str = "Enter  submit (steer while a turn runs)      Ctrl+Q  queue follow-up      Alt+↑  edit last queued
Shift+Enter / Ctrl+J / Alt+Enter  newline      Esc  abort turn (in a pty command: sent to it)      Ctrl+C  clear · abort · exit (twice)
Shift+Tab  cycle thinking      Ctrl+T  toggle thinking      Ctrl+O  expand output      Alt+M  model picker      Alt+J  background jobs      Ctrl+R  quote parts of a reply
Ctrl+G  edit in $EDITOR      Ctrl+V  paste clipboard image      Tab  accept suggestion / complete /command      →  accept suggestion      ↑/↓  history      Ctrl+A/E/K/U/W  line editing
!cmd  run shell command and send output      !!cmd  run without sending";

/// The completion list of the composer: the built-in commands, `/mcp` subcommands, the login and
/// logout targets, then the commands and skills that come from the context.
pub(super) fn command_entries(plugins: &PluginHost, ctx: &Context) -> Vec<Entry> {
    let mut commands: Vec<Entry> = BUILTIN_DESC
        .iter()
        .map(|&(name, desc, needs_arg)| {
            let (args, arg_descs): (Vec<String>, Vec<String>) = match name {
                "thinking" => (Thinking::NAMES.iter().map(|s| s.to_string()).collect(), vec![]),
                "copy" => (["code", "cmd", "link"].map(String::from).to_vec(), vec![]),
                "jobs" => (vec!["kill".to_string()], vec![]),
                "mcp" => MCP_SUBCOMMANDS.iter().map(|&(a, d)| (a.to_string(), d.to_string())).unzip(),
                "login" => (
                    models::PROVIDERS
                        .iter()
                        .map(|d| d.id.to_string())
                        .chain(plugins.mcp_names().iter().map(|n| format!("mcp:{n}")))
                        .collect(),
                    vec![],
                ),
                "logout" => (models::PROVIDERS.iter().map(|d| d.id.to_string()).collect(), vec![]),
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
            let mut ib = self.link.inbox.borrow_mut();
            if follow_up { ib.follow_up.push_back(input) } else { ib.steer.push_back(input) }
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
            "thinking" | "model" | "todo" | "skills" | "plugins" | "session" | "copy" | "reply" | "image" | "jobs"
        ) && !(name == "model" && arg.is_empty());
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
            "model" | "switch" if arg.is_empty() => {
                self.flows.pick_assign = name == "model";
                self.request_models();
            }
            "model" | "switch" => self.send(Command::SetModel(arg.into())),
            "login" => self.login(arg),
            "logout" => self.logout_cmd(arg),
            "mcp" => self.mcp_cmd(arg),
            "thinking" => match Thinking::parse(arg) {
                Some(t) => self.send(Command::SetThinking(t)),
                None => self.error("usage: /thinking off|low|medium|high|xhigh|max"),
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
                self.notice(&format!("{p}\n{t}"));
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
            _ => {
                if let Some(sk) = name.strip_prefix("skill:") {
                    match self.ctx.skill_prompt(sk, arg) {
                        Some(p) => self.send_prompt(p, false),
                        None => self.error(&format!("unknown skill {sk}")),
                    }
                } else if let Some(c) = self.ctx.commands.iter().find(|c| c.name == name) {
                    let p = c.expand(arg);
                    self.send_prompt(p, false);
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
