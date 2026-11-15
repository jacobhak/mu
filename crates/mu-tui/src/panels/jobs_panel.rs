//! The jobs panel (`Alt+J`, `/jobs`). The Jobs tab lists this session's background jobs with the
//! selected job's details below; the Sessions tab lists every other live mu process. `Enter` opens a
//! full-height view: a subagent's or another session's transcript, followed while it is written, or a
//! bash job's output. See `design/jobs-panel.md`.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::host::results::ResultArgs;
use crate::host::sessions::{Live, LiveArgs, TailArgs, TailDelta};
use crate::panels::reply::rule;
use crate::panels::session_view;
use crate::panels::sessions_tab::{self, Row};
use crate::text::line::{self, Line, dim, span};
use crate::text::{accent, pad, wrap};
use crate::theme::{self, Role};
use crate::widgets::frame::Frame;
use mu_base::events::{JobKind, SubagentState, SubagentStatus};
use mu_base::message::Message;

/// Most list rows shown at once.
const LIST_MAX: usize = 8;
/// Rows the panel needs besides the list and the details: three rules and the key line.
const CHROME: usize = 4;
/// Fewest detail rows worth splitting for; below this only the list is drawn.
const DETAIL_MIN: usize = 3;
/// Most detail rows: the facts, directory, task and last activity fit in this, and a tall screen
/// keeps showing the transcript above the panel.
const DETAIL_MAX: usize = 8;
/// Widest name and agent columns in the list.
const NAME_MAX: usize = 16;
const AGENT_MAX: usize = 12;
/// Newest messages the transcript view keeps; older ones are counted, not drawn.
const VIEW_KEEP: usize = 500;
/// How often the open transcript, and the cost of every listed session, are read again.
const VIEW_POLL: Duration = Duration::from_millis(250);
const TOTALS_POLL: Duration = Duration::from_secs(2);

/// What the panel needs read from the host; the app makes the call and hands the reply back.
pub enum Want {
    Sessions(LiveArgs),
    Tail { target: TailFor, args: TailArgs },
}

/// Whose tail a reply belongs to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TailFor {
    /// The cost column of a listed job or session.
    Total,
    /// The open transcript.
    View,
}

