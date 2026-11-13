//! The tool permission gate: tiers, rules, grants and the approval prompt.

mod bash;
mod classify;
mod glob;
mod grant;
mod guard;
mod store;
mod verdict;

pub use bash::{BashAnalysis, SimpleCmd, analyze};
pub use glob::glob_match;
pub use store::{GrantOutcome, add_global, add_project, load_project, project_file};
pub use verdict::Gate;

use crate::tools::Tools;
use crate::tools::summary;
use crate::tools::uri::physical;
use mu_base::config::MuPermissions;
use mu_base::events::{AgentEvent, EventTx};
use mu_base::permissions::{
    ApprovalDecision, ApprovalRequest, ApprovalScope, Capability, Grant, PermissionMode, RuleSet,
};
use mu_common::paths;
use serde_json::{Value, json};
use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use tokio::sync::oneshot;
use verdict::Outcome;

/// How the gate judges a tool, declared with the tool.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    /// It only reads.
    Read,
    /// It may run commands or change things.
    Exec,
    /// Judged from its arguments by `classify`: the file and shell tools.
    ByArgs,
}

/// What the gate decided for one call.
pub enum Verdict {
    Allow,
    Deny(String),
    Ask(ApprovalRequest),
}

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
        let root = physical(&root);
        let key = mu_base::git::main_checkout(cwd).await;
        let rules = store::load_project(&key, warn);
        let _ = self.project.set(Project { root, key, rules: RefCell::new(rules) });
    }

    /// A `Grant` is added to a rule set and, when `persist`, written to disk first.
    pub fn remember(&self, g: &Grant, scope: ApprovalScope, persist: bool) -> Result<GrantOutcome, String> {
        if scope == ApprovalScope::Once {
            return Ok(GrantOutcome::Written);
        }
        let rules = match scope {
            ApprovalScope::Session => &self.session,
            ApprovalScope::Project => match self.project.get() {
                Some(p) => &p.rules,
                None => return Ok(GrantOutcome::Written),
            },
            ApprovalScope::Global => &self.global,
            ApprovalScope::Once => return Ok(GrantOutcome::Written),
        };
        let grant_rules = grant::grant_rules(g);
        rules.borrow_mut().0.extend(grant_rules);
        match scope {
            ApprovalScope::Project if persist => store::add_project(&self.project.get().unwrap().key, g),
            ApprovalScope::Global if persist => store::add_global(g),
            _ => Ok(GrantOutcome::Written),
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

    /// What the rules and the mode decide, as of now, for a project at `cwd`.
    pub fn gate(&self, cwd: &Path) -> Gate {
        let root = match self.project.get() {
            Some(p) => p.root.clone(),
            None => physical(cwd),
        };
        let project = self.project.get().map(|p| p.rules.borrow().clone()).unwrap_or_default();
        Gate {
            mode: self.mode.get(),
            readonly_bash: self.readonly_bash,
            root,
            mu_home: physical(&paths::mu_home()),
            rules: [
                (self.global.borrow().clone(), "~/.mu/config.toml"),
                (project, "project permissions"),
                (self.session.borrow().clone(), "this session"),
            ],
        }
    }

    /// Decides one model tool call.
    pub fn check(&self, tools: &Tools, name: &str, args: &Value) -> Verdict {
        let gate = self.gate(&tools.cwd);
        let call = match classify::classify(tools, &gate, name, args) {
            Ok(c) => c,
            Err(_) => return Verdict::Allow,
        };
        if let Some(a) = &call.bash
            && guard::removes_root_or_home(a, grant::home_dir().as_deref().and_then(Path::to_str))
        {
            return Verdict::Deny(guard::REFUSAL.into());
        }
        let (outcome, hits) = gate.decide(&call.subjects);
        let mode = gate.mode.as_str();
        let (rule, open) = match outcome {
            Outcome::Allow => return Verdict::Allow,
            Outcome::Deny(h) => {
                return Verdict::Deny(format!(
                    "Not run: permission rule \"{}\" = \"deny\" for {} ({}).",
                    h.rule.pattern, call.subjects[h.subject].tool, h.source
                ));
            }
            Outcome::Ask { rule, open } => (rule, open),
        };
        let reason = match rule {
            Some(h) => format!("rule \"{}\" = ask ({})", h.rule.pattern, h.source),
            None if open.iter().all(|&i| call.subjects[i].needs == Capability::ReadOutside) => {
                format!("{mode} mode: reading outside the project needs approval")
            }
            None if open.iter().all(|&i| call.subjects[i].tool != name) => {
                format!("{mode} mode: writing {} needs approval", call.subjects[open[0]].text)
            }
            None => format!("{mode} mode: {name} needs approval"),
        };
        let grant = grant::grant(&gate, &call, name, args, &hits);
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
    use mu_base::permissions::{Action, Rule};
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

    /// A directory outside the checkout, held at its resolved path. macOS resolves `/etc` and
    /// `/tmp` through symlinks and subjects are matched as resolved, so the platform's own
    /// directories cannot serve as fixtures.
    fn outside(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mu-outside-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
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
        let outside = outside("reads-outside");
        let file = outside.join("passwd").to_string_lossy().into_owned();
        let dir = outside.to_string_lossy().into_owned();
        for mode in [PermissionMode::Ask, PermissionMode::Edits] {
            let (p, t) = setup(mode);
            assert!(matches!(p.check(&t, "read", &json!({"path": &file})), Verdict::Ask(_)), "{mode:?}");
            assert!(matches!(p.check(&t, "grep", &json!({"pattern": "x", "path": &dir})), Verdict::Ask(_)));
            assert!(matches!(p.check(&t, "read", &json!({"path": "src/lib.rs"})), Verdict::Allow));
            assert!(matches!(p.check(&t, "glob", &json!({"pattern": "*"})), Verdict::Allow));
            assert!(
                matches!(p.check(&t, "read", &json!({"path": "local://p.md"})), Verdict::Allow),
                "mu's own pages stay in"
            );
            let Verdict::Ask(req) = p.check(&t, "read", &json!({"path": &file})) else { panic!() };
            assert_eq!(req.reason, format!("{} mode: reading outside the project needs approval", mode.as_str()));
        }
        let (p, t) = setup(PermissionMode::Yolo);
        assert!(matches!(p.check(&t, "read", &json!({"path": &file})), Verdict::Allow));
        std::fs::remove_dir_all(&outside).ok();
    }

    #[test]
    fn an_outside_read_is_granted_by_its_path() {
        let outside = outside("grant-by-path");
        let (passwd, hosts) = (outside.join("passwd"), outside.join("hosts"));
        let (p, t) = setup(PermissionMode::Ask);
        let Verdict::Ask(req) = p.check(&t, "read", &json!({"path": passwd.to_string_lossy()})) else { panic!() };
        assert_eq!(req.grant.as_ref().unwrap().patterns, [passwd.to_string_lossy().into_owned()]);
        p.remember(&req.grant.unwrap(), ApprovalScope::Session, false).unwrap();
        assert!(matches!(p.check(&t, "read", &json!({"path": passwd.to_string_lossy()})), Verdict::Allow));
        assert!(matches!(p.check(&t, "read", &json!({"path": hosts.to_string_lossy()})), Verdict::Ask(_)));
        std::fs::remove_dir_all(&outside).ok();
    }

    #[test]
    fn an_outside_read_through_a_symlink_is_granted_as_its_target() {
        let outside = outside("symlink");
        let (real, link) = (outside.join("real"), outside.join("link"));
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join("a.txt"), "x").unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let (p, t) = setup(PermissionMode::Ask);
        let through = link.join("a.txt");
        let target = real.join("a.txt");
        let Verdict::Ask(req) = p.check(&t, "read", &json!({"path": through.to_string_lossy()})) else { panic!() };
        let g = req.grant.unwrap();
        assert_eq!(g.patterns, [target.to_string_lossy().into_owned()], "the link resolves to its target");
        p.remember(&g, ApprovalScope::Session, false).unwrap();
        assert!(
            matches!(p.check(&t, "read", &json!({"path": through.to_string_lossy()})), Verdict::Allow),
            "the same file through the link"
        );
        assert!(
            matches!(p.check(&t, "read", &json!({"path": real.join("b.txt").to_string_lossy()})), Verdict::Ask(_)),
            "another file in the target directory"
        );
        std::fs::remove_dir_all(&outside).ok();
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
        let outside = outside("landmarks");
        let dir = outside.to_string_lossy().into_owned();
        let passwd = outside.join("passwd");
        let (p, t) = setup(PermissionMode::Ask);
        let Verdict::Ask(req) = p.check(&t, "read", &json!({"path": passwd.to_string_lossy()})) else { panic!() };
        let g = req.grant.unwrap();
        assert_eq!(g.patterns, [passwd.to_string_lossy().into_owned()]);
        let labels: Vec<&str> = g.wider.iter().map(|w| w.label.as_str()).collect();
        assert_eq!(labels, ["its directory", "everything"], "no checkout and no home above this path");
        assert_eq!(g.wider[0].patterns, [format!("{dir}/*")]);
        assert_eq!(g.wider[1].patterns, ["/*"]);

        let Verdict::Ask(req) = p.check(&t, "grep", &json!({"pattern": "x", "path": &dir})) else { panic!() };
        assert_eq!(req.grant.unwrap().wider[0].patterns, [format!("{dir}/*")], "a directory is its own container");

        let Verdict::Ask(req) = verdict(&p, &t, &format!("cat {dir}/passwd")) else { panic!() };
        let g = req.grant.unwrap();
        assert_eq!(g.patterns, [format!("cat {dir}/passwd")]);
        assert_eq!(g.wider[0].patterns, [format!("cat {dir}/*")]);
        assert_eq!(g.wider[1].patterns, ["cat /*"]);
        std::fs::remove_dir_all(&outside).ok();
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
        let edit = |path: &str| json!({"input": format!("[{path}#1A2B]\nPUT 1:\n+x")});
        assert!(matches!(p.check(&t, "edit", &edit("a/b.txt")), Verdict::Allow), "editing the file the grant names");
        assert!(matches!(p.check(&t, "edit", &edit("a/c.txt")), Verdict::Ask(_)), "another file");
    }

    #[test]
    fn dot_dot_after_a_missing_directory_does_not_leave_the_project() {
        let (p, t) = setup(PermissionMode::Edits);
        let root = t.cwd.clone();
        for path in [
            "zz_missing/../../outside.txt".to_string(),
            root.join("zz_missing/../.git/config").to_string_lossy().into_owned(),
        ] {
            let v = p.check(&t, "write", &json!({"path": path, "content": "x"}));
            assert!(matches!(v, Verdict::Ask(_)), "{path}");
        }
        let v = p.check(&t, "write", &json!({"path": "zz_missing/../ok.txt", "content": "x"}));
        assert!(matches!(v, Verdict::Allow), "a `..` that stays inside is fine");
    }

    #[test]
    fn an_enveloped_edit_header_is_gated_by_the_file_it_targets() {
        let (p, t) = setup(PermissionMode::Edits);
        let outside = outside("envelope").join("zz");
        let input = format!("[*** Begin Patch] [{}#1A2B]\nPUT 1:\n+x", outside.display());
        assert!(matches!(p.check(&t, "edit", &json!({"input": input})), Verdict::Ask(_)));
        let v = p.check(&t, "edit", &json!({"input": "nothing recognisable"}));
        assert!(matches!(v, Verdict::Ask(_)), "a payload with no target fails closed");
        std::fs::remove_dir_all(outside.parent().unwrap()).ok();
    }

    #[test]
    fn an_edit_is_gated_by_the_file_the_editor_writes() {
        let outside = outside("edit-targets");
        let out = outside.display();
        let (p, t) = setup(PermissionMode::Edits);
        let edit = |input: String| p.check(&t, "edit", &json!({"input": input}));
        for header in [format!("file://{out}/zz#1A2B"), format!("@{out}/zz"), format!(":{out}/zz")] {
            let v = edit(format!("[{header}]\nPUT 1:\n+x"));
            assert!(matches!(v, Verdict::Ask(_)), "{header}");
        }
        let v = edit("[a.txt#1A2B]\nMV ../moved.txt".into());
        assert!(matches!(v, Verdict::Ask(_)), "a move out of the project");
        let v = edit("[a.txt#1A2B]\nMV b.txt".into());
        assert!(matches!(v, Verdict::Allow), "a move inside it");
        let v = edit("[a.txt#1A2B]\nPUT 1:\n+x".into());
        assert!(matches!(v, Verdict::Allow));
        std::fs::remove_dir_all(&outside).ok();
    }

    #[test]
    fn a_redirect_to_a_file_is_not_read_only_and_not_covered_by_a_command_grant() {
        let (p, t) = setup(PermissionMode::Ask);
        for cmd in ["echo x >&out.txt", "echo x >&~/.bashrc", "echo x > out.txt"] {
            assert!(matches!(verdict(&p, &t, cmd), Verdict::Ask(_)), "{cmd}");
        }
        assert!(matches!(verdict(&p, &t, "ls 2>&1"), Verdict::Allow), "a descriptor duplication writes nothing");

        let Verdict::Ask(req) = verdict(&p, &t, "cargo test") else { panic!() };
        p.remember(&req.grant.unwrap(), ApprovalScope::Session, false).unwrap();
        assert!(matches!(verdict(&p, &t, "cargo test -q"), Verdict::Allow));
        let Verdict::Ask(req) = verdict(&p, &t, "cargo test -q > out.log") else {
            panic!("the file is a write of its own, and ask mode asks about writes")
        };
        let g = req.grant.unwrap();
        assert_eq!((g.tool.as_str(), g.patterns.as_slice()), ("write", ["out.log".to_string()].as_slice()));
        assert_eq!(req.reason, "ask mode: writing out.log needs approval");
        p.set_mode(PermissionMode::Edits);
        assert!(matches!(verdict(&p, &t, "cargo test -q > out.log"), Verdict::Allow), "a file in the project");
        p.set_mode(PermissionMode::Ask);
        for cmd in [
            "cargo test -q > ~/.bashrc",
            "cargo test -q > /etc/zz",
            "cargo test -q &> ../x",
            "{ cargo test -q; } > ../x",
            "cargo test -q > $OUT",
        ] {
            assert!(matches!(verdict(&p, &t, cmd), Verdict::Ask(_)), "{cmd}");
        }
    }

    #[test]
    fn a_read_grant_covers_grep_and_glob_of_the_same_directory() {
        let outside = outside("read-grant");
        let dir = outside.to_string_lossy().into_owned();
        let (passwd, hosts) = (outside.join("passwd"), outside.join("hosts"));
        let (p, t) = setup(PermissionMode::Ask);
        let Verdict::Ask(req) = p.check(&t, "read", &json!({"path": passwd.to_string_lossy()})) else { panic!() };
        let g = req.grant.unwrap().picked(Some(vec![format!("{dir}/*")]));
        p.remember(&g, ApprovalScope::Session, false).unwrap();
        assert!(matches!(p.check(&t, "read", &json!({"path": hosts.to_string_lossy()})), Verdict::Allow));
        assert!(matches!(
            p.check(&t, "grep", &json!({"pattern": "x", "path": hosts.to_string_lossy()})),
            Verdict::Allow
        ));
        assert!(
            matches!(p.check(&t, "grep", &json!({"pattern": "x", "path": &dir})), Verdict::Allow),
            "the directory itself"
        );
        assert!(matches!(p.check(&t, "glob", &json!({"pattern": "*", "path": &dir})), Verdict::Allow));
        assert!(matches!(p.check(&t, "read", &json!({"path": "/root/x"})), Verdict::Ask(_)));
        assert!(matches!(verdict(&p, &t, &format!("cat {dir}/hosts")), Verdict::Ask(_)), "a command is its own grant");
        std::fs::remove_dir_all(&outside).ok();
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
        let outside = outside("wider-answer");
        let dir = outside.to_string_lossy().into_owned();
        let (passwd, hosts) = (outside.join("passwd"), outside.join("hosts"));
        let (p, t) = setup(PermissionMode::Ask);
        let Verdict::Ask(req) = p.check(&t, "read", &json!({"path": passwd.to_string_lossy()})) else { panic!() };
        let g = req.grant.unwrap().picked(Some(vec![format!("{dir}/*")]));
        p.remember(&g, ApprovalScope::Session, false).unwrap();
        assert!(matches!(p.check(&t, "read", &json!({"path": hosts.to_string_lossy()})), Verdict::Allow));
        assert!(matches!(p.check(&t, "read", &json!({"path": "/root/x"})), Verdict::Ask(_)));

        let Verdict::Ask(req) = verdict(&p, &t, &format!("cat {dir}/passwd")) else { panic!() };
        let g = req.grant.unwrap().picked(Some(vec![format!("cat {dir}/*")]));
        p.remember(&g, ApprovalScope::Session, false).unwrap();
        assert!(matches!(verdict(&p, &t, &format!("cat {dir}/hosts")), Verdict::Allow));
        assert!(matches!(verdict(&p, &t, "cat /root/x"), Verdict::Ask(_)));
        std::fs::remove_dir_all(&outside).ok();
    }

    #[test]
    fn an_outside_bash_read_is_granted_as_the_line_it_is() {
        let outside = outside("bash-line");
        let dir = outside.to_string_lossy().into_owned();
        let (p, t) = setup(PermissionMode::Ask);
        let Verdict::Ask(req) = verdict(&p, &t, &format!("cat {dir}/passwd")) else { panic!() };
        assert_eq!(
            req.grant.as_ref().unwrap().patterns,
            [format!("cat {dir}/passwd")],
            "not `cat *`, which would read anywhere"
        );
        p.remember(&req.grant.unwrap(), ApprovalScope::Session, false).unwrap();
        assert!(matches!(verdict(&p, &t, &format!("cat {dir}/passwd")), Verdict::Allow));
        assert!(matches!(verdict(&p, &t, &format!("cat {dir}/hosts")), Verdict::Ask(_)));
        std::fs::remove_dir_all(&outside).ok();
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
    fn removing_root_or_home_is_refused_whatever_the_rules() {
        let (p, t) = setup(PermissionMode::Yolo);
        p.session.borrow_mut().0.push(allow("*"));
        assert!(matches!(verdict(&p, &t, "rm -rf ~"), Verdict::Deny(m) if m == guard::REFUSAL));
        assert!(matches!(verdict(&p, &t, "rm -rf target"), Verdict::Allow));
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
    fn a_write_goes_where_the_gate_judged_it() {
        let outside = outside("dangling");
        let (p, t) = setup(PermissionMode::Edits);
        std::os::unix::fs::symlink(outside.join("missing/file"), t.cwd.join("dangling")).unwrap();
        let v = p.check(&t, "write", &json!({"path": "dangling", "content": "x"}));
        assert!(matches!(v, Verdict::Ask(_)), "a dangling link out of the project is outside");
        std::fs::remove_dir_all(&outside).ok();
    }

    #[test]
    fn a_read_is_judged_by_the_file_it_opens() {
        let outside = outside("read-selector");
        std::fs::write(outside.join("secret"), "x").unwrap();
        let (p, t) = setup(PermissionMode::Ask);
        std::os::unix::fs::symlink(outside.join("secret"), t.cwd.join("link")).unwrap();
        let v = p.check(&t, "read", &json!({"path": "link:1"}));
        assert!(matches!(v, Verdict::Ask(_)), "the selector is not part of the file name");
        let v = p.check(&t, "read", &json!({"path": "src/lib.rs:1-5"}));
        assert!(matches!(v, Verdict::Allow));
        std::fs::remove_dir_all(&outside).ok();
    }
    #[test]
    fn a_bash_call_is_judged_from_where_it_runs() {
        let outside = outside("bash-cwd");
        let dir = outside.to_string_lossy().into_owned();
        let (p, t) = setup(PermissionMode::Ask);
        let call = |args: Value| p.check(&t, "bash", &args);
        assert!(matches!(call(json!({"command": "cat passwd", "cwd": &dir})), Verdict::Ask(_)));
        assert!(matches!(call(json!({"command": "cat passwd"})), Verdict::Allow));
        let Verdict::Ask(req) = call(json!({"command": "ls", "pty": true})) else { panic!() };
        assert!(req.grant.is_none(), "the login shell's grammar is not the classifier's");
        assert!(matches!(verdict(&p, &t, "cd .. && cat ../x"), Verdict::Ask(_)));
        assert!(matches!(verdict(&p, &t, "cd src && ls"), Verdict::Allow));
        assert!(matches!(verdict(&p, &t, "cd src && cat lib.rs"), Verdict::Ask(_)), "after a cd, nothing is placed");
        assert!(matches!(verdict(&p, &t, "cd src && cat /nowhere"), Verdict::Ask(_)));
        assert!(matches!(verdict(&p, &t, "echo x > out.txt"), Verdict::Ask(_)));
        std::fs::remove_dir_all(&outside).ok();
    }

    #[test]
    fn an_output_redirect_is_a_write_the_write_rules_see() {
        let outside = outside("redirect");
        let (p, t) = setup(PermissionMode::Edits);
        assert!(matches!(verdict(&p, &t, "echo x > out.txt"), Verdict::Ask(_)), "the command itself");
        let g = Grant { tool: "bash".into(), patterns: vec!["echo *".into()], wider: vec![] };
        p.remember(&g, ApprovalScope::Session, false).unwrap();
        assert!(matches!(verdict(&p, &t, "echo x > out.txt"), Verdict::Allow), "edits mode runs the write");
        assert!(matches!(verdict(&p, &t, &format!("echo x > {}/log", outside.display())), Verdict::Ask(_)));

        let (p, t) = setup(PermissionMode::Yolo);
        p.session.borrow_mut().0.push(("write".into(), Rule { pattern: "*.env".into(), action: Action::Deny }));
        match verdict(&p, &t, "echo x > a.env") {
            Verdict::Deny(msg) => assert!(msg.contains("for write"), "{msg}"),
            _ => panic!("a write rule must see a redirect"),
        }
        assert!(matches!(verdict(&p, &t, "echo x > a.txt"), Verdict::Allow));
        std::fs::remove_dir_all(&outside).ok();
    }

    #[test]
    fn deny_and_ask_rules_see_through_wrappers_and_shell_strings() {
        let (p, t) = setup(PermissionMode::Yolo);
        p.session.borrow_mut().0.push(deny_rule("git push *"));
        for cmd in [
            "git push origin",
            "\"git\" push origin",
            "/usr/bin/git push origin",
            "env git push origin",
            "sudo -u x git push origin",
            "sh -c 'git push origin'",
            "eval git push origin",
            "timeout 5 bash -c \"git push origin\"",
        ] {
            assert!(matches!(verdict(&p, &t, cmd), Verdict::Deny(_)), "{cmd}");
        }
        for cmd in ["git status", "echo git push origin", "git log --grep 'git push origin'"] {
            assert!(matches!(verdict(&p, &t, cmd), Verdict::Allow), "{cmd}");
        }

        // An allow rule matches the line as written, never the form under a wrapper.
        let (p, t) = setup(PermissionMode::Ask);
        p.session.borrow_mut().0.push(allow("cargo test *"));
        assert!(matches!(verdict(&p, &t, "cargo test -q"), Verdict::Allow));
        assert!(matches!(verdict(&p, &t, "env cargo test -q"), Verdict::Ask(_)));
        assert!(matches!(verdict(&p, &t, "/usr/bin/cargo test -q"), Verdict::Ask(_)));
    }

    /// A `Tools` whose plugin host serves the tools `names`, each declared with `access`.
    fn plugin_tools(names: &[&str], access: Option<Access>, cwd: PathBuf) -> Tools {
        use crate::plugin::{host::PluginHost, manifest};
        let tools = names
            .iter()
            .map(|n| {
                let spec = manifest::ToolSpec::with_schema(n, json!({}));
                match access {
                    Some(a) => spec.with_access(a),
                    None => spec,
                }
            })
            .collect();
        let m = manifest::Manifest {
            name: "probe".into(),
            command: vec!["true".into()],
            lifetime: manifest::Lifetime::parse("session").unwrap(),
            hooks: vec![],
            provider: None,
            tools,
            timeout: std::time::Duration::from_secs(5),
            dir: cwd.clone(),
        };
        let (ntx, _nrx) = tokio::sync::mpsc::unbounded_channel();
        Tools::new(cwd, PluginHost::new(vec![m], vec![], ntx, Default::default()))
    }

    #[test]
    fn a_plugin_tool_declares_whether_it_only_reads() {
        let (p, t) = setup(PermissionMode::Ask);
        let reader = plugin_tools(&["probe"], Some(Access::Read), t.cwd.clone());
        assert!(matches!(p.check(&reader, "probe", &json!({})), Verdict::Allow));
        let undeclared = plugin_tools(&["probe"], None, t.cwd.clone());
        let Verdict::Ask(req) = p.check(&undeclared, "probe", &json!({})) else { panic!() };
        assert_eq!(req.grant.unwrap().patterns, ["*"], "the one row: every call of the tool");
        let Verdict::Ask(_) = p.check(&t, "nosuchtool", &json!({})) else { panic!("an unknown tool may do anything") };
    }

    #[test]
    fn a_grant_for_every_call_of_a_tool_is_remembered_as_a_rule() {
        let (p, t) = setup(PermissionMode::Ask);
        let undeclared = plugin_tools(&["probe"], None, t.cwd.clone());
        let Verdict::Ask(req) = p.check(&undeclared, "probe", &json!({})) else { panic!() };
        p.remember(&req.grant.unwrap(), ApprovalScope::Session, false).unwrap();
        assert!(matches!(p.check(&undeclared, "probe", &json!({})), Verdict::Allow));
    }

    #[test]
    fn a_fetch_with_no_host_is_asked_about_without_a_grant() {
        let (p, t) = setup(PermissionMode::Ask);
        let web = plugin_tools(&["fetch"], Some(Access::Read), t.cwd.clone());
        p.session.borrow_mut().0.push(("fetch".into(), Rule { pattern: "*".into(), action: Action::Ask }));
        let Verdict::Ask(req) = p.check(&web, "fetch", &json!({"url": "example.com"})) else { panic!() };
        assert!(req.grant.is_none());
        let Verdict::Ask(req) = p.check(&web, "fetch", &json!({"url": "https://example.com/a"})) else { panic!() };
        assert_eq!(req.grant.unwrap().patterns, ["https://example.com/*"]);
    }

    #[test]
    fn a_mixed_line_is_granted_part_by_part() {
        let outside = outside("mixed-line");
        let dir = outside.to_string_lossy().into_owned();
        let (p, t) = setup(PermissionMode::Ask);
        let Verdict::Ask(req) = verdict(&p, &t, &format!("cat {dir}/x && touch y")) else { panic!() };
        assert_eq!(req.grant.unwrap().patterns, [format!("cat {dir}/x"), "touch *".to_string()]);
        let Verdict::Ask(req) = verdict(&p, &t, "FOO=1 cargo test") else { panic!() };
        assert!(req.grant.is_none(), "an assignment is never covered by a grant");
        std::fs::remove_dir_all(&outside).ok();
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
