//! Who has a session open: what this process announces about itself, and the owner record
//! (`<profile home>/run/<id>.json`) it holds under `flock` while it has a session open.
use super::list::{id_of, valid_id};
use mu_base::process::try_flock;
use mu_common::paths;
use mu_common::time::iso_ms;
use serde_json::{Value, json};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

/// How long `lock` keeps retrying a busy lock. A process forked while the session is open keeps
/// a copy of the fd until it execs, so a lock released a moment ago can still look busy. A real
/// holder keeps the lock indefinitely and is refused once this runs out.
pub(super) const LOCK_RETRY: std::time::Duration = std::time::Duration::from_millis(500);

static PRINT_RUN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Marks this process as a `mu -p` run. Call it only once the SIGUSR1 handler is installed and
/// before any session is locked: an owner record that says `print_run` promises that
/// `kill -USR1` stops the run instead of killing it.
pub fn set_print_run() {
    PRINT_RUN.store(true, std::sync::atomic::Ordering::SeqCst);
}

static INBOX: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Marks this process as taking `mu notify` wake-ups. Call it only once the SIGUSR2 handler is
/// installed and before any session is locked: an owner record that says `inbox` promises that
/// `kill -USR2` wakes the session instead of killing it.
pub fn set_inbox() {
    INBOX.store(true, std::sync::atomic::Ordering::SeqCst);
}

static ABORT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Marks this process as an interactive mu that cancels its current turn on SIGUSR1 (the jobs panel's
/// `s`). Call it only once that handler is installed and before any session is locked: an owner record
/// that says `abort` promises `kill -USR1` aborts the turn instead of killing the process.
pub fn set_abort_signal() {
    ABORT.store(true, std::sync::atomic::Ordering::SeqCst);
}

/// The client an agent serves: its host name and when it attached (unix seconds).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ClientInfo {
    pub host: String,
    pub since: i64,
}

/// What an agent adds to every owner record this process holds: a request waits for the user, and
/// who is attached.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Attention {
    pub waiting: bool,
    pub client: Option<ClientInfo>,
}

static ATTENTION: std::sync::Mutex<Attention> = std::sync::Mutex::new(Attention { waiting: false, client: None });

/// The owner records this process holds, by path.
static CLAIMED: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());

/// Changes what the owner records of this process say about its client, now and in records
/// claimed later.
pub fn set_attention(change: impl FnOnce(&mut Attention)) {
    let Ok(mut now) = ATTENTION.lock() else { return };
    change(&mut now);
    let attention = now.clone();
    drop(now);
    let paths = CLAIMED.lock().map(|c| c.clone()).unwrap_or_default();
    for p in paths {
        rewrite(&p, &attention);
    }
}

/// Rewrites `waiting` and `client` of our own record in place. The new text is padded to the old
/// length and written in one call, so a reader never sees a torn or empty file.
fn rewrite(path: &Path, a: &Attention) {
    use std::io::Read;
    use std::os::unix::fs::FileExt;
    let Ok(mut f) = OpenOptions::new().read(true).write(true).open(path) else { return };
    let mut text = String::new();
    if f.read_to_string(&mut text).is_err() {
        return;
    }
    let Ok(mut v) = serde_json::from_str::<Value>(&text) else { return };
    if v["pid"].as_u64() != Some(u64::from(std::process::id())) {
        return;
    }
    set_attention_fields(&mut v, a);
    let mut out = v.to_string();
    while out.len() < text.len() {
        out.push(' ');
    }
    let _ = f.write_all_at(out.as_bytes(), 0);
}

fn set_attention_fields(rec: &mut Value, a: &Attention) {
    match a.waiting {
        true => rec["waiting"] = json!(true),
        false => {
            if let Some(o) = rec.as_object_mut() {
                o.remove("waiting");
            }
        }
    }
    match &a.client {
        Some(c) => rec["client"] = json!(c),
        None => {
            if let Some(o) = rec.as_object_mut() {
                o.remove("client");
            }
        }
    }
}

/// What a process is, as its owner records name it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunKind {
    Tui,
    Print,
    Subagent,
    Acp,
    /// `mu agent`: the agent of a session, with a client attached or not.
    Agent,
}

impl RunKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Tui => "tui",
            Self::Print => "print",
            Self::Subagent => "subagent",
            Self::Acp => "acp",
        }
    }
}