pub enum JobsResult {
    Pending,
    Close,
    /// Cancel one running job by id, or every running job.
    Kill(Option<String>),
    /// Message a live session (`mu notify`).
    Notify {
        session: PathBuf,
        text: String,
    },
    /// Stop a live session. `pid` and `started` name the run the user saw, so a session that changed
    /// hands since is left alone; `kind` is `print`, `subagent` or `tui` (an abort of its current turn).
    Stop {
        session: PathBuf,
        pid: u32,
        started: String,
        kind: String,
    },
    /// Ask an interactive mu to exit (`S`, confirmed).
    Terminate {
        session: PathBuf,
        pid: u32,
        started: String,
    },
    /// Take over a `-p` run (`mu take`).
    Take {
        session: PathBuf,
    },
    /// Show the diff of a launched job's result (`d`).
    Diff {
        job: String,
        result: ResultArgs,
    },
    /// Apply a launched job's result with a three-way merge (`a`).
    Apply {
        job: String,
        result: ResultArgs,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tab {
    Jobs,
    Sessions,
}

pub struct JobsPanel {
    tab: Tab,
    jobs: Vec<SubagentStatus>,
    at: usize,
    top: usize,
    /// Rows the list showed last, for paging.
    page: usize,
    /// The Sessions tab: live mu processes, as a tree, and its own selection and scroll.
    rows: Vec<Row>,
    s_at: usize,
    s_top: usize,
    s_page: usize,
    /// Titles of the listed sessions, read once each.
    titles: HashMap<PathBuf, String>,
    /// This session, which the Sessions tab leaves out (the Jobs tab is its children).
    own: Option<PathBuf>,
    /// Canonical form of the paths the host was asked about (this session, the jobs' sessions).
    canon: HashMap<PathBuf, PathBuf>,
    /// `$HOME` on the host, which the Sessions tab shortens directories with.
    home: Option<String>,
    /// Running totals of each listed session, for the cost column.
    totals: HashMap<PathBuf, Totals>,
    polled_totals: Option<Instant>,
    view: Option<View>,
    /// A message being typed to a session.
    input: Option<Compose>,
    /// A terminate waiting for `y`.
    confirm: Option<Confirm>,
    /// `X` waiting for `y`.
    confirm_kill_all: bool,
    /// What the last action did, shown in place of the key line until the next key.
    status: Option<String>,
    /// Jobs whose result was applied from this panel.
    applied: HashSet<String>,
    /// The host's note under the Sessions list: worker results nobody applied.
    notice: Option<String>,
    /// The newest output of each bash job, as the app last sent it.
    outputs: HashMap<String, String>,
    /// Reads the panel waits to have made, taken by the app after each key and tick.
    wants: Vec<Want>,
    /// The list of live sessions was asked for and has not come back.
    live_asked: bool,
}

/// A session file as far as the host has been read: what `Tail` keeps, built from the host's deltas.
struct Followed {
    path: PathBuf,
    keep: usize,
    offset: u64,
    messages: Vec<Message>,
    dropped: usize,
    cost: f64,
    tokens: u64,
    caught_up: bool,
    /// A read was asked for and has not come back.
    asked: bool,
}

impl Followed {
    fn new(path: PathBuf, keep: usize) -> Self {
        Self {
            path,
            keep,
            offset: 0,
            messages: Vec::new(),
            dropped: 0,
            cost: 0.0,
            tokens: 0,
            caught_up: false,
            asked: false,
        }
    }

    fn messages(&self) -> &[Message] {
        &self.messages
    }

    fn dropped(&self) -> usize {
        self.dropped
    }

    fn cost(&self) -> f64 {
        self.cost
    }

    fn tokens(&self) -> u64 {
        self.tokens
    }

    fn caught_up(&self) -> bool {
        self.caught_up
    }

    /// The read to ask the host for, unless one is out already.
    fn ask(&mut self) -> Option<TailArgs> {
        if self.asked {
            return None;
        }
        self.asked = true;
        Some(TailArgs {
            path: self.path.clone(),
            offset: self.offset,
            caught_up: self.caught_up,
            messages: self.keep > 0,
        })
    }

    /// Takes what the host read. A reply to a read from another offset is dropped.
    fn take(&mut self, d: TailDelta) {
        self.asked = false;
        if d.from != self.offset {
            return;
        }
        if d.reset {
            *self = Self::new(std::mem::take(&mut self.path), self.keep);
        }
        for m in d.messages {
            self.messages.push(m);
            if self.messages.len() > self.keep {
                // Drop in blocks so a long run does not shift the vector on every message.
                let cut = (self.messages.len() - self.keep).max(self.keep / 4).min(self.messages.len());
                self.messages.drain(..cut);
                self.dropped += cut;
            }
        }
        self.cost += d.cost;
        self.tokens += d.tokens;
        self.offset = d.offset;
        self.caught_up = d.caught_up;
    }
}

/// A listed session's totals, and whether it can still change.
struct Totals {
    tail: Followed,
    /// Read to the end after its job finished.
    settled: bool,
}

/// A one-line message for `mu notify`.
struct Compose {
    session: PathBuf,
    label: String,
    text: String,
}

/// The run `S` was pressed on, until the user answers.
struct Confirm {
    session: PathBuf,
    pid: u32,
    started: String,
    label: String,
}

/// The full-height view of one thing: a subagent's session transcript, another session's, or a bash
/// job's output.
struct View {
    /// The job it shows, or the session's id.
    name: String,
    /// Set for a session of the Sessions tab: what to call it in the title.
    label: Option<String>,
    /// The session being followed; `None` for a bash job.
    tail: Option<Followed>,
    settled: bool,
    polled: Option<Instant>,
    lines: Vec<Line>,
    /// What `lines` was drawn from (a content id) and at which width.
    built: Option<(u64, usize)>,
    top: usize,
    follow: bool,
    /// From the last render, for scrolling.
    max_top: usize,
    body_h: usize,
}

impl View {
    fn scroll(&mut self, by: isize) {
        let cur = if self.follow { self.max_top } else { self.top.min(self.max_top) };
        self.top = (cur as isize + by).clamp(0, self.max_top as isize) as usize;
        self.follow = self.top == self.max_top;
    }

    fn build(&mut self, width: usize, output: Option<&str>) {
        let content = match (&self.tail, output) {
            (Some(t), _) => (((t.messages().len() + t.dropped()) as u64) << 1) | u64::from(t.caught_up()),
            (None, o) => {
                use std::hash::{Hash, Hasher};
                let mut h = std::collections::hash_map::DefaultHasher::new();
                o.hash(&mut h);
                h.finish()
            }
        };
        if self.built == Some((content, width)) {
            return;
        }
        self.built = Some((content, width));
        let mut l: Vec<Line> = Vec::new();
        match &self.tail {
            Some(t) => {
                if t.dropped() > 0 {
                    l.push(line::styled(format!("  … {} earlier messages not shown", t.dropped()), |_| dim()));
                }
                l.extend(session_view::lines(t.messages(), width));
            }
            None => l.extend(output_lines(output.unwrap_or(""), width)),
        }
        if l.is_empty() {
            let why = match &self.tail {
                Some(t) if !t.caught_up() => "  waiting for the session file…",
                Some(_) => "  nothing yet",
                None => "  no output yet",
            };
            l.push(line::styled(why, |_| dim()));
        }
        self.lines = l;
    }
}

impl JobsPanel {
    pub fn new(jobs: Vec<SubagentStatus>) -> Self {
        // Open on the newest running job, else the newest job.
        let at = jobs.iter().rposition(|j| j.state == SubagentState::Running).unwrap_or(jobs.len().saturating_sub(1));
        Self {
            tab: Tab::Jobs,
            jobs,
            at,
            top: 0,
            page: LIST_MAX,
            rows: Vec::new(),
            s_at: 0,
            s_top: 0,
            s_page: LIST_MAX,
            titles: HashMap::new(),
            own: None,
            canon: HashMap::new(),
            home: None,
            totals: HashMap::new(),
            polled_totals: None,
            view: None,
            input: None,
            confirm: None,
            confirm_kill_all: false,
            status: None,
            applied: HashSet::new(),
            notice: None,
            outputs: HashMap::new(),
            wants: Vec::new(),
            live_asked: false,
        }
    }

    /// `$HOME` on the host, so directories of other sessions show as `~/…`.
    pub fn with_home(mut self, home: &std::path::Path) -> Self {
        self.home = Some(home.to_string_lossy().into_owned());
        self
    }

    /// The reads the panel is waiting for, to be made by the app.
    pub fn take_wants(&mut self) -> Vec<Want> {
        std::mem::take(&mut self.wants)
    }

    /// Starts with the output the app already has for its bash jobs.
    pub fn with_outputs(mut self, outputs: HashMap<String, String>) -> Self {
        self.outputs = outputs;
        self
    }

    /// This session's file, so the Sessions tab does not list it.
    pub fn with_own_session(mut self, own: Option<PathBuf>) -> Self {
        self.own = own;
        self
    }

    /// Opens on the Sessions tab.
    pub fn on_sessions(mut self) -> Self {
        self.tab = Tab::Sessions;
        self.refresh_sessions();
        self.refresh();
        self
    }

    /// The newest output of a bash job.
    pub fn set_output(&mut self, id: &str, tail: &str) {
        self.outputs.insert(id.to_string(), tail.to_string());
    }

    /// A fresh snapshot: the selection stays on the same job when it is still there.
    pub fn set_jobs(&mut self, jobs: Vec<SubagentStatus>) {
        let name = self.jobs.get(self.at).map(|j| j.name.clone());
        self.jobs = jobs;
        self.at = name
            .and_then(|n| self.jobs.iter().position(|j| j.name == n))
            .unwrap_or_else(|| self.at.min(self.jobs.len().saturating_sub(1)));
        if self.view.as_ref().is_some_and(|v| v.label.is_none() && !self.jobs.iter().any(|j| j.name == v.name)) {
            self.view = None;
        }
        self.prune_totals();
    }

    /// Asks for the live sessions, unless that is under way.
    fn refresh_sessions(&mut self) {
        if self.live_asked {
            return;
        }
        self.live_asked = true;
        let mut canon: Vec<PathBuf> = self.own.iter().cloned().collect();
        canon.extend(self.jobs.iter().filter_map(|j| j.session.clone()));
        self.wants.push(Want::Sessions(LiveArgs { titled: self.titles.keys().cloned().collect(), canon }));
    }

    /// The host's answer to the request for the live sessions.
    pub fn on_live(&mut self, r: Result<Live, String>) {
        self.live_asked = false;
        if let Ok(live) = r {
            self.set_sessions(live);
            // The new rows' costs are asked for with the list.
            self.refresh();
        }
    }

    /// A fresh list of live sessions: the selection stays on the same one when it is still there.
    pub fn set_sessions(&mut self, live: Live) {
        self.polled_totals = Some(Instant::now());
        self.notice = live.notice;
        self.canon = live.canon.into_iter().collect();
        for r in &live.rows {
            if let Some(t) = &r.title {
                self.titles.entry(r.live.path.clone()).or_insert_with(|| t.clone());
            }
        }
        let keep = self.rows.get(self.s_at).map(|r| r.live.path.clone());
        self.rows = sessions_tab::build(live.rows, &self.titles, &self.jobs, self.own.as_deref(), &self.canon);
        self.s_at = keep
            .and_then(|p| self.rows.iter().position(|r| r.live.path == p))
            .unwrap_or_else(|| self.s_at.min(self.rows.len().saturating_sub(1)));
        self.prune_totals();
    }

    /// Forgets the totals of sessions that are no longer listed.
    fn prune_totals(&mut self) {
        let mut live: Vec<&PathBuf> = self.jobs.iter().filter_map(|j| j.session.as_ref()).collect();
        live.extend(self.rows.iter().map(|r| &r.live.path));
        self.totals.retain(|p, _| live.contains(&p));
    }

    /// Asks for what the sessions gained since the last read: the open transcript and the cost of
    /// every listed subagent and session. `poll` calls it on a timer. (The list of live sessions is
    /// asked for by `refresh_sessions`, from `poll`.)
    pub fn refresh(&mut self) {
        self.polled_totals = Some(Instant::now());
        for j in &self.jobs {
            let Some(p) = &j.session else { continue };
            let t = self
                .totals
                .entry(p.clone())
                .or_insert_with(|| Totals { tail: Followed::new(p.clone(), 0), settled: false });
            if !t.settled
                && let Some(args) = t.tail.ask()
            {
                self.wants.push(Want::Tail { target: TailFor::Total, args });
            }
        }
        for r in &self.rows {
            let t = self
                .totals
                .entry(r.live.path.clone())
                .or_insert_with(|| Totals { tail: Followed::new(r.live.path.clone(), 0), settled: false });
            if let Some(args) = t.tail.ask() {
                self.wants.push(Want::Tail { target: TailFor::Total, args });
            }
        }
        if let Some(v) = &mut self.view {
            v.polled = Some(Instant::now());
            if !v.settled
                && let Some(t) = &mut v.tail
                && let Some(args) = t.ask()
            {
                self.wants.push(Want::Tail { target: TailFor::View, args });
            }
        }
    }

    /// The host's answer to a read of `path`.
    pub fn on_tail(&mut self, target: TailFor, path: &std::path::Path, r: Result<TailDelta, String>) {
        match target {
            TailFor::Total => {
                let Some(t) = self.totals.get_mut(path) else { return };
                match r {
                    Ok(d) => t.tail.take(d),
                    Err(_) => t.tail.asked = false,
                }
                if !t.settled
                    && let Some(j) = self.jobs.iter().find(|j| j.session.as_deref() == Some(path))
                {
                    t.settled = j.state != SubagentState::Running && t.tail.caught_up();
                }
            }
            TailFor::View => {
                // A job's transcript is done once the job is and the file is read; a session's never is.
                let job_state = self
                    .view
                    .as_ref()
                    .filter(|v| v.label.is_none())
                    .and_then(|v| self.jobs.iter().find(|j| j.name == v.name))
                    .map(|j| j.state.clone());
                let Some(v) = &mut self.view else { return };
                let Some(t) = v.tail.as_mut().filter(|t| t.path == path) else { return };
                match r {
                    Ok(d) => t.take(d),
                    Err(_) => t.asked = false,
                }
                if !v.settled {
                    v.settled = job_state.is_some_and(|s| s != SubagentState::Running) && t.caught_up();
                }
            }
        }
    }

    /// Called on the app's tick: reads what changed, at the polling intervals.
    pub fn poll(&mut self) {
        let now = Instant::now();
        let totals_due = self.polled_totals.is_none_or(|t| now.duration_since(t) >= TOTALS_POLL);
        let view_due = self
            .view
            .as_ref()
            .is_some_and(|v| v.tail.is_some() && v.polled.is_none_or(|t| now.duration_since(t) >= VIEW_POLL));
        if totals_due || view_due {
            if totals_due && self.tab == Tab::Sessions {
                self.refresh_sessions();
            }
            self.refresh();
        }
    }

    /// `$0.03`, `…` while the session is still being read, empty when there is no session.
    fn cost_of(&self, session: Option<&PathBuf>) -> String {
        match session.and_then(|p| self.totals.get(p)) {
            Some(t) if t.tail.caught_up() => format!("${:.2}", t.tail.cost()),
            Some(_) => "…".into(),
            None => String::new(),
        }
    }

    fn cost_cell(&self, j: &SubagentStatus) -> String {
        self.cost_of(j.session.as_ref())
    }

    fn running(&self) -> usize {
        self.jobs.iter().filter(|j| j.state == SubagentState::Running).count()
    }

    fn new_view(name: String, label: Option<String>, tail: Option<Followed>) -> View {
        View {
            name,
            label,
            tail,
            settled: false,
            polled: None,
            lines: Vec::new(),
            built: None,
            top: 0,
            follow: true,
            max_top: 0,
            body_h: 1,
        }
    }

    /// Opens the selected job's view: a subagent's transcript once it has a session file, or a bash
    /// job's output.
    fn open_view(&mut self) {
        let Some(j) = self.jobs.get(self.at) else { return };
        let tail = match (&j.session, j.kind) {
            (Some(path), _) => Some(Followed::new(path.clone(), VIEW_KEEP)),
            (None, JobKind::Bash) => None,
            (None, JobKind::Subagent) => return,
        };
        self.view = Some(Self::new_view(j.name.clone(), None, tail));
        self.refresh();
    }

    /// Opens the selected session's transcript, read-only.
    fn open_session_view(&mut self) {
        let Some(r) = self.rows.get(self.s_at) else { return };
        let label = format!("{} · pid {}", r.label(), r.live.pid);
        self.view = Some(Self::new_view(r.id(), Some(label), Some(Followed::new(r.live.path.clone(), VIEW_KEEP))));
        self.refresh();
    }

    fn switch_tab(&mut self) {
        self.tab = match self.tab {
            Tab::Jobs => Tab::Sessions,
            Tab::Sessions => Tab::Jobs,
        };
        if self.tab == Tab::Sessions {
            self.refresh_sessions();
            self.refresh();
        }
    }

    fn view_key(&mut self, k: KeyEvent) -> JobsResult {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let Some(v) = self.view.as_mut() else { return JobsResult::Pending };
        let page = v.body_h.max(1) as isize;
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') => self.view = None,
            KeyCode::Down | KeyCode::Char('j') => v.scroll(1),
            KeyCode::Char('n') if ctrl => v.scroll(1),
            KeyCode::Up | KeyCode::Char('k') => v.scroll(-1),
            KeyCode::Char('p') if ctrl => v.scroll(-1),
            KeyCode::PageDown => v.scroll(page),
            KeyCode::Char('f') if ctrl => v.scroll(page),
            KeyCode::PageUp => v.scroll(-page),
            KeyCode::Char('b') if ctrl => v.scroll(-page),
            KeyCode::Char('d') if ctrl => v.scroll(page / 2),
            KeyCode::Char('u') if ctrl => v.scroll(-(page / 2)),
            KeyCode::Home | KeyCode::Char('g') => {
                v.top = 0;
                v.follow = v.max_top == 0;
            }
            KeyCode::End | KeyCode::Char('G') => v.follow = true,
            KeyCode::Char('f') => {
                if v.follow {
                    v.top = v.max_top;
                }
                v.follow = !v.follow;
            }
            KeyCode::Char('x') if v.label.is_none() => {
                let name = v.name.clone();
                if self.jobs.iter().any(|j| j.name == name && j.state == SubagentState::Running) {
                    return JobsResult::Kill(Some(name));
                }
            }
            _ => {}
        }
        JobsResult::Pending
    }

    /// Typing a message: Enter sends it, Esc drops it.
    fn input_key(&mut self, k: KeyEvent) -> JobsResult {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let Some(c) = self.input.as_mut() else { return JobsResult::Pending };
        match k.code {
            KeyCode::Esc => self.input = None,
            KeyCode::Enter => {
                if let Some(c) = self.input.take() {
                    let text = c.text.trim().to_string();
                    if !text.is_empty() {
                        return JobsResult::Notify { session: c.session, text };
                    }
                }
            }
            KeyCode::Backspace => {
                c.text.pop();
            }
            KeyCode::Char('u') if ctrl => c.text.clear(),
            KeyCode::Char('w') if ctrl => {
                let kept = c.text.trim_end().rfind(' ').map_or(0, |i| i + 1);
                c.text.truncate(kept);
            }
            KeyCode::Char(ch) if !ctrl && !k.modifiers.contains(KeyModifiers::ALT) => c.text.push(ch),
            _ => {}
        }
        JobsResult::Pending
    }

    /// Pasted text goes into the message being typed, on one line.
    pub fn on_paste(&mut self, s: &str) {
        if let Some(c) = &mut self.input {
            c.text.push_str(&s.replace(['\r', '\n'], " "));
        }
    }

    /// Shows what an action did until the next key.
    pub fn set_status(&mut self, s: impl Into<String>) {
        self.status = Some(s.into());
    }

    /// Opens a launched job's diff in the full-height view.
    pub fn show_diff(&mut self, job: &str, diff: &str) {
        let key = format!("diff:{job}");
        let text = if diff.trim().is_empty() { "  the result changes nothing" } else { diff };
        self.outputs.insert(key.clone(), text.to_string());
        self.view = Some(Self::new_view(key, Some(format!("{job} · diff")), None));
        if let Some(v) = &mut self.view {
            v.follow = false;
        }
    }

    /// The job's result is in the working tree now.
    pub fn mark_applied(&mut self, job: &str) {
        self.applied.insert(job.to_string());
    }

    /// The selected job and its result, when it finished and left one.
    fn result_of(&self) -> Option<(&SubagentStatus, ResultArgs, bool)> {
        let j = self.jobs.get(self.at).filter(|j| j.state != SubagentState::Running)?;
        let c = j.changes.as_ref()?;
        let args = ResultArgs { cwd: j.cwd.clone(), r#ref: c.r#ref.clone(), base: c.base.clone() };
        Some((j, args, c.applied || self.applied.contains(&j.name)))
    }

    pub fn on_key(&mut self, k: KeyEvent) -> JobsResult {
        if k.kind == KeyEventKind::Release {
            return JobsResult::Pending;
        }
        self.status = None;
        if std::mem::take(&mut self.confirm_kill_all) {
            return if k.code == KeyCode::Char('y') { JobsResult::Kill(None) } else { JobsResult::Pending };
        }
        if self.confirm.is_some() {
            return self.confirm_key(k);
        }
        if self.input.is_some() {
            return self.input_key(k);
        }
        if self.view.is_some() {
            return self.view_key(k);
        }
        if matches!(k.code, KeyCode::Tab | KeyCode::BackTab) {
            self.switch_tab();
            return JobsResult::Pending;
        }
        match self.tab {
            Tab::Jobs => self.jobs_key(k),
            Tab::Sessions => self.sessions_key(k),
        }
    }

    fn jobs_key(&mut self, k: KeyEvent) -> JobsResult {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let last = self.jobs.len().saturating_sub(1);
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') => return JobsResult::Close,
            KeyCode::Down | KeyCode::Char('j') => self.at = (self.at + 1).min(last),
            KeyCode::Char('n') if ctrl => self.at = (self.at + 1).min(last),
            KeyCode::Up | KeyCode::Char('k') => self.at = self.at.saturating_sub(1),
            KeyCode::Char('p') if ctrl => self.at = self.at.saturating_sub(1),
            KeyCode::Home | KeyCode::Char('g') => self.at = 0,
            KeyCode::End | KeyCode::Char('G') => self.at = last,
            KeyCode::PageDown => self.at = (self.at + self.page).min(last),
            KeyCode::PageUp => self.at = self.at.saturating_sub(self.page),
            KeyCode::Enter => self.open_view(),
            KeyCode::Char('x') => {
                if let Some(j) = self.jobs.get(self.at).filter(|j| j.state == SubagentState::Running) {
                    return JobsResult::Kill(Some(j.name.clone()));
                }
            }
            KeyCode::Char('X') if self.running() > 0 => self.confirm_kill_all = true,
            KeyCode::Char('d') => {
                if let Some((j, result, _)) = self.result_of() {
                    return JobsResult::Diff { job: j.name.clone(), result };
                }
            }
            KeyCode::Char('a') => {
                let r = self.result_of().map(|(j, result, applied)| (j.name.clone(), result, applied));
                match r {
                    Some((_, _, true)) => self.status = Some("that result is already applied".into()),
                    Some((job, result, false)) => return JobsResult::Apply { job, result },
                    None => {}
                }
            }
            _ => {}
        }
        JobsResult::Pending
    }

    fn sessions_key(&mut self, k: KeyEvent) -> JobsResult {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let last = self.rows.len().saturating_sub(1);
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') => return JobsResult::Close,
            KeyCode::Down | KeyCode::Char('j') => self.s_at = (self.s_at + 1).min(last),
            KeyCode::Char('n') if ctrl => self.s_at = (self.s_at + 1).min(last),
            KeyCode::Up | KeyCode::Char('k') => self.s_at = self.s_at.saturating_sub(1),
            KeyCode::Char('p') if ctrl => self.s_at = self.s_at.saturating_sub(1),
            KeyCode::Home | KeyCode::Char('g') => self.s_at = 0,
            KeyCode::End | KeyCode::Char('G') => self.s_at = last,
            KeyCode::PageDown => self.s_at = (self.s_at + self.s_page).min(last),
            KeyCode::PageUp => self.s_at = self.s_at.saturating_sub(self.s_page),
            KeyCode::Enter => self.open_session_view(),
            KeyCode::Char('n') => {
                if let Some(r) = self.rows.get(self.s_at) {
                    self.input = Some(Compose { session: r.live.path.clone(), label: r.label(), text: String::new() });
                }
            }
            KeyCode::Char('s') => {
                if let Some(r) = self.rows.get(self.s_at).filter(|r| r.can_stop()) {
                    return JobsResult::Stop {
                        session: r.live.path.clone(),
                        pid: r.live.pid,
                        started: r.live.started.clone(),
                        kind: r.live.kind.clone().unwrap_or_default(),
                    };
                }
            }
            KeyCode::Char('t') => {
                if let Some(r) = self.rows.get(self.s_at).filter(|r| r.can_take()) {
                    return JobsResult::Take { session: r.live.path.clone() };
                }
            }
            KeyCode::Char('S') => {
                if let Some(r) = self.rows.get(self.s_at).filter(|r| r.can_terminate()) {
                    self.confirm = Some(Confirm {
                        session: r.live.path.clone(),
                        pid: r.live.pid,
                        started: r.live.started.clone(),
                        label: r.label(),
                    });
                }
            }
            _ => {}
        }
        JobsResult::Pending
    }

