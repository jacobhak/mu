//! The memory directory as a git repository, driven through `mu_base::git::git`.

use mu_base::git::git;
use std::path::Path;

/// Initialises the repository on first use. `false` when git is unavailable: writes still apply, without history.
pub async fn ensure(root: &Path) -> bool {
    if root.join(".git").exists() {
        return true;
    }
    if git(root, &["init", "-q"], None).await.is_err() {
        return false;
    }
    let _ = std::fs::write(root.join(".gitignore"), ".state/\n");
    for (k, v) in [("user.name", "mu memory"), ("user.email", "mu-memory@localhost"), ("commit.gpgsign", "false")] {
        let _ = git(root, &["config", "--local", k, v], None).await;
    }
    let _ = commit_all(root, "init").await;
    true
}

/// Commits everything under `root` (`.state/` is ignored). `None` when nothing changed.
pub async fn commit_all(root: &Path, message: &str) -> Result<Option<String>, String> {
    git(root, &["add", "-A"], None).await?;
    if git(root, &["status", "--porcelain"], None).await?.trim().is_empty() {
        return Ok(None);
    }
    git(root, &["-c", "commit.gpgsign=false", "commit", "-q", "--no-verify", "-m", message], None).await?;
    let sha = git(root, &["rev-parse", "HEAD"], None).await?;
    Ok(Some(sha.trim().to_string()))
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct Commit {
    pub sha: String,
    pub time: String,
    pub message: String,
    pub files: Vec<String>,
}

pub async fn log(root: &Path, n: usize) -> Result<Vec<Commit>, String> {
    if !has_commits(root).await {
        return Ok(Vec::new());
    }
    let n = format!("-n{n}");
    let out = git(root, &["log", &n, "--name-only", "--format=%x1e%H%x1f%aI%x1f%s"], None).await?;
    Ok(parse_log(&out))
}

fn parse_log(out: &str) -> Vec<Commit> {
    out.split('\u{1e}')
        .filter(|r| !r.trim().is_empty())
        .filter_map(|rec| {
            let (head, files) = rec.split_once('\n').unwrap_or((rec, ""));
            let mut f = head.split('\u{1f}');
            let (sha, time, message) = (f.next()?, f.next()?, f.next()?);
            let files = files.lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect();
            Some(Commit { sha: sha.into(), time: time.into(), message: message.into(), files })
        })
        .collect()
}

pub async fn has_commits(root: &Path) -> bool {
    git(root, &["rev-parse", "--verify", "-q", "HEAD"], None).await.is_ok()
}

/// The commit's message, files and diff.
pub async fn show(root: &Path, sha: &str) -> Result<String, String> {
    check_sha(sha)?;
    git(root, &["show", "--format=%s%n%aI%n", "--stat", "--patch", sha], None).await
}

/// `git revert` of one commit. A conflict aborts the revert and is reported.
pub async fn revert(root: &Path, sha: &str) -> Result<(), String> {
    check_sha(sha)?;
    match git(root, &["-c", "commit.gpgsign=false", "revert", "--no-edit", sha], None).await {
        Ok(_) => Ok(()),
        Err(e) => {
            let _ = git(root, &["revert", "--abort"], None).await;
            Err(format!(
                "cannot revert {}: {e} (a later change touches the same lines; edit the file by hand)",
                short(sha)
            ))
        }
    }
}

fn short(sha: &str) -> &str {
    mu_base::git::short_sha(sha)
}

fn check_sha(sha: &str) -> Result<(), String> {
    if sha.len() >= 4 && sha.len() <= 64 && sha.chars().all(|c| c.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(format!("not a commit id: {sha:?}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_log_format() {
        let out = "\u{1e}abc\u{1f}2026-01-01T00:00:00+00:00\u{1f}tool s1: add user\n\nuser.md\nprojects/x/memory.md\n\u{1e}def\u{1f}t2\u{1f}manual edit\n\nuser.md\n";
        let c = parse_log(out);
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].message, "tool s1: add user");
        assert_eq!(c[0].files, ["user.md", "projects/x/memory.md"]);
        assert_eq!(c[1].sha, "def");
    }

    #[test]
    fn rejects_non_hex_ids() {
        assert!(check_sha("abc123").is_ok());
        assert!(check_sha("--upload-pack=x").is_err());
        assert!(check_sha("").is_err());
    }
}
