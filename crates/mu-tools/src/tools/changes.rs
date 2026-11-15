//! Putting a launched worker's result (`refs/mu/result/<id>`) into the parent's working tree.
//!
//! A result is applied on its own only when every file it touches is still what the task's
//! snapshot (`refs/mu/task/<id>`) holds; otherwise the ref stays and the jobs panel offers the diff
//! and a three-way apply. Every apply runs under one lock, so results that finish together go in
//! turn. An applied result gets a marker ref, `refs/mu/applied/<id>`, which is what pruning reads.

use mu_base::git::git;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;

pub const RESULT_PREFIX: &str = "refs/mu/result/";
/// How long a result ref is kept before an applied one is pruned and an unapplied one is reported.
pub const KEPT_AFTER: Duration = Duration::from_secs(14 * 24 * 3600);
const APPLIED_PREFIX: &str = "refs/mu/applied/";

static APPLYING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// A result ref a launch could have made. Anything else is refused before git sees it.
pub fn valid_result_ref(r: &str) -> bool {
    r.strip_prefix(RESULT_PREFIX)
        .is_some_and(|id| !id.is_empty() && id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-'))
}

pub fn valid_base(b: &str) -> bool {
    matches!(b.len(), 40 | 64) && b.bytes().all(|c| c.is_ascii_hexdigit())
}

fn check(r: &str, base: &str) -> Result<(), String> {
    if !valid_result_ref(r) {
        return Err(format!("{r} is not a worker result ref"));
    }
    if !valid_base(base) {
        return Err("the base is not a commit hash".into());
    }
    Ok(())
}

/// What `auto_apply` did.
#[derive(Debug, PartialEq, Eq)]
pub enum Auto {
    /// The diff went in; this many files.
    Applied(usize),
    /// These touched files are not what the snapshot holds, so nothing was applied.
    Stale(Vec<String>),
    Failed(String),
}

/// What a three-way apply did.
#[derive(Debug, PartialEq, Eq)]
pub struct Merged {
    pub files: usize,
    /// Files left with conflict markers (or, for a file git cannot merge, left alone).
    pub conflicts: Vec<String>,
}

/// The command that puts the result over a changed tree by hand.
pub fn command(base: &str, result_ref: &str) -> String {
    format!("git diff {base} {result_ref} | git apply --3way")
}

/// The line a task's result carries for `auto`.
pub fn summary(auto: &Auto, result_ref: &str, base: &str) -> String {
    let by_hand = command(base, result_ref);
    match auto {
        Auto::Applied(n) => format!("[changes: applied: {n} files; ref {result_ref}]"),
        Auto::Stale(files) => format!(
            "[changes: not applied: {} changed since the task started; ref {result_ref}; to apply over them: {by_hand}]",
            files.join(", ")
        ),
        Auto::Failed(why) => format!("[changes: not applied: {why}; ref {result_ref}; to apply by hand: {by_hand}]"),
    }
}

async fn run(root: &Path, args: &[&str], input: Option<&[u8]>) -> Result<Vec<u8>, String> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.args(args)
        .current_dir(root)
        .stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = cmd.spawn().map_err(|e| format!("git {}: {e}", args.first().unwrap_or(&"")))?;
    let stdin = child.stdin.take();
    let feed = async {
        if let (Some(mut s), Some(data)) = (stdin, input) {
            let _ = s.write_all(data).await;
        }
    };
    let (_, out) = tokio::join!(feed, child.wait_with_output());
    let out = out.map_err(|e| format!("git {}: {e}", args.first().unwrap_or(&"")))?;
    if out.status.success() {
        return Ok(out.stdout);
    }
    let err = String::from_utf8_lossy(&out.stderr);
    let err = err.trim();
    Err(if err.is_empty() { format!("git {} failed", args.first().unwrap_or(&"")) } else { err.to_string() })
}

/// The files the result changes, renames counted as the delete and the add.
async fn touched(root: &Path, base: &str, result_ref: &str) -> Result<Vec<String>, String> {
    let out = run(root, &["diff", "--name-only", "-z", "--no-renames", base, result_ref, "--"], None).await?;
    Ok(String::from_utf8_lossy(&out).split('\0').filter(|f| !f.is_empty()).map(str::to_string).collect())
}

