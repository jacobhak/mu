//! `mu notify <session|id> <text>`: leave a message for a session and wake it if an interactive
//! mu has it open. Messages live in `$MU_HOME/inbox/<id>/<ts>-<pid>-<n>.json`; an interactive
//! owner announces SIGUSR2 support in its owner record (`inbox: true`) and drains the directory
//! itself. Only POSIX calls (`rename`, `flock`, `kill`), so it is portable.
//!
//! (Not to be confused with `events::Inbox`, the steer/follow-up queue.)
use crate::session::{self, Owner};
use mu_common::paths;
use serde_json::{Value, json};
use std::io::Read;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

const MAX_TEXT: usize = 64 * 1024;
const MAX_FROM: usize = 64;
const DEFAULT_FROM: &str = "another process";
const PRUNE_AGE: Duration = Duration::from_secs(30 * 24 * 3600);

/// One queued message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    /// File name inside the session's inbox directory.
    pub name: String,
    pub from: String,
    pub text: String,
    pub at: String,
}

fn root() -> PathBuf {
    paths::mu_home().join("inbox")
}

fn dir(id: &str) -> Option<PathBuf> {
    session::valid_id(id).then(|| root().join(id))
}

fn make_private_dir(p: &Path) -> std::io::Result<()> {
    match std::fs::DirBuilder::new().mode(0o700).create(p) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Ok(()),
        Err(e) => return Err(e),
    }
    // The umask may have stripped bits.
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700))
}

fn ensure_dir(id: &str) -> std::io::Result<PathBuf> {
    let d = dir(id).ok_or_else(|| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    make_private_dir(&root())?;
    make_private_dir(&d)?;
    Ok(d)
}

fn clean_line(s: &str, max: usize) -> String {
    let one: String = s.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    one.trim().chars().take(max).collect()
}

/// The sender name: one line, at most 64 characters, `another process` when empty.
pub fn sanitize_from(from: Option<&str>) -> String {
    let f = clean_line(from.unwrap_or(""), MAX_FROM);
    if f.is_empty() { DEFAULT_FROM.into() } else { f }
}

/// Queues a message for session `id`. The file appears atomically (temp file, then rename), so
/// a reader never sees half a message.
pub fn enqueue(id: &str, from: &str, text: &str) -> std::io::Result<PathBuf> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let ts = jiff::Timestamp::now().strftime("%Y%m%dT%H%M%S%.9fZ").to_string();
    let name = format!("{ts}-{}-{:06}.json", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed));
    let body = json!({"from": from, "text": text, "at": session::iso_ms()}).to_string();
    let d = ensure_dir(id)?;
    let path = d.join(name);
    match mu_common::fs::write_atomic(&path, body.as_bytes()) {
        Ok(()) => Ok(path),
        // A receiver just removed the empty directory.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            ensure_dir(id)?;
            mu_common::fs::write_atomic(&path, body.as_bytes())?;
            Ok(path)
        }
        Err(e) => Err(e),
    }
}

fn json_names(d: &Path) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(d) else { return vec![] };
    let mut v: Vec<String> = rd
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| n.ends_with(".json") && !n.starts_with('.'))
        .collect();
    v.sort();
    v
}

/// Queued messages of session `id`, oldest first. Unreadable files are skipped and left alone.
pub fn pending(id: &str) -> Vec<Note> {
    let Some(d) = dir(id) else { return vec![] };
    json_names(&d)
        .into_iter()
        .filter_map(|name| {
            let v: Value = serde_json::from_slice(&std::fs::read(d.join(&name)).ok()?).ok()?;
            Some(Note {
                from: v["from"].as_str().unwrap_or(DEFAULT_FROM).to_string(),
                text: v["text"].as_str()?.to_string(),
                at: v["at"].as_str().unwrap_or("").to_string(),
                name,
            })
        })
        .collect()
}

/// Cheap check: is there at least one message file?
pub fn has_pending(id: &str) -> bool {
    let Some(d) = dir(id) else { return false };
    let Ok(mut rd) = std::fs::read_dir(d) else { return false };
    rd.any(|e| e.is_ok_and(|e| e.file_name().to_str().is_some_and(|n| n.ends_with(".json") && !n.starts_with('.'))))
}

