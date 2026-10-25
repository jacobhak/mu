//! Footer: an optional quota line, then one status line built from the user's omp
//! `statusLine` segments (`config.yml`). Segment ids mu cannot supply (e.g. `pr`, `subagents`) are skipped.
use super::line::{Line, line_width, span, truncate};
use super::theme::{self, Role};
use crate::config::OmpStatusLine;
use crate::events::StatusSnapshot;
use crate::gitinfo::GitInfo;
use crate::quota::ProviderUsage;
use crossterm::style::ContentStyle;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Segments shown when `statusLine` lists none (omp's `default` preset).
const DEFAULT_LEFT: &[&str] = &["pi", "model", "mode", "path", "git", "context_pct", "cost"];
const DEFAULT_RIGHT: &[&str] = &["session_name"];
/// Every id this renderer knows.
const SEGMENT_IDS: &[&str] =
    &["pi", "model", "mode", "path", "git", "context_pct", "context_total", "cost", "session_name"];
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
    pub config: &'a OmpStatusLine,
    /// Active profile (`OMP_PROFILE`), shown as `μ <profile>`.
    pub profile: Option<String>,
    /// Ctrl+O "expanded" state.
    pub expanded: bool,
    /// Elapsed time of the running turn; drives the spinner and timer in the `pi` segment.
    pub turn_elapsed: Option<Duration>,
}

