//! `mu remote list|check|install-adapter`: the configured remotes, whether their adapters answer, and the
//! adapters mu ships.

use crate::remote::adapter::Adapter;
use anyhow::{Context, Result, bail};
use mu_base::config::{Configs, MuRemote};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

const ANSWER_TIMEOUT: Duration = Duration::from_secs(10);
const POD_ADAPTER: &str = include_str!("../../scripts/remote/mu-remote-pod");
const POD_TEMPLATE: &str = include_str!("../../scripts/remote/mu-remote-pod.yaml");

#[derive(clap::Subcommand)]
pub enum Cmd {
    /// The configured remotes: adapter, whether each is trusted and whether it answers.
    List,
    /// Run an adapter's `version` and `list` and report what fails.
    Check {
        /// A `[remote.NAME]` from config.toml.
        name: String,
    },
    /// Write a shipped adapter into `~/.mu/bin/`; `pod` is the Kubernetes one.
    InstallAdapter {
        /// The adapter to install: `pod`.
        name: String,
    },
}

/// Writes `body` to `path` through a temporary file, so a running copy is never half-written.
fn write_file(path: &Path, body: &str, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&tmp, body).with_context(|| tmp.display().to_string())?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))?;
    std::fs::rename(&tmp, path).with_context(|| path.display().to_string())
}

/// Installs the adapter `name` into `dir` and returns the program's path. Running it again replaces the files.
pub fn install_adapter(name: &str, dir: &Path) -> Result<PathBuf> {
    if name != "pod" {
        bail!("unknown adapter `{name}`; mu ships `pod` (`ssh` is built in and needs no install)");
    }
    mu_common::fs::create_private_dir(dir).with_context(|| dir.display().to_string())?;
    let program = dir.join("mu-remote-pod");
    write_file(&dir.join("mu-remote-pod.yaml"), POD_TEMPLATE, 0o644)?;
    write_file(&program, POD_ADAPTER, 0o755)?;
    Ok(program)
}

pub fn run(cmd: Cmd) -> Result<i32> {
    if let Cmd::InstallAdapter { name } = &cmd {
        let program = install_adapter(name, &mu_common::paths::mu_home().join("bin"))?;
        println!("installed {}; use `adapter = \"{name}\"` in a [remote.NAME] section", program.display());
        return Ok(0);
    }
    let cfgs = Configs::load();
    for w in cfgs.warnings.iter().filter(|w| w.contains("remote")) {
        eprintln!("{w}");
    }
    let remotes = &cfgs.mu.remote;
    match cmd {
        Cmd::List => {
            print!("{}", list(remotes, cfgs.mu.default_remote.as_deref(), ANSWER_TIMEOUT));
            Ok(0)
        }
        Cmd::Check { name } => {
            let Some(r) = remotes.get(&name) else {
                bail!("unknown remote `{name}`; `mu remote list` shows the configured ones")
            };
            let (text, ok) = check(&name, r, ANSWER_TIMEOUT);
            print!("{text}");
            Ok(if ok { 0 } else { 1 })
        }
        Cmd::InstallAdapter { .. } => unreachable!("handled above"),
    }
}

fn first_line(e: &anyhow::Error) -> String {
    format!("{e:#}").lines().next().unwrap_or_default().to_string()
}

fn answers(name: &str, r: &MuRemote, timeout: Duration) -> Result<(), String> {
    let a = Adapter::from_config(name, r).map_err(|e| first_line(&e))?;
    a.version(Some(timeout)).map_err(|e| first_line(&e))?;
    a.list(Some(timeout)).map_err(|e| first_line(&e))?;
    Ok(())
}

/// The table `mu remote list` prints. Remotes are asked at the same time.
pub fn list(remotes: &BTreeMap<String, MuRemote>, default: Option<&str>, timeout: Duration) -> String {
    if remotes.is_empty() {
        return "no remotes configured: add a [remote.NAME] section to config.toml (see docs/remote.md)\n".to_string();
    }
    let rows: Vec<[String; 4]> = std::thread::scope(|s| {
        let handles: Vec<_> = remotes.iter().map(|(name, r)| s.spawn(move || answers(name, r, timeout))).collect();
        remotes
            .iter()
            .zip(handles)
            .map(|((name, r), h)| {
                let answer = match h.join() {
                    Ok(Ok(())) => "yes".to_string(),
                    Ok(Err(e)) => format!("no: {e}"),
                    Err(_) => "no: check panicked".to_string(),
                };
                let shown = if default == Some(name) { format!("{name} (default)") } else { name.clone() };
                [shown, r.adapter.clone(), if r.trusted { "yes" } else { "no" }.to_string(), answer]
            })
            .collect()
    });
    let head = ["NAME", "ADAPTER", "TRUSTED", "ANSWERS"].map(String::from);
    let width: [usize; 3] =
        std::array::from_fn(|i| rows.iter().chain([&head]).map(|r| r[i].chars().count()).max().unwrap_or(0));
    let mut out = String::new();
    for r in std::iter::once(&head).chain(&rows) {
        out.push_str(&format!(
            "{:<w0$}  {:<w1$}  {:<w2$}  {}\n",
            r[0],
            r[1],
            r[2],
            r[3],
            w0 = width[0],
            w1 = width[1],
            w2 = width[2]
        ));
    }
    out
}

