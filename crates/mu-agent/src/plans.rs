//! Durable plans: a plan written in plan mode can be parked and approved later, in another session.
//! Each plan is `<id>.md` (the text as written) plus `<id>.json` (metadata) under
//! `$MU_HOME/plans/<repo-key>/`. All worktrees of one clone share a store.
use crate::review::facts::{numstat, repo_root, snapshot_tree};
use mu_base::events::{Approval, Base, PlanMeta, Status};
use mu_base::git::{git, short_sha};
use mu_base::message::{Message, tool_calls};
use mu_common::fs::write_atomic;
use mu_common::paths::{encode_cwd, mu_home};
use std::path::{Path, PathBuf};

pub struct Store {
    pub dir: PathBuf,
    /// The current worktree's top level; `None` outside git.
    pub root: Option<PathBuf>,
}

pub struct Save<'a> {
    pub slug: &'a str,
    pub title: &'a str,
    pub content: &'a str,
    pub read: Vec<String>,
    pub session: Option<&'a Path>,
    pub base: Option<Base>,
    pub adopt: Option<&'a str>,
}

fn hash_of(text: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(text.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

/// The short commit (or tree) the plan was written against, plus a note when the snapshot had uncommitted changes.
pub async fn base_label(meta: &PlanMeta, cwd: &Path) -> String {
    let Some(b) = &meta.base else { return "unknown".to_string() };
    let mut s = short_sha(b.commit.as_deref().unwrap_or(&b.tree)).to_string();
    if dirty(cwd, b).await {
        s.push_str(DIRTY);
    }
    s
}

/// The main checkout of the clone `cwd` is in, or `cwd` outside git.
pub async fn main_checkout(cwd: &Path) -> PathBuf {
    let root = repo_root(cwd).await.ok();
    let common = git(cwd, &["rev-parse", "--path-format=absolute", "--git-common-dir"], None).await;
    let common = match common {
        Ok(s) => Some(PathBuf::from(s.trim())),
        // Git older than 2.31: the common dir may come back relative.
        Err(_) => git(cwd, &["rev-parse", "--git-common-dir"], None).await.ok().map(|s| cwd.join(s.trim())),
    };
    match common {
        Some(c) if root.is_some() => {
            if c.file_name().is_some_and(|n| n == ".git") {
                c.parent().map(Path::to_path_buf).unwrap_or(c)
            } else {
                c
            }
        }
        _ => cwd.to_path_buf(),
    }
}

pub async fn store_for(cwd: &Path) -> Store {
    Store { dir: mu_home().join("plans").join(encode_cwd(&main_checkout(cwd).await)), root: repo_root(cwd).await.ok() }
}

pub async fn base_now(root: &Path) -> Option<Base> {
    let tree = snapshot_tree(root).await.ok()?;
    let commit = git(root, &["rev-parse", "HEAD"], None).await.ok().map(|s| s.trim().to_string());
    Some(Base { tree, commit: commit.filter(|c| !c.is_empty()) })
}

impl Store {
    fn json_path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }

    pub fn md_path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.md"))
    }

    fn read_meta(&self, id: &str) -> Result<PlanMeta, String> {
        let p = self.json_path(id);
        if !p.exists() {
            return Err(format!("plan '{id}' not found in {}", self.dir.display()));
        }
        let raw = std::fs::read_to_string(&p).map_err(|e| format!("cannot read {}: {e}", p.display()))?;
        serde_json::from_str(&raw).map_err(|e| format!("cannot parse {}: {e}", p.display()))
    }

    fn write_meta(&self, m: &PlanMeta) -> Result<(), String> {
        let p = self.json_path(&m.id);
        let bytes = serde_json::to_vec_pretty(m).map_err(|e| format!("cannot encode {}: {e}", p.display()))?;
        write_atomic(&p, &bytes).map_err(|e| format!("cannot write {}: {e}", p.display()))
    }

    pub fn list(&self, all: bool) -> Vec<PlanMeta> {
        let Ok(rd) = std::fs::read_dir(&self.dir) else { return vec![] };
        let mut out = Vec::new();
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().is_none_or(|x| x != "json") {
                continue;
            }
            let parsed = std::fs::read_to_string(&p)
                .map_err(|e| e.to_string())
                .and_then(|s| serde_json::from_str::<PlanMeta>(&s).map_err(|e| e.to_string()));
            match parsed {
                Ok(m) if all || m.status == Status::Pending => out.push(m),
                Ok(_) => {}
                Err(e) => eprintln!("warning: skipping {}: {e}", p.display()),
            }
        }
        out.sort_by(|a, b| b.updated.cmp(&a.updated));
        out
    }

    pub fn load(&self, id: &str) -> Result<(PlanMeta, String), String> {
        let meta = self.read_meta(id)?;
        let p = self.md_path(id);
        let text = std::fs::read_to_string(&p).map_err(|e| format!("cannot read {}: {e}", p.display()))?;
        Ok((meta, text))
    }

    /// Like `load`, but the plan must still be pending.
    pub fn load_pending(&self, id: &str) -> Result<(PlanMeta, String), String> {
        let (meta, text) = self.load(id)?;
        if meta.status != Status::Pending {
            return Err(format!("plan '{id}' is {}, not pending", meta.status.name()));
        }
        Ok((meta, text))
    }

    pub fn save(&self, s: Save) -> Result<PlanMeta, String> {
        std::fs::create_dir_all(&self.dir).map_err(|e| format!("cannot create {}: {e}", self.dir.display()))?;
        let now = mu_common::time::iso_ms();
        let session = s.session.map(Path::to_path_buf);
        let existing = self.read_meta(s.slug).ok();
        let update = existing.filter(|m| s.adopt == Some(s.slug) || (session.is_some() && m.session == session));
        let meta = match update {
            Some(old) => {
                let mut read = old.read.clone();
                read.extend(s.read);
                read.sort();
                read.dedup();
                PlanMeta {
                    title: s.title.into(),
                    status: Status::Pending,
                    base: s.base,
                    read,
                    session,
                    updated: now,
                    hash: hash_of(s.content),
                    approved: None,
                    ..old
                }
            }
            None => {
                let mut id = s.slug.to_string();
                let mut n = 2;
                while self.json_path(&id).exists() {
                    id = format!("{}-{n}", s.slug);
                    n += 1;
                }
                let mut read = s.read;
                read.sort();
                read.dedup();
                PlanMeta {
                    id,
                    title: s.title.into(),
                    status: Status::Pending,
                    repo: self.root.clone().unwrap_or_default(),
                    base: s.base,
                    read,
                    session,
                    created: now.clone(),
                    updated: now,
                    hash: hash_of(s.content),
                    approved: None,
                }
            }
        };
        let md = self.md_path(&meta.id);
        write_atomic(&md, s.content.as_bytes()).map_err(|e| format!("cannot write {}: {e}", md.display()))?;
        self.write_meta(&meta)?;
        Ok(meta)
    }

    fn require_pending(&self, id: &str) -> Result<PlanMeta, String> {
        let m = self.read_meta(id)?;
        if m.status != Status::Pending {
            return Err(format!("plan '{id}' is {}, not pending", m.status.name()));
        }
        Ok(m)
    }

    pub fn approve(&self, id: &str, decision: &str, session: Option<&Path>) -> Result<(), String> {
        let mut m = self.require_pending(id)?;
        let p = self.md_path(id);
        let text = std::fs::read_to_string(&p).map_err(|e| format!("cannot read {}: {e}", p.display()))?;
        m.status = Status::Approved;
        m.approved = Some(Approval {
            at: mu_common::time::iso_ms(),
            decision: decision.into(),
            session: session.map(Path::to_path_buf),
            hash: hash_of(&text),
        });
        self.write_meta(&m)
    }

    pub fn drop(&self, id: &str) -> Result<(), String> {
        let mut m = self.require_pending(id)?;
        m.status = Status::Dropped;
        self.write_meta(&m)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Drift {
    Unknown(String),
    Clean { base: String, dirty: bool, other: usize },
    Changed { base: String, dirty: bool, files: Vec<String>, other: usize },
}

const DIRTY: &str = " plus uncommitted changes";

/// The snapshot has changes its commit lacks: its tree is not the commit's tree.
async fn dirty(dir: &Path, b: &Base) -> bool {
    let Some(c) = &b.commit else { return false };
    match git(dir, &["rev-parse", "--verify", "--quiet", &format!("{c}^{{tree}}")], None).await {
        Ok(t) => t.trim() != b.tree,
        Err(_) => false, // commit gone: drift reports that separately
    }
}

impl Drift {
    pub fn describe(&self) -> String {
        match self {
            Drift::Unknown(r) => format!("drift unknown: {r}"),
            Drift::Clean { base, dirty, other } => {
                let d = if *dirty { DIRTY } else { "" };
                let mut s = format!("none of the files it read changed since {base}{d}");
                if *other > 0 {
                    s.push_str(&format!(" ({other} other files changed)"));
                }
                s
            }
            Drift::Changed { base, dirty, files, .. } => {
                let d = if *dirty { DIRTY } else { "" };
                let mut s = format!(
                    "{} file(s) it read changed since {base}{d}: {}",
                    files.len(),
                    files.iter().take(5).cloned().collect::<Vec<_>>().join(", ")
                );
                if files.len() > 5 {
                    s.push_str(", …");
                }
                s
            }
        }
    }

    pub fn json(&self) -> serde_json::Value {
        use serde_json::json;
        match self {
            Drift::Unknown(r) => json!({"state":"unknown","reason":r}),
            Drift::Clean { base, dirty, other } => {
                json!({"state":"clean","base":base,"dirty":dirty,"files":[],"other":other})
            }
            Drift::Changed { base, dirty, files, other } => {
                json!({"state":"changed","base":base,"dirty":dirty,"files":files,"other":other})
            }
        }
    }
}

async fn object_exists(root: &Path, sha: &str) -> bool {
    git(root, &["cat-file", "-e", sha], None).await.is_ok()
}

pub async fn drift(meta: &PlanMeta, cwd: &Path) -> Drift {
    let Ok(root) = repo_root(cwd).await else {
        return Drift::Unknown("not a git repository".into());
    };
    let Some(b) = &meta.base else { return Drift::Unknown("no base recorded".into()) };
    let (base, is_dirty) = if object_exists(&root, &b.tree).await {
        (&b.tree, dirty(&root, b).await)
    } else {
        match &b.commit {
            Some(c) if object_exists(&root, c).await => (c, false),
            _ => {
                return Drift::Unknown(format!(
                    "base {} no longer exists",
                    short_sha(b.commit.as_deref().unwrap_or(&b.tree))
                ));
            }
        }
    };
    let label = short_sha(b.commit.as_deref().unwrap_or(&b.tree)).to_string();
    let stats = match snapshot_tree(&root).await {
        Ok(cur) => numstat(&root, base, &cur).await,
        Err(e) => Err(e),
    };
    let stats = match stats {
        Ok(s) => s,
        Err(e) => return Drift::Unknown(e),
    };
    let read = meta.files_read(&root);
    let (files, rest): (Vec<_>, Vec<_>) =
        stats.into_iter().map(|s| s.path).partition(|p| read.is_empty() || read.contains(&p));
    let other = rest.len();
    if files.is_empty() {
        Drift::Clean { base: label, dirty: is_dirty, other }
    } else {
        Drift::Changed { base: label, dirty: is_dirty, files, other }
    }
}

fn normalize(p: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            c => out.push(c.as_os_str()),
        }
    }
    out
}

