//! `/mcp`: a server picker with per-server actions, the add/import wizard, and the typed
//! subcommands. The config files are edited (`mu_tools::mcp::store`) and the running host is
//! brought in line with them.

use super::*;
use crate::app::ui_msgs::UiMsg;
use crate::mcp::mcp_cmd::{self, Draft, McpCmd};
use crate::mcp::mcp_wizard::{Answer, Step, Wizard};
use crate::widgets::overlay::{McpAction, McpPick, McpRow, Overlay};
use crate::widgets::select::{Item, Select};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use mu_base::events::Command;
use mu_tools::mcp::config::{self, Entry, Kind};
use mu_tools::mcp::discover::{self, Found, Roots};
use mu_tools::mcp::trust;
use mu_tools::plugin::host::McpInfo;
use std::time::Instant;
use tokio_util::sync::CancellationToken;

/// Whether `name` is set and non-empty. Unlike the key flow's check, blanks and non-UTF-8 count.
fn env_set(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|v| !v.is_empty())
}

/// `scheme://host` of a url: paths, queries and credentials may carry secrets.
fn origin(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else { return String::new() };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    format!("{scheme}://{}", authority.rsplit('@').next().unwrap_or(authority))
}

fn describe(info: &McpInfo, name: &str) -> String {
    if info.needs_login {
        format!("needs login: /login mcp:{name}")
    } else if info.failed {
        "failed to list its tools".into()
    } else if info.listed {
        format!("{} tools{}", info.tools.len(), if info.running { ", running" } else { "" })
    } else {
        "not connected yet; its tools are listed on first use".into()
    }
}

/// The tool names under a server's status line, as many as fit.
fn tool_names(info: &McpInfo) -> String {
    const SHOWN: usize = 8;
    let mut s: String = info.tools.iter().take(SHOWN).map(|t| format!("\n  {t}")).collect();
    if info.tools.len() > SHOWN {
        s.push_str(&format!("\n  … and {} more", info.tools.len() - SHOWN));
    }
    s
}

/// A server as its file writes it (an untrusted entry is shown unexpanded): a command with its arguments, or a url.
fn as_written(s: &config::McpServer) -> String {
    match &s.kind {
        Kind::Stdio { command, args, .. } => format!("{command} {}", args.join(" ")).trim_end().to_string(),
        Kind::Http { url, .. } => url.clone(),
    }
}

impl App {
    pub(super) fn mcp_cmd(&mut self, arg: &str) {
        if arg.trim().is_empty() {
            return self.open_mcp_servers();
        }
        // A subcommand that needs a server, typed without one: pick it from the list.
        if matches!(arg.trim(), "remove" | "rm" | "enable" | "disable" | "reconnect" | "test") {
            return self.open_mcp_servers();
        }
        match mcp_cmd::parse(arg) {
            Err(e) => self.error(&e),
            Ok(McpCmd::Help) => self.notice(mcp_cmd::USAGE),
            Ok(McpCmd::List) => self.mcp_list(),
            Ok(McpCmd::Wizard) => self.start_mcp_wizard(false),
            Ok(McpCmd::Import) => self.start_mcp_wizard(true),
            Ok(McpCmd::Add(d)) => self.mcp_add(d),
            Ok(McpCmd::Remove(name)) => self.mcp_remove(&name),
            Ok(McpCmd::Enable(name)) => self.mcp_switch(&name, true),
            Ok(McpCmd::Disable(name)) => self.mcp_switch(&name, false),
            Ok(McpCmd::Reload) => self.mcp_apply("re-reading the MCP config".into(), true),
            Ok(McpCmd::Reconnect(name)) => self.mcp_reconnect(name, false),
            Ok(McpCmd::Test(name)) => self.mcp_reconnect(name, true),
            Ok(McpCmd::Trust) => self.mcp_trust(),
            Ok(McpCmd::Untrust) => self.mcp_untrust(),
        }
    }

