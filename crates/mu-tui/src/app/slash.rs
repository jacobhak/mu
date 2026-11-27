//! What the composer submits: prompts, `!` shell commands, slash commands and their completion
//! list, plus the copy and todo commands.

use super::*;
use crate::host::sessions::{self, AttachTarget, Live, LiveArgs, LiveRow};
use crate::term::clipboard;
use crate::text::markdown;
use crate::widgets::complete::SubArgs;
use crate::widgets::overlay::OverlayEvent;
use mu_base::image;
use mu_base::models::Thinking;
use mu_base::permissions::PermissionMode;
use mu_common::text::sh_quote as quote;
use mu_provider::provider::client::Providers;
use mu_tools::tools::todo::Status;

pub(super) const HOTKEYS: &str = "Enter  submit (steer while a turn runs)      Ctrl+Q  queue follow-up      Shift+↑  edit last queued
Shift+Enter / Ctrl+Enter / Ctrl+J / Alt+Enter  newline      Esc  abort turn · leave a pty command (kills it)      Ctrl+C  twice exits mu, from any state
Shift+Tab  cycle thinking      Ctrl+T  toggle thinking      Ctrl+O  expand jobs and output      Alt+M  model roles      Alt+J  background jobs      Alt+↓/↑  mark a running job (then k cancels it, Esc unmarks)      Alt+K  cancel the marked or only job      Ctrl+R  quote parts of a reply
Ctrl+G  edit in $EDITOR      Ctrl+V  paste clipboard image      Tab  accept suggestion / complete /command      →  accept suggestion      ↑/↓  history      Ctrl+A/E/K/U/W  line editing
!cmd  run shell command and send output      !!cmd  run without sending";

/// The custom endpoint ids in `providers.toml`, for completion. A file that does not read is its error.
pub(crate) fn custom_completion_ids(providers: &Providers) -> Result<Vec<String>, String> {
    providers.custom_registry().snapshot().map(|c| c.into_keys().collect()).map_err(|e| format!("{e:#}"))
}