/// Of `files`, those whose working tree content is not the snapshot's: different bytes, present on
/// one side only, a symlink, or a directory.
async fn changed_since(root: &Path, task_ref: &str, files: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for f in files {
        let snap = run(root, &["rev-parse", "--verify", "-q", &format!("{task_ref}:{f}")], None)
            .await
            .ok()
            .map(|b| String::from_utf8_lossy(&b).trim().to_string());
        let now = match std::fs::symlink_metadata(root.join(f)) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Ok(m) if m.is_file() => run(root, &["hash-object", "--", f], None)
                .await
                .ok()
                .map(|b| String::from_utf8_lossy(&b).trim().to_string()),
            _ => {
                out.push(f.clone());
                continue;
            }
        };
        if snap != now {
            out.push(f.clone());
        }
    }
    out
}

async fn mark_applied(root: &Path, result_ref: &str) {
    if let Some(id) = result_ref.strip_prefix(RESULT_PREFIX) {
        let _ = git(root, &["update-ref", &format!("{APPLIED_PREFIX}{id}"), result_ref], None).await;
    }
}

/// Applies the result when every file it touches is unchanged since the snapshot: with `--index`
/// when the index agrees with the files, else to the working tree alone.
pub async fn auto_apply(root: &Path, task_ref: &str, base: &str, result_ref: &str) -> Auto {
    if let Err(e) = check(result_ref, base) {
        return Auto::Failed(e);
    }
    let _turn = APPLYING.lock().await;
    let files = match touched(root, base, result_ref).await {
        Ok(f) => f,
        Err(e) => return Auto::Failed(e),
    };
    let stale = changed_since(root, task_ref, &files).await;
    if !stale.is_empty() {
        return Auto::Stale(stale);
    }
    if files.is_empty() {
        mark_applied(root, result_ref).await;
        return Auto::Applied(0);
    }
    let patch = match run(root, &["diff", "--binary", base, result_ref, "--"], None).await {
        Ok(p) => p,
        Err(e) => return Auto::Failed(e),
    };
    let mut last = String::new();
    for flags in [&["--index"][..], &[][..]] {
        let mut dry = vec!["apply", "--check"];
        dry.extend(flags);
        if let Err(e) = run(root, &dry, Some(&patch)).await {
            last = e;
            continue;
        }
        let mut real = vec!["apply"];
        real.extend(flags);
        return match run(root, &real, Some(&patch)).await {
            Ok(_) => {
                mark_applied(root, result_ref).await;
                Auto::Applied(files.len())
            }
            Err(e) => Auto::Failed(e),
        };
    }
    Auto::Failed(last)
}

/// `git diff base result`, for reading.
pub async fn diff(root: &Path, base: &str, result_ref: &str) -> Result<String, String> {
    check(result_ref, base)?;
    let out = run(root, &["diff", base, result_ref, "--"], None).await?;
    Ok(String::from_utf8_lossy(&out).into_owned())
}

/// Applies the result with a three-way merge over whatever the tree holds now. Conflicts come back
/// in `Merged`, with their markers in the files.
pub async fn apply_3way(root: &Path, base: &str, result_ref: &str) -> Result<Merged, String> {
    check(result_ref, base)?;
    let _turn = APPLYING.lock().await;
    let files = touched(root, base, result_ref).await?;
    let patch = run(root, &["diff", "--binary", base, result_ref, "--"], None).await?;
    let merged = match run(root, &["apply", "--3way"], Some(&patch)).await {
        Ok(_) => Merged { files: files.len(), conflicts: Vec::new() },
        Err(e) if e.contains("with conflicts") => Merged { files: files.len(), conflicts: conflicted(&e) },
        // Unstaged edits to a touched file: `apply --3way` insists on `--index`, so merge file by file.
        Err(_) => merge_files(root, base, result_ref, &files).await?,
    };
    mark_applied(root, result_ref).await;
    Ok(merged)
}

/// The files `git apply --3way` named in "Applied patch to 'f' with conflicts."
fn conflicted(stderr: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in stderr.lines() {
        if let Some(rest) = line.strip_prefix("Applied patch to '")
            && let Some(name) = rest.strip_suffix("' with conflicts.")
            && !out.iter().any(|o| o == name)
        {
            out.push(name.to_string());
        }
    }
    out
}

