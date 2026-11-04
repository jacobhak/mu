use super::bell::WokenBell;
use super::blocks;
use super::clipboard;
use super::complete::Entry;
use super::composer::{Action, Composer};
use super::input::{self, Input};
use super::jobs_panel::JobsPanel;
use super::line::truncate_text as trunc;
use super::line::{Line, dim, line_text, span, styled};
use super::markdown;
use super::overlay::{LoginTarget, Overlay, OverlayEvent};
use super::plan_review::{PlanReview, Tier};
use super::ptyview::PtyView;
use super::render::Renderer;
use super::reply::ReplyPicker;
use super::select::{Item, Select};
use super::status;
use super::term::{self, Term};
use super::theme::{self, Role};
use super::transcript::{Stream, Transcript};
use crate::context::Context;
use crate::events::{AgentEvent, Answer, AskQuestion, StatusSnapshot, SubagentState, SubagentStatus};
use crate::events::{Command, SharedInbox};
use crate::image;
use crate::message::{Image, UserInput};
use crate::models::{GatewayModel, Thinking};
use crate::plugin::PluginHost;
use crate::provider::Providers;
use crate::provider::native;
use crate::tools::todo::{Phase, Status};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use mu_common::paths;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::sync::oneshot;

mod keys_ui;
mod mcp_ui;

const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
const FRAME: Duration = Duration::from_millis(33);
const BUILTIN_DESC: [(&str, &str, bool); 25] = [
    ("exit", "Exit mu", false),
    ("quit", "Exit mu", false),
    ("new", "Start a new session", false),
    ("resume", "Resume a session from this directory", false),
    ("model", "Pick a model and assign a role", false),
    ("switch", "Switch this session's model", false),
    ("login", "Add an API key, or log in to a provider or MCP server", false),
    ("logout", "Remove a stored API key", false),
    ("thinking", "Set the thinking level", false),
    ("compact", "Compact the context [focus]", false),
    ("plan", "Toggle plan mode", false),
    ("review", "Review changes against the approved plan [focus]", false),
    ("session", "Show the session file and title", false),
    ("rename", "Rename this session", true),
    ("todo", "Show the todo list", false),
    ("skills", "List skills", false),
    ("plugins", "List plugins", false),
    ("mcp", "Manage MCP servers (picker; or add, import, remove, enable, test…)", true),
    ("hotkeys", "Show key bindings", false),
    ("copy", "Copy a reply, code block or command", false),
    ("reply", "Quote parts of the last reply into the composer", false),
    ("image", "Attach an image file", true),
    ("jobs", "Show background jobs and other mu sessions", false),
    ("plans", "Approve, refine or drop a parked plan", false),
    ("deliver", "Implement, review and commit a parked plan in a worktree [id]", false),
];

/// `/mcp` subcommands for the completion menu, with what each does. The names that need a server
/// open the server picker when typed bare.
const MCP_SUBCOMMANDS: [(&str, &str); 10] = [
    ("add", "Add a server: discovered, popular or custom"),
    ("import", "Copy servers other tools have configured"),
    ("list", "Show servers and their state"),
    ("enable", "Turn a server on"),
    ("disable", "Turn a server off"),
    ("remove", "Delete a server from the config"),
    ("test", "Reconnect a server and list its tools"),
    ("reconnect", "Restart a server and refresh its tools"),
    ("reload", "Re-read the config files"),
    ("help", "Show the syntax"),
];

pub struct AppInit {
    pub cmd_tx: UnboundedSender<Command>,
    pub ev_rx: UnboundedReceiver<AgentEvent>,
    pub inbox: SharedInbox,
    pub providers: Rc<Providers>,
    pub ctx: Rc<Context>,
    pub plugins: Rc<PluginHost>,
    pub notice_rx: UnboundedReceiver<String>,
    pub cwd: PathBuf,
    pub pick_session: bool,
    pub status_cfg: crate::config::OmpStatusLine,
    pub theme: crate::config::MuTheme,
    /// Ring the terminal at the end of a turn nobody typed (omp's `completion.notify`).
    pub notify: bool,
}

/// What to do when a child that owned the terminal exits.
enum ChildThen {
    Login,
    /// `omp -p '/mcp reauth <name>'` for this MCP server.
    McpLogin(String),
    /// The external editor was run on the composer text kept in this file.
    ComposerEdit(std::path::PathBuf),
    /// The editor was run on the plan file shown in the review overlay.
    PlanEdit(std::path::PathBuf),
    /// `mu take` ran on the terminal for a session picked in the jobs panel.
    Take,
}

enum UiMsg {
    ChildDone {
        then: ChildThen,
        status: Result<std::process::ExitStatus, String>,
    },
    Sessions(Vec<crate::session::SessionInfo>),
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
    /// An API key was checked: the outcome, and the entry to save when it holds.
    KeyChecked {
        flow: u64,
        def: &'static native::ProviderDef,
        entry: crate::auth_store::Entry,
        r: Result<native::Check, String>,
    },
}

struct RunningTool {
    id: String,
    name: String,
    args: Value,
    started: Instant,
    tail: VecDeque<String>,
    partial: String,
}
struct StreamingCall {
    id: String,
    name: String,
    json: super::partial_json::Partial,
    args: Value,
    /// New bytes arrived since `args` was last computed.
    stale: bool,
}

struct App {
    /// The streamed conversation: committed lines, open text and thinking, smooth reveal.
    tr: Transcript,
    cmd_tx: UnboundedSender<Command>,
    inbox: SharedInbox,
    providers: Rc<Providers>,
    ctx: Rc<Context>,
    plugins: Rc<PluginHost>,
    ui_tx: UnboundedSender<UiMsg>,
    cwd: PathBuf,
    status_cfg: crate::config::OmpStatusLine,
    bell: WokenBell,
    notify: bool,
    turn_started: Option<Instant>,
    /// `/model` assigns roles; `/switch` only changes this session.
    pick_assign: bool,
    pick_deliver: bool,
    renderer: Renderer,
    composer: Composer,
    commands: Vec<Entry>,
    tools: Vec<RunningTool>,
    /// Latest background-job snapshot, and finished jobs already printed to the transcript.
    jobs: Vec<SubagentStatus>,
    jobs_committed: HashSet<String>,
    /// Newest output of each background bash job, for the jobs panel.
    job_out: HashMap<String, String>,
    ghost_seq: u64,
    ghost_asked: String,
    /// Bash commands the user ran, for `/copy`.
    bash_cmds: Vec<String>,
    compacting: bool,
    todo: Vec<Phase>,
    status: StatusSnapshot,
    overlay: Option<Overlay>,
    /// Embedded terminal of a running `bash` call with `pty: true`.
    pty: Option<PtyView>,
    running: bool,
    quitting: bool,
    /// Tool calls whose arguments are still streaming or that are not yet executing.
    calls: Vec<StreamingCall>,
    expanded: bool,
    dirty: bool,
    /// A child process (editor, `omp login`) owns the terminal: no drawing and no input reading
    /// until it exits, but agent and plugin tasks keep running.
    child_busy: bool,
    last_draw: Instant,
    started: Instant,
    /// An `/mcp add` wizard is collecting answers; the composer's lines go to it.
    mcp_wizard: Option<super::mcp_wizard::Wizard>,
    /// An API-key flow in progress; while it types, the composer takes its text.
    key_flow: Option<keys_ui::KeyFlow>,
    key_flow_seq: u64,
    /// Queued inputs that could not go back into the composer while a key was typed.
    held_back: Vec<UserInput>,
}

fn push_capped<T>(v: &mut Vec<T>, item: T) {
    if v.len() == 50 {
        v.remove(0);
    }
    v.push(item);
}

fn ago(t: std::time::SystemTime) -> String {
    let s = t.elapsed().map_or(0, |d| d.as_secs());
    match s {
        0..=59 => "just now".into(),
        60..=3599 => format!("{}m ago", s / 60),
        3600..=86399 => format!("{}h ago", s / 3600),
        _ => format!("{}d ago", s / 86400),
    }
}