#[derive(Clone, Default)]
struct RunInfo {
    kind: Option<RunKind>,
    agent: Option<String>,
}

static RUN_INFO: std::sync::Mutex<RunInfo> = std::sync::Mutex::new(RunInfo { kind: None, agent: None });

/// What this process is, for its owner records, and the custom agent it runs as. Call it before
/// any session is created.
pub fn set_run_info(kind: RunKind, agent: Option<&str>) {
    if let Ok(mut i) = RUN_INFO.lock() {
        *i = RunInfo { kind: Some(kind), agent: agent.map(String::from) };
    }
}

/// Who has a session open.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Owner {
    Free,
    Live {
        pid: u32,
        print_run: bool,
        inbox: bool,
        started: String,
    },
    /// Locked, but by a process that left no valid owner record.
    Unknown,
}

pub(super) struct LiveRecord {
    pub(super) pid: u32,
    pub(super) print_run: bool,
    inbox: bool,
    started: String,
    session: PathBuf,
    kind: Option<String>,
    agent: Option<String>,
    cwd: Option<PathBuf>,
    parent: Option<PathBuf>,
    abort: bool,
    waiting: bool,
    client: Option<ClientInfo>,
}

fn record_path(session: &Path) -> Option<PathBuf> {
    let id = id_of(session);
    valid_id(&id).then(|| paths::profile_home().join("run").join(format!("{id}.json")))
}

fn same_file(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
    }
}

/// The owner record at `rec`, trusted only while the record's own lock is held (so the writer is
/// alive). Records from before `kind`, `agent`, `cwd` and `parent` existed still read.
fn read_record(rec: &Path) -> Option<LiveRecord> {
    let f = File::open(rec).ok()?;
    match try_flock(&f, libc::LOCK_SH) {
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
        _ => return None,
    }
    let v: Value = serde_json::from_reader(&f).ok()?;
    let text = |k: &str| v[k].as_str().filter(|s| !s.is_empty()).map(String::from);
    Some(LiveRecord {
        pid: u32::try_from(v["pid"].as_u64()?).ok()?,
        print_run: v["print_run"].as_bool()?,
        inbox: v["inbox"].as_bool().unwrap_or(false),
        started: v["started"].as_str()?.to_string(),
        session: PathBuf::from(v["session"].as_str()?),
        kind: text("kind"),
        agent: text("agent"),
        cwd: text("cwd").map(PathBuf::from),
        parent: text("parent").map(PathBuf::from),
        abort: v["abort"].as_bool().unwrap_or(false),
        waiting: v["waiting"].as_bool().unwrap_or(false),
        client: serde_json::from_value(v["client"].clone()).ok(),
    })
}

/// The owner record of `session`, trusted only while it names this very file.
pub(super) fn live_record(session: &Path) -> Option<LiveRecord> {
    let r = read_record(&record_path(session)?)?;
    same_file(&r.session, session).then_some(r)
}

/// A live mu process with a session open, from its owner record.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LiveSession {
    pub path: PathBuf,
    pub pid: u32,
    pub print_run: bool,
    pub inbox: bool,
    pub started: String,
    /// `tui`, `print`, `subagent` or `acp`; a record from an older mu is guessed from its flags.
    pub kind: Option<String>,
    pub agent: Option<String>,
    pub cwd: Option<PathBuf>,
    /// The session that spawned this one.
    pub parent: Option<PathBuf>,
    /// SIGUSR1 cancels the current turn (an interactive mu that said so).
    pub abort: bool,
    /// An agent has a question or approval that waits for a client.
    pub waiting: bool,
    /// The client an agent serves, when one is attached.
    pub client: Option<ClientInfo>,
}

/// Every live owner record of this profile (`<profile home>/run`). A record whose writer is gone (its
/// lock is free) or whose session file is missing is left out.
pub fn live_sessions() -> Vec<LiveSession> {
    live_sessions_in(&paths::profile_home().join("run"))
}

