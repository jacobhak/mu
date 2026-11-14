//! Background review of sessions. The review model turns a session's new turns into edit ops, which go through
//! `Mem::edit`, the same path as the `memory` tool. `pre_compaction` starts a review; `session_end` queues the
//! session, and the first hook of the next process drains the queue.

use crate::mem::{Config, EditError, Mem};
use crate::store::{self, Caps, Op, State};
use crate::transcript::{self, CAP};
use fs4::FileExt;
use mu_common::fs::write_atomic;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

const PROMPT: &str = include_str!("../prompts/review.md");
/// Queue entries drained per process start, oldest first.
const DRAIN_LIMIT: usize = 3;
/// A rejected reply gets one more try, with the error appended to the prompt.
const ATTEMPTS: usize = 2;
/// Failed drains after which a queued session is dropped, with one notice.
const MAX_FAILED_DRAINS: u32 = 3;
const MAX_TOKENS: u64 = 2048;
const NO_MODEL: &str = "no model is configured for the memory role (set @smol or @default)";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Job {
    pub session_file: PathBuf,
    pub session_id: String,
    pub cwd: PathBuf,
    /// Drains of this queue entry that failed.
    #[serde(default)]
    pub failed: u32,
}

#[derive(Debug, PartialEq)]
pub enum Done {
    /// The ops were applied (or none were needed) and the cursor moved.
    Reviewed,
    /// Nothing to review: the file is gone, review is off, or fewer than two new user turns.
    Skipped,
}

/// `session_end`: records the session for review at the next start. A second entry for the same session
/// replaces the first.
pub fn enqueue(job: &Job, cfg: &Config) -> Result<(), String> {
    if !cfg.review {
        return Ok(());
    }
    enqueue_in(&memory_root(), job)
}

/// `pre_compaction`: reviews the session in a spawned task; returns at once.
pub fn spawn_review(job: Job, cfg: Config) {
    tokio::spawn(async move {
        if let Err(e) = run_job(&memory_root(), &job, &cfg, &complete_rpc).await {
            report(&notify_ui, &job.session_id, &e);
        }
    });
}

/// First hook of a process: drains the queue in the background (at most once per process).
pub fn spawn_drain_once(cfg: Config) {
    static STARTED: AtomicBool = AtomicBool::new(false);
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    tokio::spawn(async move { drain(&memory_root(), &cfg, &complete_rpc, &notify_ui).await });
}

pub fn queue_len() -> usize {
    queue_len_in(&memory_root())
}

fn memory_root() -> PathBuf {
    mu_common::paths::profile_home().join("memory")
}

fn queue_len_in(root: &Path) -> usize {
    std::fs::read_to_string(queue_path(root)).map_or(0, |t| t.lines().filter(|l| !l.trim().is_empty()).count())
}

/// The model call. `Ok(None)` only when no model serves the role; any other failure is an error.
async fn complete_rpc(system: String, prompt: String) -> Result<Option<String>, String> {
    let r = mu_common::rpc::request(
        "core/complete",
        json!({"role": "memory", "system": system, "prompt": prompt, "maxTokens": MAX_TOKENS}),
    )
    .await;
    match r {
        Ok(r) => Ok(r["text"].as_str().map(str::to_string)),
        Err(e) if e.kind.as_deref() == Some(mu_common::rpc::kind::NO_MODEL) => Ok(None),
        Err(e) => Err(e.message),
    }
}

fn notify_ui(msg: &str) {
    mu_common::rpc::notify("ui/notify", json!({"level": "warning", "message": format!("memory: {msg}")}));
}

/// The missing model is reported once per process: it would repeat for every session.
fn report<N: Fn(&str)>(notify: &N, session_id: &str, err: &str) {
    static NO_MODEL_SHOWN: AtomicBool = AtomicBool::new(false);
    if err == NO_MODEL && NO_MODEL_SHOWN.swap(true, Ordering::SeqCst) {
        return;
    }
    notify(&format!("could not review session {session_id}: {err}"));
}

