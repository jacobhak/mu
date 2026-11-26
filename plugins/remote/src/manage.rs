//! `stop`, `cancel`, `notify` and `agents`: the lifecycle of agents (and workers) on a remote, through its
//! adapter. These are also what the rows' `stop`, `cancel` and `notify` argument vectors run for mu.

use crate::adapter::{Adapter, AgentInfo, Unsupported};
use crate::config::{Config, Remote};
use crate::rows::ANSWER_TIMEOUT;
use anyhow::{Result, bail};
use serde_json::json;
use std::io::Read;
use std::time::Duration;

const WAIT: Duration = Duration::from_secs(60);
const MAX_MESSAGE: u64 = 64 * 1024;
const NOT_SUPPORTED: i32 = 64;

/// Runs `act` on the adapter of remote `name`. An adapter that cannot do `verb` exits 64.
fn on_adapter(name: &str, verb: &str, act: impl FnOnce(&Adapter) -> Result<()>) -> Result<i32> {
    let cfg = Config::load()?;
    let (name, remote) = cfg.pick(Some(name))?;
    let adapter = Adapter::from_config(name, remote)?;
    match act(&adapter) {
        Ok(()) => Ok(0),
        Err(e) => match e.downcast_ref::<Unsupported>() {
            Some(u) => {
                eprintln!("mu-remote: remote {name} cannot {verb}: {u}");
                Ok(NOT_SUPPORTED)
            }
            None => Err(e),
        },
    }
}

pub fn stop(name: &str, id: &str) -> Result<i32> {
    on_adapter(name, "stop", |a| {
        a.stop(id, Some(WAIT))?;
        println!("stopped {id} on {name}");
        Ok(())
    })
}

pub fn cancel(name: &str, id: &str) -> Result<i32> {
    on_adapter(name, "cancel", |a| {
        a.cancel(id, Some(WAIT))?;
        println!("asked agent {id} on {name} to cancel its turn");
        Ok(())
    })
}

/// The message: `text`, or stdin (at most 64 KiB) for `-`.
fn message(text: &str, stdin: &mut dyn Read) -> Result<String> {
    let text = if text == "-" {
        let mut buf = Vec::new();
        stdin.take(MAX_MESSAGE).read_to_end(&mut buf)?;
        String::from_utf8_lossy(&buf).into_owned()
    } else {
        text.to_string()
    };
    let text = text.trim_end().to_string();
    if text.trim().is_empty() {
        bail!("empty message");
    }
    Ok(text)
}

pub fn notify(name: &str, id: &str, text: &str) -> Result<i32> {
    let text = message(text, &mut std::io::stdin())?;
    on_adapter(name, "notify", |a| {
        let line = a.notify(id, &text, Some(WAIT))?;
        println!("message to {id} on {name}: {line}");
        Ok(())
    })
}

fn first_line(e: &anyhow::Error) -> String {
    format!("{e:#}").lines().next().unwrap_or_default().to_string()
}

fn kind(a: &AgentInfo) -> &str {
    if a.kind.is_empty() { "agent" } else { &a.kind }
}

fn state(a: &AgentInfo) -> &'static str {
    if a.locked {
        "locked"
    } else if a.waiting {
        "waiting"
    } else {
        "-"
    }
}

fn json_line(remote: &str, a: &AgentInfo) -> String {
    json!({
        "id": a.id,
        "cwd": a.cwd,
        "started": a.started,
        "title": a.title,
        "client": a.client.as_ref().map(|c| json!({"host": c.host, "since": c.since})),
        "waiting": a.waiting,
        "locked": a.locked,
        "remote": remote,
        "kind": kind(a),
    })
    .to_string()
}

