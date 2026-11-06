//! Pickers and overlays: opening each with its data, and carrying out what the user chose in them.

use super::child::ChildThen;
use super::*;
use crate::panels::jobs_panel::JobsPanel;
use crate::panels::model_roles::RoleEditor;
use crate::panels::plan_review::Tier;
use crate::panels::reply::ReplyPicker;
use crate::text::markdown;
use crate::widgets::overlay::{LoginTarget, OverlayEvent};
use crate::widgets::select::{Item, Select};
use crossterm::event::KeyEvent;
use mu_base::events::{Answer, AskQuestion};
use mu_base::model_config::MAIN_SET;
use mu_base::models::GatewayModel;
use mu_provider::provider::providers::KeyTarget;
use tokio::sync::oneshot;

/// How long ago `t` was, in words for the session picker.
fn ago(t: std::time::SystemTime) -> String {
    let s = t.elapsed().map_or(0, |d| d.as_secs());
    match s {
        0..=59 => "just now".into(),
        60..=3599 => format!("{}m ago", s / 60),
        3600..=86399 => format!("{}h ago", s / 3600),
        _ => format!("{}d ago", s / 86400),
    }
}

impl App {
    pub(super) fn start_ask(&mut self, qs: Vec<AskQuestion>, reply: oneshot::Sender<Option<Vec<Answer>>>) {
        if qs.is_empty() {
            let _ = reply.send(Some(vec![]));
            return;
        }
        self.overlay = Some(Overlay::new_ask(qs, reply));
    }

    pub(super) fn request_models(&mut self) {
        self.flows.model_request_seq += 1;
        let request = self.flows.model_request_seq;
        self.notice("fetching models…");
        self.overlay = Some(Overlay::Model {
            sel: Select::new("Switch model (this session)", vec![]).context("Loading models…").back_keys(true),
            ids: Vec::new(),
            request,
        });
        self.fetch_models(request);
    }

    fn request_role_models(&mut self) {
        self.flows.model_request_seq += 1;
        let request = self.flows.model_request_seq;
        if let Some(Overlay::ModelRoles { st }) = &mut self.overlay {
            st.begin_models(request);
        } else {
            return;
        }
        self.notice("fetching models…");
        self.fetch_models(request);
    }

    fn fetch_models(&self, request: u64) {
        let providers = self.services.providers.clone();
        let tx = self.ui_tx.clone();
        tokio::task::spawn_local(async move {
            let result = providers.gateway_models().await.map_err(|e| e.to_string());
            let _ = tx.send(UiMsg::Models { request, result });
        });
    }

    pub(super) fn concrete_model_rows(&self, gateway: &[GatewayModel]) -> (Vec<Item>, Vec<String>) {
        let providers = &self.services.providers;
        let mut items = Vec::new();
        let mut ids = Vec::new();
        for m in providers.direct_models() {
            if !ids.contains(&m.id) {
                items.push(Item { label: m.id.clone(), detail: chatgpt::native_label(&m.provider).into() });
                ids.push(m.id);
            }
        }
        for g in gateway {
            if ids.contains(&g.id) {
                continue;
            }
            let native = g.id.split_once('/').filter(|(p, _)| providers.is_direct(p));
            let detail = match native {
                Some((p, _)) => chatgpt::native_label(p).into(),
                None => g.context_length.map_or(String::new(), |c| format!("{}k ctx", c / 1000)),
            };
            items.push(Item { label: g.id.clone(), detail });
            ids.push(g.id.clone());
        }
        for id in providers.cfg.listed_models() {
            if !ids.contains(&id) {
                items.push(Item { label: id.clone(), detail: "models.toml".into() });
                ids.push(id);
            }
        }
        (items, ids)
    }

    pub(super) fn open_session_model_picker(&mut self, request: u64, gateway: &[GatewayModel]) {
        let mut items = Vec::new();
        let mut ids = Vec::new();
        let mut details = Vec::new();
        for role in super::slash::role_views(&self.services.providers.cfg, &self.ctx) {
            if role.value.is_none() {
                continue;
            }
            items.push(Item { label: format!("@{}", role.name), detail: role.row() });
            ids.push(format!("@{}", role.name));
            details.push(Some(format!("{}\nEnter switches this session to it.", role.detail())));
        }
        let (concrete, concrete_ids) = self.concrete_model_rows(gateway);
        items.extend(concrete);
        ids.extend(concrete_ids);
        details.resize(items.len(), None);
        let context = if items.is_empty() {
            "No models available; use /login to configure a provider"
        } else {
            "Session-only model switch"
        };
        self.overlay = Some(Overlay::Model {
            sel: Select::new("Switch model (this session)", items).context(context).details(details).back_keys(true),
            ids,
            request,
        });
    }