/// Reviews the session's new turns into the memory at `root`. On error the cursor stays where it was.
async fn run_job<C, Fut>(root: &Path, job: &Job, cfg: &Config, complete: &C) -> Result<Done, String>
where
    C: Fn(String, String) -> Fut,
    Fut: Future<Output = Result<Option<String>, String>>,
{
    if !cfg.review || !job.session_file.is_file() {
        return Ok(Done::Skipped);
    }
    let session = session_guard(&job.session_id);
    let _turn = session.lock().await;
    let mem = open_at(root, &job.cwd, cfg).await;
    let loaded =
        mu_session::session::format::load(&job.session_file).map_err(|e| format!("cannot read the session: {e:#}"))?;
    let cursor = read_cursor(root, &job.session_id);
    let Some(t) = transcript::extract(&loaded, cursor.as_deref(), CAP) else {
        return Ok(Done::Skipped);
    };
    let prefix = format!("review {} turns {}-{}", job.session_id, t.first_turn, t.last_turn);
    let base = user_prompt(&mem.load()?, &mem.cfg.caps, &t.text);
    let mut prompt = base.clone();
    let mut attempt = 1;
    loop {
        let reply = complete(PROMPT.to_string(), prompt.clone()).await?.ok_or(NO_MODEL)?;
        match apply(&mem, job, &prefix, &reply).await {
            Ok(()) => {
                write_cursor(root, &job.session_id, &t.last_entry)?;
                return Ok(Done::Reviewed);
            }
            Err(EditError::Rejected(e)) if attempt < ATTEMPTS => {
                attempt += 1;
                prompt = format!(
                    "{base}\n\nYour previous reply was rejected: {e}\nPrevious reply:\n{reply}\n\nReply again with only the corrected JSON array."
                );
            }
            Err(e) => return Err(e.into_message()),
        }
    }
}

/// A reply that does not parse or whose ops do not apply is `Rejected`; a failure of the lock, files or git is
/// `Failed` and says nothing about the reply.
async fn apply(mem: &Mem, job: &Job, prefix: &str, reply: &str) -> Result<(), EditError> {
    let ops = parse_ops(reply).map_err(EditError::Rejected)?;
    if ops.is_empty() {
        return Ok(());
    }
    mem.edit_classified(ops, &job.cwd, prefix).await.map(|_| ())
}

/// The ops in a reply: the whole reply, a code fence's body, or the outermost array, in that order.
fn parse_ops(reply: &str) -> Result<Vec<Op>, String> {
    let text = reply.trim();
    let outer = text.find('[').zip(text.rfind(']')).filter(|(a, b)| a < b).map(|(a, b)| &text[a..=b]);
    let mut err = String::from("no JSON array found");
    for candidate in [Some(text), fenced(text), outer].into_iter().flatten() {
        match serde_json::from_str::<Vec<Op>>(candidate) {
            Ok(ops) => return Ok(ops),
            Err(e) => err = e.to_string(),
        }
    }
    Err(format!("the reply is not a JSON array of operations: {err}"))
}

fn fenced(text: &str) -> Option<&str> {
    let (_, rest) = text.split_once("```")?;
    let (_, body) = rest.split_once('\n')?;
    let (inner, _) = body.split_once("```")?;
    Some(inner.trim())
}

fn user_prompt(state: &State, caps: &Caps, transcript: &str) -> String {
    let entries = |es: &[String]| -> String {
        if es.is_empty() {
            return "(empty)\n".into();
        }
        es.iter().map(|e| format!("- {e}\n")).collect()
    };
    let mut s = format!(
        "## user.md (cap {} characters, now {})\n{}\n",
        caps.user,
        store::size_of(&state.user),
        entries(&state.user)
    );
    s.push_str(&format!(
        "## memory.md of this repository (cap {} characters, now {})\n{}\n",
        caps.project,
        store::size_of(&state.project),
        entries(&state.project)
    ));
    s.push_str("## Notes\n");
    if state.notes.is_empty() {
        s.push_str("(none)\n");
    }
    for (slug, n) in &state.notes {
        s.push_str(&format!("- {slug} ({}): {}\n", n.kind.name(), n.description));
    }
    s.push_str("\n## Session transcript: new turns only\n\n");
    s.push_str(transcript);
    s
}