/// `live_sessions` for the owner records in `run_dir`.
pub fn live_sessions_in(run_dir: &Path) -> Vec<LiveSession> {
    let Ok(rd) = std::fs::read_dir(run_dir) else { return Vec::new() };
    rd.flatten()
        .filter(|e| {
            let n = e.file_name();
            let n = n.to_string_lossy();
            !n.starts_with('.') && n.ends_with(".json")
        })
        .filter_map(|e| read_record(&e.path()))
        .filter(|r| r.session.is_file())
        .map(|r| {
            let parent = r.parent.clone().or_else(|| header_parent(&r.session));
            let kind = r
                .kind
                .clone()
                .or_else(|| if r.print_run { Some("print".to_string()) } else { r.inbox.then(|| "tui".to_string()) });
            LiveSession {
                path: r.session,
                pid: r.pid,
                print_run: r.print_run,
                inbox: r.inbox,
                started: r.started,
                kind,
                agent: r.agent,
                cwd: r.cwd,
                parent,
                abort: r.abort,
                waiting: r.waiting,
                client: r.client,
            }
        })
        .collect()
}

/// `parentSession` from a session file's header, which sits in its first few lines.
pub(super) fn header_parent(session: &Path) -> Option<PathBuf> {
    use std::io::BufRead;
    let f = File::open(session).ok()?;
    std::io::BufReader::new(f).lines().take(3).map_while(Result::ok).find_map(|l| {
        let v: Value = serde_json::from_str(&l).ok()?;
        (v["type"] == "session").then(|| v["parentSession"].as_str().map(PathBuf::from)).flatten()
    })
}

/// True when nobody holds the session's lock.
pub fn is_free(session: &Path) -> bool {
    match File::open(session) {
        Ok(f) => try_flock(&f, libc::LOCK_SH).is_ok(),
        Err(_) => true,
    }
}

