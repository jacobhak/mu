//! The jobs panel (`Alt+J`, `/jobs`). The Jobs tab lists this session's background jobs with the
//! selected job's details below; the Sessions tab lists every other live mu process. `Enter` opens a
//! full-height view: a subagent's or another session's transcript, followed while it is written, or a
//! bash job's output. See `design/jobs-panel.md`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use super::line::{self, Line, dim, span};
use super::reply::{accent, rule};
use super::session_view;
use super::sessions_tab::{self, Row};
use super::theme::{self, Role};
use crate::events::{JobKind, SubagentState, SubagentStatus};
use crate::session::LiveSession;
use crate::session_tail::Tail;

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
    /// hands since is left alone; `kind` is `print` or `subagent`.
    Stop {
        session: PathBuf,
        pid: u32,
        started: String,
        kind: String,
    },
    /// Take over a `-p` run (`mu take`).
    Take {
        session: PathBuf,
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
    /// Running totals of each listed session, for the cost column.
    totals: HashMap<PathBuf, Totals>,
    polled_totals: Option<Instant>,
    view: Option<View>,
    /// A message being typed to a session.
    input: Option<Compose>,
    /// What the last action did, shown in place of the key line until the next key.
    status: Option<String>,
    /// The newest output of each bash job, as the app last sent it.
    outputs: HashMap<String, String>,
}

/// A listed session's totals, and whether it can still change.
struct Totals {
    tail: Tail,
    /// Read to the end after its job finished.
    settled: bool,
}

/// A one-line message for `mu notify`.
struct Compose {
    session: PathBuf,
    label: String,
    text: String,
}

