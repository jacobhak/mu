//! The jobs panel's Sessions tab: every live mu process with a session open, from the owner records in
//! `<profile home>/run`, as a tree (a subagent under the session that spawned it). See `design/jobs-panel.md`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::host::sessions::LiveRow;
use crate::text::line::{self, Line, dim, span};
use crate::text::{accent, pad, wrap};
use crate::theme::{self, Role};
use mu_base::events::SubagentStatus;
use mu_session::session::owner::LiveSession;

pub struct Row {
    pub live: LiveSession,
    /// The session's title, else its first prompt.
    pub title: String,
    /// Nesting under the session that spawned it.
    pub depth: usize,
    /// The job of this session that runs it, when it is one of its subagents.
    pub job: Option<String>,
    /// When the session file was last written.
    pub modified: Option<SystemTime>,
}

impl Row {
    /// `print`, `subagent review-lead`, `tui`; `?` for a record that says nothing.
    pub fn label(&self) -> String {
        let kind = self.live.kind.as_deref().unwrap_or("?");
        match &self.live.agent {
            Some(a) => format!("{kind} {a}"),
            None => kind.to_string(),
        }
    }

    /// `s` can stop it: a `-p` run stops after its step, a subagent is terminated, an interactive mu
    /// that takes abort requests cancels its current turn.
    pub fn can_stop(&self) -> bool {
        match self.live.kind.as_deref() {
            Some("print" | "subagent") => true,
            Some("tui" | "agent") => self.live.abort,
            _ => false,
        }
    }

    /// `S` can terminate it: an interactive mu or an agent (it ends its session the proper way), after a
    /// confirmation. An agent is joined with `mu attach`, not taken over, so `t` is not offered for it.
    pub fn can_terminate(&self) -> bool {
        matches!(self.live.kind.as_deref(), Some("tui" | "agent"))
    }

    /// `t` can take it over: only a `-p` run hands its session over.
    pub fn can_take(&self) -> bool {
        self.live.kind.as_deref() == Some("print") && self.live.print_run
    }

    /// The session id that `mu notify` and `mu take` accept.
    pub fn id(&self) -> String {
        mu_session::session::list::id_of(&self.live.path)
    }
}

/// The rows, in tree order: roots oldest first, each followed by its children. Left out: this
/// session (the Jobs tab is that), and nothing else. `titles` caches titles by path; `canon` has the
/// canonical form of the paths the host was asked about (a path not in it stands for itself).
pub fn build(
    lives: Vec<LiveRow>,
    titles: &HashMap<PathBuf, String>,
    jobs: &[SubagentStatus],
    own: Option<&Path>,
    canon: &HashMap<PathBuf, PathBuf>,
) -> Vec<Row> {
    let canon = |p: &Path| canon.get(p).cloned().unwrap_or_else(|| p.to_path_buf());
    let own = own.map(canon);
    let job_of: Vec<(PathBuf, &str)> =
        jobs.iter().filter_map(|j| j.session.as_deref().map(|p| (canon(p), j.name.as_str()))).collect();
    let lives: Vec<LiveRow> = lives.into_iter().filter(|l| own.as_deref() != Some(l.live.path.as_path())).collect();
    let paths: Vec<PathBuf> = lives.iter().map(|l| l.live.path.clone()).collect();
    let parent_of = |l: &LiveRow| l.parent.clone().filter(|p| paths.contains(p));
    let mut order: Vec<usize> = (0..lives.len()).collect();
    order.sort_by(|&a, &b| {
        (&lives[a].live.started, &lives[a].live.path).cmp(&(&lives[b].live.started, &lives[b].live.path))
    });
    let mut out: Vec<Row> = Vec::with_capacity(lives.len());
    let mut stack: Vec<(usize, usize)> =
        order.iter().rev().filter(|&&i| parent_of(&lives[i]).is_none()).map(|&i| (i, 0)).collect();
    while let Some((i, depth)) = stack.pop() {
        let l = &lives[i];
        for &c in order.iter().rev() {
            if parent_of(&lives[c]).as_deref() == Some(l.live.path.as_path()) {
                stack.push((c, depth + 1));
            }
        }
        let job = job_of.iter().find(|(p, _)| *p == l.live.path).map(|(_, n)| n.to_string());
        out.push(Row {
            live: l.live.clone(),
            title: titles.get(&l.live.path).cloned().unwrap_or_default(),
            depth,
            job,
            modified: l.modified,
        });
    }
    out
}

