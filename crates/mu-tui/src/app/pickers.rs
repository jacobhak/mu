//! Pickers and overlays: opening each with its data, and carrying out what the user chose in them.

use super::child::ChildThen;
use super::*;
use crate::panels::jobs_panel::JobsPanel;
use crate::panels::plan_review::Tier;
use crate::panels::reply::ReplyPicker;
use crate::text::markdown;
use crate::widgets::overlay::{LoginTarget, OverlayEvent};
use crate::widgets::select::{Item, Select};
use crossterm::event::KeyEvent;
use mu_base::events::{Answer, AskQuestion};
use mu_base::models::{GatewayModel, split_level};
use mu_base::roles::{self, RoleView};
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
        self.notice("fetching models…");
        let providers = self.services.providers.clone();
        let tx = self.ui_tx.clone();
        tokio::task::spawn_local(async move {
            let r = providers.gateway_models().await.map_err(|e| e.to_string());
            let _ = tx.send(UiMsg::Models(r));
        });
    }

    /// The model roles as they are shown: what each is for, and what it is set to.
    fn role_views(&self) -> Vec<RoleView> {
        let agents: Vec<(&str, &[String])> =
            self.ctx.agents.iter().map(|a| (a.name.as_str(), a.model.as_slice())).collect();
        roles::views(&self.services.providers.cfg, &agents)
    }

    /// `target` is a role without a model that the picked model is for.
    pub(super) fn open_model_picker(&mut self, gateway: Vec<GatewayModel>, target: Option<String>) {
        let providers = self.services.providers.clone();
        let mut items = Vec::new();
        let mut ids = Vec::new();
        let mut details = Vec::new();
        if target.is_none() {
            for r in self.role_views() {
                // `/switch` only changes this session, so it lists the roles that have a model.
                if r.value.is_none() && !self.flows.pick_assign {
                    continue;
                }
                items.push(Item { label: format!("@{}", r.name), detail: r.row() });
                ids.push(format!("@{}", r.name));
                let enter = if r.value.is_some() { "\nEnter switches this session to it." } else { "" };
                details.push(Some(format!("{}{enter}", r.detail())));
            }
        }
        for m in providers.direct_models() {
            items.push(Item { label: m.id.clone(), detail: chatgpt::native_label(&m.provider).into() });
            ids.push(m.id);
        }
        let known = ids.clone();
        for g in gateway.iter().filter(|g| !known.contains(&g.id)) {
            // A gateway id of a native provider with a key is routed natively: it shows the key marker.
            let native = g.id.split_once('/').filter(|(p, _)| providers.is_direct(p));
            let detail = match native {
                Some((p, _)) => chatgpt::native_label(p).into(),
                None => g.context_length.map_or(String::new(), |c| format!("{}k ctx", c / 1000)),
            };
            items.push(Item { label: g.id.clone(), detail });
            ids.push(g.id.clone());
        }
        for (p, pc) in &providers.cfg.models.providers {
            for m in &pc.models {
                let id = format!("{p}/{}", m.id);
                if !ids.contains(&id) {
                    items.push(Item { label: id.clone(), detail: "models.yml".into() });
                    ids.push(id);
                }
            }
        }
        details.resize(items.len(), None);
        let title = match (&target, self.flows.pick_assign) {
            (Some(role), _) => format!("Choose a model for @{role}"),
            (None, true) => "Select model (then choose where it applies)".to_string(),
            (None, false) => "Switch model (this session)".to_string(),
        };
        let assign = self.flows.pick_assign || target.is_some();
        self.overlay = Some(Overlay::Model { sel: Select::new(&title, items).details(details), ids, assign, target });
    }

    pub(super) fn open_role_picker(&mut self, model: String) {
        let mut items = vec![Item { label: "This session only".into(), detail: model.clone() }];
        let mut roles = vec![None];
        for r in self.role_views() {
            let now = r.value.as_ref().map_or("unset".to_string(), |v| format!("now {v}"));
            let label = if r.name == "default" {
                "Set as @default (and switch now)".into()
            } else {
                format!("Set as @{}", r.name)
            };
            items.push(Item { label, detail: format!("{} · {now}", r.purpose) });
            roles.push(Some(r.name));
        }
        self.overlay = Some(Overlay::Role { sel: Select::new(&format!("Use {model}"), items), model, roles });
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

    /// Roles from omp's `cycleOrder` that can be switched to when implementing an approved plan.
    pub(super) fn plan_tiers(&self) -> Vec<Tier> {
        let providers = &self.services.providers;
        let cfg = &providers.cfg;
        let mut out = Vec::new();
        for r in cfg.omp.role_cycle() {
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

    /// Carries out what the user chose in an overlay.
    pub(super) fn on_overlay_event(&mut self, e: OverlayEvent) {
        match e {
            OverlayEvent::SetModel(id) => match id.strip_prefix('@') {
                // A role with no model has nothing to switch to: pick one for it instead.
                Some(role) if self.services.providers.cfg.role(&split_level(role).0).is_none() => {
                    self.flows.pick_assign = true;
                    self.flows.assign_role = Some(role.to_string());
                    self.request_models();
                }
                _ => self.send(Command::SetModel(id)),
            },
            OverlayEvent::PickRole(model) => self.open_role_picker(model),
            OverlayEvent::AssignRole { role: Some(role), model } => match self.services.set_role(&role, &model) {
                Ok(v) => {
                    self.notice(&format!("@{role} = {v}"));
                    if role == "default" {
                        self.send(Command::SetModel(model));
                    }
                }
                Err(e) => self.error(&format!("could not set @{role}: {e}")),
            },
            OverlayEvent::AssignRole { role: None, model } => self.send(Command::SetModel(model)),
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
            OverlayEvent::TakeSession(path) => self.take_session(path),
        }
    }
}
