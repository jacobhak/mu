//! The jobs panel (`Alt+J`, `/jobs`): this session's background jobs as a list with the selected
//! job's details below, and a full-height view of a subagent's session transcript that follows the
//! file while the subagent runs. See `design/jobs-panel.md`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use super::line::{self, Line, dim, span};
use super::reply::{accent, rule};
use super::session_view;
use super::theme::{self, Role};
use crate::events::{JobKind, SubagentState, SubagentStatus};
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
}

pub struct JobsPanel {
    jobs: Vec<SubagentStatus>,
    at: usize,
    top: usize,
    /// Rows the list showed last, for paging.
    page: usize,
    /// Running totals of each listed subagent's session, for the cost column.
    totals: HashMap<PathBuf, Totals>,
    polled_totals: Option<Instant>,
    view: Option<View>,
}

/// A listed session's totals, and whether it can still change.
struct Totals {
    tail: Tail,
    /// Read to the end after its job finished.
    settled: bool,
}

/// The full-height transcript of one subagent's session.
struct View {
    /// The job it shows.
    name: String,
    tail: Tail,
    settled: bool,
    polled: Option<Instant>,
    lines: Vec<Line>,
    /// What `lines` was drawn from: messages seen, width, caught up.
    built: Option<(usize, usize, bool)>,
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

    fn build(&mut self, width: usize) {
        let seen = self.tail.messages().len() + self.tail.dropped();
        let key = (seen, width, self.tail.caught_up());
        if self.built == Some(key) {
            return;
        }
        self.built = Some(key);
        let mut l: Vec<Line> = Vec::new();
        if self.tail.dropped() > 0 {
            l.push(line::styled(format!("  … {} earlier messages not shown", self.tail.dropped()), |_| dim()));
        }
        l.extend(session_view::lines(self.tail.messages(), width));
        if l.is_empty() {
            let why = if self.tail.caught_up() { "  nothing yet" } else { "  waiting for the session file…" };
            l.push(line::styled(why, |_| dim()));
        }
        self.lines = l;
    }
}

impl JobsPanel {
    pub fn new(jobs: Vec<SubagentStatus>) -> Self {
        // Open on the newest running job, else the newest job.
        let at = jobs.iter().rposition(|j| j.state == SubagentState::Running).unwrap_or(jobs.len().saturating_sub(1));
        Self { jobs, at, top: 0, page: LIST_MAX, totals: HashMap::new(), polled_totals: None, view: None }
    }

    /// A fresh snapshot: the selection stays on the same job when it is still there.
    pub fn set_jobs(&mut self, jobs: Vec<SubagentStatus>) {
        let name = self.jobs.get(self.at).map(|j| j.name.clone());
        self.jobs = jobs;
        self.at = name
            .and_then(|n| self.jobs.iter().position(|j| j.name == n))
            .unwrap_or_else(|| self.at.min(self.jobs.len().saturating_sub(1)));
        if self.view.as_ref().is_some_and(|v| !self.jobs.iter().any(|j| j.name == v.name)) {
            self.view = None;
        }
        let live: Vec<&PathBuf> = self.jobs.iter().filter_map(|j| j.session.as_ref()).collect();
        self.totals.retain(|p, _| live.contains(&p));
    }

