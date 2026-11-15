//! Mirrored sessions: the local copy of a launched worker's session. Its files live on the worker,
//! so resuming one means checking out what the worker produced (`refs/mu/result/<id>`, else the
//! snapshot `refs/mu/task/<id>`) into `<repo>/.mu-worktrees/<id>` and working there.
use serde::Deserialize;
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::process::Command;

const WORKTREES: &str = ".mu-worktrees";

/// The header fields a resume looks at.
#[derive(Deserialize)]
pub struct Header {
    #[serde(default)]
    pub mirrored: bool,
    #[serde(default, rename = "launchId")]
    pub launch_id: Option<String>,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
}

/// The header of the session file at `path`, from its first lines.
pub fn header(path: &Path) -> Option<Header> {
    let f = std::io::BufReader::new(std::fs::File::open(path).ok()?);
    f.lines().map_while(Result::ok).take(4).find_map(|l| {
        let v: serde_json::Value = serde_json::from_str(&l).ok()?;
        if v["type"] != "session" { None } else { serde_json::from_value(v).ok() }
    })
}

/// Whether `dir` is, or is inside, the worktree of mirrored session `path`.
pub fn is_in_worktree(path: &Path, dir: &Path) -> bool {
    let Some(id) = header(path).filter(|h| h.mirrored).and_then(|h| h.launch_id) else { return false };
    dir.ancestors().any(|a| {
        a.file_name().is_some_and(|n| n == id.as_str())
            && a.parent().and_then(Path::file_name).is_some_and(|n| n == WORKTREES)
    })
}

fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output().map_err(|e| format!("cannot run git: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// `Ok(None)`: the session is not a mirror, open it where it is. Otherwise the worktree to open it in,
/// created at the result ref, or at the snapshot when the run left no result; one already there is reused.
pub fn checkout(path: &Path) -> Result<Option<PathBuf>, String> {
    let Some(h) = header(path).filter(|h| h.mirrored) else { return Ok(None) };
    let id = h.launch_id.filter(|i| valid_id(i)).ok_or("the mirrored session's header names no valid task id")?;
    let cwd = h.cwd.filter(|d| d.is_dir()).ok_or("the project the task was launched from no longer exists")?;
    let common = git(&cwd, &["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .map_err(|e| format!("the task's project is not a git repository: {e}"))?;
    let common = PathBuf::from(common);
    let repo = common.parent().ok_or("the task's repository has no working directory")?;
    let dir = repo.join(WORKTREES).join(&id);
    if dir.is_dir() {
        return Ok(Some(dir));
    }
    let found = [format!("refs/mu/result/{id}"), format!("refs/mu/task/{id}")]
        .into_iter()
        .find(|r| git(repo, &["rev-parse", "--verify", "-q", &format!("{r}^{{commit}}")]).is_ok())
        .ok_or_else(|| {
            format!(
                "the result and the snapshot of task {id} are gone (refs/mu/result/{id}, refs/mu/task/{id}); \
                 there is nothing to check out"
            )
        })?;
    exclude(&common)?;
    let _ = git(repo, &["worktree", "prune"]);
    let target = dir.to_string_lossy().into_owned();
    git(repo, &["worktree", "add", "--detach", &target, &found])
        .map_err(|e| format!("git worktree add {target} {found}: {e}"))?;
    Ok(Some(dir))
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && !id.starts_with(['-', '.'])
        && id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// Keeps `.mu-worktrees/` out of the repository's untracked files without touching its `.gitignore`.
fn exclude(common: &Path) -> Result<(), String> {
    let file = common.join("info").join("exclude");
    let entry = format!("/{WORKTREES}/");
    let old = std::fs::read_to_string(&file).unwrap_or_default();
    if old.lines().any(|l| l.trim() == entry) {
        return Ok(());
    }
    let mut new = old;
    if !new.is_empty() && !new.ends_with('\n') {
        new.push('\n');
    }
    new.push_str(&entry);
    new.push('\n');
    let err = |e: std::io::Error| format!("{}: {e}", file.display());
    std::fs::create_dir_all(file.parent().unwrap_or(common)).map_err(err)?;
    std::fs::write(&file, new).map_err(err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::file::SessionFile;
    use mu_testutil::home;

    fn sh(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .current_dir(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// A repository with commit 1 (`a`: one) and a second commit (`a`: two) not on any branch.
    fn repo() -> (PathBuf, String, String) {
        let d = std::env::temp_dir().join(format!("mu-mirror-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        let d = std::fs::canonicalize(&d).unwrap();
        sh(&d, &["init", "-q"]);
        std::fs::write(d.join("a"), "one").unwrap();
        sh(&d, &["add", "a"]);
        sh(&d, &["commit", "-qm", "one"]);
        let one = sh(&d, &["rev-parse", "HEAD"]);
        std::fs::write(d.join("a"), "two").unwrap();
        sh(&d, &["commit", "-qam", "two"]);
        let two = sh(&d, &["rev-parse", "HEAD"]);
        sh(&d, &["reset", "-q", "--hard", &one]);
        (d, one, two)
    }

    /// A session file with a header like the one the parent writes for a launched worker.
    fn mirror(repo: &Path, id: &str) -> PathBuf {
        home();
        let s = SessionFile::create(repo, None).unwrap();
        let path = s.path.clone();
        drop(s);
        let text = std::fs::read_to_string(&path).unwrap();
        let (slot, header) = text.split_at(256);
        let mut v: serde_json::Value = serde_json::from_str(header).unwrap();
        v["mirrored"] = true.into();
        v["launchId"] = id.into();
        std::fs::write(&path, format!("{slot}{v}\n")).unwrap();
        path
    }

    #[test]
    fn the_header_reads_with_and_without_the_mirrored_flag() {
        home();
        let s = SessionFile::create(Path::new("/opt/plain"), None).unwrap();
        let plain = header(&s.path).unwrap();
        assert!(!plain.mirrored && plain.launch_id.is_none());
        assert_eq!(plain.cwd.as_deref(), Some(Path::new("/opt/plain")));
        let m = mirror(Path::new("/opt/flagged"), "t-1");
        let h = header(&m).unwrap();
        assert!(h.mirrored);
        assert_eq!(h.launch_id.as_deref(), Some("t-1"));
        assert_eq!(checkout(&s.path), Ok(None), "an ordinary session opens where it is");
    }

    #[test]
    fn a_mirror_resumes_in_a_worktree_at_the_result() {
        let (repo, one, two) = repo();
        sh(&repo, &["update-ref", "refs/mu/result/t-1", &two]);
        sh(&repo, &["update-ref", "refs/mu/task/t-1", &one]);
        let path = mirror(&repo, "t-1");
        let dir = checkout(&path).unwrap().unwrap();
        assert_eq!(dir, repo.join(".mu-worktrees/t-1"));
        assert_eq!(std::fs::read_to_string(dir.join("a")).unwrap(), "two");
        assert_eq!(sh(&dir, &["rev-parse", "HEAD"]), two);
        assert_eq!(sh(&repo, &["status", "--porcelain"]), "", "the worktrees are not untracked noise");
        assert!(is_in_worktree(&path, &dir.join("sub")));
        assert!(!is_in_worktree(&path, &repo));
    }

    #[test]
    fn without_a_result_it_falls_back_to_the_snapshot() {
        let (repo, one, _) = repo();
        sh(&repo, &["update-ref", "refs/mu/task/t-2", &one]);
        let dir = checkout(&mirror(&repo, "t-2")).unwrap().unwrap();
        assert_eq!(sh(&dir, &["rev-parse", "HEAD"]), one);
    }

    #[test]
    fn with_both_refs_gone_it_refuses() {
        let (repo, _, _) = repo();
        let e = checkout(&mirror(&repo, "t-3")).unwrap_err();
        assert!(e.contains("are gone"), "{e}");
        assert!(!repo.join(".mu-worktrees/t-3").exists());
    }

    #[test]
    fn an_existing_worktree_is_reused() {
        let (repo, one, two) = repo();
        sh(&repo, &["update-ref", "refs/mu/result/t-4", &two]);
        let path = mirror(&repo, "t-4");
        let dir = checkout(&path).unwrap().unwrap();
        std::fs::write(dir.join("kept"), "mine").unwrap();
        sh(&repo, &["update-ref", "refs/mu/result/t-4", &one]);
        assert_eq!(checkout(&path).unwrap().unwrap(), dir);
        assert_eq!(std::fs::read_to_string(dir.join("kept")).unwrap(), "mine");
        assert_eq!(sh(&dir, &["rev-parse", "HEAD"]), two, "left as it was");
    }

    #[test]
    fn an_id_that_could_leave_the_worktree_directory_is_refused() {
        let (repo, _, _) = repo();
        let e = checkout(&mirror(&repo, "../evil")).unwrap_err();
        assert!(e.contains("no valid task id"), "{e}");
    }
}
