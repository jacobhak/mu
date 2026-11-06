use super::index;
use super::query;
use super::{clip, repo_key};
use crate::ctx::Ctx;
use rusqlite::Connection;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("mu-memory-search-{name}-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&d).unwrap();
    d
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
}

/// Updates the index until every cwd has its key, as the tool does. Returns the files indexed.
fn sync(sessions: &Path, db: &Path) -> usize {
    let rt = rt();
    let mut keys = HashMap::new();
    let mut total = 0;
    loop {
        let rep = index::update(sessions, db, &keys).unwrap();
        total += rep.indexed;
        if rep.needed.is_empty() {
            return total;
        }
        for cwd in rep.needed {
            let k = rt.block_on(repo_key(&cwd));
            keys.insert(cwd, k);
        }
    }
}

fn scalar(db: &Path, sql: &str) -> i64 {
    index::open(db).unwrap().query_row(sql, [], |r| r.get(0)).unwrap()
}

fn texts(db: &Path) -> Vec<String> {
    let c = index::open(db).unwrap();
    let mut st = c.prepare("SELECT text FROM msgs ORDER BY rowid").unwrap();
    st.query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect()
}

fn hits(db: &Path, q: &str) -> usize {
    query::run(db, q, None, "", 10).unwrap().len()
}

fn append(p: &Path, s: &str) {
    fs::OpenOptions::new().append(true).open(p).unwrap().write_all(s.as_bytes()).unwrap();
}

fn msg(id: &str, role: &str, content: Value) -> Value {
    json!({"type": "message", "id": id, "parentId": null, "timestamp": "2026-11-01T10:00:00.000Z",
           "message": {"role": role, "content": content, "timestamp": 1}})
}

fn text(s: &str) -> Value {
    json!([{"type": "text", "text": s}])
}

/// A session file as mu writes it: a title slot, the header, then the entries.
fn session_text(id: &str, cwd: &Path, title: &str, entries: &[Value]) -> String {
    let mut out = format!("{}\n", json!({"type": "title", "v": 1, "title": title, "pad": ""}));
    out += &format!(
        "{}\n",
        json!({"type": "session", "version": 3, "id": id, "timestamp": "2026-11-01T10:00:00.000Z",
               "cwd": cwd.to_string_lossy()})
    );
    for e in entries {
        out += &format!("{e}\n");
    }
    out
}

fn write_session(sessions: &Path, cwd: &Path, file: &str, id: &str, title: &str, entries: &[Value]) -> PathBuf {
    let dir = sessions.join(mu_common::paths::encode_cwd(cwd));
    fs::create_dir_all(&dir).unwrap();
    let p = dir.join(file);
    fs::write(&p, session_text(id, cwd, title, entries)).unwrap();
    p
}

fn git(dir: &Path, args: &[&str]) {
    let st =
        Command::new("git").args(args).current_dir(dir).stdout(Stdio::null()).stderr(Stdio::null()).status().unwrap();
    assert!(st.success(), "git {args:?} in {}", dir.display());
}

fn init_repo(dir: &Path) {
    fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q"]);
    git(dir, &["config", "user.name", "mu test"]);
    git(dir, &["config", "user.email", "test@example.com"]);
    git(dir, &["-c", "commit.gpgsign=false", "commit", "-q", "--allow-empty", "-m", "init"]);
}

#[test]
fn fts5_is_compiled_in() {
    // A macOS release build must not silently lose FTS5.
    let c = Connection::open_in_memory().unwrap();
    c.execute_batch("CREATE VIRTUAL TABLE t USING fts5(x); INSERT INTO t(x) VALUES ('hello world');").unwrap();
    let n: i64 = c.query_row("SELECT count(*) FROM t WHERE t MATCH 'hello'", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 1);
}

#[test]
fn appending_indexes_only_the_new_lines() {
    let d = tmp("append");
    let (sessions, db) = (d.join("sessions"), d.join("index.db"));
    let cwd = d.join("not-a-repo");
    let path = write_session(
        &sessions,
        &cwd,
        "a.jsonl",
        "sess-a",
        "",
        &[msg("m1", "user", text("first question")), msg("m2", "assistant", text("first answer"))],
    );
    assert_eq!(sync(&sessions, &db), 1);
    assert_eq!(scalar(&db, "SELECT count(*) FROM msgs"), 2);
    let first = scalar(&db, "SELECT min(rowid) FROM msgs");
    let len = fs::metadata(&path).unwrap().len() as i64;
    assert_eq!(scalar(&db, "SELECT bytes FROM files"), len);
    assert_eq!(sync(&sessions, &db), 0, "an unchanged file is not read again");

    // One complete line and the start of another: only the complete one is indexed.
    let complete = format!("{}\n", msg("m3", "user", text("second question about widgets")));
    let partial = r#"{"type":"message","id":"m4","#;
    append(&path, &format!("{complete}{partial}"));
    let len2 = fs::metadata(&path).unwrap().len() as i64;
    assert_eq!(sync(&sessions, &db), 1);
    assert_eq!(scalar(&db, "SELECT count(*) FROM msgs"), 3);
    assert_eq!(scalar(&db, "SELECT bytes FROM files"), len2 - partial.len() as i64);
    assert_eq!(scalar(&db, "SELECT min(rowid) FROM msgs"), first, "rows indexed earlier are kept");
    assert_eq!(hits(&db, "widgets"), 1);

    append(
        &path,
        r#""parentId":null,"timestamp":"2026-11-01T10:00:00.000Z","message":{"role":"user","content":[{"type":"text","text":"third"}],"timestamp":1}}"#,
    );
    append(&path, "\n");
    assert_eq!(sync(&sessions, &db), 1);
    assert_eq!(scalar(&db, "SELECT count(*) FROM msgs"), 4);
    assert_eq!(scalar(&db, "SELECT min(rowid) FROM msgs"), first);
}

