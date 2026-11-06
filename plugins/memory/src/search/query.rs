//! Keyword search over the index: the FTS query, the best sessions, and each hit with its neighbours.

use super::index::open;
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::HashMap;
use std::path::Path;

/// Hits kept per session.
const HITS_PER_SESSION: usize = 3;
/// Message rows read before grouping by session.
const ROW_CAP: i64 = 5000;

pub(super) struct Session {
    pub path: String,
    pub title: Option<String>,
    pub started: String,
    /// One window per hit: the hit and the messages around it, in order.
    pub hits: Vec<Vec<Line>>,
}

pub(super) struct Line {
    pub hit: bool,
    pub role: String,
    pub text: String,
}

/// Searches `db`, best sessions first. `repo` limits to sessions of that repo key; `exclude` is a session id
/// to skip (the current one). A query with FTS5 syntax is passed through, falling back to quoted terms if
/// sqlite rejects it; anything else is quoted term by term (implicit AND).
pub(super) fn run(db: &Path, q: &str, repo: Option<&str>, exclude: &str, limit: usize) -> Result<Vec<Session>, String> {
    let conn = open(db)?;
    let raw = q.trim();
    let mut candidates = Vec::new();
    if is_deliberate(raw) {
        candidates.push(raw.to_string());
    }
    candidates.push(quote_terms(raw));
    let mut last = String::new();
    for fts in candidates {
        match search(&conn, &fts, repo, exclude, limit) {
            Ok(s) => return Ok(s),
            Err(e) => last = e,
        }
    }
    Err(last)
}

/// Each term as an FTS5 string, inner quotes doubled.
pub(super) fn quote_terms(q: &str) -> String {
    q.split_whitespace().map(|t| format!("\"{}\"", t.replace('"', "\"\""))).collect::<Vec<_>>().join(" ")
}

/// Whether `q` uses FTS5 syntax on purpose.
fn is_deliberate(q: &str) -> bool {
    q.contains(['"', '(', ')', '*']) || q.split_whitespace().any(|w| matches!(w, "AND" | "OR" | "NOT" | "NEAR"))
}

fn search(
    conn: &Connection,
    fts: &str,
    repo: Option<&str>,
    exclude: &str,
    limit: usize,
) -> Result<Vec<Session>, String> {
    let err = |e: rusqlite::Error| e.to_string();
    let mut st = conn
        .prepare(
            "SELECT path, seq FROM msgs WHERE msgs MATCH ?1 \
             AND (?2 IS NULL OR path IN (SELECT path FROM files WHERE repo_key = ?2)) \
             AND session_id != ?3 ORDER BY bm25(msgs) LIMIT ?4",
        )
        .map_err(err)?;
    let rows = st
        .query_map(params![fts, repo, exclude, ROW_CAP], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
        .map_err(err)?;
    // Rows come best first, so the first `limit` sessions seen are the best sessions.
    let mut order: Vec<(String, Vec<i64>)> = Vec::new();
    let mut slot: HashMap<String, usize> = HashMap::new();
    for row in rows {
        let (path, seq) = row.map_err(err)?;
        let i = match slot.get(&path) {
            Some(&i) => i,
            None if order.len() < limit => {
                slot.insert(path.clone(), order.len());
                order.push((path, Vec::new()));
                order.len() - 1
            }
            None => continue,
        };
        let hits = &mut order[i].1;
        if hits.len() < HITS_PER_SESSION {
            hits.push(seq);
        }
    }
    order.into_iter().map(|(path, seqs)| session(conn, path, &seqs)).collect()
}

fn session(conn: &Connection, path: String, seqs: &[i64]) -> Result<Session, String> {
    let err = |e: rusqlite::Error| e.to_string();
    let (title, started) = conn
        .query_row("SELECT title, started FROM files WHERE path = ?1", [&path], |r| {
            Ok((r.get::<_, Option<String>>(0)?, r.get::<_, String>(1)?))
        })
        .optional()
        .map_err(err)?
        .unwrap_or((None, String::new()));
    let mut win = conn
        .prepare(
            "SELECT p.seq, m.role, m.text FROM pos p JOIN msgs m ON m.rowid = p.rid \
             WHERE p.path = ?1 AND p.seq BETWEEN ?2 AND ?3 ORDER BY p.seq",
        )
        .map_err(err)?;
    let mut hits = Vec::with_capacity(seqs.len());
    for &s in seqs {
        let rows = win
            .query_map(params![path, s - 1, s + 1], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
            })
            .map_err(err)?;
        let lines = rows
            .map(|r| r.map(|(seq, role, text)| Line { hit: seq == s, role, text }))
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(err)?;
        hits.push(lines);
    }
    Ok(Session { path, title, started, hits })
}
