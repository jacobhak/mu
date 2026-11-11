//! Results of work that runs off the event loop (pickers' data, clipboard, plugins, child programs,
//! key checks), and the periodic tick that keeps the screen current.
use super::child::ChildThen;
use super::keys::is_baseten;
use super::*;
use crate::custom::panel::Fetch;
use crate::widgets::overlay::LoginTarget;
use crate::widgets::select::{Item, Select};
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
    /// One source of the model pickers answered (the gateway, or one custom endpoint). The picker
    /// for `request` rebuilds its rows from what is cached now; `diag` says what failed.
    ModelsRefreshed {
        request: u64,
        diag: Option<String>,
    },
    /// The custom-endpoint wizard's model check answered, for the wizard `flow`.
    CustomFetched {
        flow: u64,
        r: Fetch,
    },
    LoginProviders(Result<Vec<(usize, String)>, String>),
    /// An MCP login finished: tool count and whether the tool set changed.
    McpLoggedIn(String, Result<(usize, bool), String>),
    /// An `/mcp` change was applied: what to tell the user, and whether the tool list changed.
    McpApplied {
        lines: Vec<String>,
        refresh: bool,
    },
    /// `/mcp reconnect` or `/mcp test` finished.
    McpReconnected {
        name: String,
        verbose: bool,
        ms: u128,
        result: Result<(usize, bool), String>,
    },
    ClipboardImage(Result<Option<Image>, String>),
    Ghost {
        seq: u64,
        text: String,
        ghost: String,
    },
    /// The subagents' session files were read: the reader, handed back.
    Spend(Box<TreeCost>),
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
            UiMsg::Copied(r) => match r {
                Ok(n) => self.notice(&n),
                Err(e) => self.error(&format!("copy failed: {e}")),
            },
            UiMsg::ModelsRefreshed { request, diag } => {
                let gateway = self.services.providers.cached_models();
                let session =
                    matches!(&self.overlay, Some(Overlay::Model { request: active, .. }) if *active == request);
                if session {
                    self.open_session_model_picker(request, &gateway);
                } else if !self.show_role_models(request, &gateway) {
                    return;
                }
                if let Some(d) = diag {
                    self.notice(&d);
                }
            }
            UiMsg::CustomFetched { flow, r } => self.on_custom_fetched(flow, r),
            UiMsg::McpLoggedIn(name, r) => match r {
                Ok((n, changed)) => {
                    self.notice(&format!("mcp:{name}: logged in, {n} tools"));
                    if changed {
                        self.send(Command::RefreshTools);
                    }
                }
                Err(e) => self.error(&format!("mcp:{name}: logged in, but its tools could not be listed: {e}")),
            },
            UiMsg::McpApplied { lines, refresh } => {
                self.notice(&lines.join("\n"));
                if refresh {
                    self.send(Command::RefreshTools);
                }
            }
            UiMsg::McpReconnected { name, verbose, ms, result } => self.on_mcp_reconnected(&name, verbose, ms, result),
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
            UiMsg::LoginProviders(r) => {
                self.leave_custom_wizard();
                let (mut items, mut targets) = self.api_key_rows();
                match r {
                    Ok(list) => {
                        for (n, name) in list.into_iter().filter(|(_, name)| !is_baseten(name)) {
                            items.push(Item { label: name, detail: String::new() });
                            targets.push(LoginTarget::Provider(n));
                        }
                    }
                    // The API-key rows and MCP servers stay usable without omp.
                    Err(e) => self.error(&format!("cannot list omp's login providers: {e}")),
                }
                for (name, url) in self.plugins.needs_login() {
                    items.push(Item { label: format!("mcp:{name}"), detail: url });
                    targets.push(LoginTarget::Mcp(name));
                }
                self.overlay = Some(Overlay::Login { sel: Select::new("Log in", items), targets });
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