    /// `y` terminates; any other key keeps the session.
    fn confirm_key(&mut self, k: KeyEvent) -> JobsResult {
        let Some(c) = self.confirm.take() else { return JobsResult::Pending };
        if k.code == KeyCode::Char('y') {
            return JobsResult::Terminate { session: c.session, pid: c.pid, started: c.started };
        }
        JobsResult::Pending
    }

    /// The panel's rows (at most `max_rows`) and the cursor within them. `now` times running jobs.
    pub fn render(&mut self, width: usize, max_rows: usize, now: SystemTime) -> (Vec<Line>, (usize, usize)) {
        if self.view.is_some() {
            return self.render_view(width, max_rows, now);
        }
        let mut screen = match self.tab {
            Tab::Jobs => self.jobs_frame(width, now),
            Tab::Sessions => self.sessions_frame(width, now),
        };
        if let Some(s) = &self.status {
            screen.footer = vec![span(s.clone(), accent())];
        }
        let (top, page) = match self.tab {
            Tab::Jobs => (&mut self.top, &mut self.page),
            Tab::Sessions => (&mut self.s_top, &mut self.s_page),
        };
        draw(screen, top, page, width, max_rows)
    }

    fn jobs_frame(&self, width: usize, now: SystemTime) -> Screen {
        let name_w = self.jobs.iter().map(|j| j.name.chars().count()).max().unwrap_or(0).min(NAME_MAX);
        let agent_w = self.jobs.iter().map(|j| j.agent.chars().count()).max().unwrap_or(0).min(AGENT_MAX);
        let rows = self
            .jobs
            .iter()
            .enumerate()
            .map(|(i, j)| list_row(j, i == self.at, (name_w, agent_w), &self.cost_cell(j), width, now))
            .collect();
        let (run, done) = (self.running(), self.jobs.len() - self.running());
        let (detail_title, detail_rows) = match self.jobs.get(self.at) {
            Some(j) => {
                let mut rows = detail(j, &self.cost_cell(j), width, now);
                if let Some((_, _, applied)) = self.result_of() {
                    let what = if applied { "applied" } else { "not applied · d diff · a apply" };
                    rows.push(vec![span(format!("   result {what}"), theme::get().fg(Role::Faint))]);
                }
                (Some(format!("{} · {}", j.name, j.agent)), rows)
            }
            None => (None, Vec::new()),
        };
        let keys = if self.result_of().is_some() {
            "j/k move · Enter transcript · d diff · a apply · x kill · Tab sessions · q close"
        } else {
            "j/k move · Enter transcript · x kill · X kill all · Tab sessions · q close"
        };
        Screen {
            title: format!("jobs · {run} running · {done} finished"),
            rows,
            at: self.at,
            empty: "  no background jobs",
            detail_title,
            detail: detail_rows,
            footer: if self.confirm_kill_all {
                vec![span(format!("cancel all {run} running jobs? · y to confirm, any other key cancels"), accent())]
            } else {
                line::styled(keys, |_| dim())
            },
            footer_cursor: None,
            note: None,
        }
    }

