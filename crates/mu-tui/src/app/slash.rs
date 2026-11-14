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
use mu_tools::tools::todo::Status;

pub(super) const HOTKEYS: &str = "Enter  submit (steer while a turn runs)      Ctrl+Q  queue follow-up      Shift+↑  edit last queued
Shift+Enter / Ctrl+Enter / Ctrl+J / Alt+Enter  newline      Esc  abort turn · leave a pty command (kills it)      Ctrl+C  twice exits mu, from any state
Shift+Tab  cycle thinking      Ctrl+T  toggle thinking      Ctrl+O  expand jobs and output      Alt+M  model roles      Alt+J  background jobs      Alt+↓/↑  mark a running job (then k cancels it, Esc unmarks)      Alt+K  cancel the marked or only job      Ctrl+R  quote parts of a reply
Ctrl+G  edit in $EDITOR      Ctrl+V  paste clipboard image      Tab  accept suggestion / complete /command      →  accept suggestion      ↑/↓  history      Ctrl+A/E/K/U/W  line editing
!cmd  run shell command and send output      !!cmd  run without sending";

pub(super) fn role_views(cfg: &Configs, ctx: &Context) -> Vec<RoleView> {
    let agents: Vec<(&str, &[String])> = ctx.agents.iter().map(|a| (a.name.as_str(), a.model.as_slice())).collect();
    roles::views(cfg, &agents)
}

/// The custom endpoint ids in `providers.toml`, for completion. A file that does not read is its error.
pub(super) fn custom_completion_ids(providers: &Providers) -> Result<Vec<String>, String> {
    providers.custom_registry().snapshot().map(|c| c.into_keys().collect()).map_err(|e| format!("{e:#}"))
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
        if self.flows.mcp().is_some() && input.images.is_empty() {
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
        if let Some(b) = commands::find(name) {
            if self.agent.running && !b.busy_ok {
                return self.notice("busy — Esc to abort first");
            }
            return (b.run)(self, arg);
        }
        if self.agent.running {
            return self.notice("busy — Esc to abort first");
        }
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

    pub(super) fn cmd_new(&mut self, _: &str) {
        self.agent.todo.clear();
        self.send(Command::NewSession);
        self.commit(vec![styled("── new session".to_string(), |_| dim())]);
    }

    pub(super) fn cmd_switch(&mut self, arg: &str) {
        if arg.is_empty() {
            self.request_models();
        } else {
            self.send(Command::SetModel(arg.into()));
        }
    }

    pub(super) fn cmd_thinking(&mut self, arg: &str) {
        match Thinking::parse(arg) {
            Some(t) => self.send(Command::SetThinking(t)),
            None => self.error("usage: /thinking off|low|medium|high|xhigh|max"),
        }
    }

    pub(super) fn cmd_permissions(&mut self, arg: &str) {
        if arg.is_empty() {
            return self
                .notice(&format!("permissions: {} (ask | edits | yolo)", self.agent.status.permissions.as_str()));
        }
        match PermissionMode::parse(arg) {
            Some(m) => self.send(Command::SetPermissionMode(m)),
            None => self.error("usage: /permissions ask|edits|yolo"),
        }
    }

    pub(super) fn cmd_deliver(&mut self, arg: &str) {
        if arg.is_empty() {
            self.flows.pick_deliver = true;
            self.send(Command::ListPlans);
        } else {
            self.send(Command::Deliver(arg.into()));
        }
    }

    pub(super) fn cmd_compact(&mut self, arg: &str) {
        self.agent.running = true;
        self.send(Command::Compact((!arg.is_empty()).then(|| arg.to_string())));
    }

    pub(super) fn cmd_review(&mut self, arg: &str) {
        self.agent.running = true;
        self.send(Command::Review((!arg.is_empty()).then(|| arg.to_string())));
    }

    pub(super) fn cmd_refresh(&mut self, arg: &str) {
        if !(arg.is_empty() || arg == "status") {
            return self.error("usage: /refresh [status]");
        }
        self.send(Command::RefreshStatus);
        if self.agent.running {
            self.notice("status refresh queued until the current turn finishes");
        }
    }

    pub(super) fn cmd_session(&mut self, _: &str) {
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

    pub(super) fn cmd_rename(&mut self, arg: &str) {
        if arg.is_empty() {
            self.error("usage: /rename <title>");
        } else {
            self.send(Command::Rename(arg.into()));
        }
    }

    pub(super) fn cmd_skills(&mut self, _: &str) {
        let s: Vec<String> = self
            .ctx
            .skills
            .iter()
            .map(|s| format!("{}{}: {}", s.name, if s.hidden { " (hidden)" } else { "" }, s.description))
            .collect();
        self.notice(&if s.is_empty() { "no skills".into() } else { s.join("\n") });
    }

    pub(super) fn cmd_plugins(&mut self, _: &str) {
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

    pub(super) fn jobs_cmd(&mut self, arg: &str) {
        let mut words = arg.split_whitespace();
        match (words.next(), words.next(), words.next()) {
            (None, ..) => self.open_jobs(),
            (Some("kill"), Some("all"), None) => self.send(Command::KillJobs(None)),
            (Some("kill"), Some(id), None) => self.send(Command::KillJobs(Some(id.into()))),
            _ => self.error("usage: /jobs [kill <id|all>]"),
        }
    }

    pub(super) fn image_cmd(&mut self, arg: &str) {
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

    pub(super) fn copy_cmd(&mut self, arg: &str) {
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

    pub(super) fn show_todo(&mut self) {
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
