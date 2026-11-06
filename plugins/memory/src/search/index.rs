//! The session index: a `files` row per session file (how far it has been read) and one FTS5 row per
//! indexed message. Session files are append-only, so an update reads only what was appended.

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fmt::Display;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

pub(super) const DB_FILE: &str = "session-index.db";

/// `pos` maps a file's message sequence number to the FTS rowid, so neighbours are found by (path, seq)
/// without scanning `msgs`, which has no index on those columns.
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS files (
    path TEXT PRIMARY KEY,
    size INTEGER NOT NULL,
    mtime INTEGER NOT NULL,
    bytes INTEGER NOT NULL,
    nseq INTEGER NOT NULL,
    session_id TEXT NOT NULL,
    cwd TEXT NOT NULL,
    repo_key TEXT NOT NULL,
    title TEXT,
    started TEXT NOT NULL
);
CREATE VIRTUAL TABLE IF NOT EXISTS msgs USING fts5(
    session_id UNINDEXED, entry_id UNINDEXED, role UNINDEXED, ts UNINDEXED, path UNINDEXED, seq UNINDEXED, text
);
CREATE TABLE IF NOT EXISTS pos (
    path TEXT NOT NULL,
    seq INTEGER NOT NULL,
    rid INTEGER NOT NULL,
    PRIMARY KEY (path, seq)
);
";

/// What one update did: files whose rows changed, and the session cwds whose repo key the caller must
/// resolve (git is async) before those files can be indexed. Files waiting on a key are skipped.
#[derive(Default)]
pub(super) struct Report {
    pub indexed: usize,
    pub needed: Vec<PathBuf>,
}

fn err(e: impl Display) -> String {
    e.to_string()
}

pub(super) fn open(db: &Path) -> Result<Connection, String> {
    if let Some(d) = db.parent() {
        std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
    }
    let conn = Connection::open(db).map_err(err)?;
    conn.busy_timeout(Duration::from_secs(5)).map_err(err)?;
    conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL;").map_err(err)?;
    conn.execute_batch(SCHEMA).map_err(err)?;
    Ok(conn)
}

/// Brings the index at `db` up to date with the session files under `dir` (`<dir>/<encoded cwd>/*.jsonl`).
/// `keys` maps a session's cwd to its repo key.
pub(super) fn update(dir: &Path, db: &Path, keys: &HashMap<PathBuf, String>) -> Result<Report, String> {
    let mut conn = open(db)?;
    let files = list(dir);
    let listed: HashSet<String> = files.iter().map(|p| path_key(p)).collect();
    let known: Vec<String> = {
        let mut st = conn.prepare("SELECT path FROM files").map_err(err)?;
        let rows = st.query_map([], |r| r.get(0)).map_err(err)?;
        rows.collect::<rusqlite::Result<_>>().map_err(err)?
    };
    let gone: Vec<String> = known.into_iter().filter(|p| !listed.contains(p)).collect();
    if !gone.is_empty() {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(err)?;
        for p in &gone {
            drop_file(&tx, p)?;
            tx.execute("DELETE FROM files WHERE path = ?1", [p]).map_err(err)?;
        }
        tx.commit().map_err(err)?;
    }
    let mut rep = Report::default();
    for f in &files {
        match index_file(&mut conn, f, keys)? {
            Step::Same => {}
            Step::Done => rep.indexed += 1,
            Step::Needs(cwd) => {
                if !rep.needed.contains(&cwd) {
                    rep.needed.push(cwd);
                }
            }
        }
    }
    Ok(rep)
}

/// Session files: `<dir>/<encoded cwd>/*.jsonl`, sorted.
fn list(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(groups) = std::fs::read_dir(dir) else { return out };
    for g in groups.flatten().map(|e| e.path()).filter(|p| p.is_dir()) {
        let Ok(files) = std::fs::read_dir(&g) else { continue };
        out.extend(
            files.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "jsonl") && p.is_file()),
        );
    }
    out.sort();
    out
}

