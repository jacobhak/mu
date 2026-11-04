//! Branch and working-tree counts from one `git status --porcelain=v2 --branch`.
//! Runs off the draw path (the agent calls it after each turn).
use crate::events::GitInfo;
use std::path::Path;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(2);

/// `None` when `cwd` is not a repository, git is missing, or it does not finish in time.
pub async fn git_status(cwd: &Path) -> Option<GitInfo> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.args(["status", "--porcelain=v2", "--branch"])
        .current_dir(cwd)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let out = tokio::time::timeout(TIMEOUT, cmd.output()).await.ok()?.ok()?;
    out.status.success().then(|| parse_porcelain_v2(&String::from_utf8_lossy(&out.stdout)))
}

pub fn parse_porcelain_v2(out: &str) -> GitInfo {
    let mut info = GitInfo::default();
    let mut oid = None;
    let mut detached = false;
    for line in out.lines() {
        if let Some(head) = line.strip_prefix("# branch.head ") {
            detached = head == "(detached)";
            if !detached && head != "(initial)" {
                info.branch = Some(head.to_string());
            }
        } else if let Some(o) = line.strip_prefix("# branch.oid ") {
            oid = Some(o.to_string());
        } else if let Some(ab) = line.strip_prefix("# branch.ab ") {
            let mut it = ab.split_whitespace();
            info.ahead = it.next().and_then(|a| a.strip_prefix('+')).and_then(|a| a.parse().ok()).unwrap_or(0);
            info.behind = it.next().and_then(|b| b.strip_prefix('-')).and_then(|b| b.parse().ok()).unwrap_or(0);
        } else if line.starts_with("1 ") || line.starts_with("2 ") {
            let xy = line.as_bytes().get(2..4).unwrap_or(b"..");
            info.staged += u32::from(xy[0] != b'.');
            info.dirty += u32::from(xy[1] != b'.');
        } else if line.starts_with("u ") {
            info.dirty += 1;
        } else if line.starts_with("? ") {
            info.untracked += 1;
        }
    }
    if detached {
        info.branch = oid.map(|o| o.chars().take(7).collect());
    }
    info
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_branch_and_counts() {
        let out = "# branch.oid 0123456789abcdef\n# branch.head main\n# branch.upstream origin/main\n# branch.ab +2 -1\n\
            1 .M N... 100644 100644 100644 a b src/a.rs\n\
            1 M. N... 100644 100644 100644 a b src/b.rs\n\
            1 MM N... 100644 100644 100644 a b src/c.rs\n\
            2 R. N... 100644 100644 100644 a b R100 new old\n\
            u UU N... 100644 100644 100644 100644 a b c d conflict\n\
            ? notes.md\n? scratch/\n! ignored\n";
        let g = parse_porcelain_v2(out);
        assert_eq!(g.branch.as_deref(), Some("main"));
        assert_eq!((g.ahead, g.behind), (2, 1));
        assert_eq!((g.staged, g.dirty, g.untracked), (3, 3, 2));
    }

    #[test]
    fn detached_head_uses_short_oid_and_initial_has_no_branch() {
        let g = parse_porcelain_v2("# branch.oid 0123456789abcdef\n# branch.head (detached)\n");
        assert_eq!(g.branch.as_deref(), Some("0123456"));
        let g = parse_porcelain_v2("# branch.oid (initial)\n# branch.head (initial)\n? a\n");
        assert_eq!(g.branch, None);
        assert_eq!(g.untracked, 1);
    }
}
