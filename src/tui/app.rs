use crate::image;
use super::blocks;
use super::clipboard;
use super::composer::{Action, Composer};
use super::line::{Line, dim, line_text, span, styled};
use super::markdown;
use super::render::Renderer;
use super::select::{Item, Select, SelectResult};
use super::status;
use super::term::{self, Term};
use super::theme;
use crate::context::Context;
use crate::events::{Command, SharedInbox};
use crate::events::{AgentEvent, Answer, AskQuestion, PlanDecision, StatusSnapshot, SubagentState, SubagentStatus, UserInput};
use crate::message::Image;
use crate::models::{GatewayModel, Thinking};
use crate::plugin::PluginHost;
use crate::provider::Providers;
use crate::tools::todo::{Phase, Status};
use super::input::{self, Input};
use crossterm::event::{Event, KeyEvent};
use mu_common::paths;
use serde_json::Value;
use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::sync::oneshot;

const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
const FRAME: Duration = Duration::from_millis(33);
const BUILTINS: [&str; 19] = [
    "exit", "quit", "new", "resume", "model", "switch", "login", "thinking", "compact", "plan", "session", "rename", "todo", "skills", "plugins", "hotkeys",
    "copy", "image", "jobs",
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
}

enum Overlay {
    Model { sel: Select, ids: Vec<String>, assign: bool },
    /// Where a picked model goes: this session, or a role in omp's config.
    Role { sel: Select, model: String, roles: Vec<Option<String>> },
    /// `omp login` providers as (menu number, name).
    Login { sel: Select, nums: Vec<usize> },
    Session { sel: Select, paths: Vec<PathBuf> },
    Ask { qs: Vec<AskQuestion>, idx: usize, answers: Vec<Answer>, sel: Select, reply: Option<oneshot::Sender<Option<Vec<Answer>>>> },
    Plan { sel: Select, reply: Option<oneshot::Sender<PlanDecision>> },
    Copy { sel: Select, texts: Vec<String> },
}

enum UiMsg {
    Models(Result<Vec<GatewayModel>, String>),
    LoginProviders(Result<Vec<(usize, String)>, String>),
    ClipboardImage(Result<Option<Image>, String>),
}

struct RunningTool {
    id: String,
    name: String,
    args: Value,
    started: Instant,
    tail: VecDeque<String>,
    partial: String,
}

struct App {
    cmd_tx: UnboundedSender<Command>,
    inbox: SharedInbox,
    providers: Rc<Providers>,
    ctx: Rc<Context>,
    plugins: Rc<PluginHost>,
    ui_tx: UnboundedSender<UiMsg>,
    cwd: PathBuf,
    status_cfg: crate::config::OmpStatusLine,
    turn_started: Option<Instant>,
    /// `/model` assigns roles; `/switch` only changes this session.
    pick_assign: bool,
    renderer: Renderer,
    composer: Composer,
    commands: Vec<String>,
    pending: Vec<Line>,
    live_text: String,
    committed_in_block: bool,
    thinking: Option<(Instant, String)>,
    tools: Vec<RunningTool>,
    /// Latest background-job snapshot, and finished jobs already printed to the transcript.
    jobs: Vec<SubagentStatus>,
    jobs_committed: HashSet<String>,
    /// Assistant text blocks (newest last, capped) and bash commands, for `/copy`.
    replies: Vec<String>,
    block_text: String,
    bash_cmds: Vec<String>,
    compacting: bool,
    todo: Vec<Phase>,
    status: StatusSnapshot,
    overlay: Option<Overlay>,
    running: bool,
    quitting: bool,
    show_thinking: bool,
    expanded: bool,
    dirty: bool,
    last_draw: Instant,
    started: Instant,
}

fn push_capped<T>(v: &mut Vec<T>, item: T) {
    if v.len() == 50 {
        v.remove(0);
    }
    v.push(item);
}

