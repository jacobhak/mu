//! Whether a launch may run on a remote. An untrusted remote runs tasks only for the repositories its `repos`
//! list, and only while the config mu would have sent (the parent's `agents` and `skills`) holds nothing that
//! looks like a credential. The adapter of an untrusted remote also gets no secret in its environment
//! (`adapter::scrub_env`), and nothing secret is in the payload or the bundle.

use crate::config::Remote;
use std::path::{Path, PathBuf};

/// Why the task of the repository at `root` may not run on remote `name`, if it may not. `agent_dir` is the
/// parent's omp agent directory (`MU_TASK_AGENT_DIR`).
pub fn admit(name: &str, remote: &Remote, root: &Path, agent_dir: &Path) -> Result<(), String> {
    check(name, remote.trusted, &remote.repos, root, &[agent_dir.join("agents"), agent_dir.join("skills")])
}

/// A repository as `repos` names it: a path or an origin URL, without a trailing slash or `.git`.
fn normal(s: &str) -> String {
    let s = s.trim().trim_end_matches('/');
    s.strip_suffix(".git").unwrap_or(s).to_string()
}

fn origin_url(root: &Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(root)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    let url = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !url.is_empty()).then_some(url)
}

fn listed(repos: &[String], root: &Path, origin: Option<&str>) -> bool {
    let canonical = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    repos.iter().any(|r| {
        let r = normal(r);
        r == normal(&root.to_string_lossy())
            || std::fs::canonicalize(&r).is_ok_and(|p| p == canonical)
            || origin.is_some_and(|o| normal(o) == r)
    })
}

/// Whether a file name looks like it holds a credential.
fn credential_name(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n.starts_with(".env")
        || n.ends_with(".pem")
        || n.ends_with(".key")
        || n.starts_with("id_")
        || ["credential", "secret", "token"].iter().any(|w| n.contains(w))
}

/// The credential-looking files under `dir`, through symlinks, as paths relative to `dir`.
fn credential_files(dir: &Path) -> Vec<String> {
    fn walk(root: &Path, dir: &Path, depth: u32, seen: &mut usize, out: &mut Vec<String>) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            *seen += 1;
            if *seen > 20_000 {
                return;
            }
            let path = e.path();
            let name = e.file_name().to_string_lossy().into_owned();
            if credential_name(&name) {
                out.push(path.strip_prefix(root).unwrap_or(&path).display().to_string());
            } else if depth < 8 && std::fs::metadata(&path).is_ok_and(|m| m.is_dir()) {
                walk(root, &path, depth + 1, seen, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, 0, &mut 0, &mut out);
    out.sort();
    out
}

fn check(name: &str, trusted: bool, repos: &[String], root: &Path, dirs: &[PathBuf]) -> Result<(), String> {
    if trusted {
        return Ok(());
    }
    let origin = origin_url(root);
    let shown = origin.clone().unwrap_or_else(|| root.display().to_string());
    if !listed(repos, root, origin.as_deref()) {
        return Err(format!("remote {name} is not trusted for {shown}; list it in [remote.{name}] repos to allow it"));
    }
    let found: Vec<String> = dirs
        .iter()
        .flat_map(|d| credential_files(d).into_iter().map(move |f| d.join(f).display().to_string()))
        .collect();
    if !found.is_empty() {
        return Err(format!(
            "remote {name} is not trusted and the config sent to it would carry credential-looking files: {}; \
             move them out of the agents and skills directories",
            found.join(", ")
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{repo, sh, tmp};

    fn check_far(trusted: bool, repos: &[&str], root: &Path, dirs: &[PathBuf]) -> Result<(), String> {
        let repos: Vec<String> = repos.iter().map(|r| r.to_string()).collect();
        check("far", trusted, &repos, root, dirs)
    }

    fn repo_with_origin(url: &str) -> PathBuf {
        let root = repo();
        sh(&root, &["remote", "add", "origin", url]);
        root
    }

    #[test]
    fn a_trusted_remote_takes_any_repository() {
        assert!(check_far(true, &[], &repo(), &[]).is_ok());
    }

    #[test]
    fn an_untrusted_remote_takes_only_listed_repositories() {
        let root = repo_with_origin("git@example.com:team/app.git");
        let e = check_far(false, &[], &root, &[]).unwrap_err();
        assert!(e.starts_with("remote far is not trusted for git@example.com:team/app.git"), "{e}");
        let e = check_far(false, &["/elsewhere"], &root, &[]).unwrap_err();
        assert!(e.contains("is not trusted for"), "{e}");
        let by_path = root.to_string_lossy().into_owned();
        assert!(check_far(false, &[by_path.as_str()], &root, &[]).is_ok());
        assert!(check_far(false, &[&format!("{by_path}/")], &root, &[]).is_ok());
        assert!(check_far(false, &["git@example.com:team/app"], &root, &[]).is_ok(), "an origin, with or without .git");
        let plain = repo();
        let e = check_far(false, &[], &plain, &[]).unwrap_err();
        assert!(e.contains(&plain.display().to_string()), "no origin: the path is named: {e}");
    }

    #[test]
    fn credential_looking_files_in_what_is_sent_refuse_the_launch() {
        let root = repo_with_origin("https://example.com/app");
        let dir = tmp();
        std::fs::create_dir_all(dir.join("agents/sub")).unwrap();
        std::fs::write(dir.join("agents/ok.md"), "x").unwrap();
        let repos = ["https://example.com/app"];
        assert!(check_far(false, &repos, &root, &[dir.join("agents")]).is_ok());
        for name in [
            ".env",
            ".env.local",
            "deploy.pem",
            "tls.key",
            "id_ed25519",
            "my-credentials.json",
            "client_secret.txt",
            "api-token",
        ] {
            let f = dir.join("agents/sub").join(name);
            std::fs::write(&f, "x").unwrap();
            let e = check_far(false, &repos, &root, &[dir.join("agents")]).unwrap_err();
            assert!(e.contains(&f.display().to_string()), "{name}: {e}");
            std::fs::remove_file(&f).unwrap();
        }
        assert!(check_far(false, &repos, &root, &[dir.join("agents"), dir.join("missing")]).is_ok());
    }

    #[test]
    fn admit_reads_the_agents_and_skills_of_the_agent_dir() {
        let root = repo();
        let agent = tmp();
        let remote = Remote { trusted: false, repos: vec![root.display().to_string()], ..Default::default() };
        assert!(admit("far", &remote, &root, &agent).is_ok());
        std::fs::create_dir_all(agent.join("skills")).unwrap();
        std::fs::write(agent.join("skills/.env"), "X=1").unwrap();
        let e = admit("far", &remote, &root, &agent).unwrap_err();
        assert!(e.contains("credential-looking files") && e.contains("skills/.env"), "{e}");
    }
}
