//! `/mcp`: a server picker with per-server actions, the add/import wizard, and the typed
//! subcommands. The config files are edited and the running servers brought in line with them by
//! the host (`host::mcp`).

use super::*;
use crate::host::mcp::{self as hmcp, Applied, McpServers, Reconnected, ServerRow};
use crate::mcp::mcp_cmd::{self, Draft, McpCmd};
use crate::mcp::mcp_wizard::{Answer, Step, Wizard};
use crate::widgets::overlay::{McpAction, McpPick, McpRow, Overlay};
use crate::widgets::select::{Item, Select};
use crossterm::event::KeyEvent;
use mu_base::events::Command;

impl App {
    pub(super) fn mcp_cmd(&mut self, arg: &str) {
        if arg.trim().is_empty() {
            return self.open_mcp_servers();
        }
        // A subcommand that needs a server, typed without one: pick it from the list.
        if matches!(arg.trim(), "remove" | "rm" | "enable" | "disable" | "reconnect" | "test" | "login") {
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
            Ok(McpCmd::Login(name)) => self.run_mcp_login(&name),
            Ok(McpCmd::Trust) => self.mcp_trust(),
            Ok(McpCmd::Untrust) => self.mcp_untrust(),
        }
    }

    fn mcp_list(&mut self) {
        self.host_call(hmcp::MCP_SERVERS, &false, |app, r: Result<McpServers, String>| {
            let Ok(McpServers { servers, warnings, .. }) = r.map_err(|e| app.error(&e)) else { return };
            let mut rows: Vec<String> = servers
                .iter()
                .map(|s| format!("mcp:{} · {} · {} · {} · {}", s.name, s.kind_label, s.level, s.target, s.state))
                .collect();
            rows.extend(warnings.into_iter().map(|w| format!("warning: {w}")));
            app.notice(&if rows.is_empty() {
                "no MCP servers configured; /mcp add to add one, /mcp help for the syntax".to_string()
            } else {
                rows.join("\n")
            });
        });
    }

    // ---- pickers -----------------------------------------------------------------------------

    /// `/mcp` without arguments: the configured servers, then the add and import rows.
    fn open_mcp_servers(&mut self) {
        self.host_call(hmcp::MCP_SERVERS, &true, |app, r: Result<McpServers, String>| {
            let Ok(r) = r.map_err(|e| app.error(&e)) else { return };
            app.show_mcp_servers(r);
        });
    }

