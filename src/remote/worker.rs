//! Workers on a remote: `[task] launch` over an adapter, and the trust rules for sending a repository.
//!
//! An untrusted remote runs workers only for the repositories its `repos` list, and only while what mu
//! would send of the profile (its `agents` and `skills`) holds nothing that looks like a credential.
//! `auth.json` and token files are never sent, and the adapter gets no secret in its environment.

use super::adapter::{Adapter, Start};
use anyhow::Result;
use mu_base::config::MuConfig;
use mu_tools::tools::link::{WorkerLaunch, WorkerRemote};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::Arc;
use std::time::Duration;

pub struct RemoteWorkers {
    adapter: Adapter,
    name: String,
    trusted: bool,
    repos: Vec<String>,
}

/// The remote `[task] launch` runs on, or `None` when no kind launches. `local` is `--local`: it
/// keeps `default_remote` from taking workers, but not an explicit `[task] remote`.
pub fn launch(cfg: &MuConfig, local: bool) -> Result<Option<WorkerLaunch>, String> {
    if cfg.task.launch.is_empty() {
        return Ok(None);
    }
    let name = match cfg.task.remote.as_deref().or(if local { None } else { cfg.default_remote.as_deref() }) {
        Some(n) => n,
        None if local => return Ok(None),
        None => return Err("[task] launch needs a remote: set [task] remote or default_remote".into()),
    };
    let Some(remote) = cfg.remote.get(name) else {
        return Err(format!("[task] remote `{name}` is not a configured remote"));
    };
    if remote.adapter.trim() == "ssh" {
        return Err(format!("remote `{name}` uses the ssh adapter, which cannot run workers; use another adapter"));
    }
    let adapter = Adapter::from_config(name, remote).map_err(|e| format!("{e:#}"))?;
    Ok(Some(WorkerLaunch {
        remote: Arc::new(RemoteWorkers {
            adapter,
            name: name.to_string(),
            trusted: remote.trusted,
            repos: remote.repos.clone(),
        }),
        kinds: cfg.task.launch.clone(),
        reconnect: Duration::from_secs(cfg.task.reconnect_timeout),
    }))
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

/// What mu would put in the config it sends to a remote: the profile's agents and skills.
fn sent_dirs() -> Vec<PathBuf> {
    let agent = mu_common::paths::omp_agent_dir();
    vec![agent.join("agents"), agent.join("skills")]
}

/// Removes everything whose name looks like it holds a secret from the environment `cmd` gets: an
/// adapter for an untrusted remote must not be able to pass one on. `SSH_AUTH_SOCK` stays: it names
/// a socket, not a key.
pub(super) fn scrub_env(cmd: &mut std::process::Command) {
    for (k, _) in std::env::vars_os() {
        let name = k.to_string_lossy().to_ascii_uppercase();
        let secret =
            ["KEY", "TOKEN", "SECRET", "PASSWORD", "PASSWD", "CREDENTIAL", "AUTH"].iter().any(|w| name.contains(w));
        if secret && name != "SSH_AUTH_SOCK" {
            cmd.env_remove(&k);
        }
    }
}

impl RemoteWorkers {
    fn check(&self, root: &Path, dirs: &[PathBuf]) -> Result<(), String> {
        if self.trusted {
            return Ok(());
        }
        let origin = origin_url(root);
        let shown = origin.clone().unwrap_or_else(|| root.display().to_string());
        if !listed(&self.repos, root, origin.as_deref()) {
            return Err(format!(
                "remote {} is not trusted for {shown}; list it in [remote.{}] repos to allow it",
                self.name, self.name
            ));
        }
        let found: Vec<String> = dirs
            .iter()
            .flat_map(|d| credential_files(d).into_iter().map(move |f| d.join(f).display().to_string()))
            .collect();
        if !found.is_empty() {
            return Err(format!(
                "remote {} is not trusted and the config sent to it would carry credential-looking files: {}; \
                 move them out of the agents and skills directories",
                self.name,
                found.join(", ")
            ));
        }
        Ok(())
    }
}

fn text(e: anyhow::Error) -> String {
    format!("{e:#}")
}

impl WorkerRemote for RemoteWorkers {
    fn name(&self) -> &str {
        &self.name
    }

    fn admit(&self, root: &Path) -> Result<(), String> {
        self.check(root, &sent_dirs())
    }

    fn start(&self, launch: &str, bundle: &Path, dir: &str, progress: &mut dyn FnMut(&str)) -> Result<String, String> {
        let what = Start::Worker { launch: launch.to_string(), bundle: bundle.to_path_buf(), dir: dir.to_string() };
        self.adapter.start(&what, progress).map_err(text)
    }

    fn connect(&self, id: &str) -> Result<Child, String> {
        self.adapter.connect(id).map_err(text)
    }

    fn fetch(&self, id: &str, out: &mut (dyn Write + Send)) -> Result<(), String> {
        self.adapter.fetch(id, out).map_err(text)
    }

    fn stop(&self, id: &str) -> Result<(), String> {
        self.adapter.stop(id).map_err(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mu_base::config::MuRemote;

    fn workers(trusted: bool, repos: &[&str]) -> RemoteWorkers {
        let remote = MuRemote { adapter: "/bin/adapter".into(), trusted, ..Default::default() };
        RemoteWorkers {
            adapter: Adapter::from_config("far", &remote).unwrap(),
            name: "far".into(),
            trusted,
            repos: repos.iter().map(|r| r.to_string()).collect(),
        }
    }

    fn repo_with_origin(url: &str) -> PathBuf {
        let root = mu_testutil::repo();
        mu_testutil::sh(&root, &["remote", "add", "origin", url]);
        root
    }

    #[test]
    fn a_trusted_remote_takes_any_repository() {
        assert!(workers(true, &[]).check(&mu_testutil::repo(), &[]).is_ok());
    }

    #[test]
    fn an_untrusted_remote_takes_only_listed_repositories() {
        let root = repo_with_origin("git@example.com:team/app.git");
        let e = workers(false, &[]).check(&root, &[]).unwrap_err();
        assert!(e.starts_with("remote far is not trusted for git@example.com:team/app.git"), "{e}");
        let e = workers(false, &["/elsewhere"]).check(&root, &[]).unwrap_err();
        assert!(e.contains("is not trusted for"), "{e}");
        let by_path = root.to_string_lossy().into_owned();
        assert!(workers(false, &[by_path.as_str()]).check(&root, &[]).is_ok());
        assert!(workers(false, &[&format!("{by_path}/")]).check(&root, &[]).is_ok());
        assert!(
            workers(false, &["git@example.com:team/app"]).check(&root, &[]).is_ok(),
            "an origin, with or without .git"
        );
        let plain = mu_testutil::repo();
        let e = workers(false, &[]).check(&plain, &[]).unwrap_err();
        assert!(e.contains(&plain.display().to_string()), "no origin: the path is named: {e}");
    }

    #[test]
    fn credential_looking_files_in_what_is_sent_refuse_the_launch() {
        let root = repo_with_origin("https://example.com/app");
        let dir = mu_testutil::tmp();
        std::fs::create_dir_all(dir.join("agents/sub")).unwrap();
        std::fs::write(dir.join("agents/ok.md"), "x").unwrap();
        let w = workers(false, &["https://example.com/app"]);
        assert!(w.check(&root, &[dir.join("agents")]).is_ok());
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
            let e = w.check(&root, &[dir.join("agents")]).unwrap_err();
            assert!(e.contains(&f.display().to_string()), "{name}: {e}");
            std::fs::remove_file(&f).unwrap();
        }
        assert!(w.check(&root, &[dir.join("agents"), dir.join("missing")]).is_ok());
    }

    #[test]
    fn the_environment_of_an_adapter_loses_what_looks_like_a_secret() {
        // SAFETY: the variable names are unique to this test.
        unsafe {
            std::env::set_var("MU_SCRUB_TEST_API_KEY", "s1");
            std::env::set_var("MU_SCRUB_TEST_TOKEN", "s2");
            std::env::set_var("MU_SCRUB_TEST_PLAIN", "p");
        }
        let mut cmd = std::process::Command::new("env");
        scrub_env(&mut cmd);
        let out = cmd.output().unwrap();
        let env = String::from_utf8_lossy(&out.stdout);
        assert!(!env.contains("MU_SCRUB_TEST_API_KEY") && !env.contains("MU_SCRUB_TEST_TOKEN"), "{env}");
        assert!(env.contains("MU_SCRUB_TEST_PLAIN=p"), "{env}");
    }

    #[test]
    fn launch_follows_the_config() {
        let mut cfg = MuConfig::default();
        assert!(launch(&cfg, false).unwrap().is_none(), "nothing listed, nothing launches");
        cfg.task.launch = vec!["task".into()];
        assert!(launch(&cfg, false).unwrap_err().contains("needs a remote"));
        assert!(launch(&cfg, true).unwrap().is_none(), "--local with no explicit remote stays here");
        cfg.default_remote = Some("far".into());
        assert!(launch(&cfg, false).unwrap_err().contains("not a configured remote"));
        cfg.remote.insert("far".into(), MuRemote { adapter: "/bin/adapter".into(), ..Default::default() });
        cfg.task.reconnect_timeout = 5;
        let l = launch(&cfg, false).unwrap().unwrap();
        assert!(l.runs("task") && !l.runs("scout") && l.reconnect == Duration::from_secs(5));
        assert_eq!(l.remote.name(), "far");
        assert!(launch(&cfg, true).unwrap().is_none(), "--local overrides default_remote");
        cfg.task.remote = Some("far".into());
        assert!(launch(&cfg, true).unwrap().is_some(), "an explicit [task] remote stays");
        cfg.remote
            .insert("dev".into(), MuRemote { adapter: "ssh".into(), host: Some("h".into()), ..Default::default() });
        cfg.task.remote = Some("dev".into());
        assert!(launch(&cfg, false).unwrap_err().contains("cannot run workers"));
    }
}