    pub(super) fn open_model_roles(&mut self, role: Option<String>) {
        let cfg = &self.services.providers.cfg;
        let set = cfg.active_set().unwrap_or_else(|| MAIN_SET.into());
        let path = mu_base::config::models_path();
        match RoleEditor::new(set, path, super::slash::role_views(cfg, &self.ctx), role.as_deref()) {
            Ok(st) => self.overlay = Some(Overlay::ModelRoles { st: Box::new(st) }),
            Err(e) => self.error(&e),
        }
    }

    /// Opens the picker at once with a loading row; the (capped, sorted) listing is read on a
    /// blocking thread and fills it in when it arrives.
    pub(super) fn open_session_picker(&mut self) {
        let items = vec![Item { label: "Loading…".into(), detail: String::new() }];
        self.overlay = Some(Overlay::Session { sel: Select::new("Resume session", items), paths: vec![] });
        let (cwd, tx) = (self.cwd.clone(), self.ui_tx.clone());
        tokio::task::spawn_local(async move {
            let list =
                tokio::task::spawn_blocking(move || mu_session::session::list::list_recent(&cwd, Some(&cwd), 200))
                    .await
                    .unwrap_or_default();
            let _ = tx.send(UiMsg::Sessions(list));
        });
    }

    pub(super) fn show_sessions(&mut self, list: Vec<mu_session::session::list::SessionInfo>) {
        // Only fill in a picker that is still the loading one.
        if !matches!(&self.overlay, Some(Overlay::Session { paths, .. }) if paths.is_empty()) {
            return;
        }
        if list.is_empty() {
            self.overlay = None;
            self.notice("no sessions for this directory");
            return;
        }
        let items = list
            .iter()
            .map(|s| Item {
                label: s.title.clone().unwrap_or_else(|| trunc(&s.first_prompt, 80)),
                detail: ago(s.modified),
            })
            .collect();
        let paths = list.into_iter().map(|s| s.path).collect();
        self.overlay = Some(Overlay::Session { sel: Select::new("Resume session", items), paths });
    }

    /// The prompts that can be taken back, the newest first and selected.
    pub(super) fn show_rewind(&mut self, points: Vec<mu_base::events::RewindPoint>) {
        if points.is_empty() {
            return self.notice("no prompt to go back to");
        }
        let total = points.len();
        let (items, ids): (Vec<_>, Vec<_>) = points
            .into_iter()
            .rev()
            .enumerate()
            .map(|(n, p)| {
                let first = p.text.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
                let detail = if n == 0 { "last".to_string() } else { format!("{n} back") };
                (Item { label: trunc(first, 80), detail }, p.id)
            })
            .unzip();
        let title = format!("Rewind to before a prompt ({total}); files stay as they are");
        self.overlay = Some(Overlay::Rewind { sel: Select::new(&title, items), ids });
    }

    pub(super) fn show_plans(&mut self, list: Vec<mu_base::events::PlanMeta>) {
        let deliver = std::mem::take(&mut self.flows.pick_deliver);
        if list.is_empty() {
            self.notice("no parked plans for this repository");
            return;
        }
        let items = list
            .iter()
            .map(|m| Item {
                label: m.title.clone(),
                detail: format!("{} · {}", m.id, m.updated.get(..10).unwrap_or(&m.updated)),
            })
            .collect();
        let ids = list.into_iter().map(|m| m.id).collect();
        let title = if deliver { "Deliver plan" } else { "Parked plans" };
        self.overlay = Some(Overlay::Plans { sel: Select::new(title, items), ids, deliver });
    }

    /// Opens the reply picker on the newest reply, or says there is none.
    pub(super) fn open_reply(&mut self) {
        match ReplyPicker::open(&self.agent.tr.replies) {
            Some(st) => self.overlay = Some(Overlay::Reply { st: Box::new(st) }),
            None => self.notice("no reply to quote yet"),
        }
    }

