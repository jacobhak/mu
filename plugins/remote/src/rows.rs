//! `list`, `check` and `rows`.

use crate::adapter::{Adapter, AgentInfo};
use crate::config::{Config, Remote};
use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

pub(crate) const ANSWER_TIMEOUT: Duration = Duration::from_secs(10);

fn first_line(e: &anyhow::Error) -> String {
    format!("{e:#}").lines().next().unwrap_or_default().to_string()
}

fn answers(name: &str, r: &Remote, timeout: Duration) -> Result<(), String> {
    let a = Adapter::from_config(name, r).map_err(|e| first_line(&e))?;
    a.version(Some(timeout)).map_err(|e| first_line(&e))?;
    a.list(Some(timeout)).map_err(|e| first_line(&e))?;
    Ok(())
}

/// The table `mu-remote list` prints. Remotes are asked at the same time.
fn table(remotes: &BTreeMap<String, Remote>, default: Option<&str>, timeout: Duration) -> String {
    if remotes.is_empty() {
        return format!(
            "no remotes configured: add a [remote.NAME] section to {}\n",
            crate::paths::config_file().display()
        );
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

pub fn list() -> Result<i32> {
    let cfg = Config::load()?;
    print!("{}", table(&cfg.remote, cfg.default_remote.as_deref(), ANSWER_TIMEOUT));
    Ok(0)
}

/// What `mu-remote check` prints, and whether everything passed. A failed `version` does not skip `list`.
fn report(name: &str, r: &Remote, timeout: Duration) -> (String, bool) {
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

/// `check`, and with `full` the conformance steps after it (see `conformance`).
pub fn check(name: &str, full: bool, agent: Option<&str>) -> Result<i32> {
    let cfg = Config::load()?;
    let Some(r) = cfg.remote.get(name) else {
        bail!("unknown remote `{name}`; `mu-remote list` shows the configured ones")
    };
    let (text, mut ok) = report(name, r, ANSWER_TIMEOUT);
    print!("{text}");
    // An adapter that cannot be built was reported as such above.
    if full && let Ok(adapter) = Adapter::from_config(name, r) {
        let (steps, passed) = crate::conformance::full(&adapter, agent);
        print!("{steps}");
        ok &= passed;
    }
    Ok(if ok { 0 } else { 1 })
}

/// The sources contract this program speaks (docs/remote-contracts.md, D).
const SOURCES_CONTRACT: u32 = 2;

/// One session-source row (sources contract 2): the agent's fields plus `host`, `hint`, and `attach`, `helper`,
/// `stop`, `cancel` and `notify` as argument vectors, which mu runs without a shell, so an id never reaches one.
/// `state_key` is the key file `mu --attach` unlocks the agent with, only when the remote's config names one.
fn row(remote: &str, exe: &Path, a: &AgentInfo, state_key: Option<&Path>) -> Value {
    let exe = exe.to_string_lossy();
    let mut v = json!({
        "id": a.id,
        "cwd": a.cwd,
        "started": a.started,
        "title": a.title,
        "client": a.client.as_ref().map(|c| json!({"host": c.host, "since": c.since})),
        "waiting": a.waiting,
        "locked": a.locked,
        "host": remote,
        "hint": format!("mu-remote {remote} --resume {}", a.id),
        "attach": [exe, "connect", remote, a.id],
        "helper": [exe, "helper", remote, a.id],
        "stop": [exe, "stop", remote, a.id],
        "cancel": [exe, "cancel", remote, a.id],
        "notify": [exe, "notify", remote, a.id, "-"],
    });
    if let Some(k) = state_key {
        v["state_key"] = json!(k.to_string_lossy());
    }
    v
}

/// Refuses a mu that offers another sources contract number than this program speaks.
fn check_contract(offered: Option<&str>) -> Result<()> {
    match offered {
        Some(n) if n != SOURCES_CONTRACT.to_string() => {
            bail!("mu-remote rows speaks sources contract {SOURCES_CONTRACT}; this mu offers {n}")
        }
        _ => Ok(()),
    }
}

fn rows_of(name: &str, r: &Remote, exe: &Path) -> Vec<Value> {
    let Ok(adapter) = Adapter::from_config(name, r) else { return Vec::new() };
    if !adapter.trusted() {
        return Vec::new();
    }
    let Ok(found) = adapter.list(Some(ANSWER_TIMEOUT)) else { return Vec::new() };
    let key = r.state_key.is_some().then(|| adapter.state_key_path());
    found.iter().filter(|a| a.is_agent()).map(|a| row(name, exe, a, key.as_deref())).collect()
}

/// A `[sessions] sources` command. A remote that is unreachable or not trusted adds no rows.
pub fn rows(name: Option<&str>) -> Result<i32> {
    check_contract(std::env::var("MU_SOURCES_CONTRACT").ok().as_deref())?;
    let cfg = Config::load()?;
    let exe = std::env::current_exe()?;
    let chosen: Vec<(&String, &Remote)> = match name {
        Some(n) => match cfg.remote.get_key_value(n) {
            Some(kv) => vec![kv],
            None => bail!("unknown remote `{n}`"),
        },
        None => cfg.remote.iter().collect(),
    };
    let found: Vec<Vec<Value>> = std::thread::scope(|s| {
        let handles: Vec<_> = chosen.iter().map(|(n, r)| s.spawn(|| rows_of(n, r, &exe))).collect();
        handles.into_iter().map(|h| h.join().unwrap_or_default()).collect()
    });
    for v in found.into_iter().flatten() {
        println!("{v}");
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::tests::fake;

    fn program(path: &Path, trusted: bool) -> Remote {
        Remote { adapter: path.to_string_lossy().into_owned(), trusted, ..Default::default() }
    }

    #[test]
    fn check_reports_each_verb() {
        let dir = crate::testutil::tmp();
        let (text, ok) = report("far", &program(&fake(&dir, 1), false), Duration::from_secs(10));
        assert!(ok);
        assert!(text.contains("version: ok (contract 1)") && text.contains("list: ok (2 agents)"), "{text}");
        assert!(text.starts_with("remote far: adapter "), "{text}");
        assert!(text.contains("not trusted"), "{text}");

        let dir = crate::testutil::tmp();
        let (text, ok) = report("far", &program(&fake(&dir, 3), true), Duration::from_secs(10));
        assert!(!ok);
        assert!(text.contains("version: FAILED: adapter `far` speaks contract 3"), "{text}");
        assert!(text.contains("list: ok"), "{text}");

        let missing = program(&dir.join("nope"), false);
        let (text, ok) = report("far", &missing, Duration::from_secs(10));
        assert!(!ok && text.contains("version: FAILED") && text.contains("list: FAILED"), "{text}");
    }

    #[test]
    fn another_sources_contract_is_refused() {
        assert!(check_contract(None).is_ok() && check_contract(Some("2")).is_ok());
        let e = check_contract(Some("3")).unwrap_err().to_string();
        assert_eq!(e, "mu-remote rows speaks sources contract 2; this mu offers 3");
    }

    #[test]
    fn list_shows_adapter_trust_and_whether_it_answers() {
        let dir = crate::testutil::tmp();
        let mut remotes = BTreeMap::new();
        remotes.insert("cluster".to_string(), program(&fake(&dir, 1), true));
        remotes.insert("down".to_string(), program(&dir.join("nope"), false));
        remotes.insert("devbox".to_string(), Remote { adapter: "ssh".into(), host: None, ..Default::default() });
        let text = table(&remotes, Some("cluster"), Duration::from_secs(10));
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
        assert_eq!(table(&BTreeMap::new(), None, Duration::from_secs(1)).lines().count(), 1);
    }

    #[test]
    fn rows_carry_the_agent_the_host_a_hint_and_argv_attach_and_helper() {
        let dir = crate::testutil::tmp();
        let adapter = fake(&dir, 1);
        let exe = Path::new("/o pt/mu-remote");
        let r = rows_of("far", &program(&adapter, true), exe);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0]["id"], "abc123");
        assert_eq!(r[0]["host"], "far");
        assert_eq!(r[0]["hint"], "mu-remote far --resume abc123");
        assert_eq!(r[0]["attach"], json!(["/o pt/mu-remote", "connect", "far", "abc123"]));
        assert_eq!(r[0]["helper"], json!(["/o pt/mu-remote", "helper", "far", "abc123"]));
        assert_eq!(r[0]["stop"], json!(["/o pt/mu-remote", "stop", "far", "abc123"]));
        assert_eq!(r[0]["cancel"], json!(["/o pt/mu-remote", "cancel", "far", "abc123"]));
        assert_eq!(r[0]["notify"], json!(["/o pt/mu-remote", "notify", "far", "abc123", "-"]));
        assert!(r[0].get("state_key").is_none(), "no state_key unless the remote sets one");
        let keyed = Remote { state_key: Some("/k/id".into()), ..program(&adapter, true) };
        assert_eq!(rows_of("far", &keyed, exe)[0]["state_key"], "/k/id");
        assert_eq!(r[0]["client"]["host"], "h");
        assert_eq!(r[0]["waiting"], true);
        assert!(rows_of("far", &program(&adapter, false), exe).is_empty(), "untrusted: no rows");
        assert!(rows_of("far", &program(&dir.join("nope"), true), exe).is_empty(), "unreachable: no rows");
    }
}