    fn sessions_frame(&self, width: usize, now: SystemTime) -> Screen {
        let label_w = self.rows.iter().map(|r| r.depth * 2 + r.label().chars().count()).max().unwrap_or(0).min(26);
        let rows = self
            .rows
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let cost = self.cost_of(Some(&r.live.path));
                sessions_tab::list_row(r, i == self.s_at, label_w, &cost, width, now, self.home.as_deref())
            })
            .collect();
        let (detail_title, detail_rows) = match self.rows.get(self.s_at) {
            Some(r) => (Some(r.label()), sessions_tab::detail(r, &self.cost_of(Some(&r.live.path)), width, now)),
            None => (None, Vec::new()),
        };
        let (footer, footer_cursor) = match (&self.confirm, &self.input) {
            (Some(c), _) => (
                vec![span(
                    format!(
                        "terminate {} (pid {})? it exits and its unsent input is lost · y to confirm, any other key cancels",
                        c.label, c.pid
                    ),
                    accent(),
                )],
                None,
            ),
            (None, Some(c)) => {
                let lead = format!("message to {} (Enter send, Esc cancel): ", c.label);
                let col = lead.chars().count() + c.text.chars().count();
                (vec![span(lead, dim()), span(c.text.clone(), Default::default())], Some(col))
            }
            (None, None) => (
                line::styled(
                    "Enter peek · n message · s stop · S terminate · t take over · Tab jobs · q close",
                    |_| dim(),
                ),
                None,
            ),
        };
        Screen {
            title: format!("sessions · {} live", self.rows.len()),
            rows,
            at: self.s_at,
            empty: "  no other mu sessions are running",
            detail_title,
            detail: detail_rows,
            footer,
            footer_cursor,
            note: self.notice.as_ref().map(|n| vec![span(format!("  {n}"), accent())]),
        }
    }

    /// The transcript of the open job: a titled rule, the window of lines, a closing rule and keys.
    fn render_view(&mut self, width: usize, max_rows: usize, now: SystemTime) -> (Vec<Line>, (usize, usize)) {
        let job = self
            .view
            .as_ref()
            .filter(|v| v.label.is_none())
            .and_then(|v| self.jobs.iter().find(|j| j.name == v.name))
            .cloned();
        let output = self.view.as_ref().and_then(|v| self.outputs.get(&v.name)).map(String::as_str);
        let Some(v) = self.view.as_mut() else { return (Vec::new(), (0, 0)) };
        let mut frame = Frame::default();
        if max_rows >= 3 {
            let mut title = v.label.clone().unwrap_or_else(|| v.name.clone());
            if let Some(j) = &job {
                title.push_str(&format!(" · {} · {} {}", j.agent, mark(&j.state).0, clock(elapsed(j, now))));
            }
            if let Some(t) = v.tail.as_ref().filter(|t| t.caught_up()) {
                title.push_str(&format!(" · ${:.2}", t.cost()));
            }
            if let Some(t) = v.tail.as_ref().filter(|t| t.tokens() > 0) {
                title.push_str(&format!(" · {} tok", short_count(t.tokens())));
            }
            title.push_str(if v.follow { " · following" } else { " · paused" });
            frame.header.push(rule(Some(&title), width));
        }
        if max_rows >= 5 {
            let keys = if v.label.is_some() {
                "j/k scroll · PgUp/PgDn · g/G top/bottom · f follow · q back"
            } else {
                "j/k scroll · PgUp/PgDn · g/G top/bottom · f follow · x kill · q back"
            };
            frame.footer = vec![rule(None, width), line::styled(keys, |_| dim())];
        }
        let h = frame.body_rows(max_rows, 1);
        v.build(width, output);
        v.body_h = h;
        v.max_top = v.lines.len().saturating_sub(h);
        let top = if v.follow { v.max_top } else { v.top.min(v.max_top) };
        let end = (top + h).min(v.lines.len());
        let mut body: Vec<Line> = v.lines[top..end].to_vec();
        body.resize(h, Vec::new());
        let cursor = (frame.header.len(), 0);
        (frame.finish(body, width), cursor)
    }
}

/// One tab's screen: a title, the list rows (all of them) and which is selected, the selected row's
/// details, and the key line. `draw` fits it to the rows there are.
struct Screen {
    title: String,
    rows: Vec<Line>,
    at: usize,
    empty: &'static str,
    detail_title: Option<String>,
    detail: Vec<Line>,
    footer: Line,
    /// A column in the footer where the cursor goes (a message being typed).
    footer_cursor: Option<usize>,
    /// One row between the list and the details, when there is room.
    note: Option<Line>,
}

/// Lays a screen out in `max_rows`: a titled rule, a window of the list, the details between rules and
/// the key line; with too few rows only the list. `top` and `page` are the tab's scroll state.
fn draw(f: Screen, top: &mut usize, page: &mut usize, width: usize, max_rows: usize) -> (Vec<Line>, (usize, usize)) {
    let n = f.rows.len();
    let split = max_rows >= CHROME + 1 + DETAIL_MIN;
    let mut frame = Frame::default();
    if max_rows >= 2 {
        frame.header.push(rule(Some(&f.title), width));
    }
    if split || max_rows >= 3 {
        frame.footer.push(f.footer);
    }
    // The detail block's two rules are part of the body.
    let body = frame.body_rows(max_rows, 1);
    let note_h = usize::from(split && f.note.is_some() && body >= 2 + DETAIL_MIN + 2);
    let (list_h, detail_h) = if split {
        let list_h = n.clamp(1, LIST_MAX).min(body - 2 - note_h - DETAIL_MIN);
        (list_h, (body - 2 - note_h - list_h).min(DETAIL_MAX))
    } else {
        (body, 0)
    };
    *page = list_h;
    if f.at < *top {
        *top = f.at;
    }
    if f.at >= *top + list_h {
        *top = f.at + 1 - list_h;
    }
    *top = (*top).min(n.saturating_sub(list_h));

    let mut out: Vec<Line> = Vec::new();
    if n == 0 {
        out.push(vec![span(f.empty, dim())]);
    }
    let end = (*top + list_h).min(n);
    out.extend(f.rows[*top..end].iter().cloned());
    // Blank-pad the list so the details start on a fixed row.
    let shown = if n == 0 { 1 } else { end - *top };
    out.extend(std::iter::repeat_n(Vec::new(), list_h.saturating_sub(shown)));
    if let (1, Some(note)) = (note_h, &f.note) {
        out.push(line::truncate(note, width));
    }
    if split {
        out.push(rule(f.detail_title.as_deref(), width));
        let mut d = f.detail;
        d.truncate(detail_h);
        d.resize(detail_h, Vec::new());
        out.extend(d);
        out.push(rule(None, width));
    }
    let header = frame.header.len();
    let mut cursor = (header + f.at.saturating_sub(*top).min(list_h.saturating_sub(1)), 0);
    if let (Some(col), false) = (f.footer_cursor, frame.footer.is_empty()) {
        cursor = (header + out.len(), col.min(width.saturating_sub(1)));
    }
    out.truncate(max_rows.saturating_sub(header + frame.footer.len()));
    (frame.finish(out, width), cursor)
}