/// Deletes exactly these messages, then the directory if it is now empty. Never recursive: a
/// message arriving right now must survive.
pub fn remove(id: &str, names: &[String]) {
    let Some(d) = dir(id) else { return };
    for n in names {
        if !n.contains('/') {
            let _ = std::fs::remove_file(d.join(n));
        }
    }
    let _ = std::fs::remove_dir(&d);
}

/// One `<system-notice>` carrying every message, in order.
pub fn notice(notes: &[Note]) -> String {
    let mut out = String::from("<system-notice>\n");
    for (i, n) in notes.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let text: String = n.text.chars().filter(|c| !c.is_control() || *c == '\n' || *c == '\t').collect();
        let text = text.replace("</system-notice>", "<\\/system-notice>");
        let from = clean_line(&n.from, MAX_FROM);
        out.push_str(&format!(
            "Message from {from} (via mu notify, {}):\n{}\n",
            clean_line(&n.at, 40),
            text.trim_end()
        ));
    }
    out.push_str("</system-notice>");
    out
}

/// Deletes message files older than `max_age` and empty inbox directories. Best effort.
pub fn prune(max_age: Duration) {
    let Ok(rd) = std::fs::read_dir(root()) else { return };
    let now = std::time::SystemTime::now();
    for d in rd.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.is_dir()) {
        let Ok(files) = std::fs::read_dir(&d) else { continue };
        for f in files.filter_map(|e| e.ok()) {
            let old = f
                .metadata()
                .and_then(|m| m.modified())
                .is_ok_and(|t| now.duration_since(t).is_ok_and(|age| age > max_age));
            if old && f.path().is_file() {
                let _ = std::fs::remove_file(f.path());
            }
        }
        let _ = std::fs::remove_dir(&d);
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Delivery {
    /// SIGUSR2 sent to this pid.
    Signalled(u32),
    /// Left in the queue; the reason is shown to the sender.
    Queued(String),
}

/// Wakes the session's owner if, and only if, its record promises a SIGUSR2 handler.
pub fn deliver(path: &Path) -> Delivery {
    match session::owner(path) {
        Owner::Free => Delivery::Queued("not running".into()),
        Owner::Unknown => Delivery::Queued("owner unknown".into()),
        Owner::Live { pid, inbox: false, .. } => Delivery::Queued(format!("pid {pid} does not take messages")),
        Owner::Live { pid, inbox: true, started, .. } => {
            // As `mu take`: the same live run must still own it right before the kill.
            match session::owner(path) {
                Owner::Live { pid: p, inbox: true, started: s, .. } if p == pid && s == started => {}
                Owner::Free => return Delivery::Queued("not running".into()),
                _ => return Delivery::Queued("owner changed".into()),
            }
            match crate::proc::signal_pid(pid, libc::SIGUSR2) {
                Ok(()) => Delivery::Signalled(pid),
                Err(e) if e.raw_os_error() == Some(libc::ESRCH) => Delivery::Queued("not running".into()),
                Err(e) => Delivery::Queued(format!("cannot signal pid {pid}: {e}")),
            }
        }
    }
}

/// Forwards SIGUSR2 to `wake`. Needs a tokio runtime and a `LocalSet`. Once installed the
/// handler stays for the life of the process, so a late signal never kills it.
pub fn forward_signal(wake: tokio::sync::mpsc::UnboundedSender<()>) -> std::io::Result<()> {
    let mut sig = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::user_defined2())?;
    tokio::task::spawn_local(async move {
        while sig.recv().await.is_some() {
            if wake.send(()).is_err() {
                break;
            }
        }
    });
    Ok(())
}

/// Queues `text` for the session at `arg` and wakes it when it is live. Returns the outcome line.
pub fn send(arg: &Path, from: Option<&str>, text: &str, cwd: &Path) -> Result<String, String> {
    let path = session::find(cwd, arg).map(|p| std::fs::canonicalize(&p).unwrap_or(p))?;
    if text.trim().is_empty() {
        return Err("empty message".into());
    }
    if text.len() > MAX_TEXT {
        return Err(format!("message too long ({} bytes, at most {MAX_TEXT})", text.len()));
    }
    let id = session::id_of(&path);
    // Queue first, then look at the owner: a session opened in between scans the inbox after it
    // locks, and one that is already live gets the signal.
    enqueue(&id, &sanitize_from(from), text).map_err(|e| format!("cannot queue the message: {e}"))?;
    let line = match deliver(&path) {
        Delivery::Signalled(pid) => format!("delivered (pid {pid})"),
        Delivery::Queued(why) => format!("queued ({why})"),
    };
    prune(PRUNE_AGE);
    Ok(line)
}