    fn level_of(&self, e: &Entry, user_file: &std::path::Path) -> String {
        if e.file == user_file {
            "user".to_string()
        } else if e.file.starts_with(&self.cwd) {
            "project".to_string()
        } else {
            e.file.display().to_string()
        }
    }

    fn state_of(&self, e: &Entry) -> String {
        if !e.trusted {
            return "(untrusted)".into();
        }
        if !e.enabled {
            return "disabled".into();
        }
        match self.plugins.mcp_info(&e.server.name) {
            Some(i) => describe(&i, &e.server.name),
            None => "enabled, not loaded yet: /mcp reload".into(),
        }
    }

    fn mcp_list(&mut self) {
        let agent = paths::omp_agent_dir();
        let (all, warnings) = config::load_all(&self.cwd, &agent);
        let user_file = agent.join("mcp.json");
        let mut rows: Vec<String> = all
            .iter()
            .map(|e| {
                let s = &e.server;
                let target = match &s.kind {
                    Kind::Http { url, .. } => origin(url),
                    Kind::Stdio { command, .. } => command.clone(),
                };
                let level = self.level_of(e, &user_file);
                format!("mcp:{} · {} · {level} · {target} · {}", s.name, s.kind_label(), self.state_of(e))
            })
            .collect();
        rows.extend(warnings.into_iter().map(|w| format!("warning: {w}")));
        self.notice(&if rows.is_empty() {
            "no MCP servers configured; /mcp add to add one, /mcp help for the syntax".to_string()
        } else {
            rows.join("\n")
        });
    }

    fn found_servers(&self, configured: &[Entry]) -> Vec<Found> {
        let roots = Roots { home: paths::home(), cwd: self.cwd.clone(), agent_dir: paths::omp_agent_dir() };
        discover::scan(&roots, configured)
    }

    // ---- pickers -----------------------------------------------------------------------------

    /// `/mcp` without arguments: the configured servers, then the add and import rows.
    fn open_mcp_servers(&mut self) {
        let agent = paths::omp_agent_dir();
        let (all, warnings) = config::load_all(&self.cwd, &agent);
        let found = self.found_servers(&all);
        let user_file = agent.join("mcp.json");
        let (mut items, mut rows) = (Vec::new(), Vec::new());
        for e in &all {
            let detail = format!("{} · {} · {}", e.server.kind_label(), self.level_of(e, &user_file), self.state_of(e));
            items.push(Item { label: e.server.name.clone(), detail });
            rows.push(McpRow::Server(e.server.name.clone()));
        }
        items.push(Item { label: "+ Add a server…".into(), detail: "guided: discovered, popular or custom".into() });
        rows.push(McpRow::Add);
        if !found.is_empty() {
            let mut tools: Vec<&str> = found.iter().map(|f| f.tool.as_str()).collect();
            tools.dedup();
            items.push(Item {
                label: "↓ Import from other tools…".into(),
                detail: format!("{} found: {}", found.len(), tools.join(", ")),
            });
            rows.push(McpRow::Import);
        }
        if !warnings.is_empty() {
            self.notice(&warnings.iter().map(|w| format!("warning: {w}")).collect::<Vec<_>>().join("\n"));
        }
        self.overlay = Some(Overlay::Mcp { sel: Select::new("MCP servers", items), pick: McpPick::Servers(rows) });
    }

