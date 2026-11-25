//! `/open` and `/upload`: files of the machine the agent runs on shown here, and files of this machine
//! sent there. The bytes move through `transfer`.

use super::child::{ChildThen, private_dir};
use super::transfer::{HELPER_MAX, resolve_path, strip_line_suffix};
use super::*;
use mu_provider::auth::chatgpt::browser::open_browser;
use std::cell::RefCell;
use std::path::Path;

/// Files the agent touched or mentioned that `/open` offers, newest first.
pub(super) const RECENT_MAX: usize = 50;
/// How much of a file decides whether it is text.
const SNIFF: usize = 8 * 1024;
/// Files the system opener would run rather than show.
const NEVER_OPEN: [&str; 19] = [
    "app", "command", "desktop", "terminal", "scpt", "workflow", "jar", "dmg", "pkg", "exe", "bat", "cmd", "com",
    "msi", "ps1", "lnk", "url", "webloc", "inetloc",
];

/// An `/upload` in flight: the files still going out, the `local://` names that arrived, and the errors.
#[derive(Default)]
struct Uploads {
    left: usize,
    done: Vec<String>,
    failed: Vec<String>,
}

/// The private directories holding copies made by `/open`; they are removed when mu exits.
#[derive(Default)]
pub(super) struct Opened(Vec<PathBuf>);

