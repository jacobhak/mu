//! The agent's state, grouped by concern. Fields are visible to the `agent` module tree only.

use super::{DeliverFn, ExecPlan, PlanState};
use mu_base::events::{EventTx, GitInfo, SharedInbox};
use mu_base::models::{ModelSpec, Thinking};
use mu_base::tool::ToolDef;
use mu_provider::gateway::quota::QuotaTracker;
use mu_provider::model::cost::CostLedger;
use mu_provider::provider::client::Providers;
use mu_session::session::CompactionState;
use mu_tools::plugin::host::PluginHost;
use std::cell::{Cell, OnceCell, RefCell};
use std::collections::HashSet;
use std::path::PathBuf;
use std::rc::Rc;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio_util::sync::CancellationToken;

/// How a run is configured by its front end (`mu` print, interactive, subagent, `/review`, ACP).
/// Passed in through `AgentInit`; the agent never changes it after `new`.
#[derive(Default)]
pub struct RunConfig {
    /// Foreground or background subagent: no title, no notify inbox.
    pub subagent: bool,
    /// Tool names hidden from the model (e.g. `ask` when headless).
    pub excluded: Vec<&'static str>,
    /// Only these tools are offered when set.
    pub allow: Option<Vec<String>>,
    /// Custom-agent instructions, appended verbatim to the system prompt.
    pub agent_prompt: Option<String>,
    pub parent_session: Option<PathBuf>,
    /// `-p --resume`: when the session cannot be opened, the run ends instead of starting a fresh one.
    pub exit_on_resume_failure: bool,
    /// `--resume` with an explicit `--model`: the resumed session keeps the flag's model.
    pub pin_model: bool,
    /// `--resume` with an explicit `--thinking`: the resumed session keeps the flag's level.
    pub pin_thinking: bool,
}

/// Wiring to the outside world: providers, plugins, the event channel, the inbox and the `mu deliver` starter.
pub(super) struct Ports {
    pub(super) providers: Rc<Providers>,
    pub(super) plugins: Rc<PluginHost>,
    pub(super) events: EventTx,
    pub(super) inbox: SharedInbox,
    pub(super) deliver: DeliverFn,
}

pub(super) struct ModelState {
    pub(super) model: ModelSpec,
    pub(super) thinking: Thinking,
    /// Mirror of `thinking` that the request reads, so a level set mid-turn (or during retries)
    /// applies to the next attempt.
    pub(super) live_thinking: Rc<Cell<Thinking>>,
    pub(super) pricing_checked: bool,
}

pub(super) struct TitleState {
    pub(super) text: Option<String>,
    pub(super) requested: bool,
    /// Cancels an in-flight title request when the session ends.
    pub(super) cancel: CancellationToken,
    pub(super) tx: UnboundedSender<(String, String)>,
    pub(super) rx: Option<UnboundedReceiver<(String, String)>>,
}

pub(super) struct PlanFlow {
    pub(super) plan: Option<PlanState>,
    pub(super) exec_plan: Option<ExecPlan>,
    /// The stored plan being revised in plan mode; `propose_plan` updates it in place.
    pub(super) stored_plan: Option<String>,
    pub(super) review_base: Option<crate::review::ReviewBase>,
}

/// Wake-ups and `mu notify` delivery.
pub(super) struct Mailbox {
    pub(super) wake_rx: Option<UnboundedReceiver<()>>,
    pub(super) wake_tx: Option<UnboundedSender<()>>,
    /// Interactive only: drain `mu notify` messages (the SIGUSR2 handler is installed).
    pub(super) notify: bool,
    /// Inbox files that were handed over but could not be deleted (session write failed).
    pub(super) seen: HashSet<String>,
    /// Inbox files announced when the session was opened; they wait for the next prompt.
    pub(super) announced: HashSet<String>,
}

pub(super) struct Costs {
    pub(super) cost: CostLedger,
    pub(super) quota: QuotaTracker,
    pub(super) git: Option<GitInfo>,
}

pub(super) struct CompactionFlow {
    pub(super) state: Option<CompactionState>,
    /// Index into `messages` from which assistant usage reflects the current context.
    pub(super) fresh_from: usize,
    pub(super) since: usize,
}

/// Derived values kept between requests.
#[derive(Default)]
pub(super) struct Memo {
    /// Built-in and plugin tool definitions, built on the first request: plugin schemas are not
    /// read at startup.
    pub(super) base_defs: OnceCell<Vec<ToolDef>>,
    /// `(inputs hash, tokens)` of the system prompt, see `system_tokens`.
    pub(super) system: RefCell<Option<(u64, u64)>>,
    /// `((message count, last timestamp), tokens)`, see `messages_tokens`.
    pub(super) messages: RefCell<Option<((usize, i64), u64)>>,
}
