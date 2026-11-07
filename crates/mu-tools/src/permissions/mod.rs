//! The tool permission gate: tiers, rules, grants and the approval prompt.

mod bash;
mod glob;
mod store;

pub use bash::{BashAnalysis, SimpleCmd, analyze};
pub use glob::glob_match;
pub use store::{add_global, add_project, load_project, project_file};

use crate::tools::Tools;
use crate::tools::summary;
use mu_base::config::MuPermissions;
use mu_base::events::{AgentEvent, EventTx};
use mu_base::permissions::{
    Action, ApprovalDecision, ApprovalRequest, ApprovalScope, Capability, Grant, GrantWidth, PermissionMode, Rule,
    RuleSet,
};
use mu_common::paths;
use serde_json::{Value, json};
use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use tokio::sync::oneshot;

/// How much one call may change.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tier {
    Read,
    Write,
    Exec,
}

/// How far a read reaches: inside the project, or anywhere else on the machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reach {
    Project,
    Outside,
}

/// What the gate decided for one call.
pub enum Verdict {
    Allow,
    Deny(String),
    Ask(ApprovalRequest),
}

/// Tools that only read.
const READ_TOOLS: &[&str] = &[
    "read",
    "grep",
    "glob",
    "todo",
    "ask",
    "propose_plan",
    "yield",
    "job_cancel",
    "wait",
    "task",
    "web_search",
    "fetch",
    "memory",
];

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

static EMPTY: RuleSet = RuleSet(Vec::new());

/// One project's resolved state: its root, the key grants are stored under, and its rules.
struct Project {
    root: PathBuf,
    key: PathBuf,
    rules: RefCell<RuleSet>,
}

/// The per-session permission state, shared by the agent and `Tools`.
pub struct Permissions {
    mode: Cell<PermissionMode>,
    readonly_bash: bool,
    global: RefCell<RuleSet>,
    project: std::cell::OnceCell<Project>,
    session: RefCell<RuleSet>,
}

impl Default for Permissions {
    fn default() -> Self {
        Self {
            mode: Cell::new(PermissionMode::Yolo),
            readonly_bash: true,
            global: RefCell::new(RuleSet::default()),
            project: std::cell::OnceCell::new(),
            session: RefCell::new(RuleSet::default()),
        }
    }
}

impl Permissions {
    pub fn new(cfg: &MuPermissions) -> Self {
        Self {
            mode: Cell::new(cfg.mode),
            readonly_bash: cfg.readonly_bash,
            global: RefCell::new(cfg.rules.clone()),
            project: std::cell::OnceCell::new(),
            session: RefCell::new(RuleSet::default()),
        }
    }

    pub fn mode(&self) -> PermissionMode {
        self.mode.get()
    }

    pub fn set_mode(&self, m: PermissionMode) {
        self.mode.set(m);
    }

    pub fn readonly_bash(&self) -> bool {
        self.readonly_bash
    }

    /// Resolves the project root, its key and its stored grants, once.
    pub async fn ensure_project(&self, cwd: &Path, warn: &mut Vec<String>) {
        if self.project.get().is_some() {
            return;
        }
        let root = mu_base::git::repo_root(cwd).await.unwrap_or_else(|_| cwd.to_path_buf());
        let root = canonical_nearest(&root);
        let key = mu_base::git::main_checkout(cwd).await;
        let rules = store::load_project(&key, warn);
        let _ = self.project.set(Project { root, key, rules: RefCell::new(rules) });
    }

    /// A `Grant` is added to a rule set and, when `persist`, written to disk first.
    pub fn remember(&self, g: &Grant, scope: ApprovalScope, persist: bool) -> Result<(), String> {
        if scope == ApprovalScope::Once {
            return Ok(());
        }
        let rules = match scope {
            ApprovalScope::Session => &self.session,
            ApprovalScope::Project => match self.project.get() {
                Some(p) => &p.rules,
                None => return Ok(()),
            },
            ApprovalScope::Global => &self.global,
            ApprovalScope::Once => return Ok(()),
        };
        let grant_rules = grant_rules(g);
        rules.borrow_mut().0.extend(grant_rules);
        match scope {
            ApprovalScope::Project if persist => store::add_project(&self.project.get().unwrap().key, g),
            ApprovalScope::Global if persist => store::add_global(g),
            _ => Ok(()),
        }
    }

    /// The payload a subagent inherits.
    pub fn inherit_json(&self) -> Value {
        json!({"mode": self.mode.get().as_str(), "session": self.session.borrow().to_json()})
    }

    pub fn apply_inherited(&self, v: &Value) {
        if let Some(m) = v["mode"].as_str().and_then(PermissionMode::parse) {
            self.mode.set(m);
        }
        self.session.borrow_mut().extend(&RuleSet::from_json(&v["session"]));
    }

