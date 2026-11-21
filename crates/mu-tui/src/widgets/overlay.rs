//! The pickers and dialogs drawn in place of the composer, and what a key does inside each.
//! A key never reaches into the app: it either keeps the overlay open, closes it, or hands the
//! app an `OverlayEvent` saying what the user chose.

use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent};
use tokio::sync::oneshot;

use crate::panels::jobs_panel::{JobsPanel, JobsResult};
use crate::panels::model_roles::{RoleEditor, RoleEditorAction};
use crate::panels::plan_review::{PlanReview, ReviewResult};
use crate::panels::reply::{ReplyPicker, ReplyResult};
use crate::widgets::frame::free_rows;
use crate::widgets::picker::{HasSelect, Pick, Picker};
use crate::widgets::select::{Item, Select, SelectResult};
use mu_base::events::{Answer, AskQuestion, PlanDecision};
use mu_base::model_config::RoleMeta;
use mu_base::permissions::{ApprovalDecision, ApprovalRequest, ApprovalScope};

/// One entry of the `/login` picker.
#[derive(Clone, Debug, PartialEq)]
pub enum LoginTarget {
    /// An MCP server, by its name in `mcp.json`.
    Mcp(String),
    /// A key target (a chat provider or a search service), by id (`KeyTarget::find`).
    ApiKey(String),
    /// Sign in with ChatGPT (or the sign-in menu when already signed in).
    ChatGpt,
    /// The custom-endpoint panel: a new endpoint, or the existing one with this id to edit.
    CustomProvider(Option<String>),
}

/// What the user does next in the API-key picker.
#[derive(Clone, Debug, PartialEq)]
pub enum KeyOption {
    /// Type or paste the key (masked).
    Paste,
    /// Use `$NAME`, the variable the user already has set.
    Env(String),
    /// Type a `!command` that prints the key.
    Command,
    /// Validation could not reach the provider: keep the key anyway.
    SaveAnyway,
    /// Whether the provider's chat models use this key.
    Chat(bool),
    /// Start the key flow over for a provider that already has a key.
    Replace,
    /// Sign in with ChatGPT again (the ChatGPT menu).
    SignIn,
    /// Sign out of ChatGPT and revoke the sign-in (the ChatGPT menu).
    SignOut,
    /// Forget the ChatGPT registration and sign in again, possibly as another account (the ChatGPT menu).
    Forget,
    Cancel,
}

/// A row of the `/mcp` server list.
#[derive(Clone, Debug, PartialEq)]
pub enum McpRow {
    Server(String),
    Add,
    Import,
}