impl App {
    /// Rebuilds the completion list, which names the model sets and the custom endpoints.
    pub(super) fn refresh_commands(&mut self) {
        self.host_call(host::completion::COMPLETIONS, &(), |app, r: Result<Completions, String>| match r {
            Ok(c) => {
                if let Some(e) = c.error {
                    app.error(&e);
                }
                app.prompt.commands = c.entries;
                app.prompt.dirs_asked = None;
                app.prompt.agents_asked = None;
            }
            Err(e) => app.error(&format!("cannot read the completion list: {e}")),
        });
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

    fn observe_prompt(&mut self, text: &str) {
        if self.prompt.predict {
            self.host_call(host::completion::OBSERVE, &text, |_, _: Result<(), String>| {});
        }
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
        let (shown, name, arg) = (cmd.to_string(), name.to_string(), arg.to_string());
        self.host_call(
            host::completion::EXPAND,
            &(name.clone(), arg),
            move |app, r: Result<Expanded, String>| match r {
                Ok(Expanded::Prompt(text)) => {
                    app.send_prompt(UserInput { text, images: Vec::new(), command: Some(format!("/{shown}")) }, false)
                }
                Ok(Expanded::Mode { name, prompt }) => app.send(Command::Mode { name, prompt }),
                Ok(Expanded::UnknownSkill) => {
                    app.error(&format!("unknown skill {}", name.trim_start_matches("skill:")))
                }
                Ok(Expanded::UnknownCommand) => app.error(&format!("unknown command /{name}")),
                Err(e) => app.error(&e),
            },
        );
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

    /// `/theme` opens the picker; `/theme <name>` switches to that theme at once. Either way the
    /// choice is saved as `[theme] source`.
    pub(super) fn cmd_theme(&mut self, arg: &str) {
        if arg.is_empty() {
            self.open_overlay(Overlay::Theme { st: crate::panels::theme_picker::ThemePicker::new() });
        } else {
            self.apply_theme(arg);
        }
    }

    pub(super) fn apply_theme(&mut self, source: &str) {
        let live = match crate::theme::switch(source) {
            Ok(()) => true,
            // The terminal's colours cannot be read while running; the next start reads them.
            Err(_) if source == "terminal" => false,
            Err(e) => return self.error(&e),
        };
        self.screen.dirty = true;
        match mu_base::config::save_theme_source(source) {
            Ok(()) if live => {
                self.notice(&format!("theme {source}, saved to config.toml; earlier output keeps its colours"))
            }
            Ok(()) => self
                .notice("theme terminal saved to config.toml; it follows the terminal's colours from the next start"),
            Err(e) => self.error(&format!("theme {source} is on for this session, but not saved: {e}")),
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

    /// `/retry` re-runs the last request. The agent starts the turn, or says there is nothing to
    /// retry, so nothing here marks the composer busy.
    pub(super) fn cmd_retry(&mut self, _: &str) {
        self.send(Command::Retry);
    }

    pub(super) fn cmd_compact(&mut self, arg: &str) {
        if self.agent.running {
            self.notice("compaction queued until the current turn finishes");
        }
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
        let local = self.agent.status.local_dir.as_ref().map(|d| format!("\nlocal://  {d}")).unwrap_or_default();
        self.notice(&format!("{p}\n{t}{local}{cache}"));
    }

    pub(super) fn cmd_rename(&mut self, arg: &str) {
        if arg.is_empty() {
            self.error("usage: /rename <title>");
        } else {
            self.send(Command::Rename(arg.into()));
        }
    }

    pub(super) fn cmd_detach(&mut self) {
        self.host_call(host::startup::DETACH, &(), |app, r: Result<(), String>| {
            if let Err(e) = r {
                app.error(&e);
            }
        });
    }

    pub(super) fn cmd_cd(&mut self, arg: &str) {
        if arg.is_empty() {
            return self.notice(&self.cwd.display().to_string());
        }
        let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
        let path = match mu_common::paths::expand_tilde(arg, &home) {
            Some(p) => p,
            None => self.cwd.join(arg),
        };
        self.send(Command::SetCwd(path));
    }

    pub(super) fn cmd_skills(&mut self, _: &str) {
        self.host_call(host::completion::SKILLS, &(), |app, r: Result<Vec<SkillRow>, String>| match r {
            Ok(rows) => {
                let s: Vec<String> = rows
                    .iter()
                    .map(|s| format!("{}{}: {}", s.name, if s.hidden { " (hidden)" } else { "" }, s.description))
                    .collect();
                app.notice(&if s.is_empty() { "no skills".into() } else { s.join("\n") });
            }
            Err(e) => app.error(&e),
        });
    }

    pub(super) fn cmd_plugins(&mut self, _: &str) {
        self.host_call(host::completion::PLUGINS, &(), |app, r: Result<Vec<PluginRow>, String>| match r {
            Ok(rows) => {
                let p: Vec<String> = rows
                    .into_iter()
                    .map(|r| {
                        let extra = if r.extra.is_empty() { r.extra } else { format!(" · {}", r.extra) };
                        format!("{} · {} · {}{extra}", r.name, r.label, if r.running { "running" } else { "stopped" })
                    })
                    .collect();
                app.notice(&if p.is_empty() { "no plugins".into() } else { p.join("\n") });
            }
            Err(e) => app.error(&e),
        });
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

    /// `/agents`: the agents here and on the session sources, and stop, cancel, message or attach one.
    pub(super) fn agents_cmd(&mut self, arg: &str) {
        self.manage_agents(arg, false);
    }

    /// `/remote`: the agents the session sources list, the same actions on them, and `new` to start one.
    pub(super) fn remote_cmd(&mut self, arg: &str) {
        self.manage_agents(arg, true);
    }

    fn manage_agents(&mut self, arg: &str, remote: bool) {
        let cmd = if remote { "/remote" } else { "/agents" };
        let arg = arg.trim();
        let (verb, rest) = arg.split_once(char::is_whitespace).map_or((arg, ""), |(v, r)| (v, r.trim()));
        let one_word = |s: &str| !s.is_empty() && !s.contains(char::is_whitespace);
        let (verb, id, text): (&'static str, String, String) = match (verb, rest) {
            ("", _) => ("list", String::new(), String::new()),
            ("stop", id) if one_word(id) => ("stop", id.into(), String::new()),
            ("cancel", id) if one_word(id) => ("cancel", id.into(), String::new()),
            ("attach", id) if one_word(id) => ("attach", id.into(), String::new()),
            ("notify", r) => match r.split_once(char::is_whitespace) {
                Some((id, t)) if !t.trim().is_empty() => ("notify", id.into(), t.trim().into()),
                _ => return self.error(&format!("usage: {cmd} notify <id> <text>")),
            },
            ("new", r) if remote => match r.split_whitespace().next() {
                Some(name) => ("new", name.into(), r[name.len()..].trim().into()),
                None => return self.error("usage: /remote new <remote> [args…]"),
            },
            _ if remote => {
                return self.error("usage: /remote [stop|cancel|notify|attach <id> [text] | new <remote> [args…]]");
            }
            _ => return self.error("usage: /agents [stop|cancel|notify|attach <id> [text]]"),
        };
        self.host_call(sessions::LIVE_SESSIONS, &LiveArgs::default(), move |app, r: Result<Live, String>| {
            let (rows, targets): (Vec<LiveRow>, Vec<String>) = match r {
                Ok(l) => (l.rows.into_iter().filter(|r| is_listed(r, remote)).collect(), l.targets),
                Err(e) => return app.error(&format!("cannot list the agents: {e}")),
            };
            match verb {
                "list" => app.list_agents(&rows, remote.then_some(targets.as_slice())),
                "new" => app.new_remote_agent(&targets, &id, &text),
                _ => match pick_agent(&rows, &id) {
                    Ok(row) => app.act_on_agent(verb, row, text),
                    Err(e) => app.error(&e),
                },
            }
        });
    }

    /// Offers the agents of `live` as the ids `/agents` and `/remote` complete (and the remotes `/remote new`
    /// does), each with where it runs, its state, age, directory and title.
    pub(super) fn set_agent_choices(&mut self, live: Live) {
        for (name, remote) in [("agents", false), ("remote", true)] {
            let rows: Vec<&LiveRow> = live.rows.iter().filter(|r| is_listed(r, remote)).collect();
            let ids: Vec<String> = rows.iter().map(|r| r.agent_id()).collect();
            let items: Vec<String> = rows
                .iter()
                .zip(&ids)
                .map(|(r, id)| {
                    let shared = ids.iter().filter(|j| *j == id).count() > 1;
                    if shared { format!("{}:{id}", r.host_label()) } else { id.clone() }
                })
                .collect();
            let descs: Vec<String> = rows.iter().map(|r| agent_detail(r)).collect();
            let mut sub: Vec<SubArgs> = ["stop", "cancel", "notify", "attach"]
                .map(|arg| SubArgs { arg: arg.into(), items: items.clone(), descs: descs.clone() })
                .into();
            if remote {
                let descs = live.targets.iter().map(|_| "start an agent here".to_string()).collect();
                sub.push(SubArgs { arg: "new".into(), items: live.targets.clone(), descs });
            }
            if let Some(e) = self.prompt.commands.iter_mut().find(|e| e.name == name) {
                e.sub = sub;
            }
        }
        self.prompt.composer.refresh(&self.prompt.commands);
        self.screen.dirty = true;
    }

    /// Starts an agent on remote `name` (its source's `new` command gets `args` appended) and attaches to it.
    fn new_remote_agent(&mut self, targets: &[String], name: &str, args: &str) {
        if !targets.iter().any(|t| t == name) {
            let known = if targets.is_empty() { "none".to_string() } else { targets.join(", ") };
            return self.error(&format!("no remote `{name}` can start an agent (remotes that can: {known})"));
        }
        self.notice(&format!("starting an agent on {name}…"));
        let call =
            sessions::NewAgentArgs { remote: name.into(), args: args.split_whitespace().map(String::from).collect() };
        self.host_call(sessions::NEW_AGENT, &call, |app, r: Result<LiveRow, String>| match r {
            Err(e) => app.error(&format!("cannot start an agent: {e}")),
            Ok(row) => match row.attach.clone() {
                Some(command) => app.attach_session(AttachTarget {
                    command,
                    helper: row.helper.clone(),
                    state_key: row.state_key.clone(),
                }),
                None => {
                    let hint = row.hint.as_deref().map(|h| format!(": {h}")).unwrap_or_default();
                    app.notice(&format!("started {} on {}{hint}", row.agent_id(), row.host_label()));
                }
            },
        });
    }

    /// `remote` is the remotes that can start an agent, when the list is `/remote`'s.
    fn list_agents(&mut self, rows: &[LiveRow], remote: Option<&[String]>) {
        let starts = remote.map(|t| match t {
            [] => "no remote offers to start an agent".to_string(),
            t => format!("/remote new <remote> [args…] starts an agent on {}", t.join(", ")),
        });
        if rows.is_empty() {
            return match starts {
                Some(s) => self.notice(&format!("no agents on your remotes; {s}")),
                None => self.notice("no agents are running here or on your session sources"),
            };
        }
        let mut out: Vec<String> = rows
            .iter()
            .map(|r| {
                let state = if r.live.locked {
                    "locked"
                } else if r.live.waiting {
                    "waiting"
                } else {
                    "-"
                };
                let client = r.live.client.as_ref().map_or("-", |c| c.host.as_str());
                let mut keys: Vec<&str> = match r.host {
                    Some(_) => r.actions.iter().map(String::as_str).collect(),
                    None => vec!["stop", "cancel", "notify"],
                };
                if r.attach.is_some() || (r.host.is_none() && !self.host.is_remote()) {
                    keys.push("attach");
                }
                let title = r.title.as_deref().unwrap_or_default();
                let when = Some(started_ago(r)).filter(|a| !a.is_empty()).unwrap_or_else(|| "-".into());
                let cwd = r.live.cwd.as_ref().map_or_else(|| "-".to_string(), |c| c.display().to_string());
                format!(
                    "{}  {}  {state}  {client}  {when}  {cwd}  {title}  [{}]",
                    r.host_label(),
                    r.agent_id(),
                    keys.join(" ")
                )
            })
            .collect();
        out.push(match starts {
            Some(s) => format!("/remote stop|cancel|notify|attach <id>; {s}"),
            None => "/agents stop|cancel|notify|attach <id>; the Sessions tab (/jobs, Tab) does the same".into(),
        });
        self.notice(&out.join("\n"));
    }

    /// Runs `verb` on `row` through the panel's handlers, whose replies show as a notice here.
    fn act_on_agent(&mut self, verb: &'static str, row: &LiveRow, text: String) {
        let id = row.agent_id();
        let Some(host) = row.host.clone() else {
            let (session, pid, started) = (row.live.path.clone(), row.live.pid, row.live.started.clone());
            return match verb {
                "stop" => self.on_overlay_event(OverlayEvent::TerminateSession { session, pid, started }),
                "cancel" => {
                    self.on_overlay_event(OverlayEvent::StopSession { session, pid, started, kind: "agent".into() })
                }
                "notify" => self.on_overlay_event(OverlayEvent::NotifySession { session, text }),
                _ if self.host.is_remote() => {
                    self.error("that agent runs on the machine this client is attached to; attach to it from there")
                }
                _ => match mu_common::paths::self_exe() {
                    Ok(exe) => self.attach_session(AttachTarget {
                        command: format!("{} attach --stdio {}", quote(&exe.to_string_lossy()), quote(&id)),
                        helper: None,
                        state_key: None,
                    }),
                    Err(e) => self.error(&format!("cannot locate mu: {e}")),
                },
            };
        };
        if verb == "attach" {
            return match &row.attach {
                Some(command) => self.attach_session(AttachTarget {
                    command: command.clone(),
                    helper: row.helper.clone(),
                    state_key: row.state_key.clone(),
                }),
                None => self.error(&format!("{host} offers no attach for its agents")),
            };
        }
        if !row.actions.iter().any(|a| a == verb) {
            return self.error(&format!("{host} offers no {verb} for its agents"));
        }
        let text = (verb == "notify").then_some(text);
        self.on_overlay_event(OverlayEvent::RemoteAction { host, id, action: verb, text });
    }

    pub(super) fn image_cmd(&mut self, arg: &str) {
        if arg.is_empty() {
            return self.error("usage: /image <path>");
        }
        let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
        let path = match mu_common::paths::expand_tilde(arg, &home) {
            Some(p) => p,
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
            "local" => match self.agent.status.local_dir.clone() {
                None => self.notice("No session yet, so no local:// directory."),
                Some(dir) => match std::fs::create_dir_all(&dir) {
                    Ok(()) => self.copy_async(dir.clone(), format!("copied {dir}")),
                    Err(e) => self.error(&format!("cannot create {dir}: {e}")),
                },
            },
            _ => self.error("usage: /copy [code|cmd|link|local]"),
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

/// Whether `/agents` (every agent) or `/remote` (those on other machines) lists the row.
fn is_listed(r: &LiveRow, remote: bool) -> bool {
    r.live.kind.as_deref() == Some("agent") && (!remote || r.host.is_some())
}

/// How long ago the agent started, from its record's timestamp; empty when that does not read.
fn started_ago(r: &LiveRow) -> String {
    r.live.started.parse::<jiff::Timestamp>().map(|t| super::pickers::ago(t.into())).unwrap_or_default()
}

/// Where the agent runs and what it is doing, for the list and the completion menu: host, state, who is
/// attached, age, directory and title, leaving out what is empty.
fn agent_detail(r: &LiveRow) -> String {
    let state = if r.live.locked {
        "locked"
    } else if r.live.waiting {
        "waiting"
    } else {
        ""
    };
    let client = r.live.client.as_ref().map(|c| format!("@{}", c.host)).unwrap_or_default();
    let cwd = r.live.cwd.as_ref().map(|c| c.display().to_string()).unwrap_or_default();
    let parts = [r.host_label(), state, &client, &started_ago(r), &cwd, r.title.as_deref().unwrap_or_default()];
    parts.iter().filter(|p| !p.is_empty()).copied().collect::<Vec<_>>().join(" · ")
}

/// The agent `arg` names: a prefix of its id, or `HOST:PREFIX` (`here` is this host).
fn pick_agent<'a>(rows: &'a [LiveRow], arg: &str) -> Result<&'a LiveRow, String> {
    let (host, prefix) = match arg.split_once(':') {
        Some((h, p)) => (Some(h), p),
        None => (None, arg),
    };
    let hits: Vec<&LiveRow> =
        rows.iter().filter(|r| host.is_none_or(|h| r.host_label() == h) && r.agent_id().starts_with(prefix)).collect();
    match hits.as_slice() {
        [] => Err(format!("no agent matches {arg}")),
        [one] => Ok(one),
        many => {
            let names: Vec<String> = many.iter().map(|r| format!("{}:{}", r.host_label(), r.agent_id())).collect();
            Err(format!("several agents match {arg}: {}; use HOST:ID", names.join(", ")))
        }
    }
}

#[cfg(test)]
mod agents_tests {
    use super::*;
    use mu_session::session::owner::LiveSession;

    fn row(host: Option<&str>, path: &str) -> LiveRow {
        LiveRow {
            live: LiveSession {
                path: path.into(),
                pid: 1,
                print_run: false,
                inbox: false,
                started: String::new(),
                kind: Some("agent".into()),
                agent: None,
                cwd: None,
                parent: None,
                abort: false,
                waiting: false,
                locked: false,
                client: None,
            },
            parent: None,
            modified: None,
            title: None,
            host: host.map(String::from),
            hint: None,
            attach: None,
            helper: None,
            state_key: None,
            actions: Vec::new(),
        }
    }

    #[test]
    fn an_agent_is_picked_by_an_id_prefix_or_host_and_prefix() {
        let rows = [
            row(None, "/s/2026-01-01T00-00-00Z_abc123.jsonl"),
            row(Some("far"), "remote/far/abd_9.jsonl"),
            row(Some("near"), "remote/near/xyz.jsonl"),
        ];
        assert_eq!(pick_agent(&rows, "abc").unwrap().agent_id(), "abc123");
        assert_eq!(pick_agent(&rows, "abd_").unwrap().host_label(), "far", "a remote id keeps its underscore");
        assert_eq!(pick_agent(&rows, "near:x").unwrap().host_label(), "near");
        assert_eq!(pick_agent(&rows, "here:abc").unwrap().host_label(), "here");
        assert_eq!(pick_agent(&rows, "far:").unwrap().agent_id(), "abd_9");
    }

    #[test]
    fn an_ambiguous_or_unknown_agent_is_an_error() {
        let rows = [row(None, "/s/t_abc1.jsonl"), row(Some("far"), "remote/far/abc2.jsonl")];
        let e = pick_agent(&rows, "ab").unwrap_err();
        assert_eq!(e, "several agents match ab: here:abc1, far:abc2; use HOST:ID");
        assert!(pick_agent(&rows, "far:abc").is_ok());
        assert_eq!(pick_agent(&rows, "zz").unwrap_err(), "no agent matches zz");
        assert_eq!(pick_agent(&rows, "here:abc2").unwrap_err(), "no agent matches here:abc2");
    }
}
