//! Footer: one status line built from mu's own segments. omp's `statusLine` setting is not read.
use crate::text::line::{Line, line_width, span, truncate};
use crate::theme::{self, Role};
use crossterm::style::ContentStyle;
use mu_base::config::MuStatus;
use mu_base::events::{
    CACHE_TTL_MS, CacheBreak, CacheStatus, GitInfo, ProviderUsage, StatusSnapshot, SubagentState, SubagentStatus,
};
use mu_base::permissions::PermissionMode;
use std::path::{Path, PathBuf};
use std::time::Duration;

const DEFAULT_LEFT: &[&str] = &["pi", "model", "mode", "path", "git", "context_pct", "cost", "cache", "subagents"];
const DEFAULT_RIGHT: &[&str] = &["session_name"];
const ACTIVITY_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const SPINNER_ADVANCE_MS: u128 = 80;
const DEFAULT_PATH_MAX: usize = 40;
const MIN_PATH_MAX: usize = 8;
const MIN_TITLE_WIDTH: usize = 8;
const SEP_WIDTH: usize = 3;

/// Per-frame inputs that are not part of the agent snapshot.
pub struct StatusEnv<'a> {
    pub cwd: &'a Path,
    /// `$HOME`, used to abbreviate paths.
    pub home: Option<String>,
    /// Segment ids by side, in order; the built-in layout outside tests.
    pub left: &'a [&'a str],
    pub right: &'a [&'a str],
    /// Active profile (`OMP_PROFILE`), shown as `μ <profile>`.
    pub profile: Option<String>,
    /// Ctrl+O "expanded" state.
    pub expanded: bool,
    /// Elapsed time of the running turn; drives the spinner and timer in the `pi` segment.
    pub turn_elapsed: Option<Duration>,
    /// Background jobs by state, for the `subagents` segment.
    pub jobs: JobCounts,
    /// What this session's subagents (descendants included) have spent, for the `cost` segment.
    pub subagent_cost: f64,
    /// When the context share turns warning and error colour.
    pub levels: MuStatus,
}

/// How many background jobs are running, done and failed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct JobCounts {
    pub running: usize,
    pub done: usize,
    pub failed: usize,
}

impl JobCounts {
    pub fn of(jobs: &[SubagentStatus]) -> Self {
        let count = |s: SubagentState| jobs.iter().filter(|j| j.state == s).count();
        Self {
            running: count(SubagentState::Running),
            done: count(SubagentState::Done),
            failed: count(SubagentState::Failed),
        }
    }
}

impl<'a> StatusEnv<'a> {
    pub fn new(cwd: &'a Path) -> Self {
        Self {
            cwd,
            home: crate::host::host_home(),
            left: DEFAULT_LEFT,
            right: DEFAULT_RIGHT,
            profile: crate::host::host_profile(),
            expanded: false,
            turn_elapsed: None,
            jobs: JobCounts::default(),
            subagent_cost: 0.0,
            levels: MuStatus::default(),
        }
    }
}

/// The status line; at most `width` columns. Account quota windows sit at the right edge.
pub fn render(s: &StatusSnapshot, env: &StatusEnv, width: usize) -> Vec<Line> {
    vec![status_line(s, env, width)]
}

/// `5h 34% 3h43m  7d 25% 3d20h`: window label, used share coloured by level, time to reset dimmed.
fn usage_line(u: &ProviderUsage) -> Line {
    let t = theme::get();
    let plain = ContentStyle::default();
    let mut out: Line = Vec::new();
    for (i, w) in u.windows.iter().enumerate() {
        if i > 0 {
            out.push(span("  ", plain));
        }
        let pct = w.used_percent.round() as i64;
        let level = if pct >= 80 {
            Role::Err
        } else if pct >= 50 {
            Role::Warn
        } else {
            Role::Muted
        };
        out.push(span(format!("{} ", w.label), t.fg(Role::Faint)));
        out.push(span(format!("{pct}%"), t.fg(level)));
        out.push(span(format!(" {}", reset_text(&w.label, w.resets_in).replace(' ', "")), t.fg(Role::Faint)));
    }
    out
}

/// omp's reset text: minutes for the 5h window, hours and days for the rest.
fn reset_text(label: &str, resets_in: Duration) -> String {
    let secs = resets_in.as_secs_f64();
    if label == "5h" {
        let m = (secs / 60.0).round() as u64;
        return if m < 60 {
            format!("{m}m")
        } else if m.is_multiple_of(60) {
            format!("{}h", m / 60)
        } else {
            format!("{}h {}m", m / 60, m % 60)
        };
    }
    let h = (secs / 3600.0).round() as u64;
    if h < 24 {
        format!("{h}h")
    } else if h.is_multiple_of(24) {
        format!("{}d", h / 24)
    } else {
        format!("{}d {}h", h / 24, h % 24)
    }
}

