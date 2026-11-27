//! The TUI's event loop and the state it runs on. The method groups live in the sibling files:
//! `agent_view` (the agent-event reducer), `ui_msgs` (results of background work and the tick),
//! `slash` (what the composer submits), `commands` (the table of built-in slash commands),
//! `schedule` (prompts sent at a later time), `pickers` (overlays), `child` (programs that take the
//! terminal), `keys` (input routing and API keys),
//! `mcp` (`/mcp`), `memory` (`/memory`), `model_sets` (`/modelset`), `custom_providers` (the
//! custom-endpoint panel), and `services` (the writes that leave the screen: config, stored keys,
//! MCP definitions, session control).

mod agent_view;
mod chatgpt;
mod child;
mod commands;
mod custom_providers;
mod gateway_login;
mod hostcall;
mod keys;
mod mcp;
mod mcp_login;
mod memory;
mod model_sets;
mod open;
mod pickers;
mod schedule;
#[cfg(test)]
mod services;
mod slash;
mod transfer;
mod ui_msgs;
mod web;

use self::agent_view::{AgentView, error_lines, notice_lines};
use self::chatgpt::ChatgptFlow;
pub use self::commands::interactive_reference;
pub(crate) use self::commands::{command_entries, is_builtin};
use self::keys::KeyFlow;
use self::mcp_login::McpLoginFlow;
use self::schedule::Schedule;
#[cfg(test)]
use self::services::Services;
pub(crate) use self::slash::custom_completion_ids;
use self::ui_msgs::UiMsg;
use crate::chat::composer::Composer;
use crate::host::completion::{self, Completions, Expanded, PluginRow, SkillRow};
use crate::host::startup::{self, ImportOffer, Startup, UntrustedFile};
use crate::host::{self, Host};
use crate::mcp::mcp_wizard::Wizard;
use crate::render::Renderer;
use crate::status;
use crate::term::input::{self, Input};
use crate::term::term::{self, Term};
use crate::text::line::{Line, dim, line_text, span, styled};
use crate::theme::{self, Role};
use crate::widgets::complete::Entry;
use crate::widgets::overlay::Overlay;
use crossterm::event::Event;
use mu_base::events::{AgentEvent, Command};
use mu_base::message::UserInput;
use mu_base::permissions::{ApprovalDecision, ApprovalRequest};
use mu_base::text::truncate_text as trunc;
use mu_common::paths;
#[cfg(test)]
use mu_tools::context::Context;
#[cfg(test)]
use mu_tools::plugin::host::PluginHost;
use serde_json::json;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
const FRAME: Duration = Duration::from_millis(33);
/// Shown under whatever the screen holds while the first Ctrl-C waits for the second.
const CTRL_C_HINT: &str = "  (press Ctrl-C again to exit)";

pub struct AppInit {
    /// Where the host's state is: this process (`host::local::LocalHost`) or a remote one.
    pub host: Rc<dyn Host>,
    pub cmd_tx: UnboundedSender<Command>,
    pub ev_rx: UnboundedReceiver<AgentEvent>,
    pub notice_rx: UnboundedReceiver<String>,
    pub cwd: PathBuf,
    pub pick_session: bool,
    pub theme: mu_base::config::MuTheme,
    pub keys: mu_base::config::MuKeys,
    /// Ring the terminal when your turn ends with nothing running in the background, or when a
    /// running turn needs you: a question, an approval or a plan review, whoever started it (omp's
    /// `completion.notify`).
    pub notify: bool,
    /// Which events ring (`[bell]` in config.toml); `notify` off silences all of them.
    pub bell: mu_base::config::MuBell,
    /// `hide_thinking_block`, `smooth_streaming` and the `[status]` levels: they describe this
    /// terminal, so they come from the client's own config, not from the host.
    pub hide_thinking: bool,
    pub smooth_streaming: bool,
    pub status_levels: mu_base::config::MuStatus,
    /// `[display] hyperlinks`: write URLs as OSC 8 links.
    pub hyperlinks: bool,
}