    fn open_mcp_actions(&mut self, name: String) {
        let agent = paths::omp_agent_dir();
        let (all, _) = config::load_all(&self.cwd, &agent);
        let Some(entry) = all.iter().find(|e| e.server.name == name) else {
            return self.error(&format!("mcp:{name} is not configured any more"));
        };
        let info = self.plugins.mcp_info(&name);
        let (mut items, mut acts) = (Vec::new(), Vec::new());
        let mut add = |label: &str, detail: &str, a: McpAction| {
            items.push(Item { label: label.into(), detail: detail.into() });
            acts.push(a);
        };
        if !entry.enabled {
            add("Enable", "turn it on and connect", McpAction::Enable);
        } else {
            if info.as_ref().is_some_and(|i| i.needs_login) || entry.server.wants_oauth() {
                add("Log in", "OAuth through omp", McpAction::Login);
            }
            add("Test", "reconnect and list its tools", McpAction::Test);
            add("Reconnect", "restart the server and refresh its tools", McpAction::Reconnect);
            add("Disable", "turn it off; keeps the config", McpAction::Disable);
        }
        add("Remove…", "delete it from the config", McpAction::Remove);
        let title = format!("mcp:{name} · {}", self.state_of(entry));
        self.overlay = Some(Overlay::Mcp { sel: Select::new(&title, items), pick: McpPick::Actions(name, acts) });
    }

    /// The startup offer to trust a project's MCP servers before they start. `files` are what
    /// `config::untrusted_files` found; the context shows each file's servers as written.
    pub(super) fn open_mcp_trust(&mut self, files: Vec<(PathBuf, String, Vec<String>)>) {
        let (all, _) = config::load_all_with(&self.cwd, &paths::omp_agent_dir(), &|_| None);
        let mut context = String::new();
        for (file, _, _) in &files {
            context.push_str(&format!("{}\n", file.display()));
            for e in all.iter().filter(|e| !e.trusted && e.file == *file) {
                context.push_str(&format!("  {}: {}\n", e.server.name, as_written(&e.server)));
            }
        }
        let items = vec![
            Item { label: "Trust".into(), detail: "start them; asked again if the file's servers change".into() },
            Item { label: "Not now".into(), detail: "they stay off; /mcp trust starts them later".into() },
        ];
        let sel = Select::new("Trust this project's MCP servers?", items).context(context.trim_end());
        self.overlay = Some(Overlay::McpTrust { sel, files });
    }

    fn open_mcp_confirm_remove(&mut self, name: String) {
        let items = vec![
            Item { label: "Remove".into(), detail: "deletes the definition and its on/off switches".into() },
            Item { label: "Keep".into(), detail: String::new() },
        ];
        let title = format!("Remove mcp:{name}?");
        self.overlay = Some(Overlay::Mcp { sel: Select::new(&title, items), pick: McpPick::ConfirmRemove(name) });
    }

    pub(super) fn on_mcp_pick(&mut self, pick: McpPick, row: Option<usize>) {
        match pick {
            McpPick::Wizard => match row {
                Some(i) => self.mcp_wizard_answer(Answer::Pick(i)),
                None => self.mcp_wizard_cancel(),
            },
            McpPick::Servers(rows) => match row.and_then(|i| rows.into_iter().nth(i)) {
                Some(McpRow::Server(name)) => self.open_mcp_actions(name),
                Some(McpRow::Add) => self.start_mcp_wizard(false),
                Some(McpRow::Import) => self.start_mcp_wizard(true),
                None => {}
            },
            McpPick::Actions(name, acts) => match row.and_then(|i| acts.get(i)).copied() {
                Some(McpAction::Enable) => self.mcp_switch(&name, true),
                Some(McpAction::Disable) => self.mcp_switch(&name, false),
                Some(McpAction::Reconnect) => self.mcp_reconnect(name, false),
                Some(McpAction::Test) => self.mcp_reconnect(name, true),
                Some(McpAction::Login) => self.run_mcp_login(&name),
                Some(McpAction::Remove) => self.open_mcp_confirm_remove(name),
                None => {}
            },
            McpPick::ConfirmRemove(name) => {
                if row == Some(0) {
                    self.mcp_remove(&name);
                }
            }
        }
    }

    // ---- changes -----------------------------------------------------------------------------

    fn mcp_add(&mut self, d: Draft) {
        match self.services.mcp_add(d.level, &self.cwd, &d.name, d.entry) {
            Ok(path) => self.mcp_apply(format!("added mcp:{} to {}", d.name, path.display()), false),
            Err(e) => self.error(&e),
        }
    }

