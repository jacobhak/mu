//! Pickers and overlays: opening each with its data, and carrying out what the user chose in them.

use super::child::ChildThen;
use super::*;
use crate::host::keys::KeyInfo;
use crate::host::models::{self as models_host, ModelKind, ModelRow, ModelRows, RoleTable};
use crate::host::results::{Applied, RESULT_APPLY, RESULT_DIFF};
use crate::host::sessions::{self, Live, TailDelta};
use crate::panels::doc_review::Tier;
use crate::panels::jobs_panel::{JobsPanel, Want};
use crate::panels::model_roles::RoleEditor;
use crate::panels::reply::ReplyPicker;
use crate::text::markdown;
use crate::widgets::overlay::{ApproveState, LoginTarget, OverlayEvent};
use crate::widgets::picker::Picker;
use crate::widgets::select::{Item, Select};
use crossterm::event::KeyEvent;
use mu_base::events::{Answer, AskQuestion};
use mu_base::model_config::RoleMeta;
use mu_base::permissions::{ApprovalDecision, ApprovalRequest};
use mu_session::session::list::SessionInfo;
use mu_session::session::owner::Owner;
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

    /// Closes the dialogs whose request is gone (answered on another client, or not asked again after a
    /// reconnect).
    pub(super) fn close_stale_dialogs(&mut self) {
        if self.overlay.as_ref().is_some_and(Overlay::request_gone) {
            self.overlay = None;
        }
        self.parked.retain(|o| !o.request_gone());
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
        self.host_call(sessions::RECENT_SESSIONS, &(), |app, r: Result<Vec<SessionInfo>, String>| {
            app.show_sessions(r.unwrap_or_default())
        });
    }

    pub(super) fn show_sessions(&mut self, list: Vec<SessionInfo>) {
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
                let mode = if m.mode == "plan" { String::new() } else { format!(" · {}", m.mode) };
                let item = Item {
                    label: m.title.clone(),
                    detail: format!("{} · {}{mode}", m.id, m.updated.get(..10).unwrap_or(&m.updated)),
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

    /// `/open` with no path: files the agent touched or mentioned, newest first.
    pub(super) fn open_recent_picker(&mut self) {
        let rows: Vec<(Item, String)> = self
            .agent
            .recent_files
            .iter()
            .map(|p| (Item { label: p.clone(), detail: String::new() }, p.clone()))
            .collect();
        if rows.is_empty() {
            return self.notice("the agent has not touched or mentioned a file yet: /open <path>");
        }
        self.open_overlay(Overlay::Open { pick: Picker::from_rows(rows, |items| Select::new("Open a file", items)) });
    }

    /// The panel as this session sees it: its jobs, their output, and itself left out of the sessions.
    fn jobs_panel(&self) -> JobsPanel {
        JobsPanel::new(self.agent.jobs.clone())
            .with_outputs(self.agent.job_out.clone())
            .with_own_session(self.agent.status.session_path.as_ref().map(PathBuf::from))
            .with_home(&self.host.info().home)
    }

    /// Opens the jobs panel: on the Jobs tab, or on the Sessions tab when there are no jobs.
    pub(super) fn open_jobs(&mut self) {
        let mut panel = if self.agent.jobs.is_empty() { self.jobs_panel().on_sessions() } else { self.jobs_panel() };
        panel.poll();
        self.open_overlay(Overlay::Jobs { st: Box::new(panel) });
        self.service_panel();
    }

    pub(super) fn open_sessions(&mut self) {
        self.open_overlay(Overlay::Jobs { st: Box::new(self.jobs_panel().on_sessions()) });
        self.service_panel();
    }

    /// Makes the reads the open jobs panel waits for; each reply goes back into the panel.
    pub(super) fn service_panel(&mut self) {
        let Some(Overlay::Jobs { st }) = &mut self.overlay else { return };
        for want in st.take_wants() {
            match want {
                Want::Sessions(args) => {
                    self.host_call(sessions::LIVE_SESSIONS, &args, |app, r: Result<Live, String>| {
                        if let Some(Overlay::Jobs { st }) = &mut app.overlay {
                            st.on_live(r);
                        }
                        app.service_panel();
                    })
                }
                Want::Tail { target, args } => {
                    let path = args.path.clone();
                    self.host_call(sessions::TAIL_READ, &args, move |app, r: Result<TailDelta, String>| {
                        if let Some(Overlay::Jobs { st }) = &mut app.overlay {
                            st.on_tail(target, &path, r);
                        }
                    })
                }
            }
        }
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
        self.service_panel();
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
            OverlayEvent::ModelImport(Some(source)) => self.import_now(Some(&source)),
            OverlayEvent::ModelImport(None) => {}
            OverlayEvent::McpTrust(true, files) => self.trust_mcp_files(files),
            OverlayEvent::McpTrust(false, _) => {}
            OverlayEvent::SetModel(id) => self.send(Command::SetModel(id)),
            OverlayEvent::LoadRoleModels => self.request_role_models(),
            OverlayEvent::SaveRole { set, role, value, meta } => {
                self.save_role(&set, &role, value.as_deref(), meta.as_ref())
            }
            OverlayEvent::DeleteRole { set, role } => self.delete_role(&set, &role),
            OverlayEvent::Login(LoginTarget::ChatGpt) => self.chatgpt_row(),
            OverlayEvent::Login(LoginTarget::CustomProvider(None)) => self.start_custom_wizard(),
            OverlayEvent::Login(LoginTarget::CustomProvider(Some(id))) => self.start_custom_edit(&id),
            OverlayEvent::Login(LoginTarget::ApiKey(id)) => {
                self.host_call(host::keys::KEY_TARGET, &id, |app, r: Result<Option<KeyInfo>, String>| match r {
                    Ok(Some(target)) => app.begin_key(target),
                    Ok(None) => {}
                    Err(e) => app.error(&e),
                })
            }
            OverlayEvent::Key { provider, choice } => self.on_key_choice(&provider, choice),
            OverlayEvent::Logout(provider) => self.logout(&provider),
            OverlayEvent::Mcp(pick, row) => self.on_mcp_pick(pick, row),
            OverlayEvent::Web(row) => self.on_web_pick(row),
            OverlayEvent::Custom(pick) => self.on_custom_pick(pick),
            OverlayEvent::Copy(t) => self.copy_text(t, "nothing to copy"),
            OverlayEvent::Open(path) => self.open_cmd(&path),
            OverlayEvent::OpenPorts { job, ports } => self.open_ports(job, ports),
            OverlayEvent::Quote(q) => self.prompt.composer.append_quotes(&q),
            OverlayEvent::Resume(p) => {
                let path = p.clone();
                self.host_call(sessions::SESSION_OWNER, &p, move |app, r: Result<Owner, String>| {
                    // A live `-p` run is stopped first, as `mu --resume` does outside the TUI.
                    if matches!(r, Ok(Owner::Live { print_run: true, .. })) {
                        app.take_session(path);
                    } else {
                        app.send(Command::Resume(path));
                    }
                });
            }
            OverlayEvent::Rewind(id) => self.send(Command::Rewind(id)),
            OverlayEvent::CopyDoc(text) => self.copy_async(text, "plan copied".into()),
            OverlayEvent::EditDoc(file) => self.edit_host_file(file, false, "plan", ChildThen::DocEdit),
            OverlayEvent::ReviewCancelled { cancel_turn, notice } => {
                if cancel_turn {
                    self.send(Command::Cancel);
                }
                self.notice(&notice);
            }
            OverlayEvent::OpenPlan(id) => self.send(Command::OpenPlan(id)),
            OverlayEvent::DeliverPlan(id) => self.send(Command::Deliver(id)),
            OverlayEvent::KillJobs(id) => self.send(Command::KillJobs(id)),
            OverlayEvent::NotifySession { session, text } => {
                let id = mu_session::session::list::id_of(&session);
                self.host_call(
                    sessions::SEND_MESSAGE,
                    &sessions::MessageArgs { session, text },
                    move |app, r: Result<String, String>| {
                        let msg = match r {
                            Ok(line) => format!("message to {id}: {line}"),
                            Err(e) => format!("message not sent: {e}"),
                        };
                        app.panel_status(msg);
                    },
                );
            }
            OverlayEvent::StopSession { session, pid, started, kind } => {
                self.host_call(
                    sessions::STOP_SESSION,
                    &sessions::StopArgs { session, pid, started, kind },
                    |app, r: Result<String, String>| {
                        let msg = match r {
                            Ok(m) => m,
                            Err(e) => format!("not stopped: {e}"),
                        };
                        app.panel_status(msg);
                    },
                );
            }
            OverlayEvent::TerminateSession { session, pid, started } => {
                self.host_call(
                    sessions::TERMINATE_SESSION,
                    &sessions::TerminateArgs { session, pid, started },
                    |app, r: Result<String, String>| {
                        let msg = match r {
                            Ok(m) => m,
                            Err(e) => format!("not terminated: {e}"),
                        };
                        app.panel_status(msg);
                    },
                );
            }
            OverlayEvent::RemoteAction { host, id, action, text } => {
                self.host_call(
                    sessions::REMOTE_ACTION,
                    &sessions::RemoteActionArgs { host: host.clone(), id: id.clone(), action: action.into(), text },
                    move |app, r: Result<String, String>| {
                        let msg = match r {
                            Ok(line) => format!("{action} {id} on {host}: {line}"),
                            Err(e) => format!("{action} failed for {id} on {host}: {e}"),
                        };
                        app.panel_status(msg);
                    },
                );
            }
            OverlayEvent::SetTheme(source) => self.apply_theme(&source),
            OverlayEvent::MemoryOpen(path) => self.open_memory_file(path),
            OverlayEvent::MemoryHistory => self.open_memory_history(),
            OverlayEvent::MemoryShow(sha) => self.show_memory_commit(sha),
            OverlayEvent::MemoryRevert(sha) => self.revert_memory_commit(sha),
            OverlayEvent::TakeSession(path) => self.take_session(path),
            OverlayEvent::AttachSession(target) => self.attach_session(target),
            OverlayEvent::DiffResult { job, result } => {
                self.host_call(RESULT_DIFF, &result, move |app, r: Result<String, String>| match r {
                    Ok(diff) => {
                        if let Some(Overlay::Jobs { st }) = &mut app.overlay {
                            st.show_diff(&job, &diff);
                        }
                    }
                    Err(e) => app.panel_status(format!("no diff: {e}")),
                });
            }
            OverlayEvent::ApplyResult { job, result } => {
                self.host_call(RESULT_APPLY, &result, move |app, r: Result<Applied, String>| {
                    let msg = match r {
                        Ok(a) if a.conflicts.is_empty() => {
                            if let Some(Overlay::Jobs { st }) = &mut app.overlay {
                                st.mark_applied(&job);
                            }
                            format!("applied {} files of {job}", a.files)
                        }
                        Ok(a) => {
                            if let Some(Overlay::Jobs { st }) = &mut app.overlay {
                                st.mark_applied(&job);
                            }
                            format!("applied {job} with conflicts in {}", a.conflicts.join(", "))
                        }
                        Err(e) => format!("not applied: {e}"),
                    };
                    app.panel_status(msg);
                });
            }
        }
    }
}