struct App {
    agent: AgentView,
    host: Rc<dyn Host>,
    /// The tests' way to the objects the host answers from, to set them up and to check what was written.
    #[cfg(test)]
    services: Services,
    link: Link,
    #[cfg(test)]
    ctx: Rc<Context>,
    #[cfg(test)]
    plugins: Rc<PluginHost>,
    ui_tx: UnboundedSender<UiMsg>,
    cwd: PathBuf,
    screen: Screen,
    prompt: Prompt,
    overlay: Option<Overlay>,
    /// Dialogs opened while an approval, ask or plan review was up; see `open_overlay`.
    parked: VecDeque<Overlay>,
    /// Prompts that go out later; see `schedule`.
    schedule: Schedule,
    /// Tool calls waiting for approval, oldest first; one is on screen at a time.
    approvals: VecDeque<(ApprovalRequest, tokio::sync::oneshot::Sender<ApprovalDecision>)>,
    flows: Flows,
    quitting: bool,
    /// The first Ctrl-C was pressed: the next one exits, whatever the screen holds.
    ctrl_c_armed: bool,
    /// Copies of host files made by `/open`, removed when mu exits.
    opened: open::Opened,
    /// Ports forwarded here for the jobs that listen on them; removed with the job or at exit.
    forwards: transfer::Forwards,
}

/// `run` is never called by a test, which builds its `App` and the handles itself.
#[cfg(test)]
fn test_handles() -> (Services, Rc<Context>, Rc<PluginHost>) {
    unreachable!("the tests build their own App")
}

/// The channels to the agent.
struct Link {
    cmd_tx: UnboundedSender<Command>,
}

/// The terminal: what is drawn, when, and whether a child program owns it.
struct Screen {
    renderer: Renderer,
    last_draw: Instant,
    /// Something changed that needs a redraw.
    dirty: bool,
    /// A child process (editor, `mu take`) owns the terminal: no drawing and no input reading
    /// until it exits, but agent and plugin tasks keep running.
    child_busy: bool,
    started: Instant,
    status_levels: mu_base::config::MuStatus,
    notify: bool,
}

/// The composer, its completion list, the prediction asked for, and input held back while a key
/// is typed.
struct Prompt {
    composer: Composer,
    commands: Vec<Entry>,
    /// Ghost-text prediction is on (the host's setting, told at start-up).
    predict: bool,
    ghost_seq: u64,
    ghost_asked: String,
    /// The `/cd` argument whose directories were asked for, and the number of that ask.
    dirs_asked: Option<String>,
    dirs_seq: u64,
    /// When the agents offered to `/agents` and `/remote` completion were last asked for, and whether an ask is out.
    agents_asked: Option<Instant>,
    agents_busy: bool,
    /// Queued inputs that could not go back into the composer while a key was typed.
    held_back: Vec<UserInput>,
}

/// The `/web` wizard, and who is waiting for it: a `web_search` call that found no backend.
type WebFlow = (crate::web::wizard::Wizard, Option<tokio::sync::oneshot::Sender<bool>>);

/// The multi-step flow that owns input, if any. Starting one replaces the last.
#[derive(Default)]
enum Active {
    #[default]
    None,
    /// An `/mcp add` wizard is collecting answers; the composer's lines go to it.
    Mcp(Box<Wizard>),
    /// An API-key flow in progress; while it types, the composer takes its text. A `/web` wizard
    /// that asked for the key waits inside it.
    Key(Box<KeyFlow>),
    /// A ChatGPT sign-in in progress; while it waits the composer takes the redirect URL.
    Chatgpt(ChatgptFlow),
    /// An MCP server's OAuth sign-in in progress; while it waits the composer takes the redirect URL.
    McpLogin(McpLoginFlow),
    Web(Box<WebFlow>),
    /// The custom-endpoint wizard in progress.
    Custom(Box<custom_providers::CustomFlow>),
}