    /// Reads what the sessions gained since the last read: the open transcript and the cost of every
    /// listed subagent. `render` does this on a timer; tests and the first frame call it directly.
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
        let state =
            self.view.as_ref().and_then(|v| self.jobs.iter().find(|j| j.name == v.name)).map(|j| j.state.clone());
        if let (Some(v), Some(state)) = (&mut self.view, state) {
            v.polled = Some(Instant::now());
            if !v.settled {
                v.tail.poll();
                v.settled = state != SubagentState::Running && v.tail.caught_up();
            }
        }
    }

    fn refresh_due(&mut self) {
        let now = Instant::now();
        let totals_due = self.polled_totals.is_none_or(|t| now.duration_since(t) >= TOTALS_POLL);
        let view_due = self.view.as_ref().is_some_and(|v| v.polled.is_none_or(|t| now.duration_since(t) >= VIEW_POLL));
        if totals_due || view_due {
            self.refresh();
        }
    }

    /// `$0.03`, `…` while the session is still being read, empty when there is no session.
    fn cost_cell(&self, j: &SubagentStatus) -> String {
        match j.session.as_ref().and_then(|p| self.totals.get(p)) {
            Some(t) if t.tail.caught_up() => format!("${:.2}", t.tail.cost()),
            Some(_) => "…".into(),
            None => String::new(),
        }
    }

    fn running(&self) -> usize {
        self.jobs.iter().filter(|j| j.state == SubagentState::Running).count()
    }

    /// Opens the selected subagent's transcript, when it has a session file yet.
    fn open_view(&mut self) {
        let Some(j) = self.jobs.get(self.at) else { return };
        let Some(path) = j.session.clone() else { return };
        self.view = Some(View {
            name: j.name.clone(),
            tail: Tail::new(path, VIEW_KEEP),
            settled: false,
            polled: None,
            lines: Vec::new(),
            built: None,
            top: 0,
            follow: true,
            max_top: 0,
            body_h: 1,
        });
        self.refresh();
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
            KeyCode::Char('x') => {
                let name = v.name.clone();
                if self.jobs.iter().any(|j| j.name == name && j.state == SubagentState::Running) {
                    return JobsResult::Kill(Some(name));
                }
            }
            _ => {}
        }
        JobsResult::Pending
    }

    pub fn on_key(&mut self, k: KeyEvent) -> JobsResult {
        if k.kind == KeyEventKind::Release {
            return JobsResult::Pending;
        }
        if self.view.is_some() {
            return self.view_key(k);
        }
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

    /// The panel's rows (at most `max_rows`) and the cursor within them. `now` times running jobs.
    pub fn render(&mut self, width: usize, max_rows: usize, now: SystemTime) -> (Vec<Line>, (usize, usize)) {
        self.refresh_due();
        if self.view.is_some() {
            return self.render_view(width, max_rows, now);
        }
        let n = self.jobs.len();
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
        self.page = list_h;
        if self.at < self.top {
            self.top = self.at;
        }
        if self.at >= self.top + list_h {
            self.top = self.at + 1 - list_h;
        }
        self.top = self.top.min(n.saturating_sub(list_h));

        let mut out: Vec<Line> = Vec::new();
        if header == 1 {
            let (run, done) = (self.running(), n - self.running());
            out.push(rule(Some(&format!("jobs · {run} running · {done} finished")), width));
        }
        let name_w = self.jobs.iter().map(|j| j.name.chars().count()).max().unwrap_or(0).min(NAME_MAX);
        let agent_w = self.jobs.iter().map(|j| j.agent.chars().count()).max().unwrap_or(0).min(AGENT_MAX);
        if n == 0 {
            out.push(vec![span("  no background jobs", dim())]);
        }
        let end = (self.top + list_h).min(n);
        for (i, j) in self.jobs.iter().enumerate().take(end).skip(self.top) {
            let cost = self.cost_cell(j);
            out.push(list_row(j, i == self.at, (name_w, agent_w), &cost, width, now));
        }
        // Blank-pad the list so the details start on a fixed row.
        let shown = if n == 0 { 1 } else { end - self.top };
        out.extend(std::iter::repeat_n(Vec::new(), list_h.saturating_sub(shown)));
        if split {
            if let Some(j) = self.jobs.get(self.at) {
                out.push(rule(Some(&format!("{} · {}", j.name, j.agent)), width));
                out.extend(detail(j, &self.cost_cell(j), width, detail_h, now));
            } else {
                out.push(rule(None, width));
                out.extend(std::iter::repeat_n(Vec::new(), detail_h));
            }
            out.push(rule(None, width));
        }
        if tail >= 1 {
            out.push(line::styled(
                "j/k move · Enter transcript · x kill · X kill all · g/G first/last · q close",
                |_| dim(),
            ));
        }
        out.truncate(max_rows);
        let out: Vec<Line> = out.iter().map(|l| line::truncate(l, width)).collect();
        let cursor = header + self.at.saturating_sub(self.top).min(list_h.saturating_sub(1));
        (out, (cursor, 0))
    }

    /// The transcript of the open job: a titled rule, the window of lines, a closing rule and keys.
    fn render_view(&mut self, width: usize, max_rows: usize, now: SystemTime) -> (Vec<Line>, (usize, usize)) {
        let header = usize::from(max_rows >= 3);
        let footer = if max_rows >= 5 { 2 } else { 0 };
        let h = max_rows.saturating_sub(header + footer).max(1);
        let job = self.view.as_ref().and_then(|v| self.jobs.iter().find(|j| j.name == v.name)).cloned();
        let cost = job.as_ref().map(|j| self.cost_cell(j)).unwrap_or_default();
        let Some(v) = self.view.as_mut() else { return (Vec::new(), (0, 0)) };
        v.build(width);
        v.body_h = h;
        v.max_top = v.lines.len().saturating_sub(h);
        let top = if v.follow { v.max_top } else { v.top.min(v.max_top) };
        let mut out: Vec<Line> = Vec::new();
        if header == 1 {
            let mut title = v.name.clone();
            if let Some(j) = &job {
                title.push_str(&format!(" · {} · {} {}", j.agent, mark(&j.state).0, clock(elapsed(j, now))));
            }
            if !cost.is_empty() {
                title.push_str(&format!(" · {cost}"));
            }
            if v.tail.tokens() > 0 {
                title.push_str(&format!(" · {} tok", short_count(v.tail.tokens())));
            }
            title.push_str(if v.follow { " · following" } else { " · paused" });
            out.push(rule(Some(&title), width));
        }
        let end = (top + h).min(v.lines.len());
        out.extend(v.lines[top..end].iter().cloned());
        out.extend(std::iter::repeat_n(Vec::new(), h - (end - top)));
        if footer == 2 {
            out.push(rule(None, width));
            out.push(line::styled("j/k scroll · PgUp/PgDn · g/G top/bottom · f follow · x kill · q back", |_| {
                dim()
            }));
        }
        let out: Vec<Line> = out.iter().map(|l| line::truncate(l, width)).collect();
        (out, (header, 0))
    }
}

fn mark(state: &SubagentState) -> (char, Role) {
    match state {
        SubagentState::Running => ('◌', Role::Accent),
        SubagentState::Done => ('✓', Role::Ok),
        SubagentState::Failed => ('✗', Role::Err),
    }
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

fn pad(s: &str, w: usize) -> String {
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
fn wrap(text: &str, width: usize) -> Vec<String> {
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

/// What the panel knows about one job, cut to `h` rows.
fn detail(j: &SubagentStatus, cost: &str, width: usize, h: usize, now: SystemTime) -> Vec<Line> {
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
    if j.kind == JobKind::Subagent {
        let hint = if j.session.is_some() { "Enter  read its transcript" } else { "no session file yet" };
        rows.push(vec![span(format!("   {hint}"), t.fg(Role::Faint))]);
    }
    rows.truncate(h);
    rows.resize(h, Vec::new());
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