pub async fn run(init: AppInit) -> i32 {
    let _term = match Term::enter() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("mu: cannot initialise terminal: {e}");
            return 1;
        }
    };
    term::install_panic_hook();
    let AppInit {
        cmd_tx,
        mut ev_rx,
        inbox,
        providers,
        ctx,
        plugins,
        mut notice_rx,
        cwd,
        pick_session,
        status_cfg,
        theme,
        notify,
    } = init;
    let theme_warnings = theme::init(&theme);
    let (ui_tx, mut ui_rx) = unbounded_channel();
    let mut commands: Vec<Entry> = BUILTIN_DESC
        .iter()
        .map(|&(name, desc, needs_arg)| {
            let (args, arg_descs): (Vec<String>, Vec<String>) = match name {
                "thinking" => (Thinking::NAMES.iter().map(|s| s.to_string()).collect(), vec![]),
                "copy" => (["code", "cmd", "link"].map(String::from).to_vec(), vec![]),
                "jobs" => (vec!["kill".to_string()], vec![]),
                "mcp" => MCP_SUBCOMMANDS.iter().map(|&(a, d)| (a.to_string(), d.to_string())).unzip(),
                "login" => (
                    native::PROVIDERS
                        .iter()
                        .map(|d| d.id.to_string())
                        .chain(plugins.mcp_names().iter().map(|n| format!("mcp:{n}")))
                        .collect(),
                    vec![],
                ),
                "logout" => (native::PROVIDERS.iter().map(|d| d.id.to_string()).collect(), vec![]),
                _ => (vec![], vec![]),
            };
            Entry { name: name.into(), desc: desc.into(), needs_arg, args, arg_descs }
        })
        .collect();
    commands.extend(ctx.commands.iter().map(|c| Entry {
        name: c.name.clone(),
        desc: c.description.clone(),
        needs_arg: false,
        args: vec![],
        arg_descs: vec![],
    }));
    commands.extend(ctx.skills.iter().map(|s| Entry {
        name: format!("skill:{}", s.name),
        desc: s.description.clone(),
        needs_arg: false,
        args: vec![],
        arg_descs: vec![],
    }));
    let show_thinking = !providers.cfg.omp.hide_thinking_block;
    let smooth = providers.cfg.omp.display.smooth_streaming;
    let mut app = App {
        tr: Transcript::new(show_thinking, smooth),
        cmd_tx,
        inbox,
        providers,
        ctx,
        plugins,
        ui_tx,
        status_cfg,
        bell: WokenBell::default(),
        notify,
        turn_started: None,
        pick_assign: false,
        pick_deliver: false,
        cwd: cwd.clone(),
        renderer: Renderer::new(),
        composer: Composer::new(Some(paths::mu_home().join("history.jsonl"))).with_cwd(&cwd),
        commands,
        ghost_seq: 0,
        ghost_asked: String::new(),
        tools: Vec::new(),
        jobs: Vec::new(),
        jobs_committed: HashSet::new(),
        job_out: HashMap::new(),
        bash_cmds: Vec::new(),
        compacting: false,
        todo: Vec::new(),
        status: StatusSnapshot::default(),
        overlay: None,
        pty: None,
        running: false,
        quitting: false,
        calls: Vec::new(),
        expanded: false,
        dirty: true,
        child_busy: false,
        last_draw: Instant::now() - FRAME,
        started: Instant::now(),
        mcp_wizard: None,
        key_flow: None,
        key_flow_seq: 0,
        held_back: Vec::new(),
    };
    let store_warning = app.providers.store().permission_warning();
    let store_check = app.providers.store().check().err().map(|e| e.to_string());
    for w in
        theme_warnings.iter().chain(app.plugins.warnings.iter()).chain(store_warning.iter()).chain(store_check.iter())
    {
        app.tr.pending.push(styled(w.clone(), |_| theme::get().fg(Role::Warn)));
    }
    if pick_session {
        app.open_session_picker();
    }
    let mut reader = match input::Reader::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("mu: cannot read the terminal: {e}");
            return 1;
        }
    };
    let mut tick = tokio::time::interval(Duration::from_millis(80));
    let mut signals = term::Shutdown::new().ok();
    let mut draw_failed = false;
    loop {
        if let Some(p) = term::take_panic() {
            app.error(&p);
        }
        if app.dirty
            && !app.child_busy
            && Instant::now() >= app.last_draw + FRAME
            && let Err(e) = app.draw()
            && !draw_failed
        {
            // Said once: stderr is the terminal, and repeating it would flood the screen.
            draw_failed = true;
            eprintln!("mu: draw failed: {e}");
        }
        let due = app.last_draw + FRAME;
        tokio::select! {
            inp = reader.next(), if !app.child_busy => match inp {
                Some(Input::Term(e)) => app.on_terminal(e),
                Some(Input::PasteText(s)) => app.on_terminal(Event::Paste(s)),
                Some(Input::PasteImage(img)) => app.on_ui(UiMsg::ClipboardImage(Ok(Some(img)))),
                Some(Input::Status(s)) => app.notice(&s),
                Some(Input::Reply(seq)) => term::write_terminal_seq(&seq),
                None => break,
            },
            e = ev_rx.recv() => match e {
                Some(AgentEvent::Exit) | None => break,
                Some(e) => app.on_agent(e),
            },
            Some(()) = async { match signals.as_mut() { Some(s) => { s.next().await; Some(()) } None => std::future::pending().await } } => {
                // A second signal while shutting down means the agent is stuck: leave now.
                if app.quitting { break }
                app.quit();
            }
            Some(n) = notice_rx.recv() => app.notice(&n),
            Some(m) = ui_rx.recv() => app.on_ui(m),
            _ = tick.tick(), if app.busy() => app.dirty = true,
            _ = tokio::time::sleep_until(due.into()), if app.dirty => {}
        }
        app.sync_key_flow();
    }
    // Lines committed in the last moments (a final reply, the exit notice) are drawn before the
    // terminal is handed back.
    let _ = app.draw();
    let _ = app.renderer.finish();
    if let Some(path) = app.status.session_path.as_deref().filter(|p| std::path::Path::new(p).exists()) {
        let id = std::path::Path::new(path)
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.rsplit_once('_'))
            .map_or(path, |(_, id)| id);
        let profile = crate::session::profile_flag();
        eprint!("\r\n\x1b[2mResume this session with\r\nmu {profile}--resume {id}\x1b[0m\r\n");
    }
    0
}

impl App {
    /// Whether the screen needs redrawing on a timer: something is running, or the jobs panel is open
    /// (it follows sessions and clocks that change without any event).
    fn busy(&self) -> bool {
        self.running
            || self.compacting
            || self.tr.thinking_active()
            || !self.tools.is_empty()
            || !self.calls.is_empty()
            || self.jobs.iter().any(|j| j.state == SubagentState::Running)
            || matches!(self.overlay, Some(Overlay::Jobs { .. }))
    }

    fn width(&self) -> usize {
        self.renderer.size().0
    }

    fn commit(&mut self, lines: Vec<Line>) {
        self.tr.commit(lines);
        self.dirty = true;
    }

    fn notice(&mut self, s: &str) {
        let l = s.lines().map(|l| styled(l.to_string(), |_| dim())).collect();
        self.commit(l);
    }

    fn error(&mut self, s: &str) {
        let l = s.lines().map(|l| styled(l.to_string(), |_| theme::get().fg(Role::Err))).collect();
        self.commit(l);
    }

    fn send(&mut self, c: Command) {
        let _ = self.cmd_tx.send(c);
    }

    // ---- agent events -------------------------------------------------------------------------

    /// Streamed text and thinking go to the transcript, which commits and paces them.
    fn finish_thinking(&mut self) {
        let w = self.width();
        self.tr.finish_thinking(w);
    }