/// The full-height view of one thing: a subagent's session transcript, another session's, or a bash
/// job's output.
struct View {
    /// The job it shows, or the session's id.
    name: String,
    /// Set for a session of the Sessions tab: what to call it in the title.
    label: Option<String>,
    /// The session being followed; `None` for a bash job.
    tail: Option<Tail>,
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
            totals: HashMap::new(),
            polled_totals: None,
            view: None,
            input: None,
            status: None,
            outputs: HashMap::new(),
        }
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

    /// The live sessions, read now from the owner records.
    fn refresh_sessions(&mut self) {
        self.set_sessions(crate::session::live_sessions());
    }

    /// A fresh list of live sessions: the selection stays on the same one when it is still there.
    pub fn set_sessions(&mut self, lives: Vec<LiveSession>) {
        self.polled_totals = Some(Instant::now());
        for l in &lives {
            if !self.titles.contains_key(&l.path)
                && let Some(i) = crate::session::info(&l.path)
            {
                let t = i.title.filter(|t| !t.is_empty()).unwrap_or(i.first_prompt);
                if !t.is_empty() {
                    self.titles.insert(l.path.clone(), t);
                }
            }
        }
        let keep = self.rows.get(self.s_at).map(|r| r.live.path.clone());
        self.rows = sessions_tab::build(lives, &self.titles, &self.jobs, self.own.as_deref());
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

    /// Reads what the sessions gained since the last read: the open transcript and the cost of every
    /// listed subagent and session. `render` does this on a timer; tests and the first frame call it
    /// directly. (The list of live sessions is read by `refresh_sessions`, from `refresh_due`.)
    pub fn refresh(&mut self) {
        self.polled_totals = Some(Instant::now());
        for j in &self.jobs {
            let Some(p) = &j.session else { continue };
            let t = self
                .totals
                .entry(p.clone())
                .or_insert_with(|| Totals { tail: Tail::new(p.clone(), 0), settled: false });
            if !t.settled {
                t.tail.poll();
                t.settled = j.state != SubagentState::Running && t.tail.caught_up();
            }
        }
        for r in &self.rows {
            let t = self
                .totals
                .entry(r.live.path.clone())
                .or_insert_with(|| Totals { tail: Tail::new(r.live.path.clone(), 0), settled: false });
            t.tail.poll();
        }
        // A job's transcript is done once the job is and the file is read; a session's never is.
        let job_state = self
            .view
            .as_ref()
            .filter(|v| v.label.is_none())
            .and_then(|v| self.jobs.iter().find(|j| j.name == v.name))
            .map(|j| j.state.clone());
        if let Some(v) = &mut self.view {
            v.polled = Some(Instant::now());
            if !v.settled
                && let Some(t) = &mut v.tail
            {
                t.poll();
                v.settled = job_state.is_some_and(|s| s != SubagentState::Running) && t.caught_up();
            }
        }
    }

    fn refresh_due(&mut self) {
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

    fn new_view(name: String, label: Option<String>, tail: Option<Tail>) -> View {
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
            (Some(path), _) => Some(Tail::new(path.clone(), VIEW_KEEP)),
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
        self.view = Some(Self::new_view(r.id(), Some(label), Some(Tail::new(r.live.path.clone(), VIEW_KEEP))));
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
            KeyCode::Char('c') if ctrl => return JobsResult::Close,
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
            KeyCode::Char('c') if ctrl => self.input = None,
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

    pub fn on_key(&mut self, k: KeyEvent) -> JobsResult {
        if k.kind == KeyEventKind::Release {
            return JobsResult::Pending;
        }
        self.status = None;
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
            KeyCode::Char('c') if ctrl => return JobsResult::Close,
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
            KeyCode::Char('X') if self.running() > 0 => return JobsResult::Kill(None),
            _ => {}
        }
        JobsResult::Pending
    }

    fn sessions_key(&mut self, k: KeyEvent) -> JobsResult {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let last = self.rows.len().saturating_sub(1);
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') => return JobsResult::Close,
            KeyCode::Char('c') if ctrl => return JobsResult::Close,
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
            _ => {}
        }
        JobsResult::Pending
    }

    /// The panel's rows (at most `max_rows`) and the cursor within them. `now` times running jobs.
    pub fn render(&mut self, width: usize, max_rows: usize, now: SystemTime) -> (Vec<Line>, (usize, usize)) {
        self.refresh_due();
        if self.view.is_some() {
            return self.render_view(width, max_rows, now);
        }
        let mut frame = match self.tab {
            Tab::Jobs => self.jobs_frame(width, now),
            Tab::Sessions => self.sessions_frame(width, now),
        };
        if let Some(s) = &self.status {
            frame.footer = vec![span(s.clone(), accent())];
        }
        let (top, page) = match self.tab {
            Tab::Jobs => (&mut self.top, &mut self.page),
            Tab::Sessions => (&mut self.s_top, &mut self.s_page),
        };
        draw(frame, top, page, width, max_rows)
    }

    fn jobs_frame(&self, width: usize, now: SystemTime) -> Frame {
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
            Some(j) => (Some(format!("{} · {}", j.name, j.agent)), detail(j, &self.cost_cell(j), width, now)),
            None => (None, Vec::new()),
        };
        Frame {
            title: format!("jobs · {run} running · {done} finished"),
            rows,
            at: self.at,
            empty: "  no background jobs",
            detail_title,
            detail: detail_rows,
            footer: line::styled(
                "j/k move · Enter transcript · x kill · X kill all · Tab sessions · q close",
                |_| dim(),
            ),
            footer_cursor: None,
        }
    }

    fn sessions_frame(&self, width: usize, now: SystemTime) -> Frame {
        let label_w = self.rows.iter().map(|r| r.depth * 2 + r.label().chars().count()).max().unwrap_or(0).min(26);
        let rows = self
            .rows
            .iter()
            .enumerate()
            .map(|(i, r)| {
                sessions_tab::list_row(r, i == self.s_at, label_w, &self.cost_of(Some(&r.live.path)), width, now)
            })
            .collect();
        let (detail_title, detail_rows) = match self.rows.get(self.s_at) {
            Some(r) => (Some(r.label()), sessions_tab::detail(r, &self.cost_of(Some(&r.live.path)), width, now)),
            None => (None, Vec::new()),
        };
        let (footer, footer_cursor) = match &self.input {
            Some(c) => {
                let lead = format!("message to {} (Enter send, Esc cancel): ", c.label);
                let col = lead.chars().count() + c.text.chars().count();
                (vec![span(lead, dim()), span(c.text.clone(), Default::default())], Some(col))
            }
            None => (
                line::styled(
                    "j/k move · Enter peek · n message · s stop · t take over · Tab jobs · q close",
                    |_| dim(),
                ),
                None,
            ),
        };
        Frame {
            title: format!("sessions · {} live", self.rows.len()),
            rows,
            at: self.s_at,
            empty: "  no other mu sessions are running",
            detail_title,
            detail: detail_rows,
            footer,
            footer_cursor,
        }
    }

    /// The transcript of the open job: a titled rule, the window of lines, a closing rule and keys.
    fn render_view(&mut self, width: usize, max_rows: usize, now: SystemTime) -> (Vec<Line>, (usize, usize)) {
        let header = usize::from(max_rows >= 3);
        let footer = if max_rows >= 5 { 2 } else { 0 };
        let h = max_rows.saturating_sub(header + footer).max(1);
        let job = self
            .view
            .as_ref()
            .filter(|v| v.label.is_none())
            .and_then(|v| self.jobs.iter().find(|j| j.name == v.name))
            .cloned();
        let output = self.view.as_ref().and_then(|v| self.outputs.get(&v.name)).map(String::as_str);
        let Some(v) = self.view.as_mut() else { return (Vec::new(), (0, 0)) };
        v.build(width, output);
        v.body_h = h;
        v.max_top = v.lines.len().saturating_sub(h);
        let top = if v.follow { v.max_top } else { v.top.min(v.max_top) };
        let mut out: Vec<Line> = Vec::new();
        if header == 1 {
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
            out.push(rule(Some(&title), width));
        }
        let end = (top + h).min(v.lines.len());
        out.extend(v.lines[top..end].iter().cloned());
        out.extend(std::iter::repeat_n(Vec::new(), h - (end - top)));
        if footer == 2 {
            out.push(rule(None, width));
            let keys = if v.label.is_some() {
                "j/k scroll · PgUp/PgDn · g/G top/bottom · f follow · q back"
            } else {
                "j/k scroll · PgUp/PgDn · g/G top/bottom · f follow · x kill · q back"
            };
            out.push(line::styled(keys, |_| dim()));
        }
        let out: Vec<Line> = out.iter().map(|l| line::truncate(l, width)).collect();
        (out, (header, 0))
    }
}

