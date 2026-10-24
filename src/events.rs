use crate::gitinfo::GitInfo;
use crate::quota::ProviderUsage;
use crate::models::Thinking;
use crate::message::Image;
use crate::tools::ToolOutput;
use serde_json::Value;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

pub enum Command {
    Prompt(UserInput),
    SetModel(String),
    SetThinking(Thinking),
    NewSession,
    Compact(Option<String>),
    TogglePlan(Option<String>),
    Resume(PathBuf),
    Bash { cmd: String, include: bool },
    Rename(String),
    /// Cancel one background job by id, or all of them.
    KillJobs(Option<String>),
    Exit,
}

/// Messages the UI queues while a turn runs, plus the turn's cancellation token.
pub struct Inbox {
    pub steer: VecDeque<UserInput>,
    pub follow_up: VecDeque<UserInput>,
    pub cancel: CancellationToken,
}

/// A user prompt: text plus attached images.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UserInput {
    pub text: String,
    pub images: Vec<Image>,
}

impl From<String> for UserInput {
    fn from(text: String) -> Self {
        Self { text, images: Vec::new() }
    }
}

impl From<&str> for UserInput {
    fn from(text: &str) -> Self {
        Self { text: text.into(), images: Vec::new() }
    }
}

pub type SharedInbox = Rc<RefCell<Inbox>>;

pub fn new_inbox() -> SharedInbox {
    Rc::new(RefCell::new(Inbox { steer: VecDeque::new(), follow_up: VecDeque::new(), cancel: CancellationToken::new() }))
}

#[derive(Clone, Debug, Default)]
pub struct StatusSnapshot {
    /// Display name, e.g. `Haiku 5.5`.
    pub model_name: String,
    /// The model supports thinking levels.
    pub reasoning: bool,
    /// `off|low|medium|high`.
    pub thinking: String,
    /// Served through the gateway, so spend is subscription-billed (shown as `S`, not `$`).
    pub subscription: bool,
    pub plan: bool,
    /// Context tokens of the current request; `None` right after compaction or before any usage is known.
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

#[derive(Clone, Debug)]
pub struct AskOption {
    pub label: String,
    pub description: Option<String>,
}

#[derive(Clone, Debug)]
pub struct AskQuestion {
    pub question: String,
    pub header: Option<String>,
    pub options: Vec<AskOption>,
    pub multi: bool,
    pub recommended: Option<usize>,
}

#[derive(Clone, Debug)]
pub enum Answer {
    Options(Vec<String>),
    Custom(String),
    Cancelled,
}

pub enum PlanDecision {
    Execute,
    Keep,
    Refine(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SubagentState {
    Running,
    Done,
    Failed,
}

#[derive(Clone, Debug)]
pub struct SubagentStatus {
    pub name: String,
    pub agent: String,
    pub tools: u32,
    pub last: String,
    pub state: SubagentState,
}

pub enum AgentEvent {
    TurnStart,
    /// A user message entered the conversation (initial, steer or follow-up).
    UserPrompt(String),
    Text(String),
    Thinking(String),
    BlockEnd,
    ToolStart { id: String, name: String, args: Value },
    ToolOutput { id: String, chunk: String },
    /// Current todo phases (JSON), after a change or a resume.
    Todo(Value),
    ToolEnd { id: String, name: String, output: ToolOutput },
    Retry { attempt: u32, delay: Duration, reason: String },
    Notice(String),
    Error(String),
    Compacting,
    Compacted { before: u64, after: u64 },
    /// Snapshot of every background job (running and finished).
    Jobs(Vec<SubagentStatus>),
    /// Finished jobs whose results were handed to the model.
    JobsDelivered(Vec<String>),
    Ask { questions: Vec<AskQuestion>, reply: oneshot::Sender<Option<Vec<Answer>>> },
    PlanReview { title: String, path: String, reply: oneshot::Sender<PlanDecision> },
    Status(StatusSnapshot),
    TurnEnd { aborted: bool },
    Exit,
}

pub type EventTx = mpsc::UnboundedSender<AgentEvent>;