struct Part {
    id: &'static str,
    line: Line,
}

fn status_line(s: &StatusSnapshot, env: &StatusEnv, width: usize) -> Line {
    let mut path_max = DEFAULT_PATH_MAX;
    let mut left = segments(env.left, s, env, path_max);
    if env.expanded {
        left.push(Part { id: "expanded", line: vec![span("expanded", theme::get().fg(Role::Faint))] });
    }
    let mut right = segments(env.right, s, env, path_max);
    if let Some(u) = s.usage.as_ref().filter(|u| !u.windows.is_empty()) {
        right.insert(0, Part { id: "usage", line: usage_line(u) });
    }

    // Overflow, in omp's order: shorten the title, drop right segments, shorten the path, drop left segments.
    let total = |left: &[Part], right: &[Part]| group_width(left) + group_width(right) + gap(left, right);
    if total(&left, &right) > width
        && let Some(i) = right.iter().position(|p| p.id == "session_name")
    {
        let overflow = total(&left, &right) - width;
        let cur = line_width(&right[i].line);
        let target = cur.saturating_sub(overflow).max(MIN_TITLE_WIDTH.min(cur));
        right[i].line = truncate(&right[i].line, target);
    }
    while total(&left, &right) > width && !right.is_empty() {
        right.pop();
    }
    while total(&left, &right) > width && path_max > MIN_PATH_MAX && left.iter().any(|p| p.id == "path") {
        path_max -= 1;
        let line = seg_path(s, env, path_max).unwrap_or_default();
        if let Some(p) = left.iter_mut().find(|p| p.id == "path") {
            p.line = line;
        }
    }
    while total(&left, &right) > width {
        let Some(i) = left.iter().rposition(|p| p.id != "path") else { break };
        left.remove(i);
    }

    let left_group = group(&left);
    if right.is_empty() {
        return truncate(&left_group, width);
    }
    let right_group = group(&right);
    let pad = width.saturating_sub(line_width(&left_group) + line_width(&right_group)).max(1);
    let mut line = left_group;
    line.push(span(" ".repeat(pad), ContentStyle::default()));
    line.extend(right_group);
    truncate(&line, width)
}

/// Visible width of a group: its parts, the separators between them, and the outer spaces.
fn group_width(parts: &[Part]) -> usize {
    if parts.is_empty() {
        return 0;
    }
    parts.iter().map(|p| line_width(&p.line)).sum::<usize>() + (parts.len() - 1) * SEP_WIDTH + 2
}

fn gap(left: &[Part], right: &[Part]) -> usize {
    usize::from(!left.is_empty() && !right.is_empty())
}

fn group(parts: &[Part]) -> Line {
    if parts.is_empty() {
        return vec![];
    }
    let sep = theme::get().fg(Role::Line);
    let plain = ContentStyle::default();
    let mut out: Line = vec![span(" ", plain)];
    for (i, p) in parts.iter().enumerate() {
        if i > 0 {
            out.push(span(" · ", sep));
        }
        out.extend(p.line.iter().cloned());
    }
    out.push(span(" ", plain));
    out
}

type Render = fn(&StatusSnapshot, &StatusEnv, usize) -> Option<Line>;

/// A status segment: its id and how to draw it. The only list of segments.
struct Segment {
    id: &'static str,
    render: Render,
}

static SEGMENTS: &[Segment] = &[
    Segment { id: "pi", render: seg_pi },
    Segment { id: "model", render: seg_model },
    Segment { id: "mode", render: seg_mode },
    Segment { id: "path", render: seg_path },
    Segment { id: "git", render: seg_git },
    Segment { id: "context_pct", render: seg_context_pct },
    Segment { id: "context_total", render: seg_context_total },
    Segment { id: "cost", render: seg_cost },
    Segment { id: "cache", render: seg_cache },
    Segment { id: "subagents", render: seg_subagents },
    Segment { id: "session_name", render: seg_session_name },
];

/// Renders the ids in order, skipping unknown ids and segments with nothing to show.
fn segments(ids: &[&str], s: &StatusSnapshot, env: &StatusEnv, path_max: usize) -> Vec<Part> {
    ids.iter()
        .filter_map(|name| {
            let seg = SEGMENTS.iter().find(|g| g.id == *name)?;
            Some(Part { id: seg.id, line: (seg.render)(s, env, path_max)? })
        })
        .collect()
}

fn st(role: Role) -> ContentStyle {
    theme::get().fg(role)
}