    /// Asks the parent's UI on behalf of a subagent and passes the answer on.
    pub fn forward(
        self: &Rc<Self>,
        events: &EventTx,
        label: &str,
        mut req: ApprovalRequest,
    ) -> oneshot::Receiver<ApprovalDecision> {
        req.from = Some(match req.from.take() {
            Some(prev) => format!("{label} › {prev}"),
            None => label.to_string(),
        });
        let grant = req.grant.clone();
        let (tx, rx) = oneshot::channel();
        let _ = events.send(AgentEvent::Approve { request: req, reply: tx });
        let this = self.clone();
        let (out_tx, out_rx) = oneshot::channel();
        tokio::task::spawn_local(async move {
            let decision = rx.await.unwrap_or(ApprovalDecision::Deny { note: None });
            if let ApprovalDecision::Allow { scope, patterns, .. } = &decision
                && let Some(g) = &grant
            {
                // The originating subagent persists it; the parent only updates its memory.
                let _ = this.remember(&g.picked(patterns.clone()), *scope, false);
            }
            let _ = out_tx.send(decision);
        });
        out_rx
    }

    /// Decides one model tool call.
    pub fn check(&self, tools: &Tools, name: &str, args: &Value) -> Verdict {
        let call = match self.call(tools, name, args) {
            Ok(c) => c,
            Err(_) => return Verdict::Allow,
        };
        let hits = self.rule_hits(name, &call.subjects);
        if let Some((rule, source)) = hits.iter().find(|(r, _)| r.action == Action::Deny) {
            return Verdict::Deny(format!(
                "Not run: permission rule \"{}\" = \"deny\" for {name} ({source}).",
                rule.pattern
            ));
        }
        let ask = hits.iter().find(|(r, _)| r.action == Action::Ask);
        let covered = !call.subjects.is_empty()
            && call.subjects.iter().all(|s| {
                s.classified || (!s.no_allow && hits.iter().any(|(r, _)| r.action == Action::Allow && hit(r, s)))
            });
        if ask.is_none() && covered {
            return Verdict::Allow;
        }
        let capability = call.capability();
        if ask.is_none() && self.mode.get().allows(capability) {
            return Verdict::Allow;
        }
        let reason = match ask {
            Some((rule, source)) => format!("rule \"{}\" = ask ({source})", rule.pattern),
            None if capability == Capability::ReadOutside => {
                format!("{} mode: reading outside the project needs approval", self.mode.get().as_str())
            }
            None => format!("{} mode: {name} needs approval", self.mode.get().as_str()),
        };
        let grant = call.grant(tools, name, args, &hits);
        Verdict::Ask(ApprovalRequest {
            tool: name.to_string(),
            call_id: String::new(),
            title: format!("{name}: {}", summary(name, args)),
            reason,
            body: truncate_body(&body(name, args)),
            from: None,
            grant,
        })
    }