enum Step {
    Same,
    Done,
    Needs(PathBuf),
}

struct Row {
    size: u64,
    mtime: i64,
    bytes: u64,
    nseq: i64,
    session_id: String,
    cwd: String,
    repo_key: String,
    title: Option<String>,
    started: String,
}

fn load_row(c: &Connection, key: &str) -> Result<Option<Row>, String> {
    c.query_row(
        "SELECT size, mtime, bytes, nseq, session_id, cwd, repo_key, title, started FROM files WHERE path = ?1",
        [key],
        |r| {
            Ok(Row {
                size: r.get::<_, i64>(0)? as u64,
                mtime: r.get(1)?,
                bytes: r.get::<_, i64>(2)? as u64,
                nseq: r.get(3)?,
                session_id: r.get(4)?,
                cwd: r.get(5)?,
                repo_key: r.get(6)?,
                title: r.get(7)?,
                started: r.get(8)?,
            })
        },
    )
    .optional()
    .map_err(err)
}

fn index_file(conn: &mut Connection, path: &Path, keys: &HashMap<PathBuf, String>) -> Result<Step, String> {
    let Ok(meta) = std::fs::metadata(path) else { return Ok(Step::Same) };
    let (size, mtime) = (meta.len(), mtime_ns(&meta));
    let key = path_key(path);
    // Immediate: two sessions indexing at once must not both read the same offset.
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(err)?;
    let row = load_row(&tx, &key)?;
    if row.as_ref().is_some_and(|r| r.size == size && r.mtime == mtime) {
        return Ok(Step::Same);
    }
    // Shorter than what was indexed: rewritten, so reindex from the start.
    let row = row.filter(|r| r.bytes <= size);
    let start = row.as_ref().map_or(0, |r| r.bytes);
    let Ok(mut f) = std::fs::File::open(path) else { return Ok(Step::Same) };
    let mut buf = Vec::new();
    if f.seek(SeekFrom::Start(start)).and_then(|_| f.read_to_end(&mut buf)).is_err() {
        return Ok(Step::Same);
    }
    // An unterminated last line is not consumed; the next update reads it whole.
    let end = buf.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
    let Some(parsed) = parse(&buf[..end], row.as_ref()) else { return Ok(Step::Same) };
    let repo_key = match &row {
        Some(r) => r.repo_key.clone(),
        None if parsed.cwd.is_empty() => String::new(),
        None => match keys.get(Path::new(&parsed.cwd)) {
            Some(k) => k.clone(),
            None => return Ok(Step::Needs(PathBuf::from(&parsed.cwd))),
        },
    };
    if start == 0 {
        drop_file(&tx, &key)?;
    }
    let mut seq = row.as_ref().map_or(0, |r| r.nseq);
    for m in &parsed.msgs {
        tx.execute(
            "INSERT INTO msgs (session_id, entry_id, role, ts, path, seq, text) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![parsed.session_id, m.entry_id, m.role, m.ts, key, seq, m.text],
        )
        .map_err(err)?;
        let rid = tx.last_insert_rowid();
        tx.execute("INSERT INTO pos (path, seq, rid) VALUES (?1, ?2, ?3)", params![key, seq, rid]).map_err(err)?;
        seq += 1;
    }
    tx.execute(
        "INSERT OR REPLACE INTO files (path, size, mtime, bytes, nseq, session_id, cwd, repo_key, title, started) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            key,
            (start + buf.len() as u64) as i64,
            mtime,
            (start + end as u64) as i64,
            seq,
            parsed.session_id,
            parsed.cwd,
            repo_key,
            parsed.title,
            parsed.started
        ],
    )
    .map_err(err)?;
    tx.commit().map_err(err)?;
    Ok(Step::Done)
}

