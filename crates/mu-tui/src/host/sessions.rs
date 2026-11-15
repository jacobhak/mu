//! Session lists and the jobs panel's sources (table rows "/resume, session picker" and "Jobs panel
//! and Sessions tab"): recent sessions, who owns one, the live sessions, a session file followed from
//! an offset, what sessions cost, and messages to or signals for another session.

use super::local::{Local, Registry};
use super::{Answer, pending, ready};
use mu_base::message::Message;
use mu_session::session::control;
use mu_session::session::inbox;
use mu_session::session::list;
use mu_session::session::owner::{self, LiveSession, Owner};
use mu_session::session::runs::{RunDir, SessionSource};
use mu_session::session::tail::Tail;
use mu_session::session::tree_cost::{self, Child, TreeCost};
use mu_tools::tools::changes;
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::SystemTime;

pub const RECENT_SESSIONS: &str = "recent_sessions";
pub const SESSION_OWNER: &str = "session_owner";
pub const SESSION_EXISTS: &str = "session_exists";
pub const LIVE_SESSIONS: &str = "live_sessions";
pub const TAIL_READ: &str = "tail_read";
pub const SESSION_COST: &str = "session_cost";
pub const TREE_COST: &str = "tree_cost";
pub const SEND_MESSAGE: &str = "send_message";
pub const STOP_SESSION: &str = "stop_session";
pub const TERMINATE_SESSION: &str = "terminate_session";

/// Most sessions the resume picker lists.
const RECENT_MAX: usize = 200;

/// The reply of `session_exists`: whether the file is there, and the `--profile` flag a resume hint needs.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Exists {
    pub exists: bool,
    pub profile_flag: String,
}

/// `live_sessions` arguments: sessions whose title the caller has (not read again), and paths to
/// canonicalise on the host.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct LiveArgs {
    pub titled: Vec<PathBuf>,
    pub canon: Vec<PathBuf>,
}

/// One live session with what the Sessions tab shows of it, read on the host.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LiveRow {
    pub live: LiveSession,
    /// The spawning session's path, canonicalised.
    pub parent: Option<PathBuf>,
    /// When the session file was last written.
    pub modified: Option<SystemTime>,
    /// Its title, else its first prompt; `None` when the caller has it already or there is none.
    pub title: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Live {
    pub rows: Vec<LiveRow>,
    /// Each of `LiveArgs::canon` with its canonical form.
    pub canon: Vec<(PathBuf, PathBuf)>,
    /// A line the tab shows under the list: worker results kept because nobody applied them.
    #[serde(default)]
    pub notice: Option<String>,
}

/// `tail_read` arguments: the session file, how far the caller has read it, and whether it was caught up.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TailArgs {
    pub path: PathBuf,
    pub offset: u64,
    pub caught_up: bool,
    /// False when only the totals are wanted.
    pub messages: bool,
}

/// What was appended to a session file since `from`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TailDelta {
    /// The offset the read started from, as the caller sent it.
    pub from: u64,
    /// The file shrank: everything before is void and `messages` and the totals start from the top.
    pub reset: bool,
    pub messages: Vec<Message>,
    /// Where the next read starts.
    pub offset: u64,
    pub cost: f64,
    pub tokens: u64,
    pub caught_up: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TreeArgs {
    pub root: Option<PathBuf>,
    pub children: Vec<Child>,
}