impl<'a> StatusEnv<'a> {
    pub fn new(cwd: &'a Path, config: &'a OmpStatusLine) -> Self {
        Self {
            cwd,
            home: std::env::var("HOME").ok().filter(|h| !h.is_empty()),
            config,
            profile: std::env::var("OMP_PROFILE")
                .or_else(|_| std::env::var("PI_PROFILE"))
                .ok()
                .filter(|p| !p.is_empty()),
            expanded: false,
            turn_elapsed: None,
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
        let level = if pct >= 80 { Role::Err } else if pct >= 50 { Role::Warn } else { Role::Muted };
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
        } else if m % 60 == 0 {
            format!("{}h", m / 60)
        } else {
            format!("{}h {}m", m / 60, m % 60)
        };
    }
    let h = (secs / 3600.0).round() as u64;
    if h < 24 {
        format!("{h}h")
    } else if h % 24 == 0 {
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
    let mut path_max = env
        .config
        .segment_options
        .get("path")
        .and_then(|p| p["maxLength"].as_u64())
        .map_or(DEFAULT_PATH_MAX, |n| n as usize);
    let mut left = segments(&env.config.left_segments, DEFAULT_LEFT, s, env, path_max);
    if env.expanded {
        left.push(Part { id: "expanded", line: vec![span("expanded", theme::get().fg(Role::Faint))] });
    }
    let mut right = segments(&env.config.right_segments, DEFAULT_RIGHT, s, env, path_max);
    if let Some(u) = s.usage.as_ref().filter(|u| !u.windows.is_empty()) {
        right.insert(0, Part { id: "usage", line: usage_line(u) });
    }

    // Overflow, in omp's order: shorten the title, drop right segments, shorten the path, drop left segments.
    let total = |left: &[Part], right: &[Part]| group_width(left) + group_width(right) + gap(left, right);
    if total(&left, &right) > width {
        if let Some(i) = right.iter().position(|p| p.id == "session_name") {
            let overflow = total(&left, &right) - width;
            let cur = line_width(&right[i].line);
            let target = cur.saturating_sub(overflow).max(MIN_TITLE_WIDTH.min(cur));
            right[i].line = truncate(&right[i].line, target);
        }
    }
    while total(&left, &right) > width && !right.is_empty() {
        right.pop();
    }
    while total(&left, &right) > width && path_max > MIN_PATH_MAX && left.iter().any(|p| p.id == "path") {
        path_max -= 1;
        let line = segment("path", s, env, path_max).unwrap_or_default();
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

/// Renders the configured ids in order, skipping unknown ids and segments with nothing to show.
fn segments(ids: &[String], fallback: &[&str], s: &StatusSnapshot, env: &StatusEnv, path_max: usize) -> Vec<Part> {
    let names: Vec<&str> = if ids.is_empty() { fallback.to_vec() } else { ids.iter().map(String::as_str).collect() };
    names
        .into_iter()
        .filter_map(|name| {
            let id = SEGMENT_IDS.iter().copied().find(|k| *k == name)?;
            Some(Part { id, line: segment(id, s, env, path_max)? })
        })
        .collect()
}

fn segment(id: &str, s: &StatusSnapshot, env: &StatusEnv, path_max: usize) -> Option<Line> {
    let t = theme::get();
    let st = |role: Role| t.fg(role);
    Some(match id {
        "pi" => {
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
            line
        }
        "model" => {
            let mut line = vec![span(s.model_name.clone(), st(Role::Tool))];
            if let Some(g) = thinking_glyph(s, env) {
                line.push(span(format!(" {g}"), st(Role::Muted)));
            }
            line
        }
        "mode" => {
            if !s.plan {
                return None;
            }
            vec![span("plan", st(Role::Accent))]
        }
        "path" => path_line(env, path_max),
        "git" => git_line(s.git.as_ref()?, env)?,
        "context_pct" => context_line(s),
        "context_total" => {
            if s.context_window == 0 {
                return None;
            }
            vec![span(format!("{} ctx", format_number(s.context_window as u64)), st(Role::Muted))]
        }
        "cost" => {
            if !s.subscription && s.cost <= 0.0 {
                return None;
            }
            // omp: a zero subscription spend reads `(sub)`; otherwise the unit (`S` subscription, `$` metered) and amount.
            let text = match (s.cost > 0.0, s.subscription) {
                (false, _) => "(sub)".to_string(),
                (true, true) => format!("S{:.2}", s.cost),
                (true, false) => format!("${:.2}", s.cost),
            };
            vec![span(text, st(Role::Muted))]
        }
        "session_name" => {
            let title = s.title.as_deref().filter(|t| !t.is_empty())?;
            vec![span(title, st(Role::Faint))]
        }
        _ => return None,
    })
}

/// Glyph omp shows in place of the model icon when thinking levels are shown (`compactThinkingLevel`).
fn thinking_glyph(s: &StatusSnapshot, env: &StatusEnv) -> Option<&'static str> {
    let show = env.config.segment_options.get("model").and_then(|m| m["showThinkingLevel"].as_bool()).unwrap_or(true);
    if !show || !s.reasoning {
        return None;
    }
    Some(match s.thinking.as_str() {
        "low" => "◔",
        "medium" => "◑",
        "high" => "◒",
        _ => "⦸",
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

/// `1.5% of 1M`: context share over the window, coloured by omp's thresholds.
fn context_line(s: &StatusSnapshot) -> Line {
    let window = s.context_window as u64;
    let pct = s.context_tokens.filter(|_| window > 0).map(|t| t as f64 / window as f64 * 100.0);
    let t = theme::get();
    match pct {
        Some(p) => vec![
            span(format!("{p:.1}%"), t.fg(context_color(pct, window))),
            span(format!(" of {}", format_number(window)), t.fg(Role::Faint)),
        ],
        None => vec![span(format_number(window), t.fg(Role::Muted))],
    }
}

/// omp's `getContextUsageLevel`: a level is reached at its percent or its token count, whichever is lower.
fn context_color(pct: Option<f64>, window: u64) -> Role {
    let Some(p) = pct.filter(|p| *p > 0.0) else { return Role::Muted };
    let reaches = |percent: f64, tokens: f64| {
        let token_pct = if window > 0 { tokens / window as f64 * 100.0 } else { f64::INFINITY };
        p >= percent.min(token_pct)
    };
    if reaches(70.0, 270_000.0) {
        Role::Err
    } else if reaches(50.0, 150_000.0) {
        Role::Warn
    } else {
        Role::Muted
    }
}

/// omp's `formatNumber`: `999`, `1.5K`, `25K`, `1M`, `1.5B`.
pub(super) fn format_number(n: u64) -> String {
    let trim1 = |v: f64, unit: &str| {
        let s = format!("{v:.1}");
        format!("{}{unit}", s.strip_suffix(".0").unwrap_or(&s))
    };
    match n {
        0..1_000 => n.to_string(),
        1_000..10_000 => trim1(n as f64 / 1e3, "K"),
        10_000..1_000_000 => format!("{}K", (n as f64 / 1e3).round() as u64),
        1_000_000..10_000_000 => trim1(n as f64 / 1e6, "M"),
        10_000_000..1_000_000_000 => format!("{}M", (n as f64 / 1e6).round() as u64),
        1_000_000_000..10_000_000_000 => trim1(n as f64 / 1e9, "B"),
        _ => format!("{}B", (n as f64 / 1e9).round() as u64),
    }
}

/// `branch *unstaged +staged ?untracked`, coloured clean/dirty as omp does. `None` when nothing shows.
fn git_line(g: &GitInfo, env: &StatusEnv) -> Option<Line> {
    let opts = env.config.segment_options.get("git");
    let opt = |k: &str| opts.and_then(|o| o[k].as_bool()).unwrap_or(true);
    let t = theme::get();
    let counts: Vec<(String, Role)> = [
        (opt("showUnstaged"), g.dirty, "*", Role::Warn),
        (opt("showStaged"), g.staged, "+", Role::Ok),
        (opt("showUntracked"), g.untracked, "?", Role::Accent),
    ]
    .into_iter()
    .filter(|(on, n, _, _)| *on && *n > 0)
    .map(|(_, n, prefix, role)| (format!("{prefix}{n}"), role))
        .collect();
    let mut line: Line = vec![];
    if opt("showBranch") {
        if let Some(b) = &g.branch {
            let role = if g.dirty + g.staged + g.untracked == 0 { Role::Ok } else { Role::Warn };
            line.push(span(b.clone(), t.fg(role)));
        }
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
    let opts = env.config.segment_options.get("path");
    let strip = opts.and_then(|o| o["stripWorkPrefix"].as_bool()).unwrap_or(true);
    let abbreviate = opts.and_then(|o| o["abbreviate"].as_bool()).unwrap_or(true);
    let home = env.home.as_deref().map(|h| h.trim_end_matches('/')).filter(|h| !h.is_empty());
    let (_, display) = path_display(env.cwd, home, strip);
    let text = if abbreviate { abbreviate_home(&display, home) } else { display };
    let text = clamp_left(&text, max);
    vec![span(text, theme::get().fg(Role::Literal))]
}

/// `(in a scratch dir, text)`: the path relative to its scratch or project root when stripping, else the full path.
fn path_display(cwd: &Path, home: Option<&str>, strip: bool) -> (bool, String) {
    let full = cwd.display().to_string();
    if !strip {
        return (false, full);
    }
    let mut scratch_roots: Vec<PathBuf> = vec![std::env::temp_dir(), "/tmp".into(), "/var/tmp".into()];
    if let Some(h) = home {
        scratch_roots.push(Path::new(h).join("tmp"));
    }
    for root in scratch_roots {
        if let Ok(rel) = cwd.strip_prefix(&root) {
            let rel = rel.display().to_string();
            return (true, if rel.is_empty() { full } else { rel });
        }
    }
    if let Some(h) = home {
        let h = Path::new(h);
        let roots = [h.join("Projects"), h.join("repos"), PathBuf::from("/work")];
        for root in roots {
            if let Ok(rel) = cwd.strip_prefix(&root) {
                let rel = rel.display().to_string();
                if !rel.is_empty() {
                    return (false, rel);
                }
            }
        }
    }
    (false, full)
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
    use crate::quota::QuotaWindow;
    use crate::tui::line::line_text;
    use std::collections::BTreeMap;

    fn cfg(left: &[&str], right: &[&str]) -> OmpStatusLine {
        let opts = |y: &str| serde_yaml_ng::from_str::<serde_yaml_ng::Value>(y).unwrap();
        OmpStatusLine {
            preset: Some("custom".into()),
            left_segments: left.iter().map(|s| s.to_string()).collect(),
            right_segments: right.iter().map(|s| s.to_string()).collect(),
            segment_options: BTreeMap::from([
                ("model".to_string(), opts("showThinkingLevel: true")),
                ("path".to_string(), opts("abbreviate: true\nmaxLength: 40\nstripWorkPrefix: true")),
            ]),
        }
    }

    fn snap() -> StatusSnapshot {
        StatusSnapshot {
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

    fn env<'a>(c: &'a OmpStatusLine, cwd: &'a Path) -> StatusEnv<'a> {
        StatusEnv {
            cwd,
            home: Some("/home/u".into()),
            config: c,
            profile: Some("private".into()),
            expanded: false,
            turn_elapsed: None,
        }
    }

    const LEFT: &[&str] = &["pi", "model", "path", "git", "context_pct", "cost"];

    #[test]
    fn renders_segments_in_order_with_right_title() {
        let c = cfg(LEFT, &["session_name"]);
        let lines = render(&snap(), &env(&c, Path::new("/home/u/code/mu")), 140);
        assert_eq!(lines.len(), 1);
        let t = line_text(&lines[0]);
        assert_eq!(line_width(&lines[0]), 140);
        assert!(t.contains(" 5h 82% 2h  7d 16% 4d · EVENTS"), "{t}");
        assert!(
            t.starts_with(" μ private · Haiku 5.5 ◒ · ~/code/mu · main *2 +1 ?3 · 1.5% of 1M · (sub) "),
            "{t}"
        );
        assert!(t.ends_with(" EVENTS: Read mu events.rs header "), "{t}");
    }

    #[test]
    fn skips_unknown_and_empty_segments() {
        let c = cfg(&["pi", "pr", "subagents", "bogus", "mode", "cost"], &[]);
        let mut s = snap();
        s.title = None;
        s.plan = false;
        s.subscription = false;
        s.cost = 0.0;
        // Only `pi` has something to show, and an empty right side means no padding.
        s.usage = None;
        assert_eq!(line_text(&render(&s, &env(&c, Path::new("/home/u/x")), 80)[0]), " μ private ");
        s.plan = true;
        s.cost = 0.25;
        let c = cfg(&["mode", "cost"], &[]);
        assert_eq!(line_text(&render(&s, &env(&c, Path::new("/home/u/x")), 80)[0]), " plan · $0.25 ");
    }

    #[test]
    fn never_exceeds_width_and_drops_in_omp_order() {
        let c = cfg(LEFT, &["session_name"]);
        let cwd = Path::new("/home/u/code/mu");
        for w in [5usize, 20, 40, 60, 80, 100] {
            for l in &render(&snap(), &env(&c, cwd), w) {
                assert!(line_width(l) <= w, "width {w}: {:?}", line_text(l));
            }
        }
        // Narrow enough to lose the title and the trailing segments, but not the brand.
        let t = line_text(&render(&snap(), &env(&c, cwd), 50)[0]);
        assert!(t.starts_with(" μ private"), "{t}");
        assert!(!t.contains("EVENTS"), "{t}");
    }

    #[test]
    fn path_rules() {
        let home = Some("/home/u");
        assert_eq!(path_display(Path::new("/tmp"), home, true), (true, "/tmp".into()));
        assert_eq!(path_display(Path::new("/tmp/a/b"), home, true), (true, "a/b".into()));
        assert_eq!(path_display(Path::new("/home/u/Projects/x"), home, true), (false, "x".into()));
        assert_eq!(path_display(Path::new("/home/u/src/x"), home, true), (false, "/home/u/src/x".into()));
        assert_eq!(path_display(Path::new("/home/u/src/x"), home, false), (false, "/home/u/src/x".into()));
        assert_eq!(abbreviate_home("/home/u/src/x", home), "~/src/x");
        assert_eq!(abbreviate_home("/home/user2/x", home), "/home/user2/x");
        assert_eq!(clamp_left("abcdefghij", 6), "…fghij");
    }

    #[test]
    fn context_levels_and_numbers() {
        assert_eq!(format_number(1_000_000), "1M");
        assert_eq!(format_number(200_000), "200K");
        assert_eq!(format_number(1_500), "1.5K");
        assert_eq!(format_number(1_500_000_000), "1.5B");
        assert_eq!(context_color(Some(1.5), 1_000_000), Role::Muted);
        assert_eq!(context_color(Some(20.0), 1_000_000), Role::Warn);
        assert_eq!(context_color(None, 1_000_000), Role::Muted);
        assert_eq!(context_color(Some(30.0), 1_000_000), Role::Err);
        assert_eq!(reset_text("5h", Duration::from_secs(90 * 60)), "1h 30m");
        assert_eq!(reset_text("7d", Duration::from_secs(30 * 3600)), "1d 6h");
        assert_eq!(reset_text("7d", Duration::from_secs(4 * 86400)), "4d");
    }

    #[test]
    fn git_counts_and_clean_repo() {
        let c = cfg(&["git"], &[]);
        let cwd = Path::new("/home/u/x");
        let mut s = snap();
        s.title = None;
        s.usage = None;
        s.git = Some(GitInfo { branch: Some("dev".into()), ..Default::default() });
        assert_eq!(line_text(&render(&s, &env(&c, cwd), 40)[0]), " dev ");
        s.git = Some(GitInfo { staged: 2, ..Default::default() });
        assert_eq!(line_text(&render(&s, &env(&c, cwd), 40)[0]), " +2 ");
        s.git = None;
        assert_eq!(render(&s, &env(&c, cwd), 40).len(), 1);
    }

    #[test]
    fn git_colours() {
        let c = cfg(&["git"], &[]);
        let cwd = Path::new("/home/u/x");
        let mut s = snap();
        s.title = None;
        s.usage = None;
        let fg = |l: &Line, txt: &str| l.iter().find(|p| p.text == txt).unwrap().style.foreground_color;
        s.git = Some(GitInfo { branch: Some("dev".into()), ..Default::default() });
        let l = &render(&s, &env(&c, cwd), 40)[0];
        assert_eq!(fg(l, "dev"), theme::get().color(Role::Ok));
        s.git = Some(GitInfo { branch: Some("dev".into()), dirty: 2, ..Default::default() });
        let l = &render(&s, &env(&c, cwd), 40)[0];
        assert_eq!(fg(l, "dev"), theme::get().color(Role::Warn));
        assert_eq!(fg(l, "*2"), theme::get().color(Role::Warn));
    }
}