fn trunc(s: &str, n: usize) -> String {
    if s.chars().count() > n { s.chars().take(n.saturating_sub(1)).chain(['…']).collect() } else { s.into() }
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
    let AppInit { cmd_tx, mut ev_rx, inbox, providers, ctx, plugins, mut notice_rx, cwd, pick_session, status_cfg } = init;
    let (ui_tx, mut ui_rx) = unbounded_channel();
    let mut commands: Vec<String> = BUILTINS.iter().map(|s| s.to_string()).collect();
    commands.extend(ctx.commands.iter().map(|c| c.name.clone()));
    commands.extend(ctx.skills.iter().map(|s| format!("skill:{}", s.name)));
    let mut app = App {
        cmd_tx,
        inbox,
        providers,
        ctx,
        plugins,
        ui_tx,
        status_cfg,
        turn_started: None,
        pick_assign: false,
        cwd,
        renderer: Renderer::new(),
        composer: Composer::new(Some(paths::mu_home().join("history.jsonl"))),
        commands,
        pending: Vec::new(),
        live_text: String::new(),
        committed_in_block: false,
        thinking: None,
        tools: Vec::new(),
        jobs: Vec::new(),
        jobs_committed: HashSet::new(),
        replies: Vec::new(),
        block_text: String::new(),
        bash_cmds: Vec::new(),
        compacting: false,
        todo: Vec::new(),
        status: StatusSnapshot::default(),
        overlay: None,
        running: false,
        quitting: false,
        show_thinking: false,
        expanded: false,
        dirty: true,
        last_draw: Instant::now() - FRAME,
        started: Instant::now(),
    };
    for w in &app.plugins.warnings {
        app.pending.push(styled(w.clone(), |_| theme::get().fg("warning")));
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
    loop {
        let due = app.last_draw + FRAME;
        if app.dirty && Instant::now() >= due {
            if let Err(e) = app.draw() {
                eprintln!("mu: draw failed: {e}");
            }
        }
        tokio::select! {
            inp = reader.next() => match inp {
                Some(Input::Term(e)) => app.on_terminal(e),
                Some(Input::PasteText(s)) => app.on_terminal(Event::Paste(s)),
                Some(Input::PasteImage(img)) => app.on_ui(UiMsg::ClipboardImage(Ok(Some(img)))),
                Some(Input::Status(s)) => app.notice(&s),
                None => break,
            },
            e = ev_rx.recv() => match e {
                Some(AgentEvent::Exit) | None => break,
                Some(e) => app.on_agent(e),
            },
            Some(n) = notice_rx.recv() => app.notice(&n),
            Some(m) = ui_rx.recv() => app.on_ui(m),
            _ = tick.tick(), if app.busy() => app.dirty = true,
            _ = tokio::time::sleep_until(due.into()), if app.dirty && Instant::now() < due => {}
        }
    }
    let _ = app.renderer.finish();
    if let Some(path) = app.status.session_path.as_deref().filter(|p| std::path::Path::new(p).exists()) {
        let id = std::path::Path::new(path).file_stem().and_then(|s| s.to_str()).and_then(|s| s.rsplit_once('_')).map_or(path, |(_, id)| id);
        let profile = std::env::var("OMP_PROFILE").ok().filter(|p| !p.is_empty()).map(|p| format!("--profile {p} ")).unwrap_or_default();
        eprint!("\r\n\x1b[2mResume this session with\r\nmu {profile}--resume {id}\x1b[0m\r\n");
    }
    0
}

impl App {
    fn busy(&self) -> bool {
        self.running || self.compacting || self.thinking.is_some() || !self.tools.is_empty() || self.jobs.iter().any(|j| j.state == SubagentState::Running)
    }

    fn width(&self) -> usize {
        self.renderer.size().0
    }

    fn commit(&mut self, lines: Vec<Line>) {
        self.pending.extend(lines);
        self.dirty = true;
    }

    fn notice(&mut self, s: &str) {
        let l = s.lines().map(|l| styled(l.to_string(), |_| dim())).collect();
        self.commit(l);
    }

    fn error(&mut self, s: &str) {
        let l = s.lines().map(|l| styled(l.to_string(), |_| theme::get().fg("error"))).collect();
        self.commit(l);
    }

    fn send(&mut self, c: Command) {
        let _ = self.cmd_tx.send(c);
    }

    // ---- agent events -------------------------------------------------------------------------

    fn finish_thinking(&mut self) {
        let Some((start, text)) = self.thinking.take() else { return };
        if self.show_thinking && !text.trim().is_empty() {
            let w = self.width();
            self.commit(blocks::thinking_body(text.trim(), w));
        } else {
            self.commit(vec![blocks::thinking_done(start.elapsed().as_secs())]);
        }
    }

    fn commit_text(&mut self, text: &str, blank_after: bool) {
        let w = self.width();
        let mut lines = markdown::render(text, w);
        if blank_after {
            lines.push(vec![]);
        }
        self.committed_in_block = true;
        self.commit(lines);
    }

    fn on_agent(&mut self, e: AgentEvent) {
        self.dirty = true;
        match e {
            AgentEvent::TurnStart => {
                self.running = true;
                self.turn_started = Some(Instant::now());
            }
            AgentEvent::UserPrompt(t) => {
                let w = self.width();
                let mut l = vec![vec![]];
                l.extend(blocks::user_prompt(&t, w));
                self.commit(l);
            }
            AgentEvent::Text(s) => {
                self.finish_thinking();
                self.live_text.push_str(&s);
                self.block_text.push_str(&s);
                let n = markdown::stable_prefix(&self.live_text);
                if n > 0 {
                    let chunk: String = self.live_text.drain(..n).collect();
                    self.commit_text(chunk.trim_end(), true);
                }
            }
            AgentEvent::Thinking(s) => match self.thinking.as_mut() {
                Some((_, t)) => t.push_str(&s),
                None => self.thinking = Some((Instant::now(), s)),
            },
            AgentEvent::BlockEnd => {
                self.finish_thinking();
                if !self.live_text.trim().is_empty() {
                    let t = std::mem::take(&mut self.live_text);
                    self.commit_text(t.trim_end(), false);
                }
                self.live_text.clear();
                let block = std::mem::take(&mut self.block_text);
                if !block.trim().is_empty() {
                    push_capped(&mut self.replies, block);
                }
                if self.committed_in_block {
                    self.commit(vec![vec![]]);
                }
                self.committed_in_block = false;
            }
            AgentEvent::ToolStart { id, name, args } => {
                self.finish_thinking();
                if name == "bash" {
                    if let Some(c) = args["command"].as_str() {
                        push_capped(&mut self.bash_cmds, c.to_string());
                    }
                }
                self.tools.push(RunningTool { id, name, args, started: Instant::now(), tail: VecDeque::new(), partial: String::new() });
            }
            AgentEvent::ToolOutput { id, chunk } => {
                if let Some(t) = self.tools.iter_mut().find(|t| t.id == id) {
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
                let args = self.tools.iter().position(|t| t.id == id).map(|i| self.tools.remove(i).args).unwrap_or(Value::Null);
                let w = self.width();
                let l = blocks::tool_result(&name, &args, &output, self.expanded, w);
                self.commit(l);
            }
            AgentEvent::Retry { attempt, delay, reason } => {
                self.live_text.clear();
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
                self.jobs = v;
            }
            AgentEvent::JobsDelivered(ids) => {
                self.commit(vec![styled(format!("↳ job results delivered: {}", ids.join(", ")), |_| dim())]);
            }
            AgentEvent::Ask { questions, reply } => self.start_ask(questions, reply),
            AgentEvent::PlanReview { title, path, reply } => {
                let sel = Select::new(
                    &format!("Plan: {title} ({path})"),
                    vec![
                        Item { label: "Approve and execute".into(), detail: "new session, run the plan".into() },
                        Item { label: "Approve and keep context".into(), detail: "implement in this session".into() },
                    ],
                )
                .free_text("Refine plan");
                self.overlay = Some(Overlay::Plan { sel, reply: Some(reply) });
            }
            AgentEvent::Status(s) => {
                self.status = s;
            }
            AgentEvent::Todo(v) => self.todo = serde_json::from_value(v).unwrap_or_default(),
            AgentEvent::TurnEnd { aborted } => {
                self.finish_thinking();
                if !self.live_text.trim().is_empty() {
                    let t = std::mem::take(&mut self.live_text);
                    self.commit_text(t.trim_end(), false);
                }
                self.live_text.clear();
                self.tools.clear();
                self.compacting = false;
                self.running = false;
                self.turn_started = None;
                if aborted {
                    self.notice("[aborted]");
                    let queued: Vec<UserInput> = {
                        let mut ib = self.inbox.borrow_mut();
                        let mut q: Vec<UserInput> = ib.steer.drain(..).collect();
                        q.extend(ib.follow_up.drain(..));
                        q
                    };
                    if !queued.is_empty() {
                        self.composer.restore(queued);
                    }
                }
            }
            AgentEvent::Exit => {}
        }
    }

    fn on_ui(&mut self, m: UiMsg) {
        self.dirty = true;
        match m {
            UiMsg::Models(r) => {
                let gw = match r {
                    Ok(g) => g,
                    Err(e) => {
                        self.error(&format!("could not fetch gateway models: {e}"));
                        vec![]
                    }
                };
                self.open_model_picker(gw);
            }
            UiMsg::LoginProviders(r) => match r {
                Ok(list) if !list.is_empty() => {
                    let items = list.iter().map(|(_, n)| Item { label: n.clone(), detail: String::new() }).collect();
                    let nums = list.iter().map(|(n, _)| *n).collect();
                    self.overlay = Some(Overlay::Login { sel: Select::new("Log in to provider", items), nums });
                }
                Ok(_) => self.error("omp login listed no providers"),
                Err(e) => self.error(&format!("cannot list login providers: {e}")),
            },
            UiMsg::ClipboardImage(r) => match r {
                Ok(Some(img)) if self.overlay.is_none() => self.composer.attach_image(img),
                Ok(Some(_)) => self.notice("image paste ignored in picker"),
                Ok(None) => self.notice("clipboard has no image"),
                Err(e) => self.notice(&e),
            },
        }
    }

    // ---- ask / plan overlays ------------------------------------------------------------------

    fn ask_select(q: &AskQuestion) -> Select {
        let items: Vec<Item> = q
            .options
            .iter()
            .enumerate()
            .map(|(i, o)| Item {
                label: if q.recommended == Some(i) { format!("{} (Recommended)", o.label) } else { o.label.clone() },
                detail: o.description.clone().unwrap_or_default(),
            })
            .collect();
        let title = q.header.as_ref().map_or(q.question.clone(), |h| format!("{h}: {}", q.question));
        let mut s = Select::new(&title, items).free_text("Other (type your own)");
        if q.multi {
            s = s.multi();
        }
        if let Some(r) = q.recommended {
            s = s.preselect(r);
        }
        s
    }

    fn start_ask(&mut self, qs: Vec<AskQuestion>, reply: oneshot::Sender<Option<Vec<Answer>>>) {
        if qs.is_empty() {
            let _ = reply.send(Some(vec![]));
            return;
        }
        let sel = Self::ask_select(&qs[0]);
        self.overlay = Some(Overlay::Ask { qs, idx: 0, answers: vec![], sel, reply: Some(reply) });
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
        for g in &gateway {
            items.push(Item { label: g.id.clone(), detail: g.context_length.map_or(String::new(), |c| format!("{}k ctx", c / 1000)) });
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
        let title = if self.pick_assign { "Select model (then choose where it applies)" } else { "Switch model (this session)" };
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
            items.push(Item { label: if n == "default" { "Set as @default (and switch now)".into() } else { format!("Set as @{n}") }, detail });
            roles.push(Some(n));
        }
        self.overlay = Some(Overlay::Role { sel: Select::new(&format!("Use {model}"), items), model, roles });
    }

    fn open_session_picker(&mut self) {
        let list = crate::session::list_for(&self.cwd);
        if list.is_empty() {
            self.notice("no sessions for this directory");
            return;
        }
        let items = list
            .iter()
            .map(|s| Item { label: s.title.clone().unwrap_or_else(|| trunc(&s.first_prompt, 80)), detail: ago(s.modified) })
            .collect();
        let paths = list.into_iter().map(|s| s.path).collect();
        self.overlay = Some(Overlay::Session { sel: Select::new("Resume session", items), paths });
    }

    // ---- input --------------------------------------------------------------------------------

    fn on_terminal(&mut self, e: Event) {
        self.dirty = true;
        match e {
            Event::Key(k) => self.on_key(k),
            Event::Paste(s) => match self.overlay.as_mut() {
                Some(Overlay::Model { sel, .. } | Overlay::Role { sel, .. } | Overlay::Login { sel, .. } | Overlay::Session { sel, .. } | Overlay::Ask { sel, .. } | Overlay::Plan { sel, .. } | Overlay::Copy { sel, .. }) => {
                    sel.on_paste(&s)
                }
                None => {
                    if let Err(e) = self.composer.on_paste(&s) {
                        self.error(&format!("image: {e}"));
                    }
                }
            },
            _ => {}
        }
    }

    fn on_key(&mut self, k: KeyEvent) {
        if self.overlay.is_some() {
            return self.overlay_key(k);
        }
        let cmds = self.commands.clone();
        match self.composer.on_key(k, self.running, &cmds) {
            Action::None | Action::Redraw => {}
            Action::Submit(t) => self.submit(t, false),
            Action::QueueFollowUp(t) => self.submit(t, true),
            Action::Abort => self.inbox.borrow().cancel.cancel(),
            Action::Exit => self.quit(),
            Action::CycleThinking => {
                let cur = Thinking::parse(&self.status.thinking).unwrap_or(Thinking::Medium);
                self.send(Command::SetThinking(cur.cycle()));
            }
            Action::ToggleThinking => self.show_thinking = !self.show_thinking,
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
                    self.composer.restore(vec![p]);
                }
            }
        }
    }

    fn overlay_key(&mut self, k: KeyEvent) {
        let Some(ov) = self.overlay.take() else { return };
        match ov {
            Overlay::Model { mut sel, ids, assign } => match sel.on_key(k) {
                SelectResult::Pending => self.overlay = Some(Overlay::Model { sel, ids, assign }),
                SelectResult::Chosen(i) => {
                    if let Some(id) = i.first().and_then(|i| ids.get(*i)) {
                        if assign && !id.starts_with('@') {
                            self.open_role_picker(id.clone());
                        } else {
                            self.send(Command::SetModel(id.clone()));
                        }
                    }
                }
                _ => {}
            },
            Overlay::Role { mut sel, model, roles } => match sel.on_key(k) {
                SelectResult::Pending => self.overlay = Some(Overlay::Role { sel, model, roles }),
                SelectResult::Chosen(i) => match i.first().and_then(|i| roles.get(*i)) {
                    Some(Some(role)) => match self.providers.cfg.set_role(role, &model) {
                        Ok(v) => {
                            self.notice(&format!("@{role} = {v}"));
                            if role == "default" {
                                self.send(Command::SetModel(model));
                            }
                        }
                        Err(e) => self.error(&format!("could not set @{role}: {e}")),
                    },
                    Some(None) => self.send(Command::SetModel(model)),
                    None => {}
                },
                _ => {}
            },
            Overlay::Login { mut sel, nums } => match sel.on_key(k) {
                SelectResult::Pending => self.overlay = Some(Overlay::Login { sel, nums }),
                SelectResult::Chosen(i) => {
                    if let Some(n) = i.first().and_then(|i| nums.get(*i)) {
                        self.run_login(&[], Some(*n));
                    }
                }
                _ => {}
            },
            Overlay::Copy { mut sel, texts } => match sel.on_key(k) {
                SelectResult::Pending => self.overlay = Some(Overlay::Copy { sel, texts }),
                SelectResult::Chosen(i) => {
                    let t = i.first().and_then(|i| texts.get(*i)).cloned();
                    self.copy_text(t, "nothing to copy");
                }
                _ => {}
            },
            Overlay::Session { mut sel, paths } => match sel.on_key(k) {
                SelectResult::Pending => self.overlay = Some(Overlay::Session { sel, paths }),
                SelectResult::Chosen(i) => {
                    if let Some(p) = i.first().and_then(|i| paths.get(*i)) {
                        self.send(Command::Resume(p.clone()));
                    }
                }
                _ => {}
            },
            Overlay::Plan { mut sel, mut reply } => {
                let decision = match sel.on_key(k) {
                    SelectResult::Pending => {
                        self.overlay = Some(Overlay::Plan { sel, reply });
                        return;
                    }
                    SelectResult::Chosen(i) if i.first() == Some(&0) => PlanDecision::Execute,
                    SelectResult::Chosen(_) => PlanDecision::Keep,
                    SelectResult::Text(t) => PlanDecision::Refine(t),
                    SelectResult::Cancelled => PlanDecision::Refine(String::new()),
                };
                if let Some(r) = reply.take() {
                    let _ = r.send(decision);
                }
            }
            Overlay::Ask { qs, idx, mut answers, mut sel, mut reply } => {
                let ans = match sel.on_key(k) {
                    SelectResult::Pending => {
                        self.overlay = Some(Overlay::Ask { qs, idx, answers, sel, reply });
                        return;
                    }
                    SelectResult::Chosen(i) => {
                        let labels = i.iter().filter_map(|i| qs[idx].options.get(*i)).map(|o| o.label.clone()).collect();
                        Answer::Options(labels)
                    }
                    SelectResult::Text(t) => Answer::Custom(t),
                    SelectResult::Cancelled => {
                        let mut all = answers;
                        all.extend(std::iter::repeat(Answer::Cancelled).take(qs.len() - all.len()));
                        if let Some(r) = reply.take() {
                            let _ = r.send(Some(all));
                        }
                        return;
                    }
                };
                answers.push(ans);
                if idx + 1 < qs.len() {
                    let sel = Self::ask_select(&qs[idx + 1]);
                    self.overlay = Some(Overlay::Ask { qs, idx: idx + 1, answers, sel, reply });
                } else if let Some(r) = reply.take() {
                    let _ = r.send(Some(answers));
                }
            }
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
        let allowed_busy = matches!(name, "thinking" | "model" | "todo" | "skills" | "plugins" | "session" | "copy" | "image" | "jobs") && !(name == "model" && arg.is_empty());
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
            "thinking" => match Thinking::parse(arg) {
                Some(t) => self.send(Command::SetThinking(t)),
                None => self.error("usage: /thinking off|low|medium|high"),
            },
            "plan" => self.send(Command::TogglePlan((!arg.is_empty()).then(|| arg.to_string()))),
            "compact" => {
                self.running = true;
                self.send(Command::Compact((!arg.is_empty()).then(|| arg.to_string())));
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
                    .map(|(n, l, up)| format!("{n} · {l} · {}", if up { "running" } else { "stopped" }))
                    .collect();
                self.notice(&if p.is_empty() { "no plugins".into() } else { p.join("\n") });
            }
            "hotkeys" => self.notice(HOTKEYS),
            "copy" => self.copy_cmd(arg),
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
            (None, ..) => {
                if self.jobs.is_empty() {
                    return self.notice("no background jobs");
                }
                let rows: Vec<String> = self
                    .jobs
                    .iter()
                    .map(|j| {
                        let (mark, state) = match j.state {
                            SubagentState::Running => ('◌', "running"),
                            SubagentState::Done => ('✓', "done"),
                            SubagentState::Failed => ('✗', "failed"),
                        };
                        format!("{mark} {} ({}) · {} tools · {state}", j.name, j.agent, j.tools)
                    })
                    .collect();
                self.notice(&rows.join("\n"));
            }
            (Some("kill"), Some("all"), None) => self.send(Command::KillJobs(None)),
            (Some("kill"), Some(id), None) => self.send(Command::KillJobs(Some(id.into()))),
            _ => self.error("usage: /jobs [kill <id|all>]"),
        }
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
                let t = self.replies.iter().rev().find_map(|r| markdown::code_blocks(r).pop().map(|(_, b)| b));
                self.copy_text(t, "No code block to copy.");
            }
            "cmd" | "command" => {
                let t = self.bash_cmds.last().cloned();
                self.copy_text(t, "No command to copy.");
            }
            "link" | "url" => {
                let t = self.replies.iter().rev().find_map(|r| markdown::links(r).pop());
                self.copy_text(t, "No link to copy.");
            }
            _ => self.error("usage: /copy [code|cmd|link]"),
        }
    }

    fn copy_text(&mut self, text: Option<String>, none: &str) {
        let Some(t) = text else { return self.notice(none) };
        match clipboard::copy(&t) {
            Ok(()) => self.notice(&format!("copied {} chars", t.chars().count())),
            Err(e) => self.error(&format!("copy failed: {e}")),
        }
    }

    fn open_copy_picker(&mut self) {
        let first = |s: &str| s.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim().to_string();
        let mut items = Vec::new();
        let mut texts = Vec::new();
        for r in self.replies.iter().rev().take(20) {
            items.push(Item { label: format!("Reply: {}", trunc(&first(r), 60)), detail: format!("{} lines", r.lines().count()) });
            texts.push(r.clone());
            for (lang, body) in markdown::code_blocks(r) {
                let lang = if lang.is_empty() { "text" } else { &lang };
                items.push(Item { label: format!("Code {lang}: {}", trunc(&first(&body), 50)), detail: format!("{} lines", body.lines().count()) });
                texts.push(body);
            }
        }
        for c in self.bash_cmds.iter().rev().take(10) {
            items.push(Item { label: format!("Command: {}", trunc(&first(c), 60)), detail: format!("{} lines", c.lines().count()) });
            texts.push(c.clone());
        }
        if items.is_empty() {
            return self.notice("nothing to copy yet");
        }
        self.overlay = Some(Overlay::Copy { sel: Select::new("Copy", items), texts });
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

    /// `/login`: with a provider id run `omp login <id>`; otherwise list omp's providers in a picker.
    fn login(&mut self, provider: &str) {
        if !provider.is_empty() {
            return self.run_login(&[provider], None);
        }
        self.notice("listing login providers…");
        let tx = self.ui_tx.clone();
        tokio::task::spawn_local(async move {
            let _ = tx.send(UiMsg::LoginProviders(list_login_providers().await));
        });
    }

    /// Runs `omp login` on the real terminal. With `pick`, omp's own provider menu is swallowed and
    /// answered with that number once its prompt appears; terminal input is then relayed so OAuth
    /// prompts still work. The gateway reads the stored credentials.
    fn run_login(&mut self, args: &[&str], pick: Option<usize>) {
        use std::process::Stdio;
        let _ = self.renderer.finish();
        let _ = term::suspend();
        let mut cmd = std::process::Command::new("omp");
        cmd.arg("login").args(args);
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
        let _ = term::resume();
        self.renderer = Renderer::new();
        match status {
            Ok(s) if s.success() => self.notice("login finished"),
            Ok(s) => self.error(&format!("omp login exited with {s}")),
            Err(e) => self.error(&format!("cannot run omp login: {e}")),
        }
    }

    fn edit_external(&mut self) {
        let editor = std::env::var("VISUAL").or_else(|_| std::env::var("EDITOR")).unwrap_or_else(|_| "vi".into());
        let path = std::env::temp_dir().join(format!("mu-edit-{}.md", std::process::id()));
        if std::fs::write(&path, self.composer.text()).is_err() {
            return self.error("cannot create temp file");
        }
        let _ = self.renderer.finish();
        let _ = term::suspend();
        let status = std::process::Command::new("sh").arg("-c").arg(format!("{editor} \"$1\"")).arg("sh").arg(&path).status();
        let _ = term::resume();
        self.renderer = Renderer::new();
        match (status, std::fs::read_to_string(&path)) {
            (Ok(s), Ok(t)) if s.success() => self.composer.set_text(t.trim_end_matches('\n')),
            (Ok(_), _) => {}
            (Err(e), _) => self.error(&format!("editor failed: {e}")),
        }
        let _ = std::fs::remove_file(&path);
    }

    // ---- drawing ------------------------------------------------------------------------------

    fn spin(&self) -> char {
        SPINNER[(self.started.elapsed().as_millis() / 80) as usize % SPINNER.len()]
    }

    fn live_lines(&self, w: usize) -> Vec<Line> {
        let mut out: Vec<Line> = Vec::new();
        if !self.live_text.is_empty() {
            out.extend(markdown::render(&self.live_text, w));
        }
        if let Some((start, text)) = &self.thinking {
            out.push(blocks::thinking_live(self.spin(), start.elapsed().as_secs()));
            if self.show_thinking {
                let last = text.lines().last().unwrap_or("");
                out.push(styled(format!("  {}", trunc(last, w.saturating_sub(2))), |_| theme::get().fg("thinkingText")));
            }
        }
        for t in &self.tools {
            let mut tail: Vec<String> = t.tail.iter().cloned().collect();
            if !t.partial.is_empty() {
                tail.push(t.partial.clone());
            }
            let tail = &tail[tail.len().saturating_sub(10)..];
            out.extend(blocks::tool_live(&t.name, &t.args, tail, self.spin(), t.started.elapsed().as_secs(), w));
        }
        let running: Vec<SubagentStatus> = self.jobs.iter().filter(|j| j.state == SubagentState::Running).cloned().collect();
        out.extend(blocks::subagent_rows(&running, self.spin()));
        if self.compacting {
            out.push(styled(format!("{} Compacting context…", self.spin()), |_| theme::get().fg("warning")));
        }
        out
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
            out.push(styled(trunc(&format!("  ▸ {}", t.content), w), |_| theme::get().fg("warning")));
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
        let (w, rows) = self.renderer.size();
        let mut vp: Vec<Line> = Vec::new();
        vp.extend(self.live_lines(w));
        vp.extend(self.todo_hud(w));
        vp.extend(self.queue_bar(w));
        let (lines, cur) = match &self.overlay {
            Some(Overlay::Model { sel, .. } | Overlay::Role { sel, .. } | Overlay::Login { sel, .. } | Overlay::Session { sel, .. } | Overlay::Ask { sel, .. } | Overlay::Plan { sel, .. } | Overlay::Copy { sel, .. }) => {
                let (body, cur) = sel.render(w, (rows / 2).max(6).saturating_sub(2));
                let mut framed: Vec<Line> = vec![Vec::new()];
                framed.extend(body);
                (framed, (cur.0 + 1, cur.1))
            }
            None => self.composer.render(w),
        };
        let cursor = (vp.len() + cur.0, cur.1);
        vp.extend(lines);
        let mut env = status::StatusEnv::new(&self.cwd, &self.status_cfg);
        env.expanded = self.expanded;
        env.turn_elapsed = self.turn_started.map(|t| t.elapsed());
        vp.extend(status::render(&self.status, &env, w));
        let committed = std::mem::take(&mut self.pending);
        self.renderer.draw(&committed, &vp, cursor)?;
        self.last_draw = Instant::now();
        self.dirty = false;
        Ok(())
    }
}

const HOTKEYS: &str = "Enter  submit (steer while a turn runs)      Ctrl+Q  queue follow-up      Alt+↑  edit last queued
Shift+Enter / Ctrl+J / Alt+Enter  newline      Esc  abort turn      Ctrl+C  clear · abort · exit (twice)
Shift+Tab  cycle thinking      Ctrl+T  show thinking      Ctrl+O  expand output      Alt+M  model picker
Ctrl+G  edit in $EDITOR      Ctrl+V  paste clipboard image      Tab  complete /command      ↑/↓  history      Ctrl+A/E/K/U/W  line editing
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

/// Provider menu of `omp login`, scraped from its numbered prompt (omp has no machine-readable list).
async fn list_login_providers() -> Result<Vec<(usize, String)>, String> {
    let out = tokio::process::Command::new("omp")
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
        assert_eq!(parse_login_menu(t), vec![(1, "ChatGPT Plus/Pro (Codex)".into()), (12, "Z.AI (GLM · Sign in)".into())]);
    }
}