fn seg_pi(_: &StatusSnapshot, env: &StatusEnv, _: usize) -> Option<Line> {
    let glyph = match env.turn_elapsed {
        Some(d) => ACTIVITY_FRAMES[(d.as_millis() / SPINNER_ADVANCE_MS) as usize % ACTIVITY_FRAMES.len()],
        None => "μ",
    };
    let mut rest = env.profile.as_ref().map(|p| format!(" {p}")).unwrap_or_default();
    if let Some(d) = env.turn_elapsed {
        rest.push(' ');
        rest.push_str(&timer(d));
    }
    let glyph_role = if env.turn_elapsed.is_some() { Role::Accent } else { Role::Muted };
    let mut line = vec![span(glyph, st(glyph_role))];
    if !rest.is_empty() {
        line.push(span(rest, st(Role::Muted)));
    }
    Some(line)
}

fn seg_model(s: &StatusSnapshot, _: &StatusEnv, _: usize) -> Option<Line> {
    if s.model_id.is_empty() {
        return Some(vec![span("no model", st(Role::Faint)), span(" · /switch", st(Role::Muted))]);
    }
    let mut line = vec![span(s.model_name.clone(), st(Role::Tool))];
    if let Some(l) = thinking_label(s) {
        line.push(span(format!(" ({l})"), st(Role::Muted)));
    }
    if s.chatgpt_plan {
        line.push(span(" · ChatGPT plan", st(Role::Muted)));
    } else if s.native_keyless {
        line.push(span(" · no auth", st(Role::Muted)));
    } else if s.native {
        line.push(span(" · key", st(Role::Muted)));
    }
    Some(line)
}

fn seg_mode(s: &StatusSnapshot, _: &StatusEnv, _: usize) -> Option<Line> {
    let mut line = Vec::new();
    if let Some(mode) = &s.mode {
        line.push(span(mode.clone(), st(Role::Accent)));
    }
    if s.permissions != PermissionMode::Yolo {
        if !line.is_empty() {
            line.push(span(" ", st(Role::Accent)));
        }
        line.push(span(s.permissions.as_str(), st(Role::Accent)));
    }
    (!line.is_empty()).then_some(line)
}

fn seg_path(_: &StatusSnapshot, env: &StatusEnv, path_max: usize) -> Option<Line> {
    Some(path_line(env, path_max))
}

fn seg_git(s: &StatusSnapshot, _: &StatusEnv, _: usize) -> Option<Line> {
    git_line(s.git.as_ref()?)
}

fn seg_context_pct(s: &StatusSnapshot, env: &StatusEnv, _: usize) -> Option<Line> {
    Some(context_line(s, &env.levels))
}

fn seg_context_total(s: &StatusSnapshot, _: &StatusEnv, _: usize) -> Option<Line> {
    (s.context_window != 0)
        .then(|| vec![span(format!("{} ctx", format_number(s.context_window as u64)), st(Role::Muted))])
}

fn seg_cost(s: &StatusSnapshot, env: &StatusEnv, _: usize) -> Option<Line> {
    let sub = env.subagent_cost;
    if !s.subscription && s.cost <= 0.0 && sub <= 0.0 {
        return None;
    }
    // omp: a zero subscription spend reads `(sub)`; otherwise the unit (`S` subscription, `$` metered)
    // and amount, then the subagent tree's spend as `(+0.12)`; the unit is the session's own.
    let mut text = match (s.cost > 0.0 || sub > 0.0, s.subscription) {
        (false, _) => "(sub)".to_string(),
        (true, true) => format!("S{:.2}", s.cost),
        (true, false) => format!("${:.2}", s.cost),
    };
    if sub > 0.0 {
        text.push_str(&format!(" (+{sub:.2})"));
    }
    Some(vec![span(text, st(Role::SynType))])
}

/// Why the next message will miss the prompt cache, when it will.
fn cache_cold(c: &CacheStatus, now_ms: i64) -> Option<String> {
    let reason = match c.changed {
        Some(CacheBreak::Model) => "model changed",
        Some(CacheBreak::Thinking) => "thinking level changed",
        Some(CacheBreak::Tools) => "tool set changed",
        Some(CacheBreak::SystemPrompt) => "system prompt changed",
        Some(CacheBreak::Compacted) => "context was compacted",
        None => {
            let idle = now_ms - c.at_ms;
            return (idle > CACHE_TTL_MS).then(|| format!("expired after {} min idle", idle / 60_000));
        }
    };
    Some(reason.into())
}

/// How long until a warm cache expires, so the footer can redraw as it does; `None` when the next
/// request already misses (expired or something changed) or the cache is not in use.
pub(crate) fn cache_expires_in(c: &CacheStatus, now_ms: i64) -> Option<Duration> {
    if c.changed.is_some() {
        return None;
    }
    let left = c.at_ms + CACHE_TTL_MS - now_ms;
    // `cache_cold` flips once the idle time is *over* the TTL: land just past it.
    (left >= 0).then(|| Duration::from_millis(left as u64 + 50))
}

/// Share of the last reply's input the provider served from cache, rounded.
fn hit_percent(c: &CacheStatus) -> u64 {
    let total = c.cache_read + c.cache_write + c.input;
    (c.cache_read * 100 + total / 2).checked_div(total).unwrap_or(0)
}

