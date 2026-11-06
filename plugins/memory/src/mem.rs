//! The memory store on disk: where the files live, the cross-process lock, and the read-modify-commit cycle
//! every change (tool call or background review) goes through.

use crate::gitx;
use crate::store::{self, Caps, File, Op, Outcome, State};
use fs4::FileExt;
use mu_common::fs::write_atomic;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct Config {
    pub review: bool,
    pub caps: Caps,
}

impl Default for Config {
    fn default() -> Self {
        Self { review: true, caps: Caps::default() }
    }
}

impl Config {
    /// `[memory]` of `$MU_HOME/config.toml`: `review`, `user_cap`, `project_cap`. Anything unreadable keeps the defaults.
    pub fn load() -> Self {
        let text = std::fs::read_to_string(mu_common::paths::mu_home().join("config.toml")).unwrap_or_default();
        Self::parse(&text)
    }

    pub fn parse(text: &str) -> Self {
        let mut c = Self::default();
        let Ok(t) = text.parse::<toml::Table>() else { return c };
        let Some(m) = t.get("memory").and_then(toml::Value::as_table) else { return c };
        if let Some(b) = m.get("review").and_then(toml::Value::as_bool) {
            c.review = b;
        }
        let cap = |k: &str| m.get(k).and_then(toml::Value::as_integer).filter(|n| *n > 0).map(|n| n as usize);
        if let Some(n) = cap("user_cap") {
            c.caps.user = n;
        }
        if let Some(n) = cap("project_cap") {
            c.caps.project = n;
        }
        c
    }
}

#[derive(Clone, Debug)]
pub struct Mem {
    /// `<profile home>/memory`, the git repository.
    pub root: PathBuf,
    /// The main checkout this memory belongs to (for display).
    pub checkout: PathBuf,
    pub key: String,
    pub cfg: Config,
}

#[derive(Debug)]
pub struct Edited {
    pub outcome: Outcome,
    pub state: State,
    /// Placeholders that replaced secrets in the written text.
    pub redacted: Vec<String>,
    pub history: bool,
}

pub struct LockGuard(std::fs::File);

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.0);
    }
}

impl Mem {
    /// The memory of the repository `cwd` is in, in this profile.
    pub async fn open(cwd: &Path, cfg: Config) -> Self {
        let checkout = mu_base::git::main_checkout(cwd).await;
        let key = mu_common::paths::encode_cwd(&checkout);
        Self { root: mu_common::paths::profile_home().join("memory"), checkout, key, cfg }
    }

    pub fn user_path(&self) -> PathBuf {
        self.root.join("user.md")
    }

    pub fn project_dir(&self) -> PathBuf {
        self.root.join("projects").join(&self.key)
    }

    pub fn project_path(&self) -> PathBuf {
        self.project_dir().join("memory.md")
    }

    pub fn notes_dir(&self) -> PathBuf {
        self.project_dir().join("notes")
    }

    pub fn note_path(&self, slug: &str) -> PathBuf {
        self.notes_dir().join(format!("{slug}.md"))
    }

    pub fn state_dir(&self) -> PathBuf {
        self.root.join(".state")
    }

    pub fn path_of(&self, f: File) -> PathBuf {
        match f {
            File::User => self.user_path(),
            File::Project => self.project_path(),
        }
    }

    /// The current state on disk. A missing file is empty.
    pub fn load(&self) -> State {
        let entries = |p: PathBuf| store::parse_entries(&std::fs::read_to_string(p).unwrap_or_default());
        let mut notes = std::collections::BTreeMap::new();
        if let Ok(rd) = std::fs::read_dir(self.notes_dir()) {
            for e in rd.flatten() {
                let p = e.path();
                let Some(slug) =
                    p.file_stem().and_then(|s| s.to_str()).filter(|_| p.extension().is_some_and(|x| x == "md"))
                else {
                    continue;
                };
                if !store::valid_slug(slug) {
                    continue;
                }
                if let Ok(t) = std::fs::read_to_string(&p) {
                    notes.insert(slug.to_string(), store::parse_note(&t));
                }
            }
        }
        State { user: entries(self.user_path()), project: entries(self.project_path()), notes }
    }