/// `Mem` for `root`, with the repository of `cwd`.
async fn open_at(root: &Path, cwd: &Path, cfg: &Config) -> Mem {
    let mut mem = Mem::open(cwd, cfg.clone()).await;
    mem.root = root.to_path_buf();
    mem
}

/// A `Mem` at `root` for its lock and state files only.
fn at(root: &Path, cfg: &Config) -> Mem {
    Mem { root: root.to_path_buf(), checkout: root.to_path_buf(), key: String::new(), cfg: cfg.clone() }
}

/// One review per session at a time. A second one waits, then reads the cursor the first one wrote.
fn session_guard(session_id: &str) -> Arc<tokio::sync::Mutex<()>> {
    static GUARDS: OnceLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> = OnceLock::new();
    let mut g = GUARDS.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner());
    g.entry(session_id.to_string()).or_default().clone()
}

fn cursor_path(root: &Path, session_id: &str) -> PathBuf {
    let safe: String =
        session_id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).collect();
    root.join(".state").join("cursors").join(format!("{safe}.json"))
}

fn read_cursor(root: &Path, session_id: &str) -> Option<String> {
    let text = std::fs::read_to_string(cursor_path(root, session_id)).ok()?;
    serde_json::from_str::<Value>(&text).ok()?["entry"].as_str().map(str::to_string)
}

fn write_cursor(root: &Path, session_id: &str, entry: &str) -> Result<(), String> {
    let p = cursor_path(root, session_id);
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
    }
    write_atomic(&p, json!({ "entry": entry }).to_string().as_bytes()).map_err(|e| format!("{}: {e}", p.display()))
}

fn queue_path(root: &Path) -> PathBuf {
    root.join(".state").join("queue.jsonl")
}

fn read_queue(root: &Path) -> Vec<Job> {
    std::fs::read_to_string(queue_path(root))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

fn write_queue(root: &Path, jobs: &[Job]) -> Result<(), String> {
    let mut text = String::new();
    for j in jobs {
        text.push_str(&serde_json::to_string(j).map_err(|e| e.to_string())?);
        text.push('\n');
    }
    let p = queue_path(root);
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
    }
    write_atomic(&p, text.as_bytes()).map_err(|e| format!("{}: {e}", p.display()))
}

fn enqueue_in(root: &Path, job: &Job) -> Result<(), String> {
    let _lock = lock_sync(root)?;
    let mut jobs = read_queue(root);
    match jobs.iter_mut().find(|j| j.session_id == job.session_id) {
        Some(j) => *j = job.clone(),
        None => jobs.push(job.clone()),
    }
    write_queue(root, &jobs)
}

/// Removes the session's entries from the queue, under the store lock.
async fn dequeue(root: &Path, cfg: &Config, session_id: &str) -> Result<(), String> {
    let _lock = at(root, cfg).lock().await?;
    let jobs: Vec<Job> = read_queue(root).into_iter().filter(|j| j.session_id != session_id).collect();
    write_queue(root, &jobs)
}

struct SyncLock(std::fs::File);

impl Drop for SyncLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.0);
    }
}

/// The store lock for `enqueue`, which runs in a synchronous hook. It waits about a second at most: the
/// runtime is blocked meanwhile, so a review of this process holding the lock could not finish.
fn lock_sync(root: &Path) -> Result<SyncLock, String> {
    let dir = root.join(".state");
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(dir.join("lock"))
        .map_err(|e| format!("cannot open the memory lock: {e}"))?;
    for _ in 0..20 {
        match FileExt::try_lock(&f) {
            Ok(()) => return Ok(SyncLock(f)),
            Err(fs4::TryLockError::WouldBlock) => std::thread::sleep(Duration::from_millis(50)),
            Err(fs4::TryLockError::Error(e)) => return Err(format!("cannot lock the memory directory: {e}")),
        }
    }
    Err("the memory directory is busy, so the session was not queued for review".into())
}

