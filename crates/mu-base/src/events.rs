use crate::message::UserInput;
use crate::models::Thinking;
use crate::permissions::{ApprovalDecision, ApprovalRequest, PermissionMode};
use crate::tool::ToolOutput;
use serde_json::Value;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};
use tokio::sync::{mpsc, oneshot};

/// `45s`, `3m 20s`, `1h 12m`, `2d 3h`: the two most significant units of a wait.
pub fn wait_text(d: Duration) -> String {
    let s = d.as_secs();
    let part = |big: u64, bu: &str, small: u64, su: &str| {
        if small == 0 { format!("{big}{bu}") } else { format!("{big}{bu} {small}{su}") }
    };
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => part(s / 60, "m", s % 60, "s"),
        3600..=86399 => part(s / 3600, "h", s % 3600 / 60, "m"),
        _ => part(s / 86400, "d", s % 86400 / 3600, "h"),
    }
}

/// The retry notice every front end shows: `retrying in 4s (timeout) [attempt 2/5]`.
pub fn retry_notice(attempt: u32, max_attempts: u32, delay: Duration, reason: &str) -> String {
    format!("retrying in {} ({reason}) [attempt {attempt}/{max_attempts}]", wait_text(delay))
}

pub enum Command {
    Prompt(UserInput),
    /// Creates the session file now, so its id is known before the first prompt.
    EnsureSession,
    SetModel(String),
    /// Re-resolve the current model: a key was added or removed, so its route may have changed.
    RefreshModel,
    /// Refresh external Git and gateway quota status without a model turn.
    RefreshStatus,
    SetThinking(Thinking),
    CycleThinking,
    /// `/permissions` or `--permissions`: the mode applies to the next tool call.
    SetPermissionMode(PermissionMode),
    NewSession,
    Compact(Option<String>),
    /// `/review [focus]`.
    Review(Option<String>),
    TogglePlan(Option<String>),
    Resume(PathBuf),
    /// `/fork`: continue in a new session file holding a copy of this conversation.
    Fork,
    /// `/rewind`: list the prompts that can be taken back and the replies that can be returned to,
    /// as `RewindPoints`.
    ListRewind,
    /// Rewind to this session entry: for a prompt, the conversation returns to just before it and
    /// the prompt goes back to the composer as `QueuePopped`; for an agent reply, it returns to
    /// just after it.
    Rewind(String),
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
    /// Deliver between tool calls of the running turn; when idle, the message goes back to the composer.
    Steer(UserInput),
    /// Run after the running turn; when idle, the message goes back to the composer.
    FollowUp(UserInput),
    /// Take the newest queued message back as `QueuePopped`.
    PopQueue,
    /// Abort the in-flight turn (Esc, quit, plan review cancel, ACP cancel, SIGUSR1).
    Cancel,
    /// Finish at the next tool/turn boundary, leaving the session resumable (headless first Ctrl-C).
    Stop,
}

/// A prompt `Command::Rewind` can go back before, or an agent reply it can go back to just after.
#[derive(Clone, Debug, PartialEq)]
pub struct RewindPoint {
    /// The session entry holding the prompt or reply.
    pub id: String,
    pub text: String,
    /// The agent's closing reply of a turn, kept by the rewind; otherwise a prompt, taken back.
    pub reply: bool,
}

/// Messages queued for the agent: steered into the running turn, or held until it ends.
#[derive(Clone, Debug, Default)]
pub struct QueueState {
    pub steer: Vec<UserInput>,
    pub follow_up: Vec<UserInput>,
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
    /// Routed to the provider's own API with a key mu holds (shown as `· key`).
    pub native: bool,
    /// A native custom endpoint that takes no key: shown as `· no auth` instead of `· key`.
    pub native_keyless: bool,
    /// Runs on the user's ChatGPT plan (Sign in with ChatGPT): shown as `· ChatGPT plan` instead of
    /// `· key`, and not metered per token. Implies `native`.
    pub chatgpt_plan: bool,
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
    /// Tool permissions; the status line shows it unless it is `yolo`.
    pub permissions: PermissionMode,
    /// The provider prompt cache as of the last reply; `None` until some reply reports a cache hit.
    pub cache: Option<CacheStatus>,
}

