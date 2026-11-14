//! Pickers and overlays: opening each with its data, and carrying out what the user chose in them.

use super::child::ChildThen;
use super::*;
use crate::host::models::{self as models_host, ModelKind, ModelRow, ModelRows, RoleTable};
use crate::panels::jobs_panel::JobsPanel;
use crate::panels::model_roles::RoleEditor;
use crate::panels::plan_review::Tier;
use crate::panels::reply::ReplyPicker;
use crate::text::markdown;
use crate::widgets::overlay::{ApproveState, LoginTarget, OverlayEvent};
use crate::widgets::picker::Picker;
use crate::widgets::select::{Item, Select};
use crossterm::event::KeyEvent;
use mu_base::events::{Answer, AskQuestion};
use mu_base::model_config::RoleMeta;
use mu_base::permissions::{ApprovalDecision, ApprovalRequest};
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

/// The picker's rows for the host's model list: the label and what is said beside it, and the ids.
fn model_items(rows: Vec<ModelRow>) -> (Vec<Item>, Vec<String>) {
    let mut ids = Vec::with_capacity(rows.len());
    let items = rows
        .into_iter()
        .map(|m| {
            let detail = match &m.kind {
                ModelKind::Direct { provider, key } => key.map_or_else(
                    || chatgpt::native_label(provider).into(),
                    |k| crate::labels::key_state_word(k).to_string(),
                ),
                ModelKind::Gateway { native: Some(p), .. } => chatgpt::native_label(p).into(),
                ModelKind::Gateway { native: None, context_length } => {
                    context_length.map_or(String::new(), |c| format!("{}k ctx", c / 1000))
                }
                ModelKind::Listed => "models.toml".into(),
            };
            ids.push(m.id.clone());
            Item { label: m.id, detail }
        })
        .collect();
    (items, ids)
}

impl App {
    pub(super) fn start_ask(&mut self, qs: Vec<AskQuestion>, reply: oneshot::Sender<Option<Vec<Answer>>>) {
        if qs.is_empty() {
            let _ = reply.send(Some(vec![]));
            return;
        }
        self.set_aside_approval();
        self.open_overlay(Overlay::new_ask(qs, reply));
    }

    /// Opens `ov`, or queues it behind a pending approval, ask or plan review so the dialog
    /// on screen is not replaced (and its reply is not dropped). One queued dialog per kind,
    /// except asks and reviews, which each keep a reply.
    pub(super) fn open_overlay(&mut self, ov: Overlay) {
        if !matches!(self.overlay, Some(Overlay::Approve(_) | Overlay::Ask(_) | Overlay::Review { .. })) {
            self.overlay = Some(ov);
            return;
        }
        let replies = matches!(ov, Overlay::Ask(_) | Overlay::Review { .. });
        match self.parked.iter_mut().find(|p| !replies && std::mem::discriminant(*p) == std::mem::discriminant(&ov)) {
            Some(p) => *p = ov,
            None => self.parked.push_back(ov),
        }
    }

    /// Queues an approval and shows it unless another dialog is up.
    pub(super) fn start_approve(&mut self, request: ApprovalRequest, reply: oneshot::Sender<ApprovalDecision>) {
        self.approvals.push_back((request, reply));
        self.open_next_dialog();
    }

    /// Shows the next queued dialog once none is up: a queued picker, ask or review first, then
    /// the oldest approval (those whose turn is over are dropped).
    pub(super) fn open_next_dialog(&mut self) {
        self.approvals.retain(|(_, reply)| !reply.is_closed());
        if self.overlay.is_some() {
            return;
        }
        if let Some(ov) = self.parked.pop_front() {
            self.overlay = Some(ov);
        } else if let Some((req, reply)) = self.approvals.pop_front() {
            self.overlay = Some(Overlay::Approve(Box::new(ApproveState::new(req, reply))));
        }
    }

    /// An approval on screen gives way to another dialog: it goes back to the front of the queue
    /// unanswered, so it reopens when that dialog closes.
    pub(super) fn set_aside_approval(&mut self) {
        if matches!(self.overlay, Some(Overlay::Approve(_)))
            && let Some(Overlay::Approve(st)) = self.overlay.take()
            && let Some(pending) = st.into_pending()
        {
            self.approvals.push_front(pending);
        }
    }

    /// Opens the session picker at once with the rows already known (cached gateway models,
    /// direct and custom ones, `models.toml`), then refreshes each source; see `fetch_models`.
    pub(super) fn request_models(&mut self) {
        self.flows.model_request_seq += 1;
        let request = self.flows.model_request_seq;
        self.leave_custom_wizard();
        self.open_session_model_picker(request);
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
        self.show_role_models(request);
        self.fetch_models(request);
    }