    fn mcp_remove(&mut self, name: &str) {
        match self.services.mcp_remove(&self.cwd, name) {
            Ok(files) => {
                let at: Vec<String> = files.iter().map(|f| f.display().to_string()).collect();
                self.mcp_apply(format!("removed mcp:{name} from {}", at.join(", ")), false);
            }
            Err(e) => self.error(&e),
        }
    }

    fn mcp_switch(&mut self, name: &str, on: bool) {
        let verb = if on { "enabled" } else { "disabled" };
        match self.services.mcp_set_enabled(&self.cwd, name, on) {
            Ok(true) => self.mcp_apply(format!("{verb} mcp:{name}"), false),
            Ok(false) => self.notice(&format!("mcp:{name} is already {verb}")),
            Err(e) => self.error(&e),
        }
    }

    /// Re-reads the config and brings the running servers in line with it. New and changed servers
    /// are connected so their tools are known right away.
    fn mcp_apply(&mut self, headline: String, show_warnings: bool) {
        self.notice(&headline);
        let (plugins, tx, cwd) = (self.plugins.clone(), self.ui_tx.clone(), self.cwd.clone());
        tokio::task::spawn_local(async move {
            let (servers, warnings) = config::load(&cwd, &paths::omp_agent_dir());
            let sync = plugins.mcp_sync(servers).await;
            if !sync.added.is_empty() || !sync.changed.is_empty() {
                plugins.list_missing_until(&CancellationToken::new()).await;
            }
            let mut lines: Vec<String> = Vec::new();
            if show_warnings {
                lines.extend(warnings.into_iter().map(|w| format!("warning: {w}")));
            }
            lines.extend(sync.removed.iter().map(|n| format!("mcp:{n}: stopped")));
            for (verb, names) in [("added", &sync.added), ("restarted on its new config", &sync.changed)] {
                for n in names {
                    lines.push(match plugins.mcp_info(n) {
                        Some(i) => format!("mcp:{n}: {verb}; {}{}", describe(&i, n), tool_names(&i)),
                        None => format!("mcp:{n}: {verb}; gone"),
                    });
                }
            }
            if lines.is_empty() {
                lines.push("no change to the running servers".into());
            }
            let _ = tx.send(UiMsg::McpApplied { lines, refresh: !sync.is_empty() });
        });
    }

    /// Trusts each file's server set as the offer showed it, then starts the servers.
    pub(super) fn trust_mcp_files(&mut self, files: Vec<(PathBuf, String, Vec<String>)>) {
        for (file, digest, _) in &files {
            if let Err(e) = trust::trust(file, digest) {
                return self.error(&format!("{}: {e}", file.display()));
            }
        }
        self.mcp_apply("trusted the project's MCP servers".into(), true);
    }

    /// `/mcp trust`: trusts the project's current servers without a dialog.
    fn mcp_trust(&mut self) {
        let files = config::untrusted_files(&self.cwd, &paths::omp_agent_dir());
        if files.is_empty() {
            return self.notice("no project MCP servers are waiting for trust");
        }
        self.trust_mcp_files(files);
    }

    /// `/mcp untrust`: the project's servers stay off until they are trusted again.
    fn mcp_untrust(&mut self) {
        for file in config::candidate_files(&self.cwd, &paths::omp_agent_dir()) {
            if trust::is_project_file(&self.cwd, &file) {
                trust::untrust(&file);
            }
        }
        self.mcp_apply("untrusted the project's MCP servers".into(), true);
    }

    fn mcp_reconnect(&mut self, name: String, verbose: bool) {
        if !self.plugins.mcp_names().contains(&name) {
            return self.error(&format!("no enabled MCP server `{name}` is loaded; see /mcp list"));
        }
        self.notice(&format!("connecting mcp:{name}…"));
        let (plugins, tx) = (self.plugins.clone(), self.ui_tx.clone());
        tokio::task::spawn_local(async move {
            let t = Instant::now();
            let result = plugins.mcp_reconnect(&name).await;
            let _ = tx.send(UiMsg::McpReconnected { name, verbose, ms: t.elapsed().as_millis(), result });
        });
    }