fn table(found: &[(String, Vec<AgentInfo>)]) -> String {
    let head = ["REMOTE", "ID", "KIND", "CLIENT", "STATE", "CWD", "TITLE"].map(String::from);
    let rows: Vec<[String; 7]> = found
        .iter()
        .flat_map(|(remote, agents)| {
            agents.iter().map(move |a| {
                [
                    remote.clone(),
                    a.id.clone(),
                    kind(a).to_string(),
                    a.client.as_ref().map_or_else(|| "-".to_string(), |c| c.host.clone()),
                    state(a).to_string(),
                    a.cwd.clone(),
                    a.title.clone().unwrap_or_default(),
                ]
            })
        })
        .collect();
    if rows.is_empty() {
        return "no agents\n".to_string();
    }
    let width: [usize; 6] =
        std::array::from_fn(|i| rows.iter().chain([&head]).map(|r| r[i].chars().count()).max().unwrap_or(0));
    let mut out = String::new();
    for r in std::iter::once(&head).chain(&rows) {
        let mut line = String::new();
        for (i, &w) in width.iter().enumerate() {
            line.push_str(&format!("{:<w$}  ", r[i]));
        }
        line.push_str(&r[6]);
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

fn ask(name: &str, r: &Remote) -> Result<Vec<AgentInfo>, String> {
    let a = Adapter::from_config(name, r).map_err(|e| first_line(&e))?;
    a.list(Some(ANSWER_TIMEOUT)).map_err(|e| first_line(&e))
}

/// The agents and workers of every remote (or `name`), as a table or JSON lines. A remote that fails is named on
/// stderr and the others still print; the exit status is 1 then.
pub fn agents(name: Option<&str>, json: bool) -> Result<i32> {
    let cfg = Config::load()?;
    let chosen: Vec<(&String, &Remote)> = match name {
        Some(n) => match cfg.remote.get_key_value(n) {
            Some(kv) => vec![kv],
            None => bail!("unknown remote `{n}`; `mu-remote list` shows the configured ones"),
        },
        None => cfg.remote.iter().collect(),
    };
    let answers: Vec<Result<Vec<AgentInfo>, String>> = std::thread::scope(|s| {
        let handles: Vec<_> = chosen.iter().map(|(n, r)| s.spawn(|| ask(n, r))).collect();
        handles.into_iter().map(|h| h.join().unwrap_or_else(|_| Err("list panicked".to_string()))).collect()
    });
    let mut found = Vec::new();
    let mut failed = false;
    for ((n, _), answer) in chosen.iter().zip(answers) {
        match answer {
            Ok(list) => found.push((n.to_string(), list)),
            Err(e) => {
                eprintln!("mu-remote: {n}: {e}");
                failed = true;
            }
        }
    }
    if json {
        for (remote, list) in &found {
            for a in list {
                println!("{}", json_line(remote, a));
            }
        }
    } else {
        print!("{}", table(&found));
    }
    Ok(i32::from(failed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::ClientInfo;

    fn agent(id: &str) -> AgentInfo {
        AgentInfo { id: id.into(), cwd: "/w".into(), ..Default::default() }
    }

    #[test]
    fn the_table_pads_columns_and_names_state_client_and_kind() {
        let mut a = agent("abc");
        a.client = Some(ClientInfo { host: "laptop".into(), since: 1 });
        a.locked = true;
        a.title = Some("fix it".into());
        let mut w = agent("w1");
        w.kind = "worker".into();
        w.waiting = true;
        let text = table(&[("far".into(), vec![a, w])]);
        let rows: Vec<Vec<&str>> = text.lines().map(|l| l.split_whitespace().collect()).collect();
        assert_eq!(rows[0], ["REMOTE", "ID", "KIND", "CLIENT", "STATE", "CWD", "TITLE"]);
        assert_eq!(rows[1], ["far", "abc", "agent", "laptop", "locked", "/w", "fix", "it"]);
        assert_eq!(rows[2], ["far", "w1", "worker", "-", "waiting", "/w"]);
        assert_eq!(table(&[("far".into(), Vec::new())]), "no agents\n");
    }

    #[test]
    fn json_lines_carry_the_remote_and_kind_but_no_argv() {
        let v: serde_json::Value = serde_json::from_str(&json_line("far", &agent("abc"))).unwrap();
        assert_eq!(v["remote"], "far");
        assert_eq!(v["kind"], "agent");
        assert_eq!(v["id"], "abc");
        assert!(v.get("attach").is_none() && v.get("hint").is_none());
    }

    #[test]
    fn a_message_comes_from_the_argument_or_stdin_and_is_never_empty() {
        assert_eq!(message("hi there", &mut std::io::empty()).unwrap(), "hi there");
        assert_eq!(message("-", &mut &b"from stdin\n"[..]).unwrap(), "from stdin");
        assert_eq!(message("-", &mut std::io::empty()).unwrap_err().to_string(), "empty message");
        assert_eq!(message("  ", &mut std::io::empty()).unwrap_err().to_string(), "empty message");
        let big = vec![b'x'; 100_000];
        assert_eq!(message("-", &mut &big[..]).unwrap().len(), 64 * 1024);
    }
}
