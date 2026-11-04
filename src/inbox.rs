//! The reading side of `mu notify`: the messages queued for a session in
//! `$MU_HOME/inbox/<id>/<ts>-<pid>-<n>.json`. The owner of the session lists, announces and drains
//! them; `notify.rs` writes them. An interactive owner announces SIGUSR2 support in its owner record
//! (`inbox: true`) and drains the directory itself.
//!
//! (Not to be confused with `events::Inbox`, the steer/follow-up queue.)
use crate::session;
use mu_common::paths;
use serde_json::Value;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub(crate) const MAX_FROM: usize = 64;
pub(crate) const DEFAULT_FROM: &str = "another process";

/// One queued message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    /// File name inside the session's inbox directory.
    pub name: String,
    pub from: String,
    pub text: String,
    pub at: String,
}

pub(crate) fn root() -> PathBuf {
    paths::mu_home().join("inbox")
}

pub(crate) fn dir(id: &str) -> Option<PathBuf> {
    session::valid_id(id).then(|| root().join(id))
}

pub(crate) fn make_private_dir(p: &Path) -> std::io::Result<()> {
    match std::fs::DirBuilder::new().mode(0o700).create(p) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Ok(()),
        Err(e) => return Err(e),
    }
    // The umask may have stripped bits.
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700))
}

pub(crate) fn ensure_dir(id: &str) -> std::io::Result<PathBuf> {
    let d = dir(id).ok_or_else(|| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    make_private_dir(&root())?;
    make_private_dir(&d)?;
    Ok(d)
}

pub(crate) fn clean_line(s: &str, max: usize) -> String {
    let one: String = s.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    one.trim().chars().take(max).collect()
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

#[cfg(test)]
mod tests {
    use super::*;

    fn note(from: &str, text: &str) -> Note {
        Note { name: String::new(), from: from.into(), text: text.into(), at: "T".into() }
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