    pub(super) fn on_mcp_reconnected(
        &mut self,
        name: &str,
        verbose: bool,
        ms: u128,
        result: Result<(usize, bool), String>,
    ) {
        match result {
            Ok((n, changed)) => {
                let mut s = format!("mcp:{name}: connected, {n} tools ({ms} ms)");
                if verbose && let Some(info) = self.plugins.mcp_info(name) {
                    s.push_str(&tool_names(&info));
                }
                self.notice(&s);
                if changed {
                    self.send(Command::RefreshTools);
                }
            }
            Err(e) => self.error(&format!("mcp:{name}: {e}")),
        }
    }

    // ---- wizard ------------------------------------------------------------------------------

    /// `import` starts at the servers other tools have configured.
    fn start_mcp_wizard(&mut self, import: bool) {
        self.leave_custom_wizard();
        let (all, _) = config::load_all(&self.cwd, &paths::omp_agent_dir());
        let found = self.found_servers(&all);
        if import && found.is_empty() {
            return self
                .notice("no servers to import: nothing new found in omp, pi, Claude, Codex, Gemini CLI or OpenCode");
        }
        let wizard =
            if import { Wizard::import(found, &self.cwd, env_set) } else { Wizard::new(found, &self.cwd, env_set) };
        self.flows.mcp_wizard = Some(wizard);
        self.prompt.composer.set_private(true);
        self.mcp_wizard_show();
    }

    /// Prints what the wizard has to say, then shows its current step.
    fn mcp_wizard_show(&mut self) {
        let Some(w) = self.flows.mcp_wizard.as_mut() else { return };
        let notes = w.take_notes();
        let step = w.step();
        if !notes.is_empty() {
            self.notice(&notes.join("\n"));
        }
        match step {
            Step::Pick { title, items } => {
                let items = items.into_iter().map(|i| Item { label: i.label, detail: i.detail }).collect();
                self.overlay = Some(Overlay::Mcp { sel: Select::new(&title, items), pick: McpPick::Wizard });
            }
            Step::Text { prompt, .. } => self.notice(&format!("{prompt}   (Esc cancels)")),
            Step::Done(draft) => {
                self.end_mcp_wizard();
                self.mcp_add(draft);
            }
            Step::Cancelled => self.mcp_wizard_cancel(),
        }
    }

    fn mcp_wizard_answer(&mut self, a: Answer) {
        let Some(w) = self.flows.mcp_wizard.as_mut() else { return };
        if let Err(e) = w.answer(a) {
            self.error(&e);
        }
        self.mcp_wizard_show();
    }

    fn mcp_wizard_cancel(&mut self) {
        self.end_mcp_wizard();
        self.prompt.composer.set_text("");
        self.notice("cancelled; nothing was added");
    }

    /// Keys the wizard takes before the composer sees them. True when the key was handled.
    pub(super) fn mcp_wizard_key(&mut self, k: KeyEvent) -> bool {
        if k.code == KeyCode::Esc {
            self.mcp_wizard_cancel();
            return true;
        }
        // An empty Enter picks the step's default, which the composer would swallow.
        if k.code == KeyCode::Enter
            && self.prompt.composer.is_empty()
            && !k.modifiers.intersects(KeyModifiers::SHIFT | KeyModifiers::ALT)
        {
            self.mcp_wizard_line("");
            return true;
        }
        false
    }

    pub(super) fn mcp_wizard_line(&mut self, line: &str) {
        self.mcp_wizard_answer(Answer::Text(line.to_string()));
    }

    /// Answers may be tokens: the composer keeps them out of the history while a wizard runs.
    fn end_mcp_wizard(&mut self) {
        self.flows.mcp_wizard = None;
        self.prompt.composer.set_private(false);
    }
}