/// What `mu remote check` prints, and whether everything passed. A failed `version` does not skip `list`.
pub fn check(name: &str, r: &MuRemote, timeout: Duration) -> (String, bool) {
    let mut out =
        format!("remote {name}: adapter {}, {}\n", r.adapter, if r.trusted { "trusted" } else { "not trusted" });
    let a = match Adapter::from_config(name, r) {
        Ok(a) => a,
        Err(e) => {
            out.push_str(&format!("  adapter: FAILED: {e:#}\n"));
            return (out, false);
        }
    };
    let mut ok = true;
    match a.version(Some(timeout)) {
        Ok(v) => out.push_str(&format!("  version: ok (contract {v})\n")),
        Err(e) => {
            ok = false;
            out.push_str(&format!("  version: FAILED: {e:#}\n"));
        }
    }
    match a.list(Some(timeout)) {
        Ok(l) => out.push_str(&format!("  list: ok ({} agent{})\n", l.len(), if l.len() == 1 { "" } else { "s" })),
        Err(e) => {
            ok = false;
            out.push_str(&format!("  list: FAILED: {e:#}\n"));
        }
    }
    (out, ok)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::adapter::tests::fake;

    fn program(path: &std::path::Path, trusted: bool) -> MuRemote {
        MuRemote { adapter: path.to_string_lossy().into_owned(), trusted, ..Default::default() }
    }

    #[test]
    fn install_adapter_writes_an_executable_script_and_its_template() {
        use std::os::unix::fs::PermissionsExt;
        let dir = mu_testutil::tmp().join("bin");
        let program = install_adapter("pod", &dir).unwrap();
        assert_eq!(program, dir.join("mu-remote-pod"));
        assert_eq!(std::fs::metadata(&program).unwrap().permissions().mode() & 0o111, 0o111);
        assert_eq!(std::fs::read_to_string(&program).unwrap(), POD_ADAPTER);
        assert_eq!(std::fs::read_to_string(dir.join("mu-remote-pod.yaml")).unwrap(), POD_TEMPLATE);
        std::fs::write(&program, "stale").unwrap();
        install_adapter("pod", &dir).unwrap();
        assert_eq!(std::fs::read_to_string(&program).unwrap(), POD_ADAPTER);
        assert!(install_adapter("nope", &dir).unwrap_err().to_string().contains("unknown adapter"));
    }

    #[test]
    fn check_reports_each_verb() {
        let dir = mu_testutil::tmp();
        let (text, ok) = check("far", &program(&fake(&dir, 1), false), Duration::from_secs(10));
        assert!(ok);
        assert!(text.contains("version: ok (contract 1)") && text.contains("list: ok (2 agents)"), "{text}");
        assert!(text.starts_with("remote far: adapter "), "{text}");
        assert!(text.contains("not trusted"), "{text}");

        let dir = mu_testutil::tmp();
        let (text, ok) = check("far", &program(&fake(&dir, 3), true), Duration::from_secs(10));
        assert!(!ok);
        assert!(text.contains("version: FAILED: adapter `far` speaks contract 3"), "{text}");
        assert!(text.contains("list: ok"), "{text}");

        let missing = program(&dir.join("nope"), false);
        let (text, ok) = check("far", &missing, Duration::from_secs(10));
        assert!(!ok && text.contains("version: FAILED") && text.contains("list: FAILED"), "{text}");
    }

    #[test]
    fn list_shows_adapter_trust_and_whether_it_answers() {
        let dir = mu_testutil::tmp();
        let mut remotes = BTreeMap::new();
        remotes.insert("cluster".to_string(), program(&fake(&dir, 1), true));
        remotes.insert("down".to_string(), program(&dir.join("nope"), false));
        remotes.insert("devbox".to_string(), MuRemote { adapter: "ssh".into(), host: None, ..Default::default() });
        let text = list(&remotes, Some("cluster"), Duration::from_secs(10));
        let rows: Vec<Vec<&str>> = text.lines().map(|l| l.split_whitespace().collect()).collect();
        assert_eq!(rows[0], ["NAME", "ADAPTER", "TRUSTED", "ANSWERS"]);
        assert_eq!(rows.len(), 4, "{text}");
        let (trusted, answers) = (text.find("TRUSTED").unwrap(), text.find("ANSWERS").unwrap());
        let cols = |n: &str| {
            let l = text.lines().find(|l| l.starts_with(n)).unwrap();
            (l[trusted..answers].trim(), &l[answers..])
        };
        assert_eq!(cols("cluster (default)"), ("yes", "yes"));
        assert_eq!(cols("down").0, "no");
        assert!(cols("down").1.starts_with("no: cannot run adapter"), "{text}");
        assert!(cols("devbox").1.contains("no: [remote.devbox] uses the ssh adapter and needs a host"), "{text}");
        assert_eq!(list(&BTreeMap::new(), None, Duration::from_secs(1)).lines().count(), 1);
    }
}
