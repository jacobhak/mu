//! An open session file: created or opened under its lock, appended to entry by entry.
use super::format::{EntryKind, TITLE_SLOT_BYTES, TitleSource, parse, title_slot};
use super::list::id_of;
use super::owner::{LOCK_RETRY, OwnerRecord, live_record};
use anyhow::{Context, Result, bail};
use mu_base::message::Message;
use mu_base::process::try_flock;
use mu_common::paths;
use mu_common::time::iso_ms;
use serde_json::{Value, json};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

pub struct SessionFile {
    pub path: PathBuf,
    file: Option<File>,
    owner: Option<OwnerRecord>,
    leaf: Option<String>,
    /// Every entry id with its parent's.
    ids: std::collections::HashMap<String, Option<String>>,
    /// Consecutive failed writes; persistence is given up at `MAX_WRITE_FAILURES`.
    failures: u32,
    /// The last write may have left a partial line, so the next one starts on a fresh line.
    torn: bool,
    /// Receives write errors, so the user sees them.
    pub on_error: Option<Box<dyn Fn(String)>>,
}

const MAX_WRITE_FAILURES: u32 = 3;

/// Takes an exclusive, non-blocking `flock` that lives as long as `file`.
fn lock(file: &File, path: &Path) -> Result<()> {
    let deadline = std::time::Instant::now() + LOCK_RETRY;
    loop {
        let e = match try_flock(file, libc::LOCK_EX) {
            Ok(()) => return Ok(()),
            Err(e) => e,
        };
        if e.kind() != std::io::ErrorKind::WouldBlock {
            return Err(e).with_context(|| format!("lock {}", path.display()));
        }
        if std::time::Instant::now() >= deadline {
            let mut msg = format!("session is open in another mu process: {}", path.display());
            if let Some(o) = live_record(path) {
                msg.push_str(&format!(" (pid {})", o.pid));
                if o.print_run {
                    msg.push_str(&format!(
                        ": a running `mu -p`; `mu --resume {}` stops it after its current step and opens it here",
                        id_of(path)
                    ));
                }
            }
            bail!("{msg}");
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

impl Drop for SessionFile {
    fn drop(&mut self) {
        // Unlink the record before the session lock is released.
        self.owner = None;
    }
}

impl SessionFile {
    /// Creates the file with its title slot and header.
    pub fn create(cwd: &Path, parent_session: Option<&Path>) -> Result<Self> {
        Self::create_with(cwd, parent_session, None)
    }

    /// Creates the file for a fork of `from`. The header records it as `forkedFrom`, not as
    /// `parentSession`: that one marks a subagent, whose cost its parent's status line adds up.
    pub fn create_fork(cwd: &Path, from: &Path) -> Result<Self> {
        Self::create_with(cwd, None, Some(from))
    }

    fn create_with(cwd: &Path, parent_session: Option<&Path>, forked_from: Option<&Path>) -> Result<Self> {
        let dir = paths::session_dir(cwd);
        // Session files hold the conversation: the state home and the session directory are owner-only.
        mu_common::fs::create_private_dir(&paths::profile_home())
            .and_then(|()| mu_common::fs::create_private_dir(&dir))
            .with_context(|| format!("create {}", dir.display()))?;
        let id = uuid::Uuid::now_v7();
        let ts = jiff::Timestamp::now().strftime("%Y-%m-%dT%H-%M-%S-%3fZ").to_string();
        let path = dir.join(format!("{ts}_{id}.jsonl"));
        let mut file = {
            use std::os::unix::fs::OpenOptionsExt;
            OpenOptions::new().create_new(true).append(true).mode(0o600).open(&path)?
        };
        lock(&file, &path)?;
        let mut header =
            json!({"type":"session","version":3,"id":id.to_string(),"timestamp":iso_ms(),"cwd":cwd.to_string_lossy()});
        if let Some(p) = parent_session {
            header["parentSession"] = json!(p.to_string_lossy());
        }
        if let Some(p) = forked_from {
            header["forkedFrom"] = json!(p.to_string_lossy());
        }
        let owner = OwnerRecord::claim(&path, Some(cwd), parent_session);
        file.write_all(format!("{}{header}\n", title_slot("", None)).as_bytes())?;
        Ok(Self {
            path,
            file: Some(file),
            owner,
            leaf: None,
            ids: Default::default(),
            failures: 0,
            torn: false,
            on_error: None,
        })
    }

    /// Opens an existing session for appending after `load`.
    pub fn open_append(path: &Path) -> Result<Self> {
        let mut file = OpenOptions::new().append(true).read(true).open(path)?;
        mu_common::fs::tighten(&file);
        lock(&file, path)?;
        let owner = OwnerRecord::claim(path, None, None);
        let (_, leaf, ids) = parse(path)?;
        // A crash can leave the last line unterminated; the next entry must not join it.
        let torn = {
            use std::io::{Read, Seek, SeekFrom};
            let len = file.metadata()?.len();
            let mut last = *b"\n";
            if len > 0 {
                file.seek(SeekFrom::End(-1))?;
                file.read_exact(&mut last)?;
            }
            last[0] != b'\n'
        };
        Ok(Self { path: path.into(), file: Some(file), owner, leaf, ids, failures: 0, torn, on_error: None })
    }

    /// Reopens this session's file from disk. The new handle needs the lock this one holds, so
    /// the lock is let go for the attempt; on failure it is taken back and `self` keeps appending.
    /// On success `self` no longer holds the lock and must be dropped.
    pub fn reopen(&mut self) -> Result<Self> {
        if let Some(f) = self.file.as_ref() {
            let _ = try_flock(f, libc::LOCK_UN);
        }
        // The record is unlinked on drop, so it goes before the new handle claims the same one.
        self.owner = None;
        match Self::open_append(&self.path) {
            Ok(new) => Ok(new),
            Err(e) => {
                if let Some(f) = self.file.as_ref() {
                    let _ = lock(f, &self.path);
                }
                self.owner = OwnerRecord::claim(&self.path, None, None);
                Err(e)
            }
        }
    }

    pub fn id(&self) -> String {
        id_of(&self.path)
    }

    /// The newest entry's id: where the next entry attaches.
    pub fn leaf(&self) -> Option<&str> {
        self.leaf.as_deref()
    }

    fn report(&self, msg: String) {
        match &self.on_error {
            Some(f) => f(msg),
            None => eprintln!("{msg}"),
        }
    }

    /// Appends an entry and returns its id, or `None` when nothing was written.
    pub(crate) fn append(&mut self, kind: EntryKind, payload: Value) -> Option<String> {
        self.file.as_ref()?;
        let id = loop {
            let u = uuid::Uuid::new_v4().simple().to_string();
            let id = u[u.len() - 8..].to_string();
            if !self.ids.contains_key(&id) {
                break id;
            }
        };
        let mut entry = json!({"type":kind.as_str(),"id":id,"parentId":self.leaf,"timestamp":iso_ms()});
        if let (Some(e), Value::Object(p)) = (entry.as_object_mut(), payload) {
            e.extend(p);
        }
        let line = format!("{}{entry}\n", if self.torn { "\n" } else { "" });
        let file = self.file.as_mut()?;
        if let Err(err) = file.write_all(line.as_bytes()) {
            self.torn = true;
            self.failures += 1;
            if self.failures >= MAX_WRITE_FAILURES {
                self.owner = None;
                self.file = None;
                self.report(format!(
                    "session write failed {} times in a row ({err}); this session is no longer saved",
                    self.failures
                ));
            } else {
                self.report(format!("session write failed: {err} (will retry on the next entry)"));
            }
            return None;
        }
        self.torn = false;
        self.failures = 0;
        self.ids.insert(id.clone(), self.leaf.clone());
        self.leaf = Some(id.clone());
        Some(id)
    }

    /// Appends an existing entry's line unchanged (a fork copies the active chain), so its id and
    /// parent keep their meaning.
    pub(crate) fn copy_entry(&mut self, line: &[u8], id: &str, parent: Option<&str>) -> std::io::Result<()> {
        let file = self.file.as_mut().ok_or_else(|| std::io::Error::other("session file is closed"))?;
        file.write_all(line)?;
        if !line.ends_with(b"\n") {
            file.write_all(b"\n")?;
        }
        self.ids.insert(id.into(), parent.map(String::from));
        self.leaf = Some(id.into());
        Ok(())
    }

    /// Rewinds to just before the entry `id`: the next entry continues from its parent, and so
    /// does a reload, because a `rewind` marker is written there. What came after stays in the
    /// file as an abandoned branch.
    pub fn rewind_before(&mut self, id: &str) -> Result<()> {
        let Some(parent) = self.ids.get(id).cloned() else { bail!("no entry {id} in this session") };
        let old = std::mem::replace(&mut self.leaf, parent);
        if self.append(EntryKind::Custom, json!({"customType":"rewind","data":{"before":id}})).is_none() {
            self.leaf = old;
            bail!("could not record the rewind in the session file");
        }
        self.sync();
        Ok(())
    }

    /// Rewinds to just after the entry `id`, which stays: the next entry, and a reload, continue
    /// from it. What came after stays in the file as an abandoned branch.
    pub fn rewind_after(&mut self, id: &str) -> Result<()> {
        if !self.ids.contains_key(id) {
            bail!("no entry {id} in this session");
        }
        let old = self.leaf.replace(id.to_string());
        if self.append(EntryKind::Custom, json!({"customType":"rewind","data":{"after":id}})).is_none() {
            self.leaf = old;
            bail!("could not record the rewind in the session file");
        }
        self.sync();
        Ok(())
    }

    /// Flushes the file to disk. Called after entries that must survive a crash, and at turn end.
    /// Unit tests skip it: `fdatasync` waits for the disk's journal and stalled a test for up to
    /// two minutes on a busy disk, and nothing in-process can observe the difference.
    pub fn sync(&self) {
        if cfg!(test) {
            return;
        }
        if let Some(f) = &self.file {
            let _ = f.sync_data();
        }
    }

    pub fn model_change(&mut self, model: &str) {
        self.append(EntryKind::ModelChange, json!({"model": model}));
    }
    pub fn thinking_change(&mut self, level: &str) {
        self.append(EntryKind::ThinkingLevelChange, json!({"thinkingLevel": level}));
    }
    pub fn message(&mut self, m: &Message) -> Option<String> {
        self.append(EntryKind::Message, json!({"message": m}))
    }
    pub fn compaction(
        &mut self,
        summary: &str,
        first_kept_entry_id: &str,
        tokens_before: u64,
        tokens_after: u64,
    ) -> Option<String> {
        let id = self.append(
            EntryKind::Compaction,
            json!({"summary":summary,"firstKeptEntryId":first_kept_entry_id,"tokensBefore":tokens_before,"tokensAfter":tokens_after}),
        );
        self.sync();
        id
    }
    pub fn mode_change(&mut self, mode: &str, data: Value) {
        self.append(EntryKind::ModeChange, json!({"mode":mode,"data":data}));
        self.sync();
    }
    pub fn custom(&mut self, custom_type: &str, data: Value) {
        self.append(EntryKind::Custom, json!({"customType":custom_type,"data":data}));
    }

    /// Rewrites the fixed title slot in place (only if line 1 really is one) and records a
    /// `title_change` entry, which `load` also honours.
    pub fn set_title(&mut self, title: &str, source: TitleSource) {
        if let Err(e) = rewrite_title_slot(&self.path, title, source.as_str()) {
            self.report(format!("title write failed: {e}"));
        }
        self.append(EntryKind::TitleChange, json!({"title":title,"source":source.as_str()}));
        self.sync();
    }
}

/// Rewrites the fixed title slot of the session file at `path` in place, only if line 1 really is
/// one. `source` is `"user"` or `"auto"`.
pub fn rewrite_title_slot(path: &Path, title: &str, source: &str) -> std::io::Result<()> {
    use std::os::unix::fs::FileExt;
    let slot = title_slot(title, Some(source));
    let f = OpenOptions::new().read(true).write(true).open(path)?;
    let mut head = [0u8; TITLE_SLOT_BYTES];
    let is_slot = f.read_exact_at(&mut head, 0).is_ok()
        && head[TITLE_SLOT_BYTES - 1] == b'\n'
        && !head[..TITLE_SLOT_BYTES - 1].contains(&b'\n')
        && serde_json::from_slice::<Value>(&head).is_ok_and(|v| v["type"] == "title");
    if is_slot { f.write_all_at(slot.as_bytes(), 0) } else { Ok(()) }
}

/// A copy of a remote child's session file, written line by line as the child sends them. It holds
/// the session lock for its life but claims no owner record: nothing lists it as a live session,
/// and `--resume` and `mu take` refuse it until the run ends.
pub struct MirrorFile {
    pub path: PathBuf,
    file: File,
}

impl MirrorFile {
    /// Creates `name` (a plain `*.jsonl` file name) in the session directory of `cwd`; an existing
    /// file is an error.
    pub fn create(cwd: &Path, name: &str) -> Result<Self> {
        if !super::list::valid_file_name(name) {
            bail!("not a session file name: {name:?}");
        }
        let dir = paths::session_dir(cwd);
        mu_common::fs::create_private_dir(&paths::profile_home())
            .and_then(|()| mu_common::fs::create_private_dir(&dir))
            .with_context(|| format!("create {}", dir.display()))?;
        let path = dir.join(name);
        let file = {
            use std::os::unix::fs::OpenOptionsExt;
            OpenOptions::new().create_new(true).append(true).mode(0o600).open(&path)?
        };
        lock(&file, &path)?;
        Ok(Self { path, file })
    }

    /// Appends `line` and a newline. A `title_change` entry also rewrites the title slot, as
    /// `set_title` did in the child.
    pub fn append_line(&mut self, line: &str) -> Result<()> {
        self.file.write_all(format!("{line}\n").as_bytes())?;
        if line.contains("\"title_change\"")
            && let Ok(v) = serde_json::from_str::<Value>(line)
            && v["type"] == "title_change"
            && let (Some(title), Some(source)) = (v["title"].as_str(), v["source"].as_str())
        {
            rewrite_title_slot(&self.path, title, source)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::format::load;
    use crate::session::list::{latest_for, list_recent};
    use mu_testutil::home;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mu-test-{name}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn roundtrip_and_resume_append() {
        home();
        let mut s = SessionFile::create(Path::new("/opt/proj"), None).unwrap();
        s.model_change("anthropic/x");
        s.thinking_change("high");
        s.message(&Message::user("hi"));
        s.message(&Message::user("again"));
        let l = load(&s.path).unwrap();
        assert_eq!(l.messages.len(), 2);
        assert_eq!(l.model.as_deref(), Some("anthropic/x"));
        assert_eq!(l.thinking.as_deref(), Some("high"));
        let path = s.path.clone();
        drop(s);
        let mut r = SessionFile::open_append(&path).unwrap();
        r.message(&Message::user("third"));
        assert_eq!(load(&path).unwrap().messages.len(), 3);
        assert_eq!(latest_for(Path::new("/opt/proj")), Some(path.clone()));
    }

    #[test]
    fn a_session_file_is_owner_only_and_an_older_one_is_tightened_on_reopen() {
        use std::os::unix::fs::PermissionsExt;
        home();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        let s = SessionFile::create(Path::new("/opt/modes"), None).unwrap();
        let path = s.path.clone();
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        drop(s);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o664)).unwrap();
        let _s = SessionFile::open_append(&path).unwrap();
        assert_eq!(mode(&path), 0o600);
    }

    #[test]
    fn title_slot_is_fixed_width_and_rewritten_in_place() {
        home();
        let mut s = SessionFile::create(Path::new("/opt/titles"), None).unwrap();
        s.message(&Message::user("hello there"));
        let before = std::fs::read(&s.path).unwrap();
        assert_eq!(before[255], b'\n');
        s.set_title(&"é".repeat(400), TitleSource::Auto);
        let after = std::fs::read(&s.path).unwrap();
        assert_eq!(after[255], b'\n');
        assert_eq!(&before[256..], &after[256..before.len()]);
        let slot: Value = serde_json::from_slice(&after[..255]).unwrap();
        let t = slot["title"].as_str().unwrap();
        assert!(t.starts_with('é') && t.chars().count() < 400);
        assert_eq!(load(&s.path).unwrap().title.as_deref(), Some("é".repeat(400).as_str()));
        let info = list_recent(Path::new("/opt/titles"), Some(Path::new("/opt/titles")), 50);
        assert_eq!(info[0].first_prompt, "hello there");
        assert_eq!(info[0].title.as_deref(), Some(t));
    }

    #[test]
    fn compaction_keeps_tail_and_extracts_state() {
        home();
        let mut s = SessionFile::create(Path::new("/opt/comp"), None).unwrap();
        let _a = s.message(&Message::user("old")).unwrap();
        let b = s.message(&Message::user("kept")).unwrap();
        s.compaction("SUMMARY", &b, 100, 10);
        s.message(&Message::user("after"));
        s.mode_change("plan", json!({"planFilePath":"local://"}));
        s.custom("plugin_context", json!({"plugin":"memory","context":"ctx"}));
        s.custom("plugin_context", json!({"plugin":"gone","context":"x"}));
        s.custom("plugin_context", json!({"plugin":"gone","context":""}));
        let l = load(&s.path).unwrap();
        let texts: Vec<String> = l
            .messages
            .iter()
            .map(|m| match m {
                Message::User { content, .. } => match &content[0] {
                    mu_base::message::Block::Text { text } => text.clone(),
                    _ => unreachable!(),
                },
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(texts, ["kept", "after"]);
        assert_eq!(l.entry_ids.len(), 2);
        assert_eq!(l.entry_ids[0], b);
        assert_eq!(l.compaction.unwrap().summary, "SUMMARY");
        assert_eq!(l.mode.unwrap().0, "plan");
        assert_eq!(l.plugin_context.len(), 1);
        assert_eq!(l.plugin_context["memory"], "ctx");
    }

    #[test]
    fn a_rewind_survives_a_reload_and_keeps_the_old_branch_in_the_file() {
        home();
        let mut s = SessionFile::create(Path::new("/opt/rewind"), None).unwrap();
        s.model_change("a/x");
        let one = s.message(&Message::user("one")).unwrap();
        let two = s.message(&Message::user("two")).unwrap();
        s.message(&Message::user("three"));
        let path = s.path.clone();
        let texts = |p: &Path| -> Vec<String> {
            load(p)
                .unwrap()
                .messages
                .iter()
                .map(|m| match m {
                    Message::User { content, .. } => mu_base::message::text_of(content),
                    _ => String::new(),
                })
                .collect()
        };

        assert!(s.rewind_before("nope").is_err());
        s.rewind_before(&two).unwrap();
        assert_eq!(texts(&path), ["one"], "the rewind alone changes what loads");
        drop(s);
        let mut r = SessionFile::open_append(&path).unwrap();
        r.message(&Message::user("two again"));
        assert_eq!(texts(&path), ["one", "two again"]);
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("\"three\""), "the abandoned branch is still in the file");

        // Before the first prompt: no messages are left, but the settings written before it are.
        r.rewind_before(&one).unwrap();
        let l = load(&path).unwrap();
        assert!(l.messages.is_empty());
        assert_eq!(l.model.as_deref(), Some("a/x"));
        r.message(&Message::user("fresh"));
        assert_eq!(texts(&path), ["fresh"]);
    }

    #[test]
    fn a_rewind_after_keeps_the_entry_and_drops_what_followed() {
        home();
        let mut s = SessionFile::create(Path::new("/opt/rewind-after"), None).unwrap();
        s.message(&Message::user("one"));
        let two = s.message(&Message::user("two")).unwrap();
        s.message(&Message::user("three"));
        assert!(s.rewind_after("nope").is_err());
        s.rewind_after(&two).unwrap();
        assert_eq!(load(&s.path).unwrap().messages.len(), 2);
        s.message(&Message::user("four"));
        assert_eq!(load(&s.path).unwrap().messages.len(), 3);
    }

    #[test]
    fn review_base_reloads() {
        home();
        let mut s = SessionFile::create(Path::new("/opt/rb"), None).unwrap();
        s.message(&Message::user("hi"));
        s.custom("review_base", json!({"tree":"a","since":1}));
        s.custom("review_base", json!({"tree":"b","planUri":"local://x-plan.md","since":2}));
        let l = load(&s.path).unwrap();
        assert_eq!(l.review_base.unwrap(), json!({"tree":"b","planUri":"local://x-plan.md","since":2}));
        assert!(l.plugin_context.is_empty());
    }

    #[test]
    fn unparseable_tool_result_is_repaired_on_load() {
        home();
        let mut s = SessionFile::create(Path::new("/opt/repair"), None).unwrap();
        s.message(&Message::user("run it"));
        let call = Message::Assistant {
            content: vec![mu_base::message::Block::ToolCall {
                id: "t1".into(),
                name: "bash".into(),
                arguments: json!({}),
            }],
            api: "a".into(),
            provider: "p".into(),
            model: "m".into(),
            usage: Default::default(),
            stop_reason: mu_base::message::StopReason::ToolUse,
            timestamp: 0,
            response_id: None,
            error_message: None,
        };
        s.message(&call);
        // A tool result entry that no longer parses (missing required fields).
        s.append(EntryKind::Message, json!({"message":{"role":"toolResult","toolCallId":"t1"}}));
        let l = load(&s.path).unwrap();
        assert!(mu_base::message::check_tool_pairs(&l.messages).is_ok());
        assert_eq!(l.messages.len(), 3);
        assert_eq!(l.messages.len(), l.entry_ids.len());
        assert!(!l.warnings.is_empty());
    }

    #[test]
    fn a_stopped_session_loads_without_warnings() {
        home();
        let mut s = SessionFile::create(Path::new("/opt/stopped"), None).unwrap();
        s.message(&Message::user("run it"));
        let call =
            |id: &str| mu_base::message::Block::ToolCall { id: id.into(), name: "bash".into(), arguments: json!({}) };
        s.message(&Message::Assistant {
            content: vec![call("t1"), call("t2")],
            api: "a".into(),
            provider: "p".into(),
            model: "m".into(),
            usage: Default::default(),
            stop_reason: mu_base::message::StopReason::ToolUse,
            timestamp: 0,
            response_id: None,
            error_message: None,
        });
        for (id, text) in [("t1", "done"), ("t2", "Skipped: the run was stopped so the user can take over")] {
            s.message(&Message::ToolResult {
                tool_call_id: id.into(),
                tool_name: "bash".into(),
                content: vec![mu_base::message::Block::text(text)],
                is_error: id == "t2",
                timestamp: 0,
                details: None,
            });
        }
        let l = load(&s.path).unwrap();
        assert!(l.warnings.is_empty(), "{:?}", l.warnings);
        assert_eq!(l.messages.len(), 4);
    }

    #[test]
    fn torn_last_line_does_not_swallow_the_next_entry() {
        home();
        let mut s = SessionFile::create(Path::new("/opt/torn"), None).unwrap();
        s.message(&Message::user("one"));
        let path = s.path.clone();
        drop(s);
        let len = std::fs::metadata(&path).unwrap().len();
        let f = OpenOptions::new().write(true).open(&path).unwrap();
        f.set_len(len - 10).unwrap(); // cut the last entry mid-line
        drop(f);
        let mut r = SessionFile::open_append(&path).unwrap();
        r.message(&Message::user("two"));
        let l = load(&path).unwrap();
        assert_eq!(l.messages.len(), 1);
        assert!(matches!(&l.messages[0], Message::User { content, .. } if format!("{content:?}").contains("two")));
    }

    #[test]
    fn a_failed_reopen_returns_the_old_handle_still_writing() {
        home();
        let mut s = SessionFile::create(Path::new("/opt/reopen"), None).unwrap();
        s.message(&Message::user("one"));
        let path = s.path.clone();
        std::fs::remove_file(&path).unwrap();
        assert!(s.reopen().is_err());
        assert!(s.message(&Message::user("two")).is_some());
    }

    #[test]
    fn a_second_writer_is_refused() {
        home();
        let s = SessionFile::create(Path::new("/opt/lock"), None).unwrap();
        let e = SessionFile::open_append(&s.path).err().unwrap().to_string();
        assert!(e.contains("another mu process"), "{e}");
        assert!(e.contains(&format!("pid {}", std::process::id())), "{e}");
        let path = s.path.clone();
        drop(s);
        assert!(SessionFile::open_append(&path).is_ok());
    }

    /// Reopen-after-drop loop, optionally racing a thread that spawns children. Run with
    /// `MU_STRESS_SPAWN=1 MU_STRESS_N=3000 cargo test stress_reopen -- --ignored --nocapture`.
    #[test]
    #[ignore = "stress harness, see the comment above"]
    fn stress_reopen_vs_spawn() {
        use std::sync::atomic::{AtomicBool, Ordering};
        home();
        let n: u32 = std::env::var("MU_STRESS_N").ok().and_then(|v| v.parse().ok()).unwrap_or(2000);
        let spawn = std::env::var("MU_STRESS_SPAWN").is_ok();
        let s = SessionFile::create(Path::new("/opt/lock"), None).unwrap();
        let path = s.path.clone();
        drop(s);
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let spawner = spawn.then(|| {
            let stop = stop.clone();
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let _ = std::process::Command::new("true").status();
                }
            })
        });
        let mut fails = 0;
        for _ in 0..n {
            match SessionFile::open_append(&path) {
                Ok(f) => drop(f),
                Err(_) => fails += 1,
            }
        }
        stop.store(true, Ordering::Relaxed);
        if let Some(h) = spawner {
            h.join().unwrap();
        }
        eprintln!("STRESS spawn={spawn} n={n} fails={fails}");
        assert_eq!(fails, 0);
    }

    #[test]
    fn set_title_leaves_a_missing_slot_alone() {
        home();
        let d = tmp("noslot");
        let path = d.join("x_abc.jsonl");
        std::fs::write(&path, "{\"type\":\"session\",\"version\":3,\"id\":\"abc\"}\n").unwrap();
        let mut s = SessionFile::open_append(&path).unwrap();
        s.set_title("Hello", TitleSource::User);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("{\"type\":\"session\""), "header kept: {text}");
        assert_eq!(load(&path).unwrap().title.as_deref(), Some("Hello"));
    }

    #[test]
    fn write_failures_retry_then_give_up_visibly() {
        home();
        let mut s = SessionFile::create(Path::new("/opt/fail"), None).unwrap();
        let errs = std::rc::Rc::new(std::cell::RefCell::new(Vec::<String>::new()));
        let sink = errs.clone();
        s.on_error = Some(Box::new(move |m| sink.borrow_mut().push(m)));
        // A read-only descriptor makes every write fail.
        s.file = Some(File::open("/dev/null").unwrap());
        for _ in 0..3 {
            assert!(s.append(EntryKind::Custom, json!({})).is_none());
        }
        assert_eq!(errs.borrow().len(), 3);
        assert!(errs.borrow()[2].contains("no longer saved"));
        assert!(s.append(EntryKind::Custom, json!({})).is_none());
        assert_eq!(errs.borrow().len(), 3, "no more noise once given up");
    }
}