#[test]
fn a_shrunk_file_is_reindexed() {
    let d = tmp("shrink");
    let (sessions, db) = (d.join("sessions"), d.join("index.db"));
    let cwd = d.join("not-a-repo");
    let path = write_session(
        &sessions,
        &cwd,
        "a.jsonl",
        "sess-a",
        "",
        &[
            msg("1", "user", text("alpha one")),
            msg("2", "user", text("alpha two")),
            msg("3", "user", text("alpha three")),
        ],
    );
    sync(&sessions, &db);
    assert_eq!(scalar(&db, "SELECT count(*) FROM msgs"), 3);
    fs::write(&path, session_text("sess-a", &cwd, "", &[msg("4", "user", text("beta only"))])).unwrap();
    sync(&sessions, &db);
    assert_eq!(texts(&db), ["beta only"]);
    assert_eq!(hits(&db, "alpha"), 0);
    assert_eq!(scalar(&db, "SELECT count(*) FROM files"), 1);
}

#[test]
fn a_deleted_file_leaves_the_index() {
    let d = tmp("deleted");
    let (sessions, db) = (d.join("sessions"), d.join("index.db"));
    let path = write_session(&sessions, &d, "a.jsonl", "sess-a", "", &[msg("1", "user", text("gone soon"))]);
    sync(&sessions, &db);
    fs::remove_file(&path).unwrap();
    sync(&sessions, &db);
    assert_eq!(scalar(&db, "SELECT count(*) FROM files"), 0);
    assert_eq!(scalar(&db, "SELECT count(*) FROM msgs"), 0);
    assert_eq!(scalar(&db, "SELECT count(*) FROM pos"), 0);
}

#[test]
fn only_user_and_assistant_text_is_indexed() {
    let d = tmp("content");
    let (sessions, db) = (d.join("sessions"), d.join("index.db"));
    let path = write_session(
        &sessions,
        &d,
        "a.jsonl",
        "sess-a",
        "",
        &[
            msg("u1", "user", text("real question about caching")),
            msg(
                "a1",
                "assistant",
                json!([
                    {"type": "thinking", "thinking": "SECRET-THINK"},
                    {"type": "text", "text": "caching answer"},
                    {"type": "toolCall", "id": "t1", "name": "read", "arguments": {"path": "SECRET-ARG"}}
                ]),
            ),
            msg("r1", "toolResult", json!([{"type": "text", "text": "SECRET-TOOL"}])),
            json!({"type": "custom", "id": "c1", "parentId": null, "customType": "plugin_context",
                   "data": {"text": "SECRET-PLUGIN"}}),
            json!({"type": "message", "id": "s1", "message": {"role": "user", "timestamp": 1, "synthetic": true,
                   "content": [{"type": "text", "text": "SECRET-SYNTH"}]}}),
            msg("w1", "user", text("<system-reminder>\nSECRET-REMINDER\n</system-reminder>")),
            msg("w2", "user", text("<system-reminder>SECRET-REMINDER2</system-reminder>kept words")),
            json!({"type": "title_change", "id": "t1", "title": "Caching notes", "source": "user"}),
        ],
    );
    append(&path, "{not json\n[1,2]\n");
    sync(&sessions, &db);
    assert_eq!(texts(&db), ["real question about caching", "caching answer", "kept words"]);
    assert_eq!(hits(&db, "SECRET*"), 0);
    assert_eq!(scalar(&db, "SELECT count(*) FROM files WHERE title = 'Caching notes'"), 1);
}

#[test]
fn the_current_session_is_excluded() {
    let d = tmp("current");
    let (sessions, db) = (d.join("sessions"), d.join("index.db"));
    write_session(&sessions, &d, "cur.jsonl", "sess-cur", "", &[msg("1", "user", text("widgets here"))]);
    write_session(&sessions, &d, "old.jsonl", "sess-old", "", &[msg("2", "user", text("widgets there"))]);
    sync(&sessions, &db);
    let found = query::run(&db, "widgets", None, "sess-cur", 5).unwrap();
    assert_eq!(found.len(), 1);
    assert!(found[0].path.ends_with("old.jsonl"));
    assert_eq!(query::run(&db, "widgets", None, "", 5).unwrap().len(), 2);
}

