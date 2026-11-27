//! Fake sessions, agents, jobs and requests, built from the screens in `docs/design/gui-sketch.md`.
//! All text is `&'static str`: the data is written once and views only read it. Times, costs and counts are
//! display strings, so no view formats anything.

pub type SessionId = usize;

/// Session ids, one per scenario (see `build`).
pub mod sid {
    use super::SessionId;
    pub const DESIGN: SessionId = 0;
    pub const FLAKY: SessionId = 1;
    pub const REST: SessionId = 2;
    pub const STREAM: SessionId = 3;
    pub const APPROVAL: SessionId = 4;
    pub const APPROVAL_SCOPE: SessionId = 5;
    pub const QUESTIONS: SessionId = 6;
    pub const QUESTIONS_REVIEW: SessionId = 7;
    pub const PLAN: SessionId = 8;
    pub const COMPOSER: SessionId = 9;
    pub const DEPLOY: SessionId = 10;
}

/// A platform's key hint: `(mac, linux)`.
pub fn plat(mac: &'static str, linux: &'static str) -> &'static str {
    if cfg!(target_os = "macos") { mac } else { linux }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AgentState {
    /// ⚑
    NeedsYou,
    /// ●
    Running,
    /// ○
    Idle,
    /// ✓ finished and unseen
    Finished,
    /// ✗
    Failed,
}

impl AgentState {
    pub fn icon(self) -> &'static str {
        match self {
            Self::NeedsYou => "⚑",
            Self::Running => "●",
            Self::Idle => "○",
            Self::Finished => "✓",
            Self::Failed => "✗",
        }
    }
}

// ---------------------------------------------------------------- transcript

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SpanStyle {
    Plain,
    Code,
    Bold,
    Italic,
    Link,
}

#[derive(Clone, Debug)]
pub struct Span {
    pub text: &'static str,
    pub style: SpanStyle,
}