    /// Fills the role editor's list for `request` once the host's rows arrive; nothing changes when
    /// the editor is no longer waiting for it.
    pub(super) fn show_role_models(&mut self, request: u64) {
        self.host_call(models_host::MODEL_ROWS, &(), move |app, r: Result<ModelRows, String>| {
            app.fill_role_models(request, r);
        });
    }

    /// False when the role editor is no longer waiting for `request`.
    fn fill_role_models(&mut self, request: u64, r: Result<ModelRows, String>) -> bool {
        let r = match r {
            Ok(r) => r,
            Err(e) => {
                self.error(&format!("cannot list models: {e}"));
                return false;
            }
        };
        let (items, ids) = model_items(r.rows);
        match &mut self.overlay {
            Some(Overlay::ModelRoles { st }) => st.models_loaded(request, items, ids, r.error.as_deref()),
            _ => false,
        }
    }

    /// Refreshes what the pickers list, each source in its own call: the gateway, then every custom
    /// endpoint that discovers. Each answer rebuilds the rows for `request`, and a failure keeps
    /// the rows already listed.
    fn fetch_models(&mut self, request: u64) {
        self.host_call(models_host::REFRESH_SOURCES, &(), move |app, r: Result<Vec<String>, String>| {
            for source in r.unwrap_or_default() {
                app.host_call(models_host::REFRESH_SOURCE, &source, move |app, r: Result<Option<String>, String>| {
                    let diag = r.unwrap_or_else(|e| Some(format!("models were not refreshed: {e}")));
                    app.on_models_refreshed(request, diag);
                });
            }
        });
    }

    /// One source answered: the picker for `request` rebuilds its rows from what the host has now.
    pub(super) fn on_models_refreshed(&mut self, request: u64, diag: Option<String>) {
        self.host_call(models_host::MODEL_ROWS, &(), move |app, r: Result<ModelRows, String>| {
            let session = matches!(&app.overlay, Some(Overlay::Model { request: active, .. }) if *active == request);
            if session {
                app.fill_session_model_picker(request, r);
            } else if !app.fill_role_models(request, r) {
                return;
            }
            if let Some(d) = diag {
                app.notice(&d);
            }
        });
    }

    /// Lists the session's models for `request`, keeping the highlighted one when the list is
    /// rebuilt by a refresh.
    pub(super) fn open_session_model_picker(&mut self, request: u64) {
        self.host_call(models_host::MODEL_ROWS, &(), move |app, r: Result<ModelRows, String>| {
            app.fill_session_model_picker(request, r);
        });
    }

    fn fill_session_model_picker(&mut self, request: u64, r: Result<ModelRows, String>) {
        let r = match r {
            Ok(r) => r,
            Err(e) => return self.error(&format!("cannot list models: {e}")),
        };
        let kept = match &self.overlay {
            Some(Overlay::Model { pick, .. }) => pick.highlighted().cloned(),
            _ => None,
        };
        let mut rows = Vec::new();
        let mut details = Vec::new();
        for role in r.roles {
            if role.value.is_none() {
                continue;
            }
            let id = format!("@{}", role.name);
            rows.push((Item { label: id.clone(), detail: role.row() }, id));
            details.push(Some(format!("{}\nEnter switches this session to it.", role.detail())));
        }
        let (concrete, concrete_ids) = model_items(r.rows);
        rows.extend(concrete.into_iter().zip(concrete_ids));
        details.resize(rows.len(), None);
        let context = match r.error {
            Some(e) => e,
            None if rows.is_empty() => "No models available; use /login to configure a provider".into(),
            None => "Session-only model switch".into(),
        };
        let preselect = kept.and_then(|id| rows.iter().position(|(_, m)| *m == id));
        let pick = Picker::from_rows(rows, |items| {
            let sel = Select::new("Switch model (this session)", items).context(&context).details(details);
            let sel = sel.back_keys(true);
            match preselect {
                Some(i) => sel.preselect(i),
                None => sel,
            }
        });
        self.open_overlay(Overlay::Model { pick, request });
    }

    pub(super) fn open_model_roles(&mut self, role: Option<String>) {
        self.host_call(models_host::ROLE_TABLE, &(), move |app, r: Result<RoleTable, String>| match r {
            Ok(t) => match RoleEditor::new(t.set, t.path, t.roles, role.as_deref()) {
                Ok(st) => app.open_overlay(Overlay::ModelRoles { st: Box::new(st) }),
                Err(e) => app.error(&e),
            },
            Err(e) => app.error(&format!("cannot read the model roles: {e}")),
        });
    }