/// Repo-relative paths of the files the session read, selectors and `:raw` stripped. Directory listings are skipped: they show names, not contents; so are paths that do not exist.
pub fn reads_from(messages: &[Message], cwd: &Path, root: &Path) -> Vec<String> {
    let root = normalize(root);
    let mut out = Vec::new();
    for m in messages {
        for (_, name, args) in tool_calls(m) {
            if name != "read" {
                continue;
            }
            let Some(p) = args.get("path").and_then(|v| v.as_str()) else { continue };
            if p.contains("://") {
                continue;
            }
            let t = mu_tools::tools::uri::read_target(p, |q| cwd.join(q).exists());
            let abs = normalize(&cwd.join(&t.path));
            if !abs.is_file() {
                continue;
            }
            if let Ok(rel) = abs.strip_prefix(&root) {
                out.push(rel.to_string_lossy().into_owned());
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use mu_testutil::{repo, rt, sh, tmp};
    use serde_json::json;

    fn store_in(dir: PathBuf) -> Store {
        Store { dir, root: None }
    }

    fn save<'a>(
        s: &Store,
        slug: &'a str,
        session: Option<&'a Path>,
        adopt: Option<&'a str>,
        read: &[&str],
    ) -> PlanMeta {
        s.save(Save {
            slug,
            title: "T",
            content: "# plan\n",
            read: read.iter().map(|r| r.to_string()).collect(),
            session,
            base: None,
            adopt,
        })
        .unwrap()
    }

    #[test]
    fn round_trip_collision_and_update_in_place() {
        mu_testutil::home();
        let s = store_in(tmp());
        let (s1, s2) = (PathBuf::from("/s/1"), PathBuf::from("/s/2"));
        let a = save(&s, "x", Some(&s1), None, &["b"]);
        let (m, text) = s.load("x").unwrap();
        assert_eq!((m.clone(), text.as_str()), (a.clone(), "# plan\n"));
        assert_eq!(save(&s, "x", Some(&s2), None, &[]).id, "x-2");
        s.approve("x", "keep", None).unwrap();
        let b = save(&s, "x", Some(&s1), None, &["a", "b"]);
        assert_eq!(
            (b.created.clone(), b.read.clone(), b.status),
            (a.created.clone(), vec!["a".into(), "b".into()], Status::Pending)
        );
        assert!(b.approved.is_none());
        s.approve("x", "keep", None).unwrap();
        let c = save(&s, "x", None, Some("x"), &["c"]);
        assert_eq!((c.id.as_str(), c.read.len(), c.status), ("x", 3, Status::Pending));
    }

    #[test]
    fn status_rules_and_list() {
        mu_testutil::home();
        let s = store_in(tmp());
        save(&s, "p", None, None, &[]);
        save(&s, "q", None, None, &[]);
        s.approve("p", "execute", None).unwrap();
        assert_eq!(s.approve("p", "x", None).unwrap_err(), "plan 'p' is approved, not pending");
        assert_eq!(s.drop("p").unwrap_err(), "plan 'p' is approved, not pending");
        s.drop("q").unwrap();
        assert_eq!(s.drop("q").unwrap_err(), "plan 'q' is dropped, not pending");
        assert!(s.list(false).is_empty());
        assert_eq!(s.list(true).len(), 2);
        assert!(s.load("zz").unwrap_err().starts_with("plan 'zz' not found in "));
    }

    #[test]
    fn worktrees_share_a_store() {
        mu_testutil::home();
        let d = repo();
        let wt = tmp().join("wt");
        sh(&d, &["worktree", "add", "-q", wt.to_str().unwrap(), "-b", "other"]);
        let (a, b) = rt().block_on(async { (store_for(&d).await, store_for(&wt).await) });
        assert_eq!(a.dir, b.dir);
        assert_ne!(a.root, b.root);
    }

    fn meta(repo: &Path, read: &[&str], base: Option<Base>) -> PlanMeta {
        PlanMeta {
            id: "x".into(),
            title: "T".into(),
            status: Status::Pending,
            repo: repo.to_path_buf(),
            base,
            read: read.iter().map(|s| s.to_string()).collect(),
            session: None,
            created: String::new(),
            updated: String::new(),
            hash: String::new(),
            approved: None,
        }
    }

    #[test]
    fn drift_cases() {
        mu_testutil::home();
        let d = repo();
        let rt = rt();
        let mk = |read: &[&str], base: Option<Base>| meta(&d, read, base);
        let base = rt.block_on(base_now(&d));
        assert!(base.as_ref().unwrap().commit.is_some());
        let clean = rt.block_on(drift(&mk(&["a.txt"], base.clone()), &d));
        assert!(matches!(clean, Drift::Clean { other: 0, .. }));
        std::fs::write(d.join("a.txt"), "two\n").unwrap();
        match rt.block_on(drift(&mk(&["a.txt"], base.clone()), &d)) {
            Drift::Changed { files, other, .. } => assert_eq!((files, other), (vec!["a.txt".to_string()], 0)),
            x => panic!("{x:?}"),
        }
        assert!(matches!(rt.block_on(drift(&mk(&["b.txt"], base.clone()), &d)), Drift::Clean { other: 1, .. }));
        assert!(matches!(rt.block_on(drift(&mk(&[], base.clone()), &d)), Drift::Changed { .. }));
        assert!(matches!(rt.block_on(drift(&mk(&[""], base.clone()), &d)), Drift::Changed { .. }));
        assert!(matches!(rt.block_on(drift(&mk(&["", "b.txt"], base.clone()), &d)), Drift::Clean { other: 1, .. }));
        let commit = base.as_ref().unwrap().commit.clone().unwrap();
        assert_eq!(rt.block_on(base_label(&mk(&[], base.clone()), &d)), short_sha(&commit));
        let stale = Base { tree: "0".repeat(40), commit: base.unwrap().commit };
        assert!(matches!(rt.block_on(drift(&mk(&["a.txt"], Some(stale)), &d)), Drift::Changed { .. }));
        assert_eq!(rt.block_on(drift(&mk(&[], None), &d)), Drift::Unknown("no base recorded".into()));
        let plain = tmp();
        assert_eq!(rt.block_on(drift(&mk(&[], None), &plain)), Drift::Unknown("not a git repository".into()));
        let d = Drift::Changed {
            base: "abc".into(),
            dirty: false,
            files: (0..6).map(|i| format!("f{i}")).collect(),
            other: 0,
        };
        assert_eq!(d.describe(), "6 file(s) it read changed since abc: f0, f1, f2, f3, f4, …");
        assert_eq!(
            Drift::Clean { base: "abc".into(), dirty: false, other: 2 }.describe(),
            "none of the files it read changed since abc (2 other files changed)"
        );
        assert_eq!(
            Drift::Changed { base: "abc".into(), dirty: true, files: vec!["a.rs".into()], other: 0 }.describe(),
            "1 file(s) it read changed since abc plus uncommitted changes: a.rs"
        );
    }

    #[test]
    fn reads_from_keeps_files_only() {
        let call = |p: &str| Message::Assistant {
            content: vec![mu_base::message::Block::ToolCall {
                id: "i".into(),
                name: "read".into(),
                arguments: json!({"path": p}),
            }],
            api: String::new(),
            provider: String::new(),
            model: String::new(),
            usage: Default::default(),
            stop_reason: mu_base::message::StopReason::ToolUse,
            timestamp: 0,
            response_id: None,
            error_message: None,
        };
        let root = tmp();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.rs"), "x\n").unwrap();
        std::fs::write(root.join("b.rs"), "x\n").unwrap();
        let run = |cwd: &Path, ps: &[&str]| {
            let msgs: Vec<_> = ps.iter().copied().map(call).collect();
            reads_from(&msgs, cwd, &root)
        };
        let abs_root = root.to_string_lossy().into_owned();
        let all = [
            ".",
            "src",
            "src/",
            abs_root.as_str(),
            "src/a.rs:10-20",
            "./src/a.rs",
            "src/a.rs:raw",
            "src/a.rs:10-20:raw",
            "src/a.rs:raw:10-20",
            "b.rs:3,10-12:raw",
            "b.rs:raw:3,10-12",
            "local://x-plan.md",
            "/elsewhere/b.rs",
            "missing.rs",
        ];
        assert_eq!(run(&root, &all), ["b.rs", "src/a.rs"]);
        assert!(run(&root, &[".", "src"]).is_empty());
        assert_eq!(run(&root, &["src/a.rs:raw"]), ["src/a.rs"]);
        assert_eq!(run(&root, &["b.rs:raw:3,10-12"]), ["b.rs"]);
        assert_eq!(run(&root.join("src"), &["a.rs:raw", "../b.rs:raw"]), ["b.rs", "src/a.rs"]);
    }

    #[test]
    fn dirty_base_is_named() {
        mu_testutil::home();
        let d = repo();
        let rt = rt();
        std::fs::write(d.join("c.txt"), "new\n").unwrap();
        let base = rt.block_on(base_now(&d));
        let sha = short_sha(base.as_ref().unwrap().commit.as_deref().unwrap()).to_string();
        let m = meta(&d, &["a.txt"], base);
        assert_eq!(rt.block_on(base_label(&m, &d)), format!("{sha} plus uncommitted changes"));
        let dr = rt.block_on(drift(&m, &d));
        assert!(matches!(dr, Drift::Clean { dirty: true, other: 0, .. }), "{dr:?}");
        assert_eq!(dr.describe(), format!("none of the files it read changed since {sha} plus uncommitted changes"));
        assert_eq!(dr.json()["dirty"], true);
    }
}