/// Multi-step interactions that take over input.
struct Flows {
    model_request_seq: u64,
    pick_deliver: bool,
    active: Active,
    key_flow_seq: u64,
    /// Tells a late sign-in result from another's.
    chatgpt_seq: u64,
    /// Tells a late MCP sign-in result from another's.
    mcp_login_seq: u64,
    /// Tells a late custom-endpoint answer from another's.
    custom_wizard_seq: u64,
}

impl Flows {
    fn mcp(&self) -> Option<&Wizard> {
        match &self.active {
            Active::Mcp(w) => Some(w),
            _ => None,
        }
    }

    fn mcp_mut(&mut self) -> Option<&mut Wizard> {
        match &mut self.active {
            Active::Mcp(w) => Some(w),
            _ => None,
        }
    }

    fn key(&self) -> Option<&KeyFlow> {
        match &self.active {
            Active::Key(f) => Some(f),
            _ => None,
        }
    }

    fn key_mut(&mut self) -> Option<&mut KeyFlow> {
        match &mut self.active {
            Active::Key(f) => Some(f),
            _ => None,
        }
    }

    fn chatgpt(&self) -> Option<&ChatgptFlow> {
        match &self.active {
            Active::Chatgpt(f) => Some(f),
            _ => None,
        }
    }

    fn mcp_login(&self) -> Option<&McpLoginFlow> {
        match &self.active {
            Active::McpLogin(f) => Some(f),
            _ => None,
        }
    }

    fn web(&self) -> Option<&WebFlow> {
        match &self.active {
            Active::Web(f) => Some(f),
            _ => None,
        }
    }

    fn web_mut(&mut self) -> Option<&mut WebFlow> {
        match &mut self.active {
            Active::Web(f) => Some(f),
            _ => None,
        }
    }

    fn custom(&self) -> Option<&custom_providers::CustomFlow> {
        match &self.active {
            Active::Custom(f) => Some(f),
            _ => None,
        }
    }

    fn custom_mut(&mut self) -> Option<&mut custom_providers::CustomFlow> {
        match &mut self.active {
            Active::Custom(f) => Some(f),
            _ => None,
        }
    }

    /// Makes `next` the active flow. A `/web` wizard it displaces (or that waits inside a key
    /// flow it displaces) releases its waiting call.
    fn begin(&mut self, next: Active) {
        let web = match std::mem::replace(&mut self.active, next) {
            Active::Web(w) => Some(w),
            Active::Key(f) => f.web,
            _ => None,
        };
        if let Some(reply) = web.and_then(|w| w.1) {
            let _ = reply.send(false);
        }
    }

    fn end_mcp(&mut self) -> Option<Wizard> {
        match std::mem::take(&mut self.active) {
            Active::Mcp(w) => Some(*w),
            other => {
                self.active = other;
                None
            }
        }
    }

    /// Ends the key flow; the `/web` wizard that was waiting on it becomes active again.
    fn end_key(&mut self) -> Option<KeyFlow> {
        match std::mem::take(&mut self.active) {
            Active::Key(mut f) => {
                if let Some(w) = f.web.take() {
                    self.active = Active::Web(w);
                }
                Some(*f)
            }
            other => {
                self.active = other;
                None
            }
        }
    }

    fn end_chatgpt(&mut self) -> Option<ChatgptFlow> {
        match std::mem::take(&mut self.active) {
            Active::Chatgpt(f) => Some(f),
            other => {
                self.active = other;
                None
            }
        }
    }

    fn end_mcp_login(&mut self) -> Option<McpLoginFlow> {
        match std::mem::take(&mut self.active) {
            Active::McpLogin(f) => Some(f),
            other => {
                self.active = other;
                None
            }
        }
    }

    fn end_web(&mut self) -> Option<WebFlow> {
        match std::mem::take(&mut self.active) {
            Active::Web(f) => Some(*f),
            other => {
                self.active = other;
                None
            }
        }
    }

    fn end_custom(&mut self) -> Option<custom_providers::CustomFlow> {
        match std::mem::take(&mut self.active) {
            Active::Custom(f) => Some(*f),
            other => {
                self.active = other;
                None
            }
        }
    }
}