    fn on_agent(&mut self, e: AgentEvent) {
        self.dirty = true;
        if !matches!(
            e,
            AgentEvent::Text(_)
                | AgentEvent::Thinking(_)
                | AgentEvent::ToolCallDelta { .. }
                | AgentEvent::ToolOutput { .. }
                | AgentEvent::Status(_)
                | AgentEvent::Jobs(_)
                | AgentEvent::Todo(_)
                | AgentEvent::TurnStart
                | AgentEvent::Retry { .. }
        ) {
            let w = self.width();
            self.tr.flush_reveal(w);
        }
        match e {
            AgentEvent::TurnStart => {
                self.running = true;
                self.turn_started = Some(Instant::now());
            }
            AgentEvent::Woken => self.bell.on_woken(),
            AgentEvent::UserPrompt(t) => {
                let w = self.width();
                let mut l = vec![vec![]];
                l.extend(blocks::user_prompt(&t, w));
                l.push(vec![]);
                self.commit(l);
            }
            AgentEvent::Text(s) => {
                let w = self.width();
                self.tr.queue_reveal(Stream::Text, s, w);
            }
            AgentEvent::Thinking(s) => {
                let w = self.width();
                self.tr.queue_reveal(Stream::Thinking, s, w);
            }
            AgentEvent::ToolCallStart { id, name } => {
                self.finish_thinking();
                self.calls.push(StreamingCall { id, name, json: Default::default(), args: json!({}), stale: false });
            }
            AgentEvent::ToolCallDelta { id, json } => {
                if let Some(c) = self.calls.iter_mut().find(|c| c.id == id) {
                    {
                        c.json.push(&json);
                        c.stale = true;
                    }
                }
            }
            AgentEvent::BlockEnd => {
                let w = self.width();
                self.tr.end_block(w);
            }
            AgentEvent::ToolStart { id, name, args } => {
                self.calls.retain(|c| c.id != id);
                self.finish_thinking();
                if name == "bash"
                    && let Some(c) = args["command"].as_str()
                {
                    push_capped(&mut self.bash_cmds, c.to_string());
                }
                self.tools.push(RunningTool {
                    id,
                    name,
                    args,
                    started: Instant::now(),
                    tail: VecDeque::new(),
                    partial: String::new(),
                });
            }
            AgentEvent::PtyOpen { id, input, reply } => {
                // A pty is already open: dropping `reply` makes the tool fall back.
                if self.pty.is_none() {
                    let (w, h) = self.renderer.size();
                    let size = PtyView::size_for(w, h);
                    self.pty = Some(PtyView::new(id, input, size));
                    let _ = reply.send(size);
                }
            }
            AgentEvent::ToolOutput { id, chunk } => {
                if let Some(p) = self.pty.as_mut().filter(|p| p.id == id) {
                    p.feed(&chunk);
                } else if let Some(t) = self.tools.iter_mut().find(|t| t.id == id) {
                    t.partial.push_str(&chunk);
                    while let Some(i) = t.partial.find('\n') {
                        let line: String = t.partial.drain(..=i).collect();
                        t.tail.push_back(line.trim_end_matches(['\n', '\r']).to_string());
                        if t.tail.len() > 10 {
                            t.tail.pop_front();
                        }
                    }
                }
            }
            AgentEvent::ToolEnd { id, name, output } => {
                if self.pty.as_ref().is_some_and(|p| p.id == id) {
                    self.pty = None;
                }
                let args = self
                    .tools
                    .iter()
                    .position(|t| t.id == id)
                    .map(|i| self.tools.remove(i).args)
                    .unwrap_or(Value::Null);
                let w = self.width();
                let mut l = blocks::tool_result(&name, &args, &output, self.expanded, w);
                // Breathing room: a blank row after every tool block, as text blocks have.
                l.push(vec![]);
                self.commit(l);
            }
            AgentEvent::Retry { attempt, delay, reason } => {
                self.tr.reset_stream();
                self.calls.clear();
                self.notice(&format!("↻ retrying in {}s ({reason}) [attempt {attempt}/5]", delay.as_secs()));
            }
            AgentEvent::Notice(s) => self.notice(&s),
            AgentEvent::Error(s) => self.error(&s),
            AgentEvent::Compacting => self.compacting = true,
            AgentEvent::Compacted { before, after } => {
                self.compacting = false;
                self.notice(&format!("── compacted {before}→{after} tokens"));
            }
            AgentEvent::Jobs(v) => {
                if v.is_empty() {
                    self.jobs_committed.clear();
                }
                for r in v.iter().filter(|r| r.state != SubagentState::Running) {
                    if self.jobs_committed.insert(r.name.clone()) {
                        self.commit(blocks::subagent_rows(std::slice::from_ref(r), ' '));
                    }
                }
                self.job_out.retain(|id, _| v.iter().any(|j| &j.name == id));
                if let Some(Overlay::Jobs { st }) = &mut self.overlay {
                    st.set_jobs(v.clone());
                }
                self.jobs = v;
            }
            AgentEvent::JobOutput { id, tail } => {
                if let Some(Overlay::Jobs { st }) = &mut self.overlay {
                    st.set_output(&id, &tail);
                }
                self.job_out.insert(id, tail);
            }
            AgentEvent::JobsDelivered(ids) => {
                self.commit(vec![styled(format!("↳ job results delivered: {}", ids.join(", ")), |_| dim())]);
            }
            AgentEvent::Ask { questions, reply } => {
                let ring = self.bell.on_attention();
                self.ring(ring);
                self.start_ask(questions, reply)
            }
            AgentEvent::PlanReview { title, uri, file, content, stored, note, reply } => {
                let ring = self.bell.on_attention();
                self.ring(ring);
                let context = self
                    .status
                    .context_tokens
                    .filter(|_| self.status.context_window > 0)
                    .map(|t| (t, self.status.context_window as u64));
                let review = PlanReview::new(title, uri, file, content, context, self.plan_tiers(), stored, note);
                self.overlay = Some(Overlay::Review { review: Box::new(review), reply: Some(reply), stored });
            }
            AgentEvent::PlanParked { id, path } => self.notice(&format!("plan parked: {id} · {}", path.display())),
            AgentEvent::PlanList(list) => self.show_plans(list),
            AgentEvent::Status(s) => {
                self.status = s;
            }
            AgentEvent::Todo(v) => self.todo = serde_json::from_value(v).unwrap_or_default(),
            AgentEvent::TurnEnd { aborted } => {
                self.pty = None;
                self.calls.clear();
                let w = self.width();
                self.tr.end_turn(w);
                self.tools.clear();
                self.compacting = false;
                self.running = false;
                self.turn_started = None;
                let ring = self.bell.on_turn_end(aborted);
                self.ring(ring);
                if aborted {
                    self.notice("[aborted]");
                }
                // Whatever is still queued when a turn ends (it was aborted or failed) was written
                // for that turn; it comes back as editable text instead of leaking into the next one.
                let queued: Vec<UserInput> = {
                    let mut ib = self.inbox.borrow_mut();
                    let mut q: Vec<UserInput> = ib.steer.drain(..).collect();
                    q.extend(ib.follow_up.drain(..));
                    q
                };
                if !queued.is_empty() {
                    if !aborted {
                        self.notice("queued messages were not sent; they are back in the composer");
                    }
                    self.restore_queued(queued);
                }
            }
            AgentEvent::Exit | AgentEvent::TurnFailed | AgentEvent::ToolSkipped { .. } => {}
        }
    }

    /// Rings the terminal (BEL, plus a toast where supported) when `ring` and the setting is on.
    fn ring(&self, ring: bool) {
        if ring && self.notify {
            let name =
                self.status.title.clone().unwrap_or_else(|| {
                    self.cwd.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
                });
            term::notify(&format!("mu: {name}"));
        }
    }

    fn on_ui(&mut self, m: UiMsg) {
        self.dirty = true;
        match m {
            UiMsg::ChildDone { then, status } => self.on_child_done(then, status),
            UiMsg::Sessions(list) => self.show_sessions(list),
            UiMsg::Copied(r) => match r {
                Ok(n) => self.notice(&n),
                Err(e) => self.error(&format!("copy failed: {e}")),
            },
            UiMsg::Models(r) => {
                let gw = match r {
                    Ok(g) => g,
                    Err(e) => {
                        // Native models are listed either way; only a session without any says why.
                        if self.providers.native_models().is_empty() {
                            self.error(&format!("could not fetch gateway models: {e}"));
                        } else {
                            self.notice(&format!("gateway models unavailable: {e}"));
                        }
                        vec![]
                    }
                };
                self.open_model_picker(gw);
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
            UiMsg::KeyChecked { flow, def, entry, r } => self.on_key_checked(flow, def, entry, r),
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
                Ok(Some(img)) if self.overlay.is_none() => self.composer.attach_image(img),
                Ok(Some(_)) => self.notice("image paste ignored in picker"),
                Ok(None) => self.notice("clipboard has no image"),
                Err(e) => self.notice(&e),
            },
            UiMsg::Ghost { seq, text, ghost } => {
                if seq == self.ghost_seq && self.overlay.is_none() {
                    self.composer.set_ghost(&text, ghost);
                }
            }
        }
    }

    // ---- ask / plan overlays ------------------------------------------------------------------

    fn start_ask(&mut self, qs: Vec<AskQuestion>, reply: oneshot::Sender<Option<Vec<Answer>>>) {
        if qs.is_empty() {
            let _ = reply.send(Some(vec![]));
            return;
        }
        self.overlay = Some(Overlay::new_ask(qs, reply));
    }

    // ---- pickers ------------------------------------------------------------------------------

    fn request_models(&mut self) {
        self.notice("fetching models…");
        let providers = self.providers.clone();
        let tx = self.ui_tx.clone();
        tokio::task::spawn_local(async move {
            let r = providers.gateway_models().await.map_err(|e| e.to_string());
            let _ = tx.send(UiMsg::Models(r));
        });
    }

