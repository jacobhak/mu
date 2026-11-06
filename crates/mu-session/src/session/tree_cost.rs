//! What a session's subagents have spent: the summed cost of every descendant session file, for the
//! status line. A child the session started is known from its job; the others (children of an
//! earlier run of a resumed session, grandchildren) are found through the `parentSession` in the
//! headers of files newer than their parent in the same session directories. It only reads, and
//! does file I/O, so call it from a blocking thread.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::list::id_of;
use super::owner::header_parent;
use super::tail::Tail;

/// Running children are read again at most this often.
const POLL_EVERY: Duration = Duration::from_secs(1);
/// The session directories are listed again at most this often, and at once for a new child.
const SCAN_EVERY: Duration = Duration::from_secs(3);
/// Passes over a finished child that is not read to the end in one call (each reads up to 1 MiB).
const CATCH_UP: usize = 16;

/// A subagent this session started, as its jobs list knows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Child {
    pub path: PathBuf,
    pub running: bool,
}

#[derive(Default)]
pub struct TreeCost {
    root: Option<PathBuf>,
    /// Parent id from the header of every file looked at; a header never changes.
    parents: HashMap<PathBuf, Option<String>>,
    tails: HashMap<PathBuf, Tail>,
    /// Finished children read to the end: never read again.
    settled: HashSet<PathBuf>,
    polled: Option<Instant>,
    scanned: Option<Instant>,
    /// The children the last scan was made for.
    scanned_for: Vec<PathBuf>,
    total: f64,
}

/// What the finished session at `path` cost, read to its end now (a bounded number of passes, so a
/// huge file gives a partial figure rather than a stall). 0 when it cannot be read.
pub fn session_cost(path: &Path) -> f64 {
    let mut t = Tail::new(path, 0);
    for _ in 0..CATCH_UP {
        let before = t.offset();
        t.poll();
        if t.caught_up() || t.offset() == before {
            break;
        }
    }
    t.cost()
}

impl TreeCost {
    /// Dollars spent by the descendants of the root session, not counting the root itself.
    pub fn total(&self) -> f64 {
        self.total
    }

    /// What each descendant session has cost on its own, as of the last `poll`.
    pub fn costs(&self) -> HashMap<PathBuf, f64> {
        self.tails.iter().map(|(p, t)| (p.clone(), t.cost())).collect()
    }

    /// Whether `poll` has something to do now: the root changed, a child is new or finished and not
    /// yet read to the end, or it has been a while.
    pub fn due(&self, root: Option<&Path>, children: &[Child]) -> bool {
        if self.root.as_deref() != root {
            return true;
        }
        if children.iter().any(|c| !self.tails.contains_key(&c.path) || (!c.running && !self.settled.contains(&c.path)))
        {
            return true;
        }
        (root.is_some() || !children.is_empty()) && self.polled.is_none_or(|t| t.elapsed() >= POLL_EVERY)
    }

    /// Reads what the descendants of `root` appended since the last call. A different `root` starts
    /// over.
    pub fn poll(&mut self, root: Option<&Path>, children: &[Child]) {
        if self.root.as_deref() != root {
            let parents = std::mem::take(&mut self.parents);
            *self = Self { root: root.map(Path::to_path_buf), parents, ..Self::default() };
        }
        let paths: Vec<PathBuf> = children.iter().map(|c| c.path.clone()).collect();
        if self.scanned.is_none_or(|t| t.elapsed() >= SCAN_EVERY) || self.scanned_for != paths {
            self.scan(paths);
        }
        let members = self.members(children);
        let finished: HashSet<&PathBuf> = children.iter().filter(|c| !c.running).map(|c| &c.path).collect();
        for path in &members {
            if self.settled.contains(path) {
                continue;
            }
            let t = self.tails.entry(path.clone()).or_insert_with(|| Tail::new(path.clone(), 0));
            let mut progressed = true;
            let mut passes = 0;
            // A finished child's file is final: read it to the end now.
            let max_passes = if finished.contains(path) { CATCH_UP } else { 1 };
            while progressed && passes < max_passes {
                let before = t.offset();
                t.poll();
                progressed = t.offset() != before;
                passes += 1;
                if t.caught_up() {
                    break;
                }
            }
            // A last line without its newline (a child killed mid-write) never completes: no
            // progress on a finished child means there is nothing more to read.
            if finished.contains(path) && (t.caught_up() || !progressed) {
                self.settled.insert(path.clone());
            }
        }
        self.total = members.iter().filter_map(|p| self.tails.get(p)).map(Tail::cost).sum();
        self.polled = Some(Instant::now());
    }