    /// Opens the picker at once with a loading row; the (capped, sorted) listing is read on a
    /// blocking thread and fills it in when it arrives.
    pub(super) fn open_session_picker(&mut self) {
        let items = vec![Item { label: "Loading…".into(), detail: String::new() }];
        let pick = Picker::placeholder(items, |items| Select::new("Resume session", items));
        self.open_overlay(Overlay::Session { pick });
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
        if !matches!(&self.overlay, Some(Overlay::Session { pick }) if pick.is_empty()) {
            return;
        }
        if list.is_empty() {
            self.overlay = None;
            self.notice("no sessions for this directory");
            return;
        }
        let rows = list
            .into_iter()
            .map(|s| {
                let item = Item {
                    label: s.title.clone().unwrap_or_else(|| trunc(&s.first_prompt, 80)),
                    detail: ago(s.modified),
                };
                (item, s.path)
            })
            .collect();
        let pick = Picker::from_rows(rows, |items| Select::new("Resume session", items));
        self.open_overlay(Overlay::Session { pick });
    }

    /// The prompts that can be taken back and the replies that can be returned to, the newest
    /// first and selected.
    pub(super) fn show_rewind(&mut self, points: Vec<mu_base::events::RewindPoint>) {
        if points.is_empty() {
            return self.notice("no prompt or reply to go back to");
        }
        let total = points.len();
        let rows = points
            .into_iter()
            .rev()
            .enumerate()
            .map(|(n, p)| {
                let first = p.text.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
                let detail = if n == 0 { "last".to_string() } else { format!("{n} back") };
                let who = if p.reply { "agent" } else { "you" };
                (Item { label: format!("{who}: {}", trunc(first, 74)), detail }, p.id)
            })
            .collect();
        let title =
            format!("Rewind to before a prompt (you) or after a reply (agent) ({total}); files stay as they are");
        self.open_overlay(Overlay::Rewind { pick: Picker::from_rows(rows, |items| Select::new(&title, items)) });
    }

    pub(super) fn show_plans(&mut self, list: Vec<mu_base::events::PlanMeta>) {
        let deliver = std::mem::take(&mut self.flows.pick_deliver);
        if list.is_empty() {
            self.notice("no parked plans for this repository");
            return;
        }
        let rows = list
            .into_iter()
            .map(|m| {
                let item = Item {
                    label: m.title.clone(),
                    detail: format!("{} · {}", m.id, m.updated.get(..10).unwrap_or(&m.updated)),
                };
                (item, m.id)
            })
            .collect();
        let title = if deliver { "Deliver plan" } else { "Parked plans" };
        self.overlay =
            Some(Overlay::Plans { pick: Picker::from_rows(rows, |items| Select::new(title, items)), deliver });
    }

    /// Opens the reply picker on the newest reply, or says there is none.
    pub(super) fn open_reply(&mut self) {
        match ReplyPicker::open(&self.agent.tr.replies) {
            Some(st) => self.open_overlay(Overlay::Reply { st: Box::new(st) }),
            None => self.notice("no reply to quote yet"),
        }
    }

    pub(super) fn open_copy_picker(&mut self) {
        let first = |s: &str| s.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim().to_string();
        let mut rows = Vec::new();
        for r in self.agent.tr.replies.iter().rev().take(20) {
            let item = Item {
                label: format!("Reply: {}", trunc(&first(r), 60)),
                detail: format!("{} lines", r.lines().count()),
            };
            rows.push((item, r.clone()));
            for (lang, body) in markdown::code_blocks(r) {
                let lang = if lang.is_empty() { "text" } else { &lang };
                let item = Item {
                    label: format!("Code {lang}: {}", trunc(&first(&body), 50)),
                    detail: format!("{} lines", body.lines().count()),
                };
                rows.push((item, body));
            }
        }
        for c in self.agent.bash_cmds.iter().rev().take(10) {
            let item = Item {
                label: format!("Command: {}", trunc(&first(c), 60)),
                detail: format!("{} lines", c.lines().count()),
            };
            rows.push((item, c.clone()));
        }
        if rows.is_empty() {
            return self.notice("nothing to copy yet");
        }
        self.open_overlay(Overlay::Copy { pick: Picker::from_rows(rows, |items| Select::new("Copy", items)) });
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
        self.open_overlay(Overlay::Jobs { st: Box::new(panel) });
    }

    pub(super) fn open_sessions(&mut self) {
        self.open_overlay(Overlay::Jobs { st: Box::new(self.jobs_panel().on_sessions()) });
    }

    /// Says what a panel action did, in the panel when it is open.
    fn panel_status(&mut self, msg: String) {
        match &mut self.overlay {
            Some(Overlay::Jobs { st }) => st.set_status(msg),
            _ => self.notice(&msg),
        }
    }