impl Drop for Opened {
    fn drop(&mut self) {
        for dir in &self.0 {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// Remembers `path` as the newest file touched.
pub(super) fn note_recent(recent: &mut std::collections::VecDeque<String>, path: &str) {
    recent.retain(|p| p != path);
    recent.push_front(path.to_string());
    recent.truncate(RECENT_MAX);
}

/// Records path-like tokens without consulting the client's filesystem (which may be remote).
pub(super) fn note_mentioned(recent: &mut std::collections::VecDeque<String>, text: &str) {
    let mut remaining = text;
    while !remaining.is_empty() {
        remaining = remaining.trim_start_matches(char::is_whitespace);
        if remaining.is_empty() {
            break;
        }
        let word_end = remaining.find(char::is_whitespace).unwrap_or(remaining.len());
        let prefix = remaining.trim_start_matches(|c: char| "()[]{}<>,;!?*".contains(c));
        let quote = prefix.chars().next().filter(|c| "`'\"".contains(*c));
        let end = quote
            .and_then(|q| {
                let start = remaining.len() - prefix.len() + q.len_utf8();
                remaining[start..].find(q).map(|end| start + end + q.len_utf8())
            })
            .map(|end| end + remaining[end..].find(char::is_whitespace).unwrap_or(remaining.len() - end))
            .unwrap_or(word_end);
        let (token, rest) = remaining.split_at(end);
        remaining = rest;
        let token = token.trim_start_matches(|c: char| "`'\"()[]{}<>,;!?*".contains(c));
        let token = token.trim_end_matches(|c: char| "`'\"()[]{}<>,;!?*.".contains(c));
        if token.contains("://") && !token.starts_with("local://") {
            continue;
        }
        let token = token.trim_end_matches(':');
        let path = strip_line_suffix(token);
        let explicit = path.starts_with('/')
            || path.starts_with("./")
            || path.starts_with("../")
            || path.starts_with("~/")
            || path.starts_with("local://");
        let relative_file = path.contains('/')
            && path.rsplit('/').next().is_some_and(|name| {
                name.rsplit_once('.').is_some_and(|(stem, ext)| !stem.is_empty() && !ext.is_empty())
            });
        if (explicit || relative_file) && path.chars().any(char::is_alphanumeric) {
            note_recent(recent, path);
        }
    }
}

/// The first 8 KiB has no NUL and is UTF-8, allowing the cut to fall inside a character.
fn is_text(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(SNIFF)];
    if head.contains(&0) {
        return false;
    }
    match std::str::from_utf8(head) {
        Ok(_) => true,
        Err(e) => e.error_len().is_none() && head.len() < bytes.len(),
    }
}

fn file_name(path: &Path) -> String {
    path.file_name().and_then(|n| n.to_str()).filter(|n| !n.is_empty()).unwrap_or("file").to_string()
}

/// `name` with everything but `[A-Za-z0-9._-]` replaced by `-`, as a name for an upload.
fn sanitize(name: &str) -> String {
    let clean: String =
        name.chars().map(|c| if c.is_ascii_alphanumeric() || "._-".contains(c) { c } else { '-' }).collect();
    let clean = clean.trim_start_matches('.');
    if clean.is_empty() { "file".into() } else { clean.chars().take(100).collect() }
}

/// The files `/upload` was given: the whole argument when it names one file, else one per word.
fn upload_paths(arg: &str, home: &Path, cwd: &Path) -> Vec<PathBuf> {
    let resolve = |a: &str| resolve_path(a, home, cwd, None).unwrap_or_else(|_| PathBuf::from(a));
    let unquote = |a: &str| a.trim_matches(|c| c == '\'' || c == '"').replace("\\ ", " ");
    let whole = resolve(&unquote(arg.trim()));
    if whole.is_file() {
        return vec![whole];
    }
    arg.split_whitespace().map(|w| resolve(&unquote(w))).collect()
}

impl App {
    /// `/open [path]`: a copy of a file of the agent's machine, in `$EDITOR` when it is text and in the
    /// system's opener otherwise.
    pub(super) fn open_cmd(&mut self, arg: &str) {
        let arg = arg.trim();
        if arg.is_empty() {
            return self.open_recent_picker();
        }
        let path = match self.resolve_host_path(strip_line_suffix(arg)) {
            Ok(p) => p,
            Err(e) => return self.error(&e),
        };
        let shown = path.clone();
        self.fetch_file(path, move |app, r| match r {
            Ok(bytes) => app.show_copy(&shown, &bytes),
            Err(e) => app.error(&e),
        });
    }

    fn show_copy(&mut self, remote: &Path, bytes: &[u8]) {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let name = file_name(remote);
        let dir = match private_dir("mu-open") {
            Ok(d) => d,
            Err(e) => return self.error(&format!("cannot make a temp directory: {e}")),
        };
        self.opened.0.push(dir.clone());
        let copy = dir.join(&name);
        let written = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&copy)
            .and_then(|mut f| f.write_all(bytes));
        if let Err(e) = written {
            return self.error(&format!("cannot write {}: {e}", copy.display()));
        }
        if is_text(bytes) {
            self.notice("opened a copy; changes are not sent back");
            return self.run_editor(&copy, ChildThen::OpenCopy(dir));
        }
        let ext = copy.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
        if NEVER_OPEN.contains(&ext.as_str()) {
            return self.error(&format!("refusing to open a {ext} file; the copy is at {}", copy.display()));
        }
        if open_browser(&copy.to_string_lossy()) {
            self.notice(&format!("opened {name}"));
        } else {
            self.error(&format!("cannot start the system opener; the copy is at {}", copy.display()));
        }
    }

    /// `/upload path…`: sends files of this machine to the session's `local://uploads/` and puts their
    /// `local://` names in the composer.
    pub(super) fn upload_cmd(&mut self, arg: &str) {
        if !self.host.is_remote() {
            return self.notice("not attached: the agent already sees this machine's files");
        }
        if arg.trim().is_empty() {
            return self.error("usage: /upload <path>…");
        }
        let Some(local_dir) = self.agent.status.local_dir.clone() else {
            return self.error("no session directory yet");
        };
        let cwd = std::env::current_dir().unwrap_or_else(|_| self.cwd.clone());
        let files = upload_paths(arg, &paths::home(), &cwd);
        for f in &files {
            match std::fs::metadata(f) {
                Ok(m) if m.is_file() && m.len() <= HELPER_MAX => {}
                Ok(m) if m.is_file() => return self.error(&format!("{} is larger than 512 MiB", f.display())),
                Ok(_) => return self.error(&format!("{} is not a file", f.display())),
                Err(e) => return self.error(&format!("{}: {e}", f.display())),
            }
        }
        let state = Rc::new(RefCell::new(Uploads { left: files.len(), ..Default::default() }));
        for file in files {
            let name = format!("{}-{}", &uuid::Uuid::new_v4().simple().to_string()[..8], sanitize(&file_name(&file)));
            let dest = Path::new(&local_dir).join("uploads").join(&name);
            let uri = format!("local://uploads/{name}");
            let state = state.clone();
            let tx = self.ui_tx.clone();
            tokio::task::spawn_local(async move {
                let read = tokio::task::spawn_blocking(move || {
                    std::fs::read(&file).map_err(|e| format!("{}: {e}", file.display()))
                });
                let read = read.await.unwrap_or_else(|e| Err(e.to_string()));
                let _ = tx.send(UiMsg::Host(Box::new(move |app| match read {
                    Err(e) => app.upload_done(&state, Err(e)),
                    Ok(bytes) => app.put_file(dest, bytes, move |app, r| app.upload_done(&state, r.map(|()| uri))),
                })));
            });
        }
    }

    fn upload_done(&mut self, state: &Rc<RefCell<Uploads>>, r: Result<String, String>) {
        let u = &mut *state.borrow_mut();
        match r {
            Ok(uri) => u.done.push(uri),
            Err(e) => u.failed.push(e),
        }
        u.left -= 1;
        if u.left > 0 {
            return;
        }
        let (done, failed) = (std::mem::take(&mut u.done), std::mem::take(&mut u.failed));
        for e in &failed {
            self.error(&format!("upload: {e}"));
        }
        if done.is_empty() {
            return;
        }
        let text = format!("{} ", done.join(" "));
        if let Err(e) = self.prompt.composer.on_paste(&text, &self.prompt.commands) {
            self.error(&e);
        }
        self.notice(&format!("uploaded {} file(s)", done.len()));
    }

    /// `o` in the jobs panel: forward what the job listens on (attached) and open the first in the
    /// browser; without an attach helper, not attached, localhost is already the agent's machine.
    pub(super) fn open_ports(&mut self, job: String, ports: Vec<u16>) {
        let Some(lowest) = ports.iter().copied().min() else {
            return self.notice("no listening ports");
        };
        if !self.host.is_remote() {
            return self.open_url(&format!("http://localhost:{lowest}"));
        }
        let todo: Vec<u16> = ports.iter().copied().filter(|p| self.forwards.local(&job, *p).is_none()).collect();
        self.forward_ports(job, todo, lowest);
    }

    fn forward_ports(&mut self, job: String, mut todo: Vec<u16>, lowest: u16) {
        let Some(port) = (!todo.is_empty()).then(|| todo.remove(0)) else {
            let local = self.forwards.local(&job, lowest).unwrap_or(lowest);
            return self.open_url(&format!("http://localhost:{local}"));
        };
        let next = job.clone();
        self.forward_port(port, move |app, r| match r {
            Ok(local) => {
                app.forwards.add(&next, port, local);
                app.notice(&format!("forwarded {port} → localhost:{local}"));
                app.forward_ports(next.clone(), todo, lowest);
            }
            Err(e) => app.error(&e),
        });
    }

    fn open_url(&mut self, url: &str) {
        if open_browser(url) {
            self.notice(&format!("opened {url}"));
        } else {
            self.error(&format!("cannot start the system opener for {url}"));
        }
    }

    /// A pasted line that names a file of this machine, when the agent is on another one: the paste stays,
    /// with a hint about `/upload`. Nothing is sent without being asked.
    pub(super) fn upload_hint(&mut self, pasted: &str) {
        if !self.host.is_remote() || mu_base::image::paths_in_paste(pasted).is_some() {
            return;
        }
        if let Some(path) = mu_base::image::file_in_paste(pasted) {
            let name = file_name(&path);
            self.notice(&format!("{name} is a file on this machine; /upload {} sends it to the agent", path.display()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_is_told_from_binary() {
        assert!(is_text(b"fn main() {}\n"));
        assert!(is_text("héllo".as_bytes()));
        assert!(!is_text(b"abc\0def"));
        assert!(!is_text(&[0xff, 0xfe, 0x00, 0x01]));
        assert!(!is_text(b"\xff\xfe abc"));
        // A character cut at the 8 KiB mark is still text; the same bytes at the end of a short file are not.
        let mut cut = vec![b'a'; SNIFF - 1];
        cut.extend("é".as_bytes());
        assert!(is_text(&cut));
        assert!(!is_text(&cut[..SNIFF]));
    }

    #[test]
    fn uploading_without_an_agent_elsewhere_says_so() {
        use crate::app::keys::tests::{test_app_with_commands, transcript_text};
        let dir = mu_testutil::tmp();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "x").unwrap();
        let (mut app, _cmds) = test_app_with_commands(std::env::temp_dir().join("mu-unused-history.jsonl"));
        app.upload_cmd(&file.to_string_lossy());
        let text = transcript_text(&app);
        assert!(text.contains("not attached"), "{text}");
        // The hint about a pasted path is for an attached client only.
        app.upload_hint(&file.to_string_lossy());
        assert!(!transcript_text(&app).contains("/upload"), "no hint while the agent is here");
    }

    #[test]
    fn upload_names_are_safe() {
        assert_eq!(sanitize("my file (1).png"), "my-file--1-.png");
        assert_eq!(sanitize("../../etc/passwd"), "-..-etc-passwd");
        assert_eq!(sanitize(".hidden"), "hidden");
        assert_eq!(sanitize("日本語"), "---");
        assert_eq!(sanitize(""), "file");
    }

    #[test]
    fn mentioned_paths_ignore_urls_and_prose() {
        let mut recent = std::collections::VecDeque::new();
        note_mentioned(
            &mut recent,
            "See `/usr/share/cmake-4.2/Templates/Windows/SplashScreen.png`, (src/app/main.rs:42), \
             `./config/settings.toml` and ~/notes.txt. Not https://example.com/a.png or and/or.",
        );
        assert_eq!(
            recent,
            [
                "~/notes.txt",
                "./config/settings.toml",
                "src/app/main.rs",
                "/usr/share/cmake-4.2/Templates/Windows/SplashScreen.png"
            ]
        );
    }

    #[test]
    fn mentioned_paths_keep_quoted_spaces_without_fragments() {
        let mut recent = std::collections::VecDeque::new();
        note_mentioned(
            &mut recent,
            "See `/tmp/my notes/file.txt`, (\"src/my files/main.rs:42\") and \
             'local://uploads/my file.txt'. Not `https://example.com/my file.txt`.",
        );
        assert_eq!(recent, ["local://uploads/my file.txt", "src/my files/main.rs", "/tmp/my notes/file.txt"]);
    }

    #[test]
    fn mentioned_paths_keep_unmatched_quotes_as_separate_words() {
        let mut recent = std::collections::VecDeque::new();
        note_mentioned(&mut recent, "See `/tmp/a.txt and ./other.txt");
        assert_eq!(recent, ["./other.txt", "/tmp/a.txt"]);
    }

    #[test]
    fn recent_files_are_newest_first_without_repeats() {
        let mut r = std::collections::VecDeque::new();
        for p in ["a", "b", "a", "c"] {
            note_recent(&mut r, p);
        }
        assert_eq!(r, ["c", "a", "b"]);
        for i in 0..100 {
            note_recent(&mut r, &i.to_string());
        }
        assert_eq!(r.len(), RECENT_MAX);
        assert_eq!(r[0], "99");
    }

    #[test]
    fn an_upload_argument_is_one_file_or_several_words() {
        let dir = mu_testutil::tmp();
        let spaced = dir.path().join("a b.txt");
        std::fs::write(&spaced, "x").unwrap();
        std::fs::write(dir.path().join("c.txt"), "x").unwrap();
        let home = Path::new("/nonexistent-home");
        assert_eq!(upload_paths(&spaced.to_string_lossy(), home, dir.path()), std::slice::from_ref(&spaced));
        let escaped = spaced.to_string_lossy().replace(' ', "\\ ");
        assert_eq!(upload_paths(&escaped, home, dir.path()), [spaced]);
        assert_eq!(
            upload_paths("c.txt other.txt", home, dir.path()),
            [dir.path().join("c.txt"), dir.path().join("other.txt")]
        );
    }
}
