//! The TUI's event loop and the state it runs on. The method groups live in the sibling files:
//! `agent_view` (the agent-event reducer), `ui_msgs` (results of background work and the tick),
//! `slash` (what the composer submits), `pickers` (overlays), `child` (programs that take the
//! terminal), `keys` (input routing and API keys), `mcp` (`/mcp`), and `services` (the writes that
//! leave the screen: config, stored keys, MCP definitions, session control).

mod agent_view;
mod child;
mod keys;
mod mcp;
mod pickers;
mod services;
mod slash;
mod ui_msgs;

use self::agent_view::{AgentView, error_lines, notice_lines};
use self::keys::KeyFlow;
use self::services::Services;
use self::slash::command_entries;
use self::ui_msgs::UiMsg;
use crate::chat::composer::Composer;
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
use mu_base::text::truncate_text as trunc;
use mu_common::paths;
use mu_provider::provider::client::Providers;
use mu_tools::context::Context;
use mu_tools::plugin::host::PluginHost;
use serde_json::json;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
const FRAME: Duration = Duration::from_millis(33);

pub struct AppInit {
    pub cmd_tx: UnboundedSender<Command>,
    pub ev_rx: UnboundedReceiver<AgentEvent>,
    pub providers: Rc<Providers>,
    pub ctx: Rc<Context>,
    pub plugins: Rc<PluginHost>,
    pub notice_rx: UnboundedReceiver<String>,
    pub cwd: PathBuf,
    pub pick_session: bool,
    pub status_cfg: mu_base::config::OmpStatusLine,
    pub theme: mu_base::config::MuTheme,
    /// Ring the terminal at the end of a turn nobody typed (omp's `completion.notify`).
    pub notify: bool,
}

struct App {
    agent: AgentView,
    services: Services,
    link: Link,
    ctx: Rc<Context>,
    plugins: Rc<PluginHost>,
    ui_tx: UnboundedSender<UiMsg>,
    cwd: PathBuf,
    screen: Screen,
    prompt: Prompt,
    overlay: Option<Overlay>,
    flows: Flows,
    quitting: bool,
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
    /// A child process (editor, `omp login`) owns the terminal: no drawing and no input reading
    /// until it exits, but agent and plugin tasks keep running.
    child_busy: bool,
    started: Instant,
    status_cfg: mu_base::config::OmpStatusLine,
    notify: bool,
}

/// The composer, its completion list, the prediction asked for, and input held back while a key
/// is typed.
struct Prompt {
    composer: Composer,
    commands: Vec<Entry>,
    ghost_seq: u64,
    ghost_asked: String,
    /// Queued inputs that could not go back into the composer while a key was typed.
    held_back: Vec<UserInput>,
}

/// Multi-step interactions that take over input.
struct Flows {
    /// `/model` assigns roles; `/switch` only changes this session.
    pick_assign: bool,
    pick_deliver: bool,
    /// An `/mcp add` wizard is collecting answers; the composer's lines go to it.
    mcp_wizard: Option<Wizard>,
    /// An API-key flow in progress; while it types, the composer takes its text.
    key_flow: Option<KeyFlow>,
    key_flow_seq: u64,
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
    let commands = command_entries(&plugins, &ctx);
    let show_thinking = !providers.cfg.omp.hide_thinking_block;
    let smooth = providers.cfg.omp.display.smooth_streaming;
    let mut app = App {
        agent: AgentView::new(show_thinking, smooth),
        services: Services { providers },
        link: Link { cmd_tx },
        ctx,
        plugins,
        ui_tx,
        cwd: cwd.clone(),
        screen: Screen {
            renderer: Renderer::new(),
            last_draw: Instant::now() - FRAME,
            dirty: true,
            child_busy: false,
            started: Instant::now(),
            status_cfg,
            notify,
        },
        prompt: Prompt {
            composer: Composer::new(Some(paths::mu_home().join("history.jsonl"))).with_cwd(&cwd),
            commands,
            ghost_seq: 0,
            ghost_asked: String::new(),
            held_back: Vec::new(),
        },
        overlay: None,
        flows: Flows { pick_assign: false, pick_deliver: false, mcp_wizard: None, key_flow: None, key_flow_seq: 0 },
        quitting: false,
    };
    let store_warning = app.services.providers.store().permission_warning();
    let store_check = app.services.providers.store().check().err().map(|e| e.to_string());
    for w in
        theme_warnings.iter().chain(app.plugins.warnings.iter()).chain(store_warning.iter()).chain(store_check.iter())
    {
        app.agent.tr.pending.push(styled(w.clone(), |_| theme::get().fg(Role::Warn)));
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
        if app.screen.dirty
            && !app.screen.child_busy
            && Instant::now() >= app.screen.last_draw + FRAME
            && let Err(e) = app.draw()
            && !draw_failed
        {
            // Said once: stderr is the terminal, and repeating it would flood the screen.
            draw_failed = true;
            eprintln!("mu: draw failed: {e}");
        }
        let due = app.screen.last_draw + FRAME;
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
            _ = tick.tick(), if app.busy() => app.on_tick(),
            _ = tokio::time::sleep_until(due.into()), if app.screen.dirty => {}
        }
        app.sync_key_flow();
    }
    // Lines committed in the last moments (a final reply, the exit notice) are drawn before the
    // terminal is handed back.
    let _ = app.draw();
    let _ = app.screen.renderer.finish();
    if let Some(path) = app.agent.status.session_path.as_deref().filter(|p| std::path::Path::new(p).exists()) {
        let id = std::path::Path::new(path)
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.rsplit_once('_'))
            .map_or(path, |(_, id)| id);
        let profile = mu_session::session::profile_flag();
        eprint!("\r\n\x1b[2mResume this session with\r\nmu {profile}--resume {id}\x1b[0m\r\n");
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
        self.send(Command::Cancel);
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
            if used + 12 <= w {
                last.push(span("  alt+↑ edit", dim()));
            }
        }
        out
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
        let mut env = status::StatusEnv::new(&self.cwd, &self.screen.status_cfg);
        env.expanded = self.agent.expanded;
        env.turn_elapsed = self.agent.turn_started.map(|t| t.elapsed());
        env.jobs = status::JobCounts::of(&self.agent.jobs);
        let status_lines = status::render(&self.agent.status, &env, w);
        let pty_open = self.agent.pty.is_some();
        let (lines, cur) = match &mut self.overlay {
            _ if pty_open => {
                (vec![styled("  keys go to the command · ctrl+c twice force-kills".to_string(), |_| dim())], (0, 0))
            }
            Some(ov) => ov.render(w, rows, vp.len() + status_lines.len()),
            None => self.prompt.composer.render(w),
        };
        let cursor = pty_cursor.filter(|_| pty_open).unwrap_or((vp.len() + cur.0, cur.1));
        vp.extend(lines);
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