fn seg_cache(s: &StatusSnapshot, _: &StatusEnv, _: usize) -> Option<Line> {
    let c = s.cache.as_ref()?;
    let line = match cache_cold(c, mu_base::message::now_ms()) {
        Some(_) => vec![span("cache cold", st(Role::Warn))],
        None => vec![span(format!("cache {}%", hit_percent(c)), st(Role::Muted))],
    };
    Some(line)
}

/// `/session`'s two lines about the prompt cache: the last reply's hit, and the next request's fate.
pub(crate) fn cache_summary(c: &CacheStatus, now_ms: i64) -> String {
    let total = c.cache_read + c.cache_write + c.input;
    let first = format!(
        "Prompt cache: last reply read {}% from cache ({} of {} input tokens)",
        hit_percent(c),
        format_number(c.cache_read),
        format_number(total)
    );
    let next = match cache_cold(c, now_ms) {
        Some(reason) => format!("Next message: not cached — {reason}"),
        None => "Next message: likely cached".to_string(),
    };
    format!("{first}\n{next}")
}

/// `◌2 ✓1 ✗1`: running, done and failed background jobs; a count of zero is left out and no jobs at
/// all leaves no segment.
fn seg_subagents(_: &StatusSnapshot, env: &StatusEnv, _: usize) -> Option<Line> {
    let c = env.jobs;
    let mut line: Line = Vec::new();
    for (mark, n, role) in [('◌', c.running, Role::Accent), ('✓', c.done, Role::Ok), ('✗', c.failed, Role::Err)] {
        if n == 0 {
            continue;
        }
        if !line.is_empty() {
            line.push(span(" ", ContentStyle::default()));
        }
        line.push(span(format!("{mark}{n}"), st(role)));
    }
    (!line.is_empty()).then_some(line)
}

fn seg_session_name(s: &StatusSnapshot, _: &StatusEnv, _: usize) -> Option<Line> {
    let title = s.title.as_deref().filter(|t| !t.is_empty())?;
    Some(vec![span(title, st(Role::Tool))])
}

/// Abbreviated level after the model name; parenthesised so `Haiku 5.5 h` doesn't read as a version suffix.
fn thinking_label(s: &StatusSnapshot) -> Option<&'static str> {
    if !s.reasoning {
        return None;
    }
    Some(match s.thinking.as_str() {
        "low" => "l",
        "medium" => "m",
        "high" => "h",
        "xhigh" => "xh",
        "max" => "max",
        _ => "off",
    })
}

fn timer(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else {
        format!("{}h", (secs / 3600).min(99))
    }
}

/// `1.5% of 1M`: context share over the window, coloured by the `[status]` thresholds.
fn context_line(s: &StatusSnapshot, levels: &MuStatus) -> Line {
    let window = s.context_window as u64;
    let pct = s.context_tokens.filter(|_| window > 0).map(|t| t as f64 / window as f64 * 100.0);
    let t = theme::get();
    let level = context_color(pct, window, levels);
    match pct {
        Some(p) => vec![
            span(format!("{p:.1}%"), t.fg(if level == Role::Muted { Role::Accent } else { level })),
            span(format!(" of {}", format_number(window)), t.fg(Role::Faint)),
        ],
        None => vec![span(format_number(window), t.fg(Role::Muted))],
    }
}

/// Modelled on omp's `getContextUsageLevel`: a level is reached at its percent or its token count,
/// whichever is lower.
fn context_color(pct: Option<f64>, window: u64, levels: &MuStatus) -> Role {
    let Some(p) = pct.filter(|p| *p > 0.0) else { return Role::Muted };
    let reaches = |percent: f64, tokens: u64| {
        let token_pct = if window > 0 { tokens as f64 / window as f64 * 100.0 } else { f64::INFINITY };
        p >= percent.min(token_pct)
    };
    if reaches(levels.err_percent, levels.err_tokens) {
        Role::Err
    } else if reaches(levels.warn_percent, levels.warn_tokens) {
        Role::Warn
    } else {
        Role::Muted
    }
}

/// omp's `formatNumber`: `999`, `1.5K`, `25K`, `1M`, `1.5B`.
pub(crate) fn format_number(n: u64) -> String {
    let trim1 = |v: f64, unit: &str| {
        let s = format!("{v:.1}");
        format!("{}{unit}", s.strip_suffix(".0").unwrap_or(&s))
    };
    // The unit is chosen after rounding: 999_600 is "1M", not "1000K".
    match n {
        0..1_000 => n.to_string(),
        1_000..10_000 => trim1(n as f64 / 1e3, "K"),
        10_000..1_000_000 => match (n as f64 / 1e3).round() as u64 {
            1000 => "1M".into(),
            k => format!("{k}K"),
        },
        1_000_000..10_000_000 => trim1(n as f64 / 1e6, "M"),
        10_000_000..1_000_000_000 => match (n as f64 / 1e6).round() as u64 {
            1000 => "1B".into(),
            m => format!("{m}M"),
        },
        1_000_000_000..10_000_000_000 => trim1(n as f64 / 1e9, "B"),
        _ => format!("{}B", (n as f64 / 1e9).round() as u64),
    }
}