    fn show_mcp_servers(&mut self, McpServers { servers, warnings, found }: McpServers) {
        let (mut items, mut rows) = (Vec::new(), Vec::new());
        for s in &servers {
            let detail = format!("{} · {} · {}", s.kind_label, s.level, s.state);
            items.push(Item { label: s.name.clone(), detail });
            rows.push(McpRow::Server(s.name.clone()));
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
        self.open_overlay(Overlay::Mcp { sel: Select::new("MCP servers", items), pick: McpPick::Servers(rows) });
    }

    fn open_mcp_actions(&mut self, name: String) {
        self.host_call(hmcp::MCP_SERVERS, &false, move |app, r: Result<McpServers, String>| {
            let Ok(r) = r.map_err(|e| app.error(&e)) else { return };
            let Some(entry) = r.servers.into_iter().find(|s| s.name == name) else {
                return app.error(&format!("mcp:{name} is not configured any more"));
            };
            app.show_mcp_actions(name, entry);
        });
    }

    fn show_mcp_actions(&mut self, name: String, entry: ServerRow) {
        let (mut items, mut acts) = (Vec::new(), Vec::new());
        let mut add = |label: &str, detail: &str, a: McpAction| {
            items.push(Item { label: label.into(), detail: detail.into() });
            acts.push(a);
        };
        if !entry.enabled {
            add("Enable", "turn it on and connect", McpAction::Enable);
        } else {
            if entry.needs_login || entry.wants_oauth {
                add("Log in", "OAuth sign-in", McpAction::Login);
            }
            add("Test", "reconnect and list its tools", McpAction::Test);
            add("Reconnect", "restart the server and refresh its tools", McpAction::Reconnect);
            add("Disable", "turn it off; keeps the config", McpAction::Disable);
        }
        add("Remove…", "delete it from the config", McpAction::Remove);
        let title = format!("mcp:{name} · {}", entry.state);
        self.open_overlay(Overlay::Mcp { sel: Select::new(&title, items), pick: McpPick::Actions(name, acts) });
    }

    /// The startup offer to trust a project's MCP servers before they start. `files` are what
    /// `config::untrusted_files` found; the context shows each file's servers as written.
    pub(super) fn open_mcp_trust(&mut self, found: Vec<UntrustedFile>) {
        let mut context = String::new();
        for f in &found {
            context.push_str(&format!("{}\n", f.file.display()));
            for s in &f.servers {
                context.push_str(&format!("  {}: {}\n", s.name, s.written));
            }
        }
        let files =
            found.into_iter().map(|f| (f.file, f.digest, f.servers.into_iter().map(|s| s.name).collect())).collect();
        let items = vec![
            Item { label: "Trust".into(), detail: "start them; asked again if the file's servers change".into() },
            Item { label: "Not now".into(), detail: "they stay off; /mcp trust starts them later".into() },
        ];
        let sel = Select::new("Trust this project's MCP servers?", items).context(context.trim_end());
        self.open_overlay(Overlay::McpTrust { sel, files });
    }

    fn open_mcp_confirm_remove(&mut self, name: String) {
        let items = vec![
            Item { label: "Remove".into(), detail: "deletes the definition and its on/off switches".into() },
            Item { label: "Keep".into(), detail: String::new() },
        ];
        let title = format!("Remove mcp:{name}?");
        self.open_overlay(Overlay::Mcp { sel: Select::new(&title, items), pick: McpPick::ConfirmRemove(name) });
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

    fn mcp_add(&mut self, d: Draft) {
        let name = d.name.clone();
        self.host_call(hmcp::MCP_ADD, &(d.level, d.name, d.entry), move |app, r: Result<PathBuf, String>| match r {
            Ok(path) => app.mcp_apply(format!("added mcp:{name} to {}", path.display()), false),
            Err(e) => app.error(&e),
        });
    }

    fn mcp_remove(&mut self, name: &str) {
        let name = name.to_string();
        self.host_call(hmcp::MCP_REMOVE, &name.clone(), move |app, r: Result<Vec<PathBuf>, String>| match r {
            Ok(files) => {
                let at: Vec<String> = files.iter().map(|f| f.display().to_string()).collect();
                app.mcp_apply(format!("removed mcp:{name} from {}", at.join(", ")), false);
            }
            Err(e) => app.error(&e),
        });
    }

    fn mcp_switch(&mut self, name: &str, on: bool) {
        let verb = if on { "enabled" } else { "disabled" };
        let name = name.to_string();
        self.host_call(hmcp::MCP_SET_ENABLED, &(name.clone(), on), move |app, r: Result<bool, String>| match r {
            Ok(true) => app.mcp_apply(format!("{verb} mcp:{name}"), false),
            Ok(false) => app.notice(&format!("mcp:{name} is already {verb}")),
            Err(e) => app.error(&e),
        });
    }

    /// Re-reads the config and brings the running servers in line with it. New and changed servers
    /// are connected so their tools are known right away.
    fn mcp_apply(&mut self, headline: String, show_warnings: bool) {
        self.notice(&headline);
        self.host_call(hmcp::MCP_APPLY, &show_warnings, |app, r: Result<Applied, String>| match r {
            Ok(Applied { lines, refresh }) => {
                app.notice(&lines.join("\n"));
                if refresh {
                    app.send(Command::RefreshTools);
                }
            }
            Err(e) => app.error(&e),
        });
    }

    /// Trusts each file's server set as the offer showed it, then starts the servers.
    pub(super) fn trust_mcp_files(&mut self, files: Vec<(PathBuf, String, Vec<String>)>) {
        let files: Vec<(PathBuf, String)> = files.into_iter().map(|(file, digest, _)| (file, digest)).collect();
        self.host_call(hmcp::MCP_TRUST, &Some(files), |app, r: Result<usize, String>| match r {
            Ok(_) => app.mcp_apply("trusted the project's MCP servers".into(), true),
            Err(e) => app.error(&e),
        });
    }

    /// `/mcp trust`: trusts the project's current servers without a dialog.
    fn mcp_trust(&mut self) {
        self.host_call(hmcp::MCP_TRUST, &None::<Vec<(PathBuf, String)>>, |app, r: Result<usize, String>| match r {
            Ok(0) => app.notice("no project MCP servers are waiting for trust"),
            Ok(_) => app.mcp_apply("trusted the project's MCP servers".into(), true),
            Err(e) => app.error(&e),
        });
    }

    /// `/mcp untrust`: the project's servers stay off until they are trusted again.
    fn mcp_untrust(&mut self) {
        self.host_call(hmcp::MCP_UNTRUST, &(), |app, r: Result<(), String>| match r {
            Ok(()) => app.mcp_apply("untrusted the project's MCP servers".into(), true),
            Err(e) => app.error(&e),
        });
    }

    fn mcp_reconnect(&mut self, name: String, verbose: bool) {
        self.host_call(hmcp::MCP_NAMES, &(), move |app, r: Result<Vec<String>, String>| {
            if !r.unwrap_or_default().contains(&name) {
                return app.error(&format!("no enabled MCP server `{name}` is loaded; see /mcp list"));
            }
            app.notice(&format!("connecting mcp:{name}…"));
            app.host_call(hmcp::MCP_RECONNECT, &(name.clone(), verbose), move |app, r: Result<Reconnected, String>| {
                match r {
                    Ok(Reconnected { tools, changed, ms, listing }) => {
                        app.notice(&format!("mcp:{name}: connected, {tools} tools ({ms} ms){listing}"));
                        if changed {
                            app.send(Command::RefreshTools);
                        }
                    }
                    Err(e) => app.error(&format!("mcp:{name}: {e}")),
                }
            });
        });
    }

    // ---- wizard ------------------------------------------------------------------------------

    /// `import` starts at the servers other tools have configured.
    fn start_mcp_wizard(&mut self, import: bool) {
        self.leave_custom_wizard();
        self.host_call(hmcp::MCP_SERVERS, &true, move |app, r: Result<McpServers, String>| {
            let Ok(McpServers { found, .. }) = r.map_err(|e| app.error(&e)) else { return };
            if import && found.is_empty() {
                return app.notice(
                    "no servers to import: nothing new found in omp, pi, Claude, Codex, Gemini CLI or OpenCode",
                );
            }
            let names: Vec<&str> = crate::mcp::mcp_presets::ENV_NAMES.to_vec();
            app.host_call(hmcp::MCP_ENV_SET, &names, move |app, set: Result<Vec<String>, String>| {
                let set = set.unwrap_or_default();
                let env_set = |n: &str| set.iter().any(|s| s == n);
                let cwd = app.cwd.clone();
                let wizard =
                    if import { Wizard::import(found, &cwd, env_set) } else { Wizard::new(found, &cwd, env_set) };
                app.flows.begin(Active::Mcp(Box::new(wizard)));
                app.prompt.composer.set_private(true);
                app.mcp_wizard_show();
            });
        });
    }

    /// Prints what the wizard has to say, then shows its current step.
    fn mcp_wizard_show(&mut self) {
        let Some(w) = self.flows.mcp_mut() else { return };
        let notes = w.take_notes();
        let step = w.step();
        if !notes.is_empty() {
            self.notice(&notes.join("\n"));
        }
        match step {
            Step::Pick { title, items } => {
                let items = items.into_iter().map(|i| Item { label: i.label, detail: i.detail }).collect();
                self.open_overlay(Overlay::Mcp { sel: Select::new(&title, items), pick: McpPick::Wizard });
            }
            Step::Text { prompt, .. } => {
                self.notice(&format!("{prompt}   ({} cancels)", crate::keymap::WIZARD_CANCEL.label()))
            }
            Step::Done(draft) => {
                self.end_mcp_wizard();
                self.mcp_add(draft);
            }
            Step::Cancelled => self.mcp_wizard_cancel(),
        }
    }

    fn mcp_wizard_answer(&mut self, a: Answer) {
        let Some(w) = self.flows.mcp_mut() else { return };
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
        if crate::keymap::WIZARD_CANCEL.hit(&k) {
            self.mcp_wizard_cancel();
            return true;
        }
        // An empty Enter picks the step's default, which the composer would swallow.
        if crate::keymap::SUBMIT.hit(&k) && self.prompt.composer.is_empty() {
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
        self.flows.end_mcp();
        self.prompt.composer.set_private(false);
    }
}

#[cfg(test)]
mod tests {
    use super::super::keys::tests::{in_local, press, test_app_with_commands, transcript_text};
    use super::*;
    use crate::app::ui_msgs::UiMsg;
    use crate::host::local::LocalHost;
    use crate::host::mcp_login as hlogin;
    use crate::host::{Answer as HostAnswer, Deferred, HostInfo, Pushed, encode};
    use crossterm::event::{KeyCode, KeyModifiers};
    use mu_tools::mcp::discover::Found;
    use serde_json::{Value, json};
    use std::cell::RefCell;
    use std::collections::HashMap;
    use tokio::sync::mpsc::UnboundedReceiver;

    /// The local host, except for the calls that would start or trust real servers: those answer
    /// with a canned reply, and every call that reaches the stub is recorded.
    struct Stub {
        inner: LocalHost,
        canned: HashMap<&'static str, Result<Value, String>>,
        seen: Rc<RefCell<Vec<(String, Value)>>>,
    }

    impl Host for Stub {
        fn info(&self) -> &HostInfo {
            self.inner.info()
        }

        fn call(&self, name: &str, args: Value) -> HostAnswer {
            match self.canned.get(name) {
                Some(r) => {
                    self.seen.borrow_mut().push((name.to_string(), args));
                    HostAnswer::Ready(r.clone())
                }
                None => self.inner.call(name, args),
            }
        }

        fn pushed(&self) -> Option<UnboundedReceiver<Pushed>> {
            None
        }
    }

    struct Rig {
        app: App,
        cmds: UnboundedReceiver<Command>,
        ui: UnboundedReceiver<UiMsg>,
        seen: Rc<RefCell<Vec<(String, Value)>>>,
        dir: PathBuf,
    }

    fn rig(deferred: bool, canned: Vec<(&'static str, Result<Value, String>)>) -> Rig {
        let dir = std::env::temp_dir().join(format!("mu-mcp-ui-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let (mut app, cmds) = test_app_with_commands(dir.join("history.jsonl"));
        app.cwd = dir.clone();
        let inner = LocalHost::new(app.services.providers.clone(), app.ctx.clone(), app.plugins.clone(), dir.clone());
        let seen = Rc::new(RefCell::new(Vec::new()));
        let stub = Stub { inner, canned: canned.into_iter().collect(), seen: seen.clone() };
        app.host = if deferred { Rc::new(Deferred(stub)) } else { Rc::new(stub) };
        let (ui_tx, ui) = tokio::sync::mpsc::unbounded_channel();
        app.ui_tx = ui_tx;
        Rig { app, cmds, ui, seen, dir }
    }

    /// Delivers the host's replies until `done` holds.
    async fn settle(r: &mut Rig, done: impl Fn(&App) -> bool) {
        for _ in 0..20 {
            if done(&r.app) {
                return;
            }
            match tokio::time::timeout(std::time::Duration::from_secs(5), r.ui.recv()).await {
                Ok(Some(m)) => r.app.on_ui(m),
                _ => return,
            }
        }
    }

    fn says(text: &'static str) -> impl Fn(&App) -> bool {
        move |app| transcript_text(app).contains(text)
    }

    fn project_server(dir: &std::path::Path, name: &str) {
        std::fs::create_dir_all(dir.join(".omp")).unwrap();
        let body = json!({"mcpServers": {name: {"type": "stdio", "command": "demo-cmd"}}});
        std::fs::write(dir.join(".omp/mcp.json"), body.to_string()).unwrap();
    }

    fn ok<T: serde::Serialize>(v: T) -> Result<Value, String> {
        Ok(serde_json::to_value(v).unwrap())
    }

    #[test]
    fn the_list_and_the_picker_show_a_project_server() {
        for deferred in [false, true] {
            in_local(async move {
                let mut r = rig(deferred, vec![]);
                project_server(&r.dir, "mu-t-list");
                r.app.mcp_cmd("list");
                settle(&mut r, says("mcp:mu-t-list")).await;
                let t = transcript_text(&r.app);
                assert!(t.contains("mcp:mu-t-list · stdio · project · demo-cmd · (untrusted)"), "{t}");

                r.app.mcp_cmd("");
                settle(&mut r, |app| app.overlay.is_some()).await;
                match &r.app.overlay {
                    Some(Overlay::Mcp { pick: McpPick::Servers(rows), .. }) => {
                        assert!(rows.contains(&McpRow::Server("mu-t-list".into())), "{rows:?}");
                    }
                    _ => panic!("the server picker did not open"),
                }
                std::fs::remove_dir_all(&r.dir).unwrap();
            });
        }
    }

    #[test]
    fn a_server_is_added_and_removed_through_the_host() {
        for deferred in [false, true] {
            in_local(async move {
                let applied = Applied { lines: vec!["applied it".into()], refresh: true };
                let mut r = rig(deferred, vec![(hmcp::MCP_APPLY, ok(applied))]);
                r.app.mcp_cmd("add mu-t-add --project demo-cmd");
                settle(&mut r, says("applied it")).await;
                let t = transcript_text(&r.app);
                assert!(t.contains("added mcp:mu-t-add to"), "{t}");
                assert!(std::fs::read_to_string(r.dir.join(".omp/mcp.json")).unwrap().contains("mu-t-add"));
                assert!(matches!(r.cmds.try_recv(), Ok(Command::RefreshTools)));

                r.app.mcp_cmd("remove mu-t-add");
                settle(&mut r, says("removed mcp:mu-t-add from")).await;
                assert!(!std::fs::read_to_string(r.dir.join(".omp/mcp.json")).unwrap().contains("mu-t-add"));
                assert_eq!(r.seen.borrow().iter().filter(|(n, _)| n == hmcp::MCP_APPLY).count(), 2);
                std::fs::remove_dir_all(&r.dir).unwrap();
            });
        }
    }

    #[test]
    fn enable_and_disable_report_what_the_host_changed() {
        for deferred in [false, true] {
            in_local(async move {
                let mut r = rig(deferred, vec![(hmcp::MCP_SET_ENABLED, ok(false))]);
                r.app.mcp_cmd("enable srv");
                settle(&mut r, says("mcp:srv is already enabled")).await;
                assert_eq!(r.seen.borrow()[0], (hmcp::MCP_SET_ENABLED.to_string(), json!(["srv", true])));
                let mut r =
                    rig(deferred, vec![(hmcp::MCP_SET_ENABLED, Err("no MCP server `srv` is configured".into()))]);
                r.app.mcp_cmd("disable srv");
                settle(&mut r, says("no MCP server `srv` is configured")).await;
            });
        }
    }

    #[test]
    fn reload_shows_what_the_host_applied() {
        for deferred in [false, true] {
            in_local(async move {
                let applied = Applied { lines: vec!["mcp:a: added; 1 tools".into()], refresh: false };
                let mut r = rig(deferred, vec![(hmcp::MCP_APPLY, ok(applied))]);
                r.app.mcp_cmd("reload");
                settle(&mut r, says("mcp:a: added; 1 tools")).await;
                assert!(says("re-reading the MCP config")(&r.app));
                assert_eq!(r.seen.borrow()[0].1, json!(true));
                assert!(r.cmds.try_recv().is_err());
            });
        }
    }

    #[test]
    fn reconnect_needs_a_loaded_server_and_reports_the_result() {
        for deferred in [false, true] {
            in_local(async move {
                let mut r = rig(deferred, vec![]);
                r.app.mcp_cmd("reconnect nope");
                settle(&mut r, says("no enabled MCP server `nope` is loaded")).await;

                let done = Reconnected { tools: 3, changed: true, ms: 7, listing: "\n  a\n  b".into() };
                let mut r = rig(deferred, vec![(hmcp::MCP_NAMES, ok(vec!["srv"])), (hmcp::MCP_RECONNECT, ok(done))]);
                r.app.mcp_cmd("test srv");
                settle(&mut r, says("mcp:srv: connected")).await;
                let t = transcript_text(&r.app);
                assert!(t.contains("connecting mcp:srv…") && t.contains("connected, 3 tools (7 ms)\n  a\n  b"), "{t}");
                assert_eq!(r.seen.borrow()[1].1, json!(["srv", true]));
                assert!(matches!(r.cmds.try_recv(), Ok(Command::RefreshTools)));

                let mut r =
                    rig(deferred, vec![(hmcp::MCP_NAMES, ok(vec!["srv"])), (hmcp::MCP_RECONNECT, Err("boom".into()))]);
                r.app.mcp_cmd("reconnect srv");
                settle(&mut r, says("mcp:srv: boom")).await;
            });
        }
    }

    #[test]
    fn trust_and_untrust_go_through_the_host() {
        for deferred in [false, true] {
            in_local(async move {
                let applied = Applied { lines: vec!["applied it".into()], refresh: false };
                let mut r = rig(deferred, vec![(hmcp::MCP_TRUST, ok(0)), (hmcp::MCP_APPLY, ok(applied.clone()))]);
                r.app.mcp_cmd("trust");
                settle(&mut r, says("no project MCP servers are waiting for trust")).await;
                assert_eq!(r.seen.borrow()[0].1, Value::Null);

                let mut r = rig(deferred, vec![(hmcp::MCP_TRUST, ok(2)), (hmcp::MCP_APPLY, ok(applied.clone()))]);
                r.app.mcp_cmd("trust");
                settle(&mut r, says("applied it")).await;
                assert!(says("trusted the project's MCP servers")(&r.app));

                let mut r = rig(deferred, vec![(hmcp::MCP_TRUST, ok(1)), (hmcp::MCP_APPLY, ok(applied.clone()))]);
                r.app.trust_mcp_files(vec![("/p/.omp/mcp.json".into(), "dig".into(), vec!["s".into()])]);
                settle(&mut r, says("applied it")).await;
                assert_eq!(r.seen.borrow()[0].1, json!([["/p/.omp/mcp.json", "dig"]]));

                let mut r = rig(deferred, vec![(hmcp::MCP_TRUST, Err("/p/mcp.json: denied".into()))]);
                r.app.trust_mcp_files(vec![("/p/mcp.json".into(), "dig".into(), vec![])]);
                settle(&mut r, says("/p/mcp.json: denied")).await;

                let mut r = rig(deferred, vec![(hmcp::MCP_UNTRUST, ok(())), (hmcp::MCP_APPLY, ok(applied))]);
                r.app.mcp_cmd("untrust");
                settle(&mut r, says("applied it")).await;
                assert!(says("untrusted the project's MCP servers")(&r.app));
            });
        }
    }

    #[test]
    fn a_login_needs_a_configured_server_and_lists_its_tools_after() {
        for deferred in [false, true] {
            in_local(async move {
                let mut r = rig(deferred, vec![]);
                r.app.run_mcp_login("nope");
                settle(&mut r, says("no MCP server `nope` is configured")).await;

                let mut r = rig(deferred, login_calls(ok((4, true))));
                let flow = start_login(&mut r).await;
                assert_eq!(
                    r.seen.borrow()[1],
                    (hlogin::MCP_LOGIN_START.to_string(), json!({"flow": flow, "name": "srv"}))
                );
                assert!(r.app.typing(), "the composer is locked to the redirect URL");
                r.app
                    .on_pushed(Pushed { name: hlogin::DONE.into(), data: encode(&hlogin::Done { flow, error: None }) });
                settle(&mut r, says("mcp:srv: logged in, 4 tools")).await;
                assert_eq!(r.seen.borrow().last().unwrap(), &(hmcp::MCP_AFTER_LOGIN.to_string(), json!("srv")));
                assert!(matches!(r.cmds.try_recv(), Ok(Command::RefreshTools)));
                assert!(!r.app.typing());

                let mut r = rig(deferred, login_calls(Err("no tools".into())));
                let flow = start_login(&mut r).await;
                r.app
                    .on_pushed(Pushed { name: hlogin::DONE.into(), data: encode(&hlogin::Done { flow, error: None }) });
                settle(&mut r, says("logged in, but its tools could not be listed: no tools")).await;

                let mut r = rig(deferred, login_calls(ok((0, false))));
                let flow = start_login(&mut r).await;
                let failed = hlogin::Done { flow, error: Some("timed out".into()) };
                r.app.on_pushed(Pushed { name: hlogin::DONE.into(), data: encode(&failed) });
                assert!(says("mcp:srv: login did not finish: timed out")(&r.app));
                assert!(r.seen.borrow().iter().all(|(n, _)| n != hmcp::MCP_AFTER_LOGIN));
            });
        }
    }

    #[test]
    fn escape_cancels_a_waiting_login_and_unlocks_the_composer() {
        for deferred in [false, true] {
            in_local(async move {
                let mut r = rig(deferred, login_calls(ok((0, false))));
                let flow = start_login(&mut r).await;
                let url = hlogin::Url { flow, url: "https://auth.example/authorize?x=1".into() };
                r.app.on_pushed(Pushed { name: hlogin::URL.into(), data: encode(&url) });
                assert!(says("open this link:\nhttps://auth.example/authorize?x=1")(&r.app));
                r.app.on_terminal(press(KeyCode::Char('x'), KeyModifiers::NONE));
                assert_eq!(r.app.prompt.composer.text(), "x");
                r.app.on_terminal(press(KeyCode::Esc, KeyModifiers::NONE));
                assert!(r.app.flows.mcp_login().is_none() && !r.app.typing());
                assert_eq!(r.app.prompt.composer.text(), "");
                assert_eq!(
                    r.seen.borrow().last().unwrap(),
                    &(hlogin::MCP_LOGIN_CANCEL.to_string(), json!({"flow": flow}))
                );
                assert!(says("sign-in cancelled")(&r.app));
            });
        }
    }

    #[test]
    fn a_rebound_cancel_key_cancels_a_waiting_login() {
        in_local(async move {
            let mut r = rig(false, login_calls(ok((0, false))));
            start_login(&mut r).await;
            crate::keymap::with_keys(&[("wizard", "cancel", &["f5"])], || {
                r.app.on_terminal(press(KeyCode::Esc, KeyModifiers::NONE));
                assert!(r.app.flows.mcp_login().is_some(), "the old default no longer cancels");
                r.app.on_terminal(press(KeyCode::F(5), KeyModifiers::NONE));
                assert!(r.app.flows.mcp_login().is_none());
            });
        });
    }

    /// A configured server `srv` whose login starts, and whose after-login call answers `after`.
    fn login_calls(after: Result<Value, String>) -> Vec<(&'static str, Result<Value, String>)> {
        vec![
            (hmcp::MCP_NAMES, ok(vec!["srv"])),
            (hlogin::MCP_LOGIN_START, ok(())),
            (hlogin::MCP_LOGIN_CANCEL, ok(())),
            (hmcp::MCP_AFTER_LOGIN, after),
        ]
    }

    /// Starts the login of `srv` and returns its flow id.
    async fn start_login(r: &mut Rig) -> u64 {
        r.app.run_mcp_login("srv");
        settle(r, |app| app.flows.mcp_login().is_some() && says("signing in to mcp:srv")(app)).await;
        r.app.flows.mcp_login().expect("the login is waiting").id
    }

    #[test]
    fn the_wizard_asks_the_host_which_variables_are_set() {
        for deferred in [false, true] {
            in_local(async move {
                let mut r = rig(deferred, vec![(hmcp::MCP_ENV_SET, ok(vec!["GITHUB_TOKEN"]))]);
                r.app.mcp_cmd("add");
                settle(&mut r, |app| app.overlay.is_some()).await;
                assert!(matches!(&r.app.overlay, Some(Overlay::Mcp { pick: McpPick::Wizard, .. })));
                assert_eq!(r.seen.borrow()[0].1, json!(crate::mcp::mcp_presets::ENV_NAMES));

                let r = rig(deferred, vec![]);
                let set: Vec<String> =
                    crate::host::call(&*r.app.host, hmcp::MCP_ENV_SET, &vec!["PATH", "MU_SURELY_NOT_SET_X"])
                        .await
                        .unwrap();
                assert_eq!(set, vec!["PATH".to_string()]);
            });
        }
    }

    #[test]
    fn the_replies_survive_the_wire() {
        let found = Found {
            tool: "Codex".into(),
            file: "/h/.codex/config.toml".into(),
            name: "api".into(),
            entry: json!({"type": "http", "url": "https://h/mcp"}),
            kind: "http".into(),
            target: "https://h".into(),
        };
        let servers = McpServers {
            servers: vec![ServerRow {
                name: "s".into(),
                kind_label: "stdio".into(),
                target: "cmd".into(),
                level: "user".into(),
                state: "disabled".into(),
                enabled: false,
                needs_login: true,
                wants_oauth: false,
            }],
            warnings: vec!["w".into()],
            found: vec![found.clone()],
        };
        let back: McpServers = serde_json::from_value(serde_json::to_value(&servers).unwrap()).unwrap();
        assert_eq!(back.servers[0].state, "disabled");
        assert!(back.servers[0].needs_login && !back.servers[0].enabled);
        assert_eq!(back.found, vec![found]);
        let back: Applied =
            serde_json::from_value(serde_json::to_value(Applied { lines: vec!["l".into()], refresh: true }).unwrap())
                .unwrap();
        assert!(back.refresh && back.lines == ["l"]);
        let r = Reconnected { tools: 2, changed: false, ms: 9, listing: "\n  t".into() };
        let back: Reconnected = serde_json::from_value(serde_json::to_value(r).unwrap()).unwrap();
        assert_eq!((back.tools, back.changed, back.ms, back.listing.as_str()), (2, false, 9, "\n  t"));
    }
}
