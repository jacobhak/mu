//! Prompt corpus: omp's history.db, Claude Code's history.jsonl and mu's own history.

use serde_json::Value;
use std::path::Path;

#[derive(Debug, Clone)]
pub struct Row {
    pub text: String,
    pub cwd: String,
    pub ts: i64,
    /// How many times the source saw this prompt (omp dedups prompts into one row).
    pub count: u32,
}

fn omp_rows(agent_dir: &Path, out: &mut Vec<Row>) {
    use rusqlite::{Connection, OpenFlags};
    let Ok(db) = Connection::open_with_flags(agent_dir.join("history.db"), OpenFlags::SQLITE_OPEN_READ_ONLY) else { return };
    let Ok(mut st) = db.prepare("SELECT prompt, cwd, created_at, use_count FROM history") else { return };
    let Ok(rows) = st.query_map([], |r| {
        Ok(Row {
            text: r.get(0)?,
            cwd: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
            ts: r.get(2)?,
            count: r.get::<_, i64>(3).unwrap_or(1).clamp(1, 1000) as u32,
        })
    }) else {
        return;
    };
    out.extend(rows.flatten());
}

fn claude_rows(home: &Path, out: &mut Vec<Row>) {
    let Ok(text) = std::fs::read_to_string(home.join(".claude/history.jsonl")) else { return };
    for l in text.lines() {
        let Ok(v) = serde_json::from_str::<Value>(l) else { continue };
        let Some(t) = v["display"].as_str() else { continue };
        out.push(Row {
            text: t.to_owned(),
            cwd: v["project"].as_str().unwrap_or_default().to_owned(),
            ts: v["timestamp"].as_i64().unwrap_or(0) / 1000,
            count: 1,
        });
    }
}

/// Parses one mu history line: a bare JSON string (legacy) or `{text, cwd, ts}`.
pub fn mu_row(line: &str) -> Option<Row> {
    match serde_json::from_str::<Value>(line).ok()? {
        Value::String(text) => Some(Row { text, cwd: String::new(), ts: 0, count: 1 }),
        Value::Object(o) => Some(Row {
            text: o.get("text")?.as_str()?.to_owned(),
            cwd: o.get("cwd").and_then(Value::as_str).unwrap_or_default().to_owned(),
            ts: o.get("ts").and_then(Value::as_i64).unwrap_or(0),
            count: 1,
        }),
        _ => None,
    }
}

fn mu_rows(mu_home: &Path, out: &mut Vec<Row>) {
    let Ok(text) = std::fs::read_to_string(mu_home.join("history.jsonl")) else { return };
    out.extend(text.lines().filter_map(mu_row));
}

/// All rows from every readable source, oldest first. Missing sources are skipped.
pub fn load_corpus(agent_dir: &Path, mu_home: &Path, home: &Path) -> Vec<Row> {
    let mut rows = Vec::new();
    omp_rows(agent_dir, &mut rows);
    claude_rows(home, &mut rows);
    mu_rows(mu_home, &mut rows);
    rows.retain(|r| !r.text.trim().is_empty());
    rows.sort_by_key(|r| r.ts);
    rows
}