/// Moves a failed entry to the back of the queue with its failure count raised, so later sessions are reached.
/// Returns `true` when the entry was dropped for failing `MAX_FAILED_DRAINS` times.
async fn requeue_failed(root: &Path, cfg: &Config, session_id: &str) -> Result<bool, String> {
    let _lock = at(root, cfg).lock().await?;
    let mut jobs = read_queue(root);
    let Some(i) = jobs.iter().position(|j| j.session_id == session_id) else { return Ok(false) };
    let mut job = jobs.remove(i);
    job.failed += 1;
    let dropped = job.failed >= MAX_FAILED_DRAINS;
    if !dropped {
        jobs.push(job);
    }
    write_queue(root, &jobs)?;
    Ok(dropped)
}

/// Reviews the first `DRAIN_LIMIT` queued sessions in turn. An entry leaves the queue once its review succeeds,
/// its session file is gone, or it has failed `MAX_FAILED_DRAINS` drains (one notice); other failures go to the
/// back of the queue and are reported. A missing model counts for nothing: it is not the session's fault.
async fn drain<C, Fut, N>(root: &Path, cfg: &Config, complete: &C, notify: &N)
where
    C: Fn(String, String) -> Fut,
    Fut: Future<Output = Result<Option<String>, String>>,
    N: Fn(&str),
{
    for job in read_queue(root).into_iter().take(DRAIN_LIMIT) {
        let failure = match run_job(root, &job, cfg, complete).await {
            Ok(_) => dequeue(root, cfg, &job.session_id).await.err(),
            Err(e) => Some(e),
        };
        let Some(e) = failure else { continue };
        if e == NO_MODEL {
            report(notify, &job.session_id, &e);
            continue;
        }
        match requeue_failed(root, cfg, &job.session_id).await {
            Ok(true) => notify(&format!(
                "giving up on session {} after {MAX_FAILED_DRAINS} failed attempts: {e}",
                job.session_id
            )),
            Ok(false) => report(notify, &job.session_id, &e),
            Err(q) => report(notify, &job.session_id, &format!("{e} (and the queue could not be updated: {q})")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gitx;
    use crate::transcript::fixture::{assistant, user, write};
    use std::cell::RefCell;
    use std::collections::VecDeque;

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
    }

    struct Env {
        dir: PathBuf,
        root: PathBuf,
        cwd: PathBuf,
    }

    fn env(name: &str) -> Env {
        let dir = std::env::temp_dir().join(format!("mu-memory-review-{name}-{}", uuid::Uuid::new_v4()));
        let cwd = dir.join("repo");
        std::fs::create_dir_all(&cwd).unwrap();
        Env { root: dir.join("memory"), dir, cwd }
    }

    /// A session of `turns` user turns, each answered once. Entries are e1..e(2·turns).
    fn session(env: &Env, id: &str, turns: usize) -> Job {
        let file = env.dir.join(format!("{id}.jsonl"));
        let entries: Vec<Value> = (1..=turns)
            .flat_map(|n| {
                [
                    user(&format!("Turn {n} instruction")),
                    assistant(json!([{"type": "text", "text": format!("Answer {n}")}])),
                ]
            })
            .collect();
        write(&file, &entries);
        Job { session_file: file, session_id: id.into(), cwd: env.cwd.clone(), failed: 0 }
    }

    /// Replies in order; the last one repeats. Records every prompt.
    struct Fake {
        replies: RefCell<VecDeque<Option<String>>>,
        prompts: RefCell<Vec<String>>,
    }

    impl Fake {
        fn new(replies: &[Option<&str>]) -> Self {
            Self {
                replies: RefCell::new(replies.iter().map(|r| r.map(String::from)).collect()),
                prompts: RefCell::new(Vec::new()),
            }
        }

        fn answer(&self, prompt: String) -> Result<Option<String>, String> {
            self.prompts.borrow_mut().push(prompt);
            let mut q = self.replies.borrow_mut();
            let next = if q.len() > 1 { q.pop_front() } else { q.front().cloned() };
            Ok(next.unwrap_or(Some("[]".into())))
        }

        fn calls(&self) -> usize {
            self.prompts.borrow().len()
        }
    }

    fn cfg() -> Config {
        Config::default()
    }

    #[test]
    fn a_review_applies_its_ops_commits_and_moves_the_cursor() {
        let env = env("ok");
        let job = session(&env, "s1", 2);
        let fake =
            Fake::new(&[Some("```json\n[{\"op\": \"add\", \"file\": \"user\", \"text\": \"Prefers tabs\"}]\n```")]);
        let model = |_: String, p: String| std::future::ready(fake.answer(p));
        rt().block_on(async {
            assert_eq!(run_job(&env.root, &job, &cfg(), &model).await.unwrap(), Done::Reviewed);
            assert_eq!(at(&env.root, &cfg()).load().unwrap().user, ["Prefers tabs"]);
            let log = gitx::log(&env.root, 5).await.unwrap();
            assert_eq!(log[0].message, "review s1 turns 1-2: add user");
            assert_eq!(read_cursor(&env.root, "s1").as_deref(), Some("e4"));
            let prompt = fake.prompts.borrow()[0].clone();
            assert!(prompt.contains("## user.md") && prompt.contains("Turn 2 instruction"), "{prompt}");
            // Nothing after the cursor: skipped without a model call.
            assert_eq!(run_job(&env.root, &job, &cfg(), &model).await.unwrap(), Done::Skipped);
            assert_eq!(fake.calls(), 1);
        });
    }

    #[test]
    fn fewer_than_two_new_user_turns_skip_without_a_model_call() {
        let env = env("short");
        let job = session(&env, "s2", 1);
        let fake = Fake::new(&[None]);
        let model = |_: String, p: String| std::future::ready(fake.answer(p));
        rt().block_on(async {
            assert_eq!(run_job(&env.root, &job, &cfg(), &model).await.unwrap(), Done::Skipped);
            assert_eq!(fake.calls(), 0);
            assert!(read_cursor(&env.root, "s2").is_none());
        });
    }

    #[test]
    fn a_missing_session_file_is_skipped() {
        let env = env("gone");
        let mut job = session(&env, "s3", 2);
        job.session_file = env.dir.join("nowhere.jsonl");
        let fake = Fake::new(&[None]);
        let model = |_: String, p: String| std::future::ready(fake.answer(p));
        rt().block_on(async {
            assert_eq!(run_job(&env.root, &job, &cfg(), &model).await.unwrap(), Done::Skipped);
            assert_eq!(fake.calls(), 0);
        });
    }

    #[test]
    fn a_rejected_reply_is_retried_with_the_error_appended() {
        let env = env("parse");
        let job = session(&env, "s4", 2);
        let fake = Fake::new(&[
            Some("Sure, here is the memory update."),
            Some(r#"[{"op": "add", "file": "project", "text": "Releases are cut from main"}]"#),
        ]);
        let model = |_: String, p: String| std::future::ready(fake.answer(p));
        rt().block_on(async {
            assert_eq!(run_job(&env.root, &job, &cfg(), &model).await.unwrap(), Done::Reviewed);
            assert_eq!(fake.calls(), 2);
            let retry = fake.prompts.borrow()[1].clone();
            assert!(retry.contains("rejected") && retry.contains("not a JSON array"), "{retry}");
            assert_eq!(
                open_at(&env.root, &env.cwd, &cfg()).await.load().unwrap().project,
                ["Releases are cut from main"]
            );
        });
    }

    #[test]
    fn an_op_that_does_not_apply_is_retried_with_the_error() {
        let env = env("apply");
        let job = session(&env, "s5", 2);
        let fake = Fake::new(&[Some(r#"[{"op": "remove", "file": "user", "match": "nothing here"}]"#), Some("[]")]);
        let model = |_: String, p: String| std::future::ready(fake.answer(p));
        rt().block_on(async {
            assert_eq!(run_job(&env.root, &job, &cfg(), &model).await.unwrap(), Done::Reviewed);
            assert!(fake.prompts.borrow()[1].contains("hits no entry"));
            assert_eq!(read_cursor(&env.root, "s5").as_deref(), Some("e4"));
        });
    }

    #[test]
    fn two_bad_replies_fail_and_leave_the_cursor() {
        let env = env("fail");
        let job = session(&env, "s6", 2);
        let fake = Fake::new(&[Some("garbage"), Some("still garbage")]);
        let model = |_: String, p: String| std::future::ready(fake.answer(p));
        rt().block_on(async {
            assert!(run_job(&env.root, &job, &cfg(), &model).await.is_err());
            assert_eq!(fake.calls(), 2);
            assert!(read_cursor(&env.root, "s6").is_none());
        });
    }

    #[test]
    fn a_model_error_other_than_no_model_reaches_the_job_error() {
        let env = env("rate");
        let job = session(&env, "s8", 2);
        let model =
            |_: String, _: String| std::future::ready(Err::<Option<String>, String>("HTTP 429: rate limited".into()));
        rt().block_on(async {
            let e = run_job(&env.root, &job, &cfg(), &model).await.unwrap_err();
            assert!(e.contains("HTTP 429: rate limited"), "{e}");
        });
    }

    #[test]
    fn no_model_fails_without_retrying_and_is_reported_once() {
        let env = env("nomodel");
        let job = session(&env, "s7", 2);
        let fake = Fake::new(&[None]);
        let model = |_: String, p: String| std::future::ready(fake.answer(p));
        let seen = RefCell::new(Vec::<String>::new());
        let notify = |m: &str| seen.borrow_mut().push(m.to_string());
        rt().block_on(async {
            let e = run_job(&env.root, &job, &cfg(), &model).await.unwrap_err();
            assert_eq!(e, NO_MODEL);
            assert_eq!(fake.calls(), 1);
        });
        report(&notify, "s7", NO_MODEL);
        report(&notify, "s7", NO_MODEL);
        assert_eq!(seen.borrow().len(), 1);
    }

    #[test]
    fn parse_ops_accepts_fences_prose_and_empty_arrays() {
        assert!(parse_ops("[]").unwrap().is_empty());
        assert!(parse_ops("```json\n[]\n```").unwrap().is_empty());
        let ops =
            parse_ops("Here you go [memory]:\n```json\n[{\"op\": \"delete_note\", \"note\": \"old\"}]\n```\nDone.")
                .unwrap();
        assert_eq!(ops, [Op::DeleteNote { note: "old".into() }]);
        assert!(parse_ops("[{\"op\": \"update\", \"file\": \"user\"}]").is_err());
        assert!(parse_ops("nothing").is_err());
    }

    #[test]
    fn enqueue_dedupes_by_session_and_keeps_others() {
        let env = env("enqueue");
        let a = session(&env, "a", 2);
        let b = session(&env, "b", 2);
        enqueue_in(&env.root, &a).unwrap();
        enqueue_in(&env.root, &a).unwrap();
        enqueue_in(&env.root, &b).unwrap();
        let ids: Vec<String> = read_queue(&env.root).into_iter().map(|j| j.session_id).collect();
        assert_eq!(ids, ["a", "b"]);
        assert_eq!(queue_len_in(&env.root), 2);
    }

    #[test]
    fn drain_takes_three_oldest_and_drops_done_and_gone_entries() {
        let env = env("drain");
        let gone =
            Job { session_file: env.dir.join("gone.jsonl"), session_id: "x".into(), cwd: env.cwd.clone(), failed: 0 };
        let jobs = [gone, session(&env, "a", 2), session(&env, "b", 2), session(&env, "c", 2), session(&env, "d", 2)];
        for j in &jobs {
            enqueue_in(&env.root, j).unwrap();
        }
        let fake = Fake::new(&[Some("[]")]);
        let model = |_: String, p: String| std::future::ready(fake.answer(p));
        let seen = RefCell::new(Vec::<String>::new());
        let notify = |m: &str| seen.borrow_mut().push(m.to_string());
        rt().block_on(drain(&env.root, &cfg(), &model, &notify));
        assert_eq!(fake.calls(), 2, "the gone session needs no call");
        let left: Vec<String> = read_queue(&env.root).into_iter().map(|j| j.session_id).collect();
        assert_eq!(left, ["c", "d"]);
        assert!(seen.borrow().is_empty());
    }

    #[test]
    fn a_failed_review_stays_queued_and_is_reported() {
        let env = env("stays");
        enqueue_in(&env.root, &session(&env, "f", 2)).unwrap();
        let fake = Fake::new(&[Some("garbage")]);
        let model = |_: String, p: String| std::future::ready(fake.answer(p));
        let seen = RefCell::new(Vec::<String>::new());
        let notify = |m: &str| seen.borrow_mut().push(m.to_string());
        rt().block_on(drain(&env.root, &cfg(), &model, &notify));
        assert_eq!(queue_len_in(&env.root), 1);
        let seen = seen.borrow();
        assert_eq!(seen.len(), 1);
        assert!(seen[0].contains("could not review session f"), "{}", seen[0]);
    }

    #[test]
    fn failing_sessions_at_the_head_do_not_starve_later_ones() {
        let env = env("starve");
        for id in ["a", "b", "c", "d"] {
            enqueue_in(&env.root, &session(&env, id, 2)).unwrap();
        }
        let ids = || read_queue(&env.root).into_iter().map(|j| j.session_id).collect::<Vec<_>>();
        let bad = Fake::new(&[Some("garbage")]);
        let model = |_: String, p: String| std::future::ready(bad.answer(p));
        let seen = RefCell::new(Vec::<String>::new());
        let notify = |m: &str| seen.borrow_mut().push(m.to_string());
        rt().block_on(drain(&env.root, &cfg(), &model, &notify));
        assert_eq!(ids(), ["d", "a", "b", "c"]);
        assert!(read_queue(&env.root).iter().skip(1).all(|j| j.failed == 1));
        let good = Fake::new(&[Some("[]")]);
        let model = |_: String, p: String| std::future::ready(good.answer(p));
        rt().block_on(drain(&env.root, &cfg(), &model, &notify));
        assert_eq!(ids(), ["c"], "d was reached on the second drain");
    }

    #[test]
    fn a_session_that_keeps_failing_is_dropped_with_one_notice() {
        let env = env("giveup");
        enqueue_in(&env.root, &session(&env, "f", 2)).unwrap();
        let fake = Fake::new(&[Some("garbage")]);
        let model = |_: String, p: String| std::future::ready(fake.answer(p));
        let seen = RefCell::new(Vec::<String>::new());
        let notify = |m: &str| seen.borrow_mut().push(m.to_string());
        for _ in 0..MAX_FAILED_DRAINS {
            rt().block_on(drain(&env.root, &cfg(), &model, &notify));
        }
        assert_eq!(queue_len_in(&env.root), 0);
        let seen = seen.borrow();
        assert_eq!(seen.len(), MAX_FAILED_DRAINS as usize);
        assert_eq!(seen.iter().filter(|m| m.contains("giving up")).count(), 1);
    }

    #[test]
    fn an_infrastructure_error_is_not_fed_back_as_a_rejected_reply() {
        let env = env("infra");
        let job = session(&env, "s9", 2);
        let calls = RefCell::new(Vec::<String>::new());
        let model = |_: String, p: String| {
            calls.borrow_mut().push(p);
            // The memory file becomes unreadable between the prompt and the edit.
            std::fs::create_dir_all(&env.root).unwrap();
            std::fs::write(env.root.join("user.md"), b"\xff\xfe").unwrap();
            std::future::ready(Ok::<_, String>(Some(r#"[{"op": "add", "file": "user", "text": "x"}]"#.to_string())))
        };
        rt().block_on(async {
            let e = run_job(&env.root, &job, &cfg(), &model).await.unwrap_err();
            assert!(e.contains("user.md"), "{e}");
            assert!(!e.contains("rejected"), "{e}");
        });
        assert_eq!(calls.borrow().len(), 1, "no second attempt with the error as feedback");
        assert!(read_cursor(&env.root, "s9").is_none());
    }

    #[test]
    fn one_session_has_one_guard() {
        assert!(Arc::ptr_eq(&session_guard("same"), &session_guard("same")));
        assert!(!Arc::ptr_eq(&session_guard("same"), &session_guard("other")));
    }
}