/// Removes a file's messages (the `files` row is the caller's).
fn drop_file(c: &Connection, key: &str) -> Result<(), String> {
    let rids: Vec<i64> = {
        let mut st = c.prepare("SELECT rid FROM pos WHERE path = ?1").map_err(err)?;
        let rows = st.query_map([key], |r| r.get(0)).map_err(err)?;
        rows.collect::<rusqlite::Result<_>>().map_err(err)?
    };
    for rid in rids {
        c.execute("DELETE FROM msgs WHERE rowid = ?1", [rid]).map_err(err)?;
    }
    c.execute("DELETE FROM pos WHERE path = ?1", [key]).map_err(err)?;
    Ok(())
}

/// The session header and title, and the indexable messages, of the complete lines in `bytes`. When
/// resuming, `row` supplies the header (it is not in `bytes`). `None` when there is no header yet.
struct Parsed {
    session_id: String,
    cwd: String,
    started: String,
    title: Option<String>,
    msgs: Vec<Msg>,
}

struct Msg {
    entry_id: String,
    role: &'static str,
    ts: String,
    text: String,
}

fn parse(bytes: &[u8], row: Option<&Row>) -> Option<Parsed> {
    let mut p = match row {
        Some(r) => Parsed {
            session_id: r.session_id.clone(),
            cwd: r.cwd.clone(),
            started: r.started.clone(),
            title: r.title.clone(),
            msgs: Vec::new(),
        },
        None => Parsed {
            session_id: String::new(),
            cwd: String::new(),
            started: String::new(),
            title: None,
            msgs: Vec::new(),
        },
    };
    let mut have_header = row.is_some();
    for line in bytes.split(|&b| b == b'\n') {
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        // Malformed lines are skipped, not fatal.
        let Ok(v) = serde_json::from_slice::<Value>(line) else { continue };
        if !have_header {
            match v["type"].as_str() {
                Some("title") => p.title = non_empty(&v["title"]),
                Some("session") => {
                    let Some(id) = v["id"].as_str() else { continue };
                    p.session_id = id.into();
                    p.cwd = v["cwd"].as_str().unwrap_or_default().into();
                    p.started = v["timestamp"].as_str().unwrap_or_default().into();
                    have_header = true;
                }
                _ => {}
            }
            continue;
        }
        match v["type"].as_str() {
            Some("title_change") => p.title = non_empty(&v["title"]),
            Some("message") => p.msgs.extend(indexable(&v)),
            _ => {}
        }
    }
    have_header.then_some(p)
}

fn non_empty(v: &Value) -> Option<String> {
    v.as_str().filter(|s| !s.is_empty()).map(String::from)
}

/// A user or assistant message's text blocks, without `<system-reminder>` blocks. Tool results, thinking,
/// tool calls and synthetic user messages are not indexed.
fn indexable(entry: &Value) -> Option<Msg> {
    let m = &entry["message"];
    let role = match m["role"].as_str()? {
        "user" => "user",
        "assistant" => "assistant",
        _ => return None,
    };
    if m["synthetic"].as_bool() == Some(true) {
        return None;
    }
    let raw = match &m["content"] {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|b| b["type"] == "text")
            .filter_map(|b| b["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => return None,
    };
    let text = strip_reminders(&raw);
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    Some(Msg {
        entry_id: entry["id"].as_str().unwrap_or_default().into(),
        role,
        ts: entry["timestamp"].as_str().unwrap_or_default().into(),
        text: text.into(),
    })
}

/// Removes `<system-reminder>…</system-reminder>` spans; an unclosed one runs to the end.
fn strip_reminders(s: &str) -> String {
    const OPEN: &str = "<system-reminder>";
    const CLOSE: &str = "</system-reminder>";
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find(OPEN) {
        out.push_str(&rest[..i]);
        rest = match rest[i..].find(CLOSE) {
            Some(j) => &rest[i + j + CLOSE.len()..],
            None => "",
        };
    }
    out.push_str(rest);
    out
}

fn mtime_ns(m: &std::fs::Metadata) -> i64 {
    m.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos() as i64)
}

fn path_key(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}