#[cfg(test)]
mod session_tests {
    use super::super::keys::tests::{in_local, test_app_with_commands, transcript_text};
    use super::*;
    use crate::host::local::LocalHost;
    use crate::host::sessions::Exists;
    use crate::host::{self, Deferred};
    use crate::text::line::line_text;
    use crate::widgets::overlay::OverlayEvent;
    use crossterm::event::KeyCode;
    use mu_base::events::{JobKind, SubagentState, SubagentStatus};
    use mu_session::session::owner::LiveSession;
    use mu_session::session::runs::SessionSource;
    use mu_testutil::key;
    use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

    /// A session file with a header, one prompt and a reply that cost `cost`.
    fn session_file(dir: &std::path::Path, name: &str, cwd: &std::path::Path, prompt: &str, cost: f64) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join(format!("2026-01-01T00-00-00-000Z_{name}.jsonl"));
        let header = serde_json::json!({"type":"session","version":3,"id":name,"timestamp":"t","cwd":cwd});
        let user = serde_json::json!({"type":"message","id":"u","parentId":null,
            "message":{"role":"user","content":prompt,"timestamp":1}});
        let reply = serde_json::json!({"type":"message","id":"a","parentId":"u","message":{"role":"assistant",
            "content":[{"type":"text","text":"On it."}],"api":"a","provider":"p","model":"m",
            "usage":{"input":10,"output":5,"cacheRead":0,"cacheWrite":0,"totalTokens":0,"cost":{"total":cost}},
            "stopReason":"stop","timestamp":2}});
        std::fs::write(&path, format!("{header}\n{user}\n{reply}\n")).unwrap();
        path
    }

    fn fresh_dir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("mu-sessions-ui-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// An app whose host answers at once, or later (`deferred`); its UI messages come out of the receiver.
    fn app_with(deferred: bool, cwd: PathBuf) -> (App, UnboundedReceiver<UiMsg>, UnboundedReceiver<Command>) {
        let (mut app, commands) = test_app_with_commands(std::env::temp_dir().join("mu-unused-history.jsonl"));
        app.cwd = cwd.clone();
        let local = LocalHost::new(app.services.providers.clone(), app.ctx.clone(), app.plugins.clone(), cwd);
        app.host = if deferred { Rc::new(Deferred(local)) } else { Rc::new(local) };
        let (ui_tx, ui_rx) = unbounded_channel();
        app.ui_tx = ui_tx;
        (app, ui_rx, commands)
    }

    /// Handles UI messages until `done` holds.
    async fn pump(app: &mut App, rx: &mut UnboundedReceiver<UiMsg>, mut done: impl FnMut(&mut App) -> bool) {
        let wait = async {
            while !done(app) {
                let Some(m) = rx.recv().await else { return };
                app.on_ui(m);
            }
        };
        tokio::time::timeout(std::time::Duration::from_secs(10), wait).await.expect("the reply never came");
    }

    fn panel_text(app: &mut App) -> String {
        match &mut app.overlay {
            Some(Overlay::Jobs { st }) => {
                st.render(120, 20, std::time::SystemTime::now()).0.iter().map(line_text).collect::<Vec<_>>().join("\n")
            }
            _ => String::new(),
        }
    }

    struct Fixed(Vec<LiveSession>);

    impl SessionSource for Fixed {
        fn live_sessions(&self) -> Vec<LiveSession> {
            self.0.clone()
        }
    }

    fn live(path: &std::path::Path, kind: &str) -> LiveSession {
        LiveSession {
            path: path.to_path_buf(),
            pid: 4242,
            print_run: kind == "print",
            inbox: kind == "tui",
            started: "2026-01-01T00:00:00Z".into(),
            kind: Some(kind.into()),
            agent: None,
            cwd: Some(PathBuf::from("/work/other")),
            parent: None,
            abort: false,
            waiting: false,
            locked: false,
            client: None,
        }
    }

    fn serve_runs(lives: Vec<LiveSession>) {
        host::sessions::TEST_SOURCE.with(|s| *s.borrow_mut() = Some(Rc::new(Fixed(lives))));
    }

    #[test]
    fn the_resume_picker_lists_this_directorys_sessions_with_either_host() {
        mu_testutil::home();
        for deferred in [false, true] {
            let cwd = fresh_dir();
            let (mut app, mut rx, _cmds) = app_with(deferred, cwd.clone());
            in_local(async {
                app.open_session_picker();
                pump(&mut app, &mut rx, |a| transcript_text(a).contains("no sessions for this directory")).await;
                assert!(app.overlay.is_none(), "{deferred}");
            });
            session_file(&mu_common::paths::session_dir(&cwd), "abc", &cwd, "Fix the flaky test", 0.5);
            in_local(async {
                app.open_session_picker();
                pump(&mut app, &mut rx, |a| matches!(&a.overlay, Some(Overlay::Session { pick }) if !pick.is_empty()))
                    .await;
                let Some(Overlay::Session { pick }) = &app.overlay else { panic!("no picker") };
                assert_eq!(pick.values().len(), 1, "{deferred}");
            });
        }
    }

    #[test]
    fn resuming_a_free_session_asks_the_agent_with_either_host() {
        mu_testutil::home();
        for deferred in [false, true] {
            let cwd = fresh_dir();
            let path = session_file(&cwd, "free", &cwd, "hi", 0.0);
            let (mut app, mut rx, mut cmds) = app_with(deferred, cwd);
            let mut got = Vec::new();
            in_local(async {
                app.on_overlay_event(OverlayEvent::Resume(path.clone()));
                pump(&mut app, &mut rx, |_| {
                    got.extend(std::iter::from_fn(|| cmds.try_recv().ok()));
                    !got.is_empty()
                })
                .await;
            });
            assert!(matches!(got.as_slice(), [Command::Resume(p)] if *p == path), "{deferred}");
        }
    }

    #[test]
    fn the_exit_hint_asks_the_host_whether_the_session_file_exists() {
        for deferred in [false, true] {
            let cwd = fresh_dir();
            let path = session_file(&cwd, "here", &cwd, "hi", 0.0);
            let (app, _rx, _cmds) = app_with(deferred, cwd.clone());
            in_local(async {
                let r: Exists = host::call(app.host.as_ref(), host::sessions::SESSION_EXISTS, &path).await.unwrap();
                assert!(r.exists, "{deferred}");
                let gone = cwd.join("no.jsonl");
                let r: Exists = host::call(app.host.as_ref(), host::sessions::SESSION_EXISTS, &gone).await.unwrap();
                assert!(!r.exists, "{deferred}");
            });
        }
    }

    #[test]
    fn the_sessions_tab_shows_titles_and_costs_with_either_host() {
        mu_testutil::home();
        for deferred in [false, true] {
            let dir = fresh_dir();
            let path = session_file(&dir, "other", &dir, "Fix the flaky test", 0.5);
            serve_runs(vec![live(&path, "print")]);
            let (mut app, mut rx, _cmds) = app_with(deferred, dir);
            in_local(async {
                app.open_sessions();
                pump(&mut app, &mut rx, |a| {
                    let t = panel_text(a);
                    t.contains("Fix the flaky test") && t.contains("$0.50")
                })
                .await;
                assert!(panel_text(&mut app).contains("sessions · 1 live"), "{deferred}");
            });
        }
    }

    #[test]
    fn a_job_transcript_opens_and_costs_come_through_either_host() {
        mu_testutil::home();
        for deferred in [false, true] {
            let dir = fresh_dir();
            let path = session_file(&dir, "kid", &dir, "Find the bug", 0.5);
            let job = SubagentStatus {
                name: "scout-1".into(),
                agent: "scout".into(),
                tools: 1,
                last: String::new(),
                state: SubagentState::Running,
                kind: JobKind::Subagent,
                cwd: dir.clone(),
                task: "look".into(),
                started: std::time::SystemTime::now(),
                ended: None,
                delivered: false,
                note: None,
                session: Some(path),
                model: None,
                changes: None,
                progress: None,
                ports: Vec::new(),
            };
            let (mut app, mut rx, _cmds) = app_with(deferred, dir);
            app.agent.jobs = vec![job];
            in_local(async {
                app.open_jobs();
                pump(&mut app, &mut rx, |a| panel_text(a).contains("$0.50")).await;
                app.overlay_key(key(KeyCode::Enter));
                pump(&mut app, &mut rx, |a| {
                    let t = panel_text(a);
                    t.contains("Find the bug") && t.contains("On it.")
                })
                .await;
            });
        }
    }

    #[test]
    fn messages_stops_and_terminations_report_what_the_host_did() {
        mu_testutil::home();
        for deferred in [false, true] {
            let dir = fresh_dir();
            let path = session_file(&dir, "other", &dir, "hi", 0.0);
            serve_runs(vec![live(&path, "tui")]);
            let (mut app, mut rx, _cmds) = app_with(deferred, dir);
            in_local(async {
                app.open_sessions();
                app.on_overlay_event(OverlayEvent::NotifySession { session: path.clone(), text: "hello".into() });
                pump(&mut app, &mut rx, |a| panel_text(a).contains("message to other: queued")).await;
                let (session, started) = (path.clone(), "x".to_string());
                app.on_overlay_event(OverlayEvent::StopSession {
                    session: session.clone(),
                    pid: 4242,
                    started: started.clone(),
                    kind: "print".into(),
                });
                pump(&mut app, &mut rx, |a| panel_text(a).contains("not stopped:")).await;
                app.on_overlay_event(OverlayEvent::TerminateSession { session, pid: 4242, started });
                pump(&mut app, &mut rx, |a| panel_text(a).contains("not terminated:")).await;
            });
        }
    }

    #[test]
    fn agents_lists_the_agents_here_and_acts_on_one_with_either_host() {
        mu_testutil::home();
        for deferred in [false, true] {
            let dir = fresh_dir();
            let path = session_file(&dir, "other", &dir, "hi", 0.0);
            serve_runs(vec![live(&path, "agent"), live(&session_file(&dir, "tui1", &dir, "hi", 0.0), "tui")]);
            let (mut app, mut rx, _cmds) = app_with(deferred, dir);
            in_local(async {
                app.agents_cmd("");
                pump(&mut app, &mut rx, |a| transcript_text(a).contains("here  other  ")).await;
                let t = transcript_text(&app);
                assert!(t.contains("[stop cancel notify attach]") && !t.contains("tui1"), "{deferred}: {t}");
                app.agents_cmd("stop oth");
                pump(&mut app, &mut rx, |a| transcript_text(a).contains("not terminated:")).await;
                app.agents_cmd("stop zz");
                pump(&mut app, &mut rx, |a| transcript_text(a).contains("no agent matches zz")).await;
                app.agents_cmd("nosuch");
                app.agents_cmd("notify other");
                let t = transcript_text(&app);
                assert!(t.contains("usage: /agents [stop|cancel|notify|attach <id> [text]]"), "{deferred}: {t}");
                assert!(t.contains("usage: /agents notify <id> <text>"), "{deferred}: {t}");
            });
        }
    }
}
