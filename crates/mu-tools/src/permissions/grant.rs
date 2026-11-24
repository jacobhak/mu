//! What "don't ask again" would remember, and the rules a grant becomes.

use super::bash::BashAnalysis;
use super::bash::SimpleCmd;
use super::classify::{Call, place, word_outside};
use super::verdict::{Gate, Hit, Subject};
use mu_base::permissions::{Action, Capability, Grant, GrantWidth, Rule};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// Programs whose grant pattern keeps the subcommand: `cargo test --release` grants `cargo test *`.
const SUBCOMMAND_PROGRAMS: &[&str] = &[
    "git",
    "cargo",
    "npm",
    "pnpm",
    "yarn",
    "bun",
    "go",
    "docker",
    "kubectl",
    "make",
    "just",
    "uv",
    "pip",
    "gh",
    "rustup",
    "brew",
    "systemctl",
];

/// What "don't ask again" would remember.
pub(super) fn grant(gate: &Gate, call: &Call, name: &str, args: &Value, hits: &[Hit]) -> Option<Grant> {
    let taken = |s: &Subject| !Gate::covered(hits, s) && !s.no_allow;
    let open = |s: &Subject| !Gate::covered(hits, s) && !gate.mode.allows(s.needs);
    match name {
        "bash" => {
            let analysis = call.bash.as_ref()?;
            if !analysis.parsed {
                return None;
            }
            // Each line with the widths above it: a read-only line widens by its path words,
            // a command only keeps the program and subcommand.
            let mut parts: Vec<(String, Vec<GrantWidth>)> = Vec::new();
            for (i, (c, s)) in analysis.cmds.iter().zip(&call.subjects).enumerate() {
                if !taken(s) || s.needs == Capability::ReadProject {
                    continue;
                }
                let part = if s.needs == Capability::ReadOutside {
                    (c.text.clone(), bash_widths(gate, call, analysis, i))
                } else {
                    (bash_pattern(c)?, Vec::new())
                };
                if !parts.iter().any(|(p, _)| *p == part.0) {
                    parts.push(part);
                }
            }
            if !parts.is_empty() {
                return Some(Grant {
                    tool: name.to_string(),
                    wider: merge_widths(&parts),
                    patterns: parts.into_iter().map(|(p, _)| p).collect(),
                });
            }
            // The commands are settled; what is left to ask about is the files the line writes. A
            // command no grant can cover leaves nothing a grant of the files would settle.
            if call.subjects.iter().any(|s| s.tool == name && open(s)) {
                return None;
            }
            let writes: Vec<&Subject> = call.subjects.iter().filter(|s| s.tool == "write" && open(s)).collect();
            if writes.is_empty() || writes.iter().any(|s| s.no_allow) {
                return None;
            }
            Some(Grant {
                tool: "write".to_string(),
                wider: path_widths(&writes),
                patterns: writes.iter().map(|s| s.text.clone()).collect(),
            })
        }
        "set_cwd" => {
            // What is left to ask about is the write the target directory stands for.
            let writes: Vec<&Subject> = call.subjects.iter().filter(|s| s.tool == "write" && open(s)).collect();
            if writes.is_empty() {
                return None;
            }
            Some(Grant {
                tool: "write".to_string(),
                wider: path_widths(&writes),
                patterns: writes.iter().map(|s| s.text.clone()).collect(),
            })
        }
        "write" | "edit" | "read" | "grep" | "glob" => {
            // The subjects are the paths, so the grant remembers exactly what was reached.
            let taken: Vec<&Subject> = call.subjects.iter().filter(|s| taken(s)).collect();
            if taken.is_empty() {
                return None;
            }
            Some(Grant {
                tool: name.to_string(),
                wider: path_widths(&taken),
                patterns: taken.iter().map(|s| s.text.clone()).collect(),
            })
        }
        "fetch" => {
            let patterns = fetch_patterns(args["url"].as_str().unwrap_or(""));
            // A URL with no host leaves nothing to scope a grant by.
            (!patterns.is_empty()).then(|| Grant { tool: name.to_string(), patterns, wider: Vec::new() })
        }
        // A tool with nothing to scope a grant by gets one row: every call of it.
        _ => Some(Grant { tool: name.to_string(), patterns: vec!["*".to_string()], wider: Vec::new() }),
    }
}

