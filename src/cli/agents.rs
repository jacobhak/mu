//! `mu agent list`, `stop` and `cancel`: the agents running here, from their owner records.

use anyhow::Result;
use mu_session::session::owner::LiveSession;
use mu_session::session::{control, list, owner};
use serde_json::{Value, json};
use std::time::{Duration, Instant};

/// One object per live agent: the shape `mu agent list --json` prints, a line each.
fn agents() -> Vec<Value> {
    let mut found: Vec<(i64, Value)> = owner::live_sessions()
        .into_iter()
        .filter(|s| s.kind.as_deref() == Some("agent"))
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

const STOP_WAIT: Duration = Duration::from_secs(15);

/// The one running agent whose session id starts with `prefix`.
fn find(prefix: &str) -> Result<LiveSession, String> {
    let mut hits: Vec<(String, LiveSession)> = owner::live_sessions()
        .into_iter()
        .filter(|s| s.kind.as_deref() == Some("agent"))
        .map(|s| (list::id_of(&s.path), s))
        .filter(|(id, _)| id.starts_with(prefix))
        .collect();
    match hits.len() {
        0 => Err(format!("no agent with a session starting {prefix} is running here (`mu agent list` shows them)")),
        1 => Ok(hits.remove(0).1),
        _ => {
            let ids: Vec<&str> = hits.iter().map(|(id, _)| id.as_str()).collect();
            Err(format!("several agents match {prefix}: {}", ids.join(", ")))
        }
    }
}

/// Past this long without its record, an agent whose pid still answers is taken as gone: an
/// unreaped zombie answers too.
const RECORD_GRACE: Duration = Duration::from_secs(5);

/// An exited process nobody has reaped yet (a container without an init process leaves them).
/// Only Linux's `/proc` says so; elsewhere init reaps them.
fn is_zombie(pid: impl std::fmt::Display) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|s| s.rsplit_once(") ").map(|(_, rest)| rest.starts_with('Z')))
        .unwrap_or(false)
}

/// The agent has exited: its pid is dead, or its record has been gone for `RECORD_GRACE` (the record
/// goes first, while the process is still shutting down).
fn gone(s: &LiveSession, record_gone_since: &mut Option<Instant>) -> bool {
    // SAFETY: signal 0 only checks that the pid exists.
    if i32::try_from(s.pid).is_ok_and(|p| unsafe { libc::kill(p, 0) } != 0) || is_zombie(s.pid) {
        return true;
    }
    if owner::live_sessions().iter().any(|l| l.path == s.path) {
        *record_gone_since = None;
        return false;
    }
    record_gone_since.get_or_insert_with(Instant::now).elapsed() >= RECORD_GRACE
}

pub fn stop(id: &str) -> Result<i32> {
    let s = match find(id) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("mu agent stop: {e}");
            return Ok(1);
        }
    };
    if let Err(e) = control::terminate_session(&s.path, s.pid, &s.started) {
        eprintln!("mu agent stop: {e}");
        return Ok(1);
    }
    let deadline = Instant::now() + STOP_WAIT;
    let mut record_gone_since = None;
    while !gone(&s, &mut record_gone_since) {
        if Instant::now() >= deadline {
            eprintln!("mu agent stop: agent {id} was asked to exit but still runs after 15 s");
            return Ok(1);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    println!("stopped agent {id}");
    Ok(0)
}

pub fn cancel(id: &str) -> Result<i32> {
    let result =
        find(id).and_then(|s| control::stop_session(&s.path, s.pid, &s.started, "agent").map_err(|e| e.to_string()));
    match result {
        Ok(text) => {
            println!("{text}");
            Ok(0)
        }
        Err(e) => {
            eprintln!("mu agent cancel: {e}");
            Ok(1)
        }
    }
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
