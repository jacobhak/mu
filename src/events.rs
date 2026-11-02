use crate::gitinfo::GitInfo;
use crate::message::UserInput;
use crate::models::Thinking;
use crate::quota::ProviderUsage;
use crate::tools::ToolOutput;
use serde_json::Value;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, SystemTime};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

pub enum Command {
    Prompt(UserInput),
    /// Creates the session file now, so its id is known before the first prompt.
    EnsureSession,
    SetModel(String),
    SetThinking(Thinking),
    NewSession,
    Compact(Option<String>),
    /// `/review [focus]`.
    Review(Option<String>),
    TogglePlan(Option<String>),
    Resume(PathBuf),
    Bash {
        cmd: String,
        include: bool,
    },
    Rename(String),
    /// Cancel one background job by id, or all of them.
    KillJobs(Option<String>),
    /// An MCP login added tools: rebuild the tool list for the next request (one prompt-cache miss).
    RefreshTools,
    Exit,
    /// List the repository's pending plans.
    ListPlans,
    /// Open a parked plan in the review dialog.
    OpenPlan(String),
    /// `/deliver <id>`: start `mu deliver <id> --worktree` detached, notifying this session when it ends.
    Deliver(String),
    /// Approve a parked plan without a dialog (headless).
    StartPlan {
        id: String,
        decision: PlanDecision,
        anyway: bool,
    },
    /// Revise a parked plan in plan mode.
    RefinePlan {
        id: String,
        text: Option<String>,
    },
}

/// Messages the UI queues while a turn runs, plus the turn's cancellation token.
pub struct Inbox {
    pub steer: VecDeque<UserInput>,
    pub follow_up: VecDeque<UserInput>,
    pub cancel: CancellationToken,
    /// Set by a headless run when asked to stop: finish at the next tool/turn boundary.
    pub stop: bool,
}

pub type SharedInbox = Rc<RefCell<Inbox>>;

pub fn new_inbox() -> SharedInbox {
    Rc::new(RefCell::new(Inbox {
        steer: VecDeque::new(),
        follow_up: VecDeque::new(),
        cancel: CancellationToken::new(),
        stop: false,
    }))
}

#[derive(Clone, Debug, Default)]
pub struct StatusSnapshot {
    /// `provider/model`.
    pub model_id: String,
    /// Display name, e.g. `Haiku 5.5`.
    pub model_name: String,
    /// The model supports thinking levels.
    pub reasoning: bool,
    /// `off|low|medium|high`.
    pub thinking: String,
    /// Served through the gateway, so spend is subscription-billed (shown as `S`, not `$`).
    pub subscription: bool,
    pub plan: bool,
    /// Estimated context tokens of the next request: the last response's usage plus an estimate for
    /// what was added since. `None` only when a producer cannot estimate.
    pub context_tokens: Option<u64>,
    pub context_window: u32,
    /// Session spend in dollars (from catalogue prices).
    pub cost: f64,
    /// Session totals, shown by the headless result.
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub title: Option<String>,
    pub session_path: Option<String>,
    pub git: Option<GitInfo>,
    /// Quota windows of the current provider, when the gateway reports them.
    pub usage: Option<ProviderUsage>,
}

/// What an ask preview shows; a frontend styles its box by it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PreviewKind {
    Code,
    Diff,
    Layout,
    Text,
}

#[derive(Clone, Debug)]
pub struct AskOption {
    pub label: String,
    pub description: Option<String>,
    /// Monospace block shown with the option (code, diff, layout). Frontends without a preview
    /// pane may ignore it.
    pub preview: Option<String>,
    /// Declared content of `preview`; frontends guess it when `None`.
    pub kind: Option<PreviewKind>,
}

#[derive(Clone, Debug)]
pub struct AskQuestion {
    pub question: String,
    /// Why the question comes up and what hangs on it, so the dialog reads on its own. Shown above
    /// the question, dimmed.
    pub context: Option<String>,
    pub header: Option<String>,
    pub options: Vec<AskOption>,
    pub multi: bool,
    pub recommended: Option<usize>,
    /// A yes/no question: `options` are Yes and No, and the picker answers on `y`/`n`.
    pub confirm: bool,
}

#[derive(Clone, Debug)]
pub enum Answer {
    Options(Vec<String>),
    /// Picked options (one, or several when multi) with a free-text note attached.
    Noted {
        picks: Vec<String>,
        note: String,
    },
    Custom(String),
    Cancelled,
}