/// The widths of a plain path grant: its directory, the checkout it sits in, the checkouts beside
/// that, `~`, and `/`. They are named by their absolute paths, which a rule matches through the
/// subject's resolved path as well as its displayed one.
fn path_widths(subjects: &[&Subject]) -> Vec<GrantWidth> {
    let parts: Vec<(String, Vec<GrantWidth>)> = subjects
        .iter()
        .map(|s| (s.text.clone(), widths_of(s.abs.as_deref().and_then(|a| path_ladder(Path::new(a))))))
        .collect();
    merge_widths(&parts)
}

/// The read-only command at each width: its outside path words replaced by their landmark
/// directories (`cat /etc/*`, then `cat /home/*`).
fn bash_widths(gate: &Gate, call: &Call, analysis: &BashAnalysis, at: usize) -> Vec<GrantWidth> {
    let c = &analysis.cmds[at];
    let mut ladders: Vec<(usize, Vec<(&'static str, PathBuf)>)> = Vec::new();
    for w in &c.paths {
        let (Some(prefix), Some(i)) = (&w.prefix, w.word) else { continue };
        if !word_outside(gate, &call.bash_dir, analysis, at, Some(prefix)) {
            continue;
        }
        // A word an earlier `cd` moved has no known place to widen from.
        let known = std::path::Path::new(prefix).is_absolute() || analysis.unplaced_from.is_none_or(|n| at < n);
        if known && let Some(ladder) = path_ladder(&place(&call.bash_dir, prefix)) {
            ladders.push((i, ladder));
        }
    }
    let mut out: Vec<GrantWidth> = Vec::new();
    let mut n = 0;
    while ladders.iter().any(|(_, ladder)| ladder.len() > n) {
        let mut words = c.words.clone();
        for (i, ladder) in &ladders {
            if let Some((_, dir)) = ladder.get(n).or(ladder.last()) {
                words[*i] = glob_of(dir);
            }
        }
        let label = ladders.iter().find_map(|(_, l)| l.get(n)).map(|(label, _)| *label).unwrap_or("");
        let width = GrantWidth { label: label.to_string(), patterns: vec![words.join(" ")] };
        if out.last() != Some(&width) {
            out.push(width);
        }
        n += 1;
    }
    out
}

/// The widths of several parts (one per command) at once: each keeps its own last width when it
/// runs out, and a width that changes nothing is dropped.
fn merge_widths(parts: &[(String, Vec<GrantWidth>)]) -> Vec<GrantWidth> {
    let mut out: Vec<GrantWidth> = Vec::new();
    let mut n = 0;
    while parts.iter().any(|(_, w)| w.len() > n) {
        let patterns: Vec<String> = parts
            .iter()
            .map(|(exact, w)| {
                w.get(n).or(w.last()).and_then(|g| g.patterns.first().cloned()).unwrap_or_else(|| exact.clone())
            })
            .collect();
        let label = parts.iter().find_map(|(_, w)| w.get(n)).map(|g| g.label.clone()).unwrap_or_default();
        let width = GrantWidth { label, patterns };
        if out.last() != Some(&width) {
            out.push(width);
        }
        n += 1;
    }
    out
}

/// The named widths of one ladder, as the grant wants them.
fn widths_of(ladder: Option<Vec<(&'static str, PathBuf)>>) -> Vec<GrantWidth> {
    let Some(ladder) = ladder else { return Vec::new() };
    ladder
        .into_iter()
        .map(|(label, dir)| GrantWidth { label: label.to_string(), patterns: vec![glob_of(&dir)] })
        .collect()
}

/// The directories a path can widen to, meaningfully: its own, the checkout around it, the
/// checkouts beside that, home, and the filesystem root. Unnamed ancestors are not offered.
fn path_ladder(path: &Path) -> Option<Vec<(&'static str, PathBuf)>> {
    if !path.is_absolute() {
        return None;
    }
    let start = if path.is_dir() { path } else { path.parent()? };
    let mut out: Vec<(&'static str, PathBuf)> = vec![("its directory", start.to_path_buf())];
    if let Some(repo) = checkout_root(start) {
        if repo != start {
            out.push(("this checkout", repo));
        }
        if let Some(parent) = out.last().and_then(|(_, d)| d.parent()) {
            if let Some(home) = home_dir() {
                if parent != home {
                    out.push(("the checkouts beside it", parent.to_path_buf()));
                }
            } else {
                out.push(("the checkouts beside it", parent.to_path_buf()));
            }
        }
    }
    if let Some(home) = home_dir()
        && start.starts_with(&home)
        && out.last().is_some_and(|(_, d)| *d != home)
    {
        out.push(("all of ~", home));
    }
    if out.last().is_some_and(|(_, d)| *d != Path::new("/")) {
        out.push(("everything", PathBuf::from("/")));
    }
    out.dedup_by(|a, b| a.1 == b.1);
    Some(out)
}

/// The checkout a directory belongs to: the nearest ancestor holding a `.git` entry.
fn checkout_root(dir: &Path) -> Option<PathBuf> {
    dir.ancestors().find(|d| d.join(".git").exists()).map(Path::to_path_buf)
}

pub(super) fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").filter(|h| !h.is_empty()).map(PathBuf::from)
}

/// `<dir>/*`, which matches everything below it; `*` crosses `/`.
fn glob_of(dir: &Path) -> String {
    let text = dir.to_string_lossy();
    if text.ends_with('/') { format!("{text}*") } else { format!("{text}/*") }
}

/// The rules one grant becomes: each pattern, plus the directory itself when it names `<dir>/*`,
/// so searching the granted directory is covered too.
pub(super) fn grant_rules(g: &Grant) -> Vec<(String, Rule)> {
    let patterns = grant_patterns(&g.patterns);
    grant_tools(&g.tool)
        .into_iter()
        .flat_map(|tool| {
            patterns
                .iter()
                .map(move |p| (tool.to_string(), Rule { pattern: p.clone(), action: Action::Allow }))
                .collect::<Vec<_>>()
        })
        .collect()
}

/// The tools a grant covers. Reads are one family and changes another: `read`, `grep` and `glob`
/// reach the same bytes, and `write` and `edit` change the same paths, so a decision about one
/// holds for its siblings. The two families stay apart, since approving a read outside the project
/// must not allow writing there, and a write approval must not open reads.
pub(super) fn grant_tools(tool: &str) -> Vec<&str> {
    match tool {
        "read" | "grep" | "glob" => vec!["read", "grep", "glob"],
        "write" | "edit" => vec!["write", "edit"],
        other => vec![other],
    }
}

/// A grant's patterns, each widened to include the directory itself: `…/mu/*` also grants `…/mu`.
pub(super) fn grant_patterns(patterns: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for p in patterns {
        if let Some(head) = p.strip_suffix("/*")
            && !head.is_empty()
            && !out.iter().any(|s| s == head)
        {
            out.push(head.to_string());
        }
        if !out.iter().any(|s| s == p) {
            out.push(p.clone());
        }
    }
    out
}

pub(super) fn bash_pattern(c: &SimpleCmd) -> Option<String> {
    let argv0 = c.argv0.as_ref()?;
    let sub = c.second.as_deref().filter(|s| !s.starts_with('-') && SUBCOMMAND_PROGRAMS.contains(&argv0.as_str()));
    Some(match sub {
        Some(s) => format!("{argv0} {s} *"),
        None => format!("{argv0} *"),
    })
}

fn fetch_patterns(url: &str) -> Vec<String> {
    let Some((scheme, rest)) = url.split_once("://") else { return Vec::new() };
    let host = rest.split('/').next().unwrap_or("");
    if host.is_empty() { Vec::new() } else { vec![format!("{scheme}://{host}/*")] }
}