/// Seconds since an ISO timestamp (`2026-11-02T22:18:14.123Z`).
pub fn age(started: &str, now: SystemTime) -> Option<u64> {
    let ts: jiff::Timestamp = started.parse().ok()?;
    let then = SystemTime::UNIX_EPOCH + Duration::from_secs(u64::try_from(ts.as_second()).ok()?);
    now.duration_since(then).ok().map(|d| d.as_secs())
}

/// `45s`, `12m`, `3h`, `2d`.
pub fn age_text(secs: u64) -> String {
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m", secs / 60),
        3600..86400 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86400),
    }
}

/// `p` with the home directory as `~`, and its end kept when it has to be cut to `max` columns.
pub fn short_path(p: &Path, home: Option<&str>, max: usize) -> String {
    let s = p.to_string_lossy();
    let s = match home.filter(|h| !h.is_empty()).and_then(|h| s.strip_prefix(h)) {
        Some("") => "~".to_string(),
        Some(rest) if rest.starts_with('/') => format!("~{rest}"),
        _ => s.into_owned(),
    };
    let n = s.chars().count();
    if n <= max {
        return s;
    }
    let tail: String = s.chars().skip(n - max.saturating_sub(1)).collect();
    format!("…{tail}")
}

/// One list row: a live mark, kind and agent, pid, cost, age, directory and title. `home` is the
/// host's home directory.
pub fn list_row(
    r: &Row,
    selected: bool,
    label_w: usize,
    cost: &str,
    width: usize,
    now: SystemTime,
    home: Option<&str>,
) -> Line {
    let t = theme::get();
    let started = age(&r.live.started, now).map(age_text).unwrap_or_default();
    let cwd = r.live.cwd.as_deref().map(|c| short_path(c, home, 24)).unwrap_or_default();
    let indent = "  ".repeat(r.depth);
    let label = pad(&format!("{indent}{}", r.label()), label_w);
    let mut name = t.fg(Role::Strong);
    if selected {
        name.attributes.set(crossterm::style::Attribute::Bold);
    }
    let rest = format!(" {:>6} {cost:>6} {started:>4}  {cwd}  {}", r.live.pid, r.title);
    let mut l = vec![
        if selected { span("❯ ", accent()) } else { span("  ", dim()) },
        span("● ", t.fg(Role::Ok)),
        span(label, name),
        span(rest, t.fg(Role::Muted)),
    ];
    if selected {
        l = line::truncate(&l, width);
        l = line::fill(&l, width, t.color(Role::Selected));
    }
    l
}