fn mark(state: &SubagentState) -> (char, Role) {
    match state {
        SubagentState::Running => ('◌', Role::Accent),
        SubagentState::Done => ('✓', Role::Ok),
        SubagentState::Failed => ('✗', Role::Err),
    }
}

/// A bash job's output as rows: terminal escapes dropped, a carriage return keeping only what was
/// written last on the line (progress bars), long lines split at `width`.
fn output_lines(text: &str, width: usize) -> Vec<Line> {
    let muted = theme::get().fg(Role::Muted);
    let mut out: Vec<Line> = Vec::new();
    for raw in mu_common::text::strip_ansi(text).lines() {
        let l = raw.rsplit('\r').find(|s| !s.is_empty()).unwrap_or("");
        if l.is_empty() {
            out.push(Vec::new());
            continue;
        }
        let chars: Vec<char> = l.chars().collect();
        for c in chars.chunks(width.max(1)) {
            out.push(vec![span(c.iter().collect::<String>(), muted)]);
        }
    }
    out
}

/// `950`, `12.3k`, `1.2M`.
fn short_count(n: u64) -> String {
    match n {
        0..1000 => n.to_string(),
        1000..1_000_000 => format!("{:.1}k", n as f64 / 1e3),
        _ => format!("{:.1}M", n as f64 / 1e6),
    }
}

