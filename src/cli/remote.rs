//! `mu remote list|check`: the configured remotes and whether their adapters answer.

use crate::remote::adapter::Adapter;
use anyhow::{Result, bail};
use mu_base::config::{Configs, MuRemote};
use std::collections::BTreeMap;
use std::time::Duration;

const ANSWER_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(clap::Subcommand)]
pub enum Cmd {
    /// The configured remotes: adapter, whether each is trusted and whether it answers.
    List,
    /// Run an adapter's `version` and `list` and report what fails.
    Check {
        /// A `[remote.NAME]` from config.toml.
        name: String,
    },
}

pub fn run(cmd: Cmd) -> Result<i32> {
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