/// `branch *unstaged +staged ?untracked`, coloured clean/dirty as omp does. `None` when nothing shows.
fn git_line(g: &GitInfo) -> Option<Line> {
    let t = theme::get();
    let counts: Vec<(String, Role)> =
        [(g.dirty, "*", Role::Warn), (g.staged, "+", Role::Ok), (g.untracked, "?", Role::Accent)]
            .into_iter()
            .filter(|(n, _, _)| *n > 0)
            .map(|(n, prefix, role)| (format!("{prefix}{n}"), role))
            .collect();
    let mut line: Line = vec![];
    if let Some(b) = &g.branch {
        let role = if g.dirty + g.staged + g.untracked == 0 { Role::Ok } else { Role::Warn };
        line.push(span(b.clone(), t.fg(role)));
    }
    for (text, role) in counts {
        if !line.is_empty() {
            line.push(span(" ", ContentStyle::default()));
        }
        line.push(span(text, t.fg(role)));
    }
    (!line.is_empty()).then_some(line)
}

/// omp's path segment: `~` for home; project dirs under `~/Projects`, `~/repos` and `/work` shown relative
/// to that root; left-truncated to `max`.
fn path_line(env: &StatusEnv, max: usize) -> Line {
    let home = env.home.as_deref().map(|h| h.trim_end_matches('/')).filter(|h| !h.is_empty());
    let display = path_display(env.cwd, home, true);
    let text = clamp_left(&abbreviate_home(&display, home), max);
    vec![span(text, theme::get().fg(Role::Literal))]
}

/// The path relative to its scratch or project root when stripping, else the full path.
fn path_display(cwd: &Path, home: Option<&str>, strip: bool) -> String {
    let full = cwd.display().to_string();
    if !strip {
        return full;
    }
    let mut scratch_roots: Vec<PathBuf> = vec![crate::host::host_tmp(), "/tmp".into(), "/var/tmp".into()];
    if let Some(h) = home {
        scratch_roots.push(Path::new(h).join("tmp"));
    }
    for root in scratch_roots {
        if let Ok(rel) = cwd.strip_prefix(&root) {
            let rel = rel.display().to_string();
            return if rel.is_empty() { full } else { rel };
        }
    }
    if let Some(h) = home {
        let h = Path::new(h);
        let roots = [h.join("Projects"), h.join("repos"), PathBuf::from("/work")];
        for root in roots {
            if let Ok(rel) = cwd.strip_prefix(&root) {
                let rel = rel.display().to_string();
                if !rel.is_empty() {
                    return rel;
                }
            }
        }
    }
    full
}

fn abbreviate_home(p: &str, home: Option<&str>) -> String {
    match home.and_then(|h| p.strip_prefix(h)) {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => format!("~{rest}"),
        _ => p.to_string(),
    }
}