    pub(super) fn open_copy_picker(&mut self) {
        let first = |s: &str| s.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim().to_string();
        let mut items = Vec::new();
        let mut texts = Vec::new();
        for r in self.agent.tr.replies.iter().rev().take(20) {
            items.push(Item {
                label: format!("Reply: {}", trunc(&first(r), 60)),
                detail: format!("{} lines", r.lines().count()),
            });
            texts.push(r.clone());
            for (lang, body) in markdown::code_blocks(r) {
                let lang = if lang.is_empty() { "text" } else { &lang };
                items.push(Item {
                    label: format!("Code {lang}: {}", trunc(&first(&body), 50)),
                    detail: format!("{} lines", body.lines().count()),
                });
                texts.push(body);
            }
        }
        for c in self.agent.bash_cmds.iter().rev().take(10) {
            items.push(Item {
                label: format!("Command: {}", trunc(&first(c), 60)),
                detail: format!("{} lines", c.lines().count()),
            });
            texts.push(c.clone());
        }
        if items.is_empty() {
            return self.notice("nothing to copy yet");
        }
        self.overlay = Some(Overlay::Copy { sel: Select::new("Copy", items), texts });
    }

    /// The panel as this session sees it: its jobs, their output, and itself left out of the sessions.
    fn jobs_panel(&self) -> JobsPanel {
        JobsPanel::new(self.agent.jobs.clone())
            .with_outputs(self.agent.job_out.clone())
            .with_own_session(self.agent.status.session_path.as_ref().map(PathBuf::from))
    }

    /// Opens the jobs panel: on the Jobs tab, or on the Sessions tab when there are no jobs.
    pub(super) fn open_jobs(&mut self) {
        let mut panel = if self.agent.jobs.is_empty() { self.jobs_panel().on_sessions() } else { self.jobs_panel() };
        panel.poll();
        self.overlay = Some(Overlay::Jobs { st: Box::new(panel) });
    }

    pub(super) fn open_sessions(&mut self) {
        self.overlay = Some(Overlay::Jobs { st: Box::new(self.jobs_panel().on_sessions()) });
    }

    /// Says what a panel action did, in the panel when it is open.
    fn panel_status(&mut self, msg: String) {
        match &mut self.overlay {
            Some(Overlay::Jobs { st }) => st.set_status(msg),
            _ => self.notice(&msg),
        }
    }

    /// Roles from `cycle_order` that can be switched to when implementing an approved plan.
    pub(super) fn plan_tiers(&self) -> Vec<Tier> {
        let providers = &self.services.providers;
        let cfg = &providers.cfg;
        let mut out = Vec::new();
        for r in cfg.role_cycle() {
            match cfg.role(&r) {
                Some(v) if providers.resolve(&format!("@{r}")).is_ok() => {
                    out.push(Tier { model: Some(format!("@{r}")), detail: v, role: r });
                }
                None if r == "default" => out.push(Tier { role: r, detail: "current model".into(), model: None }),
                _ => {}
            }
        }
        out
    }

    pub(super) fn overlay_key(&mut self, k: KeyEvent) {
        let Some(ov) = self.overlay.take() else { return };
        let (rest, event) = ov.on_key(k);
        self.overlay = rest;
        if let Some(e) = event {
            self.on_overlay_event(e);
        }
    }