/// `m:ss`, or `h:mm:ss` from an hour on.
fn clock(secs: u64) -> String {
    if secs >= 3600 {
        format!("{}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60)
    } else {
        format!("{}:{:02}", secs / 60, secs % 60)
    }
}

fn elapsed(j: &SubagentStatus, now: SystemTime) -> u64 {
    let end = j.ended.unwrap_or(now);
    end.duration_since(j.started).map(|d| d.as_secs()).unwrap_or(0)
}

fn list_row(
    j: &SubagentStatus,
    selected: bool,
    (name_w, agent_w): (usize, usize),
    cost: &str,
    width: usize,
    now: SystemTime,
) -> Line {
    let t = theme::get();
    let (m, role) = mark(&j.state);
    let tools = match j.kind {
        JobKind::Subagent => format!("{:>3} tools", j.tools),
        JobKind::Bash => " ".repeat(9),
    };
    let rest = format!(" {} {tools}  {:>7}  {cost:>6}  {}", pad(&j.agent, agent_w), clock(elapsed(j, now)), j.last);
    let mut name = t.fg(Role::Strong);
    if selected {
        name.attributes.set(crossterm::style::Attribute::Bold);
    }
    let mut l = vec![
        if selected { span("❯ ", accent()) } else { span("  ", dim()) },
        span(format!("{m} "), t.fg(role)),
        span(pad(&j.name, name_w), name),
        span(rest, t.fg(Role::Muted)),
    ];
    if selected {
        l = line::truncate(&l, width);
        l = line::fill(&l, width, t.color(Role::Selected));
    }
    l
}

/// What the panel knows about one job, as many rows as it takes.
fn detail(j: &SubagentStatus, cost: &str, width: usize, now: SystemTime) -> Vec<Line> {
    let t = theme::get();
    let (m, role) = mark(&j.state);
    let state = match j.state {
        SubagentState::Running => "running",
        SubagentState::Done => "done",
        SubagentState::Failed => "failed",
    };
    let muted = t.fg(Role::Muted);
    let mut facts = format!("{state} · {}", clock(elapsed(j, now)));
    if let Some(m) = &j.model {
        facts.push_str(&format!(" · {m}"));
    }
    if j.kind == JobKind::Subagent {
        facts.push_str(&format!(" · {} tool{}", j.tools, if j.tools == 1 { "" } else { "s" }));
    }
    if !cost.is_empty() {
        facts.push_str(&format!(" · {cost}"));
    }
    if let Some(n) = j.note.as_deref().filter(|n| !n.is_empty()) {
        facts.push_str(&format!(" · {n}"));
    }
    if j.state != SubagentState::Running {
        facts.push_str(if j.delivered { " · result delivered" } else { " · result not delivered yet" });
    }
    let mut rows: Vec<Line> = vec![vec![span(format!(" {m} "), t.fg(role)), span(facts, muted)]];
    if !j.cwd.as_os_str().is_empty() {
        rows.push(vec![span(format!("   {}", j.cwd.display()), muted)]);
    }
    let label = match j.kind {
        JobKind::Subagent => "task",
        JobKind::Bash => "$",
    };
    let body_w = width.saturating_sub(3 + label.len() + 1).max(8);
    for (i, r) in wrap(&j.task, body_w).into_iter().enumerate() {
        let lead = if i == 0 { format!("   {label} ") } else { format!("   {} ", " ".repeat(label.len())) };
        rows.push(vec![span(lead, t.fg(Role::Faint)), span(r, t.fg(Role::Strong))]);
    }
    if j.state == SubagentState::Running && !j.last.is_empty() && j.last != j.task {
        rows.push(vec![span(format!("   last {}", j.last), muted)]);
    }
    let hint = match (j.kind, &j.session) {
        (JobKind::Subagent, Some(_)) => "Enter  read its transcript",
        (JobKind::Subagent, None) => "no session file yet",
        (JobKind::Bash, _) => "Enter  see its output",
    };
    rows.push(vec![span(format!("   {hint}"), t.fg(Role::Faint))]);
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::sessions::LiveRow;
    use crate::text::line::line_text;
    use mu_session::session::owner::LiveSession;
    use mu_testutil::key;
    use std::path::PathBuf;
    use std::time::Duration;

    fn job(name: &str, state: SubagentState) -> SubagentStatus {
        let started = SystemTime::UNIX_EPOCH + Duration::from_secs(1000);
        SubagentStatus {
            name: name.into(),
            agent: "scout".into(),
            tools: 4,
            last: "read src/x.rs".into(),
            ended: (state != SubagentState::Running).then(|| started + Duration::from_secs(41)),
            state,
            kind: JobKind::Subagent,
            cwd: PathBuf::from("/work/proj"),
            task: "Find where subagent rows are drawn".into(),
            started,
            delivered: false,
            note: None,
            session: None,
            model: None,
            changes: None,
        }
    }

    fn now() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1012)
    }

    /// Does what the app does with the panel's wants, with the host's own readers.
    fn serve(p: &mut JobsPanel) {
        for w in p.take_wants() {
            match w {
                Want::Tail { target, args } => {
                    let path = args.path.clone();
                    p.on_tail(target, &path, Ok(crate::host::sessions::read_tail(args)));
                }
                // Tests set the list themselves.
                Want::Sessions(_) => p.on_live(Err(String::new())),
            }
        }
    }

    fn text(p: &mut JobsPanel, w: usize, h: usize) -> Vec<String> {
        serve(p);
        p.render(w, h, now()).0.iter().map(line_text).collect()
    }

    fn live_of(lives: Vec<LiveSession>) -> Live {
        let rows =
            lives.into_iter().map(|live| LiveRow { parent: live.parent.clone(), modified: None, title: None, live });
        Live { rows: rows.collect(), canon: Vec::new(), notice: None }
    }

    #[test]
    fn clock_formats_minutes_and_hours() {
        assert_eq!(clock(0), "0:00");
        assert_eq!(clock(75), "1:15");
        assert_eq!(clock(3600 + 61), "1:01:01");
    }

    #[test]
    fn opens_on_the_newest_running_job() {
        let p = JobsPanel::new(vec![
            job("a", SubagentState::Running),
            job("b", SubagentState::Done),
            job("c", SubagentState::Running),
            job("d", SubagentState::Failed),
        ]);
        assert_eq!(p.at, 2);
        assert_eq!(JobsPanel::new(vec![job("a", SubagentState::Done), job("b", SubagentState::Done)]).at, 1);
    }

    #[test]
    fn full_layout_has_list_details_and_keys() {
        let mut p = JobsPanel::new(vec![job("scout-1", SubagentState::Running), job("review", SubagentState::Done)]);
        let rows = text(&mut p, 80, 14);
        assert_eq!(rows.len(), 14);
        assert!(rows[0].starts_with("── jobs · 1 running · 1 finished"), "{rows:?}");
        assert!(rows[1].contains("scout-1") && rows[1].contains("0:12"), "{rows:?}");
        assert!(rows[2].contains("review") && rows[2].contains("0:41"), "{rows:?}");
        assert!(
            rows.iter().any(|r| r.starts_with("── scout-1 · scout")),
            "the newest running job is selected and heads the details: {rows:?}"
        );
        assert!(rows.iter().any(|r| r.contains("/work/proj")));
        assert!(rows.iter().any(|r| r.contains("task Find where subagent rows are drawn")));
        assert!(!rows.iter().any(|r| r.contains("delivered")), "a running job has no delivery state");
        p.on_key(key(KeyCode::Char('j')));
        let rows = text(&mut p, 80, 14);
        assert!(rows.iter().any(|r| r.starts_with("── review · scout")), "{rows:?}");
        assert!(rows.iter().any(|r| r.contains("done · 0:41") && r.contains("result not delivered yet")), "{rows:?}");
        assert!(rows.last().unwrap().contains("x kill"));
    }

    #[test]
    fn a_tall_screen_does_not_stretch_the_panel() {
        let mut p = JobsPanel::new(vec![job("a", SubagentState::Running), job("b", SubagentState::Done)]);
        // rule + 2 jobs + rule + DETAIL_MAX + rule + keys
        assert_eq!(text(&mut p, 80, 60).len(), 1 + 2 + 1 + DETAIL_MAX + 1 + 1);
        let many: Vec<_> = (0..30).map(|i| job(&format!("j{i}"), SubagentState::Done)).collect();
        let mut p = JobsPanel::new(many);
        assert_eq!(text(&mut p, 80, 60).len(), 1 + LIST_MAX + 1 + DETAIL_MAX + 1 + 1);
    }

    #[test]
    fn short_screens_drop_the_details() {
        let mut p = JobsPanel::new(vec![job("a", SubagentState::Running)]);
        for h in 1..=14 {
            let rows = text(&mut p, 60, h);
            assert!(rows.len() <= h, "height {h}: {rows:?}");
        }
        let rows = text(&mut p, 60, 5);
        assert!(!rows.iter().any(|r| r.contains("task ")), "{rows:?}");
        assert!(rows[1].contains('a'));
    }

    #[test]
    fn the_list_scrolls_to_keep_the_selection_visible() {
        let jobs: Vec<_> = (0..20).map(|i| job(&format!("j{i:02}"), SubagentState::Done)).collect();
        let mut p = JobsPanel::new(jobs);
        let rows = text(&mut p, 60, 16);
        assert!(rows.iter().any(|r| r.contains("j19")), "newest job selected and visible: {rows:?}");
        assert!(!rows.iter().any(|r| r.contains("j00")));
        p.on_key(key(KeyCode::Home));
        let rows = text(&mut p, 60, 16);
        assert!(rows.iter().any(|r| r.contains("j00")) && !rows.iter().any(|r| r.contains("j19")));
    }

    #[test]
    fn keys_move_close_and_kill() {
        let mut p = JobsPanel::new(vec![job("a", SubagentState::Running), job("b", SubagentState::Done)]);
        p.at = 0;
        assert!(matches!(p.on_key(key(KeyCode::Char('x'))), JobsResult::Kill(Some(id)) if id == "a"));
        p.on_key(key(KeyCode::Char('j')));
        assert_eq!(p.at, 1);
        assert!(matches!(p.on_key(key(KeyCode::Char('x'))), JobsResult::Pending), "a finished job is not killed");
        assert!(matches!(p.on_key(key(KeyCode::Char('X'))), JobsResult::Pending), "X asks first");
        assert!(matches!(p.on_key(key(KeyCode::Char('y'))), JobsResult::Kill(None)));
        p.on_key(key(KeyCode::Down));
        assert_eq!(p.at, 1, "stops at the last job");
        p.on_key(key(KeyCode::Char('k')));
        assert_eq!(p.at, 0);
        assert!(matches!(p.on_key(key(KeyCode::Char('q'))), JobsResult::Close));
        assert!(matches!(p.on_key(key(KeyCode::Esc)), JobsResult::Close));
    }

    #[test]
    fn kill_all_is_cancelled_by_any_key_but_y() {
        let mut p = JobsPanel::new(vec![job("a", SubagentState::Running)]);
        p.on_key(key(KeyCode::Char('X')));
        assert!(text(&mut p, 80, 10).iter().any(|r| r.contains("cancel all 1 running")));
        assert!(matches!(p.on_key(key(KeyCode::Char('n'))), JobsResult::Pending));
        assert!(matches!(p.on_key(key(KeyCode::Char('y'))), JobsResult::Pending), "the question is gone");
    }

    fn left_result(name: &str, state: SubagentState, applied: bool) -> SubagentStatus {
        let changes = mu_base::events::JobChanges { r#ref: "refs/mu/result/a-1".into(), base: "0".repeat(40), applied };
        SubagentStatus { changes: Some(changes), ..job(name, state) }
    }

    #[test]
    fn d_and_a_name_the_result_of_a_finished_launched_job() {
        let mut p = JobsPanel::new(vec![left_result("a", SubagentState::Done, false)]);
        let JobsResult::Diff { job, result } = p.on_key(key(KeyCode::Char('d'))) else { panic!("not a diff") };
        assert_eq!((job.as_str(), result.r#ref.as_str()), ("a", "refs/mu/result/a-1"));
        assert_eq!(result.cwd, PathBuf::from("/work/proj"));
        assert_eq!(result.base, "0".repeat(40));
        assert!(matches!(p.on_key(key(KeyCode::Char('a'))), JobsResult::Apply { job, .. } if job == "a"));
        let rows = text(&mut p, 100, 20);
        assert!(rows.iter().any(|r| r.contains("result not applied · d diff · a apply")), "{rows:?}");
        assert!(
            rows.iter().any(|r| r.contains("Enter transcript · d diff · a apply")),
            "the key line says so: {rows:?}"
        );
    }

    #[test]
    fn a_result_applied_automatically_or_from_the_panel_is_not_applied_again() {
        let mut p = JobsPanel::new(vec![left_result("a", SubagentState::Done, true)]);
        assert!(matches!(p.on_key(key(KeyCode::Char('a'))), JobsResult::Pending));
        assert!(text(&mut p, 100, 20).iter().any(|r| r.contains("already applied")));
        assert!(matches!(p.on_key(key(KeyCode::Char('d'))), JobsResult::Diff { .. }), "the diff stays readable");

        let mut p = JobsPanel::new(vec![left_result("b", SubagentState::Done, false)]);
        p.mark_applied("b");
        assert!(matches!(p.on_key(key(KeyCode::Char('a'))), JobsResult::Pending));
        assert!(text(&mut p, 100, 20).iter().any(|r| r.contains("result applied")));
    }

    #[test]
    fn d_and_a_do_nothing_without_a_finished_result() {
        let mut p = JobsPanel::new(vec![left_result("a", SubagentState::Running, false)]);
        assert!(matches!(p.on_key(key(KeyCode::Char('d'))), JobsResult::Pending));
        assert!(matches!(p.on_key(key(KeyCode::Char('a'))), JobsResult::Pending));
        let mut p = JobsPanel::new(vec![job("b", SubagentState::Done)]);
        assert!(matches!(p.on_key(key(KeyCode::Char('d'))), JobsResult::Pending));
        assert!(matches!(p.on_key(key(KeyCode::Char('a'))), JobsResult::Pending));
        assert!(!text(&mut p, 100, 20).iter().any(|r| r.contains("d diff")));
    }

    #[test]
    fn the_diff_opens_in_the_full_height_view_and_q_returns() {
        let mut p = JobsPanel::new(vec![left_result("a", SubagentState::Done, false)]);
        p.show_diff("a", "diff --git a/f b/f\n-one\n+two\n");
        let rows = text(&mut p, 80, 12);
        assert!(rows[0].contains("a · diff"), "{rows:?}");
        assert!(rows.iter().any(|r| r.contains("+two")) && rows.iter().any(|r| r.contains("-one")), "{rows:?}");
        p.on_key(key(KeyCode::Char('q')));
        assert!(p.view.is_none());
        p.show_diff("a", "");
        assert!(text(&mut p, 80, 12).iter().any(|r| r.contains("changes nothing")));
    }

    #[test]
    fn the_sessions_tab_shows_the_hosts_notice_when_there_is_room() {
        let mut p = JobsPanel::new(vec![]).on_sessions();
        p.set_sessions(Live { notice: Some("2 unapplied worker results kept".into()), ..live_of(vec![]) });
        assert!(text(&mut p, 80, 14).iter().any(|r| r.contains("2 unapplied worker results kept")));
        p.set_sessions(live_of(vec![]));
        assert!(!text(&mut p, 80, 14).iter().any(|r| r.contains("unapplied")));
    }

    #[test]
    fn kill_all_needs_something_running() {
        let mut p = JobsPanel::new(vec![job("a", SubagentState::Done)]);
        assert!(matches!(p.on_key(key(KeyCode::Char('X'))), JobsResult::Pending));
    }

    #[test]
    fn a_new_snapshot_keeps_the_selected_job() {
        let mut p = JobsPanel::new(vec![job("a", SubagentState::Running), job("b", SubagentState::Running)]);
        p.at = 1;
        p.set_jobs(vec![
            job("z", SubagentState::Done),
            job("a", SubagentState::Running),
            job("b", SubagentState::Done),
        ]);
        assert_eq!(p.jobs[p.at].name, "b");
        p.set_jobs(vec![job("a", SubagentState::Running)]);
        assert_eq!(p.at, 0, "a vanished selection clamps");
        p.set_jobs(vec![]);
        assert_eq!(p.at, 0);
        assert!(text(&mut p, 60, 12).iter().any(|r| r.contains("no background jobs")));
    }

    #[test]
    fn a_bash_job_shows_its_command_and_note() {
        let mut j = job("bg_1", SubagentState::Failed);
        j.kind = JobKind::Bash;
        j.agent = "bash".into();
        j.task = "cargo test --workspace".into();
        j.note = Some("exit 101".into());
        let mut p = JobsPanel::new(vec![j]);
        let rows = text(&mut p, 80, 14);
        assert!(rows.iter().any(|r| r.contains("$ cargo test --workspace")), "{rows:?}");
        assert!(rows.iter().any(|r| r.contains("failed") && r.contains("exit 101") && !r.contains("tool")), "{rows:?}");
    }

    fn session_path() -> PathBuf {
        let d = std::env::temp_dir().join(format!("mu-panel-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d.join("child.jsonl")
    }

    fn append(p: &std::path::Path, entries: &[serde_json::Value]) {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(p).unwrap();
        for e in entries {
            writeln!(f, "{e}").unwrap();
        }
    }

    fn user_entry(t: &str) -> serde_json::Value {
        serde_json::json!({"type":"message","id":"u","parentId":null,"message":{"role":"user","content":t,"timestamp":1}})
    }

    fn reply_entry(t: &str, cost: f64) -> serde_json::Value {
        serde_json::json!({"type":"message","id":"a","parentId":"u","message":{"role":"assistant",
            "content":[{"type":"text","text":t}],"api":"a","provider":"p","model":"m",
            "usage":{"input":1000,"output":200,"cacheRead":0,"cacheWrite":0,"totalTokens":0,"cost":{"total":cost}},
            "stopReason":"stop","timestamp":2}})
    }

    fn with_session(name: &str, state: SubagentState, path: &std::path::Path) -> SubagentStatus {
        SubagentStatus { session: Some(path.to_path_buf()), ..job(name, state) }
    }

    #[test]
    fn enter_opens_the_transcript_and_it_follows_the_file() {
        let path = session_path();
        append(&path, &[user_entry("Find the bug"), reply_entry("Looking at it", 0.25)]);
        let mut p = JobsPanel::new(vec![with_session("scout-1", SubagentState::Running, &path)]);
        p.refresh();
        let rows = text(&mut p, 80, 20);
        assert!(rows.iter().any(|r| r.contains("scout-1") && r.contains("$0.25")), "cost in the list: {rows:?}");
        assert!(rows.iter().any(|r| r.contains("Enter  read its transcript")), "{rows:?}");

        assert!(matches!(p.on_key(key(KeyCode::Enter)), JobsResult::Pending));
        let rows = text(&mut p, 80, 20);
        assert_eq!(rows.len(), 20, "the transcript takes the height");
        assert!(rows[0].starts_with("── scout-1 · scout"), "{rows:?}");
        assert!(rows[0].contains("$0.25") && rows[0].contains("1.2k tok") && rows[0].contains("following"), "{rows:?}");
        assert!(rows.iter().any(|r| r.contains("Find the bug")) && rows.iter().any(|r| r.contains("Looking at it")));

        append(&path, &[reply_entry("Second thought", 0.5)]);
        p.refresh();
        let rows = text(&mut p, 80, 20);
        assert!(rows.iter().any(|r| r.contains("Second thought")), "{rows:?}");
        assert!(rows[0].contains("$0.75"), "{rows:?}");

        assert!(matches!(p.on_key(key(KeyCode::Char('q'))), JobsResult::Pending));
        assert!(text(&mut p, 80, 20)[0].starts_with("── jobs"), "q goes back to the list");
        assert!(matches!(p.on_key(key(KeyCode::Esc)), JobsResult::Close), "and Esc then closes");
    }

    #[test]
    fn scrolling_up_pauses_following_and_g_resumes_it() {
        let path = session_path();
        let entries: Vec<_> = (0..40).map(|i| reply_entry(&format!("message-{i:02}"), 0.0)).collect();
        append(&path, &entries);
        let mut p = JobsPanel::new(vec![with_session("a", SubagentState::Running, &path)]);
        p.on_key(key(KeyCode::Enter));
        let rows = text(&mut p, 80, 12);
        assert!(rows[0].contains("following") && rows.iter().any(|r| r.contains("message-39")), "{rows:?}");
        p.on_key(key(KeyCode::PageUp));
        let rows = text(&mut p, 80, 12);
        assert!(rows[0].contains("paused") && !rows.iter().any(|r| r.contains("message-39")), "{rows:?}");
        append(&path, &[reply_entry("message-40", 0.0)]);
        p.refresh();
        assert!(text(&mut p, 80, 12)[0].contains("paused"), "new output does not pull a paused view down");
        p.on_key(key(KeyCode::Char('G')));
        let rows = text(&mut p, 80, 12);
        assert!(rows[0].contains("following") && rows.iter().any(|r| r.contains("message-40")), "{rows:?}");
        p.on_key(key(KeyCode::Char('g')));
        let rows = text(&mut p, 80, 12);
        assert!(rows[0].contains("paused") && rows.iter().any(|r| r.contains("message-00")), "{rows:?}");
        p.on_key(key(KeyCode::Char('f')));
        assert!(text(&mut p, 80, 12)[0].contains("following"), "f toggles");
    }

    #[test]
    fn a_job_without_a_session_has_no_transcript_and_a_vanished_job_closes_it() {
        let mut p = JobsPanel::new(vec![job("a", SubagentState::Running)]);
        p.on_key(key(KeyCode::Enter));
        assert!(text(&mut p, 80, 20)[0].starts_with("── jobs"), "nothing to open");

        let path = session_path();
        append(&path, &[user_entry("hi")]);
        let mut p = JobsPanel::new(vec![with_session("b", SubagentState::Done, &path)]);
        p.on_key(key(KeyCode::Enter));
        assert!(text(&mut p, 80, 20)[0].starts_with("── b ·"));
        p.set_jobs(vec![]);
        assert!(text(&mut p, 80, 20).iter().any(|r| r.contains("no background jobs")));
    }

    fn bash_job(name: &str, state: SubagentState) -> SubagentStatus {
        SubagentStatus { kind: JobKind::Bash, agent: "bash".into(), task: "make all".into(), ..job(name, state) }
    }

    #[test]
    fn a_bash_job_opens_its_output_and_follows_what_arrives() {
        let out = HashMap::from([("bg_1".to_string(), "compiling\n\u{1b}[31mred\u{1b}[0m\n".to_string())]);
        let mut p = JobsPanel::new(vec![bash_job("bg_1", SubagentState::Running)]).with_outputs(out);
        let rows = text(&mut p, 80, 20);
        assert!(rows.iter().any(|r| r.contains("Enter  see its output")), "{rows:?}");
        p.on_key(key(KeyCode::Enter));
        let rows = text(&mut p, 80, 20);
        assert!(rows[0].starts_with("── bg_1 · bash") && rows[0].contains("following"), "{rows:?}");
        assert!(rows.iter().any(|r| r.contains("compiling")), "{rows:?}");
        assert!(rows.iter().any(|r| r.trim() == "red"), "escapes are dropped: {rows:?}");
        p.set_output("bg_1", "compiling\nred\nlinking\n");
        let rows = text(&mut p, 80, 20);
        assert!(rows.iter().any(|r| r.contains("linking")), "{rows:?}");
        assert!(matches!(p.on_key(key(KeyCode::Char('x'))), JobsResult::Kill(Some(id)) if id == "bg_1"));

        let mut quiet = JobsPanel::new(vec![bash_job("bg_2", SubagentState::Running)]);
        quiet.on_key(key(KeyCode::Enter));
        assert!(text(&mut quiet, 80, 20).iter().any(|r| r.contains("no output yet")));
    }

    #[test]
    fn escapes_and_carriage_returns_are_cleaned_from_output() {
        assert_eq!(mu_common::text::strip_ansi("a\u{1b}[1;31mb\u{1b}[0mc\u{1b}]0;title\u{7}d\u{1b}Ze"), "abcde");
        let rows: Vec<String> = output_lines("10%\r50%\r100%\nnext\n\nlast", 80).iter().map(line_text).collect();
        assert_eq!(rows, ["100%", "next", "", "last"]);
        let wrapped = output_lines(&"x".repeat(25), 10);
        assert_eq!(wrapped.len(), 3);
    }

    fn lived(path: &std::path::Path, kind: &str, started: &str) -> LiveSession {
        LiveSession {
            path: path.to_path_buf(),
            pid: 4242,
            print_run: kind == "print",
            inbox: kind == "tui",
            started: started.into(),
            kind: Some(kind.into()),
            agent: None,
            cwd: Some(PathBuf::from("/work/other")),
            parent: None,
            abort: false,
            waiting: false,
            client: None,
        }
    }

    #[test]
    fn an_interactive_mu_aborts_if_it_says_it_can_and_terminates_only_after_y() {
        let path = session_path();
        let mut older = lived(&path, "tui", "2026-01-01T00:00:00Z");
        let mut p = sessions_panel(vec![older.clone()]);
        assert!(matches!(p.on_key(key(KeyCode::Char('s'))), JobsResult::Pending), "no abort without the promise");
        older.abort = true;
        let mut p = sessions_panel(vec![older]);
        assert!(matches!(p.on_key(key(KeyCode::Char('s'))), JobsResult::Stop { kind, .. } if kind == "tui"));
        let d = p.render(100, 14, now()).0.iter().map(line_text).collect::<Vec<_>>();
        assert!(d.iter().any(|r| r.contains("s abort its turn") && r.contains("S terminate")), "{d:?}");

        // S asks first; nothing but `y` goes through.
        assert!(matches!(p.on_key(key(KeyCode::Char('S'))), JobsResult::Pending));
        let rows = text(&mut p, 140, 14);
        assert!(rows.last().unwrap().contains("terminate tui (pid 4242)?"), "{rows:?}");
        assert!(matches!(p.on_key(key(KeyCode::Char('n'))), JobsResult::Pending), "another key cancels");
        assert!(p.confirm.is_none() && p.input.is_none(), "and is not taken as `n`");
        p.on_key(key(KeyCode::Char('S')));
        assert!(
            matches!(p.on_key(key(KeyCode::Char('y'))), JobsResult::Terminate { pid: 4242, session, .. } if session == path)
        );
        assert!(p.confirm.is_none());
    }

    #[test]
    fn only_an_interactive_mu_can_be_terminated() {
        let t = "2026-01-01T00:00:00Z";
        let mut p = sessions_panel(vec![
            lived(&session_path(), "print", t),
            lived(&session_path(), "subagent", "2026-01-01T00:00:01Z"),
            lived(&session_path(), "acp", "2026-01-01T00:00:02Z"),
        ]);
        for i in 0..3 {
            p.s_at = i;
            p.on_key(key(KeyCode::Char('S')));
            assert!(p.confirm.is_none(), "row {i}");
        }
    }

    /// A panel on the Sessions tab with these sessions.
    fn sessions_panel(lives: Vec<LiveSession>) -> JobsPanel {
        let mut p = JobsPanel::new(vec![]);
        p.tab = Tab::Sessions;
        p.set_sessions(live_of(lives));
        p
    }

    #[test]
    fn tab_switches_between_jobs_and_sessions() {
        let mut p = JobsPanel::new(vec![job("a", SubagentState::Running)]);
        p.polled_totals = Some(Instant::now());
        assert!(text(&mut p, 80, 14)[0].starts_with("── jobs ·"));
        p.on_key(key(KeyCode::Tab));
        assert_eq!(p.tab, Tab::Sessions);
        p.set_sessions(live_of(vec![lived(&session_path(), "print", "2026-01-01T00:00:00Z")]));
        let rows = text(&mut p, 80, 14);
        assert!(rows[0].starts_with("── sessions · 1 live"), "{rows:?}");
        assert!(rows.iter().any(|r| r.contains("print") && r.contains("4242")), "{rows:?}");
        assert!(rows.last().unwrap().contains("n message") && rows.last().unwrap().contains("Tab jobs"));
        p.on_key(key(KeyCode::BackTab));
        assert_eq!(p.tab, Tab::Jobs);
    }

    #[test]
    fn an_empty_sessions_tab_says_so() {
        let mut p = sessions_panel(vec![]);
        assert!(text(&mut p, 80, 14).iter().any(|r| r.contains("no other mu sessions are running")));
        for c in ['n', 's', 't'] {
            assert!(matches!(p.on_key(key(KeyCode::Char(c))), JobsResult::Pending));
        }
        assert!(p.input.is_none());
    }

    #[test]
    fn enter_peeks_at_another_sessions_transcript() {
        let path = session_path();
        append(&path, &[user_entry("Fix the flaky test"), reply_entry("On it.", 0.5)]);
        let mut p = sessions_panel(vec![lived(&path, "print", "2026-01-01T00:00:00Z")]);
        p.refresh();
        assert!(text(&mut p, 80, 20).iter().any(|r| r.contains("$0.50")), "cost in the list");
        p.on_key(key(KeyCode::Enter));
        let rows = text(&mut p, 80, 20);
        assert!(rows[0].starts_with("── print · pid 4242") && rows[0].contains("$0.50"), "{rows:?}");
        assert!(rows.iter().any(|r| r.contains("Fix the flaky test")) && rows.iter().any(|r| r.contains("On it.")));
        assert!(rows.last().unwrap().contains("q back") && !rows.last().unwrap().contains("x kill"), "{rows:?}");
        assert!(matches!(p.on_key(key(KeyCode::Char('x'))), JobsResult::Pending), "no kill from a session view");
        append(&path, &[reply_entry("Done.", 0.25)]);
        p.refresh();
        assert!(text(&mut p, 80, 20).iter().any(|r| r.contains("Done.")), "a session keeps being followed");
        p.on_key(key(KeyCode::Char('q')));
        assert!(text(&mut p, 80, 20)[0].starts_with("── sessions"), "q goes back to the Sessions tab");
    }

    #[test]
    fn stop_and_take_are_offered_only_where_they_work() {
        let t = "2026-01-01T00:00:00Z";
        let (a, b, c) = (session_path(), session_path(), session_path());
        let mut p = sessions_panel(vec![
            lived(&a, "print", t),
            lived(&b, "subagent", "2026-01-01T00:00:01Z"),
            lived(&c, "tui", "2026-01-01T00:00:02Z"),
        ]);
        assert_eq!(p.rows.len(), 3);
        p.s_at = 0;
        assert!(
            matches!(p.on_key(key(KeyCode::Char('s'))), JobsResult::Stop { pid: 4242, kind, .. } if kind == "print")
        );
        assert!(matches!(p.on_key(key(KeyCode::Char('t'))), JobsResult::Take { session } if session == a));
        p.s_at = 1;
        assert!(matches!(p.on_key(key(KeyCode::Char('s'))), JobsResult::Stop { kind, .. } if kind == "subagent"));
        assert!(matches!(p.on_key(key(KeyCode::Char('t'))), JobsResult::Pending), "only -p runs hand over");
        p.s_at = 2;
        assert!(matches!(p.on_key(key(KeyCode::Char('s'))), JobsResult::Pending));
        assert!(matches!(p.on_key(key(KeyCode::Char('t'))), JobsResult::Pending));
    }

    #[test]
    fn a_message_is_typed_in_the_footer_and_sent_with_enter() {
        let path = session_path();
        let mut p = sessions_panel(vec![lived(&path, "tui", "2026-01-01T00:00:00Z")]);
        p.on_key(key(KeyCode::Char('n')));
        assert!(p.input.is_some());
        for ch in "hello wide world".chars() {
            p.on_key(key(KeyCode::Char(ch)));
        }
        let (rows, cursor) = p.render(80, 14, now());
        let last = line_text(rows.last().unwrap());
        assert!(last.starts_with("message to tui") && last.ends_with("hello wide world"), "{last}");
        assert_eq!(cursor, (rows.len() - 1, last.chars().count()), "the cursor sits at the end of the text");
        p.on_paste("!\nnext");
        p.on_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
        match p.on_key(key(KeyCode::Enter)) {
            JobsResult::Notify { session, text } => {
                assert_eq!(session, path);
                assert_eq!(text, "hello wide world!", "sent trimmed");
            }
            _ => panic!("no message"),
        }
        assert!(p.input.is_none());

        p.on_key(key(KeyCode::Char('n')));
        p.on_key(key(KeyCode::Char('x')));
        assert!(matches!(p.on_key(key(KeyCode::Esc)), JobsResult::Pending), "Esc drops the message, not the panel");
        assert!(p.input.is_none());
        p.on_key(key(KeyCode::Char('n')));
        assert!(matches!(p.on_key(key(KeyCode::Enter)), JobsResult::Pending), "an empty message is not sent");
    }

    #[test]
    fn what_an_action_did_shows_until_the_next_key() {
        let mut p = sessions_panel(vec![lived(&session_path(), "tui", "2026-01-01T00:00:00Z")]);
        p.set_status("message to a: delivered (pid 7)");
        assert!(text(&mut p, 80, 14).last().unwrap().contains("delivered (pid 7)"));
        p.on_key(key(KeyCode::Char('j')));
        assert!(text(&mut p, 80, 14).last().unwrap().contains("Tab jobs"));
    }

    #[test]
    fn a_new_list_of_sessions_keeps_the_selected_one() {
        let (a, b) = (session_path(), session_path());
        let (ta, tb) = ("2026-01-01T00:00:01Z", "2026-01-01T00:00:02Z");
        let mut p = sessions_panel(vec![lived(&a, "print", ta), lived(&b, "print", tb)]);
        p.s_at = 1;
        let mut c = lived(&session_path(), "print", "2026-01-01T00:00:00Z");
        c.pid = 1;
        p.set_sessions(live_of(vec![lived(&b, "print", tb), c, lived(&a, "print", ta)]));
        assert_eq!(p.rows[p.s_at].live.path, b);
        p.set_sessions(live_of(vec![]));
        assert_eq!(p.s_at, 0);
    }

    #[test]
    fn a_missing_session_file_says_it_is_waiting_and_kill_works_from_the_view() {
        let path = session_path();
        let mut p = JobsPanel::new(vec![with_session("a", SubagentState::Running, &path)]);
        p.on_key(key(KeyCode::Enter));
        assert!(text(&mut p, 80, 12).iter().any(|r| r.contains("waiting for the session file")));
        assert!(matches!(p.on_key(key(KeyCode::Char('x'))), JobsResult::Kill(Some(id)) if id == "a"));
        assert!(text(&mut p, 80, 12).iter().any(|r| r.contains("waiting for the session file")), "the view stays");
        p.set_jobs(vec![with_session("a", SubagentState::Failed, &path)]);
        assert!(matches!(p.on_key(key(KeyCode::Char('x'))), JobsResult::Pending), "a finished job is not killed");
    }

    #[test]
    fn long_tasks_wrap_and_rows_never_exceed_the_width() {
        let mut j = job("a", SubagentState::Running);
        j.task = "word ".repeat(60);
        let mut p = JobsPanel::new(vec![j]);
        for w in [20, 40, 80] {
            let (rows, _) = p.render(w, 14, now());
            for r in &rows {
                assert!(line::line_width(r) <= w, "width {w}: {:?}", line_text(r));
            }
        }
    }
}