/// What a custom-endpoint panel picker answered.
#[derive(Clone, Debug, PartialEq)]
pub enum PanelPick {
    /// The row chosen, or `None` when the picker was dismissed.
    Row(Option<usize>),
    /// The text typed into the panel's free-text field.
    Text(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum McpAction {
    Enable,
    Disable,
    Reconnect,
    Test,
    Login,
    Remove,
}

/// Which `/mcp` picker this is, with what its rows stand for.
#[derive(Clone, Debug, PartialEq)]
pub enum McpPick {
    Servers(Vec<McpRow>),
    Actions(String, Vec<McpAction>),
    /// Remove this server? Row 0 confirms.
    ConfirmRemove(String),
    /// A step of the `/mcp add` wizard.
    Wizard,
}

/// A row of the `/memory` picker.
#[derive(Clone, Debug, PartialEq)]
pub enum MemoryPick {
    /// A memory file (the user's, this repository's, or a note), opened in `$EDITOR`.
    File(PathBuf),
    History,
}

pub enum Overlay {
    /// The models to switch to; each row carries its id or `@role`.
    Model {
        pick: Picker<String>,
        request: u64,
    },
    ModelRoles {
        st: Box<RoleEditor>,
    },
    /// What can be logged into: the key rows, ChatGPT, custom endpoints, then MCP servers that need auth.
    Login {
        pick: Picker<LoginTarget>,
    },
    /// The API-key picker for `provider`: the options it offers.
    KeyChoice {
        pick: Picker<KeyOption>,
        provider: String,
    },
    /// `/logout`: the stored API keys that can be removed, by provider.
    Logout {
        pick: Picker<String>,
    },
    /// A `/mcp` picker: the server list, a server's actions, or a wizard step.
    Mcp {
        sel: Select,
        pick: McpPick,
    },
    /// A step of the `/web` setup wizard.
    Web {
        sel: Select,
    },
    /// A screen of the custom-endpoint panel.
    Custom {
        sel: Select,
    },
    /// `/modelset`: the model sets, to switch to one.
    ModelSet {
        pick: Picker<String>,
    },
    /// The startup offer to copy omp's model config into models.toml.
    ModelImport {
        sel: Select,
    },
    /// The startup offer to trust a project's MCP servers: each file, its digest and its server names.
    McpTrust {
        sel: Select,
        files: Vec<(PathBuf, String, Vec<String>)>,
    },
    Session {
        pick: Picker<PathBuf>,
    },
    /// The repository's parked plans, by id.
    Plans {
        pick: Picker<String>,
        /// Chosen plans are delivered, not opened.
        deliver: bool,
    },
    /// `/rewind`: the prompts and replies to go back to, by session entry.
    Rewind {
        pick: Picker<String>,
    },
    /// A question, or the review after the last one. Boxed: the largest variant.
    Ask(Box<AskState>),
    /// A tool call waiting for approval.
    Approve(Box<ApproveState>),
    Review {
        review: Box<PlanReview>,
        reply: Option<oneshot::Sender<PlanDecision>>,
        /// Opened from the store, not from a running `propose_plan`.
        stored: bool,
    },
    Copy {
        pick: Picker<String>,
    },
    /// Pick parts of the last reply to quote into the composer.
    Reply {
        st: Box<ReplyPicker>,
    },
    /// This session's background jobs.
    Jobs {
        st: Box<JobsPanel>,
    },
    /// `/memory`: the memory files and notes, then the history. Empty while the list loads.
    Memory {
        pick: Picker<MemoryPick>,
    },
    /// `/memory` history: the commits, newest first, by sha.
    MemoryLog {
        pick: Picker<String>,
    },
}

type Outcome = (Option<Overlay>, Option<OverlayEvent>);

/// Runs a key on a picker whose choice is one value: the overlay stays while it is pending, and a
/// choice closes it with the event made from the value.
fn pick_one<T: Clone>(
    mut pick: Picker<T>,
    k: KeyEvent,
    keep: impl FnOnce(Picker<T>) -> Overlay,
    event: impl FnOnce(T) -> OverlayEvent,
) -> Outcome {
    match pick.on_key(k) {
        Pick::Pending => (Some(keep(pick)), None),
        Pick::Chosen(value) => (None, value.map(event)),
        Pick::Dismissed => (None, None),
    }
}

/// The `Select` of every overlay that is a picker, as a shared or a mutable reference (`$m` is `sel`
/// or `sel_mut`). The overlays that are not a plain `Select` are answered by the caller first.
macro_rules! picker_select {
    ($ov:expr, $m:ident) => {
        match $ov {
            Overlay::Model { pick, .. } => Some(pick.$m()),
            Overlay::Login { pick } => Some(pick.$m()),
            Overlay::KeyChoice { pick, .. } => Some(pick.$m()),
            Overlay::Logout { pick } => Some(pick.$m()),
            Overlay::ModelSet { pick } => Some(pick.$m()),
            Overlay::Session { pick } => Some(pick.$m()),
            Overlay::Plans { pick, .. } => Some(pick.$m()),
            Overlay::Rewind { pick } => Some(pick.$m()),
            Overlay::Copy { pick } => Some(pick.$m()),
            Overlay::Memory { pick } => Some(pick.$m()),
            Overlay::MemoryLog { pick } => Some(pick.$m()),
            Overlay::Mcp { sel, .. }
            | Overlay::Custom { sel }
            | Overlay::Web { sel }
            | Overlay::ModelImport { sel }
            | Overlay::McpTrust { sel, .. } => Some(sel.$m()),
            Overlay::ModelRoles { .. }
            | Overlay::Ask(_)
            | Overlay::Approve(_)
            | Overlay::Review { .. }
            | Overlay::Reply { .. }
            | Overlay::Jobs { .. } => None,
        }
    };
}

/// What the user chose; the app carries it out.
pub enum OverlayEvent {
    /// Switch this session to the model.
    SetModel(String),
    LoadRoleModels,
    SaveRole {
        set: String,
        role: String,
        value: Option<String>,
        /// A custom role's `[roles.<name>]` settings; `None` for a role mu uses itself.
        meta: Option<RoleMeta>,
    },
    /// Remove a custom role from this set and its `[roles.<name>]` settings.
    DeleteRole {
        set: String,
        role: String,
    },
    Login(LoginTarget),
    /// The API-key picker chose this for `provider` (`Cancel` when it was dismissed).
    Key {
        provider: String,
        choice: KeyOption,
    },
    /// Remove the stored API key of this provider.
    Logout(String),
    /// A `/mcp` picker closed: the row chosen, or `None` when dismissed.
    Mcp(McpPick, Option<usize>),
    /// A `/web` wizard picker closed: the row chosen, or `None` when dismissed.
    Web(Option<usize>),
    /// A step of the custom-endpoint panel: the row chosen, the text typed into a field, or
    /// `None` when it was dismissed.
    Custom(PanelPick),
    /// Switch to this model set.
    ModelSet(String),
    /// The import offer: `true` to copy omp's model config.
    ModelImport(bool),
    /// The MCP trust offer: `true` to trust these files as they were shown.
    McpTrust(bool, Vec<(PathBuf, String, Vec<String>)>),
    Copy(Option<String>),
    /// Insert these blockquotes at the end of the composer.
    Quote(Vec<String>),
    Resume(PathBuf),
    /// Go back to before the prompt in this session entry.
    Rewind(String),
    /// Copy the plan text.
    CopyPlan(String),
    /// Edit the plan file; the overlay stays up and reloads it afterwards.
    EditPlan(PathBuf),
    /// The plan review was dismissed. A live plan cancels the turn; a stored one just stays parked.
    PlanCancelled {
        stored: bool,
    },
    /// A parked plan was chosen from the list.
    OpenPlan(String),
    /// A parked plan was chosen to be delivered.
    DeliverPlan(String),
    /// Cancel one running job by id, or all of them.
    KillJobs(Option<String>),
    /// Message a live session (`mu notify`).
    NotifySession {
        session: PathBuf,
        text: String,
    },
    /// Stop a live session; `pid` and `started` say which run the user saw.
    StopSession {
        session: PathBuf,
        pid: u32,
        started: String,
        kind: String,
    },
    /// Ask an interactive mu to exit (confirmed in the panel).
    TerminateSession {
        session: PathBuf,
        pid: u32,
        started: String,
    },
    /// Take over a `-p` run: the panel closes and `mu take` runs on the terminal.
    TakeSession(PathBuf),
    /// Join a session on another host: the panel closes and `mu --attach COMMAND` runs on the terminal.
    AttachSession(String),
    /// Show a launched job's diff in the panel.
    DiffResult {
        job: String,
        result: crate::host::results::ResultArgs,
    },
    /// Apply a launched job's result with a three-way merge.
    ApplyResult {
        job: String,
        result: crate::host::results::ResultArgs,
    },
    /// Open a memory file in `$EDITOR`.
    MemoryOpen(PathBuf),
    /// Load the memory history.
    MemoryHistory,
    /// Show a commit's message and diff.
    MemoryShow(String),
    /// Revert a commit.
    MemoryRevert(String),
}

/// The question as a picker: its options, a free-text row, multi-select, the recommended choice
/// preselected. Single-select answers on the digit keys (`y`/`n` for a confirm), and a note can be
/// typed on the highlighted option (Tab, or Right). Options show their
/// descriptions in place.
#[cfg(test)]
pub fn ask_select(q: &AskQuestion) -> Select {
    ask_at(q, None, None)
}

/// A question's picker: `pos` is its place among the questions (`k` of `n`) for the title, and
/// `prior` its earlier answer, preselected so a revisit shows it.
fn ask_at(q: &AskQuestion, pos: Option<(usize, usize)>, prior: Option<&Answer>) -> Select {
    let items: Vec<Item> = q
        .options
        .iter()
        .enumerate()
        .map(|(i, o)| Item {
            label: if q.recommended == Some(i) { format!("{} (Recommended)", o.label) } else { o.label.clone() },
            detail: o.description.clone().unwrap_or_default(),
        })
        .collect();
    let previews = q.options.iter().map(|o| o.preview.clone()).collect();
    let kinds = q.options.iter().map(|o| o.kind).collect();
    let title = q.header.as_ref().map_or(q.question.clone(), |h| format!("{h}: {}", q.question));
    let title = match pos {
        Some((k, n)) => format!("{k}/{n} · {title}"),
        None => title,
    };
    let back = pos.is_some_and(|(k, _)| k > 1);
    let mut s = Select::new(&title, items)
        .context(q.context.as_deref().unwrap_or(""))
        .free_text("Other (type your own)")
        .previews(previews)
        .preview_kinds(kinds)
        .notes()
        .unfiltered()
        .described()
        .back_keys(back)
        .hint(&ask_hint(q, pos));
    if q.skippable {
        s = s.skip_row("Skip");
    }
    if q.multi {
        s = s.multi();
    } else if q.confirm {
        s = s.quick_keys(vec![('y', 0), ('n', 1)]);
    } else {
        let n = q.options.len().min(9);
        s = s.quick_keys((0..n).map(|i| (char::from(b'1' + i as u8), i)).collect());
    }
    match prior {
        Some(prior) => prefill(s, q, prior),
        None => match q.recommended {
            Some(r) => s.preselect(r),
            None => s,
        },
    }
}

/// Preselects an earlier answer: its options, its note, or its free text.
fn prefill(s: Select, q: &AskQuestion, prior: &Answer) -> Select {
    let indices = |labels: &[String]| -> Vec<usize> {
        labels.iter().filter_map(|l| q.options.iter().position(|o| o.label == *l)).collect()
    };
    match prior {
        Answer::Options(labels) => s.preselect_all(&indices(labels)),
        Answer::Noted { picks, note } => s.preselect_all(&indices(picks)).note_text(note),
        Answer::Custom(text) => s.prefill_text(text),
        Answer::Skipped => s.prefill_skip(),
        Answer::Cancelled => s,
    }
}

/// The footer: the keys of the question's kind, then how to go back (or cancel, on the first).
fn ask_hint(q: &AskQuestion, pos: Option<(usize, usize)>) -> String {
    let mut hint = if q.multi {
        "↑↓ move · Space mark · Tab/→ note · Enter submit".to_string()
    } else if q.confirm {
        "y yes · n no · Enter choose · Tab/→ note".to_string()
    } else {
        format!("↑↓ move · Enter choose · 1–{} pick · Tab/→ note", q.options.len().min(9))
    };
    if pos.is_some_and(|(k, _)| k > 1) {
        hint.push_str(" · ← / ⇧Tab back");
    }
    hint.push_str(" · Esc cancel");
    if q.context.as_ref().is_some_and(|c| !c.trim().is_empty()) {
        hint = format!("PgUp/PgDn context · {hint}");
    }
    hint
}

/// The review screen: each question with its answer, then a Submit row, which starts highlighted.
fn review_select(qs: &[AskQuestion], answers: &[Option<Answer>]) -> Select {
    let mut items: Vec<Item> = qs
        .iter()
        .zip(answers)
        .enumerate()
        .map(|(i, (q, a))| {
            let name = q.header.as_deref().filter(|h| !h.is_empty()).map_or(q.question.as_str(), |h| h);
            Item {
                label: format!("{}. {}", i + 1, name.replace('\n', " ")),
                detail: a.as_ref().map_or(String::new(), answer_text),
            }
        })
        .collect();
    items.push(Item { label: "Submit answers".into(), detail: String::new() });
    let submit = qs.len();
    Select::new("Review your answers", items)
        .preselect(submit)
        .back_keys(true)
        .hint("↑↓ move · Enter reopen or submit · ← / ⇧Tab back · Esc cancel")
}

/// An answer on one line, as the review lists it.
fn answer_text(a: &Answer) -> String {
    let text = match a {
        Answer::Options(picks) => picks.join(", "),
        Answer::Noted { picks, note } => format!("{} — note: {note}", picks.join(", ")),
        Answer::Custom(t) => format!("\"{t}\""),
        Answer::Skipped => "(skipped)".to_string(),
        Answer::Cancelled => "(cancelled)".to_string(),
    };
    text.replace('\n', " ")
}

/// An ask in progress: its questions, the answer to each so far, and the picker on screen.
pub struct AskState {
    qs: Vec<AskQuestion>,
    /// The question being answered, or reopened from the review.
    idx: usize,
    /// Each question's answer. Going back keeps them, so a revised answer replaces the earlier one.
    answers: Vec<Option<Answer>>,
    /// The picker for question `idx`, or the review list while `review` is set.
    sel: Select,
    review: bool,
    /// Reopened from the review: answering the question returns to the review.
    revisit: bool,
    reply: Option<oneshot::Sender<Option<Vec<Answer>>>>,
}

impl AskState {
    fn new(qs: Vec<AskQuestion>, reply: oneshot::Sender<Option<Vec<Answer>>>) -> Self {
        let answers = vec![None; qs.len()];
        let sel = picker(&qs, 0, &answers);
        AskState { qs, idx: 0, answers, sel, review: false, revisit: false, reply: Some(reply) }
    }

    /// Shows question `idx` with its earlier answer.
    fn goto(&mut self, idx: usize) {
        self.idx = idx;
        self.review = false;
        self.revisit = false;
        self.sel = picker(&self.qs, idx, &self.answers);
    }

    fn show_review(&mut self) {
        self.review = true;
        self.revisit = false;
        self.sel = review_select(&self.qs, &self.answers);
    }

    fn keep(self) -> (Option<Overlay>, Option<OverlayEvent>) {
        (Some(Overlay::Ask(Box::new(self))), None)
    }

    /// Replies with every answer; a question never answered reads as cancelled.
    fn done(mut self) -> (Option<Overlay>, Option<OverlayEvent>) {
        let all: Vec<Answer> = self.answers.drain(..).map(|a| a.unwrap_or(Answer::Cancelled)).collect();
        self.reply_with(all)
    }

    /// Cancels from the current question on: the answers before it stand; from the review, all cancel.
    fn cancel(mut self) -> (Option<Overlay>, Option<OverlayEvent>) {
        let cut = if self.review { 0 } else { self.idx };
        let all: Vec<Answer> = self
            .answers
            .drain(..)
            .enumerate()
            .map(|(i, a)| match a {
                Some(a) if i < cut => a,
                _ => Answer::Cancelled,
            })
            .collect();
        self.reply_with(all)
    }

    fn reply_with(&mut self, all: Vec<Answer>) -> (Option<Overlay>, Option<OverlayEvent>) {
        if let Some(r) = self.reply.take() {
            let _ = r.send(Some(all));
        }
        (None, None)
    }

    fn key(mut self, k: KeyEvent) -> (Option<Overlay>, Option<OverlayEvent>) {
        let n = self.qs.len();
        let res = self.sel.on_key(k);
        if matches!(res, SelectResult::Pending) {
            return self.keep();
        }
        if self.review {
            match res {
                SelectResult::Chosen(i) if i.first() == Some(&n) => return self.done(),
                SelectResult::Chosen(i) => {
                    if let Some(&j) = i.first() {
                        self.goto(j);
                        self.revisit = true;
                    }
                }
                SelectResult::Back => self.goto(n - 1),
                SelectResult::Cancelled => return self.cancel(),
                _ => {}
            }
            return self.keep();
        }
        let ans = match res {
            SelectResult::Chosen(i) => Answer::Options(picked_labels(&self.qs[self.idx], &i)),
            SelectResult::Noted(i, note) => Answer::Noted { picks: picked_labels(&self.qs[self.idx], &i), note },
            SelectResult::Text(t) => Answer::Custom(t),
            SelectResult::Skipped => Answer::Skipped,
            SelectResult::Back => {
                if self.idx > 0 {
                    self.goto(self.idx - 1);
                }
                return self.keep();
            }
            SelectResult::Cancelled => return self.cancel(),
            SelectResult::Pending => return self.keep(),
        };
        self.answers[self.idx] = Some(ans);
        if n == 1 {
            return self.done();
        }
        if self.revisit || self.idx + 1 == n {
            self.show_review();
        } else {
            self.goto(self.idx + 1);
        }
        self.keep()
    }
}

/// A tool call waiting for approval: first how much to allow, then where to remember it.
pub struct ApproveState {
    req: ApprovalRequest,
    /// The question on screen; `None` only while it is being answered.
    step: Option<Step>,
    /// A note typed at the width step, carried into the scope step.
    note: Option<String>,
    reply: Option<oneshot::Sender<ApprovalDecision>>,
}

/// Which question the dialog is on.
enum Step {
    /// How much: this once, one of the grant's widths, or a pattern typed by hand.
    Width(Select),
    /// The pattern being typed.
    Pattern(Select),
    /// Where to remember the patterns picked above.
    Scope { patterns: Vec<String>, sel: Select },
}

impl ApproveState {
    pub fn new(req: ApprovalRequest, reply: oneshot::Sender<ApprovalDecision>) -> Self {
        let sel = Self::width_picker(&req);
        Self { req, step: Some(Step::Width(sel)), note: None, reply: Some(reply) }
    }

    /// The widths on offer: the exact grant, then each named one above it.
    fn widths(req: &ApprovalRequest) -> Vec<(String, Vec<String>)> {
        let Some(g) = &req.grant else { return Vec::new() };
        let mut out = Vec::new();
        if g.patterns == ["*"] {
            out.push((format!("every call of {}", g.tool), g.patterns.clone()));
        } else if !g.patterns.is_empty() {
            out.push(("what it asked for".to_string(), g.patterns.clone()));
        }
        out.extend(g.wider.iter().map(|w| (w.label.clone(), w.patterns.clone())));
        out
    }

    /// What the request is about: where it came from, why it asks, and what it would do.
    fn context(req: &ApprovalRequest) -> String {
        let mut context = String::new();
        if let Some(from) = &req.from {
            context.push_str(&format!("from {from}\n"));
        }
        context.push_str(&req.reason);
        context.push_str("\n\n");
        context.push_str(&truncated(&req.body));
        context
    }

    /// First step: how much to allow. The widths are landmarks, not every directory, so the list
    /// stays short however deep the path is.
    fn width_picker(req: &ApprovalRequest) -> Select {
        let widths = Self::widths(req);
        let mut items = vec![Item { label: "Yes, just this once".into(), detail: String::new() }];
        for (label, patterns) in &widths {
            items.push(Item { label: format!("For {label}"), detail: patterns.join(", ") });
        }
        items.push(Item { label: "No".into(), detail: String::new() });
        let hint = if widths.is_empty() {
            "y yes · n no · Enter choose · Tab/→ note · Esc cancel".to_string()
        } else {
            format!("y yes · n no · 1–{} pick · e type a pattern · Enter choose · Tab/→ note · Esc cancel", items.len())
        };
        Select::new(&req.title, items).context(&Self::context(req)).notes().described().hint(&hint)
    }

    /// The typed pattern, prefilled with the directory width so it only has to be trimmed.
    fn pattern_picker(&self) -> Select {
        let widths = Self::widths(&self.req);
        let prefill = widths.get(1).or_else(|| widths.first()).map(|(_, p)| p.join(", ")).unwrap_or_default();
        Select::new("Pattern to remember", Vec::new())
            .free_text("pattern")
            .edit_text(&prefill)
            .hint("Enter saves what you type · Esc goes back")
    }

    /// Second step: where to remember the patterns.
    fn scope_picker(&self, patterns: &[String]) -> Select {
        let items = ["This session", "In this project", "Everywhere", "No"]
            .into_iter()
            .map(|label| Item { label: label.into(), detail: String::new() })
            .collect();
        Select::new(&format!("Remember {} for", patterns.join(", ")), items)
            .context("Esc goes back to the widths.")
            .notes()
            .note_text(self.note.as_deref().unwrap_or(""))
            .hint("1–3 pick · Enter choose · Tab/→ note · Esc back")
    }

    fn keep(self) -> (Option<Overlay>, Option<OverlayEvent>) {
        (Some(Overlay::Approve(Box::new(self))), None)
    }

    /// The picker on screen, whichever step it is.
    fn select(&self) -> &Select {
        match self.step.as_ref().expect("an approval dialog always has a step") {
            Step::Width(sel) | Step::Pattern(sel) => sel,
            Step::Scope { sel, .. } => sel,
        }
    }

    fn select_mut(&mut self) -> &mut Select {
        match self.step.as_mut().expect("an approval dialog always has a step") {
            Step::Width(sel) | Step::Pattern(sel) => sel,
            Step::Scope { sel, .. } => sel,
        }
    }

    fn answer(mut self, decision: ApprovalDecision) -> (Option<Overlay>, Option<OverlayEvent>) {
        if let Some(r) = self.reply.take() {
            let _ = r.send(decision);
        }
        (None, None)
    }

    fn key(mut self, k: KeyEvent) -> (Option<Overlay>, Option<OverlayEvent>) {
        let step = self.step.take().expect("an approval dialog always has a step");
        match step {
            Step::Width(sel) => self.width_key(k, sel),
            Step::Pattern(sel) => self.pattern_key(k, sel),
            Step::Scope { patterns, sel } => self.scope_key(k, patterns, sel),
        }
    }

    fn width_key(mut self, k: KeyEvent, mut sel: Select) -> (Option<Overlay>, Option<OverlayEvent>) {
        let widths = Self::widths(&self.req);
        // Shortcuts move the highlight rather than answer, so a note can still be typed.
        if k.modifiers.is_empty()
            && let KeyCode::Char(c) = k.code
            && !sel.is_typing()
        {
            if c == 'e' && !widths.is_empty() {
                let sel = self.pattern_picker();
                self.step = Some(Step::Pattern(sel));
                return self.keep();
            }
            let rows = widths.len() + 2;
            let row = match c {
                'y' => Some(0),
                'n' => Some(rows - 1),
                '1'..='9' => c.to_digit(10).map(|d| d as usize - 1).filter(|i| *i < rows),
                _ => None,
            };
            if let Some(i) = row {
                sel.select_row(i);
                self.step = Some(Step::Width(sel));
                return self.keep();
            }
        }
        match sel.on_key(k) {
            SelectResult::Pending => {
                self.step = Some(Step::Width(sel));
                self.keep()
            }
            SelectResult::Chosen(i) => self.width_chosen(widths, i.first().copied(), None),
            SelectResult::Noted(i, note) => self.width_chosen(widths, i.first().copied(), Some(note)),
            SelectResult::Cancelled => self.answer(ApprovalDecision::Deny { note: None }),
            _ => {
                self.step = Some(Step::Width(sel));
                self.keep()
            }
        }
    }

    /// What the width step answered: once, a width (then ask where), or no.
    fn width_chosen(
        mut self,
        widths: Vec<(String, Vec<String>)>,
        row: Option<usize>,
        note: Option<String>,
    ) -> (Option<Overlay>, Option<OverlayEvent>) {
        match row {
            Some(0) => self.answer(ApprovalDecision::Allow { scope: ApprovalScope::Once, note, patterns: None }),
            Some(i) if i == widths.len() + 1 => self.answer(ApprovalDecision::Deny { note }),
            Some(i) => {
                let patterns = widths[i - 1].1.clone();
                self.note = note;
                let sel = self.scope_picker(&patterns);
                self.step = Some(Step::Scope { patterns, sel });
                self.keep()
            }
            None => self.keep(),
        }
    }

    fn pattern_key(mut self, k: KeyEvent, sel: Select) -> (Option<Overlay>, Option<OverlayEvent>) {
        let mut sel = sel;
        match sel.on_key(k) {
            SelectResult::Text(text) => {
                let patterns: Vec<String> =
                    text.split(',').map(str::trim).filter(|p| !p.is_empty()).map(String::from).collect();
                if patterns.is_empty() {
                    self.step = Some(Step::Pattern(sel));
                    return self.keep();
                }
                let picker = self.scope_picker(&patterns);
                self.step = Some(Step::Scope { patterns, sel: picker });
                self.keep()
            }
            SelectResult::Cancelled => {
                let sel = Self::width_picker(&self.req);
                self.step = Some(Step::Width(sel));
                self.keep()
            }
            _ => {
                self.step = Some(Step::Pattern(sel));
                self.keep()
            }
        }
    }

    fn scope_key(
        mut self,
        k: KeyEvent,
        patterns: Vec<String>,
        mut sel: Select,
    ) -> (Option<Overlay>, Option<OverlayEvent>) {
        if k.modifiers.is_empty()
            && let KeyCode::Char(c) = k.code
            && !sel.is_typing()
        {
            let row = match c {
                'y' => Some(0),
                'n' => Some(3),
                '1'..='3' => c.to_digit(10).map(|d| d as usize - 1),
                _ => None,
            };
            if let Some(i) = row {
                sel.select_row(i);
                self.step = Some(Step::Scope { patterns, sel });
                return self.keep();
            }
        }
        match sel.on_key(k) {
            SelectResult::Pending => {
                self.step = Some(Step::Scope { patterns, sel });
                self.keep()
            }
            SelectResult::Chosen(i) => self.scope_chosen(patterns, i.first().copied(), None),
            SelectResult::Noted(i, note) => self.scope_chosen(patterns, i.first().copied(), Some(note)),
            SelectResult::Cancelled => {
                let sel = Self::width_picker(&self.req);
                self.step = Some(Step::Width(sel));
                self.keep()
            }
            _ => {
                self.step = Some(Step::Scope { patterns, sel });
                self.keep()
            }
        }
    }

    fn scope_chosen(
        self,
        patterns: Vec<String>,
        row: Option<usize>,
        note: Option<String>,
    ) -> (Option<Overlay>, Option<OverlayEvent>) {
        let scope = match row {
            Some(0) => ApprovalScope::Session,
            Some(1) => ApprovalScope::Project,
            Some(2) => ApprovalScope::Global,
            _ => return self.answer(ApprovalDecision::Deny { note }),
        };
        self.answer(ApprovalDecision::Allow { scope, note, patterns: Some(patterns) })
    }

    /// The turn it belonged to ended before it was answered.
    pub fn reply_closed(&self) -> bool {
        self.reply.as_ref().is_some_and(|r| r.is_closed())
    }

    /// The request and its unanswered reply, so the dialog can be reopened later.
    pub fn into_pending(mut self) -> Option<(ApprovalRequest, oneshot::Sender<ApprovalDecision>)> {
        self.reply.take().map(|r| (self.req, r))
    }
}

/// The body as at most 40 lines.
fn truncated(body: &str) -> String {
    const MAX: usize = 40;
    let lines: Vec<&str> = body.lines().collect();
    if lines.len() <= MAX {
        body.to_string()
    } else {
        format!("{}\n… ({} more lines)", lines[..MAX].join("\n"), lines.len() - MAX)
    }
}

/// The picker on screen for question `idx`.
fn picker(qs: &[AskQuestion], idx: usize, answers: &[Option<Answer>]) -> Select {
    let pos = (qs.len() > 1).then_some((idx + 1, qs.len()));
    ask_at(&qs[idx], pos, answers[idx].as_ref())
}

/// The labels of the picked options of an ask question.
fn picked_labels(q: &AskQuestion, picks: &[usize]) -> Vec<String> {
    picks.iter().filter_map(|i| q.options.get(*i)).map(|o| o.label.clone()).collect()
}

impl Overlay {
    /// An ask: its questions, answered one by one, then the review (after two or more).
    pub fn new_ask(qs: Vec<AskQuestion>, reply: oneshot::Sender<Option<Vec<Answer>>>) -> Overlay {
        Overlay::Ask(Box::new(AskState::new(qs, reply)))
    }
    /// A dialog whose request is gone: the agent answered it elsewhere or no longer has it.
    pub fn request_gone(&self) -> bool {
        match self {
            Overlay::Approve(a) => a.reply_closed(),
            Overlay::Ask(a) => a.reply.as_ref().is_some_and(|r| r.is_closed()),
            Overlay::Review { reply, .. } => reply.as_ref().is_some_and(|r| r.is_closed()),
            _ => false,
        }
    }

    /// The picker inside this overlay, for everything but the plan review.
    pub fn select(&self) -> Option<&Select> {
        match self {
            Overlay::ModelRoles { st } => Some(st.select()),
            Overlay::Ask(a) => Some(&a.sel),
            Overlay::Approve(a) => Some(a.select()),
            other => picker_select!(other, sel),
        }
    }

    pub fn select_mut(&mut self) -> Option<&mut Select> {
        match self {
            Overlay::ModelRoles { st } => Some(st.select_mut()),
            Overlay::Ask(a) => Some(&mut a.sel),
            Overlay::Approve(a) => Some(a.select_mut()),
            other => picker_select!(other, sel_mut),
        }
    }

    /// The overlay's lines (a blank row first) and the cursor within them. `used` is how many
    /// rows the transcript and status already take; the plan review fits itself to what is left.
    pub fn render(&mut self, w: usize, rows: usize, used: usize) -> (Vec<crate::text::line::Line>, (usize, usize)) {
        let (body, cur) = match self {
            Overlay::Review { review, .. } => review.render(w, free_rows(rows, used)),
            Overlay::Reply { st } => st.render(w, free_rows(rows, used)),
            Overlay::Jobs { st } => st.render(w, free_rows(rows, used), std::time::SystemTime::now()),
            Overlay::Ask(a) => {
                let avail = free_rows(rows, used);
                if w < 4 || avail < 6 || a.review {
                    a.sel.render(w, avail)
                } else {
                    use crate::text::line::{dim, span, styled};
                    let (body, cur, hint) = a.sel.render_with_hint(w - 2, avail - 2);
                    let legend = if a.qs[a.idx].context.as_ref().is_some_and(|c| !c.trim().is_empty()) && w >= 22 {
                        " PgUp/PgDn context "
                    } else {
                        ""
                    };
                    let mut framed = vec![styled(format!("╭{legend}{}╮", "─".repeat(w - 2 - legend.len())), |_| dim())];
                    for mut line in body {
                        let padding = (w - 2).saturating_sub(crate::text::line::line_width(&line));
                        line.insert(0, span("│", dim()));
                        line.push(span(format!("{}│", " ".repeat(padding)), dim()));
                        framed.push(line);
                    }
                    framed.push(styled(format!("╰{}╯", "─".repeat(w - 2)), |_| dim()));
                    if let Some(hint) = hint {
                        framed.push(hint);
                    }
                    (framed, (cur.0 + 1, cur.1 + 1))
                }
            }
            other => match other.select() {
                Some(sel) => sel.render(w, ((rows / 2).max(6).saturating_sub(2)).min(free_rows(rows, used)).max(3)),
                None => (Vec::new(), (0, 0)),
            },
        };
        let mut framed: Vec<crate::text::line::Line> = vec![Vec::new()];
        framed.extend(body);
        (framed, (cur.0 + 1, cur.1))
    }

    pub fn on_paste(&mut self, s: &str) {
        match self {
            Overlay::Review { review, .. } => review.on_paste(s),
            Overlay::Jobs { st } => st.on_paste(s),
            other => {
                if let Some(sel) = other.select_mut() {
                    sel.on_paste(s);
                }
            }
        }
    }

    /// Handles one key. Returns the overlay if it stays open, and what the user chose, if anything.
    pub fn on_key(self, k: KeyEvent) -> (Option<Overlay>, Option<OverlayEvent>) {
        match self {
            Overlay::Model { pick, request } => {
                pick_one(pick, k, |pick| Overlay::Model { pick, request }, OverlayEvent::SetModel)
            }
            Overlay::ModelRoles { mut st } => match st.on_key(k) {
                RoleEditorAction::Pending => (Some(Overlay::ModelRoles { st }), None),
                RoleEditorAction::Close => (None, None),
                RoleEditorAction::RequestModels => {
                    (Some(Overlay::ModelRoles { st }), Some(OverlayEvent::LoadRoleModels))
                }
                RoleEditorAction::Save { set, role, value, meta } => {
                    (Some(Overlay::ModelRoles { st }), Some(OverlayEvent::SaveRole { set, role, value, meta }))
                }
                RoleEditorAction::Delete { set, role } => {
                    (Some(Overlay::ModelRoles { st }), Some(OverlayEvent::DeleteRole { set, role }))
                }
            },
            Overlay::Login { pick } => pick_one(pick, k, |pick| Overlay::Login { pick }, OverlayEvent::Login),
            Overlay::KeyChoice { mut pick, provider } => match pick.on_key(k) {
                Pick::Pending => (Some(Overlay::KeyChoice { pick, provider }), None),
                Pick::Chosen(choice) => {
                    (None, Some(OverlayEvent::Key { provider, choice: choice.unwrap_or(KeyOption::Cancel) }))
                }
                Pick::Dismissed => (None, Some(OverlayEvent::Key { provider, choice: KeyOption::Cancel })),
            },
            Overlay::Logout { pick } => pick_one(pick, k, |pick| Overlay::Logout { pick }, OverlayEvent::Logout),
            Overlay::Mcp { mut sel, pick } => match sel.on_key(k) {
                SelectResult::Pending => (Some(Overlay::Mcp { sel, pick }), None),
                SelectResult::Chosen(i) => (None, Some(OverlayEvent::Mcp(pick, i.first().copied()))),
                _ => (None, Some(OverlayEvent::Mcp(pick, None))),
            },
            Overlay::Web { mut sel } => match sel.on_key(k) {
                SelectResult::Pending => (Some(Overlay::Web { sel }), None),
                SelectResult::Chosen(i) => (None, Some(OverlayEvent::Web(i.first().copied()))),
                _ => (None, Some(OverlayEvent::Web(None))),
            },
            Overlay::Custom { mut sel } => match sel.on_key(k) {
                SelectResult::Pending => (Some(Overlay::Custom { sel }), None),
                SelectResult::Chosen(i) => (None, Some(OverlayEvent::Custom(PanelPick::Row(i.first().copied())))),
                SelectResult::Text(t) => (None, Some(OverlayEvent::Custom(PanelPick::Text(t)))),
                _ => (None, Some(OverlayEvent::Custom(PanelPick::Row(None)))),
            },
            Overlay::ModelSet { pick } => pick_one(pick, k, |pick| Overlay::ModelSet { pick }, OverlayEvent::ModelSet),
            Overlay::ModelImport { mut sel } => match sel.on_key(k) {
                SelectResult::Pending => (Some(Overlay::ModelImport { sel }), None),
                SelectResult::Chosen(i) => (None, Some(OverlayEvent::ModelImport(i.first() == Some(&0)))),
                _ => (None, Some(OverlayEvent::ModelImport(false))),
            },
            Overlay::McpTrust { mut sel, files } => match sel.on_key(k) {
                SelectResult::Pending => (Some(Overlay::McpTrust { sel, files }), None),
                SelectResult::Chosen(i) => (None, Some(OverlayEvent::McpTrust(i.first() == Some(&0), files))),
                _ => (None, Some(OverlayEvent::McpTrust(false, files))),
            },
            Overlay::Copy { mut pick } => match pick.on_key(k) {
                Pick::Pending => (Some(Overlay::Copy { pick }), None),
                Pick::Chosen(text) => (None, Some(OverlayEvent::Copy(text))),
                Pick::Dismissed => (None, None),
            },
            Overlay::Reply { mut st } => match st.on_key(k) {
                ReplyResult::Pending => (Some(Overlay::Reply { st }), None),
                ReplyResult::Close => (None, None),
                ReplyResult::Quote(q) => (None, Some(OverlayEvent::Quote(q))),
            },
            Overlay::Jobs { mut st } => match st.on_key(k) {
                JobsResult::Pending => (Some(Overlay::Jobs { st }), None),
                JobsResult::Close => (None, None),
                JobsResult::Kill(id) => (Some(Overlay::Jobs { st }), Some(OverlayEvent::KillJobs(id))),
                JobsResult::Notify { session, text } => {
                    (Some(Overlay::Jobs { st }), Some(OverlayEvent::NotifySession { session, text }))
                }
                JobsResult::Stop { session, pid, started, kind } => {
                    (Some(Overlay::Jobs { st }), Some(OverlayEvent::StopSession { session, pid, started, kind }))
                }
                JobsResult::Terminate { session, pid, started } => {
                    (Some(Overlay::Jobs { st }), Some(OverlayEvent::TerminateSession { session, pid, started }))
                }
                JobsResult::Take { session } => (None, Some(OverlayEvent::TakeSession(session))),
                JobsResult::Attach { command } => (None, Some(OverlayEvent::AttachSession(command))),
                JobsResult::Diff { job, result } => {
                    (Some(Overlay::Jobs { st }), Some(OverlayEvent::DiffResult { job, result }))
                }
                JobsResult::Apply { job, result } => {
                    (Some(Overlay::Jobs { st }), Some(OverlayEvent::ApplyResult { job, result }))
                }
            },
            Overlay::Session { pick } => pick_one(pick, k, |pick| Overlay::Session { pick }, OverlayEvent::Resume),
            Overlay::Rewind { pick } => pick_one(pick, k, |pick| Overlay::Rewind { pick }, OverlayEvent::Rewind),
            Overlay::Plans { pick, deliver } => pick_one(
                pick,
                k,
                |pick| Overlay::Plans { pick, deliver },
                |id| if deliver { OverlayEvent::DeliverPlan(id) } else { OverlayEvent::OpenPlan(id) },
            ),
            Overlay::Memory { pick } => pick_one(
                pick,
                k,
                |pick| Overlay::Memory { pick },
                |p| match p {
                    MemoryPick::File(path) => OverlayEvent::MemoryOpen(path),
                    MemoryPick::History => OverlayEvent::MemoryHistory,
                },
            ),
            Overlay::MemoryLog { mut pick } => {
                // `r` is the revert key unless the filter holds text, which then takes it.
                if k.code == KeyCode::Char('r') && k.modifiers.is_empty() && !pick.sel.is_filtering() {
                    let ev = pick.highlighted().cloned().map(OverlayEvent::MemoryRevert);
                    return (Some(Overlay::MemoryLog { pick }), ev);
                }
                match pick.on_key(k) {
                    Pick::Pending => (Some(Overlay::MemoryLog { pick }), None),
                    Pick::Chosen(sha) => (Some(Overlay::MemoryLog { pick }), sha.map(OverlayEvent::MemoryShow)),
                    Pick::Dismissed => (None, None),
                }
            }
            Overlay::Review { mut review, mut reply, stored } => match review.on_key(k) {
                ReviewResult::Pending => (Some(Overlay::Review { review, reply, stored }), None),
                ReviewResult::Copy => {
                    let text = review.content().to_string();
                    (Some(Overlay::Review { review, reply, stored }), Some(OverlayEvent::CopyPlan(text)))
                }
                ReviewResult::Edit => {
                    let file = review.file.clone();
                    (Some(Overlay::Review { review, reply, stored }), Some(OverlayEvent::EditPlan(file)))
                }
                ReviewResult::Cancelled => (None, Some(OverlayEvent::PlanCancelled { stored })),
                ReviewResult::Decided(d) => {
                    if let Some(r) = reply.take() {
                        let _ = r.send(d);
                    }
                    (None, None)
                }
            },
            Overlay::Ask(a) => a.key(k),
            Overlay::Approve(a) => a.key(k),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyCode;
    use mu_base::events::AskOption;
    use mu_testutil::key;

    fn items(labels: &[&str]) -> Vec<Item> {
        labels.iter().map(|l| Item { label: l.to_string(), detail: String::new() }).collect()
    }

    fn rows<T: Clone>(title: &str, rows: &[(&str, T)]) -> Picker<T> {
        let rows = rows.iter().map(|(l, v)| (items(&[*l]).remove(0), v.clone())).collect();
        Picker::from_rows(rows, |items| Select::new(title, items))
    }

    #[test]
    fn model_picker_switches_only_and_model_roles_expose_the_editor_select() {
        let ov = Overlay::Model { pick: rows("m", &[("a/x", "a/x".into()), ("@smol", "@smol".into())]), request: 9 };
        let (rest, event) = ov.on_key(key(KeyCode::Enter));
        assert!(rest.is_none());
        assert!(matches!(event, Some(OverlayEvent::SetModel(id)) if id == "a/x"));

        let role = mu_base::roles::RoleView {
            name: "task".into(),
            purpose: "subagents".into(),
            used_by: "Agents".into(),
            value: None,
            fallback: None,
            unset: "uses session".into(),
            agents: vec![],
            description: None,
            fallbacks: Vec::new(),
            delegate: false,
            custom: false,
            other_sets: Vec::new(),
        };
        let st =
            crate::panels::model_roles::RoleEditor::new("main".into(), "models.toml".into(), vec![role], None).unwrap();
        let mut ov = Overlay::ModelRoles { st: Box::new(st) };
        assert!(ov.select().is_some());
        assert!(ov.select_mut().is_some());
        assert!(ov.on_key(key(KeyCode::Enter)).1.is_none(), "enter opens edit, not assignment");
    }

    #[test]
    fn a_grant_with_nothing_to_scope_it_is_named_by_its_tool() {
        let req = |patterns: &[&str]| ApprovalRequest {
            tool: "mcp__x__y".into(),
            call_id: "c".into(),
            title: "mcp__x__y".into(),
            reason: "ask mode: mcp__x__y needs approval".into(),
            body: String::new(),
            from: None,
            grant: Some(mu_base::permissions::Grant {
                tool: "mcp__x__y".into(),
                patterns: patterns.iter().map(|p| p.to_string()).collect(),
                wider: Vec::new(),
            }),
        };
        let widths = ApproveState::widths(&req(&["*"]));
        assert_eq!(widths, [("every call of mcp__x__y".to_string(), vec!["*".to_string()])]);
        let widths = ApproveState::widths(&req(&["touch *"]));
        assert_eq!(widths[0].0, "what it asked for");
        assert!(ApproveState::widths(&req(&[])).is_empty(), "nothing to remember, no row");
    }

    #[test]
    fn typing_keeps_the_overlay_open_and_select_helpers_find_the_picker() {
        let ov = Overlay::Copy { pick: rows("c", &[("one", "1".to_string())]) };
        let (rest, ev) = ov.on_key(key(KeyCode::Char('o')));
        assert!(ev.is_none());
        let mut rest = rest.expect("still open");
        assert!(rest.select().is_some() && rest.select_mut().is_some());
        rest.on_paste("ne");
        assert!(matches!(rest.on_key(key(KeyCode::Esc)), (None, None)));
    }

    #[test]
    fn memory_picker_opens_a_file_or_loads_the_history() {
        let memory = || rows("m", &[("User", MemoryPick::File("/m/user.md".into())), ("History", MemoryPick::History)]);
        let ov = Overlay::Memory { pick: memory() };
        let (rest, ev) = ov.on_key(key(KeyCode::Enter));
        assert!(rest.is_none());
        assert!(matches!(ev, Some(OverlayEvent::MemoryOpen(p)) if p.to_str() == Some("/m/user.md")));
        let ov = Overlay::Memory { pick: memory() };
        let (rest, ev) = ov.on_key(key(KeyCode::Down));
        assert!(ev.is_none());
        let (rest, ev) = rest.expect("still open").on_key(key(KeyCode::Enter));
        assert!(rest.is_none() && matches!(ev, Some(OverlayEvent::MemoryHistory)));
    }

    #[test]
    fn memory_picker_closes_while_it_loads_on_enter() {
        let ov = Overlay::Memory { pick: Picker::placeholder(items(&["Loading…"]), |items| Select::new("m", items)) };
        assert!(matches!(ov.on_key(key(KeyCode::Enter)), (None, None)));
    }

    #[test]
    fn memory_history_shows_a_commit_and_reverts_the_highlighted_one_on_r() {
        let ov = Overlay::MemoryLog { pick: rows("h", &[("one", "aaa".to_string()), ("two", "bbb".to_string())]) };
        let (rest, ev) = ov.on_key(key(KeyCode::Enter));
        assert!(matches!(ev, Some(OverlayEvent::MemoryShow(s)) if s == "aaa"));
        let (rest, ev) = rest.expect("the history stays open after a diff").on_key(key(KeyCode::Down));
        assert!(ev.is_none());
        let (rest, ev) = rest.expect("still open").on_key(key(KeyCode::Char('r')));
        assert!(matches!(ev, Some(OverlayEvent::MemoryRevert(s)) if s == "bbb"));
        let (rest, ev) = rest.expect("the history stays open after a revert").on_key(key(KeyCode::Char('o')));
        assert!(ev.is_none(), "typing filters the list");
        let (rest, ev) = rest.expect("still open").on_key(key(KeyCode::Char('r')));
        assert!(ev.is_none(), "with the filter holding text, r is typed into it");
        assert!(matches!(rest.expect("still open").on_key(key(KeyCode::Esc)), (None, None)));
    }

    #[test]
    fn ask_walks_its_questions_and_replies_once() {
        let q = |text: &str| AskQuestion {
            question: text.into(),
            context: None,
            header: None,
            options: vec![],
            multi: false,
            recommended: None,
            confirm: false,
            skippable: false,
        };
        let (tx, mut rx) = oneshot::channel();
        let qs = vec![q("one"), q("two")];
        let ov = Overlay::new_ask(qs, tx);
        let (rest, _) = ov.on_key(key(KeyCode::Esc));
        assert!(rest.is_none());
        let all = rx.try_recv().unwrap().unwrap();
        assert_eq!(all.len(), 2, "cancelling answers every question");
    }

    #[test]
    fn ask_picker_shows_the_highlighted_description_below_the_options() {
        let opt = |label: &str, d: Option<&str>| AskOption {
            label: label.into(),
            description: d.map(Into::into),
            preview: None,
            kind: None,
        };
        let q = AskQuestion {
            question: "Which?".into(),
            context: None,
            header: None,
            options: vec![opt("Yes", Some("apply the change now and run the tests afterwards")), opt("No", None)],
            multi: false,
            recommended: None,
            confirm: false,
            skippable: false,
        };
        let (lines, _) = ask_select(&q).render(40, 20);
        let t: Vec<String> = lines.iter().map(crate::text::line::line_text).collect();
        assert_eq!(t[1].trim_end(), "❯ Yes", "{t:?}");
        assert_eq!(t[2], "    apply the change now and run the", "{t:?}");
        assert_eq!(t[3], "    tests afterwards", "{t:?}");
        assert_eq!(t[4], "  No", "{t:?}");
    }

    #[test]
    fn ask_picker_shows_a_multi_line_question_in_full() {
        let q = AskQuestion {
            question: "Deploy the canary?\nstatus: 3 services green\nerrors: none in the last hour".into(),
            context: None,
            header: None,
            options: vec![
                AskOption { label: "Yes".into(), description: None, preview: None, kind: None },
                AskOption { label: "No".into(), description: None, preview: None, kind: None },
            ],
            multi: false,
            recommended: None,
            confirm: false,
            skippable: false,
        };
        let (lines, cur) = ask_select(&q).render(30, 20);
        let t: Vec<String> = lines.iter().map(crate::text::line::line_text).collect();
        assert_eq!(
            t[..3],
            ["Deploy the canary?", "status: 3 services green", "errors: none in the last hour"],
            "{t:?}"
        );
        assert!(t[3].starts_with("❯ Yes"), "no filter line: {t:?}");
        assert_eq!(cur, (3, 0));
        assert!(lines.iter().all(|l| crate::text::line::line_width(l) <= 30), "{t:?}");
    }

    #[test]
    fn ask_picker_shows_the_context_above_the_question() {
        let q = AskQuestion {
            question: "Keep the old API?".into(),
            context: Some("Three callers in `src/ui` still use `Foo::old`; removing it breaks the build.".into()),
            header: None,
            options: vec![
                AskOption { label: "Keep".into(), description: None, preview: None, kind: None },
                AskOption { label: "Remove".into(), description: None, preview: None, kind: None },
            ],
            multi: false,
            recommended: None,
            confirm: false,
            skippable: false,
        };
        let (lines, cur) = ask_select(&q).render(30, 20);
        let t: Vec<String> = lines.iter().map(crate::text::line::line_text).collect();
        assert_eq!(t[0], "Three callers in `src/ui`", "{t:?}");
        let q_at = t.iter().position(|l| l == "Keep the old API?").expect("question row");
        assert!(q_at >= 3, "the context comes first: {t:?}");
        assert_eq!(cur.0, q_at + 1, "{t:?}");
        assert!(lines.iter().all(|l| crate::text::line::line_width(l) <= 30), "{t:?}");
        let (lines, _) = ask_select(&q).render(30, 4);
        let t: Vec<String> = lines.iter().map(crate::text::line::line_text).collect();
        assert!(t.iter().any(|l| l == "Keep the old API?"), "short screens keep the question: {t:?}");
    }

    #[test]
    fn ask_context_scrolls_in_the_bordered_panel() {
        use crossterm::style::Attribute;

        let q = AskQuestion {
            context: Some((0..12).map(|i| format!("context {i}")).collect::<Vec<_>>().join("\n")),
            ..ask_q(&["Yes", "No"], false, false, None)
        };
        let (mut ov, _) = ask_ov(q);
        let (lines, _) = ov.render(40, 12, 0);
        let text: Vec<_> = lines.iter().map(crate::text::line::line_text).collect();
        assert!(text.iter().any(|l| l.starts_with('╭')), "{text:?}");
        assert!(text.iter().any(|l| l.contains("context 0")), "{text:?}");
        assert!(text.iter().any(|l| l.contains('↕')), "{text:?}");
        assert!(text.iter().any(|l| l.contains("PgUp/PgDn")), "{text:?}");
        assert!(text.iter().any(|l| l.contains("❯ Yes")), "{text:?}");
        let context = lines.iter().flatten().find(|s| s.text.contains("context 0")).unwrap();
        assert!(!context.style.attributes.has(Attribute::Dim));
        let question = lines.iter().flatten().find(|s| s.text.contains("Q?")).unwrap();
        assert!(question.style.attributes.has(Attribute::Bold));
        for _ in 0..12 {
            ov = ov.on_key(key(KeyCode::PageDown)).0.expect("still open");
        }
        let (lines, _) = ov.render(40, 12, 0);
        let text: Vec<_> = lines.iter().map(crate::text::line::line_text).collect();
        assert!(text.iter().any(|l| l.contains("context 11")), "{text:?}");
        assert!(text.iter().any(|l| l.contains("Q?")), "{text:?}");
        assert!(text.iter().any(|l| l.contains("❯ Yes")), "{text:?}");
    }

    fn ask_ov(q: AskQuestion) -> (Overlay, oneshot::Receiver<Option<Vec<Answer>>>) {
        let (tx, rx) = oneshot::channel();
        (Overlay::new_ask(vec![q], tx), rx)
    }

    fn ask_q(labels: &[&str], multi: bool, confirm: bool, recommended: Option<usize>) -> AskQuestion {
        let options =
            labels.iter().map(|l| AskOption { label: l.to_string(), description: None, preview: None, kind: None });
        AskQuestion {
            question: "Q?".into(),
            context: None,
            header: None,
            options: options.collect(),
            multi,
            recommended,
            confirm,
            skippable: false,
        }
    }

    #[test]
    fn the_skip_row_answers_skipped_and_the_turn_goes_on() {
        let q = AskQuestion { skippable: true, ..ask_q(&["A", "B"], false, false, None) };
        let (ov, mut rx) = ask_ov(q.clone());
        let ov = press(ov, &[KeyCode::Down, KeyCode::Down, KeyCode::Down]);
        let (rest, _) = ov.on_key(key(KeyCode::Enter));
        assert!(rest.is_none());
        assert_eq!(answer(&mut rx), "[Skipped]");

        let (ov, mut rx) = ask_ov(AskQuestion { multi: true, ..q.clone() });
        let ov = press(ov, &[KeyCode::Char(' '), KeyCode::Down, KeyCode::Down, KeyCode::Down]);
        let (rest, _) = ov.on_key(key(KeyCode::Enter));
        assert!(rest.is_none());
        assert_eq!(answer(&mut rx), "[Skipped]", "Enter on the skip row wins over checked options");

        let (ov, mut rx) = ask_ov(ask_q(&["A", "B"], false, false, None));
        let ov = press(ov, &[KeyCode::Down, KeyCode::Down]);
        let (rest, _) = ov.on_key(key(KeyCode::Enter));
        assert!(rest.is_some(), "the last row is the free-text one unless the question is skippable");
        assert!(rx.try_recv().is_err());
    }

    /// Feeds keys to an open ask, which must stay open.
    fn press(ov: Overlay, keys: &[KeyCode]) -> Overlay {
        keys.iter().fold(ov, |ov, k| ov.on_key(key(*k)).0.expect("still open"))
    }

    fn typed(s: &str) -> Vec<KeyCode> {
        s.chars().map(KeyCode::Char).collect()
    }

    fn answer(rx: &mut oneshot::Receiver<Option<Vec<Answer>>>) -> String {
        let all = rx.try_recv().expect("replied").expect("answered");
        format!("{all:?}")
    }

    #[test]
    fn note_on_the_highlighted_option_is_submitted_with_it() {
        let (ov, mut rx) = ask_ov(ask_q(&["Skip", "Keep"], false, false, None));
        let ov = press(ov, &[KeyCode::Down, KeyCode::Right]);
        let ov = press(ov, &typed("tests later"));
        let (rest, ev) = ov.on_key(key(KeyCode::Enter));
        assert!(rest.is_none() && ev.is_none());
        assert_eq!(answer(&mut rx), r#"[Noted { picks: ["Keep"], note: "tests later" }]"#);
    }

    #[test]
    fn esc_leaves_the_note_open_and_enter_without_text_picks_plainly() {
        let (ov, mut rx) = ask_ov(ask_q(&["Skip", "Keep"], false, false, None));
        let ov = press(ov, &[KeyCode::Right, KeyCode::Char('x'), KeyCode::Esc]);
        assert!(ov.on_key(key(KeyCode::Enter)).0.is_none());
        assert_eq!(answer(&mut rx), r#"[Options(["Skip"])]"#);
    }

    #[test]
    fn multi_note_goes_with_the_list_of_picks() {
        let (ov, mut rx) = ask_ov(ask_q(&["X", "Y", "Z"], true, false, None));
        let ov = press(ov, &[KeyCode::Char(' '), KeyCode::Down, KeyCode::Char(' '), KeyCode::Tab]);
        let ov = press(ov, &typed("both"));
        assert!(ov.on_key(key(KeyCode::Enter)).0.is_none());
        assert_eq!(answer(&mut rx), r#"[Noted { picks: ["X", "Y"], note: "both" }]"#);
    }

    #[test]
    fn confirm_answers_on_y_or_n_at_once() {
        let (ov, mut rx) = ask_ov(ask_q(&["Yes", "No"], false, true, None));
        assert!(ov.on_key(key(KeyCode::Char('n'))).0.is_none());
        assert_eq!(answer(&mut rx), r#"[Options(["No"])]"#);
        let (ov, mut rx) = ask_ov(ask_q(&["Yes", "No"], false, true, None));
        assert!(ov.on_key(key(KeyCode::Char('Y'))).0.is_none());
        assert_eq!(answer(&mut rx), r#"[Options(["Yes"])]"#);
    }

    #[test]
    fn confirm_enter_takes_the_recommended_side_and_defaults_to_yes() {
        let (ov, mut rx) = ask_ov(ask_q(&["Yes", "No"], false, true, None));
        assert!(ov.on_key(key(KeyCode::Enter)).0.is_none());
        assert_eq!(answer(&mut rx), r#"[Options(["Yes"])]"#);
        let (ov, mut rx) = ask_ov(ask_q(&["Yes", "No"], false, true, Some(1)));
        assert!(ov.on_key(key(KeyCode::Enter)).0.is_none());
        assert_eq!(answer(&mut rx), r#"[Options(["No"])]"#);
    }

    #[test]
    fn confirm_tab_adds_a_note_before_submitting() {
        let (ov, mut rx) = ask_ov(ask_q(&["Yes", "No"], false, true, None));
        let ov = press(ov, &[KeyCode::Down, KeyCode::Tab]);
        let ov = press(ov, &typed("because"));
        assert!(ov.on_key(key(KeyCode::Enter)).0.is_none());
        assert_eq!(answer(&mut rx), r#"[Noted { picks: ["No"], note: "because" }]"#);
    }

    #[test]
    fn confirm_esc_cancels_like_any_ask() {
        let (ov, mut rx) = ask_ov(ask_q(&["Yes", "No"], false, true, None));
        assert!(ov.on_key(key(KeyCode::Esc)).0.is_none());
        assert_eq!(answer(&mut rx), "[Cancelled]");
    }

    #[test]
    fn digit_keys_pick_an_option_in_single_select() {
        let (ov, mut rx) = ask_ov(ask_q(&["A", "B", "C"], false, false, None));
        assert!(ov.on_key(key(KeyCode::Char('2'))).0.is_none());
        assert_eq!(answer(&mut rx), r#"[Options(["B"])]"#);
    }

    #[test]
    fn other_still_takes_custom_text() {
        let (ov, mut rx) = ask_ov(ask_q(&["A"], false, false, None));
        let ov = press(ov, &[KeyCode::Down, KeyCode::Enter]);
        assert!(press(ov, &typed("hi")).on_key(key(KeyCode::Enter)).0.is_none());
        assert_eq!(answer(&mut rx), r#"[Custom("hi")]"#);
    }

    #[test]
    fn picker_footer_names_the_keys() {
        let (lines, _) = ask_select(&ask_q(&["Yes", "No"], false, true, None)).render(60, 20);
        let t: Vec<String> = lines.iter().map(crate::text::line::line_text).collect();
        assert_eq!(t.last().map(String::as_str), Some("  y yes · n no · Enter choose · Tab/→ note · Esc cancel"));
    }

    #[test]
    fn every_option_shows_its_preview_and_a_declared_kind_labels_the_box() {
        let mut q = ask_q(&["A", "B"], false, false, None);
        q.options[0].preview = Some("fn a() {}".into());
        q.options[1].preview = Some("plain words".into());
        q.options[1].kind = Some(mu_base::events::PreviewKind::Diff);
        let (lines, _) = ask_select(&q).render(40, 30);
        let t: Vec<String> = lines.iter().map(crate::text::line::line_text).collect();
        assert!(t[2].contains("┌─ code ─") && t[3].contains("fn a() {}"), "{t:?}");
        assert!(t[6].contains("┌─ diff ─") && t[7].contains("plain words"), "{t:?}");
    }

    /// Two questions with two options each, for the back keys and the review.
    fn two_q() -> Vec<AskQuestion> {
        vec![ask_q(&["A", "B"], false, false, None), ask_q(&["X", "Y"], false, false, None)]
    }

    fn open(qs: Vec<AskQuestion>) -> (Overlay, oneshot::Receiver<Option<Vec<Answer>>>) {
        let (tx, rx) = oneshot::channel();
        (Overlay::new_ask(qs, tx), rx)
    }

    /// The overlay's rows as text, trailing spaces dropped.
    fn screen(ov: &mut Overlay) -> Vec<String> {
        let (lines, _) = ov.render(100, 30, 0);
        lines.iter().map(|l| crate::text::line::line_text(l).trim_end().to_string()).collect()
    }

    fn panel_row(text: &str) -> String {
        format!("│{text:<98}│")
    }

    #[test]
    fn left_and_shift_tab_go_back_and_restore_the_answer() {
        let (ov, mut rx) = open(two_q());
        let ov = press(ov, &[KeyCode::Down, KeyCode::Enter]);
        let mut ov = press(ov, &[KeyCode::Left]);
        let t = screen(&mut ov);
        assert!(t.iter().any(|l| l == &panel_row("❯ B")), "the earlier answer is preselected: {t:?}");
        assert!(t.iter().any(|l| l.contains("1/2")), "{t:?}");
        let ov = press(ov, &[KeyCode::Enter, KeyCode::Down, KeyCode::Enter]);
        let mut ov = press(ov, &[KeyCode::BackTab]);
        assert!(screen(&mut ov).iter().any(|l| l.contains("2/2")), "Shift-Tab on the review goes back to question two");
        let ov = press(ov, &[KeyCode::Enter]);
        assert!(ov.on_key(key(KeyCode::Enter)).0.is_none());
        assert_eq!(answer(&mut rx), r#"[Options(["B"]), Options(["Y"])]"#);
    }

    #[test]
    fn typing_does_not_filter_an_ask_and_left_still_goes_back() {
        let (ov, _rx) = open(two_q());
        let ov = press(ov, &[KeyCode::Enter]);
        let ov = press(ov, &typed("x"));
        let mut ov = press(ov, &[KeyCode::Left]);
        let t = screen(&mut ov);
        assert!(t.iter().any(|l| l.contains("1/2")), "{t:?}");
        assert!(t.iter().any(|l| l.contains("A")) && t.iter().any(|l| l.contains("B")), "nothing was filtered out");
    }

    #[test]
    fn shift_tab_restores_notes_and_custom_text() {
        let (ov, mut rx) = open(two_q());
        let ov = press(ov, &[KeyCode::Right]);
        let ov = press(ov, &typed("why"));
        let ov = press(ov, &[KeyCode::Enter]);
        let mut ov = press(ov, &[KeyCode::BackTab]);
        assert!(
            screen(&mut ov).iter().any(|l| l == &panel_row("    note: why")),
            "the note comes back under its option"
        );
        let ov = press(ov, &[KeyCode::Down, KeyCode::Down, KeyCode::Enter]);
        let ov = press(ov, &typed("hi"));
        let ov = press(ov, &[KeyCode::Enter]);
        let mut ov = press(ov, &[KeyCode::BackTab]);
        assert!(
            screen(&mut ov).iter().any(|l| l == &panel_row("❯ hi")),
            "the custom answer comes back on the free row"
        );
        let ov = press(ov, &[KeyCode::Enter, KeyCode::Enter]);
        let mut ov = press(ov, &[KeyCode::Enter]);
        assert!(screen(&mut ov).iter().any(|l| l.contains("Review your answers")));
        assert!(ov.on_key(key(KeyCode::Enter)).0.is_none());
        assert_eq!(answer(&mut rx), r#"[Custom("hi"), Options(["X"])]"#);
    }

    #[test]
    fn review_reopens_a_question_and_returns_to_the_review() {
        let (ov, mut rx) = open(two_q());
        let mut ov = press(ov, &[KeyCode::Enter, KeyCode::Enter]);
        let t = screen(&mut ov);
        assert!(t.iter().any(|l| l.contains("1. Q?") && l.ends_with('A')), "{t:?}");
        assert!(t.iter().any(|l| l.contains("2. Q?") && l.ends_with('X')), "{t:?}");
        assert!(t.iter().any(|l| l == "❯ Submit answers"), "Submit starts highlighted: {t:?}");
        let mut ov = press(ov, &[KeyCode::Up, KeyCode::Enter]);
        let t = screen(&mut ov);
        assert!(t.iter().any(|l| l.contains("2/2")), "{t:?}");
        assert!(t.iter().any(|l| l == &panel_row("❯ X")), "the earlier answer is preselected: {t:?}");
        let mut ov = press(ov, &[KeyCode::Down, KeyCode::Enter]);
        assert!(screen(&mut ov).iter().any(|l| l.contains("2. Q?") && l.ends_with('Y')));
        assert!(ov.on_key(key(KeyCode::Enter)).0.is_none());
        assert_eq!(answer(&mut rx), r#"[Options(["A"]), Options(["Y"])]"#);
    }

    #[test]
    fn esc_cancels_the_ask_from_the_review_and_from_any_question() {
        let (ov, mut rx) = open(two_q());
        let ov = press(ov, &[KeyCode::Enter, KeyCode::Enter]);
        assert!(ov.on_key(key(KeyCode::Esc)).0.is_none());
        assert_eq!(answer(&mut rx), "[Cancelled, Cancelled]", "Esc on the review cancels everything");
        let (ov, mut rx) = open(two_q());
        let ov = press(ov, &[KeyCode::Enter]);
        assert!(ov.on_key(key(KeyCode::Esc)).0.is_none());
        assert_eq!(answer(&mut rx), "[Options([\"A\"]), Cancelled]", "Esc on question 2 cancels from there");
    }

    #[test]
    fn left_steps_back_from_the_review_and_then_from_each_question() {
        let (ov, _rx) = open(two_q());
        let ov = press(ov, &[KeyCode::Enter, KeyCode::Enter, KeyCode::Left]);
        let mut ov = ov;
        let t = screen(&mut ov);
        assert!(t.iter().any(|l| l.contains("2/2")) && t.iter().any(|l| l == &panel_row("❯ X")), "{t:?}");
        let mut ov = press(ov, &[KeyCode::Left]);
        let t = screen(&mut ov);
        assert!(t.iter().any(|l| l.contains("1/2")) && t.iter().any(|l| l == &panel_row("❯ A")), "{t:?}");
    }

    #[test]
    fn footers_name_the_back_and_cancel_keys_per_step() {
        let (ov, _rx) = open(two_q());
        let mut ov = ov;
        assert!(screen(&mut ov).last().is_some_and(|l| l.ends_with("Esc cancel")), "{:?}", screen(&mut ov));
        let ov = press(ov, &[KeyCode::Enter]);
        let mut ov = ov;
        assert!(screen(&mut ov).last().is_some_and(|l| l.ends_with("⇧Tab back · Esc cancel")));
        let ov = press(ov, &[KeyCode::Enter]);
        let mut ov = ov;
        assert!(screen(&mut ov).last().is_some_and(|l| l.contains("Enter reopen or submit")));
    }

    #[test]
    fn ask_hints_are_outside_the_panel_in_each_input_mode() {
        let (ov, _rx) = open(vec![ask_q(&["A"], false, false, None)]);
        let mut ov = ov;
        let t = screen(&mut ov);
        assert!(t[t.len() - 2].starts_with('╰'));
        assert!(t.last().unwrap().ends_with("Esc cancel"));

        let mut note = press(ov, &[KeyCode::Right]);
        let t = screen(&mut note);
        assert!(t[t.len() - 2].starts_with('╰'));
        assert_eq!(t.last().unwrap(), "Enter submit · Shift/Alt/Ctrl-Enter newline · Esc to options");

        let ov = press(note, &[KeyCode::Esc, KeyCode::Down, KeyCode::Enter]);
        let mut text = ov;
        let t = screen(&mut text);
        assert!(t[t.len() - 2].starts_with('╰'));
        assert_eq!(t.last().unwrap(), "Enter submit · Shift/Alt/Ctrl-Enter newline · Esc to options");
    }

    #[test]
    fn a_single_question_shows_no_position_and_no_review() {
        let (ov, mut rx) = open(vec![ask_q(&["A", "B"], false, false, None)]);
        let mut ov = ov;
        assert!(!screen(&mut ov).iter().any(|l| l.contains("/1")));
        assert!(ov.on_key(key(KeyCode::Enter)).0.is_none());
        assert_eq!(answer(&mut rx), r#"[Options(["A"])]"#);
    }

    #[test]
    fn going_back_restores_a_multi_select_set() {
        let qs = vec![ask_q(&["X", "Y", "Z"], true, false, None), ask_q(&["P", "Q"], false, false, None)];
        let (ov, mut rx) = open(qs);
        let ov = press(ov, &[KeyCode::Char(' '), KeyCode::Down, KeyCode::Char(' '), KeyCode::Enter]);
        let mut ov = press(ov, &[KeyCode::BackTab]);
        let t = screen(&mut ov);
        assert!(t.iter().any(|l| l == &panel_row("❯ [x] X")), "{t:?}");
        assert!(t.iter().any(|l| l == &panel_row("  [x] Y")), "{t:?}");
        assert!(t.iter().any(|l| l == &panel_row("  [ ] Z")), "{t:?}");
        let ov = press(ov, &[KeyCode::Enter, KeyCode::Enter]);
        assert!(ov.on_key(key(KeyCode::Enter)).0.is_none());
        assert_eq!(answer(&mut rx), r#"[Options(["X", "Y"]), Options(["P"])]"#);
    }
}