#[test]
fn repo_scope_covers_every_worktree_of_the_clone() {
    let d = tmp("worktrees");
    let (sessions, db) = (d.join("sessions"), d.join("index.db"));
    let clone = d.join("clone");
    init_repo(&clone);
    let (wt_a, wt_b) = (d.join("wt-a"), d.join("wt-b"));
    git(&clone, &["worktree", "add", "-q", "-b", "a", wt_a.to_str().unwrap()]);
    git(&clone, &["worktree", "add", "-q", "-b", "b", wt_b.to_str().unwrap()]);
    let other = d.join("other");
    init_repo(&other);

    write_session(&sessions, &wt_a, "a.jsonl", "sess-a", "A", &[msg("1", "user", text("deploy the blue service"))]);
    write_session(&sessions, &wt_b, "b.jsonl", "sess-b", "B", &[msg("2", "user", text("deploy from the other tree"))]);
    write_session(&sessions, &other, "o.jsonl", "sess-o", "O", &[msg("3", "user", text("deploy the unrelated thing"))]);
    sync(&sessions, &db);

    let r = rt();
    let (key_a, key_b, key_o) =
        (r.block_on(repo_key(&wt_a)), r.block_on(repo_key(&wt_b)), r.block_on(repo_key(&other)));
    assert_eq!(key_a, key_b);
    assert_ne!(key_a, key_o);
    let titles = |key: Option<&str>| {
        let mut t: Vec<String> =
            query::run(&db, "deploy", key, "", 10).unwrap().into_iter().filter_map(|s| s.title).collect();
        t.sort();
        t
    };
    assert_eq!(titles(Some(&key_a)), ["A", "B"]);
    assert_eq!(titles(Some(&key_b)), ["A", "B"]);
    assert_eq!(titles(Some(&key_o)), ["O"]);
    assert_eq!(titles(None), ["A", "B", "O"]);
}

#[test]
fn queries_are_quoted_unless_they_are_fts_syntax() {
    assert_eq!(query::quote_terms("foo \"bar\""), "\"foo\" \"\"\"bar\"\"\"");
    let d = tmp("quoting");
    let (sessions, db) = (d.join("sessions"), d.join("index.db"));
    write_session(
        &sessions,
        &d,
        "a.jsonl",
        "sess-a",
        "",
        &[msg("1", "user", text("the deploy script runs nightly")), msg("2", "assistant", text("deploy and rollback"))],
    );
    sync(&sessions, &db);
    assert_eq!(hits(&db, "deploy script"), 1, "plain words are an implicit AND");
    assert_eq!(hits(&db, "\"deploy script\""), 1, "a phrase passes through");
    assert_eq!(hits(&db, "\"script deploy\""), 0, "phrase order matters");
    assert_eq!(hits(&db, "deploy AND rollback"), 1, "operators pass through");
    // Syntax sqlite rejects falls back to quoted terms instead of failing.
    for q in ["deploy AND", "foo (bar", "c++ (x)", "\"unbalanced", "NOT"] {
        assert!(query::run(&db, q, None, "", 5).is_ok(), "{q}");
    }
}

#[test]
fn hits_come_with_one_message_either_side() {
    let d = tmp("window");
    let (sessions, db) = (d.join("sessions"), d.join("index.db"));
    write_session(
        &sessions,
        &d,
        "a.jsonl",
        "sess-a",
        "",
        &[
            msg("1", "user", text("setup notes")),
            msg("2", "assistant", text("the widget listens on port 8080")),
            msg("3", "user", text("thanks")),
        ],
    );
    sync(&sessions, &db);
    let found = query::run(&db, "port 8080", None, "", 5).unwrap();
    assert_eq!(found.len(), 1);
    let window = &found[0].hits[0];
    let shape: Vec<(&str, bool)> = window.iter().map(|l| (l.role.as_str(), l.hit)).collect();
    assert_eq!(shape, [("user", false), ("assistant", true), ("user", false)]);
    assert_eq!(found[0].started, "2026-11-01T10:00:00.000Z");
}

#[test]
fn clip_cuts_by_characters_and_flattens_whitespace() {
    let long = "é".repeat(500);
    let c = clip(&long, 400);
    assert_eq!(c.chars().count(), 401);
    assert!(c.ends_with('…'));
    assert_eq!(clip("short  text\n here", 400), "short text here");
}

#[test]
fn an_empty_query_is_an_error() {
    let (text, is_error) = rt().block_on(super::call(&json!({"query": "  "}), &Ctx::default()));
    assert!(is_error);
    assert!(text.contains("query"));
}
