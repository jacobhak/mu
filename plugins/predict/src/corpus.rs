//! Prompt corpus: omp's history.db, Claude Code's history.jsonl and mu's own history.

use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::io::BufRead;
use std::path::Path;

/// Prompts kept per working directory; older ones are dropped as new ones stream in.
const PER_CWD: usize = 2000;

/// Keeps the most recent `PER_CWD` rows per cwd, in arrival order.
#[derive(Default)]
struct Recent(HashMap<String, VecDeque<Row>>);

impl Recent {
    fn push(&mut self, r: Row) {
        let q = self.0.entry(r.cwd.clone()).or_default();
        if q.len() == PER_CWD {
            q.pop_front();
        }
        q.push_back(r);
    }

    fn into_rows(self) -> Vec<Row> {
        self.0.into_values().flatten().collect()
    }
}

#[derive(Debug, Clone)]
pub struct Row {
    pub text: String,
    pub cwd: String,
    pub ts: i64,
    /// How many times the source saw this prompt (omp dedups prompts into one row).
    pub count: u32,
}

fn omp_rows(agent_dir: &Path, out: &mut Recent) {
    use rusqlite::{Connection, OpenFlags};
    let Ok(db) = Connection::open_with_flags(agent_dir.join("history.db"), OpenFlags::SQLITE_OPEN_READ_ONLY) else {
        return;
    };
    let Ok(mut st) = db.prepare("SELECT prompt, cwd, created_at, use_count FROM history ORDER BY created_at") else {
        return;
    };
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
    for r in rows.flatten() {
        out.push(r);
    }
}

fn claude_rows(home: &Path, out: &mut Recent) {
    let Ok(f) = std::fs::File::open(home.join(".claude/history.jsonl")) else { return };
    for l in std::io::BufReader::new(f).split(b'\n').map_while(Result::ok) {
        let Ok(v) = serde_json::from_slice::<Value>(&l) else { continue };
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

fn mu_rows(mu_home: &Path, out: &mut Recent) {
    let Ok(f) = std::fs::File::open(mu_home.join("history.jsonl")) else { return };
    for l in std::io::BufReader::new(f).lines().map_while(Result::ok) {
        if let Some(r) = mu_row(&l) {
            out.push(r);
        }
    }
}

/// All rows from every readable source, oldest first. Missing sources are skipped.
pub fn load_corpus(agent_dir: &Path, mu_home: &Path, home: &Path) -> Vec<Row> {
    let mut recent = Recent::default();
    omp_rows(agent_dir, &mut recent);
    claude_rows(home, &mut recent);
    mu_rows(mu_home, &mut recent);
    let mut rows = recent.into_rows();
    rows.retain(|r| !r.text.trim().is_empty());
    rows.sort_by_key(|r| r.ts);
    rows
}

#[cfg(test)]
mod stream_tests {
    use super::*;

    #[test]
    fn keeps_only_the_most_recent_per_cwd() {
        let mut r = Recent::default();
        for i in 0..PER_CWD + 5 {
            r.push(Row { text: format!("p{i}"), cwd: "/a".into(), ts: i as i64, count: 1 });
        }
        r.push(Row { text: "other".into(), cwd: "/b".into(), ts: 0, count: 1 });
        let rows = r.into_rows();
        assert_eq!(rows.len(), PER_CWD + 1);
        assert!(!rows.iter().any(|x| x.text == "p0"));
        assert!(rows.iter().any(|x| x.text == format!("p{}", PER_CWD + 4)));
    }
}