    fn rule_hits(&self, name: &str, subjects: &[Subject]) -> Vec<(Rule, &'static str)> {
        let global = self.global.borrow();
        let project = self.project.get().map(|p| p.rules.borrow());
        let session = self.session.borrow();
        let sets: [(&RuleSet, &'static str); 3] = [
            (&global, "~/.mu/config.toml"),
            (project.as_deref().unwrap_or(&EMPTY), "project permissions"),
            (&session, "this session"),
        ];
        let mut hits = Vec::new();
        for (set, source) in sets {
            for (tool, rule) in &set.0 {
                if glob_match(tool, name) && subjects.iter().any(|s| hit(rule, s)) {
                    hits.push((rule.clone(), source));
                }
            }
        }
        hits
    }

    /// The subjects and tier of one call; `Err(())` when the arguments do not fit the tool, which
    /// is the tool's own error to report.
    fn call(&self, tools: &Tools, name: &str, args: &Value) -> Result<Call, ()> {
        let root = match self.project.get() {
            Some(p) => p.root.clone(),
            None => canonical_nearest(&tools.cwd),
        };
        let mut call =
            Call { subjects: Vec::new(), tier: Tier::Read, reach: Reach::Project, root: root.clone(), bash: None };
        if name == "bash" {
            let Some(cmd) = args["command"].as_str() else { return Err(()) };
            let analysis = analyze(cmd);
            call.tier = if self.readonly_bash
                && analysis.parsed
                && !analysis.cmds.is_empty()
                && analysis.cmds.iter().all(|c| c.readonly)
            {
                Tier::Read
            } else {
                Tier::Exec
            };
            let outside: Vec<bool> = analysis
                .cmds
                .iter()
                .map(|c| c.readonly && self.readonly_bash && reads_outside(tools, &root, c))
                .collect();
            if outside.iter().any(|o| *o) {
                call.reach = Reach::Outside;
            }
            call.subjects = analysis
                .cmds
                .iter()
                .zip(&outside)
                .map(|(c, outside)| Subject {
                    text: c.text.clone(),
                    abs: None,
                    classified: c.readonly && self.readonly_bash && !*outside,
                    no_allow: c.assigns || !analysis.parsed,
                })
                .collect();
            call.bash = Some(analysis);
            return Ok(call);
        }
        if matches!(name, "write" | "edit") {
            let paths = if name == "write" {
                serde_json::from_value::<crate::tools::write::Args>(args.clone()).map_err(|_| ())?.paths()
            } else {
                serde_json::from_value::<crate::tools::edit::Args>(args.clone()).map_err(|_| ())?.paths()
            };
            let mut tier = Tier::Read;
            for raw in &paths {
                let kind = target_kind(tools, &root, raw);
                tier = tier.max(kind);
                call.subjects.push(Subject {
                    text: path_subject(&root, tools, raw),
                    abs: abs_of(tools, raw),
                    classified: false,
                    no_allow: false,
                });
            }
            call.tier = tier;
            return Ok(call);
        }
        if matches!(name, "read" | "grep" | "glob") {
            let raw = match name {
                "read" => args["path"].as_str().unwrap_or(""),
                _ => args["path"].as_str().unwrap_or("."),
            };
            call.reach = reach_of_path(tools, &root, raw);
            call.subjects.push(Subject {
                text: path_subject(&root, tools, raw),
                abs: abs_of(tools, raw),
                classified: false,
                no_allow: false,
            });
            call.tier = Tier::Read;
            return Ok(call);
        }
        if name == "fetch" {
            call.subjects.push(Subject {
                text: args["url"].as_str().unwrap_or("").to_string(),
                abs: None,
                classified: false,
                no_allow: false,
            });
        } else {
            call.subjects.push(Subject { text: String::new(), abs: None, classified: false, no_allow: false });
        }
        call.tier = if READ_TOOLS.contains(&name) { Tier::Read } else { Tier::Exec };
        Ok(call)
    }
}

struct Call {
    subjects: Vec<Subject>,
    tier: Tier,
    reach: Reach,
    /// The project root, and the key grants are stored under.
    root: PathBuf,
    bash: Option<BashAnalysis>,
}

/// A rule matches a subject by its displayed text (relative inside the project) or by its absolute
/// path, so a grant can name the directory a path sits in either way.
fn hit(rule: &Rule, s: &Subject) -> bool {
    glob_match(&rule.pattern, &s.text) || s.abs.as_deref().is_some_and(|a| glob_match(&rule.pattern, a))
}

struct Subject {
    text: String,
    /// The resolved path, as text, when the subject is a file. Widths and absolute rules use it.
    abs: Option<String>,
    /// The read-only classifier covers it (bash only, with `readonly_bash` on).
    classified: bool,
    /// An `allow` rule can never cover it (leading assignments, or a line that does not parse).
    no_allow: bool,
}

impl Call {
    /// What the mode has to grant for this call to run unasked.
    fn capability(&self) -> Capability {
        match (self.tier, self.reach) {
            (Tier::Read, Reach::Project) => Capability::ReadProject,
            (Tier::Read, Reach::Outside) => Capability::ReadOutside,
            (Tier::Write, _) => Capability::WriteProject,
            (Tier::Exec, _) => Capability::Exec,
        }
    }