/// What the panel knows about one session, as many rows as it takes.
pub fn detail(r: &Row, cost: &str, width: usize, now: SystemTime) -> Vec<Line> {
    let t = theme::get();
    let muted = t.fg(Role::Muted);
    let faint = t.fg(Role::Faint);
    let mut facts = format!("pid {}", r.live.pid);
    if let Some(a) = age(&r.live.started, now) {
        facts.push_str(&format!(" · up {}", age_text(a)));
    }
    if let Some(m) = r.modified.and_then(|m| now.duration_since(m).ok()) {
        facts.push_str(&format!(" · idle {}", age_text(m.as_secs())));
    }
    if !cost.is_empty() {
        facts.push_str(&format!(" · {cost}"));
    }
    let mut rows: Vec<Line> = vec![vec![span(" ● ", t.fg(Role::Ok)), span(facts, muted)]];
    if let Some(c) = &r.live.cwd {
        rows.push(vec![span(format!("   {}", c.display()), muted)]);
    }
    rows.push(vec![span(format!("   session {}", r.id()), faint)]);
    if let Some(j) = &r.job {
        rows.push(vec![span(format!("   job {j} of this session"), muted)]);
    }
    let body_w = width.saturating_sub(10).max(8);
    for (i, l) in wrap(&r.title, body_w).into_iter().enumerate() {
        let lead = if i == 0 { "   prompt " } else { "          " };
        rows.push(vec![span(lead, faint), span(l, t.fg(Role::Strong))]);
    }
    let mut keys = vec!["Enter peek", "n message"];
    if r.can_stop() {
        keys.push(match r.live.kind.as_deref() {
            Some("print") => "s stop after its step",
            Some("tui" | "agent") => "s abort its turn",
            _ => "s terminate",
        });
    }
    if r.can_terminate() {
        keys.push("S terminate");
    }
    if r.can_take() {
        keys.push("t take over");
    }
    rows.push(vec![span(format!("   {}", keys.join(" · ")), faint)]);
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::line::{line_text, line_width};

    fn live(path: &str, kind: &str, started: &str) -> LiveSession {
        LiveSession {
            path: PathBuf::from(path),
            pid: 4000 + path.len() as u32,
            print_run: kind == "print",
            inbox: kind == "tui",
            started: started.into(),
            kind: Some(kind.into()),
            agent: None,
            cwd: Some(PathBuf::from("/work/proj")),
            parent: None,
            abort: false,
        }
    }

    fn rows_of(lives: Vec<LiveSession>) -> Vec<LiveRow> {
        lives
            .into_iter()
            .map(|live| LiveRow { parent: live.parent.clone(), modified: None, title: None, live })
            .collect()
    }

    fn names(rows: &[Row]) -> Vec<(String, usize)> {
        rows.iter().map(|r| (r.live.path.display().to_string(), r.depth)).collect()
    }

    #[test]
    fn children_nest_under_their_parent_and_roots_go_oldest_first() {
        let mut child_a = live("/s/child-a.jsonl", "subagent", "2026-01-01T00:00:03Z");
        child_a.parent = Some(PathBuf::from("/s/root-old.jsonl"));
        let mut child_b = live("/s/child-b.jsonl", "subagent", "2026-01-01T00:00:02Z");
        child_b.parent = Some(PathBuf::from("/s/root-old.jsonl"));
        let mut grand = live("/s/grand.jsonl", "subagent", "2026-01-01T00:00:04Z");
        grand.parent = Some(PathBuf::from("/s/child-b.jsonl"));
        let mut orphan = live("/s/orphan.jsonl", "subagent", "2026-01-01T00:00:09Z");
        orphan.parent = Some(PathBuf::from("/s/gone.jsonl"));
        let rows = build(
            rows_of(vec![
                orphan,
                grand,
                live("/s/root-new.jsonl", "print", "2026-01-01T00:00:05Z"),
                child_a,
                live("/s/root-old.jsonl", "print", "2026-01-01T00:00:01Z"),
                child_b,
            ]),
            &HashMap::new(),
            &[],
            None,
            &HashMap::new(),
        );
        assert_eq!(
            names(&rows),
            [
                ("/s/root-old.jsonl".to_string(), 0),
                ("/s/child-b.jsonl".to_string(), 1),
                ("/s/grand.jsonl".to_string(), 2),
                ("/s/child-a.jsonl".to_string(), 1),
                ("/s/root-new.jsonl".to_string(), 0),
                ("/s/orphan.jsonl".to_string(), 0),
            ]
        );
    }

    #[test]
    fn this_process_and_its_own_session_are_left_out_and_own_subagents_are_marked() {
        let mut mine = live("/s/mine.jsonl", "tui", "2026-01-01T00:00:01Z");
        mine.pid = std::process::id();
        let mut sub = live("/s/sub.jsonl", "subagent", "2026-01-01T00:00:02Z");
        sub.parent = Some(PathBuf::from("/s/mine.jsonl"));
        let other = live("/s/other.jsonl", "tui", "2026-01-01T00:00:03Z");
        let job = SubagentStatus {
            name: "scout-1".into(),
            agent: "scout".into(),
            tools: 0,
            last: String::new(),
            state: mu_base::events::SubagentState::Running,
            kind: mu_base::events::JobKind::Subagent,
            cwd: PathBuf::new(),
            task: String::new(),
            started: SystemTime::UNIX_EPOCH,
            ended: None,
            delivered: false,
            note: None,
            session: Some(PathBuf::from("/s/sub.jsonl")),
            model: None,
            changes: None,
        };
        let rows = build(
            rows_of(vec![mine, sub, other]),
            &HashMap::new(),
            &[job],
            Some(Path::new("/s/mine.jsonl")),
            &HashMap::new(),
        );
        assert_eq!(names(&rows), [("/s/sub.jsonl".to_string(), 0), ("/s/other.jsonl".to_string(), 0)]);
        assert_eq!(rows[0].job.as_deref(), Some("scout-1"));
        assert_eq!(rows[1].job, None);
    }

    #[test]
    fn actions_depend_on_what_the_session_is() {
        let row = |kind: &str| Row {
            live: live("/s/a.jsonl", kind, "2026-01-01T00:00:00Z"),
            title: String::new(),
            depth: 0,
            job: None,
            modified: None,
        };
        assert!(row("print").can_stop() && row("print").can_take());
        assert!(row("subagent").can_stop() && !row("subagent").can_take());
        assert!(!row("tui").can_stop() && !row("tui").can_take(), "an older mu takes no abort");
        assert!(row("tui").can_terminate() && !row("print").can_terminate() && !row("subagent").can_terminate());
        let mut modern = row("tui");
        modern.live.abort = true;
        assert!(modern.can_stop() && !modern.can_take());
        assert!(!row("acp").can_stop() && !row("acp").can_terminate());
        // An agent is joined with `mu attach`, not taken over; it can be told to abort or to end.
        let mut agent = row("agent");
        assert!(agent.can_terminate() && !agent.can_take() && !agent.can_stop());
        agent.live.abort = true;
        assert!(agent.can_stop());
        assert_eq!(agent.label(), "agent");
        let mut r = row("subagent");
        r.live.agent = Some("review-lead".into());
        assert_eq!(r.label(), "subagent review-lead");
        assert_eq!(row("print").id(), "a");
    }

    #[test]
    fn ages_and_paths_read_short() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_767_225_600 + 125);
        assert_eq!(age("2026-01-01T00:00:00Z", now), Some(125));
        assert_eq!(age("not a time", now), None);
        assert_eq!([45, 125, 7200, 200_000].map(age_text), ["45s", "2m", "2h", "2d"]);
        assert_eq!(short_path(Path::new("/home/u/code/mu"), Some("/home/u"), 40), "~/code/mu");
        assert_eq!(short_path(Path::new("/home/u"), Some("/home/u"), 40), "~");
        assert_eq!(short_path(Path::new("/home/ux/a"), Some("/home/u"), 40), "/home/ux/a");
        assert_eq!(short_path(Path::new("/a/very/long/path/name"), None, 10), "…path/name");
    }

    #[test]
    fn rows_and_details_fit_and_show_the_essentials() {
        let mut l = live("/s/018f_abc.jsonl", "subagent", "2026-01-01T00:00:00Z");
        l.agent = Some("review-lead".into());
        let r = Row {
            live: l,
            title: "Review the diff against the plan and list blocking findings".into(),
            depth: 1,
            job: Some("scout-1".into()),
            modified: None,
        };
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_767_225_600 + 3700);
        let row = list_row(&r, true, 24, "$0.12", 80, now, None);
        let text = line_text(&row);
        assert!(text.contains("subagent review-lead") && text.contains("$0.12") && text.contains("1h"), "{text}");
        assert!(line_width(&row) <= 80);
        let d: Vec<String> = detail(&r, "$0.12", 60, now).iter().map(line_text).collect();
        assert!(d[0].contains("up 1h") && d[0].contains("$0.12"), "{d:?}");
        assert!(d.iter().any(|x| x.contains("/work/proj")), "{d:?}");
        assert!(d.iter().any(|x| x.contains("job scout-1 of this session")), "{d:?}");
        assert!(d.iter().any(|x| x.contains("prompt Review the diff")), "{d:?}");
        assert!(d.last().unwrap().contains("s terminate") && !d.last().unwrap().contains("take over"), "{d:?}");
    }
}