    fn open_model_picker(&mut self, gateway: Vec<GatewayModel>) {
        let mut items = Vec::new();
        let mut ids = Vec::new();
        for r in ["default", "smol", "slow", "plan", "task", "commit"] {
            if let Some(v) = self.providers.cfg.role(r) {
                items.push(Item { label: format!("@{r}"), detail: v });
                ids.push(format!("@{r}"));
            }
        }
        for m in self.providers.native_models() {
            items.push(Item { label: m.id.clone(), detail: "key".into() });
            ids.push(m.id);
        }
        let known = ids.clone();
        for g in gateway.iter().filter(|g| !known.contains(&g.id)) {
            // A gateway id of a native provider with a key is routed natively: it shows the key marker.
            let native = g.id.split_once('/').is_some_and(|(p, _)| self.providers.is_native(p));
            let detail = if native {
                "key".into()
            } else {
                g.context_length.map_or(String::new(), |c| format!("{}k ctx", c / 1000))
            };
            items.push(Item { label: g.id.clone(), detail });
            ids.push(g.id.clone());
        }
        for (p, pc) in &self.providers.cfg.models.providers {
            for m in &pc.models {
                let id = format!("{p}/{}", m.id);
                if !ids.contains(&id) {
                    items.push(Item { label: id.clone(), detail: "models.yml".into() });
                    ids.push(id);
                }
            }
        }
        let title = if self.pick_assign {
            "Select model (then choose where it applies)"
        } else {
            "Switch model (this session)"
        };
        self.overlay = Some(Overlay::Model { sel: Select::new(title, items), ids, assign: self.pick_assign });
    }

    fn open_role_picker(&mut self, model: String) {
        const COMMON: [&str; 6] = ["default", "smol", "slow", "plan", "task", "commit"];
        let cfg = &self.providers.cfg;
        let mut names: Vec<String> = COMMON.iter().map(|s| s.to_string()).collect();
        names.extend(cfg.omp.model_roles.keys().filter(|k| !COMMON.contains(&k.as_str())).cloned());
        let mut items = vec![Item { label: "This session only".into(), detail: model.clone() }];
        let mut roles = vec![None];
        for n in names {
            let detail = cfg.role(&n).map_or("(unset)".to_string(), |v| format!("now {v}"));
            items.push(Item {
                label: if n == "default" { "Set as @default (and switch now)".into() } else { format!("Set as @{n}") },
                detail,
            });
            roles.push(Some(n));
        }
        self.overlay = Some(Overlay::Role { sel: Select::new(&format!("Use {model}"), items), model, roles });
    }

    /// Opens the picker at once with a loading row; the (capped, sorted) listing is read on a
    /// blocking thread and fills it in when it arrives.
    fn open_session_picker(&mut self) {
        let items = vec![Item { label: "Loading…".into(), detail: String::new() }];
        self.overlay = Some(Overlay::Session { sel: Select::new("Resume session", items), paths: vec![] });
        let (cwd, tx) = (self.cwd.clone(), self.ui_tx.clone());
        tokio::task::spawn_local(async move {
            let list = tokio::task::spawn_blocking(move || crate::session::list_recent(&cwd, Some(&cwd), 200))
                .await
                .unwrap_or_default();
            let _ = tx.send(UiMsg::Sessions(list));
        });
    }

    fn show_sessions(&mut self, list: Vec<crate::session::SessionInfo>) {
        // Only fill in a picker that is still the loading one.
        if !matches!(&self.overlay, Some(Overlay::Session { paths, .. }) if paths.is_empty()) {
            return;
        }
        if list.is_empty() {
            self.overlay = None;
            self.notice("no sessions for this directory");
            return;
        }
        let items = list
            .iter()
            .map(|s| Item {
                label: s.title.clone().unwrap_or_else(|| trunc(&s.first_prompt, 80)),
                detail: ago(s.modified),
            })
            .collect();
        let paths = list.into_iter().map(|s| s.path).collect();
        self.overlay = Some(Overlay::Session { sel: Select::new("Resume session", items), paths });
    }

    fn show_plans(&mut self, list: Vec<crate::plans::PlanMeta>) {
        let deliver = std::mem::take(&mut self.pick_deliver);
        if list.is_empty() {
            self.notice("no parked plans for this repository");
            return;
        }
        let items = list
            .iter()
            .map(|m| Item {
                label: m.title.clone(),
                detail: format!("{} · {}", m.id, m.updated.get(..10).unwrap_or(&m.updated)),
            })
            .collect();
        let ids = list.into_iter().map(|m| m.id).collect();
        let title = if deliver { "Deliver plan" } else { "Parked plans" };
        self.overlay = Some(Overlay::Plans { sel: Select::new(title, items), ids, deliver });
    }

    // ---- input --------------------------------------------------------------------------------

    fn on_terminal(&mut self, e: Event) {
        self.dirty = true;
        match e {
            Event::Paste(s) if self.pty.is_some() => {
                if let Some(p) = &self.pty {
                    p.paste(&s);
                }
            }
            Event::Resize(..) if self.pty.is_some() => {
                let (w, h) = self.renderer.size();
                if let Some(p) = self.pty.as_mut() {
                    p.resize(w, h);
                }
            }
            Event::Key(k) => self.on_key(k),
            Event::Paste(s) => {
                // A key being typed takes the paste even when an overlay is open.
                let typing = self.typing();
                match self.overlay.as_mut() {
                    Some(ov) if !typing => ov.on_paste(&s),
                    _ => {
                        if let Err(e) = self.composer.on_paste(&s, &self.commands) {
                            self.error(&format!("image: {e}"));
                        }
                        self.request_ghost();
                    }
                }
            }
            _ => {}
        }
    }

    /// Asks the predict plugin for a ghost when the composer wants one. Failures are silent.
    fn request_ghost(&mut self) {
        let cfg = &self.providers.cfg.mu.predict;
        if !cfg.enabled || self.composer.has_ghost() || self.mcp_wizard.is_some() || self.key_flow_pending() {
            return;
        }
        let Some(q) = self.composer.predict_query() else { return };
        if q == self.ghost_asked {
            return;
        }
        self.ghost_seq += 1;
        self.ghost_asked = q.clone();
        let seq = self.ghost_seq;
        let assistant: String = {
            let r = self.tr.replies.last().map(String::as_str).unwrap_or_default();
            let skip = r.chars().count().saturating_sub(4000);
            r.chars().skip(skip).collect()
        };
        let params = json!({ "text": q, "cwd": self.cwd, "assistant": assistant, "threshold": cfg.threshold });
        let (plugins, tx) = (self.plugins.clone(), self.ui_tx.clone());
        tokio::task::spawn_local(async move {
            let ghost = plugins
                .request("predict", "predict/complete", params, Duration::from_millis(300))
                .await
                .ok()
                .and_then(|v| v["ghost"].as_str().map(String::from))
                .unwrap_or_default();
            let _ = tx.send(UiMsg::Ghost { seq, text: q, ghost });
        });
    }

    fn observe_prompt(&self, text: &str) {
        if !self.providers.cfg.mu.predict.enabled {
            return;
        }
        let params = json!({ "text": text, "cwd": self.cwd });
        let plugins = self.plugins.clone();
        tokio::task::spawn_local(async move {
            let _ = plugins.request("predict", "predict/observe", params, Duration::from_secs(2)).await;
        });
    }

    fn on_key_inner(&mut self, k: KeyEvent) {
        // While a key is typed, every key goes to the masked composer, even past an open overlay.
        let typing = self.typing();
        if typing {
            if self.key_flow_key(k) {
                return;
            }
        } else {
            if self.overlay.is_some() {
                return self.overlay_key(k);
            }
            if self.mcp_wizard.is_some() && self.mcp_wizard_key(k) {
                return;
            }
        }
        let action = self.composer.on_key(k, self.running, &self.commands);
        // Only actions that stay in the composer may run while a key or command is typed.
        if typing
            && !matches!(action, Action::None | Action::Redraw | Action::Abort | Action::Exit | Action::ToggleExpanded)
        {
            return;
        }
        match action {
            Action::None | Action::Redraw => {}
            Action::Reply => self.open_reply(),
            Action::Jobs => self.open_jobs(),
            Action::Submit(t) => self.submit(t, false),
            Action::QueueFollowUp(t) => self.submit(t, true),
            Action::Abort => self.inbox.borrow().cancel.cancel(),
            Action::Exit => self.quit(),
            Action::CycleThinking => {
                let cur = Thinking::parse(&self.status.thinking).unwrap_or(Thinking::Medium);
                self.send(Command::SetThinking(cur.cycle()));
            }
            Action::ToggleThinking => {
                let w = self.width();
                self.tr.toggle_thinking(w);
            }
            Action::ToggleExpanded => self.expanded = !self.expanded,
            Action::ModelPicker => self.request_models(),
            Action::EditExternal => self.edit_external(),
            Action::PasteImage => {
                let tx = self.ui_tx.clone();
                tokio::task::spawn_local(async move {
                    let _ = tx.send(UiMsg::ClipboardImage(clipboard::read_image().await));
                });
            }
            Action::PopQueue => {
                let popped = {
                    let mut ib = self.inbox.borrow_mut();
                    ib.steer.pop_back().or_else(|| ib.follow_up.pop_back())
                };
                if let Some(p) = popped {
                    self.restore_queued(vec![p]);
                }
            }
        }
    }

    fn on_key(&mut self, k: KeyEvent) {
        if let Some(p) = &self.pty {
            p.key(k);
            return;
        }
        self.on_key_inner(k);
        if self.overlay.is_none() {
            self.request_ghost();
        }
    }