    /// Every descendant session file known: the children, and the files whose recorded parent is the
    /// root or one of these. Sessions are compared by id, so two spellings of a path agree.
    fn members(&self, children: &[Child]) -> Vec<PathBuf> {
        let mut ids: HashSet<String> = self.root.iter().map(|r| id_of(r)).collect();
        let mut out: Vec<PathBuf> =
            children.iter().filter(|c| ids.insert(id_of(&c.path))).map(|c| c.path.clone()).collect();
        loop {
            let before = out.len();
            for (path, parent) in &self.parents {
                if parent.as_ref().is_some_and(|p| ids.contains(p)) && ids.insert(id_of(path)) {
                    out.push(path.clone());
                }
            }
            if out.len() == before {
                return out;
            }
        }
    }

    /// Reads the header of every file in the root's and the children's session directories that has
    /// not been looked at: only files named no earlier than a known session, as a child is created
    /// after its parent and names start with the creation time.
    fn scan(&mut self, children: Vec<PathBuf>) {
        self.scanned = Some(Instant::now());
        let mut floor: HashMap<PathBuf, String> = HashMap::new();
        for p in self.root.iter().chain(&children) {
            let (Some(dir), Some(name)) = (p.parent(), p.file_name().and_then(|n| n.to_str())) else { continue };
            let f = floor.entry(dir.to_path_buf()).or_insert_with(|| name.to_string());
            if name < f.as_str() {
                *f = name.to_string();
            }
        }
        self.scanned_for = children;
        for (dir, floor) in floor {
            let Ok(rd) = std::fs::read_dir(&dir) else { continue };
            for e in rd.filter_map(Result::ok) {
                let path = e.path();
                let named = path.file_name().and_then(|n| n.to_str());
                if !named.is_some_and(|n| n.ends_with(".jsonl") && n >= floor.as_str())
                    || self.parents.contains_key(&path)
                {
                    continue;
                }
                // Created but its header not written yet: look again next time.
                if e.metadata().is_ok_and(|m| m.len() == 0) {
                    continue;
                }
                let parent = header_parent(&path).map(|p| id_of(&p));
                self.parents.insert(path, parent);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;

    fn dir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("mu-tree-cost-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// `<dir>/2026-01-01T00-00-<n>-000Z_<id>.jsonl`, with its header and one reply costing `cost`.
    fn session(d: &Path, n: u32, id: &str, parent: Option<&Path>, cost: f64) -> PathBuf {
        let p = d.join(format!("2026-01-01T00-00-{n:02}-000Z_{id}.jsonl"));
        let mut header = json!({"type":"session","version":3,"id":id,"cwd":"/"});
        if let Some(parent) = parent {
            header["parentSession"] = json!(parent.to_string_lossy());
        }
        std::fs::write(&p, format!("{{\"type\":\"title\"}}\n{header}\n")).unwrap();
        reply(&p, cost);
        p
    }

    fn reply(p: &Path, cost: f64) {
        let m = json!({"role":"assistant","content":[],"usage":{"input":1,"output":1,"cacheRead":0,"cacheWrite":0,
            "totalTokens":2,"cost":{"total":cost}},"stopReason":"stop","timestamp":1});
        let mut f = std::fs::OpenOptions::new().append(true).open(p).unwrap();
        writeln!(f, "{}", json!({"type":"message","id":"x","parentId":null,"message":m})).unwrap();
    }

    fn child(path: &Path, running: bool) -> Child {
        Child { path: path.to_path_buf(), running }
    }

    #[test]
    fn the_children_a_session_started_add_up_without_the_session_itself() {
        let d = dir();
        let root = session(&d, 0, "root", None, 5.0);
        let a = session(&d, 1, "a", Some(&root), 0.25);
        let b = session(&d, 2, "b", Some(&root), 0.5);
        let mut t = TreeCost::default();
        t.poll(Some(&root), &[child(&a, true), child(&b, false)]);
        assert!((t.total() - 0.75).abs() < 1e-9, "{}", t.total());
        reply(&a, 0.25);
        t.poll(Some(&root), &[child(&a, true), child(&b, false)]);
        assert!((t.total() - 1.0).abs() < 1e-9, "a running child is read again: {}", t.total());
    }

    #[test]
    fn a_resumed_session_finds_its_children_and_grandchildren_by_their_headers() {
        let d = dir();
        let before = session(&d, 0, "before", None, 9.0);
        let root = session(&d, 1, "root", None, 5.0);
        let kid = session(&d, 2, "kid", Some(&root), 0.25);
        let grandkid = session(&d, 3, "grandkid", Some(&kid), 0.125);
        session(&d, 4, "stranger", None, 7.0);
        session(&d, 5, "other-kid", Some(&before), 3.0);
        let mut t = TreeCost::default();
        t.poll(Some(&root), &[]);
        assert!((t.total() - 0.375).abs() < 1e-9, "{}", t.total());
        assert!(!t.parents.contains_key(&before), "files older than the session are not even opened");
        // A grandchild of a child the jobs list knows, found in the child's directory.
        let other = dir();
        let far = session(&other, 0, "far", Some(&root), 1.0);
        let far_kid = session(&other, 1, "far-kid", Some(&far), 2.0);
        let mut t = TreeCost::default();
        t.poll(Some(&root), &[child(&far, false)]);
        assert!((t.total() - (0.25 + 0.125 + 1.0 + 2.0)).abs() < 1e-9, "{}", t.total());
        drop((grandkid, far_kid));
    }

    #[test]
    fn each_sessions_own_cost_is_kept_and_a_finished_one_reads_whole() {
        let d = dir();
        let root = session(&d, 0, "root", None, 5.0);
        let kid = session(&d, 1, "kid", Some(&root), 0.25);
        let grandkid = session(&d, 2, "grandkid", Some(&kid), 0.125);
        let mut t = TreeCost::default();
        t.poll(Some(&root), &[child(&kid, true)]);
        let costs = t.costs();
        assert_eq!((costs[&kid], costs[&grandkid]), (0.25, 0.125), "a child's figure leaves its own children out");
        assert!(!costs.contains_key(&root));
        reply(&kid, 0.5);
        assert!((session_cost(&kid) - 0.75).abs() < 1e-9);
        assert_eq!(session_cost(&d.join("missing.jsonl")), 0.0);
    }

    #[test]
    fn a_new_root_starts_over() {
        let d = dir();
        let one = session(&d, 0, "one", None, 1.0);
        let two = session(&d, 1, "two", None, 1.0);
        session(&d, 2, "of-one", Some(&one), 0.5);
        let mut t = TreeCost::default();
        t.poll(Some(&one), &[]);
        assert!((t.total() - 0.5).abs() < 1e-9);
        assert!(t.due(Some(&two), &[]));
        t.poll(Some(&two), &[]);
        assert_eq!(t.total(), 0.0);
        t.poll(None, &[]);
        assert_eq!(t.total(), 0.0);
    }

    #[test]
    fn a_finished_child_is_read_once_and_nothing_is_due_until_the_next_interval() {
        let d = dir();
        let root = session(&d, 0, "root", None, 1.0);
        let done = session(&d, 1, "done", Some(&root), 0.25);
        let mut t = TreeCost::default();
        let kids = [child(&done, false)];
        assert!(t.due(Some(&root), &kids), "new and finished");
        t.poll(Some(&root), &kids);
        assert!(!t.due(Some(&root), &kids));
        // Settled: what is appended later is not read.
        reply(&done, 10.0);
        t.polled = Some(Instant::now() - POLL_EVERY * 2);
        t.poll(Some(&root), &kids);
        assert!((t.total() - 0.25).abs() < 1e-9);
        let running = [child(&done, true)];
        assert!(!t.due(Some(&root), &running), "just polled");
        t.polled = Some(Instant::now() - POLL_EVERY * 2);
        assert!(t.due(Some(&root), &running));
        assert!(!TreeCost::default().due(None, &[]), "no session and no children: nothing to read");
    }

    #[test]
    fn a_finished_child_with_a_torn_last_line_or_no_file_settles() {
        let d = dir();
        let root = session(&d, 0, "root", None, 1.0);
        let torn = session(&d, 1, "torn", Some(&root), 0.25);
        std::fs::OpenOptions::new().append(true).open(&torn).unwrap().write_all(b"{\"type\":\"mess").unwrap();
        let missing = d.join("2026-01-01T00-00-09-000Z_missing.jsonl");
        let kids = [child(&torn, false), child(&missing, false)];
        let mut t = TreeCost::default();
        t.poll(Some(&root), &kids);
        assert!((t.total() - 0.25).abs() < 1e-9);
        assert!(!t.due(Some(&root), &kids), "both settled, so no read loop");
    }
}