async fn blob(root: &Path, rev: &str, file: &str) -> Option<Vec<u8>> {
    run(root, &["cat-file", "blob", &format!("{rev}:{file}")], None).await.ok()
}

/// The three-way merge of each touched file: the base, the result, and the file as it is.
async fn merge_files(root: &Path, base: &str, result_ref: &str, files: &[String]) -> Result<Merged, String> {
    let scratch = std::env::temp_dir().join(format!("mu-merge-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&scratch).map_err(|e| e.to_string())?;
    let mut conflicts = Vec::new();
    for f in files {
        let path: PathBuf = root.join(f);
        let (old, theirs) = (blob(root, base, f).await, blob(root, result_ref, f).await);
        let ours = std::fs::read(&path).ok();
        if old == theirs || ours == theirs {
            continue;
        }
        if ours == old {
            let done = match &theirs {
                Some(bytes) => {
                    path.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|()| std::fs::write(&path, bytes))
                }
                None => std::fs::remove_file(&path),
            };
            if done.is_err() {
                conflicts.push(f.clone());
            }
            continue;
        }
        let (Some(old), Some(theirs), Some(ours)) = (old, theirs, ours) else {
            conflicts.push(f.clone());
            continue;
        };
        let (a, b, c) = (scratch.join("ours"), scratch.join("base"), scratch.join("theirs"));
        let wrote = std::fs::write(&a, &ours)
            .and_then(|()| std::fs::write(&b, &old))
            .and_then(|()| std::fs::write(&c, &theirs));
        let status = match wrote {
            Ok(()) => tokio::process::Command::new("git")
                .args(["merge-file", "-L", "ours", "-L", "base", "-L", "theirs"])
                .args([&a, &b, &c])
                .current_dir(root)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .await
                .ok()
                .and_then(|s| s.code()),
            Err(_) => None,
        };
        match status {
            // Negative codes and 255 are errors (a binary file); the file is left as it was.
            Some(n) if (0..128).contains(&n) => {
                if std::fs::copy(&a, &path).is_err() || n > 0 {
                    conflicts.push(f.clone());
                }
            }
            _ => conflicts.push(f.clone()),
        }
    }
    let _ = std::fs::remove_dir_all(&scratch);
    Ok(Merged { files: files.len(), conflicts })
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Deletes the result refs older than `max_age` that were applied, with their markers. A result that
/// was never applied is kept: it is the only copy of the worker's changes.
pub async fn prune(root: &Path, max_age: Duration) -> Result<(), String> {
    let results =
        git(root, &["for-each-ref", "--format=%(committerdate:unix) %(refname)", RESULT_PREFIX], None).await?;
    let applied = git(root, &["for-each-ref", "--format=%(refname)", APPLIED_PREFIX], None).await?;
    let applied: Vec<&str> = applied.lines().collect();
    let mut alive: Vec<&str> = Vec::new();
    for line in results.lines() {
        let Some((when, name)) = line.split_once(' ') else { continue };
        let Some(id) = name.strip_prefix(RESULT_PREFIX) else { continue };
        let marker = format!("{APPLIED_PREFIX}{id}");
        let old = when.parse::<u64>().is_ok_and(|w| now_secs().saturating_sub(w) > max_age.as_secs());
        if old && applied.contains(&marker.as_str()) {
            git(root, &["update-ref", "-d", name], None).await?;
            git(root, &["update-ref", "-d", &marker], None).await?;
        } else {
            alive.push(id);
        }
    }
    for marker in applied {
        if marker.strip_prefix(APPLIED_PREFIX).is_some_and(|id| !alive.contains(&id)) {
            git(root, &["update-ref", "-d", marker], None).await?;
        }
    }
    Ok(())
}

/// The Sessions tab's notice: results kept past `max_age` because nobody applied them. `None` when
/// there are none. Runs git synchronously; a listing of refs is quick.
pub fn unapplied_notice(cwd: &Path, max_age: Duration) -> Option<String> {
    let list = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()
            .filter(|o| o.status.success())?;
        Some(String::from_utf8_lossy(&out.stdout).into_owned())
    };
    let results = list(&["for-each-ref", "--format=%(committerdate:unix) %(refname)", RESULT_PREFIX])?;
    let applied = list(&["for-each-ref", "--format=%(refname)", APPLIED_PREFIX])?;
    let now = now_secs();
    let ages: Vec<u64> = results
        .lines()
        .filter_map(|l| l.split_once(' '))
        .filter(|(_, name)| {
            name.strip_prefix(RESULT_PREFIX)
                .is_some_and(|id| !applied.lines().any(|a| a == format!("{APPLIED_PREFIX}{id}")))
        })
        .filter_map(|(when, _)| when.parse::<u64>().ok())
        .map(|w| now.saturating_sub(w))
        .filter(|age| *age > max_age.as_secs())
        .collect();
    let oldest = ages.iter().max()? / 86400;
    Some(format!(
        "{} unapplied worker result{} kept past {} days (oldest {oldest}): refs/mu/result/*",
        ages.len(),
        if ages.len() == 1 { "" } else { "s" },
        max_age.as_secs() / 86400
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use mu_testutil::{repo, sh, tmp};

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    /// A task snapshot of `root`'s current tree and a result that rewrites `edits` on top of it.
    /// Returns (task ref, base, result ref).
    fn task(root: &Path, id: &str, edits: &[(&str, &str)]) -> (String, String, String) {
        let tree = sh(root, &["write-tree"]);
        let snap = sh(root, &["commit-tree", tree.trim(), "-p", "HEAD", "-m", "snapshot"]);
        let task_ref = format!("refs/mu/task/{id}");
        sh(root, &["update-ref", &task_ref, snap.trim()]);
        let work = tmp().join("work");
        sh(root, &["worktree", "add", "-q", "--detach", work.to_str().unwrap(), snap.trim()]);
        for (f, text) in edits {
            std::fs::write(work.join(f), text).unwrap();
        }
        sh(&work, &["add", "-A"]);
        sh(&work, &["commit", "-qm", "work"]);
        let result_ref = format!("{RESULT_PREFIX}{id}");
        sh(&work, &["update-ref", &result_ref, "HEAD"]);
        (task_ref, snap.trim().to_string(), result_ref)
    }

    fn marked(root: &Path, id: &str) -> bool {
        !sh(root, &["for-each-ref", &format!("{APPLIED_PREFIX}{id}")]).is_empty()
    }

    #[tokio::test]
    async fn a_result_over_an_unchanged_tree_is_applied_and_staged() {
        let root = repo();
        let (t, base, r) = task(&root, "x-1", &[("a.txt", "two\n")]);
        let got = auto_apply(&root, &t, &base, &r).await;
        assert_eq!(got, Auto::Applied(1));
        assert_eq!(summary(&got, &r, &base), format!("[changes: applied: 1 files; ref {r}]"));
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "two\n");
        assert_eq!(sh(&root, &["diff", "--cached", "--name-only"]).trim(), "a.txt", "the clean index took it too");
        assert!(marked(&root, "x-1"));
    }

    #[tokio::test]
    async fn a_file_the_result_touches_that_changed_since_the_snapshot_stops_the_apply() {
        let root = repo();
        let (t, base, r) = task(&root, "x-2", &[("a.txt", "theirs\n"), ("b.txt", "theirs\n")]);
        std::fs::write(root.join("a.txt"), "mine\n").unwrap();
        let got = auto_apply(&root, &t, &base, &r).await;
        assert_eq!(got, Auto::Stale(vec!["a.txt".into()]));
        let text = summary(&got, &r, &base);
        assert!(text.contains("a.txt changed since the task started"), "{text}");
        assert!(
            text.contains(&format!("ref {r}")) && text.contains(&format!("git diff {base} {r} | git apply --3way")),
            "{text}"
        );
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "mine\n");
        assert_eq!(std::fs::read_to_string(root.join("b.txt")).unwrap(), "one\n", "nothing was applied");
        assert!(!marked(&root, "x-2"));
    }

    #[tokio::test]
    async fn a_change_to_a_file_the_result_does_not_touch_does_not_matter() {
        let root = repo();
        let (t, base, r) = task(&root, "x-3", &[("b.txt", "theirs\n")]);
        std::fs::write(root.join("a.txt"), "mine\n").unwrap();
        std::fs::write(root.join("c.txt"), "new\n").unwrap();
        assert_eq!(auto_apply(&root, &t, &base, &r).await, Auto::Applied(1));
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "mine\n");
    }

    #[tokio::test]
    async fn a_file_that_was_untracked_at_the_snapshot_is_applied_without_the_index() {
        let root = repo();
        std::fs::write(root.join("new.txt"), "one\n").unwrap();
        // The snapshot holds `new.txt` as `git add -A` would; the real index does not.
        let ix = tmp().join("ix");
        let with = |args: &[&str]| {
            let o = std::process::Command::new("git")
                .args(args)
                .current_dir(&root)
                .env("GIT_INDEX_FILE", &ix)
                .output()
                .unwrap();
            assert!(o.status.success(), "{args:?}");
            String::from_utf8_lossy(&o.stdout).into_owned()
        };
        with(&["read-tree", "HEAD"]);
        with(&["add", "-A"]);
        let snap_tree = with(&["write-tree"]);
        let snap = sh(&root, &["commit-tree", snap_tree.trim(), "-p", "HEAD", "-m", "snapshot"]);
        let snap = snap.trim();
        sh(&root, &["update-ref", "refs/mu/task/x-4", snap]);
        let work = tmp().join("work");
        sh(&root, &["worktree", "add", "-q", "--detach", work.to_str().unwrap(), snap]);
        std::fs::write(work.join("new.txt"), "two\n").unwrap();
        sh(&work, &["commit", "-qam", "work"]);
        sh(&work, &["update-ref", "refs/mu/result/x-4", "HEAD"]);
        let got = auto_apply(&root, "refs/mu/task/x-4", snap, "refs/mu/result/x-4").await;
        assert_eq!(got, Auto::Applied(1));
        assert_eq!(std::fs::read_to_string(root.join("new.txt")).unwrap(), "two\n");
        assert_eq!(sh(&root, &["diff", "--cached", "--name-only"]).trim(), "", "the index was left alone");
    }

    #[tokio::test]
    async fn two_results_finishing_together_are_applied_in_turn() {
        let root = repo();
        let (t1, b1, r1) = task(&root, "x-5", &[("a.txt", "from one\n")]);
        let (t2, b2, r2) = task(&root, "x-6", &[("b.txt", "from two\n")]);
        let (one, two) = tokio::join!(auto_apply(&root, &t1, &b1, &r1), auto_apply(&root, &t2, &b2, &r2));
        assert_eq!((one, two), (Auto::Applied(1), Auto::Applied(1)));
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "from one\n");
        assert_eq!(std::fs::read_to_string(root.join("b.txt")).unwrap(), "from two\n");
    }

    #[tokio::test]
    async fn a_second_result_over_a_file_the_first_just_changed_is_not_applied() {
        let root = repo();
        let (t1, b1, r1) = task(&root, "x-7", &[("a.txt", "from one\n")]);
        let (t2, b2, r2) = task(&root, "x-8", &[("a.txt", "from two\n")]);
        let (one, two) = tokio::join!(auto_apply(&root, &t1, &b1, &r1), auto_apply(&root, &t2, &b2, &r2));
        assert_eq!(one, Auto::Applied(1));
        assert_eq!(two, Auto::Stale(vec!["a.txt".into()]));
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "from one\n");
    }

    #[tokio::test]
    async fn refs_and_bases_that_a_launch_could_not_have_made_are_refused() {
        let root = repo();
        for r in ["refs/heads/main", "refs/mu/result/", "refs/mu/result/a b", "refs/mu/result/../x", "HEAD"] {
            assert!(matches!(auto_apply(&root, "refs/mu/task/x", SHA, r).await, Auto::Failed(_)), "{r}");
            assert!(diff(&root, SHA, r).await.is_err(), "{r}");
            assert!(apply_3way(&root, SHA, r).await.is_err(), "{r}");
        }
        assert!(diff(&root, "HEAD", "refs/mu/result/x").await.is_err());
    }

    #[tokio::test]
    async fn the_diff_is_what_the_worker_changed() {
        let root = repo();
        let (_, base, r) = task(&root, "x-9", &[("a.txt", "two\n")]);
        let text = diff(&root, &base, &r).await.unwrap();
        assert!(text.contains("-one") && text.contains("+two") && text.contains("a.txt"), "{text}");
    }

    #[tokio::test]
    async fn a_three_way_apply_reports_a_conflict_and_leaves_the_markers() {
        let root = repo();
        let (_, base, r) = task(&root, "x-10", &[("a.txt", "theirs\n")]);
        std::fs::write(root.join("a.txt"), "mine\n").unwrap();
        let got = apply_3way(&root, &base, &r).await.unwrap();
        assert_eq!(got, Merged { files: 1, conflicts: vec!["a.txt".into()] });
        let text = std::fs::read_to_string(root.join("a.txt")).unwrap();
        assert!(text.contains("<<<<<<<") && text.contains("mine") && text.contains("theirs"), "{text}");
        assert!(marked(&root, "x-10"));
    }

    #[tokio::test]
    async fn a_three_way_apply_merges_edits_to_different_lines() {
        let root = repo();
        std::fs::write(root.join("a.txt"), "l1\nl2\nl3\nl4\nl5\n").unwrap();
        sh(&root, &["commit", "-qam", "lines"]);
        let (_, base, r) = task(&root, "x-11", &[("a.txt", "l1\nl2\nl3\nl4\nRESULT\n"), ("b.txt", "theirs\n")]);
        std::fs::write(root.join("a.txt"), "MINE\nl2\nl3\nl4\nl5\n").unwrap();
        let got = apply_3way(&root, &base, &r).await.unwrap();
        assert_eq!(got, Merged { files: 2, conflicts: Vec::new() });
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "MINE\nl2\nl3\nl4\nRESULT\n");
        assert_eq!(std::fs::read_to_string(root.join("b.txt")).unwrap(), "theirs\n");
    }

    #[tokio::test]
    async fn a_three_way_apply_over_a_clean_tree_stages_the_result() {
        let root = repo();
        let (_, base, r) = task(&root, "x-12", &[("a.txt", "two\n")]);
        let got = apply_3way(&root, &base, &r).await.unwrap();
        assert_eq!(got, Merged { files: 1, conflicts: Vec::new() });
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "two\n");
    }

    fn old_ref(root: &Path, name: &str, days: u64) {
        let tree = sh(root, &["rev-parse", "HEAD^{tree}"]);
        let out = std::process::Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@t", "commit-tree", tree.trim(), "-m", "x"])
            .env("GIT_COMMITTER_DATE", format!("{} +0000", now_secs() - days * 86400))
            .current_dir(root)
            .output()
            .unwrap();
        sh(root, &["update-ref", name, String::from_utf8_lossy(&out.stdout).trim()]);
    }

    fn refs(root: &Path, prefix: &str) -> Vec<String> {
        sh(root, &["for-each-ref", "--format=%(refname)", prefix]).lines().map(str::to_string).collect()
    }

    #[tokio::test]
    async fn pruning_skips_results_that_were_never_applied() {
        let root = repo();
        let max = Duration::from_secs(14 * 86400);
        old_ref(&root, "refs/mu/result/applied-old", 15);
        old_ref(&root, "refs/mu/result/never-old", 15);
        old_ref(&root, "refs/mu/result/applied-new", 1);
        old_ref(&root, "refs/mu/applied/applied-old", 15);
        old_ref(&root, "refs/mu/applied/applied-new", 1);
        old_ref(&root, "refs/mu/applied/orphan", 1);
        let notice = unapplied_notice(&root, max).unwrap();
        assert!(notice.starts_with("1 unapplied worker result kept") && notice.contains("oldest 15)"), "{notice}");
        prune(&root, max).await.unwrap();
        assert_eq!(refs(&root, "refs/mu/result/"), ["refs/mu/result/applied-new", "refs/mu/result/never-old"]);
        assert_eq!(refs(&root, "refs/mu/applied/"), ["refs/mu/applied/applied-new"]);
        assert!(unapplied_notice(&root, max).is_some(), "the never-applied result is still reported");
    }

    #[test]
    fn there_is_no_notice_without_old_unapplied_results() {
        let root = repo();
        let max = Duration::from_secs(14 * 86400);
        assert_eq!(unapplied_notice(&root, max), None);
        old_ref(&root, "refs/mu/result/fresh", 1);
        assert_eq!(unapplied_notice(&root, max), None);
        assert_eq!(unapplied_notice(&tmp(), max), None, "outside a repository");
    }
}