/// One tab's screen: a title, the list rows (all of them) and which is selected, the selected row's
/// details, and the key line. `draw` fits it to the rows there are.
struct Frame {
    title: String,
    rows: Vec<Line>,
    at: usize,
    empty: &'static str,
    detail_title: Option<String>,
    detail: Vec<Line>,
    footer: Line,
    /// A column in the footer where the cursor goes (a message being typed).
    footer_cursor: Option<usize>,
}

/// Lays a frame out in `max_rows`: a titled rule, a window of the list, the details between rules and
/// the key line; with too few rows only the list. `top` and `page` are the tab's scroll state.
fn draw(f: Frame, top: &mut usize, page: &mut usize, width: usize, max_rows: usize) -> (Vec<Line>, (usize, usize)) {
    let n = f.rows.len();
    let header = usize::from(max_rows >= 2);
    let split = max_rows >= CHROME + 1 + DETAIL_MIN;
    let (list_h, detail_h, tail) = if split {
        let body = max_rows - CHROME;
        let list_h = n.clamp(1, LIST_MAX).min(body - DETAIL_MIN);
        (list_h, (body - list_h).min(DETAIL_MAX), CHROME - 1)
    } else {
        let footer = usize::from(max_rows >= 3);
        (max_rows.saturating_sub(header + footer).max(1), 0, footer)
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
    if header == 1 {
        out.push(rule(Some(&f.title), width));
    }
    if n == 0 {
        out.push(vec![span(f.empty, dim())]);
    }
    let end = (*top + list_h).min(n);
    out.extend(f.rows[*top..end].iter().cloned());
    // Blank-pad the list so the details start on a fixed row.
    let shown = if n == 0 { 1 } else { end - *top };
    out.extend(std::iter::repeat_n(Vec::new(), list_h.saturating_sub(shown)));
    if split {
        out.push(rule(f.detail_title.as_deref(), width));
        let mut d = f.detail;
        d.truncate(detail_h);
        d.resize(detail_h, Vec::new());
        out.extend(d);
        out.push(rule(None, width));
    }
    let mut cursor = (header + f.at.saturating_sub(*top).min(list_h.saturating_sub(1)), 0);
    if tail >= 1 {
        if let Some(col) = f.footer_cursor {
            cursor = (out.len(), col.min(width.saturating_sub(1)));
        }
        out.push(f.footer);
    }
    out.truncate(max_rows);
    let out: Vec<Line> = out.iter().map(|l| line::truncate(l, width)).collect();
    (out, cursor)
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
    for raw in strip_ansi(text).lines() {
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

/// `s` without CSI and OSC sequences and lone escapes.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match it.peek() {
            Some('[') => {
                it.next();
                for n in it.by_ref() {
                    if ('@'..='~').contains(&n) {
                        break;
                    }
                }
            }
            Some(']') => {
                it.next();
                while let Some(n) = it.next() {
                    if n == '\u{7}' {
                        break;
                    }
                    if n == '\u{1b}' {
                        it.next();
                        break;
                    }
                }
            }
            _ => {
                it.next();
            }
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

pub(super) fn pad(s: &str, w: usize) -> String {
    let t = line::truncate_text(s, w);
    let used = t.chars().count();
    format!("{t}{}", " ".repeat(w.saturating_sub(used)))
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

/// Wraps `text` to `width` columns on spaces; words longer than a row are cut by `truncate`.
pub(super) fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut rows = Vec::new();
    let mut cur = String::new();
    for w in text.split_whitespace() {
        let fits = cur.is_empty() || cur.chars().count() + 1 + w.chars().count() <= width;
        if !fits {
            rows.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(w);
    }
    if !cur.is_empty() {
        rows.push(cur);
    }
    rows
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
    use crate::tui::line::line_text;
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
        }
    }

    fn key(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    fn now() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1012)
    }

    fn text(p: &mut JobsPanel, w: usize, h: usize) -> Vec<String> {
        p.render(w, h, now()).0.iter().map(line_text).collect()
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
        assert!(matches!(p.on_key(key(KeyCode::Char('X'))), JobsResult::Kill(None)));
        p.on_key(key(KeyCode::Down));
        assert_eq!(p.at, 1, "stops at the last job");
        p.on_key(key(KeyCode::Char('k')));
        assert_eq!(p.at, 0);
        assert!(matches!(p.on_key(key(KeyCode::Char('q'))), JobsResult::Close));
        assert!(matches!(p.on_key(key(KeyCode::Esc)), JobsResult::Close));
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(matches!(p.on_key(ctrl_c), JobsResult::Close));
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
        assert_eq!(strip_ansi("a\u{1b}[1;31mb\u{1b}[0mc\u{1b}]0;title\u{7}d\u{1b}Ze"), "abcde");
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
        }
    }

    /// A panel on the Sessions tab with these sessions, not reading `$MU_HOME`.
    fn sessions_panel(lives: Vec<LiveSession>) -> JobsPanel {
        let mut p = JobsPanel::new(vec![]);
        p.tab = Tab::Sessions;
        p.set_sessions(lives);
        p
    }

    #[test]
    fn tab_switches_between_jobs_and_sessions() {
        let mut p = JobsPanel::new(vec![job("a", SubagentState::Running)]);
        p.polled_totals = Some(Instant::now());
        assert!(text(&mut p, 80, 14)[0].starts_with("── jobs ·"));
        p.on_key(key(KeyCode::Tab));
        assert_eq!(p.tab, Tab::Sessions);
        // Not reading the real run directory: the tab switch did that once; what it found is replaced.
        p.set_sessions(vec![lived(&session_path(), "print", "2026-01-01T00:00:00Z")]);
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
        p.set_sessions(vec![lived(&b, "print", tb), c, lived(&a, "print", ta)]);
        assert_eq!(p.rows[p.s_at].live.path, b);
        p.set_sessions(vec![]);
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