/// What the subagents of the root session have spent.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Spend {
    pub total: f64,
    pub costs: Vec<(PathBuf, f64)>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MessageArgs {
    pub session: PathBuf,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StopArgs {
    pub session: PathBuf,
    pub pid: u32,
    pub started: String,
    pub kind: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TerminateArgs {
    pub session: PathBuf,
    pub pid: u32,
    pub started: String,
}

pub(super) fn register(r: &mut Registry) {
    r.add(RECENT_SESSIONS, recent_sessions);
    r.add(SESSION_OWNER, session_owner);
    r.add(SESSION_EXISTS, session_exists);
    r.add(LIVE_SESSIONS, live_sessions);
    r.add(TAIL_READ, tail_read);
    r.add(SESSION_COST, session_cost);
    r.add(TREE_COST, tree_cost);
    r.add(SEND_MESSAGE, send_message);
    r.add(STOP_SESSION, stop_session);
    r.add(TERMINATE_SESSION, terminate_session);
}

thread_local! {
    /// The reader of the subagents' session files: its state lives on the host between calls.
    static SPEND: RefCell<Option<TreeCost>> = RefCell::new(Some(TreeCost::default()));
    /// Where the live sessions come from in a test, instead of this profile's run directory.
    #[cfg(test)]
    pub(crate) static TEST_SOURCE: RefCell<Option<Rc<dyn SessionSource>>> = const { RefCell::new(None) };
}

fn recent_sessions(l: &Rc<Local>, args: serde_json::Value) -> Answer {
    pending(args, |(): ()| {
        let cwd = l.cwd.clone();
        async move {
            Ok(tokio::task::spawn_blocking(move || list::list_recent(&cwd, Some(&cwd), RECENT_MAX))
                .await
                .unwrap_or_default())
        }
    })
}

fn session_owner(_: &Rc<Local>, args: serde_json::Value) -> Answer {
    ready(args, |p: PathBuf| Ok::<Owner, String>(owner::owner(&p)))
}

fn session_exists(_: &Rc<Local>, args: serde_json::Value) -> Answer {
    ready(args, |p: PathBuf| Ok::<_, String>(Exists { exists: p.exists(), profile_flag: list::profile_flag() }))
}

fn canon(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// The live sessions of `source` other than the process `me`, with their titles (unless known) and
/// modification times.
fn live_rows(source: &dyn SessionSource, me: u32, a: LiveArgs) -> Live {
    let rows = source
        .live_sessions()
        .into_iter()
        .filter(|l| l.pid != me)
        .map(|l| LiveRow {
            parent: l.parent.as_deref().map(canon),
            modified: std::fs::metadata(&l.path).and_then(|m| m.modified()).ok(),
            title: if a.titled.contains(&l.path) { None } else { source.title(&l.path) },
            live: l,
        })
        .collect();
    Live { rows, canon: a.canon.into_iter().map(|p| (p.clone(), canon(&p))).collect(), notice: None }
}

fn live_sessions(l: &Rc<Local>, args: serde_json::Value) -> Answer {
    ready(args, |a: LiveArgs| {
        let me = std::process::id();
        #[cfg(test)]
        if let Some(s) = TEST_SOURCE.with(|s| s.borrow().clone()) {
            return Ok::<_, String>(live_rows(s.as_ref(), me, a));
        }
        let mut live = live_rows(&RunDir::profile(), me, a);
        live.notice = changes::unapplied_notice(&l.cwd, changes::KEPT_AFTER);
        Ok(live)
    })
}

pub(crate) fn read_tail(a: TailArgs) -> TailDelta {
    let keep = if a.messages { usize::MAX } else { 0 };
    let mut t = Tail::resume(a.path, keep, a.offset, a.caught_up);
    t.poll();
    TailDelta {
        from: a.offset,
        reset: t.was_reset(),
        messages: t.messages().to_vec(),
        offset: t.offset(),
        cost: t.cost(),
        tokens: t.tokens(),
        caught_up: t.caught_up(),
    }
}

fn tail_read(_: &Rc<Local>, args: serde_json::Value) -> Answer {
    ready(args, |a: TailArgs| Ok::<_, String>(read_tail(a)))
}

fn session_cost(_: &Rc<Local>, args: serde_json::Value) -> Answer {
    pending(args, |p: PathBuf| async move {
        tokio::task::spawn_blocking(move || tree_cost::session_cost(&p)).await.map_err(|e| e.to_string())
    })
}

fn spend_of(t: &TreeCost) -> Spend {
    Spend { total: t.total(), costs: t.costs().into_iter().collect() }
}

/// Answers at once when the reader has nothing to do; otherwise reads on a blocking thread.
fn tree_cost(_: &Rc<Local>, args: serde_json::Value) -> Answer {
    let a: TreeArgs = match serde_json::from_value(args) {
        Ok(a) => a,
        Err(e) => return Answer::Ready(Err(format!("bad arguments: {e}"))),
    };
    let Some(mut t) = SPEND.with(|s| s.borrow_mut().take()) else {
        return Answer::Ready(Err("the subagent cost reader is busy".into()));
    };
    if !t.due(a.root.as_deref(), &a.children) {
        let out = spend_of(&t);
        SPEND.with(|s| *s.borrow_mut() = Some(t));
        return Answer::Ready(serde_json::to_value(out).map_err(|e| e.to_string()));
    }
    Answer::Pending(Box::pin(async move {
        let t = tokio::task::spawn_blocking(move || {
            t.poll(a.root.as_deref(), &a.children);
            t
        })
        .await
        .unwrap_or_default();
        let out = spend_of(&t);
        SPEND.with(|s| *s.borrow_mut() = Some(t));
        serde_json::to_value(out).map_err(|e| e.to_string())
    }))
}

fn send_message(l: &Rc<Local>, args: serde_json::Value) -> Answer {
    ready(args, |a: MessageArgs| inbox::send(&a.session, Some("mu jobs panel"), &a.text, &l.cwd))
}

fn stop_session(_: &Rc<Local>, args: serde_json::Value) -> Answer {
    ready(args, |a: StopArgs| control::stop_session(&a.session, a.pid, &a.started, &a.kind).map_err(|e| e.to_string()))
}

fn terminate_session(_: &Rc<Local>, args: serde_json::Value) -> Answer {
    ready(args, |a: TerminateArgs| control::terminate_session(&a.session, a.pid, &a.started).map_err(|e| e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::de::DeserializeOwned;
    use serde_json::json;

    fn round<T: Serialize + DeserializeOwned>(v: T) {
        let sent = serde_json::to_value(&v).unwrap();
        let back: T = serde_json::from_value(sent.clone()).unwrap();
        assert_eq!(serde_json::to_value(&back).unwrap(), sent);
    }

    fn live(path: &str) -> LiveSession {
        LiveSession {
            path: PathBuf::from(path),
            pid: 77,
            print_run: true,
            inbox: false,
            started: "2026-01-01T00:00:00Z".into(),
            kind: Some("print".into()),
            agent: None,
            cwd: Some(PathBuf::from("/work")),
            parent: Some(PathBuf::from("/s/p.jsonl")),
            abort: false,
        }
    }

    #[test]
    fn the_reply_types_survive_the_wire() {
        round(Exists { exists: true, profile_flag: "--profile x ".into() });
        round(Live {
            rows: vec![LiveRow {
                live: live("/s/a.jsonl"),
                parent: Some("/s/p.jsonl".into()),
                modified: Some(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(5)),
                title: Some("t".into()),
            }],
            canon: vec![("/a".into(), "/b".into())],
            notice: Some("n".into()),
        });
        round(TailDelta { from: 3, reset: true, messages: vec![], offset: 9, cost: 0.5, tokens: 12, caught_up: true });
        round(Spend { total: 0.75, costs: vec![("/s/k.jsonl".into(), 0.75)] });
        round(TreeArgs { root: None, children: vec![Child { path: "/s/k.jsonl".into(), running: true }] });
        round(Owner::Live { pid: 1, print_run: true, inbox: false, started: "s".into() });
        round(Owner::Unknown);
    }

    #[test]
    fn a_tail_delta_carries_messages_and_resumes_from_its_offset() {
        let d = std::env::temp_dir().join(format!("mu-host-tail-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("s.jsonl");
        let user = |t: &str| {
            format!(
                "{}\n",
                json!({"type":"message","id":"u","parentId":null,"message":{"role":"user","content":t,"timestamp":1}})
            )
        };
        std::fs::write(&p, user("one")).unwrap();
        let args = |offset, caught_up, messages| TailArgs { path: p.clone(), offset, caught_up, messages };
        let first = read_tail(args(0, false, true));
        assert_eq!((first.messages.len(), first.caught_up, first.reset), (1, true, false));
        std::fs::write(&p, format!("{}{}", user("one"), user("two"))).unwrap();
        let next = read_tail(args(first.offset, true, true));
        assert_eq!((next.from, next.messages.len()), (first.offset, 1));
        assert!(read_tail(args(0, false, false)).messages.is_empty(), "totals only");
        std::fs::write(&p, user("x")).unwrap();
        let shrunk = read_tail(args(next.offset, true, true));
        assert!(shrunk.reset && shrunk.messages.len() == 1, "{shrunk:?}");
    }

    #[test]
    fn the_live_list_leaves_out_this_process_and_reads_titles_once() {
        struct Fixed(Vec<LiveSession>);
        impl SessionSource for Fixed {
            fn live_sessions(&self) -> Vec<LiveSession> {
                self.0.clone()
            }
            fn title(&self, s: &Path) -> Option<String> {
                Some(format!("title of {}", s.display()))
            }
        }
        let (mut me, other, known) = (live("/s/me.jsonl"), live("/s/other.jsonl"), live("/s/known.jsonl"));
        me.pid = 5;
        let src = Fixed(vec![me, other, known]);
        let out =
            live_rows(&src, 5, LiveArgs { titled: vec!["/s/known.jsonl".into()], canon: vec!["/no/such/path".into()] });
        let got: Vec<_> = out.rows.iter().map(|r| (r.live.path.display().to_string(), r.title.clone())).collect();
        assert_eq!(
            got,
            [
                ("/s/other.jsonl".to_string(), Some("title of /s/other.jsonl".to_string())),
                ("/s/known.jsonl".to_string(), None)
            ]
        );
        assert_eq!(out.canon, [(PathBuf::from("/no/such/path"), PathBuf::from("/no/such/path"))]);
        assert_eq!(out.rows[0].parent, Some(PathBuf::from("/s/p.jsonl")));
    }
}
