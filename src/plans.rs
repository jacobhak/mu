//! Durable plans: a plan written in plan mode can be parked and approved later, in another session.
//! Each plan is `<id>.md` (the text as written) plus `<id>.json` (metadata) under
//! `$MU_HOME/plans/<repo-key>/`. All worktrees of one clone share a store.
use crate::message::Message;
use crate::review;
use mu_common::fs::write_atomic;
use mu_common::paths::{encode_cwd, mu_home};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(clap::Subcommand)]
pub enum PlanCmd {
    /// List this repository's plans (pending only unless --all)
    List {
        #[arg(long)]
        all: bool,
        #[arg(long)]
        json: bool,
    },
    /// Show a plan, its metadata and drift
    Show {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Plan a brief in plan mode without a user and park the result
    New { brief: String },
    /// Revise a parked plan headless and park it again
    Refine { id: String, text: Option<String> },
    /// Approve a parked plan and run it headless in this directory
    Approve {
        id: String,
        #[arg(long)]
        anyway: bool,
    },
    /// Drop a parked plan
    Drop { id: String },
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Pending,
    Approved,
    Dropped,
}

impl Status {
    fn name(self) -> &'static str {
        match self {
            Status::Pending => "pending",
            Status::Approved => "approved",
            Status::Dropped => "dropped",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Base {
    pub tree: String,
    pub commit: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Approval {
    pub at: String,
    pub decision: String,
    pub session: Option<PathBuf>,
    pub hash: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PlanMeta {
    pub id: String,
    pub title: String,
    pub status: Status,
    pub repo: PathBuf,
    pub base: Option<Base>,
    pub read: Vec<String>,
    pub session: Option<PathBuf>,
    pub created: String,
    pub updated: String,
    pub hash: String,
    pub approved: Option<Approval>,
}

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

impl PlanMeta {
    /// The short commit (or tree) the plan was written against.
    pub fn base_label(&self) -> String {
        self.base.as_ref().map_or("unknown", |b| short(b.commit.as_deref().unwrap_or(&b.tree))).to_string()
    }
}

pub fn short(s: &str) -> &str {
    &s[..s.len().min(7)]
}

pub async fn store_for(cwd: &Path) -> Store {
    let root = review::repo_root(cwd).await.ok();
    let common = review::git(cwd, &["rev-parse", "--path-format=absolute", "--git-common-dir"], None).await;
    let common = match common {
        Ok(s) => Some(PathBuf::from(s.trim())),
        // Git older than 2.31: the common dir may come back relative.
        Err(_) => review::git(cwd, &["rev-parse", "--git-common-dir"], None).await.ok().map(|s| cwd.join(s.trim())),
    };
    let main = match common {
        Some(c) if root.is_some() => {
            if c.file_name().is_some_and(|n| n == ".git") {
                c.parent().map(Path::to_path_buf).unwrap_or(c)
            } else {
                c
            }
        }
        _ => cwd.to_path_buf(),
    };
    Store { dir: mu_home().join("plans").join(encode_cwd(&main)), root }
}

pub async fn base_now(root: &Path) -> Option<Base> {
    let tree = review::snapshot_tree(root).await.ok()?;
    let commit = review::git(root, &["rev-parse", "HEAD"], None).await.ok().map(|s| s.trim().to_string());
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
        let now = crate::session::iso_ms();
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
            at: crate::session::iso_ms(),
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
    Clean { base: String, other: usize },
    Changed { base: String, files: Vec<String>, other: usize },
}

impl Drift {
    pub fn describe(&self) -> String {
        match self {
            Drift::Unknown(r) => format!("drift unknown: {r}"),
            Drift::Clean { base, other } => {
                let mut s = format!("none of the files it read changed since {base}");
                if *other > 0 {
                    s.push_str(&format!(" ({other} other files changed)"));
                }
                s
            }
            Drift::Changed { base, files, .. } => {
                let mut s = format!(
                    "{} file(s) it read changed since {base}: {}",
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
            Drift::Clean { base, other } => json!({"state":"clean","base":base,"files":[],"other":other}),
            Drift::Changed { base, files, other } => {
                json!({"state":"changed","base":base,"files":files,"other":other})
            }
        }
    }
}

async fn object_exists(root: &Path, sha: &str) -> bool {
    review::git(root, &["cat-file", "-e", sha], None).await.is_ok()
}

pub async fn drift(meta: &PlanMeta, cwd: &Path) -> Drift {
    let Ok(root) = review::repo_root(cwd).await else {
        return Drift::Unknown("not a git repository".into());
    };
    let Some(b) = &meta.base else { return Drift::Unknown("no base recorded".into()) };
    let base = if object_exists(&root, &b.tree).await {
        &b.tree
    } else {
        match &b.commit {
            Some(c) if object_exists(&root, c).await => c,
            _ => {
                return Drift::Unknown(format!(
                    "base {} no longer exists",
                    short(b.commit.as_deref().unwrap_or(&b.tree))
                ));
            }
        }
    };
    let label = short(b.commit.as_deref().unwrap_or(&b.tree)).to_string();
    let stats = match review::snapshot_tree(&root).await {
        Ok(cur) => review::numstat(&root, base, &cur).await,
        Err(e) => Err(e),
    };
    let stats = match stats {
        Ok(s) => s,
        Err(e) => return Drift::Unknown(e),
    };
    let (files, rest): (Vec<_>, Vec<_>) =
        stats.into_iter().map(|s| s.path).partition(|p| meta.read.is_empty() || meta.read.contains(p));
    let other = rest.len();
    if files.is_empty() { Drift::Clean { base: label, other } } else { Drift::Changed { base: label, files, other } }
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

/// Repo-relative paths of every file the session read, selectors stripped.
pub fn reads_from(messages: &[Message], cwd: &Path, root: &Path) -> Vec<String> {
    let root = normalize(root);
    let mut out = Vec::new();
    for m in messages {
        for (_, name, args) in crate::agent::tool_calls(m) {
            if name != "read" {
                continue;
            }
            let Some(p) = args.get("path").and_then(|v| v.as_str()) else { continue };
            let p = crate::tools::uri::split_selector(p).map_or(p, |(h, _)| h);
            if p.contains("://") {
                continue;
            }
            let abs = normalize(&cwd.join(p));
            if let Ok(rel) = abs.strip_prefix(&root) {
                out.push(rel.to_string_lossy().into_owned());
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

fn fail(e: String) -> i32 {
    eprintln!("mu plan: {e}");
    1
}

async fn cli(cmd: PlanCmd, cwd: &Path) -> i32 {
    let store = store_for(cwd).await;
    match cmd {
        PlanCmd::List { all, json } => {
            let list = store.list(all);
            if json {
                let arr: Vec<_> = list
                    .iter()
                    .map(|m| {
                        let mut v = serde_json::to_value(m).unwrap_or_default();
                        v["path"] = store.md_path(&m.id).to_string_lossy().into_owned().into();
                        v
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&arr).unwrap_or_default());
            } else {
                for m in &list {
                    println!("{}  {}  {}  {}", m.id, m.status.name(), &m.updated[..m.updated.len().min(10)], m.title);
                }
            }
            0
        }
        PlanCmd::Show { id, json } => {
            let (meta, text) = match store.load(&id) {
                Ok(v) => v,
                Err(e) => return fail(e),
            };
            let d = drift(&meta, cwd).await;
            if json {
                let mut v = serde_json::to_value(&meta).unwrap_or_default();
                v["path"] = store.md_path(&id).to_string_lossy().into_owned().into();
                v["content"] = text.into();
                v["drift"] = d.json();
                println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
            } else {
                let base = meta.base.as_ref().map_or("none", |b| short(b.commit.as_deref().unwrap_or(&b.tree)));
                println!("id: {}\ntitle: {}\nstatus: {}", meta.id, meta.title, meta.status.name());
                println!("base: {base}\nread: {} files\ndrift: {}\n", meta.read.len(), d.describe());
                print!("{text}");
                if !text.ends_with('\n') {
                    println!();
                }
            }
            0
        }
        PlanCmd::Drop { id } => store.drop(&id).map_or_else(fail, |()| 0),
        _ => fail("not a store command".into()),
    }
}

/// `mu plan list|show|drop`; returns the exit code.
pub fn run_cli(cmd: PlanCmd, cwd: &Path) -> i32 {
    let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(r) => r,
        Err(e) => return fail(e.to_string()),
    };
    tokio::task::LocalSet::new().block_on(&rt, cli(cmd, cwd))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sh(dir: &Path, args: &[&str]) -> String {
        let o = std::process::Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(o.status.success(), "{args:?}: {}", String::from_utf8_lossy(&o.stderr));
        String::from_utf8_lossy(&o.stdout).into_owned()
    }

    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(format!("mu-plans-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d.canonicalize().unwrap()
    }

    fn repo() -> PathBuf {
        let d = tmp();
        sh(&d, &["init", "-q"]);
        std::fs::write(d.join("a.txt"), "one\n").unwrap();
        std::fs::write(d.join("b.txt"), "one\n").unwrap();
        sh(&d, &["add", "."]);
        sh(&d, &["commit", "-qm", "init"]);
        d
    }

    fn store_in(dir: PathBuf) -> Store {
        Store { dir, root: None }
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
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
        crate::testutil::home();
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
        crate::testutil::home();
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
        crate::testutil::home();
        let d = repo();
        let wt = tmp().join("wt");
        sh(&d, &["worktree", "add", "-q", wt.to_str().unwrap(), "-b", "other"]);
        let (a, b) = rt().block_on(async { (store_for(&d).await, store_for(&wt).await) });
        assert_eq!(a.dir, b.dir);
        assert_ne!(a.root, b.root);
    }

    #[test]
    fn drift_cases() {
        crate::testutil::home();
        let d = repo();
        let rt = rt();
        let mk = |read: &[&str], base: Option<Base>| PlanMeta {
            id: "x".into(),
            title: "T".into(),
            status: Status::Pending,
            repo: d.clone(),
            base,
            read: read.iter().map(|s| s.to_string()).collect(),
            session: None,
            created: String::new(),
            updated: String::new(),
            hash: String::new(),
            approved: None,
        };
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
        let stale = Base { tree: "0".repeat(40), commit: base.unwrap().commit };
        assert!(matches!(rt.block_on(drift(&mk(&["a.txt"], Some(stale)), &d)), Drift::Changed { .. }));
        assert_eq!(rt.block_on(drift(&mk(&[], None), &d)), Drift::Unknown("no base recorded".into()));
        let plain = tmp();
        assert_eq!(rt.block_on(drift(&mk(&[], None), &plain)), Drift::Unknown("not a git repository".into()));
        let d = Drift::Changed { base: "abc".into(), files: (0..6).map(|i| format!("f{i}")).collect(), other: 0 };
        assert_eq!(d.describe(), "6 file(s) it read changed since abc: f0, f1, f2, f3, f4, …");
        assert_eq!(
            Drift::Clean { base: "abc".into(), other: 2 }.describe(),
            "none of the files it read changed since abc (2 other files changed)"
        );
    }

    #[test]
    fn reads_from_normalizes() {
        let call = |p: &str| Message::Assistant {
            content: vec![crate::message::Block::ToolCall {
                id: "i".into(),
                name: "read".into(),
                arguments: json!({"path": p}),
            }],
            api: String::new(),
            provider: String::new(),
            model: String::new(),
            usage: Default::default(),
            stop_reason: crate::message::StopReason::ToolUse,
            timestamp: 0,
            response_id: None,
            error_message: None,
        };
        let msgs: Vec<_> =
            ["src/a.rs:10-20", "./src/a.rs", "local://x-plan.md", "/elsewhere/b.rs"].into_iter().map(call).collect();
        assert_eq!(reads_from(&msgs, Path::new("/r"), Path::new("/r")), ["src/a.rs"]);
    }
}