    fn overlay_key(&mut self, k: KeyEvent) {
        let Some(ov) = self.overlay.take() else { return };
        let (rest, event) = ov.on_key(k);
        self.overlay = rest;
        if let Some(e) = event {
            self.on_overlay_event(e);
        }
    }

    /// Carries out what the user chose in an overlay.
    fn on_overlay_event(&mut self, e: OverlayEvent) {
        match e {
            OverlayEvent::SetModel(id) => self.send(Command::SetModel(id)),
            OverlayEvent::PickRole(model) => self.open_role_picker(model),
            OverlayEvent::AssignRole { role: Some(role), model } => match self.providers.cfg.set_role(&role, &model) {
                Ok(v) => {
                    self.notice(&format!("@{role} = {v}"));
                    if role == "default" {
                        self.send(Command::SetModel(model));
                    }
                }
                Err(e) => self.error(&format!("could not set @{role}: {e}")),
            },
            OverlayEvent::AssignRole { role: None, model } => self.send(Command::SetModel(model)),
            OverlayEvent::Login(LoginTarget::Provider(n)) => self.run_login(&[], Some(n)),
            OverlayEvent::Login(LoginTarget::Mcp(name)) => self.run_mcp_login(&name),
            OverlayEvent::Login(LoginTarget::ApiKey(id)) => {
                if let Some(def) = native::def(&id) {
                    self.start_key_flow(def);
                }
            }
            OverlayEvent::Key { provider, choice } => self.on_key_choice(&provider, choice),
            OverlayEvent::Logout(provider) => self.logout(&provider),
            OverlayEvent::Mcp(pick, row) => self.on_mcp_pick(pick, row),
            OverlayEvent::Copy(t) => self.copy_text(t, "nothing to copy"),
            OverlayEvent::Quote(q) => self.composer.append_quotes(&q),
            OverlayEvent::Resume(p) => self.send(Command::Resume(p)),
            OverlayEvent::CopyPlan(text) => self.copy_async(text, "plan copied".into()),
            OverlayEvent::EditPlan(file) => self.run_editor(&file.clone(), ChildThen::PlanEdit(file)),
            OverlayEvent::PlanCancelled { stored: true } => self.notice("plan left parked"),
            OverlayEvent::PlanCancelled { stored: false } => {
                self.inbox.borrow().cancel.cancel();
                self.notice("plan review cancelled; plan mode stays on");
            }
            OverlayEvent::OpenPlan(id) => self.send(Command::OpenPlan(id)),
            OverlayEvent::DeliverPlan(id) => self.send(Command::Deliver(id)),
            OverlayEvent::KillJobs(id) => self.send(Command::KillJobs(id)),
            OverlayEvent::NotifySession { session, text } => {
                let msg = match crate::notify::send(&session, Some("mu jobs panel"), &text, &self.cwd) {
                    Ok(line) => format!("message to {}: {line}", crate::session::id_of(&session)),
                    Err(e) => format!("message not sent: {e}"),
                };
                self.panel_status(msg);
            }
            OverlayEvent::StopSession { session, pid, started, kind } => {
                let msg = match crate::take::stop_session(&session, pid, &started, &kind) {
                    Ok(m) => m,
                    Err(e) => format!("not stopped: {e}"),
                };
                self.panel_status(msg);
            }
            OverlayEvent::TerminateSession { session, pid, started } => {
                let msg = match crate::take::terminate_session(&session, pid, &started) {
                    Ok(m) => m,
                    Err(e) => format!("not terminated: {e}"),
                };
                self.panel_status(msg);
            }
            OverlayEvent::TakeSession(path) => self.take_session(path),
        }
    }

    fn quit(&mut self) {
        if self.quitting {
            return;
        }
        self.quitting = true;
        self.inbox.borrow().cancel.cancel();
        self.notice("exiting…");
        self.send(Command::Exit);
    }

    fn submit(&mut self, mut input: UserInput, follow_up: bool) {
        if self.mcp_wizard.is_some() && input.images.is_empty() {
            let line = input.text.trim().to_string();
            return self.mcp_wizard_line(&line);
        }
        input.text = input.text.trim().to_string();
        if input.text.is_empty() && input.images.is_empty() {
            return;
        }
        if input.images.is_empty() {
            let t = input.text.as_str();
            if let Some(cmd) = t.strip_prefix("!!") {
                return self.user_bash(cmd.trim(), false);
            }
            if let Some(cmd) = t.strip_prefix('!') {
                return self.user_bash(cmd.trim(), true);
            }
            if let Some(cmd) = t.strip_prefix('/') {
                return self.slash(cmd);
            }
            let t = t.to_string();
            self.observe_prompt(&t);
        }
        self.send_prompt(input, follow_up);
    }

    fn user_bash(&mut self, cmd: &str, include: bool) {
        if self.running {
            return self.notice("busy — Esc to abort first");
        }
        if cmd.is_empty() {
            return;
        }
        self.running = true;
        self.send(Command::Bash { cmd: cmd.into(), include });
    }

    fn send_prompt(&mut self, input: impl Into<UserInput>, follow_up: bool) {
        let input = input.into();
        if self.running {
            let mut ib = self.inbox.borrow_mut();
            if follow_up { ib.follow_up.push_back(input) } else { ib.steer.push_back(input) }
        } else {
            self.running = true;
            self.send(Command::Prompt(input));
        }
    }

    fn slash(&mut self, cmd: &str) {
        let (name, arg) = cmd.split_once(char::is_whitespace).map_or((cmd, ""), |(n, a)| (n, a.trim()));
        let allowed_busy = matches!(
            name,
            "thinking" | "model" | "todo" | "skills" | "plugins" | "session" | "copy" | "reply" | "image" | "jobs"
        ) && !(name == "model" && arg.is_empty());
        if self.running && !allowed_busy {
            return self.notice("busy — Esc to abort first");
        }
        match name {
            "exit" | "quit" => self.quit(),
            "new" => {
                self.todo.clear();
                self.send(Command::NewSession);
                self.commit(vec![styled("── new session".to_string(), |_| dim())]);
            }
            "resume" => self.open_session_picker(),
            "model" | "switch" if arg.is_empty() => {
                self.pick_assign = name == "model";
                self.request_models();
            }
            "model" | "switch" => self.send(Command::SetModel(arg.into())),
            "login" => self.login(arg),
            "logout" => self.logout_cmd(arg),
            "mcp" => self.mcp_cmd(arg),
            "thinking" => match Thinking::parse(arg) {
                Some(t) => self.send(Command::SetThinking(t)),
                None => self.error("usage: /thinking off|low|medium|high|xhigh|max"),
            },
            "plan" => self.send(Command::TogglePlan((!arg.is_empty()).then(|| arg.to_string()))),
            "plans" => self.send(Command::ListPlans),
            "deliver" if arg.is_empty() => {
                self.pick_deliver = true;
                self.send(Command::ListPlans);
            }
            "deliver" => self.send(Command::Deliver(arg.into())),
            "compact" => {
                self.running = true;
                self.send(Command::Compact((!arg.is_empty()).then(|| arg.to_string())));
            }
            "review" => {
                self.running = true;
                self.send(Command::Review((!arg.is_empty()).then(|| arg.to_string())));
            }
            "session" => {
                let p = self.status.session_path.clone().unwrap_or_else(|| "no session yet".into());
                let t = self.status.title.clone().unwrap_or_else(|| "(untitled)".into());
                self.notice(&format!("{p}\n{t}"));
            }
            "rename" if !arg.is_empty() => self.send(Command::Rename(arg.into())),
            "rename" => self.error("usage: /rename <title>"),
            "todo" => self.show_todo(),
            "skills" => {
                let s: Vec<String> = self
                    .ctx
                    .skills
                    .iter()
                    .map(|s| format!("{}{}: {}", s.name, if s.hidden { " (hidden)" } else { "" }, s.description))
                    .collect();
                self.notice(&if s.is_empty() { "no skills".into() } else { s.join("\n") });
            }
            "plugins" => {
                let p: Vec<String> = self
                    .plugins
                    .status()
                    .into_iter()
                    .map(|(n, l, up, extra)| {
                        let extra = if extra.is_empty() { extra } else { format!(" · {extra}") };
                        format!("{n} · {l} · {}{extra}", if up { "running" } else { "stopped" })
                    })
                    .collect();
                self.notice(&if p.is_empty() { "no plugins".into() } else { p.join("\n") });
            }
            "hotkeys" => self.notice(HOTKEYS),
            "copy" => self.copy_cmd(arg),
            "reply" => self.open_reply(),
            "image" => self.image_cmd(arg),
            "jobs" => self.jobs_cmd(arg),
            _ => {
                if let Some(sk) = name.strip_prefix("skill:") {
                    match self.ctx.skill_prompt(sk, arg) {
                        Some(p) => self.send_prompt(p, false),
                        None => self.error(&format!("unknown skill {sk}")),
                    }
                } else if let Some(c) = self.ctx.commands.iter().find(|c| c.name == name) {
                    let p = c.expand(arg);
                    self.send_prompt(p, false);
                } else {
                    self.error(&format!("unknown command /{name}"));
                }
            }
        }
    }