/// Keeps the last `max - 1` characters behind a leading `…`.
fn clamp_left(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    let tail: String = s.chars().skip(n - max.saturating_sub(1)).collect();
    format!("…{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::line::line_text;
    use mu_base::events::QuotaWindow;
    #[test]
    fn the_model_segment_marks_the_plan_apart_from_a_key() {
        let mut s = StatusSnapshot {
            model_id: "openai-chatgpt/gpt-6.1-sol".into(),
            model_name: "openai-chatgpt/gpt-6.1-sol".into(),
            native: true,
            chatgpt_plan: true,
            ..Default::default()
        };
        let text = |s: &StatusSnapshot| line_text(&seg_model(s, &env(&["model"], &[], Path::new("/")), 0).unwrap());
        assert!(text(&s).ends_with(" · ChatGPT plan"), "{}", text(&s));
        s.chatgpt_plan = false;
        assert!(text(&s).ends_with(" · key"), "{}", text(&s));
        s.native_keyless = true;
        assert!(text(&s).ends_with(" · no auth"), "{}", text(&s));
    }

    #[test]
    fn no_model_chosen_is_shown_as_such() {
        let t = line_text(&seg_model(&StatusSnapshot::default(), &env(&["model"], &[], Path::new("/")), 0).unwrap());
        assert_eq!(t, "no model · /switch");
    }

    #[test]
    fn the_mode_segment_shows_plan_and_a_non_yolo_mode() {
        let e = env(&["mode"], &[], Path::new("/"));
        assert!(seg_mode(&StatusSnapshot::default(), &e, 0).is_none(), "yolo and no plan is silent");
        let s = StatusSnapshot { permissions: PermissionMode::Ask, ..Default::default() };
        assert_eq!(line_text(&seg_mode(&s, &e, 0).unwrap()), "ask");
        let s = StatusSnapshot { mode: Some("plan".into()), permissions: PermissionMode::Edits, ..Default::default() };
        assert_eq!(line_text(&seg_mode(&s, &e, 0).unwrap()), "plan edits");
    }

    fn snap() -> StatusSnapshot {
        StatusSnapshot {
            model_id: "anthropic/claude-haiku-5-5".into(),
            model_name: "Haiku 5.5".into(),
            reasoning: true,
            thinking: "high".into(),
            subscription: true,
            context_tokens: Some(15_000),
            context_window: 1_000_000,
            cost: 0.0,
            title: Some("EVENTS: Read mu events.rs header".into()),
            git: Some(GitInfo { branch: Some("main".into()), staged: 1, dirty: 2, untracked: 3, ..Default::default() }),
            usage: Some(ProviderUsage {
                provider: "anthropic".into(),
                windows: vec![
                    QuotaWindow { label: "5h".into(), used_percent: 82.4, resets_in: Duration::from_secs(2 * 3600) },
                    QuotaWindow { label: "7d".into(), used_percent: 15.6, resets_in: Duration::from_secs(4 * 86400) },
                ],
            }),
            ..Default::default()
        }
    }

    fn env<'a>(left: &'a [&'a str], right: &'a [&'a str], cwd: &'a Path) -> StatusEnv<'a> {
        StatusEnv {
            cwd,
            home: Some("/home/u".into()),
            left,
            right,
            profile: Some("private".into()),
            expanded: false,
            turn_elapsed: None,
            jobs: JobCounts::default(),
            subagent_cost: 0.0,
            levels: MuStatus::default(),
        }
    }

    const LEFT: &[&str] = &["pi", "model", "path", "git", "context_pct", "cost"];

    #[test]
    fn renders_segments_in_order_with_right_title() {
        let lines = render(&snap(), &env(LEFT, &["session_name"], Path::new("/home/u/code/mu")), 140);
        assert_eq!(lines.len(), 1);
        let t = line_text(&lines[0]);
        assert_eq!(line_width(&lines[0]), 140);
        assert!(t.contains(" 5h 82% 2h  7d 16% 4d · EVENTS"), "{t}");
        assert!(t.starts_with(" μ private · Haiku 5.5 (h) · ~/code/mu · main *2 +1 ?3 · 1.5% of 1M · (sub) "), "{t}");
        assert!(t.ends_with(" EVENTS: Read mu events.rs header "), "{t}");
    }

    #[test]
    fn model_shows_abbreviated_thinking_level() {
        let c: &[&str] = &["model"];
        let cwd = Path::new("/home/u/x");
        let mut s = StatusSnapshot { usage: None, title: None, ..snap() };
        for (level, label) in
            [("off", "off"), ("low", "l"), ("medium", "m"), ("high", "h"), ("xhigh", "xh"), ("max", "max")]
        {
            s.thinking = level.into();
            assert_eq!(line_text(&render(&s, &env(c, &[], cwd), 80)[0]).trim(), format!("Haiku 5.5 ({label})"));
        }
        s.reasoning = false;
        assert_eq!(line_text(&render(&s, &env(c, &[], cwd), 80)[0]).trim(), "Haiku 5.5");
    }

    #[test]
    fn skips_unknown_and_empty_segments() {
        let c: &[&str] = &["pi", "pr", "subagents", "bogus", "mode", "cost"];
        let mut s = snap();
        s.title = None;
        s.mode = None;
        s.subscription = false;
        s.cost = 0.0;
        // Only `pi` has something to show, and an empty right side means no padding.
        s.usage = None;
        assert_eq!(line_text(&render(&s, &env(c, &[], Path::new("/home/u/x")), 80)[0]), " μ private ");
        s.mode = Some("plan".into());
        s.cost = 0.25;
        assert_eq!(
            line_text(&render(&s, &env(&["mode", "cost"], &[], Path::new("/home/u/x")), 80)[0]),
            " plan · $0.25 "
        );
    }

    #[test]
    fn cost_shows_the_subagent_tree_after_the_sessions_own_spend() {
        let cwd = Path::new("/home/u/x");
        let text = |s: &StatusSnapshot, sub: f64| {
            let mut e = env(&["cost"], &[], cwd);
            e.subagent_cost = sub;
            line_text(&render(s, &e, 80)[0]).trim().to_string()
        };
        let mut s = StatusSnapshot { subscription: false, cost: 0.38, ..snap() };
        s.usage = None;
        s.title = None;
        assert_eq!(text(&s, 0.12), "$0.38 (+0.12)");
        assert_eq!(text(&s, 0.0), "$0.38");
        s.cost = 0.0;
        assert_eq!(text(&s, 0.0), "", "nothing spent, nothing shown");
        assert_eq!(text(&s, 0.2), "$0.00 (+0.20)", "subagents alone still show");
        // Subscription spend keeps its mark on the session's own amount only.
        s.subscription = true;
        assert_eq!(text(&s, 0.0), "(sub)");
        assert_eq!(text(&s, 0.2), "S0.00 (+0.20)");
        s.cost = 0.38;
        assert_eq!(text(&s, 0.2), "S0.38 (+0.20)");
    }

    #[test]
    fn subagents_segment_counts_jobs_and_hides_zeros() {
        let cwd = Path::new("/home/u/x");
        let mut s = snap();
        s.usage = None;
        s.title = None;
        let mut e = env(&["subagents"], &[], cwd);
        assert_eq!(line_text(&render(&s, &e, 80)[0]), "", "no jobs, no segment");
        e.jobs = JobCounts { running: 2, done: 1, failed: 1 };
        assert_eq!(line_text(&render(&s, &e, 80)[0]).trim(), "◌2 ✓1 ✗1");
        e.jobs = JobCounts { running: 0, done: 3, failed: 0 };
        assert_eq!(line_text(&render(&s, &e, 80)[0]).trim(), "✓3");
        let lines = render(&s, &e, 80);
        assert!(line_width(&lines[0]) <= 80);
    }

    #[test]
    fn job_counts_group_by_state() {
        let mk = |state| SubagentStatus {
            name: "a".into(),
            agent: "scout".into(),
            tools: 0,
            last: String::new(),
            state,
            kind: mu_base::events::JobKind::Subagent,
            cwd: PathBuf::new(),
            task: String::new(),
            started: std::time::SystemTime::UNIX_EPOCH,
            ended: None,
            delivered: false,
            note: None,
            session: None,
            model: None,
            changes: None,
            progress: None,
            ports: Vec::new(),
        };
        let jobs =
            [mk(SubagentState::Running), mk(SubagentState::Done), mk(SubagentState::Done), mk(SubagentState::Failed)];
        assert_eq!(JobCounts::of(&jobs), JobCounts { running: 1, done: 2, failed: 1 });
        assert_eq!(JobCounts::of(&[]), JobCounts::default());
    }

    #[test]
    fn never_exceeds_width_and_drops_in_omp_order() {
        let cwd = Path::new("/home/u/code/mu");
        for w in [5usize, 20, 40, 60, 80, 100] {
            for l in &render(&snap(), &env(LEFT, &["session_name"], cwd), w) {
                assert!(line_width(l) <= w, "width {w}: {:?}", line_text(l));
            }
        }
        // Narrow enough to lose the title and the trailing segments, but not the brand.
        let t = line_text(&render(&snap(), &env(LEFT, &["session_name"], cwd), 50)[0]);
        assert!(t.starts_with(" μ private"), "{t}");
        assert!(!t.contains("EVENTS"), "{t}");
    }

    #[test]
    fn path_rules() {
        let home = Some("/home/u");
        assert_eq!(path_display(Path::new("/tmp"), home, true), "/tmp");
        assert_eq!(path_display(Path::new("/tmp/a/b"), home, true), "a/b");
        assert_eq!(path_display(Path::new("/home/u/Projects/x"), home, true), "x");
        assert_eq!(path_display(Path::new("/home/u/src/x"), home, true), "/home/u/src/x");
        assert_eq!(path_display(Path::new("/home/u/src/x"), home, false), "/home/u/src/x");
        assert_eq!(abbreviate_home("/home/u/src/x", home), "~/src/x");
        assert_eq!(abbreviate_home("/home/user2/x", home), "/home/user2/x");
        assert_eq!(clamp_left("abcdefghij", 6), "…fghij");
    }

    #[test]
    fn context_levels_and_numbers() {
        assert_eq!(format_number(1_000_000), "1M");
        assert_eq!(format_number(999_600), "1M");
        assert_eq!(format_number(999_400), "999K");
        assert_eq!(format_number(999_600_000), "1B");
        assert_eq!(format_number(999_400_000), "999M");
        assert_eq!(format_number(200_000), "200K");
        assert_eq!(format_number(1_500), "1.5K");
        assert_eq!(format_number(1_500_000_000), "1.5B");
        let d = MuStatus::default();
        assert_eq!(context_color(Some(1.5), 1_000_000, &d), Role::Muted);
        assert_eq!(context_color(Some(20.0), 1_000_000, &d), Role::Muted);
        assert_eq!(context_color(Some(30.0), 1_000_000, &d), Role::Warn);
        assert_eq!(context_color(Some(55.0), 1_000_000, &d), Role::Err);
        assert_eq!(context_color(None, 1_000_000, &d), Role::Muted);
        // On a small window the percent is reached before the token count.
        assert_eq!(context_color(Some(55.0), 200_000, &d), Role::Warn);
        assert_eq!(context_color(Some(75.0), 200_000, &d), Role::Err);
        let custom = MuStatus { warn_tokens: 100_000, err_tokens: 200_000, ..d };
        assert_eq!(context_color(Some(12.0), 1_000_000, &custom), Role::Warn);
        assert_eq!(context_color(Some(25.0), 1_000_000, &custom), Role::Err);
        assert_eq!(reset_text("5h", Duration::from_secs(90 * 60)), "1h 30m");
        assert_eq!(reset_text("7d", Duration::from_secs(30 * 3600)), "1d 6h");
        assert_eq!(reset_text("7d", Duration::from_secs(4 * 86400)), "4d");
    }

    #[test]
    fn git_counts_and_clean_repo() {
        let c: &[&str] = &["git"];
        let cwd = Path::new("/home/u/x");
        let mut s = snap();
        s.title = None;
        s.usage = None;
        s.git = Some(GitInfo { branch: Some("dev".into()), ..Default::default() });
        assert_eq!(line_text(&render(&s, &env(c, &[], cwd), 40)[0]), " dev ");
        s.git = Some(GitInfo { staged: 2, ..Default::default() });
        assert_eq!(line_text(&render(&s, &env(c, &[], cwd), 40)[0]), " +2 ");
        s.git = None;
        assert_eq!(render(&s, &env(c, &[], cwd), 40).len(), 1);
    }

    #[test]
    fn git_colours() {
        let c: &[&str] = &["git"];
        let cwd = Path::new("/home/u/x");
        let mut s = snap();
        s.title = None;
        s.usage = None;
        let fg = |l: &Line, txt: &str| l.iter().find(|p| p.text == txt).unwrap().style.foreground_color;
        s.git = Some(GitInfo { branch: Some("dev".into()), ..Default::default() });
        let l = &render(&s, &env(c, &[], cwd), 40)[0];
        assert_eq!(fg(l, "dev"), theme::get().color(Role::Ok));
        s.git = Some(GitInfo { branch: Some("dev".into()), dirty: 2, ..Default::default() });
        let l = &render(&s, &env(c, &[], cwd), 40)[0];
        assert_eq!(fg(l, "dev"), theme::get().color(Role::Warn));
        assert_eq!(fg(l, "*2"), theme::get().color(Role::Warn));
    }

    fn cache(input: u64, read: u64, at_ms: i64, changed: Option<CacheBreak>) -> CacheStatus {
        CacheStatus { input, cache_read: read, cache_write: 0, at_ms, changed }
    }

    #[test]
    fn cache_segment_shows_the_last_hit_and_turns_cold_when_it_will_miss() {
        let cwd = Path::new("/home/u/x");
        let now = mu_base::message::now_ms();
        let mut s = snap();
        s.usage = None;
        s.title = None;
        let text = |s: &StatusSnapshot| line_text(&render(s, &env(&["cache"], &[], cwd), 80)[0]).trim().to_string();
        s.cache = Some(cache(100, 900, now, None));
        assert_eq!(text(&s), "cache 90%");
        s.cache = Some(cache(100, 900, now, Some(CacheBreak::Model)));
        assert_eq!(text(&s), "cache cold");
        s.cache = Some(cache(100, 900, now - 6 * 60_000, None));
        assert_eq!(text(&s), "cache cold");
        s.cache = None;
        assert_eq!(text(&s), "", "no cache activity, no segment");
    }

    #[test]
    fn cache_expiry_timer_lands_after_the_ttl_and_only_for_a_warm_cache() {
        let now = mu_base::message::now_ms();
        let wait = cache_expires_in(&cache(1, 9, now - 60_000, None), now).unwrap();
        assert!(wait > Duration::from_secs(239) && wait < Duration::from_secs(241), "{wait:?}");
        assert!(cache_cold(&cache(1, 9, now - CACHE_TTL_MS, None), now + wait.as_millis() as i64).is_some());
        assert_eq!(cache_expires_in(&cache(1, 9, now - 6 * 60_000, None), now), None);
        assert_eq!(cache_expires_in(&cache(1, 9, now, Some(CacheBreak::Tools)), now), None);
    }

    #[test]
    fn cache_summary_names_why_the_next_request_misses() {
        let now = mu_base::message::now_ms();
        let t = cache_summary(&cache(100, 900, now - 6 * 60_000, None), now);
        assert!(t.contains("read 90% from cache (900 of 1K input tokens)"), "{t}");
        assert!(t.contains("Next message: not cached — expired after 6 min idle"), "{t}");
        let t = cache_summary(&cache(100, 900, now, Some(CacheBreak::Compacted)), now);
        assert!(t.ends_with("Next message: not cached — context was compacted"), "{t}");
        let t = cache_summary(&cache(100, 900, now, None), now);
        assert!(t.ends_with("Next message: likely cached"), "{t}");
    }
}