/// Parsed markdown, as the semantic blocks will carry it.
#[derive(Clone, Debug)]
pub enum Md {
    Heading(u8, &'static str),
    Para(Vec<Span>),
    List { ordered: bool, items: Vec<Vec<Span>> },
    Code { lang: &'static str, lines: Vec<&'static str> },
}

fn p(text: &'static str) -> Span {
    Span { text, style: SpanStyle::Plain }
}
fn c(text: &'static str) -> Span {
    Span { text, style: SpanStyle::Code }
}
fn b(text: &'static str) -> Span {
    Span { text, style: SpanStyle::Bold }
}
fn i(text: &'static str) -> Span {
    Span { text, style: SpanStyle::Italic }
}
fn l(text: &'static str) -> Span {
    Span { text, style: SpanStyle::Link }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ToolKind {
    /// ⌕
    Read,
    /// ⌕
    Grep,
    /// ✎
    Edit,
    /// ✎
    Write,
    /// ⇣
    Web,
    /// ◇
    Other,
}

impl ToolKind {
    pub fn icon(self) -> &'static str {
        match self {
            Self::Read | Self::Grep => "⌕",
            Self::Edit | Self::Write => "✎",
            Self::Web => "⇣",
            Self::Other => "◇",
        }
    }
}

/// One muted line: icon, verb, target, short facts.
#[derive(Clone, Debug)]
pub struct Tool {
    pub kind: ToolKind,
    pub verb: &'static str,
    pub target: &'static str,
    pub facts: &'static str,
    /// The arguments are still arriving: the row ends in `…`.
    pub streaming: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DiffKind {
    Context,
    Add,
    Del,
}

#[derive(Clone, Debug)]
pub struct DiffLine {
    pub no: Option<u32>,
    pub kind: DiffKind,
    pub text: &'static str,
    /// Char range of the changed part of the line, drawn in the stronger shade.
    pub changed: Option<(usize, usize)>,
}

#[derive(Clone, Debug)]
pub struct Diff {
    pub file: &'static str,
    pub plus: u32,
    pub minus: u32,
    pub lines: Vec<DiffLine>,
    /// "N more lines" past the cut-off; 0 for none.
    pub more: u32,
}

#[derive(Clone, Debug)]
pub struct Bash {
    pub command: &'static str,
    /// `None` while it runs.
    pub exit: Option<i32>,
    /// Duration for a finished call, the live timer for a running one.
    pub time: &'static str,
    /// The last 10 lines at most, or the first 10 of an error result.
    pub tail: Vec<&'static str>,
    /// "(N lines)" not shown.
    pub hidden: u32,
}

#[derive(Clone, Debug)]
pub struct Sub {
    pub name: &'static str,
    /// scout / task
    pub kind: &'static str,
    pub time: &'static str,
    pub summary: &'static str,
    pub done: bool,
}

#[derive(Clone, Debug)]
pub struct TodoItem {
    pub text: &'static str,
    /// 0 pending, 1 in progress, 2 done
    pub state: u8,
}

#[derive(Clone, Debug)]
pub enum Block {
    /// The user's prompt, with an optional attached image's label.
    User { text: &'static str, image: Option<&'static str> },
    /// A turn no prompt started (a finished job or a `mu notify` message).
    Woken { text: &'static str },
    /// `live` holds the streaming tail (dimmed) while it thinks; folded to one line once `live` is `None`.
    Thinking { time: &'static str, live: Option<&'static str> },
    Tool(Tool),
    Prose(Vec<Md>),
    Diff(Diff),
    Bash(Bash),
    Subagents(Vec<Sub>),
    Todo(Vec<TodoItem>),
    /// ↻
    Retry(&'static str),
    Notice(&'static str),
    /// A red edge.
    Error(&'static str),
    Compaction(&'static str),
    /// "✓ Ran once · answered on devbox"
    Answered(&'static str),
    /// A parked plan.
    Plan(&'static str),
}

// ---------------------------------------------------------------- requests

#[derive(Clone, Debug)]
pub struct Choice {
    /// The row's shortcut: `1`, `y`, `n`.
    pub key: &'static str,
    pub label: &'static str,
    /// The grant it remembers, dimmed at the row's end.
    pub detail: &'static str,
}

#[derive(Clone, Debug)]
pub struct Approval {
    /// `tool: summary`
    pub title: &'static str,
    pub from: Option<&'static str>,
    pub why: &'static str,
    /// The command, content or arguments, in a code box.
    pub body: &'static str,
    /// 1: how much to allow. 2: where to remember it.
    pub step: u8,
    pub choices: Vec<Choice>,
    pub selected: usize,
    pub hint: &'static str,
}

#[derive(Clone, Debug)]
pub struct QOption {
    pub label: &'static str,
    pub desc: &'static str,
    pub recommended: bool,
    pub preview: Option<&'static str>,
}

#[derive(Clone, Debug)]
pub struct Question {
    pub header: &'static str,
    pub index: u8,
    pub total: u8,
    /// Dimmed context above the question.
    pub context: &'static str,
    /// The strongest line.
    pub question: &'static str,
    pub options: Vec<QOption>,
    pub selected: usize,
    pub hint: &'static str,
}

#[derive(Clone, Debug)]
pub struct QuestionsReview {
    pub header: &'static str,
    /// (question, answer)
    pub answers: Vec<(&'static str, &'static str)>,
    pub hint: &'static str,
}

#[derive(Clone, Debug)]
pub struct ReviewAction {
    pub key: &'static str,
    pub label: &'static str,
    pub detail: &'static str,
}

/// A document review: a plan, with the mode's actions and the "continue with" model slider.
#[derive(Clone, Debug)]
pub struct DocReview {
    pub title: &'static str,
    pub doc: Vec<Md>,
    pub actions: Vec<ReviewAction>,
    pub selected: usize,
    pub models: Vec<&'static str>,
    pub model: usize,
    pub hint: &'static str,
}

#[derive(Clone, Debug)]
pub enum Request {
    Approval(Approval),
    Question(Question),
    QuestionsReview(QuestionsReview),
    Review(DocReview),
}

// ---------------------------------------------------------------- composer

#[derive(Clone, Debug)]
pub struct QueueRow {
    /// `steer` or `after turn`
    pub kind: &'static str,
    pub text: &'static str,
    /// Shows "shift+↑ edit": the newest row only.
    pub newest: bool,
}

#[derive(Clone, Debug)]
pub struct SlashMenu {
    pub entries: Vec<(&'static str, &'static str)>,
    pub selected: usize,
}

#[derive(Clone, Debug)]
pub struct Composer {
    /// What is typed before the cursor.
    pub before: &'static str,
    /// What follows the cursor (typed text), empty at the end of the line.
    pub after: &'static str,
    /// Dimmed prediction after the cursor, taken with Tab.
    pub ghost: &'static str,
    /// Shown when empty.
    pub placeholder: &'static str,
    /// (name, size) of an attached image.
    pub image: Option<(&'static str, &'static str)>,
    pub slash: Option<SlashMenu>,
}

// ---------------------------------------------------------------- sessions

#[derive(Clone, Debug)]
pub struct Session {
    pub id: SessionId,
    pub title: &'static str,
    pub host: &'static str,
    pub branch: &'static str,
    pub state: AgentState,
    pub blocks: Vec<Block>,
    /// "↓ new output": scrolled up while the turn streams.
    pub new_output: bool,
    pub request: Option<Request>,
    pub queue: Vec<QueueRow>,
    pub composer: Composer,
    // Session bar segments.
    pub model: &'static str,
    pub thinking: &'static str,
    /// ask / edits / yolo / plan
    pub mode: &'static str,
    pub ctx_pct: u8,
    pub cost: &'static str,
    /// The turn's spinner and timer; `None` when idle.
    pub turn_timer: Option<&'static str>,
}

// ---------------------------------------------------------------- agents and jobs

#[derive(Clone, Debug)]
pub struct AgentRow {
    pub state: AgentState,
    pub title: &'static str,
    /// tui / agent / print / subagent
    pub kind: &'static str,
    pub host: &'static str,
    pub branch: &'static str,
    /// Who is attached, `—` for none.
    pub attached: &'static str,
    pub ran: &'static str,
    pub cost: &'static str,
    /// The row for the session the focused tile shows (◂).
    pub current: bool,
    pub subs: Vec<AgentRow>,
}

#[derive(Clone, Debug)]
pub struct RecentRow {
    pub title: &'static str,
    pub age: &'static str,
    pub cost: &'static str,
}

#[derive(Clone, Debug)]
pub struct AgentGroup {
    /// A project path.
    pub name: &'static str,
    pub rows: Vec<AgentRow>,
    pub recent: Vec<RecentRow>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum JobState {
    Running,
    Done,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug)]
pub struct JobRow {
    pub state: JobState,
    pub name: &'static str,
    /// scout / task / bash
    pub agent: &'static str,
    pub tools: u32,
    pub elapsed: &'static str,
    pub cost: &'static str,
    pub last: &'static str,
    /// Details of the selected job: its task, then recent output.
    pub task: &'static str,
    pub output: Vec<&'static str>,
}

// ---------------------------------------------------------------- review

#[derive(Clone, Debug)]
pub struct ReviewFile {
    pub path: &'static str,
    pub plus: u32,
    pub minus: u32,
    pub expanded: bool,
    pub lines: Vec<DiffLine>,
}

#[derive(Clone, Debug)]
pub struct ReviewData {
    /// "Branch since main"
    pub scope: &'static str,
    pub plus: u32,
    pub minus: u32,
    pub files: Vec<ReviewFile>,
}

// ---------------------------------------------------------------- palette

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EntryKind {
    Action,
    Command,
    Agent,
    Layout,
    View,
}

#[derive(Clone, Debug)]
pub struct PaletteEntry {
    pub kind: EntryKind,
    /// `Tile: split right`, `/compact`, `Agent: Deploy pod adapter`
    pub label: &'static str,
    /// Dimmed text after the label (a description, host and state).
    pub detail: &'static str,
    /// The current binding for this platform, `""` for none.
    pub key: &'static str,
}

#[derive(Clone, Debug)]
pub struct PaletteData {
    /// What is typed in the full palette.
    pub query: &'static str,
    pub entries: Vec<PaletteEntry>,
    /// The agent palette: agents and sessions, most recent first.
    pub agents: Vec<PaletteEntry>,
}

#[derive(Clone, Debug)]
pub struct StatusData {
    pub profile: &'static str,
    pub running: u32,
    pub need_you: u32,
    pub today: &'static str,
    /// (remote, connected)
    pub remotes: Vec<(&'static str, bool)>,
}

pub struct Data {
    pub sessions: Vec<Session>,
    /// The agent list's "Needs you" group.
    pub needs_you: Vec<AgentRow>,
    pub groups: Vec<AgentGroup>,
    pub jobs: Vec<JobRow>,
    /// The selected job in the jobs view.
    pub job_selected: usize,
    pub review: ReviewData,
    pub palette: PaletteData,
    pub status: StatusData,
}

impl Data {
    pub fn session(&self, id: SessionId) -> &Session {
        &self.sessions[id]
    }

    /// Sessions with a request waiting, oldest first (the order of `sessions`).
    pub fn waiting(&self) -> Vec<SessionId> {
        self.sessions.iter().filter(|s| s.request.is_some()).map(|s| s.id).collect()
    }
}

// ================================================================ content

fn line(no: u32, kind: DiffKind, text: &'static str) -> DiffLine {
    DiffLine { no: Some(no), kind, text, changed: None }
}
fn line_hi(no: u32, kind: DiffKind, text: &'static str, changed: (usize, usize)) -> DiffLine {
    DiffLine { no: Some(no), kind, text, changed: Some(changed) }
}

fn tool(kind: ToolKind, verb: &'static str, target: &'static str, facts: &'static str) -> Block {
    Block::Tool(Tool { kind, verb, target, facts, streaming: false })
}

fn serve_diff() -> Diff {
    use DiffKind::*;
    Diff {
        file: "src/run/serve.rs",
        plus: 6,
        minus: 2,
        lines: vec![
            line(370, Context, "    if let Some(old) = &self.client {"),
            line_hi(371, Del, "        if old.who == hello.client && !hello.client.is_empty() {", (11, 62)),
            line_hi(371, Add, "        let same = !hello.client.is_empty() && old.who == hello.client;", (11, 70)),
            line(372, Add, "        if same {"),
            line(373, Add, "            self.take_over(hello);"),
            line(374, Context, "            return Ok(());"),
            line(375, Context, "        }"),
            line(376, Context, "        if !hello.take {"),
            line(377, Context, "            return Err(Busy);"),
        ],
        more: 0,
    }
}

fn base_composer() -> Composer {
    Composer {
        before: "",
        after: "",
        ghost: "",
        placeholder: "Message mu — / for commands",
        image: None,
        slash: None,
    }
}

#[allow(clippy::too_many_arguments)]
fn session(id: SessionId, title: &'static str, host: &'static str, branch: &'static str, state: AgentState) -> Session {
    Session {
        id,
        title,
        host,
        branch,
        state,
        blocks: vec![],
        new_output: false,
        request: None,
        queue: vec![],
        composer: base_composer(),
        model: "opus-5",
        thinking: "high",
        mode: "edits",
        ctx_pct: 41,
        cost: "$3.10",
        turn_timer: None,
    }
}

fn user(text: &'static str) -> Block {
    Block::User { text, image: None }
}

fn rest_blocks() -> Vec<Block> {
    vec![
        Block::User {
            text: "make attach reconnect without asking when the client id is the same, and add a test",
            image: Some("image 1"),
        },
        Block::Thinking { time: "Thought for 14s", live: None },
        tool(ToolKind::Read, "Read", "src/run/serve.rs", "L340-400"),
        tool(ToolKind::Read, "Read", "src/run/attach.rs", "142 lines"),
        tool(ToolKind::Grep, "Grep", "`reconnect` in src/", "6 matches"),
        Block::Prose(vec![
            Md::Heading(2, "Where the flag is lost"),
            Md::Para(vec![
                p("The agent already ignores "),
                c("reconnect"),
                p(" (it matches the hello with "),
                c(".."),
                p("). I'll make a hello with the same client id take over without "),
                c("Busy"),
                p(", and test it with two connections."),
            ]),
            Md::List {
                ordered: false,
                items: vec![
                    vec![b("serve.rs"), p(": compare the client id before refusing")],
                    vec![b("attach.rs"), p(": send the id it already keeps in "), c("ClientAdapter")],
                    vec![b("tests/agent.rs"), p(": two connections, the second with the same id")],
                ],
            },
        ]),
        tool(ToolKind::Edit, "Edit", "src/run/serve.rs", "+6 −2"),
        Block::Diff(serve_diff()),
        Block::Bash(Bash {
            command: "cargo test -p mu --test agent reconnect",
            exit: Some(0),
            time: "41s",
            tail: vec!["running 2 tests", "test result: ok. 2 passed; 0 failed"],
            hidden: 0,
        }),
        Block::Bash(Bash {
            command: "cargo clippy -p mu -- -D warnings",
            exit: Some(101),
            time: "18s",
            tail: vec![
                "error: this `if` has identical blocks",
                "   --> src/run/serve.rs:373:9",
                "    |",
                "373 |         if same {",
                "    = note: `-D clippy::if-same-then-else` implied by `-D warnings`",
                "error: could not compile `mu` (bin \"mu\") due to 1 previous error",
            ],
            hidden: 0,
        }),
        Block::Subagents(vec![
            Sub {
                name: "ReviewDiff",
                kind: "scout",
                time: "1m 12s",
                summary: "No issues; one nit on the error message in take_over()",
                done: true,
            },
            Sub {
                name: "CheckMacOS",
                kind: "task",
                time: "3m 40s",
                summary: "Builds and passes on the Mac; the attach tests took 2m",
                done: true,
            },
        ]),
        Block::Todo(vec![
            TodoItem { text: "Compare the client id in join()", state: 2 },
            TodoItem { text: "Test with two connections", state: 2 },
            TodoItem { text: "Document the new rule", state: 0 },
        ]),
        Block::Compaction("Compacted 38 messages · saved 41k tokens"),
        Block::Notice("Switched to opus-5 (high)"),
        Block::Prose(vec![Md::Para(vec![
            p("Done: a hello with the same client id now takes over silently, and "),
            c("tests/agent.rs"),
            p(" covers it. See "),
            l("docs/remote-contracts.md"),
            p(" for the "),
            i("take"),
            p(" rules."),
        ])]),
    ]
}

fn stream_blocks() -> Vec<Block> {
    vec![
        user("why does the second attach get Busy even with the same client id?"),
        Block::Woken { text: "Background job finished: cargo build -p mu (ok, 1m 02s)" },
        Block::Thinking {
            time: "6s",
            live: Some(
                "The reconnect flag is sent but serve.rs never reads it, so the fix belongs in join();\ncheck whether take() also clears the old client's pending requests …",
            ),
        },
        tool(ToolKind::Read, "Read", "src/run/serve.rs", "…"),
        Block::Tool(Tool {
            kind: ToolKind::Edit,
            verb: "Edit",
            target: "src/run/serve.rs",
            facts: "{\"old\": \"if old.who == hello.client\", \"new\": \"let same = …",
            streaming: true,
        }),
        Block::Bash(Bash {
            command: "cargo test -p mu --test agent reconnect",
            exit: None,
            time: "12s",
            tail: vec!["   Compiling mu v0.1.0", "   Compiling mu-tui v0.1.0", "   Compiling mu-agent v0.1.0"],
            hidden: 0,
        }),
        Block::Retry("Retrying in 8s (2/5): overloaded"),
        Block::Error("provider: 529 overloaded_error — the request was not accepted"),
    ]
}

fn approval_session(step: u8, id: SessionId) -> Session {
    let mut s = session(id, "Attach reconnect", "devbox", "fix/attach-reconnect", AgentState::NeedsYou);
    s.mode = "ask";
    s.blocks = vec![
        Block::Answered("✓ Ran once · cargo test -p mu --test agent · answered on laptop"),
        Block::Bash(Bash {
            command: "cargo test -p mu --test agent reconnect",
            exit: Some(0),
            time: "41s",
            tail: vec!["test result: ok. 2 passed; 0 failed"],
            hidden: 0,
        }),
        Block::Prose(vec![Md::Para(vec![p("The tests pass. I'll push the branch so the Mac job can pick it up.")])]),
    ];
    s.composer.placeholder = "Message mu — the agent waits for your answer above";
    let (choices, hint) = if step == 1 {
        (
            vec![
                Choice { key: "1", label: "Yes, just this once", detail: "" },
                Choice {
                    key: "2",
                    label: "For what it asked for",
                    detail: "git push origin HEAD:refs/heads/fix/attach-reconnect",
                },
                Choice { key: "3", label: "For git push", detail: "git push *" },
                Choice { key: "4", label: "No", detail: "" },
            ],
            "y yes · n no · 1–4 pick · e type a pattern · Enter choose · Tab/→ note · Esc cancel",
        )
    } else {
        (
            vec![
                Choice { key: "1", label: "This session", detail: "until it ends" },
                Choice { key: "2", label: "This project", detail: "~/code/private/mu" },
                Choice { key: "3", label: "Everywhere", detail: "all projects" },
                Choice { key: "4", label: "No", detail: "" },
            ],
            "1–4 pick · Enter choose · ← back · Esc cancel",
        )
    };
    s.request = Some(Request::Approval(Approval {
        title: "bash: git push origin HEAD:refs/heads/fix/attach-reconnect",
        from: Some("CheckMacOS"),
        why: "not covered by your rules (mode: ask)",
        body: "git push origin HEAD:refs/heads/fix/attach-reconnect",
        step,
        choices,
        selected: if step == 1 { 0 } else { 1 },
        hint,
    }));
    s
}

fn question_session(review: bool, id: SessionId) -> Session {
    let mut s = session(id, "Desktop app", "local", "gui-design", AgentState::NeedsYou);
    s.mode = "plan";
    s.blocks = vec![
        user("let's settle how observers connect to an agent"),
        tool(ToolKind::Read, "Read", "src/run/serve.rs", "L1-120"),
        Block::Prose(vec![Md::Para(vec![p(
            "Three things are open before the wire bump. I'll ask them one at a time.",
        )])]),
    ];
    s.composer.placeholder = "Message mu — the agent waits for your answer above";
    s.request = Some(if review {
        Request::QuestionsReview(QuestionsReview {
            header: "Desktop app · review your answers",
            answers: vec![
                ("Which transport carries observers?", "The agent's existing socket"),
                ("How does a connection become an observer?", "Role in hello"),
                ("Who may answer an approval?", "Any connection; the first answer wins"),
            ],
            hint: "Enter submit · ← back · 1–3 change an answer · Esc cancel",
        })
    } else {
        Request::Question(Question {
            header: "Desktop app",
            index: 2,
            total: 3,
            context: "The agent accepts one client today; a cockpit needs more.",
            question: "How does a connection become an observer?",
            options: vec![
                QOption {
                    label: "Role in hello",
                    desc: "Explicit; old semantics kept",
                    recommended: true,
                    preview: Some("{\"hello\": {\n  \"role\": \"observe\",\n  \"attend\": true,\n  \"client\": \"laptop\"\n}}"),
                },
                QOption {
                    label: "Observer by default",
                    desc: "Busy disappears",
                    recommended: false,
                    preview: Some("{\"hello\": {\n  \"client\": \"laptop\",\n  \"control\": false\n}}"),
                },
                QOption { label: "Something else…", desc: "", recommended: false, preview: None },
            ],
            selected: 0,
            hint: "1–3 pick · Enter choose · n note · s skip · ← back · Esc cancel",
        })
    });
    s
}

fn plan_session() -> Session {
    let mut s = session(sid::PLAN, "Gateway sign-in", "local", "feat/gateway-login", AgentState::NeedsYou);
    s.mode = "plan";
    s.blocks = vec![
        user("plan how /login signs in to a gateway provider"),
        tool(ToolKind::Read, "Read", "plugins/gateway/src/main.rs", "310 lines"),
        tool(ToolKind::Grep, "Grep", "`Pushed` in crates/", "9 matches"),
        Block::Plan("Plan parked: Sign in to gateway providers from /login"),
    ];
    s.composer.placeholder = "Message mu — the agent waits for your answer above";
    s.request = Some(Request::Review(DocReview {
        title: "Plan · Sign in to gateway providers from /login",
        doc: vec![
            Md::Heading(1, "Sign in to gateway providers from /login"),
            Md::Para(vec![
                p("The gateway runs the provider's own login. mu shows its "),
                b("notices"),
                p(" and the address it asks you to open, then waits for the code."),
            ]),
            Md::Heading(2, "Steps"),
            Md::List {
                ordered: true,
                items: vec![
                    vec![p("Add a "), c("Pushed"), p(" event for sign-in addresses, sent to one connection.")],
                    vec![p("Show the address in a dialog with "), b("o"), p(" to open it and "), b("c"), p(" to copy.")],
                    vec![p("Store the result in "), c("core/secrets"), p(", namespaced per plugin.")],
                    vec![p("Test the flow against a fake gateway that answers after one poll.")],
                ],
            },
            Md::Code {
                lang: "rust",
                lines: vec!["enum Pushed {", "    SignIn { url: String, code: Option<String> },", "}"],
            },
            Md::Heading(2, "Risks"),
            Md::Para(vec![p("A second client must not see the first one's address. "), i("Per-connection delivery covers it.")]),
        ],
        actions: vec![
            ReviewAction { key: "1", label: "Execute", detail: "start implementing in this session" },
            ReviewAction { key: "2", label: "Execute with a fresh context", detail: "compact first" },
            ReviewAction { key: "3", label: "Keep planning", detail: "stay in plan mode" },
            ReviewAction { key: "4", label: "Refine", detail: "type what to change" },
            ReviewAction { key: "5", label: "Park", detail: "save the plan and leave plan mode" },
        ],
        selected: 0,
        models: vec!["sonnet-5", "opus-5", "opus-5 (max)"],
        model: 1,
        hint: "Tab document/actions · 1–5 pick · Enter choose · c copy · Ctrl+G edit · n note · o open in a tile · Esc cancel",
    }));
    s
}

fn composer_session() -> Session {
    let mut s = session(sid::COMPOSER, "Attach tests", "local", "gui-design", AgentState::Running);
    s.blocks = vec![
        user("run the attach tests"),
        tool(ToolKind::Read, "Read", "tests/agent.rs", "212 lines"),
        Block::Bash(Bash {
            command: "cargo test -p mu --test agent",
            exit: None,
            time: "1m 12s",
            tail: vec!["running 14 tests", "test attach_reconnect ... ok", "test attach_busy ... ok"],
            hidden: 0,
        }),
    ];
    s.turn_timer = Some("1m 12s");
    s.queue = vec![
        QueueRow { kind: "steer", text: "also check that the macOS job is green", newest: false },
        QueueRow { kind: "after turn", text: "then open a PR", newest: true },
    ];
    s.composer = Composer {
        before: "run the attach tests again",
        after: "",
        ghost: " on both platforms and push",
        placeholder: "",
        image: Some(("screenshot.png", "212 KB")),
        slash: Some(SlashMenu {
            entries: vec![
                ("/model", "switch model"),
                ("/mcp", "MCP servers"),
                ("/memory", "memory history"),
                ("/merge", "merge the branch"),
            ],
            selected: 0,
        }),
    };
    s
}

fn design_session() -> Session {
    let mut s = session(sid::DESIGN, "Desktop app design", "local", "gui-design", AgentState::Running);
    s.blocks = vec![
        user("let's work through the design"),
        tool(ToolKind::Read, "Read", "docs/remote-contracts.md", "210 lines"),
        tool(ToolKind::Grep, "Grep", "`Busy` in crates/", "4 matches"),
        Block::Prose(vec![Md::Para(vec![
            p("The agent serves one client today. I'll write the observer model into "),
            c("docs/design/gui.md"),
            p(", wrapped to the tile's width."),
        ])]),
        tool(ToolKind::Edit, "Edit", "docs/design/gui.md", "+180"),
        Block::Diff(Diff {
            file: "docs/design/gui.md",
            plus: 4,
            minus: 0,
            lines: vec![
                line(1, DiffKind::Add, "# Desktop app: design"),
                line(2, DiffKind::Add, ""),
                line(3, DiffKind::Add, "Status: design only, decided 2026-11-27."),
                line(4, DiffKind::Add, "A native desktop app for mu, with no web engine or runtime."),
            ],
            more: 176,
        }),
        Block::Thinking { time: "14s", live: Some("Which of the open questions blocks the wire bump?") },
    ];
    s.turn_timer = Some("2m 03s");
    s.cost = "$3.10";
    s
}

fn flaky_session() -> Session {
    let mut s = session(sid::FLAKY, "Fix flaky attach test", "devbox", "fix/attach", AgentState::NeedsYou);
    s.mode = "ask";
    s.model = "sonnet-5";
    s.cost = "$1.20";
    s.ctx_pct = 22;
    s.blocks = vec![
        user("the attach test is flaky on CI, find out why"),
        tool(ToolKind::Grep, "Grep", "`attach_busy` in tests/", "3 matches"),
        Block::Bash(Bash {
            command: "cargo test -p mu --test agent -- --test-threads=1 attach_busy",
            exit: Some(1),
            time: "9s",
            tail: vec!["thread 'attach_busy' panicked at tests/agent.rs:88:", "expected Busy, got Ok(Hello)"],
            hidden: 0,
        }),
    ];
    s.request = Some(Request::Approval(Approval {
        title: "bash: cargo test -p mu --test agent -- --repeat 50",
        from: None,
        why: "not covered by your rules (mode: ask)",
        body: "cargo test -p mu --test agent -- --repeat 50",
        step: 1,
        choices: vec![
            Choice { key: "1", label: "Yes, just this once", detail: "" },
            Choice { key: "2", label: "For cargo test", detail: "cargo test *" },
            Choice { key: "3", label: "No", detail: "" },
        ],
        selected: 0,
        hint: "y yes · n no · 1–3 pick · e type a pattern · Enter choose · Tab/→ note · Esc cancel",
    }));
    s
}

fn deploy_session() -> Session {
    let mut s = session(sid::DEPLOY, "Deploy pod adapter", "test-cluster", "pod-adapter", AgentState::NeedsYou);
    s.mode = "ask";
    s.cost = "$0.41";
    s.ctx_pct = 18;
    s.blocks = vec![
        user("deploy the pod adapter to the test cluster"),
        tool(ToolKind::Read, "Read", "plugins/remote/adapters/pod.sh", "88 lines"),
        Block::Bash(Bash {
            command: "kubectl rollout status deploy/mu-adapter",
            exit: Some(0),
            time: "7s",
            tail: vec!["deployment \"mu-adapter\" successfully rolled out"],
            hidden: 0,
        }),
    ];
    s.request = Some(Request::Approval(Approval {
        title: "bash: kubectl delete pod mu-adapter-0",
        from: None,
        why: "not covered by your rules (mode: ask)",
        body: "kubectl delete pod mu-adapter-0 --namespace mu",
        step: 1,
        choices: vec![
            Choice { key: "1", label: "Yes, just this once", detail: "" },
            Choice { key: "2", label: "For kubectl delete", detail: "kubectl delete *" },
            Choice { key: "3", label: "No", detail: "" },
        ],
        selected: 0,
        hint: "y yes · n no · 1–3 pick · e type a pattern · Enter choose · Tab/→ note · Esc cancel",
    }));
    s
}

fn agent(
    state: AgentState,
    title: &'static str,
    kind: &'static str,
    host: &'static str,
    branch: &'static str,
    attached: &'static str,
    ran: &'static str,
    cost: &'static str,
) -> AgentRow {
    AgentRow { state, title, kind, host, branch, attached, ran, cost, current: false, subs: vec![] }
}

fn entry(kind: EntryKind, label: &'static str, detail: &'static str, key: &'static str) -> PaletteEntry {
    PaletteEntry { kind, label, detail, key }
}

fn review_data() -> ReviewData {
    use DiffKind::*;
    ReviewData {
        scope: "Branch since main",
        plus: 245,
        minus: 13,
        files: vec![
            ReviewFile {
                path: "docs/design/gui.md",
                plus: 180,
                minus: 0,
                expanded: true,
                lines: vec![
                    line(1, Add, "# Desktop app: design"),
                    line(2, Add, ""),
                    line(3, Add, "Status: design only, decided 2026-11-27."),
                    line(4, Add, "A native desktop app for mu, with no web engine."),
                    line(5, Add, "## Scope and relation to the TUI"),
                ],
            },
            ReviewFile {
                path: "docs/README.md",
                plus: 1,
                minus: 1,
                expanded: false,
                lines: vec![
                    line(12, Del, "- [Remote execution](remote.md)"),
                    line(12, Add, "- [Remote execution](remote.md), [Desktop app](design/gui.md)"),
                ],
            },
            ReviewFile {
                path: "docs/design/gui-wireframes.md",
                plus: 64,
                minus: 12,
                expanded: false,
                lines: vec![],
            },
        ],
    }
}

pub fn build() -> Data {
    let mut sessions = vec![
        design_session(),
        flaky_session(),
        {
            let mut s = session(sid::REST, "Attach reconnect", "local", "fix/attach-reconnect", AgentState::Idle);
            s.blocks = rest_blocks();
            s.cost = "$0.84";
            s
        },
        {
            let mut s = session(sid::STREAM, "Why Busy?", "local", "fix/attach-reconnect", AgentState::Running);
            s.blocks = stream_blocks();
            s.new_output = true;
            s.turn_timer = Some("38s");
            s
        },
        approval_session(1, sid::APPROVAL),
        approval_session(2, sid::APPROVAL_SCOPE),
        question_session(false, sid::QUESTIONS),
        question_session(true, sid::QUESTIONS_REVIEW),
        plan_session(),
        composer_session(),
        deploy_session(),
    ];
    for (n, s) in sessions.iter().enumerate() {
        debug_assert_eq!(n, s.id);
    }
    // A request is answered somewhere else: the dock closes and leaves one line.
    sessions[sid::REST].blocks.push(Block::Answered("✓ Ran once · answered on devbox"));

    let mut design_row =
        agent(AgentState::Running, "Desktop app design", "tui", "here", "gui-design", "—", "2h", "$3.10");
    design_row.current = true;
    design_row.subs = vec![agent(AgentState::Running, "ReviewDiff", "subagent", "here", "gui-design", "—", "1m", "$0.22")];

    let needs_you = vec![
        agent(AgentState::NeedsYou, "Deploy pod adapter", "agent", "test-cluster", "pod-adapter", "—", "4m", "$0.41"),
        agent(AgentState::NeedsYou, "Fix flaky attach test", "agent", "devbox", "fix/attach", "laptop", "12m", "$1.20"),
    ];
    let groups = vec![
        AgentGroup {
            name: "~/code/private/mu",
            rows: vec![
                design_row,
                agent(AgentState::Idle, "Bump omp to 0.46", "agent", "here", "maint/omp", "—", "3h", "$0.20"),
                agent(AgentState::Finished, "Release notes", "print", "here", "main", "—", "1d", "$0.08"),
            ],
            recent: vec![
                RecentRow { title: "Attach reconnect fix", age: "2d", cost: "$0.84" },
                RecentRow { title: "Pod adapter test", age: "4d", cost: "$0.51" },
            ],
        },
        AgentGroup {
            name: "~/code/private/brewery",
            rows: vec![
                agent(AgentState::Idle, "Formula audit", "agent", "build-pod-3", "main", "—", "2d", "$0.30"),
                agent(AgentState::Failed, "Bottle upload", "agent", "build-pod-3", "main", "—", "3d", "$0.12"),
            ],
            recent: vec![],
        },
    ];

    let jobs = vec![
        JobRow {
            state: JobState::Running,
            name: "ReviewDiff",
            agent: "scout",
            tools: 14,
            elapsed: "1m 12s",
            cost: "$0.22",
            last: "Read src/run/serve.rs",
            task: "Review the diff of the branch against main and report only real problems.",
            output: vec!["Reading the changed files", "Checking take_over() against the contract", "Looking at the new test"],
        },
        JobRow {
            state: JobState::Running,
            name: "cargo test --workspace",
            agent: "bash",
            tools: 0,
            elapsed: "3m 40s",
            cost: "—",
            last: "test result: ok. 211 passed",
            task: "cargo test --workspace",
            output: vec!["     Running unittests src/lib.rs", "test result: ok. 211 passed; 0 failed"],
        },
        JobRow {
            state: JobState::Done,
            name: "CheckMacOS",
            agent: "task",
            tools: 31,
            elapsed: "3m 40s",
            cost: "$0.38",
            last: "Builds and passes on the Mac",
            task: "Build and test the branch on the Mac and report the result.",
            output: vec!["Built mu in 2m 1s", "Attach tests: 14 passed"],
        },
        JobRow {
            state: JobState::Failed,
            name: "AuditLicenses",
            agent: "scout",
            tools: 6,
            elapsed: "52s",
            cost: "$0.04",
            last: "Error: provider overloaded",
            task: "List dependencies whose licence changed since the last release.",
            output: vec!["Reading Cargo.lock", "provider: 529 overloaded_error"],
        },
        JobRow {
            state: JobState::Cancelled,
            name: "sleep 600",
            agent: "bash",
            tools: 0,
            elapsed: "2m 8s",
            cost: "—",
            last: "cancelled",
            task: "sleep 600",
            output: vec![],
        },
    ];

    let palette = PaletteData {
        query: "tile spl",
        entries: vec![
            entry(EntryKind::Action, "Tile: split right", "", plat("⌘⇧\\", "Ctrl+X v")),
            entry(EntryKind::Action, "Tile: split down", "", plat("⌘⇧-", "Ctrl+X s")),
            entry(EntryKind::Action, "Tile: split (spiral)", "", plat("⌘\\", "Ctrl+X Enter")),
            entry(EntryKind::Command, "/compact", "compact the conversation", ""),
            entry(EntryKind::Agent, "Agent: Deploy pod adapter", "test-cluster · ⚑ needs you", ""),
            entry(EntryKind::Layout, "Layout: review", "3 tiles", ""),
            entry(EntryKind::View, "View: jobs", "open in the focused tile", "Ctrl+X o"),
        ],
        agents: vec![
            entry(EntryKind::Agent, "Desktop app design", "here · running · 2m", ""),
            entry(EntryKind::Agent, "Deploy pod adapter", "test-cluster · ⚑ needs you", ""),
            entry(EntryKind::Agent, "Fix flaky attach test", "devbox · ⚑ needs you", ""),
            entry(EntryKind::Agent, "Bump omp to 0.46", "here · idle · 3h", ""),
            entry(EntryKind::Agent, "Release notes", "here · ✓ finished · 1d", ""),
            entry(EntryKind::Agent, "Formula audit", "build-pod-3 · idle · 2d", ""),
            entry(EntryKind::Agent, "Attach reconnect fix", "recent · ended 2d ago", ""),
        ],
    };

    Data {
        sessions,
        needs_you,
        groups,
        jobs,
        job_selected: 0,
        review: review_data(),
        palette,
        status: StatusData {
            profile: "μ private",
            running: 3,
            need_you: 2,
            today: "$4.79",
            remotes: vec![("test-cluster", true), ("devbox", true)],
        },
    }
}
