//! Results of work that runs off the event loop (pickers' data, clipboard, plugins, child programs,
//! key checks), and the periodic tick that keeps the screen current.
use super::child::ChildThen;
use super::*;
use crate::chat::blocks;
use mu_base::events::SubagentStatus;
use mu_base::message::Image;
use mu_session::session::tree_cost::TreeCost;

pub(super) enum UiMsg {
    ChildDone {
        then: ChildThen,
        status: Result<std::process::ExitStatus, String>,
    },
    Sessions(Vec<mu_session::session::list::SessionInfo>),
    /// The result of a clipboard copy: the notice to show, or the error.
    Copied(Result<String, String>),
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
            UiMsg::ScheduleDue(id) => self.on_schedule_due(id),
            UiMsg::Host(then) => then(self),
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