    /// Takes the exclusive lock, waiting up to 30 s. Concurrent sessions serialise on it.
    pub async fn lock(&self) -> Result<LockGuard, String> {
        std::fs::create_dir_all(self.state_dir()).map_err(|e| format!("{}: {e}", self.state_dir().display()))?;
        let f = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.state_dir().join("lock"))
            .map_err(|e| format!("cannot open the memory lock: {e}"))?;
        for _ in 0..600 {
            match FileExt::try_lock(&f) {
                Ok(()) => return Ok(LockGuard(f)),
                Err(fs4::TryLockError::WouldBlock) => tokio::time::sleep(Duration::from_millis(50)).await,
                Err(fs4::TryLockError::Error(e)) => return Err(format!("cannot lock the memory directory: {e}")),
            }
        }
        Err("the memory directory is locked by another session".into())
    }

    /// Initialises git and commits hand edits as `manual edit`, so they are history before ours.
    /// Call with the lock held. `false` when there is no history.
    pub async fn prepare(&self) -> bool {
        let history = gitx::ensure(&self.root).await;
        if history {
            let _ = gitx::commit_all(&self.root, "manual edit").await;
        }
        history
    }

    /// Applies `ops` under the lock and commits as `<prefix>: <summary>`.
    pub async fn edit(&self, mut ops: Vec<Op>, cwd: &Path, prefix: &str) -> Result<Edited, String> {
        let redacted = redact_ops(&mut ops, cwd);
        let _guard = self.lock().await?;
        let history = self.prepare().await;
        let (next, outcome) = store::apply(&self.load(), &ops, &self.cfg.caps)?;
        if outcome.changed() {
            self.write(&next, &outcome)?;
            if history {
                let msg = format!("{prefix}: {}", outcome.summary);
                if let Err(e) = gitx::commit_all(&self.root, &msg).await {
                    return Err(format!("saved, but the commit failed: {e}"));
                }
            }
        }
        Ok(Edited { outcome, state: next, redacted, history })
    }

    fn write(&self, next: &State, out: &Outcome) -> Result<(), String> {
        let put = |p: PathBuf, text: &str| -> Result<(), String> {
            if let Some(d) = p.parent() {
                std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
            }
            write_atomic(&p, text.as_bytes()).map_err(|e| format!("{}: {e}", p.display()))
        };
        for f in &out.files {
            put(self.path_of(*f), &store::format_entries(next.entries(*f)))?;
        }
        for slug in &out.notes {
            match next.notes.get(slug) {
                Some(n) => put(self.note_path(slug), &store::format_note(n))?,
                None => match std::fs::remove_file(self.note_path(slug)) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(format!("{slug}: {e}")),
                },
            }
        }
        Ok(())
    }
}