/// The provider prompt cache as of the last reply: what it read, and what changed since the request.
#[derive(Clone, Debug, PartialEq)]
pub struct CacheStatus {
    pub input: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    /// The last reply's timestamp (ms since the epoch); the cache expires `CACHE_TTL_MS` after it.
    pub at_ms: i64,
    /// What changed since the last request that makes the next one miss the cache.
    pub changed: Option<CacheBreak>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CacheBreak {
    Model,
    Thinking,
    Tools,
    SystemPrompt,
    Compacted,
}

/// Anthropic's ephemeral breakpoints (the only kind mu sets) and OpenAI's automatic cache both keep
/// an entry ~5 min.
pub const CACHE_TTL_MS: i64 = 5 * 60_000;

/// A plan's state in the plan store.
#[derive(serde::Serialize, serde::Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Pending,
    Approved,
    Dropped,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Base {
    pub tree: String,
    pub commit: Option<String>,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Approval {
    pub at: String,
    pub decision: String,
    pub session: Option<PathBuf>,
    pub hash: String,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PlanMeta {
    pub id: String,
    pub title: String,
    pub status: Status,
    pub repo: PathBuf,
    pub base: Option<Base>,
    pub read: Vec<String>,
    pub session: Option<PathBuf>,
    pub created: String,
    pub updated: String,
    pub hash: String,
    pub approved: Option<Approval>,
}

impl Status {
    pub fn name(self) -> &'static str {
        match self {
            Status::Pending => "pending",
            Status::Approved => "approved",
            Status::Dropped => "dropped",
        }
    }
}

impl PlanMeta {
    /// `read` minus entries that are directories now; plans saved before directory reads were skipped list them (the root as "").
    pub fn files_read(&self, root: &std::path::Path) -> Vec<&String> {
        self.read.iter().filter(|r| !root.join(r).is_dir()).collect()
    }
}

/// Branch and working-tree counts of a repository (`gitinfo`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GitInfo {
    pub branch: Option<String>,
    /// Entries with a staged (index) change.
    pub staged: u32,
    /// Entries with an unstaged (worktree) change, unmerged entries included.
    pub dirty: u32,
    pub untracked: u32,
    pub ahead: u32,
    pub behind: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct QuotaWindow {
    /// `5h`, `1d` or `7d`.
    pub label: String,
    pub used_percent: f64,
    pub resets_in: Duration,
}

/// Subscription usage windows of one provider (`quota`).
#[derive(Clone, Debug, PartialEq)]
pub struct ProviderUsage {
    pub provider: String,
    pub windows: Vec<QuotaWindow>,
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
    /// The picker offers a last row that declines the question (`Answer::Skipped`).
    pub skippable: bool,
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
    /// The user declined this question; the turn goes on.
    Skipped,
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

impl PlanDecision {
    /// The name the tool result records as `planDecision` and the plan store records as the approval.
    pub fn wire_name(&self) -> &'static str {
        match self {
            Self::Execute { .. } => "execute",
            Self::Compact { .. } => "compact",
            Self::Keep { .. } => "keep",
            Self::Refine(_) => "refine",
            Self::Park => "park",
            Self::Drop => "drop",
        }
    }

    pub fn model(&self) -> Option<&str> {
        match self {
            Self::Execute { model } | Self::Compact { model } | Self::Keep { model } => model.as_deref(),
            _ => None,
        }
    }
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
    /// The model a subagent runs on, as `role · provider/model` (no role when named directly); known
    /// once its process has resolved it.
    pub model: Option<String>,
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
    /// Desired thinking level changed while a turn is running.
    ThinkingLevel(Thinking),
    /// Tool permissions changed (`/permissions`).
    PermissionMode(PermissionMode),
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
        max_attempts: u32,
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
    /// The newest output of a running `bash` job (redacted), at most a few times a second.
    JobOutput {
        id: String,
        tail: String,
    },
    Ask {
        questions: Vec<AskQuestion>,
        reply: oneshot::Sender<Option<Vec<Answer>>>,
    },
    /// A tool call needs the user's approval. Dropping `reply` denies it and ends the turn.
    Approve {
        request: ApprovalRequest,
        reply: oneshot::Sender<ApprovalDecision>,
    },
    /// `web_search` was called and no backend is set up: the front end runs the setup wizard and
    /// replies whether web search is now ready. A dropped `reply` means no.
    WebSetup {
        reply: oneshot::Sender<bool>,
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
    PlanList(Vec<PlanMeta>),
    Status(StatusSnapshot),
    TurnEnd {
        aborted: bool,
    },
    /// The turn ended on a fatal error, already reported as an `Error`, and is followed by `TurnEnd`.
    /// The agent loop sends it when a run fails (a provider or compaction error), not when it is
    /// aborted or stopped. A `-p` run that sees it exits 1 even if a stop was requested, so a
    /// failure is not reported as "interrupted, resume with …".
    TurnFailed,
    /// What is queued for the agent; sent whenever it changes.
    Queue(QueueState),
    /// A queued message taken back by `Command::PopQueue`, or the prompt `Command::Rewind` took back.
    QueuePopped(UserInput),
    /// The prompts and closing replies of the conversation that `Command::Rewind` can go back to, oldest first.
    RewindPoints(Vec<RewindPoint>),
    /// Queued input a finished turn did not take. Sent after its `TurnEnd`; the front end restores it.
    QueuedDropped(Vec<UserInput>),
    Exit,
}

pub type EventTx = mpsc::UnboundedSender<AgentEvent>;