/// Looks up who has `session` open. Retries `Unknown` for `LOCK_RETRY`: a lock released a moment
/// ago can look busy (forked fds), and a new owner writes its record just after locking.
pub fn owner(session: &Path) -> Owner {
    let deadline = std::time::Instant::now() + LOCK_RETRY;
    loop {
        if is_free(session) {
            return Owner::Free;
        }
        if let Some(o) = live_record(session) {
            return Owner::Live { pid: o.pid, print_run: o.print_run, inbox: o.inbox, started: o.started };
        }
        if std::time::Instant::now() >= deadline {
            return Owner::Unknown;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

/// Removes records in `dir` whose writer is gone (crashed or killed before it could unlink).
/// A record is stale when nobody holds its lock. The probe is a shared lock, like the one
/// `live_record` takes: a live writer holds an exclusive lock, so live records stay, and a stale
/// record never looks live to a concurrent `owner()` while it is probed. A claimer's `rename` is
/// not blocked by any lock; the dev/ino re-check catches it, but a rename landing between the check
/// and the unlink still loses that new record. The result is then a missing record (`owner` says
/// `Unknown`, `mu take` signals nothing), never a wrong signal.
fn sweep_stale(dir: &Path) {
    use std::os::unix::fs::MetadataExt;
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let name = e.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') || !name.ends_with(".json") {
            continue;
        }
        let path = e.path();
        let Ok(f) = File::open(&path) else { continue };
        if try_flock(&f, libc::LOCK_SH).is_err() {
            continue;
        }
        if let (Ok(a), Ok(b)) = (std::fs::metadata(&path), f.metadata())
            && a.dev() == b.dev()
            && a.ino() == b.ino()
        {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// `<profile home>/run/<id>.json`, held under `flock` by the process that has the session open.
/// Best effort: any failure just means no record.
pub(super) struct OwnerRecord {
    path: PathBuf,
    file: File,
}

/// What an owner record says besides who holds the lock.
#[derive(Default)]
struct Extra {
    kind: Option<RunKind>,
    agent: Option<String>,
    cwd: Option<PathBuf>,
    parent: Option<PathBuf>,
    abort: bool,
    attention: Attention,
}

impl OwnerRecord {
    /// Call right after taking the session lock. `cwd` defaults to the process's.
    pub(super) fn claim(session: &Path, cwd: Option<&Path>, parent: Option<&Path>) -> Option<Self> {
        let info = RUN_INFO.lock().map(|i| i.clone()).unwrap_or_default();
        let extra = Extra {
            kind: info.kind,
            agent: info.agent,
            cwd: cwd.map(Path::to_path_buf).or_else(|| std::env::current_dir().ok()),
            parent: parent.map(Path::to_path_buf),
            abort: ABORT.load(std::sync::atomic::Ordering::SeqCst),
            attention: ATTENTION.lock().map(|a| a.clone()).unwrap_or_default(),
        };
        Self::claim_with(
            session,
            PRINT_RUN.load(std::sync::atomic::Ordering::SeqCst),
            INBOX.load(std::sync::atomic::Ordering::SeqCst),
            &extra,
        )
    }

    #[cfg(test)]
    fn claim_as(session: &Path, print_run: bool, inbox: bool) -> Option<Self> {
        Self::claim_with(session, print_run, inbox, &Extra::default())
    }

    fn claim_with(session: &Path, print_run: bool, inbox: bool, extra: &Extra) -> Option<Self> {
        let path = record_path(session)?;
        let dir = path.parent()?;
        mu_common::fs::create_private_dir(dir).ok()?;
        sweep_stale(dir);
        let pid = std::process::id();
        let tmp = dir.join(format!(".{}.{pid}.tmp", id_of(session)));
        let _ = std::fs::remove_file(&tmp);
        let mut file = OpenOptions::new().write(true).create_new(true).open(&tmp).ok()?;
        let done = (|| -> std::io::Result<()> {
            try_flock(&file, libc::LOCK_EX)?;
            let abs = std::fs::canonicalize(session).unwrap_or_else(|_| session.to_path_buf());
            let mut rec = json!({
                "pid": pid,
                "print_run": print_run,
                "inbox": inbox,
                "started": iso_ms(),
                "session": abs.to_string_lossy(),
            });
            if let Some(k) = extra.kind {
                rec["kind"] = json!(k.as_str());
            }
            if let Some(a) = &extra.agent {
                rec["agent"] = json!(a);
            }
            if let Some(c) = &extra.cwd {
                rec["cwd"] = json!(c.to_string_lossy());
            }
            if let Some(p) = &extra.parent {
                rec["parent"] = json!(p.to_string_lossy());
            }
            if extra.abort {
                rec["abort"] = json!(true);
            }
            set_attention_fields(&mut rec, &extra.attention);
            file.write_all(rec.to_string().as_bytes())?;
            // The lock follows the inode through the rename.
            std::fs::rename(&tmp, &path)
        })();
        if done.is_err() {
            let _ = std::fs::remove_file(&tmp);
            return None;
        }
        if let Ok(mut c) = CLAIMED.lock() {
            c.push(path.clone());
        }
        Some(Self { path, file })
    }
}

impl Drop for OwnerRecord {
    fn drop(&mut self) {
        use std::os::unix::fs::MetadataExt;
        // Only unlink our own record: the name may have been taken over by another owner.
        if let (Ok(a), Ok(b)) = (std::fs::metadata(&self.path), self.file.metadata())
            && a.dev() == b.dev()
            && a.ino() == b.ino()
        {
            let _ = std::fs::remove_file(&self.path);
        }
        if let Ok(mut c) = CLAIMED.lock() {
            c.retain(|p| p != &self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::file::SessionFile;
    use mu_testutil::home;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mu-test-{name}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn record_of(path: &Path) -> PathBuf {
        home().join("run").join(format!("{}.json", id_of(path)))
    }

    #[test]
    fn the_owner_is_recorded_while_a_session_is_open() {
        home();
        let s = SessionFile::create(Path::new("/opt/owner"), None).unwrap();
        let path = s.path.clone();
        assert!(record_of(&path).exists());
        match owner(&path) {
            Owner::Live { pid, .. } => assert_eq!(pid, std::process::id()),
            o => panic!("{o:?}"),
        }
        drop(s);
        assert_eq!(owner(&path), Owner::Free);
        assert!(!record_of(&path).exists());
        let s = SessionFile::open_append(&path).unwrap();
        assert!(record_of(&path).exists());
        drop(s);
        assert!(!record_of(&path).exists());
    }

    #[test]
    fn a_stale_record_is_never_trusted() {
        home();
        let d = tmp("stale");
        let path = d.join("x_stale-one.jsonl");
        std::fs::write(&path, "{\"type\":\"session\",\"version\":3,\"id\":\"stale-one\"}\n").unwrap();
        let mut child = std::process::Command::new("sleep").arg("30").spawn().unwrap();
        std::fs::create_dir_all(home().join("run")).unwrap();
        let rec = json!({"pid": child.id(), "print_run": true, "started": iso_ms(), "session": path.to_string_lossy()});
        std::fs::write(record_of(&path), rec.to_string()).unwrap();
        // Someone holds the session without leaving a live record.
        let held = File::open(&path).unwrap();
        try_flock(&held, libc::LOCK_EX).unwrap();
        assert_eq!(owner(&path), Owner::Unknown);
        let e = SessionFile::open_append(&path).err().unwrap().to_string();
        assert!(e.contains("another mu process") && !e.contains("pid"), "{e}");
        assert!(child.try_wait().unwrap().is_none(), "the pid in a stale record is never signalled");
        drop(held);
        assert_eq!(owner(&path), Owner::Free);
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn a_record_for_another_file_with_the_same_id_is_ignored() {
        home();
        let s = SessionFile::create(Path::new("/opt/same-id"), None).unwrap();
        let d = tmp("copy");
        let copy = d.join(s.path.file_name().unwrap());
        std::fs::copy(&s.path, &copy).unwrap();
        let held = File::open(&copy).unwrap();
        try_flock(&held, libc::LOCK_EX).unwrap();
        // The live record names `s.path`, not the copy.
        assert_eq!(owner(&copy), Owner::Unknown);
        assert!(matches!(owner(&s.path), Owner::Live { .. }));
    }

    #[test]
    fn print_run_is_reported() {
        home();
        let d = tmp("print-run");
        let path = d.join("x_print-run.jsonl");
        std::fs::write(&path, "{\"type\":\"session\",\"version\":3,\"id\":\"print-run\"}\n").unwrap();
        let held = File::open(&path).unwrap();
        try_flock(&held, libc::LOCK_EX).unwrap();
        // No process-global flag: other tests create sessions in parallel.
        let _rec = OwnerRecord::claim_as(&path, true, false).unwrap();
        assert!(matches!(owner(&path), Owner::Live { print_run: true, inbox: false, .. }));
        let e = SessionFile::open_append(&path).err().unwrap().to_string();
        assert!(e.contains("mu --resume"), "{e}");
    }

    fn listed(path: &Path) -> Option<LiveSession> {
        let abs = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        live_sessions().into_iter().find(|l| l.path == abs)
    }

    #[test]
    fn a_created_session_records_where_it_runs_and_who_spawned_it() {
        home();
        let s = SessionFile::create(Path::new("/opt/rec-cwd"), Some(Path::new("/p/parent.jsonl"))).unwrap();
        let l = listed(&s.path).expect("a live session is listed");
        assert_eq!(l.pid, std::process::id());
        assert_eq!(l.cwd.as_deref(), Some(Path::new("/opt/rec-cwd")));
        assert_eq!(l.parent.as_deref(), Some(Path::new("/p/parent.jsonl")));
        drop(s);
    }

    #[test]
    fn a_record_from_before_the_new_fields_lists_with_a_guessed_kind_and_the_header_parent() {
        home();
        let d = tmp("old-record");
        let path = d.join(format!("x_old-record-{}.jsonl", uuid::Uuid::new_v4()));
        std::fs::write(
            &path,
            "{\"type\":\"session\",\"version\":3,\"id\":\"o\",\"parentSession\":\"/from/header.jsonl\"}\n",
        )
        .unwrap();
        let _rec = OwnerRecord::claim_as(&path, true, false).unwrap();
        let l = listed(&path).unwrap();
        assert_eq!((l.kind.as_deref(), l.agent, l.cwd), (Some("print"), None, None));
        assert_eq!(l.parent.as_deref(), Some(Path::new("/from/header.jsonl")));
    }

    #[test]
    fn an_agents_client_and_waiting_flag_reach_its_record_and_the_listing() {
        home();
        let s = SessionFile::create(Path::new("/opt/attention"), None).unwrap();
        set_attention(|a| {
            a.waiting = true;
            a.client = Some(ClientInfo { host: "laptop".into(), since: 100 });
        });
        let l = listed(&s.path).unwrap();
        assert!(l.waiting);
        assert_eq!(l.client, Some(ClientInfo { host: "laptop".into(), since: 100 }));
        // A record claimed later starts with what is set now.
        let late = SessionFile::create(Path::new("/opt/attention-late"), None).unwrap();
        assert!(listed(&late.path).unwrap().waiting);
        set_attention(|a| *a = Attention::default());
        let l = listed(&s.path).unwrap();
        assert!(!l.waiting && l.client.is_none());
        assert!(listed(&late.path).is_some(), "the record is still whole after rewrites");
    }

    #[test]
    fn the_run_kind_is_recorded_by_name() {
        home();
        for (kind, name) in [
            (RunKind::Tui, "tui"),
            (RunKind::Print, "print"),
            (RunKind::Subagent, "subagent"),
            (RunKind::Acp, "acp"),
            (RunKind::Agent, "agent"),
        ] {
            let d = tmp("run-kind");
            let path = d.join(format!("x_run-kind-{}.jsonl", uuid::Uuid::new_v4()));
            std::fs::write(&path, "{\"type\":\"session\",\"version\":3,\"id\":\"k\"}\n").unwrap();
            let extra = Extra { kind: Some(kind), ..Default::default() };
            let _rec = OwnerRecord::claim_with(&path, false, false, &extra).unwrap();
            assert_eq!(listed(&path).unwrap().kind.as_deref(), Some(name));
        }
    }

    #[test]
    fn a_record_nobody_holds_is_not_listed() {
        home();
        let d = tmp("stale-record");
        let id = format!("stale-{}", uuid::Uuid::new_v4());
        let path = d.join(format!("x_{id}.jsonl"));
        std::fs::write(&path, "{\"type\":\"session\"}\n").unwrap();
        let dir = home().join("run");
        std::fs::create_dir_all(&dir).unwrap();
        let abs = std::fs::canonicalize(&path).unwrap();
        let rec = json!({"pid": 1, "print_run": false, "started": iso_ms(), "session": abs.to_string_lossy()});
        std::fs::write(dir.join(format!("{id}.json")), rec.to_string()).unwrap();
        assert!(listed(&path).is_none());
    }

    #[test]
    fn claiming_sweeps_stale_records_but_not_live_ones() {
        home();
        // A private directory: another test sweeping home()/run in parallel takes a shared lock
        // on every record it finds, so the exclusive lock below would fail with EWOULDBLOCK. That
        // is a race in the test, not in the sweeper, and it showed up first on macOS runners.
        let dir = tmp("sweep");
        let stale = dir.join("sweep-stale-record.json");
        std::fs::write(&stale, "{\"pid\":1}").unwrap();
        let live = dir.join("sweep-live-record.json");
        std::fs::write(&live, "{\"pid\":2}").unwrap();
        let held = File::open(&live).unwrap();
        try_flock(&held, libc::LOCK_EX).unwrap();
        sweep_stale(&dir);
        assert!(!stale.exists(), "a record nobody holds is removed");
        assert!(live.exists(), "a record under lock is kept");

        // Claiming records its own file and sweeps the shared directory. The stale record it
        // should remove may already be gone (a parallel sweep), which cannot make this fail.
        let run = home().join("run");
        std::fs::create_dir_all(&run).unwrap();
        let abandoned = run.join(format!("abandoned-{}.json", uuid::Uuid::new_v4()));
        std::fs::write(&abandoned, "{\"pid\":1}").unwrap();
        let s = SessionFile::create(Path::new("/opt/sweep"), None).unwrap();
        assert!(!abandoned.exists(), "claiming sweeps the shared directory");
        assert!(record_of(&s.path).exists(), "the claimer's own record stays");
    }

    #[test]
    fn sweeping_does_not_make_a_stale_record_look_live() {
        home();
        // Private, like the test above: the shared directory is swept by every parallel test.
        let dir = tmp("sweep-probe");
        let stale = dir.join("sweep-shared-probe.json");
        std::fs::write(&stale, "{\"pid\":1}").unwrap();
        // A concurrent `owner()` holds a shared lock while it reads the record. An exclusive probe
        // would conflict with it (and, the other way round, make the record look live to it).
        let reader = File::open(&stale).unwrap();
        try_flock(&reader, libc::LOCK_SH).unwrap();
        sweep_stale(&dir);
        assert!(!stale.exists(), "the probe takes a shared lock, which coexists with a reader's");
    }

    #[test]
    fn inbox_is_reported() {
        home();
        let d = tmp("inbox-flag");
        let path = d.join("x_inbox-flag.jsonl");
        std::fs::write(&path, "{\"type\":\"session\",\"version\":3,\"id\":\"inbox-flag\"}\n").unwrap();
        let held = File::open(&path).unwrap();
        try_flock(&held, libc::LOCK_EX).unwrap();
        let _rec = OwnerRecord::claim_as(&path, false, true).unwrap();
        assert!(matches!(owner(&path), Owner::Live { print_run: false, inbox: true, .. }));
    }
}
