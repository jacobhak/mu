//! `mu agent list`: the agents running here, from their owner records.

use anyhow::Result;
use mu_session::session::{list, owner};
use serde_json::{Value, json};

/// One object per live agent: the shape `mu agent list --json` prints, a line each.
fn agents() -> Vec<Value> {
    let mut found: Vec<(i64, Value)> = owner::live_sessions()
        .into_iter()
        .filter(|s| matches!(s.kind.as_deref(), Some("agent" | "worker")))
        .map(|s| {
            let started = s.started.parse::<jiff::Timestamp>().map_or(0, |t| t.as_second());
            let title = list::info(&s.path).and_then(|i| i.title);
            let row = json!({
                "id": list::id_of(&s.path),
                "kind": s.kind,
                "cwd": s.cwd.map(|c| c.to_string_lossy().into_owned()).unwrap_or_default(),
                "started": started,
                "title": title,
                "client": s.client,
                "waiting": s.waiting,
                "locked": s.locked,
            });
            (started, row)
        })
        .collect();
    found.sort_by_key(|(started, _)| *started);
    found.into_iter().map(|(_, row)| row).collect()
}

/// `mu agent key`: the fingerprint of the key that every agent of this profile shows its clients.
pub fn key() -> Result<i32> {
    let key = mu_common::secure::agent_key().map_err(anyhow::Error::msg)?;
    println!("{}", key.fingerprint());
    Ok(0)
}

pub fn list(json: bool) -> Result<i32> {
    let rows = agents();
    if json {
        for r in &rows {
            println!("{r}");
        }
        return Ok(0);
    }
    if rows.is_empty() {
        println!("no agents are running");
    }
    for r in &rows {
        let client = match r["client"]["host"].as_str() {
            Some(h) => format!("client {h}"),
            None => "no client".into(),
        };
        let waiting = match (r["locked"] == true, r["waiting"] == true) {
            (true, _) => "  locked: attach to unlock",
            (false, true) => "  waiting for you",
            _ => "",
        };
        let title = r["title"].as_str().map(|t| format!("  {t}")).unwrap_or_default();
        println!("{}  {}  {client}{waiting}{title}", r["id"].as_str().unwrap_or(""), r["cwd"].as_str().unwrap_or(""));
    }
    Ok(0)
}