    fn save_role(&mut self, set: &str, role: &str, value: Option<&str>) {
        match self.services.edit_role(set, role, value) {
            Ok(()) => {
                let cfg = &self.services.providers.cfg;
                let active = cfg.active_set().unwrap_or_else(|| MAIN_SET.into());
                let roles = super::slash::role_views(cfg, &self.ctx);
                if let Some(Overlay::ModelRoles { st }) = &mut self.overlay {
                    st.saved(active, roles);
                }
                self.prompt.commands = super::slash::command_entries(&self.plugins, &self.ctx, cfg);
                if value.is_some() {
                    self.notice(&format!("saved @{role} in set {set}"));
                } else {
                    self.notice(&format!("cleared @{role} in set {set}"));
                }
            }
            Err(e) => self.error(&format!("could not save @{role}: {e}")),
        }
    }
    /// Carries out what the user chose in an overlay.
    pub(super) fn on_overlay_event(&mut self, e: OverlayEvent) {
        match e {
            OverlayEvent::ModelSet(name) => self.switch_to(&name),
            OverlayEvent::ModelImport(true) => self.import_now(),
            OverlayEvent::ModelImport(false) => {}
            OverlayEvent::McpTrust(true, files) => self.trust_mcp_files(files),
            OverlayEvent::McpTrust(false, _) => {}
            OverlayEvent::SetModel(id) => self.send(Command::SetModel(id)),
            OverlayEvent::LoadRoleModels => self.request_role_models(),
            OverlayEvent::SaveRole { set, role, value } => self.save_role(&set, &role, value.as_deref()),
            OverlayEvent::Login(LoginTarget::Provider(n)) => self.run_login(&[], Some(n)),
            OverlayEvent::Login(LoginTarget::Mcp(name)) => self.run_mcp_login(&name),
            OverlayEvent::Login(LoginTarget::ChatGpt) => self.chatgpt_row(),
            OverlayEvent::Login(LoginTarget::ApiKey(id)) => {
                if let Some(target) = KeyTarget::find(&id) {
                    self.begin_key(target);
                }
            }
            OverlayEvent::Key { provider, choice } => self.on_key_choice(&provider, choice),
            OverlayEvent::Logout(provider) => self.logout(&provider),
            OverlayEvent::Mcp(pick, row) => self.on_mcp_pick(pick, row),
            OverlayEvent::Web(row) => self.on_web_pick(row),
            OverlayEvent::Copy(t) => self.copy_text(t, "nothing to copy"),
            OverlayEvent::Quote(q) => self.prompt.composer.append_quotes(&q),
            OverlayEvent::Resume(p) => {
                use mu_session::session::owner::{Owner, owner};
                // A live `-p` run is stopped first, as `mu --resume` does outside the TUI.
                if matches!(owner(&p), Owner::Live { print_run: true, .. }) {
                    self.take_session(p);
                } else {
                    self.send(Command::Resume(p));
                }
            }
            OverlayEvent::Rewind(id) => self.send(Command::Rewind(id)),
            OverlayEvent::CopyPlan(text) => self.copy_async(text, "plan copied".into()),
            OverlayEvent::EditPlan(file) => self.run_editor(&file.clone(), ChildThen::PlanEdit(file)),
            OverlayEvent::PlanCancelled { stored: true } => self.notice("plan left parked"),
            OverlayEvent::PlanCancelled { stored: false } => {
                self.send(Command::Cancel);
                self.notice("plan review cancelled; plan mode stays on");
            }
            OverlayEvent::OpenPlan(id) => self.send(Command::OpenPlan(id)),
            OverlayEvent::DeliverPlan(id) => self.send(Command::Deliver(id)),
            OverlayEvent::KillJobs(id) => self.send(Command::KillJobs(id)),
            OverlayEvent::NotifySession { session, text } => {
                let msg = match self.services.send_message(&session, &self.cwd, &text) {
                    Ok(line) => format!("message to {}: {line}", mu_session::session::list::id_of(&session)),
                    Err(e) => format!("message not sent: {e}"),
                };
                self.panel_status(msg);
            }
            OverlayEvent::StopSession { session, pid, started, kind } => {
                let msg = match self.services.stop_session(&session, pid, &started, &kind) {
                    Ok(m) => m,
                    Err(e) => format!("not stopped: {e}"),
                };
                self.panel_status(msg);
            }
            OverlayEvent::TerminateSession { session, pid, started } => {
                let msg = match self.services.terminate_session(&session, pid, &started) {
                    Ok(m) => m,
                    Err(e) => format!("not terminated: {e}"),
                };
                self.panel_status(msg);
            }
            OverlayEvent::MemoryOpen(path) => self.open_memory_file(path),
            OverlayEvent::MemoryHistory => self.open_memory_history(),
            OverlayEvent::MemoryShow(sha) => self.show_memory_commit(sha),
            OverlayEvent::MemoryRevert(sha) => self.revert_memory_commit(sha),
            OverlayEvent::TakeSession(path) => self.take_session(path),
        }
    }
}
