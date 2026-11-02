//! The jobs panel (`Alt+J`, `/jobs`): this session's background jobs as a list with the selected
//! job's details below. See `design/jobs-panel.md`.

use std::time::SystemTime;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use super::line::{self, Line, dim, span};
use super::reply::{accent, rule};
use super::theme::{self, Role};
use crate::events::{JobKind, SubagentState, SubagentStatus};

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
}

impl JobsPanel {
    pub fn new(jobs: Vec<SubagentStatus>) -> Self {
        // Open on the newest running job, else the newest job.
        let at = jobs.iter().rposition(|j| j.state == SubagentState::Running).unwrap_or(jobs.len().saturating_sub(1));
        Self { jobs, at, top: 0, page: LIST_MAX }
    }

    /// A fresh snapshot: the selection stays on the same job when it is still there.
    pub fn set_jobs(&mut self, jobs: Vec<SubagentStatus>) {
        let name = self.jobs.get(self.at).map(|j| j.name.clone());
        self.jobs = jobs;
        self.at = name
            .and_then(|n| self.jobs.iter().position(|j| j.name == n))
            .unwrap_or_else(|| self.at.min(self.jobs.len().saturating_sub(1)));
    }

    fn running(&self) -> usize {
        self.jobs.iter().filter(|j| j.state == SubagentState::Running).count()
    }

    pub fn on_key(&mut self, k: KeyEvent) -> JobsResult {
        if k.kind == KeyEventKind::Release {
            return JobsResult::Pending;
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
            out.push(list_row(j, i == self.at, (name_w, agent_w), width, now));
        }
        // Blank-pad the list so the details start on a fixed row.
        let shown = if n == 0 { 1 } else { end - self.top };
        out.extend(std::iter::repeat_n(Vec::new(), list_h.saturating_sub(shown)));
        if split {
            if let Some(j) = self.jobs.get(self.at) {
                out.push(rule(Some(&format!("{} · {}", j.name, j.agent)), width));
                out.extend(detail(j, width, detail_h, now));
            } else {
                out.push(rule(None, width));
                out.extend(std::iter::repeat_n(Vec::new(), detail_h));
            }
            out.push(rule(None, width));
        }
        if tail >= 1 {
            out.push(line::styled("j/k move · x kill · X kill all · g/G first/last · q close", |_| dim()));
        }
        out.truncate(max_rows);
        let out: Vec<Line> = out.iter().map(|l| line::truncate(l, width)).collect();
        let cursor = header + self.at.saturating_sub(self.top).min(list_h.saturating_sub(1));
        (out, (cursor, 0))
    }
}

fn mark(state: &SubagentState) -> (char, Role) {
    match state {
        SubagentState::Running => ('◌', Role::Accent),
        SubagentState::Done => ('✓', Role::Ok),
        SubagentState::Failed => ('✗', Role::Err),
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
    width: usize,
    now: SystemTime,
) -> Line {
    let t = theme::get();
    let (m, role) = mark(&j.state);
    let tools = match j.kind {
        JobKind::Subagent => format!("{:>3} tools", j.tools),
        JobKind::Bash => " ".repeat(9),
    };
    let rest = format!(" {} {tools}  {:>7}  {}", pad(&j.agent, agent_w), clock(elapsed(j, now)), j.last);
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
fn detail(j: &SubagentStatus, width: usize, h: usize, now: SystemTime) -> Vec<Line> {
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