    fn jobs_cmd(&mut self, arg: &str) {
        let mut words = arg.split_whitespace();
        match (words.next(), words.next(), words.next()) {
            (None, ..) => self.open_jobs(),
            (Some("kill"), Some("all"), None) => self.send(Command::KillJobs(None)),
            (Some("kill"), Some(id), None) => self.send(Command::KillJobs(Some(id.into()))),
            _ => self.error("usage: /jobs [kill <id|all>]"),
        }
    }

    /// The panel as this session sees it: its jobs, their output, and itself left out of the sessions.
    fn jobs_panel(&self) -> JobsPanel {
        JobsPanel::new(self.jobs.clone())
            .with_outputs(self.job_out.clone())
            .with_own_session(self.status.session_path.as_ref().map(PathBuf::from))
    }

    /// Opens the jobs panel: on the Jobs tab, or on the Sessions tab when there are no jobs.
    fn open_jobs(&mut self) {
        let panel = if self.jobs.is_empty() { self.jobs_panel().on_sessions() } else { self.jobs_panel() };
        self.overlay = Some(Overlay::Jobs { st: Box::new(panel) });
    }

    fn open_sessions(&mut self) {
        self.overlay = Some(Overlay::Jobs { st: Box::new(self.jobs_panel().on_sessions()) });
    }

    /// Says what a panel action did, in the panel when it is open.
    fn panel_status(&mut self, msg: String) {
        match &mut self.overlay {
            Some(Overlay::Jobs { st }) => st.set_status(msg),
            _ => self.notice(&msg),
        }
    }

    /// Hands the terminal to `mu take <session>`, then comes back to the Sessions tab.
    fn take_session(&mut self, path: PathBuf) {
        let exe = match mu_common::paths::self_exe() {
            Ok(e) => e,
            Err(e) => return self.error(&format!("cannot locate mu: {e}")),
        };
        self.start_child(ChildThen::Take, move || {
            extern "C" fn noop(_: libc::c_int) {}
            // As in `run_login`: Ctrl-C reaches `mu take`, not this mu.
            // SAFETY: installs a no-op handler; restored to the default below.
            unsafe { libc::signal(libc::SIGINT, noop as extern "C" fn(libc::c_int) as libc::sighandler_t) };
            let status = std::process::Command::new(exe).arg("take").arg(&path).status();
            // SAFETY: back to the default disposition.
            unsafe { libc::signal(libc::SIGINT, libc::SIG_DFL) };
            status
        });
    }

    fn image_cmd(&mut self, arg: &str) {
        if arg.is_empty() {
            return self.error("usage: /image <path>");
        }
        let path = match arg.strip_prefix("~/") {
            Some(rest) => PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(rest),
            None => self.cwd.join(arg),
        };
        match image::load(&path) {
            Ok(img) => self.composer.attach_image(img),
            Err(e) => self.error(&format!("image: {e}")),
        }
    }

    fn copy_cmd(&mut self, arg: &str) {
        match arg {
            "" => self.open_copy_picker(),
            "code" => {
                let t = self.tr.replies.iter().rev().find_map(|r| markdown::code_blocks(r).pop().map(|(_, b)| b));
                self.copy_text(t, "No code block to copy.");
            }
            "cmd" | "command" => {
                let t = self.bash_cmds.last().cloned();
                self.copy_text(t, "No command to copy.");
            }
            "link" | "url" => {
                let t = self.tr.replies.iter().rev().find_map(|r| markdown::links(r).pop());
                self.copy_text(t, "No link to copy.");
            }
            _ => self.error("usage: /copy [code|cmd|link]"),
        }
    }

    fn copy_text(&mut self, text: Option<String>, none: &str) {
        let Some(t) = text else { return self.notice(none) };
        let done = format!("copied {} chars", t.chars().count());
        self.copy_async(t, done);
    }

    /// Copies on a blocking thread with a 2 s limit, so a stuck clipboard helper cannot freeze the UI.
    fn copy_async(&mut self, text: String, done: String) {
        let tx = self.ui_tx.clone();
        tokio::task::spawn_local(async move {
            let r = match tokio::time::timeout(
                Duration::from_secs(2),
                tokio::task::spawn_blocking(move || clipboard::copy(&text)),
            )
            .await
            {
                Ok(Ok(r)) => r,
                Ok(Err(e)) => Err(e.to_string()),
                Err(_) => Err("the clipboard helper did not answer in 2 s".into()),
            };
            let _ = tx.send(UiMsg::Copied(r.map(|()| done)));
        });
    }

    fn open_copy_picker(&mut self) {
        let first = |s: &str| s.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim().to_string();
        let mut items = Vec::new();
        let mut texts = Vec::new();
        for r in self.tr.replies.iter().rev().take(20) {
            items.push(Item {
                label: format!("Reply: {}", trunc(&first(r), 60)),
                detail: format!("{} lines", r.lines().count()),
            });
            texts.push(r.clone());
            for (lang, body) in markdown::code_blocks(r) {
                let lang = if lang.is_empty() { "text" } else { &lang };
                items.push(Item {
                    label: format!("Code {lang}: {}", trunc(&first(&body), 50)),
                    detail: format!("{} lines", body.lines().count()),
                });
                texts.push(body);
            }
        }
        for c in self.bash_cmds.iter().rev().take(10) {
            items.push(Item {
                label: format!("Command: {}", trunc(&first(c), 60)),
                detail: format!("{} lines", c.lines().count()),
            });
            texts.push(c.clone());
        }
        if items.is_empty() {
            return self.notice("nothing to copy yet");
        }
        self.overlay = Some(Overlay::Copy { sel: Select::new("Copy", items), texts });
    }

    /// Opens the reply picker on the newest reply, or says there is none.
    fn open_reply(&mut self) {
        match ReplyPicker::open(&self.tr.replies) {
            Some(st) => self.overlay = Some(Overlay::Reply { st: Box::new(st) }),
            None => self.notice("no reply to quote yet"),
        }
    }

    fn show_todo(&mut self) {
        if self.todo.iter().all(|p| p.tasks.is_empty()) {
            return self.notice("todo list is empty");
        }
        let mut out = Vec::new();
        for p in &self.todo {
            out.push(p.name.clone());
            for t in &p.tasks {
                let mark = match t.status {
                    Status::Completed => "[x]",
                    Status::InProgress => "[/]",
                    Status::Abandoned => "[-]",
                    Status::Blocked => "[!]",
                    Status::Pending => "[ ]",
                };
                out.push(format!("  {mark} {}", t.content));
            }
        }
        self.notice(&out.join("\n"));
    }

    /// `/login`: with a provider id run `omp login <id>`, with `mcp:<name>` log in to that MCP server;
    /// otherwise list omp's providers and the MCP servers that need auth in a picker.
    fn login(&mut self, provider: &str) {
        if let Some(def) = native::def(provider) {
            return self.start_key_flow(def);
        }
        if let Some(name) = provider.strip_prefix("mcp:") {
            return self.run_mcp_login(name);
        }
        if !provider.is_empty() {
            return self.run_login(&[provider], None);
        }
        self.notice("listing login providers…");
        let tx = self.ui_tx.clone();
        tokio::task::spawn_local(async move {
            let _ = tx.send(UiMsg::LoginProviders(list_login_providers().await));
        });
    }

    /// Hands the terminal to a child that runs on a blocking thread, so the agent, plugins and
    /// timers keep running. Input reading and drawing pause until `ChildDone` arrives.
    fn start_child(
        &mut self,
        then: ChildThen,
        job: impl FnOnce() -> std::io::Result<std::process::ExitStatus> + Send + 'static,
    ) {
        if self.child_busy {
            return self.notice("another program already has the terminal");
        }
        let committed = std::mem::take(&mut self.tr.pending);
        let _ = self.renderer.draw(&committed, &[], (0, 0));
        let _ = self.renderer.finish();
        let _ = term::suspend();
        self.child_busy = true;
        let tx = self.ui_tx.clone();
        tokio::task::spawn_local(async move {
            let status = match tokio::task::spawn_blocking(job).await {
                Ok(r) => r.map_err(|e| e.to_string()),
                Err(e) => Err(e.to_string()),
            };
            let _ = tx.send(UiMsg::ChildDone { then, status });
        });
    }