pub enum PlanDecision {
    Execute {
        model: Option<String>,
    },
    Compact {
        model: Option<String>,
    },
    Keep {
        model: Option<String>,
    },
    Refine(String),
    /// Save the plan for later approval and end the turn (live review only).
    Park,
    /// Drop a stored plan (stored review only).
    Drop,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SubagentState {
    Running,
    Done,
    Failed,
}

/// What a background job runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobKind {
    Subagent,
    Bash,
}

#[derive(Clone, Debug)]
pub struct SubagentStatus {
    pub name: String,
    pub agent: String,
    pub tools: u32,
    pub last: String,
    pub state: SubagentState,
    pub kind: JobKind,
    pub cwd: PathBuf,
    /// The task prompt or the command, truncated.
    pub task: String,
    pub started: SystemTime,
    /// Set once the job finished.
    pub ended: Option<SystemTime>,
    /// The model has been handed the result.
    pub delivered: bool,
    /// How a bash job ended ("exit 3", "timed out", …).
    pub note: Option<String>,
    /// A subagent's session file, once its process has opened it.
    pub session: Option<PathBuf>,
}

/// Input from the TUI to a command running on a pseudo-terminal.
pub enum PtyInput {
    Bytes(Vec<u8>),
    Resize { cols: u16, rows: u16 },
    Kill,
}

pub enum AgentEvent {
    TurnStart,
    /// Sent right after `TurnStart` when the turn began without user input (a background job
    /// finished or a `mu notify` message arrived).
    Woken,
    /// A user message entered the conversation (initial, steer or follow-up).
    UserPrompt(String),
    Text(String),
    Thinking(String),
    BlockEnd,
    /// The model began a tool call; its arguments follow as `ToolCallDelta`s.
    ToolCallStart {
        id: String,
        name: String,
    },
    /// Raw JSON fragment of a streaming tool call's arguments.
    ToolCallDelta {
        id: String,
        json: String,
    },
    ToolStart {
        id: String,
        name: String,
        args: Value,
    },
    /// For a pty call the chunk is raw terminal text, escape sequences included.
    ToolOutput {
        id: String,
        chunk: String,
    },
    /// A bash call wants a terminal. `reply` carries `(cols, rows)`; dropping it makes the tool fall back.
    PtyOpen {
        id: String,
        input: mpsc::UnboundedSender<PtyInput>,
        reply: oneshot::Sender<(u16, u16)>,
    },
    /// Current todo phases (JSON), after a change or a resume.
    Todo(Value),
    /// The next `ToolStart` (same `id`) is a call that will not run: its placeholder result follows
    /// as `ToolEnd`. Sent for calls skipped by a stop, an abort, a new user message or truncation.
    ToolSkipped {
        id: String,
    },
    ToolEnd {
        id: String,
        name: String,
        output: ToolOutput,
    },
    Retry {
        attempt: u32,
        delay: Duration,
        reason: String,
    },
    Notice(String),
    Error(String),
    Compacting,
    Compacted {
        before: u64,
        after: u64,
    },
    /// Snapshot of every background job (running and finished).
    Jobs(Vec<SubagentStatus>),
    /// Finished jobs whose results were handed to the model.
    JobsDelivered(Vec<String>),
    Ask {
        questions: Vec<AskQuestion>,
        reply: oneshot::Sender<Option<Vec<Answer>>>,
    },
    PlanReview {
        title: String,
        uri: String,
        file: PathBuf,
        content: String,
        /// The plan comes from the store (a parked plan being opened), not from a live `propose_plan`.
        stored: bool,
        /// One dim line under the title, e.g. how far the code drifted since the plan was written.
        note: Option<String>,
        reply: oneshot::Sender<PlanDecision>,
    },
    /// A plan was saved to the store without being approved.
    PlanParked {
        id: String,
        path: PathBuf,
    },
    /// The repository's pending plans, newest first.
    PlanList(Vec<crate::plans::PlanMeta>),
    Status(StatusSnapshot),
    TurnEnd {
        aborted: bool,
    },
    /// The turn ended on a fatal error, already reported as an `Error`, and is followed by `TurnEnd`.
    /// The agent loop sends it when a run fails (a provider or compaction error), not when it is
    /// aborted or stopped. A `-p` run that sees it exits 1 even if a stop was requested, so a
    /// failure is not reported as "interrupted, resume with …".
    TurnFailed,
    Exit,
}

pub type EventTx = mpsc::UnboundedSender<AgentEvent>;