/// Replaces secrets in every text of `ops`; returns the placeholders used.
pub fn redact_ops(ops: &mut [Op], cwd: &Path) -> Vec<String> {
    let mut hits = Vec::new();
    for op in ops {
        for t in op.texts_mut() {
            let r = mu_base::secrets::redact(t, cwd);
            if !r.hits.is_empty() {
                *t = r.text;
                mu_base::secrets::merge(&mut hits, r.hits);
            }
        }
    }
    hits.iter().map(|h| h.placeholder()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::NoteType;

    pub(crate) fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mu-memory-test-{name}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    pub(crate) fn mem_in(dir: &Path) -> Mem {
        Mem { root: dir.join("memory"), checkout: dir.join("repo"), key: "-repo".into(), cfg: Config::default() }
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
    }

    fn add(f: File, t: &str) -> Op {
        Op::Add { file: f, text: t.into() }
    }

    #[test]
    fn config_reads_the_memory_table() {
        let c = Config::parse("[memory]\nreview = false\nuser_cap = 900\nproject_cap = -1\n");
        assert!(!c.review);
        assert_eq!((c.caps.user, c.caps.project), (900, 4000));
        assert!(Config::parse("not toml [").review);
    }

    #[test]
    fn an_edit_writes_files_and_commits() {
        let d = tmp("edit");
        let m = mem_in(&d);
        rt().block_on(async {
            let e = m
                .edit(
                    vec![
                        add(File::User, "Prefers terse replies"),
                        add(File::Project, "Releases are cut from main"),
                        Op::WriteNote {
                            note: "rel".into(),
                            description: "Release process".into(),
                            kind: NoteType::Reference,
                            text: "Tag it.".into(),
                        },
                    ],
                    &d,
                    "tool s1",
                )
                .await
                .unwrap();
            assert!(e.history && e.outcome.changed());
            let log = gitx::log(&m.root, 10).await.unwrap();
            assert_eq!(log[0].message, "tool s1: add user, add project, write_note rel");
            assert!(log[0].files.iter().any(|f| f == "user.md"));
            assert!(log[0].files.iter().any(|f| f.ends_with("notes/rel.md")));
            assert_eq!(m.load(), e.state);
            assert!(std::fs::read_to_string(m.root.join(".gitignore")).unwrap().contains(".state/"));
            // A no-op edit commits nothing.
            let e = m.edit(vec![add(File::User, "Prefers terse replies")], &d, "tool s1").await.unwrap();
            assert!(!e.outcome.changed());
            assert_eq!(gitx::log(&m.root, 10).await.unwrap().len(), 2);
        });
    }

    #[test]
    fn hand_edits_are_committed_before_ours_and_survive() {
        let d = tmp("drift");
        let m = mem_in(&d);
        rt().block_on(async {
            m.edit(vec![add(File::User, "One")], &d, "tool s").await.unwrap();
            std::fs::write(m.user_path(), "One\n§\nHand written\n").unwrap();
            m.edit(vec![add(File::User, "Two")], &d, "tool s").await.unwrap();
            assert_eq!(m.load().user, ["One", "Hand written", "Two"]);
            let log = gitx::log(&m.root, 10).await.unwrap();
            let msgs: Vec<&str> = log.iter().map(|c| c.message.as_str()).collect();
            assert_eq!(msgs, ["tool s: add user", "manual edit", "tool s: add user", "init"]);
        });
    }

    #[test]
    fn a_failed_batch_changes_nothing() {
        let d = tmp("atomic");
        let m = mem_in(&d);
        rt().block_on(async {
            m.edit(vec![add(File::User, "One")], &d, "t").await.unwrap();
            let e = m
                .edit(vec![add(File::User, "Two"), Op::Remove { file: File::User, pattern: "zzz".into() }], &d, "t")
                .await
                .unwrap_err();
            assert!(e.contains("hits no entry"));
            assert_eq!(m.load().user, ["One"]);
        });
    }

    #[test]
    fn concurrent_edits_serialise_on_the_lock() {
        let d = tmp("lock");
        let m = mem_in(&d);
        rt().block_on(async {
            let mut tasks = Vec::new();
            for i in 0..6 {
                let (m, d) = (m.clone(), d.clone());
                tasks.push(tokio::spawn(async move {
                    m.edit(vec![add(File::Project, &format!("entry {i}"))], &d, "t").await.unwrap();
                }));
            }
            for t in tasks {
                t.await.unwrap();
            }
            assert_eq!(m.load().project.len(), 6);
            assert_eq!(gitx::log(&m.root, 20).await.unwrap().len(), 7);
        });
    }

    #[test]
    fn revert_undoes_a_commit() {
        let d = tmp("revert");
        let m = mem_in(&d);
        rt().block_on(async {
            m.edit(vec![add(File::User, "One")], &d, "t").await.unwrap();
            m.edit(vec![add(File::User, "Two")], &d, "t").await.unwrap();
            let log = gitx::log(&m.root, 10).await.unwrap();
            let _g = m.lock().await.unwrap();
            gitx::revert(&m.root, &log[0].sha).await.unwrap();
            assert_eq!(m.load().user, ["One"]);
        });
    }
}