    fn on_child_done(&mut self, then: ChildThen, status: Result<std::process::ExitStatus, String>) {
        let _ = term::resume();
        self.renderer = Renderer::new();
        self.child_busy = false;
        self.dirty = true;
        match then {
            ChildThen::Login => match status {
                Ok(s) if s.success() => self.notice("login finished"),
                Ok(s) => self.error(&format!("omp login exited with {s}")),
                Err(e) => self.error(&format!("cannot run omp login: {e}")),
            },
            ChildThen::McpLogin(name) => match status {
                Ok(s) if s.success() => {
                    self.notice(&format!("mcp:{name}: logged in; loading tools…"));
                    let (plugins, tx) = (self.plugins.clone(), self.ui_tx.clone());
                    tokio::task::spawn_local(async move {
                        let r = plugins.after_login(&name).await;
                        let _ = tx.send(UiMsg::McpLoggedIn(name, r));
                    });
                }
                Ok(s) => self.error(&format!("omp /mcp reauth {name} exited with {s}")),
                Err(e) => self.error(&format!("cannot run omp: {e}")),
            },
            ChildThen::ComposerEdit(path) => {
                match status {
                    Ok(s) if s.success() => {
                        if let Ok(t) = std::fs::read_to_string(&path) {
                            self.composer.set_text(t.trim_end_matches('\n'));
                        }
                    }
                    Ok(_) => {}
                    Err(e) => self.error(&format!("editor failed: {e}")),
                }
                let _ = std::fs::remove_file(&path);
            }
            ChildThen::Take => {
                match status {
                    Ok(s) if s.success() => self.notice("back from mu take"),
                    Ok(s) => self.error(&format!("mu take exited with {s}")),
                    Err(e) => self.error(&format!("cannot run mu take: {e}")),
                }
                self.open_sessions();
            }
            ChildThen::PlanEdit(path) => {
                if let Err(e) = status {
                    self.error(&format!("editor failed: {e}"));
                }
                match (std::fs::read_to_string(&path), &mut self.overlay) {
                    (Ok(t), Some(Overlay::Review { review, .. })) => review.set_content(t),
                    (Err(e), _) => self.error(&format!("cannot read plan: {e}")),
                    _ => {}
                }
            }
        }
    }

    /// Runs `omp login` on the real terminal. With `pick`, omp's own provider menu is swallowed and
    /// answered with that number once its prompt appears; terminal input is then relayed so OAuth
    /// prompts still work. The gateway reads the stored credentials.
    fn run_login(&mut self, args: &[&str], pick: Option<usize>) {
        use std::process::Stdio;
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        self.start_child(ChildThen::Login, move || {
            let mut cmd = std::process::Command::new(mu_common::paths::omp_bin());
            cmd.arg("login").args(&args);
            if pick.is_some() {
                cmd.stdin(Stdio::piped()).stdout(Stdio::piped());
            }
            extern "C" fn noop(_: libc::c_int) {}
            // A caught (not ignored) SIGINT: Ctrl-C reaches omp, which resets it on exec, but not mu.
            // SAFETY: installs a no-op handler; restored to the default below.
            unsafe { libc::signal(libc::SIGINT, noop as extern "C" fn(libc::c_int) as libc::sighandler_t) };
            let status = match (cmd.spawn(), pick) {
                (Ok(mut child), Some(n)) => relay_login(&mut child, n),
                (Ok(mut child), None) => child.wait(),
                (Err(e), _) => Err(e),
            };
            // SAFETY: back to the default disposition.
            unsafe { libc::signal(libc::SIGINT, libc::SIG_DFL) };
            status
        });
    }

    /// Runs omp's own MCP OAuth flow (`omp -p --no-session '/mcp reauth <name>'`) on the real terminal,
    /// then makes the server usable in this session (`PluginHost::after_login`).
    fn run_mcp_login(&mut self, name: &str) {
        if !self.plugins.mcp_names().iter().any(|n| n == name) {
            return self.error(&format!("no MCP server `{name}` is configured"));
        }
        let (cwd, name) = (self.cwd.clone(), name.to_string());
        self.start_child(ChildThen::McpLogin(name.clone()), move || {
            let mut cmd = std::process::Command::new(mu_common::paths::omp_bin());
            cmd.args(["-p", "--no-session"]).arg(format!("/mcp reauth {name}")).current_dir(&cwd);
            extern "C" fn noop(_: libc::c_int) {}
            // As in `run_login`: Ctrl-C reaches omp, not mu.
            // SAFETY: installs a no-op handler; restored to the default below.
            unsafe { libc::signal(libc::SIGINT, noop as extern "C" fn(libc::c_int) as libc::sighandler_t) };
            let status = cmd.status();
            // SAFETY: back to the default disposition.
            unsafe { libc::signal(libc::SIGINT, libc::SIG_DFL) };
            status
        });
    }

    /// Runs `$VISUAL`/`$EDITOR` on `path` with the terminal handed over.
    fn run_editor(&mut self, path: &std::path::Path, then: ChildThen) {
        let editor = std::env::var("VISUAL").or_else(|_| std::env::var("EDITOR")).unwrap_or_else(|_| "vi".into());
        let path = path.to_path_buf();
        self.start_child(then, move || {
            std::process::Command::new("sh").arg("-c").arg(format!("{editor} \"$1\"")).arg("sh").arg(&path).status()
        });
    }

    fn edit_external(&mut self) {
        let path = match private_temp_file(&self.composer.text()) {
            Ok(p) => p,
            Err(e) => return self.error(&format!("cannot create temp file: {e}")),
        };
        self.run_editor(&path.clone(), ChildThen::ComposerEdit(path));
    }

    /// Roles from omp's `cycleOrder` that can be switched to when implementing an approved plan.
    fn plan_tiers(&self) -> Vec<Tier> {
        let cfg = &self.providers.cfg;
        let mut out = Vec::new();
        for r in cfg.omp.role_cycle() {
            match cfg.role(&r) {
                Some(v) if self.providers.resolve(&format!("@{r}")).is_ok() => {
                    out.push(Tier { model: Some(format!("@{r}")), detail: v, role: r });
                }
                None if r == "default" => out.push(Tier { role: r, detail: "current model".into(), model: None }),
                _ => {}
            }
        }
        out
    }

    // ---- drawing ------------------------------------------------------------------------------

    fn spin(&self) -> char {
        SPINNER[(self.started.elapsed().as_millis() / 80) as usize % SPINNER.len()]
    }

    fn live_lines(&self, w: usize) -> (Vec<Line>, Option<(usize, usize)>) {
        let mut pty_cursor = None;
        let mut out: Vec<Line> = Vec::new();
        out.extend(self.tr.live_lines(w, self.spin()));
        for t in &self.tools {
            if let Some(p) = self.pty.as_ref().filter(|p| p.id == t.id) {
                let (rows, cur) = p.screen();
                let (lines, c) = blocks::pty_live(&t.args, &rows, cur, t.started.elapsed().as_secs(), w);
                pty_cursor = c.map(|c| (out.len() + c.0, c.1));
                out.extend(lines);
                continue;
            }
            let mut tail: Vec<String> = t.tail.iter().cloned().collect();
            if !t.partial.is_empty() {
                tail.push(t.partial.clone());
            }
            let tail = &tail[tail.len().saturating_sub(10)..];
            out.extend(blocks::tool_live(&t.name, &t.args, tail, self.spin(), t.started.elapsed().as_secs(), w));
        }
        for c in &self.calls {
            out.extend(blocks::tool_streaming(&c.name, &c.args, self.spin(), w));
        }
        let running: Vec<SubagentStatus> =
            self.jobs.iter().filter(|j| j.state == SubagentState::Running).cloned().collect();
        out.extend(blocks::subagent_rows(&running, self.spin()));
        if self.compacting {
            out.push(styled(format!("{} Compacting context…", self.spin()), |_| theme::get().fg(Role::Warn)));
        }
        (out, pty_cursor)
    }

    fn todo_hud(&self, w: usize) -> Vec<Line> {
        let all: Vec<&crate::tools::todo::Task> = self.todo.iter().flat_map(|p| p.tasks.iter()).collect();
        if all.is_empty() {
            return vec![];
        }
        let open = |t: &crate::tools::todo::Task| matches!(t.status, Status::Pending | Status::InProgress);
        let Some(phase) = self.todo.iter().find(|p| p.tasks.iter().any(open)) else { return vec![] };
        let done = all.iter().filter(|t| matches!(t.status, Status::Completed | Status::Abandoned)).count();
        let mut out = vec![styled(trunc(&format!("todo {done}/{} · {}", all.len(), phase.name), w), |_| dim())];
        for t in phase.tasks.iter().filter(|t| t.status == Status::InProgress).take(1) {
            out.push(styled(trunc(&format!("  ▸ {}", t.content), w), |_| theme::get().fg(Role::Warn)));
        }
        for t in phase.tasks.iter().filter(|t| t.status == Status::Pending).take(3) {
            out.push(styled(trunc(&format!("  ○ {}", t.content), w), |_| dim()));
        }
        out.truncate(6);
        out
    }

