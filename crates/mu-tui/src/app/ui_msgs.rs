//! Results of work that runs off the event loop (pickers' data, clipboard, plugins, child programs,
//! key checks), and the periodic tick that keeps the screen current.
use super::child::ChildThen;
use super::keys::is_baseten;
use super::*;
use crate::widgets::overlay::LoginTarget;
use crate::widgets::select::{Item, Select};
use mu_base::message::Image;
use mu_base::models::GatewayModel;
use mu_provider::auth::check::Check;
use mu_provider::auth::store::Entry;
use mu_provider::provider::providers::KeyTarget;
use mu_session::session::tree_cost::TreeCost;

pub(super) enum UiMsg {
    ChildDone {
        then: ChildThen,
        status: Result<std::process::ExitStatus, String>,
    },
    Sessions(Vec<mu_session::session::list::SessionInfo>),
    /// The result of a clipboard copy: the notice to show, or the error.
    Copied(Result<String, String>),
    Models(Result<Vec<GatewayModel>, String>),
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
            UiMsg::Models(r) => {
                let target = self.flows.assign_role.take();
                let gw = match r {
                    Ok(g) => g,
                    Err(e) => {
                        // Native models are listed either way; only a session without any says why.
                        if self.services.providers.direct_models().is_empty() {
                            self.error(&format!("could not fetch gateway models: {e}"));
                        } else {
                            self.notice(&format!("gateway models unavailable: {e}"));
                        }
                        vec![]
                    }
                };
                self.open_model_picker(gw, target);
            }
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
            UiMsg::LoginProviders(r) => {
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
                if seq == self.prompt.ghost_seq && self.overlay.is_none() {
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
