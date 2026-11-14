//! Results of work that runs off the event loop (pickers' data, clipboard, plugins, child programs,
//! key checks), and the periodic tick that keeps the screen current.
use super::child::ChildThen;
use super::keys::is_baseten;
use super::*;
use crate::chat::blocks;
use crate::widgets::overlay::LoginTarget;
use crate::widgets::picker::Picker;
use crate::widgets::select::{Item, Select};
use mu_base::events::SubagentStatus;
use mu_base::message::Image;
use mu_provider::auth::chatgpt::Account;
use mu_provider::auth::check::Check;
use mu_provider::auth::store::Entry;
use mu_provider::provider::providers::KeyTarget;
use mu_session::session::tree_cost::TreeCost;
use serde_json::Value;

pub(super) enum UiMsg {
    ChildDone {
        then: ChildThen,
        status: Result<std::process::ExitStatus, String>,
    },
    Sessions(Vec<mu_session::session::list::SessionInfo>),
    /// The result of a clipboard copy: the notice to show, or the error.
    Copied(Result<String, String>),
    LoginProviders(Result<Vec<(usize, String)>, String>),
    ClipboardImage(Result<Option<Image>, String>),
    Ghost {
        seq: u64,
        text: String,
        ghost: String,
    },
    /// The subagents' session files were read: the reader, handed back.
    Spend(Box<TreeCost>),
    /// A finished subagent's session was read: its row, and what the session cost (`None` if the read failed).
    SubagentCost(Box<SubagentStatus>, Option<f64>),
    /// An API key was checked: the outcome, and the entry to save when it holds.
    KeyChecked {
        flow: u64,
        target: KeyTarget,
        entry: Entry,
        r: Result<Check, String>,
    },
    /// A ChatGPT sign-in is waiting: the authorization URL to show.
    ChatgptUrl {
        flow: u64,
        url: String,
    },
    /// A ChatGPT sign-in finished: the account, or why it did not.
    ChatgptDone {
        flow: u64,
        r: Result<Account, String>,
    },
    /// A ChatGPT sign-in reports a line while it waits.
    ChatgptNotice {
        flow: u64,
        text: String,
    },
    /// Revoking a signed-out ChatGPT sign-in finished.
    ChatgptRevoked(Result<(), String>),
    /// The `/memory` picker's files and notes, or why they could not be read.
    MemoryFiles(Result<Value, String>),
    MemoryLog(Result<Value, String>),
    /// A commit's message and diff.
    MemoryShown(Result<Value, String>),
    /// A revert finished; `short` names the commit.
    MemoryReverted {
        short: String,
        r: Result<Value, String>,
    },
    /// A scheduled prompt reached its deadline; the app decides when it goes out.
    ScheduleDue(u64),
    /// The host answered a call that took a while (`App::host_call`): what to do with the reply.
    Host(Box<dyn FnOnce(&mut App)>),
}

impl App {
    pub(super) fn on_ui(&mut self, m: UiMsg) {
        self.screen.dirty = true;
        match m {
            UiMsg::ChildDone { then, status } => self.on_child_done(then, status),
            UiMsg::Sessions(list) => self.show_sessions(list),
            UiMsg::Spend(t) => {
                self.agent.subagent_cost = t.total();
                self.agent.child_costs = t.costs();
                self.agent.spend = Some(*t);
                // The jobs or the session may have moved on while the files were read.
                self.poll_spend();
            }
            UiMsg::SubagentCost(row, cost) => {
                let lines = blocks::subagent_rows(std::slice::from_ref(&row), ' ', |_| cost);
                self.agent.tr.commit(lines);
            }
            UiMsg::Copied(r) => match r {
                Ok(n) => self.notice(&n),
                Err(e) => self.error(&format!("copy failed: {e}")),
            },
            UiMsg::KeyChecked { flow, target, entry, r } => self.on_key_checked(flow, target, entry, r),
            UiMsg::ChatgptUrl { flow, url } => self.on_chatgpt_url(flow, &url),
            UiMsg::ChatgptDone { flow, r } => self.on_chatgpt_done(flow, r),
            UiMsg::ChatgptNotice { flow, text } => self.on_chatgpt_notice(flow, &text),
            UiMsg::ChatgptRevoked(r) => self.on_chatgpt_revoked(r),
            UiMsg::MemoryFiles(r) => self.show_memory(r),
            UiMsg::MemoryLog(r) => self.show_memory_log(r),
            UiMsg::MemoryShown(r) => self.on_memory_shown(r),
            UiMsg::MemoryReverted { short, r } => self.on_memory_reverted(&short, r),
            UiMsg::ScheduleDue(id) => self.on_schedule_due(id),
            UiMsg::Host(then) => then(self),
            UiMsg::LoginProviders(r) => {
                self.leave_custom_wizard();
                let mut rows = self.api_key_rows();
                match r {
                    Ok(list) => {
                        for (n, name) in list.into_iter().filter(|(_, name)| !is_baseten(name)) {
                            rows.push((Item { label: name, detail: String::new() }, LoginTarget::Provider(n)));
                        }
                    }
                    // The API-key rows and MCP servers stay usable without omp.
                    Err(e) => self.error(&format!("cannot list omp's login providers: {e}")),
                }
                for (name, url) in self.plugins.needs_login() {
                    rows.push((Item { label: format!("mcp:{name}"), detail: url }, LoginTarget::Mcp(name)));
                }
                self.overlay =
                    Some(Overlay::Login { pick: Picker::from_rows(rows, |items| Select::new("Log in", items)) });
            }
            UiMsg::ClipboardImage(r) => match r {
                Ok(Some(_)) if self.typing() => self.notice("image paste ignored while typing"),
                Ok(Some(img)) if self.overlay.is_none() => self.prompt.composer.attach_image(img),
                Ok(Some(_)) => self.notice("image paste ignored in picker"),
                Ok(None) => self.notice("clipboard has no image"),
                Err(e) => self.notice(&e),
            },
            UiMsg::Ghost { seq, text, ghost } => {
                if seq == self.prompt.ghost_seq && self.overlay.is_none() && !self.key_flow_pending() {
                    self.prompt.composer.set_ghost(&text, ghost);
                }
            }
        }
    }

    /// The periodic tick, which runs while the screen is busy: it redraws, and lets an open jobs
    /// panel read the sessions it follows (the panel itself only draws what it has read).
    pub(super) fn on_tick(&mut self) {
        self.screen.dirty = true;
        self.poll_spend();
        if let Some(Overlay::Jobs { st }) = &mut self.overlay {
            st.poll();
        }
    }
}