    /// Roles from `cycle_order` that can be switched to when implementing an approved plan; the
    /// plan review opens with them (none when the host cannot say).
    pub(super) fn with_plan_tiers(&mut self, then: impl FnOnce(&mut App, Vec<Tier>) + 'static) {
        self.host_call(models_host::PLAN_TIERS, &(), move |app, r: Result<Vec<Tier>, String>| match r {
            Ok(tiers) => then(app, tiers),
            Err(e) => {
                app.error(&format!("cannot list the plan tiers: {e}"));
                then(app, Vec::new());
            }
        });
    }

    pub(super) fn overlay_key(&mut self, k: KeyEvent) {
        let Some(ov) = self.overlay.take() else { return };
        let (rest, event) = ov.on_key(k);
        self.overlay = rest;
        if let Some(e) = event {
            self.on_overlay_event(e);
        }
        self.open_next_dialog();
    }

    fn save_role(&mut self, set: &str, role: &str, value: Option<&str>, meta: Option<&RoleMeta>) {
        let (set, role, value) = (set.to_string(), role.to_string(), value.map(String::from));
        let (args_set, args_role, model) = (set.clone(), role.clone(), value.clone());
        let done = move |app: &mut App, saved: Result<(), String>| match saved {
            Ok(()) => {
                app.reload_role_views();
                if value.is_some() {
                    app.notice(&format!("saved @{role} in set {set}"));
                } else {
                    app.notice(&format!("cleared @{role} in set {set}"));
                }
            }
            Err(e) => app.error(&format!("could not save @{role}: {e}")),
        };
        match meta {
            Some(meta) => {
                let args = models_host::EditCustomRole { set: args_set, role: args_role, model, meta: meta.clone() };
                self.host_call(models_host::EDIT_CUSTOM_ROLE, &args, done);
            }
            None => {
                let args = models_host::EditRole { set: args_set, role: args_role, model };
                self.host_call(models_host::EDIT_ROLE, &args, done);
            }
        }
    }

    fn delete_role(&mut self, set: &str, role: &str) {
        let args = models_host::EditCustomRole {
            set: set.to_string(),
            role: role.to_string(),
            model: None,
            meta: RoleMeta::default(),
        };
        let (set, role) = (set.to_string(), role.to_string());
        self.host_call(models_host::EDIT_CUSTOM_ROLE, &args, move |app, r: Result<(), String>| match r {
            Ok(()) => {
                app.reload_role_views();
                app.notice(&format!("deleted @{role} from set {set}"));
            }
            Err(e) => app.error(&format!("could not delete @{role}: {e}")),
        });
    }

    /// Rebuilds what the role editor and the command list show from the config just written.
    fn reload_role_views(&mut self) {
        self.host_call(models_host::ROLE_TABLE, &(), |app, r: Result<RoleTable, String>| {
            match r {
                Ok(t) => {
                    if let Some(Overlay::ModelRoles { st }) = &mut app.overlay {
                        st.saved(t.set, t.roles);
                    }
                }
                Err(e) => app.error(&format!("cannot read the model roles: {e}")),
            }
            app.refresh_commands();
        });
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
            OverlayEvent::SaveRole { set, role, value, meta } => {
                self.save_role(&set, &role, value.as_deref(), meta.as_ref())
            }
            OverlayEvent::DeleteRole { set, role } => self.delete_role(&set, &role),
            OverlayEvent::Login(LoginTarget::Provider(n)) => self.run_login(&[], Some(n)),
            OverlayEvent::Login(LoginTarget::Mcp(name)) => self.run_mcp_login(&name),
            OverlayEvent::Login(LoginTarget::ChatGpt) => self.chatgpt_row(),
            OverlayEvent::Login(LoginTarget::CustomProvider(None)) => self.start_custom_wizard(),
            OverlayEvent::Login(LoginTarget::CustomProvider(Some(id))) => self.start_custom_edit(&id),
            OverlayEvent::Login(LoginTarget::ApiKey(id)) => {
                match KeyTarget::find(&id, self.services.providers.custom_registry()) {
                    Ok(Some(target)) => self.begin_key(target),
                    Ok(None) => {}
                    Err(e) => self.error(&format!("{e:#}")),
                }
            }
            OverlayEvent::Key { provider, choice } => self.on_key_choice(&provider, choice),
            OverlayEvent::Logout(provider) => self.logout(&provider),
            OverlayEvent::Mcp(pick, row) => self.on_mcp_pick(pick, row),
            OverlayEvent::Web(row) => self.on_web_pick(row),
            OverlayEvent::Custom(pick) => self.on_custom_pick(pick),
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