    /// What "don't ask again" would remember.
    fn grant(&self, tools: &Tools, name: &str, args: &Value, hits: &[(Rule, &'static str)]) -> Option<Grant> {
        let allow = |s: &Subject| hits.iter().any(|(r, _)| r.action == Action::Allow && hit(r, s));
        match name {
            "bash" => {
                let analysis = self.bash.as_ref()?;
                if !analysis.parsed {
                    return None;
                }
                // Each line with the widths above it: a read-only line widens by its path words,
                // a command only keeps the program and subcommand.
                let mut parts: Vec<(String, Vec<GrantWidth>)> = Vec::new();
                for (i, c) in analysis.cmds.iter().enumerate() {
                    if self.subjects.get(i).is_some_and(|s| s.classified || allow(s)) {
                        continue;
                    }
                    let part = if self.tier == Tier::Read {
                        (c.text.clone(), bash_widths(tools, &self.root, c))
                    } else {
                        (bash_pattern(c)?, Vec::new())
                    };
                    if !parts.iter().any(|(p, _)| *p == part.0) {
                        parts.push(part);
                    }
                }
                if parts.is_empty() {
                    return None;
                }
                Some(Grant {
                    tool: name.to_string(),
                    wider: merge_widths(&parts),
                    patterns: parts.into_iter().map(|(p, _)| p).collect(),
                })
            }
            "write" | "edit" | "read" | "grep" | "glob" => {
                // The subjects are the paths, so the grant remembers exactly what was reached.
                let taken: Vec<&Subject> = self.subjects.iter().filter(|s| !s.classified && !allow(s)).collect();
                if taken.is_empty() {
                    return None;
                }
                Some(Grant {
                    tool: name.to_string(),
                    wider: path_widths(&taken),
                    patterns: taken.iter().map(|s| s.text.clone()).collect(),
                })
            }
            "fetch" => Some(Grant {
                tool: name.to_string(),
                patterns: fetch_patterns(args["url"].as_str().unwrap_or("")),
                wider: Vec::new(),
            }),
            _ => {
                let _ = tools;
                Some(Grant { tool: name.to_string(), patterns: Vec::new(), wider: Vec::new() })
            }
        }
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
fn bash_widths(tools: &Tools, root: &Path, c: &SimpleCmd) -> Vec<GrantWidth> {
    let mut ladders: Vec<(usize, Vec<(&'static str, PathBuf)>)> = Vec::new();
    for w in &c.paths {
        let (Some(prefix), Some(i)) = (&w.prefix, w.word) else { continue };
        if reach_of_path(tools, root, prefix) != Reach::Outside {
            continue;
        }
        if let Ok(r) = tools.resolve(prefix)
            && r.scheme.is_none()
            && let Some(ladder) = path_ladder(&r.path)
        {
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

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").filter(|h| !h.is_empty()).map(PathBuf::from)
}

/// `<dir>/*`, which matches everything below it; `*` crosses `/`.
fn glob_of(dir: &Path) -> String {
    let text = dir.to_string_lossy();
    if text.ends_with('/') { format!("{text}*") } else { format!("{text}/*") }
}

/// The rules one grant becomes: each pattern, plus the directory itself when it names `<dir>/*`,
/// so searching the granted directory is covered too.
fn grant_rules(g: &Grant) -> Vec<(String, Rule)> {
    let patterns = grant_patterns(&g.patterns);
    let patterns = if patterns.is_empty() { vec!["*".to_string()] } else { patterns };
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
fn grant_tools(tool: &str) -> Vec<&str> {
    match tool {
        "read" | "grep" | "glob" => vec!["read", "grep", "glob"],
        "write" | "edit" => vec!["write", "edit"],
        other => vec![other],
    }
}

/// A grant's patterns, each widened to include the directory itself: `…/mu/*` also grants `…/mu`.
fn grant_patterns(patterns: &[String]) -> Vec<String> {
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

fn bash_pattern(c: &SimpleCmd) -> Option<String> {
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

/// A target's tier: `local://` and the read-only schemes are read, an in-project path is write,
/// anything else is exec.
fn target_kind(tools: &Tools, root: &Path, raw: &str) -> Tier {
    match tools.resolve(raw) {
        Ok(r) if r.scheme.is_some() => Tier::Read,
        Err(_) if raw.contains("://") => Tier::Exec,
        Ok(r) => {
            let c = canonical_nearest(&r.path);
            if c.starts_with(root) && !protected(root, &c) { Tier::Write } else { Tier::Exec }
        }
        Err(_) => Tier::Exec,
    }
}

/// Whether a path a tool names lies outside the project; internal URLs always stay inside.
fn reach_of_path(tools: &Tools, root: &Path, raw: &str) -> Reach {
    match tools.resolve(raw) {
        Ok(r) if r.scheme.is_some() => Reach::Project,
        Ok(r) if canonical_nearest(&r.path).starts_with(root) => Reach::Project,
        _ => Reach::Outside,
    }
}

/// Whether a read-only command may open something outside the project.
fn reads_outside(tools: &Tools, root: &Path, c: &SimpleCmd) -> bool {
    c.paths.iter().any(|w| match &w.prefix {
        Some(prefix) => reach_of_path(tools, root, prefix) == Reach::Outside,
        None => true,
    })
}

/// `.git` of the project and mu's own home are never edited by an `edits`-mode call.
fn protected(root: &Path, c: &Path) -> bool {
    if c.starts_with(root.join(".git")) {
        return true;
    }
    c.starts_with(canonical_nearest(&paths::mu_home()))
}

/// A subject's resolved path as text, for widths and absolute rules. `None` when it names no file:
/// a `local://` page, or text that does not resolve at all.
fn abs_of(tools: &Tools, raw: &str) -> Option<String> {
    let r = tools.resolve(raw).ok()?;
    if r.scheme.is_some() {
        return None;
    }
    Some(canonical_nearest(&r.path).to_string_lossy().into_owned())
}

/// The subject of a path: relative to the project root when inside it, else absolute.
fn path_subject(root: &Path, tools: &Tools, raw: &str) -> String {
    match tools.resolve(raw) {
        Ok(r) if r.scheme.is_some() => raw.to_string(),
        Ok(r) => {
            let c = canonical_nearest(&r.path);
            match c.strip_prefix(root) {
                Ok(rel) if rel.as_os_str().is_empty() => ".".to_string(),
                Ok(rel) => rel.to_string_lossy().into_owned(),
                Err(_) => c.to_string_lossy().into_owned(),
            }
        }
        Err(_) => raw.to_string(),
    }
}

/// Canonicalises the nearest existing ancestor and appends the rest, so symlinks resolve.
fn canonical_nearest(p: &Path) -> PathBuf {
    let mut rest: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = p;
    loop {
        if let Ok(c) = cur.canonicalize() {
            let mut out = c;
            for r in rest.iter().rev() {
                out.push(r);
            }
            return out;
        }
        match (cur.parent(), cur.file_name()) {
            (Some(parent), Some(name)) if !parent.as_os_str().is_empty() => {
                rest.push(name.to_os_string());
                cur = parent;
            }
            _ => return p.to_path_buf(),
        }
    }
}

fn body(name: &str, args: &Value) -> String {
    match name {
        "bash" => args["command"].as_str().unwrap_or("").to_string(),
        "write" => {
            let path = args["path"].as_str().unwrap_or("");
            let content = args["content"].as_str().unwrap_or("");
            format!("{path}\n\n{content}")
        }
        "edit" => args["input"].as_str().unwrap_or("").to_string(),
        _ => serde_json::to_string_pretty(args).unwrap_or_else(|_| args.to_string()),
    }
}

fn truncate_body(s: &str) -> String {
    const MAX: usize = 40;
    let lines: Vec<&str> = s.lines().collect();
    if lines.len() <= MAX {
        s.to_string()
    } else {
        format!("{}\n… ({} more lines)", lines[..MAX].join("\n"), lines.len() - MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mu_testutil::{block_on, repo};

    fn allow(pattern: &str) -> (String, Rule) {
        ("bash".into(), Rule { pattern: pattern.into(), action: Action::Allow })
    }

    fn ask_rule(pattern: &str) -> (String, Rule) {
        ("bash".into(), Rule { pattern: pattern.into(), action: Action::Ask })
    }

    fn deny_rule(pattern: &str) -> (String, Rule) {
        ("bash".into(), Rule { pattern: pattern.into(), action: Action::Deny })
    }

    fn setup(mode: PermissionMode) -> (Permissions, Tools) {
        mu_testutil::home();
        let root = repo();
        let perms = Permissions::new(&MuPermissions { mode, readonly_bash: true, rules: RuleSet::default() });
        block_on(perms.ensure_project(&root, &mut Vec::new()));
        let mut t = Tools::for_test();
        t.cwd = root;
        (perms, t)
    }

    fn verdict(p: &Permissions, t: &Tools, cmd: &str) -> Verdict {
        p.check(t, "bash", &json!({"command": cmd}))
    }

    #[test]
    fn titles_paths_and_local_urls_get_their_tier() {
        let (p, t) = setup(PermissionMode::Edits);
        let root = t.cwd.clone();
        t.set_session("sid", None);
        let v = p.check(&t, "write", &json!({"path": root.join("a.txt").to_string_lossy(), "content": "x"}));
        assert!(matches!(v, Verdict::Allow), "inside the project is write tier");
        let v = p.check(&t, "write", &json!({"path": "../x.txt", "content": "x"}));
        assert!(matches!(v, Verdict::Ask(_)), "../x is exec tier");
        let v = p.check(&t, "write", &json!({"path": root.join(".git/config").to_string_lossy(), "content": "x"}));
        assert!(matches!(v, Verdict::Ask(_)), ".git is protected");
        let v = p.check(&t, "write", &json!({"path": "local://p.md", "content": "x"}));
        assert!(matches!(v, Verdict::Allow), "local:// is read tier");
    }

    #[test]
    fn reads_outside_the_project_ask_while_inside_ones_run() {
        for mode in [PermissionMode::Ask, PermissionMode::Edits] {
            let (p, t) = setup(mode);
            assert!(matches!(p.check(&t, "read", &json!({"path": "/etc/passwd"})), Verdict::Ask(_)), "{mode:?}");
            assert!(matches!(p.check(&t, "grep", &json!({"pattern": "x", "path": "/etc"})), Verdict::Ask(_)));
            assert!(matches!(p.check(&t, "read", &json!({"path": "src/lib.rs"})), Verdict::Allow));
            assert!(matches!(p.check(&t, "glob", &json!({"pattern": "*"})), Verdict::Allow));
            assert!(
                matches!(p.check(&t, "read", &json!({"path": "local://p.md"})), Verdict::Allow),
                "mu's own pages stay in"
            );
            let Verdict::Ask(req) = p.check(&t, "read", &json!({"path": "/etc/passwd"})) else { panic!() };
            assert_eq!(req.reason, format!("{} mode: reading outside the project needs approval", mode.as_str()));
        }
        let (p, t) = setup(PermissionMode::Yolo);
        assert!(matches!(p.check(&t, "read", &json!({"path": "/etc/passwd"})), Verdict::Allow));
    }

    #[test]
    fn an_outside_read_is_granted_by_its_path() {
        let (p, t) = setup(PermissionMode::Ask);
        let Verdict::Ask(req) = p.check(&t, "read", &json!({"path": "/etc/passwd"})) else { panic!() };
        assert_eq!(req.grant.as_ref().unwrap().patterns, ["/etc/passwd"]);
        p.remember(&req.grant.unwrap(), ApprovalScope::Session, false).unwrap();
        assert!(matches!(p.check(&t, "read", &json!({"path": "/etc/passwd"})), Verdict::Allow));
        assert!(matches!(p.check(&t, "read", &json!({"path": "/etc/hosts"})), Verdict::Ask(_)));
    }

    #[test]
    fn read_only_bash_that_leaves_the_project_asks() {
        let (p, t) = setup(PermissionMode::Ask);
        assert!(matches!(verdict(&p, &t, "cat /etc/passwd"), Verdict::Ask(_)));
        assert!(matches!(verdict(&p, &t, "cat < /etc/passwd"), Verdict::Ask(_)), "an input redirection reads too");
        assert!(matches!(verdict(&p, &t, "cat ~/.ssh/id_rsa"), Verdict::Ask(_)), "a tilde can point anywhere");
        assert!(matches!(verdict(&p, &t, "ls -la && cat /etc/passwd"), Verdict::Ask(_)), "one outside read is enough");
        assert!(matches!(verdict(&p, &t, "cat README.md"), Verdict::Allow));
        assert!(matches!(verdict(&p, &t, "git status"), Verdict::Allow), "the subcommand is not a path");
        assert!(matches!(verdict(&p, &t, "echo /etc/passwd"), Verdict::Allow), "echo prints, it does not read");
    }

    #[test]
    fn a_grant_offers_landmarks_rather_than_every_ancestor() {
        let (p, t) = setup(PermissionMode::Ask);
        let Verdict::Ask(req) = p.check(&t, "read", &json!({"path": "/etc/passwd"})) else { panic!() };
        let g = req.grant.unwrap();
        assert_eq!(g.patterns, ["/etc/passwd"]);
        let labels: Vec<&str> = g.wider.iter().map(|w| w.label.as_str()).collect();
        assert_eq!(labels, ["its directory", "everything"], "no checkout and no home above this path");
        assert_eq!(g.wider[0].patterns, ["/etc/*"]);
        assert_eq!(g.wider[1].patterns, ["/*"]);

        let Verdict::Ask(req) = p.check(&t, "grep", &json!({"pattern": "x", "path": "/etc"})) else { panic!() };
        assert_eq!(req.grant.unwrap().wider[0].patterns, ["/etc/*"], "a directory is its own container");

        let Verdict::Ask(req) = verdict(&p, &t, "cat /etc/passwd") else { panic!() };
        let g = req.grant.unwrap();
        assert_eq!(g.patterns, ["cat /etc/passwd"]);
        assert_eq!(g.wider[0].patterns, ["cat /etc/*"]);
        assert_eq!(g.wider[1].patterns, ["cat /*"]);
    }

    #[test]
    fn the_checkout_is_a_landmark_however_deep_the_path() {
        let (p, t) = setup(PermissionMode::Ask);
        let base = std::env::temp_dir().join(format!("mu-landmarks-{}", std::process::id()));
        let repo = base.join("checkout");
        std::fs::create_dir_all(repo.join("src").join("deep")).unwrap();
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::write(repo.join("src/deep/x.rs"), "x").unwrap();
        let (base, repo) = (base.canonicalize().unwrap(), repo.canonicalize().unwrap());
        let path = repo.join("src/deep/x.rs");

        let Verdict::Ask(req) = p.check(&t, "read", &json!({"path": path.to_string_lossy()})) else { panic!() };
        let g = req.grant.unwrap();
        let labels: Vec<&str> = g.wider.iter().map(|w| w.label.as_str()).collect();
        assert_eq!(labels, ["its directory", "this checkout", "the checkouts beside it", "everything"]);
        assert_eq!(g.wider[0].patterns, [format!("{}/src/deep/*", repo.display())]);
        assert_eq!(g.wider[1].patterns, [format!("{}/*", repo.display())], "the git root, not three parents up");
        assert_eq!(g.wider[2].patterns, [format!("{}/*", base.display())]);
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn a_session_grant_for_the_checkout_covers_the_files_inside_it() {
        let (p, t) = setup(PermissionMode::Ask);
        let base = std::env::temp_dir().join(format!("mu-checkout-{}", std::process::id()));
        let repo = base.join("other");
        std::fs::create_dir_all(repo.join("src/deep")).unwrap();
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::write(repo.join("src/deep/a.rs"), "a").unwrap();
        std::fs::write(repo.join("src/b.rs"), "b").unwrap();
        let repo = repo.canonicalize().unwrap();
        let (first, second) = (repo.join("src/deep/a.rs"), repo.join("src/b.rs"));

        let Verdict::Ask(req) = p.check(&t, "read", &json!({"path": first.to_string_lossy()})) else { panic!() };
        let g = req.grant.unwrap();
        let width = g.wider.iter().find(|w| w.label == "this checkout").expect("a checkout width").patterns.clone();
        assert_eq!(width, [format!("{}/*", repo.display())]);
        p.remember(&g.picked(Some(width)), ApprovalScope::Session, true).unwrap();
        assert!(matches!(p.check(&t, "read", &json!({"path": second.to_string_lossy()})), Verdict::Allow));
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn a_write_grant_covers_editing_the_same_file() {
        let (p, t) = setup(PermissionMode::Ask);
        let Verdict::Ask(req) = p.check(&t, "write", &json!({"path": "a/b.txt", "content": "x"})) else { panic!() };
        p.remember(&req.grant.unwrap(), ApprovalScope::Session, false).unwrap();
        assert!(matches!(p.check(&t, "write", &json!({"path": "a/b.txt", "content": "y"})), Verdict::Allow));
        let edit = |path: &str| json!({"input": format!("*** Edit File: {path}\n*** Find\nx\n*** Replace\ny")});
        assert!(matches!(p.check(&t, "edit", &edit("a/b.txt")), Verdict::Allow), "editing the file the grant names");
        assert!(matches!(p.check(&t, "edit", &edit("a/c.txt")), Verdict::Ask(_)), "another file");
    }

    #[test]
    fn a_read_grant_covers_grep_and_glob_of_the_same_directory() {
        let (p, t) = setup(PermissionMode::Ask);
        let Verdict::Ask(req) = p.check(&t, "read", &json!({"path": "/etc/passwd"})) else { panic!() };
        let g = req.grant.unwrap().picked(Some(vec!["/etc/*".into()]));
        p.remember(&g, ApprovalScope::Session, false).unwrap();
        assert!(matches!(p.check(&t, "read", &json!({"path": "/etc/hosts"})), Verdict::Allow));
        assert!(matches!(p.check(&t, "grep", &json!({"pattern": "x", "path": "/etc/hosts"})), Verdict::Allow));
        assert!(
            matches!(p.check(&t, "grep", &json!({"pattern": "x", "path": "/etc"})), Verdict::Allow),
            "the directory itself"
        );
        assert!(matches!(p.check(&t, "glob", &json!({"pattern": "*", "path": "/etc"})), Verdict::Allow));
        assert!(matches!(p.check(&t, "read", &json!({"path": "/root/x"})), Verdict::Ask(_)));
        assert!(matches!(verdict(&p, &t, "cat /etc/hosts"), Verdict::Ask(_)), "a command is its own grant");
    }

    #[test]
    fn a_path_inside_the_project_widens_to_its_directory() {
        let (p, t) = setup(PermissionMode::Ask);
        let root = t.cwd.clone();
        let Verdict::Ask(req) = p.check(&t, "write", &json!({"path": "src/a.rs", "content": "x"})) else { panic!() };
        let grant = req.grant.unwrap();
        assert_eq!(grant.patterns, ["src/a.rs"]);
        // The test checkout sits outside ~, so the ~ rung is not on this ladder.
        let labels: Vec<&str> = grant.wider.iter().map(|w| w.label.as_str()).collect();
        assert_eq!(labels, ["its directory", "this checkout", "the checkouts beside it", "everything"]);
        assert_eq!(grant.wider[0].patterns, [format!("{}/src/*", root.display())]);
        assert_eq!(grant.wider[1].patterns, [format!("{}/*", root.display())]);

        // The width names an absolute directory, and a rule matches it through the resolved path.
        p.remember(&grant.picked(Some(grant.wider[0].patterns.clone())), ApprovalScope::Session, true).unwrap();
        assert!(matches!(p.check(&t, "write", &json!({"path": "src/b.rs", "content": "x"})), Verdict::Allow));
        assert!(matches!(p.check(&t, "write", &json!({"path": "c.txt", "content": "x"})), Verdict::Ask(_)));
    }

    #[test]
    fn the_checkout_width_covers_what_edits_would_not() {
        let (p, t) = setup(PermissionMode::Ask);
        let root = t.cwd.clone();
        let Verdict::Ask(req) = p.check(&t, "write", &json!({"path": "src/a.rs", "content": "x"})) else { panic!() };
        let grant = req.grant.unwrap();
        let checkout = grant.wider.iter().find(|w| w.label == "this checkout").unwrap().patterns.clone();
        assert_eq!(checkout, [format!("{}/*", root.display())]);
        p.remember(&grant.picked(Some(checkout)), ApprovalScope::Session, true).unwrap();
        assert!(matches!(p.check(&t, "write", &json!({"path": "c.txt", "content": "x"})), Verdict::Allow));
        assert!(
            matches!(p.check(&t, "write", &json!({"path": ".git/config", "content": "x"})), Verdict::Allow),
            "a grant is explicit approval, so it can cover what the mode alone would not"
        );
    }

    #[test]
    fn a_wider_answer_covers_neighbouring_files() {
        let (p, t) = setup(PermissionMode::Ask);
        let Verdict::Ask(req) = p.check(&t, "read", &json!({"path": "/etc/passwd"})) else { panic!() };
        let g = req.grant.unwrap().picked(Some(vec!["/etc/*".into()]));
        p.remember(&g, ApprovalScope::Session, false).unwrap();
        assert!(matches!(p.check(&t, "read", &json!({"path": "/etc/hosts"})), Verdict::Allow));
        assert!(matches!(p.check(&t, "read", &json!({"path": "/root/x"})), Verdict::Ask(_)));

        let Verdict::Ask(req) = verdict(&p, &t, "cat /etc/passwd") else { panic!() };
        let g = req.grant.unwrap().picked(Some(vec!["cat /etc/*".into()]));
        p.remember(&g, ApprovalScope::Session, false).unwrap();
        assert!(matches!(verdict(&p, &t, "cat /etc/hosts"), Verdict::Allow));
        assert!(matches!(verdict(&p, &t, "cat /root/x"), Verdict::Ask(_)));
    }

    #[test]
    fn an_outside_bash_read_is_granted_as_the_line_it_is() {
        let (p, t) = setup(PermissionMode::Ask);
        let Verdict::Ask(req) = verdict(&p, &t, "cat /etc/passwd") else { panic!() };
        assert_eq!(req.grant.as_ref().unwrap().patterns, ["cat /etc/passwd"], "not `cat *`, which would read anywhere");
        p.remember(&req.grant.unwrap(), ApprovalScope::Session, false).unwrap();
        assert!(matches!(verdict(&p, &t, "cat /etc/passwd"), Verdict::Allow));
        assert!(matches!(verdict(&p, &t, "cat /etc/hosts"), Verdict::Ask(_)));
    }

    #[test]
    fn merely_waiting_needs_no_execution_approval() {
        let (p, t) = setup(PermissionMode::Ask);
        assert!(matches!(p.check(&t, "wait", &json!({})), Verdict::Allow), "wait is read tier");
        assert!(matches!(p.check(&t, "bash", &json!({"command": "cargo build"})), Verdict::Ask(_)));
    }

    #[test]
    fn deny_beats_allow_and_ask_beats_allow() {
        let (p, t) = setup(PermissionMode::Yolo);
        p.session.borrow_mut().0.extend([allow("*"), deny_rule("rm *")]);
        assert!(matches!(verdict(&p, &t, "ls"), Verdict::Allow));
        match verdict(&p, &t, "rm x") {
            Verdict::Deny(msg) => assert!(msg.contains("deny") && msg.contains("this session"), "{msg}"),
            _ => panic!("a deny rule must win"),
        }

        let (p, t) = setup(PermissionMode::Yolo);
        p.session.borrow_mut().0.extend([allow("*"), ask_rule("git push *")]);
        assert!(matches!(verdict(&p, &t, "git push"), Verdict::Ask(_)));
        assert!(matches!(verdict(&p, &t, "git status"), Verdict::Allow));
    }

    #[test]
    fn yolo_still_honours_ask_rules() {
        let (p, t) = setup(PermissionMode::Yolo);
        p.session.borrow_mut().0.push(ask_rule("touch *"));
        match verdict(&p, &t, "touch x") {
            Verdict::Ask(req) => assert_eq!(req.reason, "rule \"touch *\" = ask (this session)"),
            _ => panic!("yolo must still ask for an ask rule"),
        }
    }

    #[test]
    fn a_compound_command_needs_every_part_covered() {
        let (p, t) = setup(PermissionMode::Ask);
        p.session.borrow_mut().0.push(allow("ls *"));
        assert!(matches!(verdict(&p, &t, "ls -la"), Verdict::Allow), "read-only commands need no rule");
        assert!(matches!(verdict(&p, &t, "ls -la && ls -l"), Verdict::Allow));
        assert!(matches!(verdict(&p, &t, "touch a && ls"), Verdict::Ask(_)), "touch is not covered");
        assert!(matches!(verdict(&p, &t, "FOO=1 ls"), Verdict::Ask(_)), "assignments are never allow-covered");
    }

    #[test]
    fn grants_use_the_program_and_subcommand() {
        let (p, t) = setup(PermissionMode::Ask);
        let Verdict::Ask(req) = verdict(&p, &t, "cargo test --release") else { panic!() };
        assert_eq!(req.grant.as_ref().unwrap().patterns, ["cargo test *"]);
        let Verdict::Ask(req) = verdict(&p, &t, "./run.sh x") else { panic!() };
        assert_eq!(req.grant.as_ref().unwrap().patterns, ["./run.sh *"]);
        let Verdict::Ask(req) = verdict(&p, &t, "$PROG x") else { panic!() };
        assert!(req.grant.is_none(), "a non-literal program word cannot be granted");
    }

    #[test]
    fn session_grants_cover_later_calls() {
        let (p, t) = setup(PermissionMode::Ask);
        let Verdict::Ask(req) = verdict(&p, &t, "cargo test") else { panic!() };
        p.remember(&req.grant.unwrap(), ApprovalScope::Session, false).unwrap();
        assert!(matches!(verdict(&p, &t, "cargo test --release"), Verdict::Allow));
        assert!(matches!(verdict(&p, &t, "cargo build"), Verdict::Ask(_)));
    }
}