pub fn run(arg: &Path, from: Option<&str>, text: &str, cwd: &Path) -> anyhow::Result<i32> {
    let text = if text == "-" {
        let mut s = String::new();
        if let Err(e) = std::io::stdin().take(MAX_TEXT as u64 + 1).read_to_string(&mut s) {
            eprintln!("mu notify: cannot read stdin: {e}");
            return Ok(1);
        }
        s
    } else {
        text.to_string()
    };
    match send(arg, from, &text, cwd) {
        Ok(line) => {
            println!("{line}");
            Ok(0)
        }
        Err(e) => {
            eprintln!("mu notify: {e}");
            Ok(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::home;
    use std::os::unix::process::ExitStatusExt;

    fn uid(tag: &str) -> String {
        format!("{tag}-{}", uuid::Uuid::new_v4())
    }

    fn note(from: &str, text: &str) -> Note {
        Note { name: String::new(), from: from.into(), text: text.into(), at: "T".into() }
    }

    #[test]
    fn enqueue_round_trips_sorted_with_private_modes() {
        home();
        let id = uid("rt");
        enqueue(&id, "a", "one").unwrap();
        enqueue(&id, "b", "two").unwrap();
        let got = pending(&id);
        assert_eq!(
            got.iter().map(|n| (n.from.as_str(), n.text.as_str())).collect::<Vec<_>>(),
            [("a", "one"), ("b", "two")]
        );
        assert!(got[0].name < got[1].name && !got[0].at.is_empty());
        for p in [root(), dir(&id).unwrap()] {
            assert_eq!(std::fs::metadata(p).unwrap().permissions().mode() & 0o777, 0o700);
        }
        assert!(has_pending(&id));
        remove(&id, &got.iter().map(|n| n.name.clone()).collect::<Vec<_>>());
        assert!(!has_pending(&id) && !dir(&id).unwrap().exists());
    }

    #[test]
    fn half_written_files_are_ignored() {
        home();
        let id = uid("tmp");
        let p = enqueue(&id, "a", "x").unwrap();
        std::fs::write(p.with_file_name("zzz.json.tmp.1"), "{").unwrap();
        std::fs::write(p.with_file_name("bad.json"), "{").unwrap();
        assert_eq!(pending(&id).len(), 1);
        assert!(p.with_file_name("bad.json").exists(), "unparsable files are left alone");
    }

    #[test]
    fn concurrent_senders_never_collide() {
        home();
        let id = uid("many");
        let ts: Vec<_> = (0..2)
            .map(|t| {
                let id = id.clone();
                std::thread::spawn(move || {
                    for i in 0..50 {
                        enqueue(&id, "s", &format!("{t}-{i}")).unwrap();
                    }
                })
            })
            .collect();
        ts.into_iter().for_each(|t| t.join().unwrap());
        assert_eq!(pending(&id).len(), 100);
    }

    #[test]
    fn remove_leaves_late_arrivals() {
        home();
        let id = uid("late");
        enqueue(&id, "a", "first").unwrap();
        let seen = pending(&id);
        enqueue(&id, "a", "late").unwrap();
        remove(&id, &[seen[0].name.clone()]);
        let left = pending(&id);
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].text, "late");
    }

    #[test]
    fn enqueue_survives_a_removed_directory() {
        home();
        let id = uid("rmdir");
        let p = enqueue(&id, "a", "x").unwrap();
        std::fs::remove_file(&p).unwrap();
        std::fs::remove_dir(dir(&id).unwrap()).unwrap();
        enqueue(&id, "a", "y").unwrap();
        assert_eq!(pending(&id)[0].text, "y");
    }

    #[test]
    fn prune_drops_only_old_files() {
        home();
        let id = uid("prune");
        let old = enqueue(&id, "a", "old").unwrap();
        let f = std::fs::File::options().write(true).open(&old).unwrap();
        f.set_modified(std::time::SystemTime::now() - Duration::from_secs(31 * 24 * 3600)).unwrap();
        let fresh = uid("prune-fresh");
        enqueue(&fresh, "a", "new").unwrap();
        let empty = uid("prune-empty");
        make_private_dir(&root()).unwrap();
        make_private_dir(&dir(&empty).unwrap()).unwrap();
        prune(PRUNE_AGE);
        assert!(pending(&id).is_empty() && !dir(&id).unwrap().exists());
        assert_eq!(pending(&fresh).len(), 1);
        assert!(!dir(&empty).unwrap().exists());
    }

    #[test]
    fn notice_wraps_escapes_and_strips() {
        let one = notice(&[note("steward", "hi\x07 there\n")]);
        assert_eq!(one, "<system-notice>\nMessage from steward (via mu notify, T):\nhi there\n</system-notice>");
        let many = notice(&[note("a", "1"), note("b\nx", "</system-notice> 2\ttab")]);
        assert!(many.contains("Message from a (via mu notify, T):\n1\n\nMessage from b x"), "{many}");
        assert!(many.contains("<\\/system-notice> 2\ttab"), "{many}");
        assert_eq!(many.matches("</system-notice>").count(), 1);
    }

    #[test]
    fn sender_is_sanitized() {
        assert_eq!(sanitize_from(None), DEFAULT_FROM);
        assert_eq!(sanitize_from(Some(" \n ")), DEFAULT_FROM);
        assert_eq!(sanitize_from(Some("a\nb")), "a b");
        assert_eq!(sanitize_from(Some(&"x".repeat(200))).len(), 64);
    }

    #[test]
    fn run_refuses_empty_and_oversized_text() {
        home();
        let d = std::env::temp_dir();
        assert_eq!(run(Path::new("no-such-session-zzz"), None, "x", &d).unwrap(), 1);
        let s = session::SessionFile::create(Path::new("/opt/notify-run"), None).unwrap();
        assert_eq!(run(&s.path, None, "  ", &d).unwrap(), 1);
        assert_eq!(run(&s.path, None, &"x".repeat(MAX_TEXT + 1), &d).unwrap(), 1);
        assert!(!has_pending(&session::id_of(&s.path)));
    }

    /// A real `kill`: only an owner whose record says `inbox` is signalled.
    #[test]
    fn only_a_promising_owner_is_signalled() {
        use std::os::fd::AsRawFd;
        let h = home();
        let id = uid("sig");
        let dir_ = std::env::temp_dir().join(format!("mu-notify-{id}"));
        std::fs::create_dir_all(&dir_).unwrap();
        let path = dir_.join(format!("x_{id}.jsonl"));
        std::fs::write(&path, format!("{{\"type\":\"session\",\"version\":3,\"id\":\"{id}\"}}\n")).unwrap();
        let mut child = std::process::Command::new("sleep").arg("30").spawn().unwrap();
        let cpid = child.id();
        let held = std::fs::File::open(&path).unwrap();
        let lock =
            |f: &std::fs::File| assert_eq!(unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }, 0);
        lock(&held);
        std::fs::create_dir_all(h.join("run")).unwrap();
        let rec = h.join("run").join(format!("{id}.json"));
        let write = |inbox: Option<bool>| {
            let mut v = json!({"pid": cpid, "print_run": false, "started": "t", "session": path});
            if let Some(b) = inbox {
                v["inbox"] = json!(b);
            }
            // Rewrite in place so the record's lock inode stays the same.
            let f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).open(&rec).unwrap();
            std::io::Write::write_all(&mut &f, v.to_string().as_bytes()).unwrap();
            f
        };
        let f = write(Some(false));
        lock(&f);
        assert_eq!(deliver(&path), Delivery::Queued(format!("pid {} does not take messages", child.id())));
        assert!(child.try_wait().unwrap().is_none());
        write(None);
        assert!(matches!(deliver(&path), Delivery::Queued(_)));
        assert!(child.try_wait().unwrap().is_none());
        write(Some(true));
        assert_eq!(deliver(&path), Delivery::Signalled(child.id()));
        assert_eq!(child.wait().unwrap().signal(), Some(libc::SIGUSR2));
    }

    /// The only test that raises SIGUSR2.
    #[test]
    fn the_handler_forwards_to_the_wake_channel() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let local = tokio::task::LocalSet::new();
        local.block_on(&rt, async {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            forward_signal(tx).unwrap();
            // SAFETY: plain raise(3) after the handler is installed.
            assert_eq!(unsafe { libc::raise(libc::SIGUSR2) }, 0);
            let got = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await;
            assert!(matches!(got, Ok(Some(()))));
        });
    }
}