    fn queue_bar(&self, w: usize) -> Vec<Line> {
        let ib = self.inbox.borrow();
        let mut out: Vec<Line> = ib
            .steer
            .iter()
            .map(|s| ("steer", s))
            .chain(ib.follow_up.iter().map(|s| ("after turn", s)))
            .map(|(k, s)| styled(trunc(&format!("↳ {k}: {}", s.text.replace('\n', " ")), w), |_| dim()))
            .collect();
        if let Some(last) = out.last_mut() {
            let used: usize = line_text(last).chars().count();
            if used + 12 <= w {
                last.push(span("  alt+↑ edit", dim()));
            }
        }
        out
    }

    fn draw(&mut self) -> std::io::Result<()> {
        let w = self.width();
        self.tr.advance_reveal(w);
        for c in &mut self.calls {
            // At most once per frame: deltas arrive far more often than frames are drawn.
            if c.stale {
                if let Some(v) = c.json.value() {
                    c.args = v;
                }
                c.stale = false;
            }
        }
        let (w, rows) = self.renderer.size();
        let mut vp: Vec<Line> = Vec::new();
        let (live, pty_cursor) = self.live_lines(w);
        vp.extend(live);
        vp.extend(self.todo_hud(w));
        vp.extend(self.queue_bar(w));
        let mut env = status::StatusEnv::new(&self.cwd, &self.status_cfg);
        env.expanded = self.expanded;
        env.turn_elapsed = self.turn_started.map(|t| t.elapsed());
        env.jobs = status::JobCounts::of(&self.jobs);
        let status_lines = status::render(&self.status, &env, w);
        let pty_open = self.pty.is_some();
        let (lines, cur) = match &mut self.overlay {
            _ if pty_open => {
                (vec![styled("  keys go to the command · ctrl+c twice force-kills".to_string(), |_| dim())], (0, 0))
            }
            Some(ov) => ov.render(w, rows, vp.len() + status_lines.len()),
            None => self.composer.render(w),
        };
        let cursor = pty_cursor.filter(|_| pty_open).unwrap_or((vp.len() + cur.0, cur.1));
        vp.extend(lines);
        vp.extend(status_lines);
        let committed = std::mem::take(&mut self.tr.pending);
        if let Err(e) = self.renderer.draw(&committed, &vp, cursor) {
            // Nothing reached the terminal: keep the lines for the next try, and wait a frame
            // before it so a persistent failure cannot spin the loop.
            self.tr.pending = committed;
            self.last_draw = Instant::now();
            return Err(e);
        }
        self.last_draw = Instant::now();
        self.dirty = self.tr.revealing();
        Ok(())
    }
}

const HOTKEYS: &str = "Enter  submit (steer while a turn runs)      Ctrl+Q  queue follow-up      Alt+↑  edit last queued
Shift+Enter / Ctrl+J / Alt+Enter  newline      Esc  abort turn (in a pty command: sent to it)      Ctrl+C  clear · abort · exit (twice)
Shift+Tab  cycle thinking      Ctrl+T  toggle thinking      Ctrl+O  expand output      Alt+M  model picker      Alt+J  background jobs      Ctrl+R  quote parts of a reply
Ctrl+G  edit in $EDITOR      Ctrl+V  paste clipboard image      Tab  accept suggestion / complete /command      →  accept suggestion      ↑/↓  history      Ctrl+A/E/K/U/W  line editing
!cmd  run shell command and send output      !!cmd  run without sending";

/// Drives a child `omp login` whose stdin and stdout are piped (see `run_login`).
fn relay_login(child: &mut std::process::Child, pick: usize) -> std::io::Result<std::process::ExitStatus> {
    use std::io::Write;
    use std::os::fd::AsRawFd;
    let (stdin, Some(out)) = (child.stdin.take(), child.stdout.take()) else { return child.wait() };
    let mut stdin = stdin;
    let out_fd = out.as_raw_fd();
    let mut asked = false;
    let mut seen = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let mut fds = [
            libc::pollfd { fd: out_fd, events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: if stdin.is_some() && asked { 0 } else { -1 }, events: libc::POLLIN, revents: 0 },
        ];
        // SAFETY: two valid pollfds for the duration of the call.
        unsafe { libc::poll(fds.as_mut_ptr(), 2, 200) };
        if fds[0].revents & (libc::POLLIN | libc::POLLHUP) != 0 {
            // SAFETY: `buf` is a live, writable buffer of the given length.
            let r = unsafe { libc::read(out_fd, buf.as_mut_ptr().cast(), buf.len()) };
            if r <= 0 {
                break;
            }
            let chunk = &buf[..r as usize];
            if asked {
                let mut o = std::io::stdout();
                let _ = o.write_all(chunk).and_then(|_| o.flush());
            } else {
                seen.extend_from_slice(chunk);
                if String::from_utf8_lossy(&seen).contains("Enter number") {
                    asked = true;
                    if let Some(w) = stdin.as_mut() {
                        let _ = writeln!(w, "{pick}");
                    }
                }
            }
        }
        if fds[1].revents & libc::POLLIN != 0 {
            // SAFETY: as above, for fd 0.
            let r = unsafe { libc::read(0, buf.as_mut_ptr().cast(), buf.len()) };
            if r > 0 {
                if let Some(w) = stdin.as_mut() {
                    let _ = w.write_all(&buf[..r as usize]);
                }
            } else {
                stdin = None;
            }
        }
    }
    child.wait()
}

/// omp's login menu names Baseten too; mu owns that provider's key, so the omp row is dropped.
fn is_baseten(name: &str) -> bool {
    name.split(|c: char| !c.is_alphanumeric())
        .find(|w| !w.is_empty())
        .is_some_and(|w| w.eq_ignore_ascii_case("baseten"))
}

/// Provider menu of `omp login`, scraped from its numbered prompt (omp has no machine-readable list).
async fn list_login_providers() -> Result<Vec<(usize, String)>, String> {
    let out = tokio::process::Command::new(mu_common::paths::omp_bin())
        .arg("login")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .await
        .map_err(|e| e.to_string())?;
    Ok(parse_login_menu(&String::from_utf8_lossy(&out.stdout)))
}

fn parse_login_menu(text: &str) -> Vec<(usize, String)> {
    let mut plain = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for n in chars.by_ref() {
                if n.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            plain.push(c);
        }
    }
    plain
        .lines()
        .filter_map(|l| {
            let (n, name) = l.trim().split_once(". ")?;
            Some((n.parse().ok()?, name.trim().to_string())).filter(|(_, name): &(usize, String)| !name.is_empty())
        })
        .collect()
}

#[cfg(test)]
mod login_tests {
    use super::parse_login_menu;

    #[test]
    fn menu_numbers_and_names_survive_ansi_and_noise() {
        let t = "Select a provider:\n\n  1. ChatGPT Plus/Pro (Codex)\n\x1b[2m  12. Z.AI (GLM · Sign in)\x1b[0m\nEnter number: ";
        assert_eq!(
            parse_login_menu(t),
            vec![(1, "ChatGPT Plus/Pro (Codex)".into()), (12, "Z.AI (GLM · Sign in)".into())]
        );
    }

    #[test]
    fn baseten_is_the_only_omp_row_mu_drops() {
        use super::is_baseten;
        assert!(is_baseten("Baseten"));
        assert!(is_baseten(" Baseten (API key)"));
        assert!(!is_baseten("Basetenx"));
        assert!(!is_baseten("Z.AI (GLM · Sign in)"));
    }

    #[test]
    #[ignore]
    fn contract_omp_login_menu() {
        let out = mu_common::paths::run_omp_blocking(&["login"], &[]);
        assert!(!parse_login_menu(&out).is_empty(), "no providers parsed from: {out}");
    }
}

/// Creates a new file (`O_EXCL`, mode 0600, random name) in `$XDG_RUNTIME_DIR` or the temp dir.
fn private_temp_file(content: &str) -> std::io::Result<std::path::PathBuf> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .filter(|d| d.is_dir())
        .unwrap_or_else(std::env::temp_dir);
    let path = dir.join(format!("mu-edit-{}.md", uuid::Uuid::new_v4().simple()));
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path)?;
    f.write_all(content.as_bytes())?;
    Ok(path)
}

#[cfg(test)]
mod temp_tests {
    #[test]
    fn temp_file_is_private_and_unique() {
        use std::os::unix::fs::PermissionsExt;
        let a = super::private_temp_file("x").unwrap();
        let b = super::private_temp_file("y").unwrap();
        assert_ne!(a, b);
        assert_eq!(std::fs::metadata(&a).unwrap().permissions().mode() & 0o777, 0o600);
        let _ = (std::fs::remove_file(a), std::fs::remove_file(b));
    }
}