pub async fn run(init: AppInit) -> i32 {
    let _term = match Term::enter() {
        Ok(t) => t,
        Err(e) => {
            term::say_err(format_args!("mu: cannot initialise terminal: {e}\n"));
            return 1;
        }
    };
    term::install_panic_hook();
    let AppInit {
        host,
        cmd_tx,
        mut ev_rx,
        mut notice_rx,
        cwd,
        pick_session,
        theme,
        keys,
        notify,
        bell,
        hide_thinking,
        smooth_streaming,
        status_levels,
        hyperlinks,
    } = init;
    crate::text::line::set_hyperlinks(hyperlinks);
    crate::host::set_facts(host.info().clone());
    let mut theme_warnings = theme::init(&theme);
    theme_warnings.extend(crate::keymap::init(&keys));
    let (ui_tx, mut ui_rx) = unbounded_channel();
    let completions: Completions = crate::host::call(&*host, completion::COMPLETIONS, &()).await.unwrap_or_else(|e| {
        Completions { entries: Vec::new(), error: Some(format!("cannot read the completion list: {e}")) }
    });
    let startup: Startup = match crate::host::call(&*host, startup::STARTUP, &()).await {
        Ok(s) => s,
        Err(e) => {
            term::say_err(format_args!("mu: the host did not answer: {e}\n"));
            return 1;
        }
    };
    #[cfg(test)]
    let (services, ctx, plugins) = test_handles();
    let mut app = App {
        agent: AgentView::new(!hide_thinking, smooth_streaming).with_bell(bell),
        host,
        #[cfg(test)]
        services,
        link: Link { cmd_tx },
        #[cfg(test)]
        ctx,
        #[cfg(test)]
        plugins,
        ui_tx,
        cwd: cwd.clone(),
        screen: Screen {
            renderer: Renderer::new(),
            last_draw: Instant::now() - FRAME,
            dirty: true,
            child_busy: false,
            started: Instant::now(),
            status_levels,
            notify,
        },
        prompt: Prompt {
            composer: Composer::new(Some(paths::profile_home().join("history.jsonl"))).with_cwd(&cwd),
            commands: completions.entries,
            ghost_seq: 0,
            ghost_asked: String::new(),
            dirs_asked: None,
            dirs_seq: 0,
            agents_asked: None,
            agents_busy: false,
            held_back: Vec::new(),
            predict: startup.predict,
        },
        overlay: None,
        parked: VecDeque::new(),
        schedule: Schedule::default(),
        approvals: VecDeque::new(),
        flows: Flows {
            model_request_seq: 0,
            pick_deliver: false,
            active: Active::None,
            key_flow_seq: 0,
            chatgpt_seq: 0,
            mcp_login_seq: 0,
            custom_wizard_seq: 0,
        },
        quitting: false,
        ctrl_c_armed: false,
        opened: Default::default(),
        forwards: Default::default(),
    };
    for w in theme_warnings.iter().chain(startup.warnings.iter()) {
        app.agent.tr.pending.push(styled(w.clone(), |_| theme::get().fg(Role::Warn)));
    }
    if let Some(e) = completions.error {
        app.error(&e);
    }
    let offers: Vec<ImportOffer> = startup.import_offer.into_iter().chain(startup.pi_import_offer).collect();
    if !offers.is_empty() {
        if pick_session {
            let names = offers.iter().map(|o| o.source.as_str()).collect::<Vec<_>>().join(" and ");
            app.notice(&format!("{names}'s setup was not copied: /modelset import"));
        } else {
            app.open_import_offer(&offers);
        }
    }
    if !startup.untrusted_mcp.is_empty() {
        if app.overlay.is_none() && !pick_session {
            app.open_mcp_trust(startup.untrusted_mcp);
        } else {
            app.notice("this project's MCP servers are off until you trust them: /mcp trust");
        }
    }
    if pick_session {
        app.open_session_picker();
    }
    let mut reader = match input::Reader::new() {
        Ok(r) => r,
        Err(e) => {
            term::say_err(format_args!("mu: cannot read the terminal: {e}\n"));
            return 1;
        }
    };
    let mut pushed = app.host.pushed();
    let mut tick = tokio::time::interval(Duration::from_millis(80));
    // The countdown above the composer while a prompt waits for its time.
    let mut clock = tokio::time::interval(Duration::from_secs(1));
    let mut signals = term::Shutdown::new().ok();
    let mut draw_failed = false;
    loop {
        if let Some(p) = term::take_panic() {
            app.error(&p);
        }
        if app.screen.dirty && !app.screen.child_busy && Instant::now() >= app.screen.last_draw + FRAME {
            match app.draw() {
                Ok(()) => {}
                // The terminal hung up: nothing can be drawn on it again. Ask the agent to stop and
                // leave without a word.
                Err(e) if term::hangup(&e) => {
                    term::mark_lost();
                    app.quit();
                    break;
                }
                Err(e) if !draw_failed => {
                    // Said once: stderr is the terminal, and repeating it would flood the screen.
                    draw_failed = true;
                    term::say_err(format_args!("mu: draw failed: {e}\n"));
                }
                Err(_) => {}
            }
        }
        let due = app.screen.last_draw + FRAME;
        let cache_expiry = app.cache_expires_in();
        tokio::select! {
            inp = reader.next(), if !app.screen.child_busy => match inp {
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
            Some(p) = async { match pushed.as_mut() { Some(r) => r.recv().await, None => std::future::pending().await } } => app.on_pushed(p),
            _ = tick.tick(), if app.busy() => app.on_tick(),
            _ = clock.tick(), if app.schedule_pending() => app.screen.dirty = true,
            // The footer turns `cache cold` when the cache's TTL passes, with nothing else to redraw it.
            () = async { match cache_expiry { Some(d) => tokio::time::sleep(d).await, None => std::future::pending().await } } => app.screen.dirty = true,
            _ = tokio::time::sleep_until(due.into()), if app.screen.dirty => {}
        }
        app.sync_key_flow();
    }
    // Lines committed in the last moments (a final reply, the exit notice) are drawn before the
    // terminal is handed back. A terminal that hung up takes neither the last frame nor the
    // farewell: writes to it fail, and this is exactly the path that used to abort on the panic
    // from a failed `eprint!`.
    if !term::is_lost() {
        if let Err(e) = app.draw()
            && term::hangup(&e)
        {
            term::mark_lost();
        }
        let _ = app.screen.renderer.finish();
        if !term::is_lost()
            && let Some(path) = app.agent.status.session_path.as_deref()
            && let Ok(host::sessions::Exists { exists: true, profile_flag: profile }) =
                host::call(app.host.as_ref(), host::sessions::SESSION_EXISTS, &path).await
        {
            let id = std::path::Path::new(path)
                .file_stem()
                .and_then(|s| s.to_str())
                .and_then(|s| s.rsplit_once('_'))
                .map_or(path, |(_, id)| id);
            term::say_err(format_args!("\r\n\x1b[2mResume this session with\r\nmu {profile}--resume {id}\x1b[0m\r\n"));
        }
    }
    0
}

impl App {
    /// Whether the screen needs redrawing on a timer: something is running, or the jobs panel is open
    /// (it follows sessions and clocks that change without any event).
    fn busy(&self) -> bool {
        self.agent.busy() || matches!(self.overlay, Some(Overlay::Jobs { .. }))
    }

    fn width(&self) -> usize {
        self.screen.renderer.size().0
    }

    fn commit(&mut self, lines: Vec<Line>) {
        self.agent.tr.commit(lines);
        self.screen.dirty = true;
    }

    fn notice(&mut self, s: &str) {
        self.commit(notice_lines(s));
    }

    fn error(&mut self, s: &str) {
        self.commit(error_lines(s));
    }

    fn send(&mut self, c: Command) {
        #[cfg(test)]
        let c = if mu_base::wire::loopback_on() { mu_base::wire::roundtrip_command(c) } else { c };
        let _ = self.link.cmd_tx.send(c);
    }

    fn spin(&self) -> char {
        SPINNER[(self.screen.started.elapsed().as_millis() / 80) as usize % SPINNER.len()]
    }

    /// Rings the terminal (BEL, plus a toast where supported) when the setting is on.
    fn ring(&self) {
        if self.screen.notify {
            let name =
                self.agent.status.title.clone().unwrap_or_else(|| {
                    self.cwd.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
                });
            term::notify(&format!("mu: {name}"));
        }
    }

    fn quit(&mut self) {
        if self.quitting {
            return;
        }
        self.quitting = true;
        match self.schedule.clear() {
            0 => {}
            n => self.notice(&format!("{n} scheduled prompt(s) dropped")),
        }
        self.send(Command::Cancel);
        // What this client forwarded is its own to take back.
        transfer::unforward_now(self.forwards.take_all());
        self.notice("exiting…");
        self.send(Command::Exit);
    }

    /// Queued inputs waiting for the agent, one row each; the newest gets a hint to edit it.
    fn queue_bar(&self, w: usize) -> Vec<Line> {
        let ib = &self.agent.queued;
        let mut out: Vec<Line> = ib
            .steer
            .iter()
            .map(|s| ("steer", s))
            .chain(ib.follow_up.iter().map(|s| ("after turn", s)))
            .map(|(k, s)| styled(trunc(&format!("↳ {k}: {}", s.text.replace('\n', " ")), w), |_| dim()))
            .collect();
        if let Some(last) = out.last_mut() {
            let used: usize = line_text(last).chars().count();
            if used + 15 <= w {
                last.push(span("  shift+↑ edit", dim()));
            }
        }
        out
    }

    fn cache_expires_in(&self) -> Option<Duration> {
        status::cache_expires_in(self.agent.status.cache.as_ref()?, mu_base::message::now_ms())
    }

    fn draw(&mut self) -> std::io::Result<()> {
        let w = self.width();
        self.agent.tr.advance_reveal(w);
        self.agent.refresh_call_args();
        let (w, rows) = self.screen.renderer.size();
        let mut vp: Vec<Line> = Vec::new();
        let (live, pty_cursor) = self.agent.live_lines(w, self.spin());
        vp.extend(live);
        vp.extend(self.agent.todo_hud(w));
        vp.extend(self.queue_bar(w));
        vp.extend(self.schedule_bar(w));
        let mut env = status::StatusEnv::new(&self.cwd);
        env.expanded = self.agent.expanded;
        env.turn_elapsed = self.agent.turn_started.map(|t| t.elapsed());
        env.jobs = status::JobCounts::of(&self.agent.jobs);
        env.subagent_cost = self.agent.subagent_cost;
        env.levels = self.screen.status_levels;
        let status_lines = status::render(&self.agent.status, &env, w);
        let pty_open = self.agent.pty.is_some();
        let (lines, cur) = match &mut self.overlay {
            _ if pty_open => (vec![styled("  keys go to the command · Esc kills it".to_string(), |_| dim())], (0, 0)),
            Some(ov) => ov.render(w, rows, vp.len() + status_lines.len()),
            None => self.prompt.composer.render(w),
        };
        let cursor = pty_cursor.filter(|_| pty_open).unwrap_or((vp.len() + cur.0, cur.1));
        vp.extend(lines);
        if self.ctrl_c_armed {
            vp.push(vec![span(CTRL_C_HINT.to_string(), theme::get().fg(Role::Faint))]);
        }
        vp.extend(status_lines);
        let committed = std::mem::take(&mut self.agent.tr.pending);
        if let Err(e) = self.screen.renderer.draw(&committed, &vp, cursor) {
            // Nothing reached the terminal: keep the lines for the next try, and wait a frame
            // before it so a persistent failure cannot spin the loop.
            self.agent.tr.pending = committed;
            self.screen.last_draw = Instant::now();
            return Err(e);
        }
        self.screen.last_draw = Instant::now();
        self.screen.dirty = self.agent.tr.revealing();
        Ok(())
    }
}
