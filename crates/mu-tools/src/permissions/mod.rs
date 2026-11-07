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
    Action, ApprovalDecision, ApprovalRequest, ApprovalScope, Grant, PermissionMode, Rule, RuleSet,
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
            if let ApprovalDecision::Allow { scope, .. } = &decision
                && let Some(g) = &grant
            {
                // The originating subagent persists it; the parent only updates its memory.
                let _ = this.remember(g, *scope, false);
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
                s.classified
                    || (!s.no_allow
                        && hits.iter().any(|(r, _)| r.action == Action::Allow && glob_match(&r.pattern, &s.text)))
            });
        if ask.is_none() && covered {
            return Verdict::Allow;
        }
        let ceiling = match self.mode.get() {
            PermissionMode::Ask => Tier::Read,
            PermissionMode::Edits => Tier::Write,
            PermissionMode::Yolo => Tier::Exec,
        };
        if ask.is_none() && call.tier <= ceiling {
            return Verdict::Allow;
        }
        let reason = match ask {
            Some((rule, source)) => format!("rule \"{}\" = ask ({source})", rule.pattern),
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
                if glob_match(tool, name) && subjects.iter().any(|s| glob_match(&rule.pattern, &s.text)) {
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
        let mut call = Call { subjects: Vec::new(), tier: Tier::Read, bash: None };
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
            call.subjects = analysis
                .cmds
                .iter()
                .map(|c| Subject {
                    text: c.text.clone(),
                    classified: c.readonly && self.readonly_bash,
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
            call.subjects.push(Subject { text: path_subject(&root, tools, raw), classified: false, no_allow: false });
            call.tier = Tier::Read;
            return Ok(call);
        }
        if name == "fetch" {
            call.subjects.push(Subject {
                text: args["url"].as_str().unwrap_or("").to_string(),
                classified: false,
                no_allow: false,
            });
        } else {
            call.subjects.push(Subject { text: String::new(), classified: false, no_allow: false });
        }
        call.tier = if READ_TOOLS.contains(&name) { Tier::Read } else { Tier::Exec };
        Ok(call)
    }
}

struct Call {
    subjects: Vec<Subject>,
    tier: Tier,
    bash: Option<BashAnalysis>,
}

struct Subject {
    text: String,
    /// The read-only classifier covers it (bash only, with `readonly_bash` on).
    classified: bool,
    /// An `allow` rule can never cover it (leading assignments, or a line that does not parse).
    no_allow: bool,
}

impl Call {
    /// What "don't ask again" would remember.
    fn grant(&self, tools: &Tools, name: &str, args: &Value, hits: &[(Rule, &'static str)]) -> Option<Grant> {
        let allow = |text: &str| hits.iter().any(|(r, _)| r.action == Action::Allow && glob_match(&r.pattern, text));
        match name {
            "bash" => {
                let analysis = self.bash.as_ref()?;
                if !analysis.parsed {
                    return None;
                }
                let mut patterns: Vec<String> = Vec::new();
                for (i, c) in analysis.cmds.iter().enumerate() {
                    if self.subjects.get(i).is_some_and(|s| s.classified) || allow(&c.text) {
                        continue;
                    }
                    let p = bash_pattern(c)?;
                    if !patterns.contains(&p) {
                        patterns.push(p);
                    }
                }
                if patterns.is_empty() {
                    return None;
                }
                Some(Grant { tool: name.to_string(), patterns })
            }
            "write" | "edit" => {
                let patterns: Vec<String> =
                    self.subjects.iter().filter(|s| !s.classified && !allow(&s.text)).map(|s| s.text.clone()).collect();
                (!patterns.is_empty()).then_some(Grant { tool: name.to_string(), patterns })
            }
            "fetch" => {
                Some(Grant { tool: name.to_string(), patterns: fetch_patterns(args["url"].as_str().unwrap_or("")) })
            }
            _ => {
                let _ = tools;
                Some(Grant { tool: name.to_string(), patterns: Vec::new() })
            }
        }
    }
}

fn grant_rules(g: &Grant) -> Vec<(String, Rule)> {
    let patterns = if g.patterns.is_empty() { vec!["*".to_string()] } else { g.patterns.clone() };
    patterns.into_iter().map(|p| (g.tool.clone(), Rule { pattern: p, action: Action::Allow })).collect()
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

/// `.git` of the project and mu's own home are never edited by an `edits`-mode call.
fn protected(root: &Path, c: &Path) -> bool {
    if c.starts_with(root.join(".git")) {
        return true;
    }
    c.starts_with(canonical_nearest(&paths::mu_home()))
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
