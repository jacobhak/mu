use crate::message::Message;
use anyhow::{Context, Result, bail};
use mu_common::paths;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

const TITLE_SLOT_BYTES: usize = 256;

pub struct SessionFile {
    pub path: PathBuf,
    file: Option<File>,
    leaf: Option<String>,
    ids: std::collections::HashSet<String>,
    /// Consecutive failed writes; persistence is given up at `MAX_WRITE_FAILURES`.
    failures: u32,
    /// The last write may have left a partial line, so the next one starts on a fresh line.
    torn: bool,
    /// Receives write errors, so the user sees them.
    pub on_error: Option<Box<dyn Fn(String)>>,
}

const MAX_WRITE_FAILURES: u32 = 3;

/// The `type` of a session entry. Serialized to the same strings omp uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    Message,
    Compaction,
    ModelChange,
    ThinkingLevelChange,
    TitleChange,
    ModeChange,
    Custom,
}

impl EntryKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Message => "message",
            Self::Compaction => "compaction",
            Self::ModelChange => "model_change",
            Self::ThinkingLevelChange => "thinking_level_change",
            Self::TitleChange => "title_change",
            Self::ModeChange => "mode_change",
            Self::Custom => "custom",
        }
    }
}

/// Who chose a session's title.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TitleSource {
    User,
    Auto,
}

impl TitleSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Auto => "auto",
        }
    }
}

/// Takes an exclusive, non-blocking `flock` that lives as long as `file`.
fn lock(file: &File, path: &Path) -> Result<()> {
    use std::os::fd::AsRawFd;
    // SAFETY: the fd is valid for the duration of the call.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let e = std::io::Error::last_os_error();
        if e.kind() == std::io::ErrorKind::WouldBlock {
            bail!("session is open in another mu process: {}", path.display());
        }
        return Err(e).with_context(|| format!("lock {}", path.display()));
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub struct CompactionState {
    pub summary: String,
}

#[derive(Default)]
pub struct Loaded {
    pub messages: Vec<Message>,
    /// Parallel to `messages`.
    pub entry_ids: Vec<String>,
    pub model: Option<String>,
    pub thinking: Option<String>,
    pub compaction: Option<CompactionState>,
    pub title: Option<String>,
    pub mode: Option<(String, Value)>,
    pub plugin_context: BTreeMap<String, String>,
    pub review_base: Option<Value>,
    pub todo: Option<Value>,
    /// Problems found (and repaired) while loading.
    pub warnings: Vec<String>,
}

pub struct SessionInfo {
    pub path: PathBuf,
    pub title: Option<String>,
    pub first_prompt: String,
    pub modified: std::time::SystemTime,
    /// Working directory recorded in the session header.
    pub cwd: Option<PathBuf>,
}

pub fn iso_ms() -> String {
    jiff::Timestamp::now().strftime("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

fn slot_line(title: &str, source: Option<&str>, updated_at: &str, pad: &str) -> String {
    let mut v = json!({"type":"title","v":1,"title":title});
    if let Some(src) = source {
        v["source"] = json!(src);
    }
    v["updatedAt"] = json!(updated_at);
    v["pad"] = json!(pad);
    format!("{v}\n")
}

/// Fixed 256-byte first line, newline included; mirrors omp's `serializeTitleSlot`.
fn title_slot(title: &str, source: Option<&str>) -> String {
    let updated_at = iso_ms();
    let cps: Vec<char> = title.chars().collect();
    let (mut lo, mut hi) = (0, cps.len());
    let mut best = String::new();
    while lo <= hi {
        let mid = (lo + hi) / 2;
        let cand: String = cps[..mid].iter().collect();
        if slot_line(&cand, source, &updated_at, "").len() <= TITLE_SLOT_BYTES {
            best = cand;
            lo = mid + 1;
        } else {
            if mid == 0 {
                break;
            }
            hi = mid - 1;
        }
    }
    let pad = TITLE_SLOT_BYTES - slot_line(&best, source, &updated_at, "").len();
    slot_line(&best, source, &updated_at, &" ".repeat(pad))
}

impl SessionFile {
    /// Creates the file with its title slot and header.
    pub fn create(cwd: &Path, parent_session: Option<&Path>) -> Result<Self> {
        let dir = paths::session_dir(cwd);
        std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        let id = uuid::Uuid::now_v7();
        let ts = jiff::Timestamp::now().strftime("%Y-%m-%dT%H-%M-%S-%3fZ").to_string();
        let path = dir.join(format!("{ts}_{id}.jsonl"));
        let mut file = OpenOptions::new().create_new(true).append(true).open(&path)?;
        lock(&file, &path)?;
        let mut header =
            json!({"type":"session","version":3,"id":id.to_string(),"timestamp":iso_ms(),"cwd":cwd.to_string_lossy()});
        if let Some(p) = parent_session {
            header["parentSession"] = json!(p.to_string_lossy());
        }
        file.write_all(format!("{}{header}\n", title_slot("", None)).as_bytes())?;
        Ok(Self {
            path,
            file: Some(file),
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
        lock(&file, path)?;
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
        Ok(Self { path: path.into(), file: Some(file), leaf, ids, failures: 0, torn, on_error: None })
    }

    pub fn id(&self) -> String {
        id_of(&self.path)
    }

    fn report(&self, msg: String) {
        match &self.on_error {
            Some(f) => f(msg),
            None => eprintln!("{msg}"),
        }
    }

    /// Appends an entry and returns its id, or `None` when nothing was written.
    pub fn append(&mut self, kind: EntryKind, payload: Value) -> Option<String> {
        self.file.as_ref()?;
        let id = loop {
            let u = uuid::Uuid::new_v4().simple().to_string();
            let id = u[u.len() - 8..].to_string();
            if !self.ids.contains(&id) {
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
        self.ids.insert(id.clone());
        self.leaf = Some(id.clone());
        Some(id)
    }

    /// Flushes the file to disk. Called after entries that must survive a crash, and at turn end.
    pub fn sync(&self) {
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
        use std::os::unix::fs::FileExt;
        let slot = title_slot(title, Some(source.as_str()));
        let wrote = OpenOptions::new().read(true).write(true).open(&self.path).and_then(|f| {
            let mut head = [0u8; TITLE_SLOT_BYTES];
            let is_slot = f.read_exact_at(&mut head, 0).is_ok()
                && head[TITLE_SLOT_BYTES - 1] == b'\n'
                && !head[..TITLE_SLOT_BYTES - 1].contains(&b'\n')
                && serde_json::from_slice::<Value>(&head).is_ok_and(|v| v["type"] == "title");
            if is_slot { f.write_all_at(slot.as_bytes(), 0) } else { Ok(()) }
        });
        if let Err(e) = wrote {
            self.report(format!("title write failed: {e}"));
        }
        self.append(EntryKind::TitleChange, json!({"title":title,"source":source.as_str()}));
        self.sync();
    }
}

type Parsed = (Loaded, Option<String>, std::collections::HashSet<String>);

/// Session id from a file named `<timestamp>_<id>.jsonl`.
pub fn id_of(path: &Path) -> String {
    let name = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    name.rsplit_once('_').map_or(name.clone(), |(_, id)| id.to_string())
}

fn slot_title(first: &Value) -> Option<String> {
    first["title"].as_str().filter(|t| !t.is_empty()).map(String::from)
}

/// What the first pass keeps per entry: enough to walk the chain and find what to read back.
struct Index {
    id: String,
    parent: Option<String>,
    offset: u64,
    kind: String,
    /// A successful `todo` tool result (it carries the todo list).
    todo: bool,
    /// A `plugin_context` custom entry.
    plugin_ctx: bool,
    /// A `review_base` custom entry.
    review_base: bool,
}

#[derive(serde::Deserialize)]
struct Probe {
    id: Option<String>,
    #[serde(rename = "parentId")]
    parent_id: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
    #[serde(rename = "customType")]
    custom_type: Option<String>,
    message: Option<ProbeMessage>,
}

#[derive(serde::Deserialize)]
struct ProbeMessage {
    #[serde(rename = "toolName")]
    tool_name: Option<String>,
    #[serde(rename = "isError")]
    is_error: Option<bool>,
}

fn read_at(r: &mut std::io::BufReader<File>, offset: u64) -> Option<Value> {
    use std::io::{BufRead, Seek, SeekFrom};
    r.seek(SeekFrom::Start(offset)).ok()?;
    let mut buf = Vec::new();
    r.read_until(b'\n', &mut buf).ok()?;
    serde_json::from_slice(&buf).ok()
}

/// Two passes over the file. The first reads line by line and keeps only an index; the second
/// deserializes just the entries that matter: everything from the latest compaction's first kept
/// entry onwards, plus the small state entries (model, title, mode, plugin context, latest todo).
/// Peak memory follows the active chain, not the file.
fn parse(path: &Path) -> Result<Parsed> {
    use std::io::{BufRead, BufReader};
    let file = File::open(path).with_context(|| format!("read {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut offset = 0u64;
    let mut buf = Vec::new();
    let mut title = None;
    let mut header_seen = false;
    let mut first_seen = false;
    let mut index: Vec<Index> = Vec::new();
    let mut by_id: HashMap<String, usize> = HashMap::new();
    let mut last: Option<usize> = None;
    loop {
        buf.clear();
        let start = offset;
        let n = reader.read_until(b'\n', &mut buf).with_context(|| format!("read {}", path.display()))?;
        if n == 0 {
            break;
        }
        offset += n as u64;
        if buf.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        if !header_seen {
            let v: Option<Value> = serde_json::from_slice(&buf).ok();
            if !first_seen && v.as_ref().is_some_and(|v| v["type"] == "title") {
                first_seen = true;
                title = v.as_ref().and_then(slot_title);
                continue;
            }
            if !v.is_some_and(|h| h["type"] == "session" && h["version"] == 3) {
                bail!("not a v3 session file: {}", path.display());
            }
            header_seen = true;
            continue;
        }
        let Ok(p) = serde_json::from_slice::<Probe>(&buf) else { continue };
        let Some(id) = p.id else { continue };
        let kind = p.kind.unwrap_or_default();
        let todo = kind == "message"
            && p.message.is_some_and(|m| m.tool_name.as_deref() == Some("todo") && m.is_error == Some(false));
        let plugin_ctx = kind == "custom" && p.custom_type.as_deref() == Some("plugin_context");
        let review_base = kind == "custom" && p.custom_type.as_deref() == Some("review_base");
        by_id.insert(id.clone(), index.len());
        last = Some(index.len());
        index.push(Index { id, parent: p.parent_id, offset: start, kind, todo, plugin_ctx, review_base });
    }
    if !header_seen {
        bail!("not a v3 session file: {}", path.display());
    }
    // Walk from the leaf to the root.
    let mut chain: Vec<usize> = Vec::new();
    let mut cur = last;
    while let Some(i) = cur {
        chain.push(i);
        cur = index[i].parent.as_deref().and_then(|p| by_id.get(p).copied());
        if chain.len() > index.len() {
            break;
        }
    }
    chain.reverse();

    let mut rd = BufReader::new(File::open(path)?);
    let mut loaded = Loaded { title, ..Default::default() };
    // Entries before the latest compaction's first kept entry are summarized away.
    let comp = chain.iter().rposition(|&i| index[i].kind == "compaction");
    let mut start = 0;
    if let Some(c) = comp {
        let v = read_at(&mut rd, index[chain[c]].offset).unwrap_or(Value::Null);
        let kept = v["firstKeptEntryId"].as_str();
        start = chain[..c].iter().position(|&i| Some(index[i].id.as_str()) == kept).unwrap_or(c);
        loaded.compaction = Some(CompactionState { summary: v["summary"].as_str().unwrap_or_default().into() });
    }
    let last_todo = chain.iter().rposition(|&i| index[i].todo);
    for (pos, &i) in chain.iter().enumerate() {
        let e = &index[i];
        let in_window = pos >= start;
        let needed = match e.kind.as_str() {
            "message" => in_window || last_todo == Some(pos),
            "custom_message" => in_window,
            "model_change" | "thinking_level_change" | "title_change" | "mode_change" => true,
            "custom" => e.plugin_ctx || e.review_base,
            _ => false,
        };
        if !needed {
            continue;
        }
        let Some(v) = read_at(&mut rd, e.offset) else { continue };
        let id = || e.id.clone();
        match e.kind.as_str() {
            "message" => {
                let Ok(m) = serde_json::from_value::<Message>(v["message"].clone()) else {
                    loaded.warnings.push(format!("skipped unparseable message entry {}", e.id));
                    continue;
                };
                if last_todo == Some(pos)
                    && let Message::ToolResult { details: Some(d), .. } = &m
                    && d.get("phases").is_some()
                {
                    loaded.todo = Some(d["phases"].clone());
                }
                if in_window {
                    loaded.entry_ids.push(id());
                    loaded.messages.push(m);
                }
            }
            "custom_message" => {
                let text = match &v["content"] {
                    Value::String(s) => s.clone(),
                    Value::Array(a) => a.iter().filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n"),
                    _ => continue,
                };
                loaded.entry_ids.push(id());
                loaded.messages.push(Message::user(&format!("<system-reminder>\n{text}\n</system-reminder>")));
            }
            "model_change" => loaded.model = v["model"].as_str().map(String::from),
            "thinking_level_change" => loaded.thinking = v["thinkingLevel"].as_str().map(String::from),
            "title_change" => loaded.title = v["title"].as_str().filter(|t| !t.is_empty()).map(String::from),
            "mode_change" => {
                loaded.mode = Some((v["mode"].as_str().unwrap_or("none").into(), v["data"].clone()));
            }
            "custom" => {
                if e.review_base {
                    loaded.review_base = Some(v["data"].clone());
                } else if let (Some(p), Some(c)) = (v["data"]["plugin"].as_str(), v["data"]["context"].as_str()) {
                    loaded.plugin_context.insert(p.into(), c.into());
                }
            }
            _ => {}
        }
    }
    loaded.plugin_context.retain(|_, c| !c.is_empty());
    let notes = crate::message::repair_tool_pairs_with_ids(&mut loaded.messages, Some(&mut loaded.entry_ids));
    loaded.warnings.extend(notes);
    Ok((loaded, index.last().map(|e| e.id.clone()), by_id.into_keys().collect()))
}

pub fn load(path: &Path) -> Result<Loaded> {
    Ok(parse(path)?.0)
}

pub fn latest_for(cwd: &Path) -> Option<PathBuf> {
    let dir = paths::session_dir(cwd);
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .max_by(|a, b| a.file_name().cmp(&b.file_name()))
}

/// A session id is `[A-Za-z0-9-]+`: nothing that could name a path.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// Session file for `arg`: an existing `.jsonl` file, else a session id (or id prefix).
pub fn find(cwd: &Path, arg: &Path) -> Result<PathBuf, String> {
    let Some(s) = arg.to_str().filter(|s| !s.trim().is_empty()) else {
        return Err("empty session argument".into());
    };
    if arg.is_file() && arg.extension().is_some_and(|x| x == "jsonl") {
        return Ok(arg.to_path_buf());
    }
    find_id(cwd, s)
}

/// Session file for an id or unique id prefix, searched in `cwd`'s sessions first and then in
/// every other project's. Never treats `id` as a path.
pub fn find_id(cwd: &Path, id: &str) -> Result<PathBuf, String> {
    if !valid_id(id) {
        return Err(format!("invalid session id `{id}`"));
    }
    let own = paths::session_dir(cwd);
    let root = own.parent().ok_or("no session directory")?;
    let matches_in = |d: &Path| -> Vec<PathBuf> {
        let Ok(rd) = std::fs::read_dir(d) else { return vec![] };
        let mut v: Vec<PathBuf> = rd
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
                p.extension().is_some_and(|x| x == "jsonl")
                    && stem.rsplit_once('_').is_some_and(|(_, sid)| sid.starts_with(id))
            })
            .collect();
        v.sort();
        v
    };
    let mut found = matches_in(&own);
    if found.is_empty() {
        let others = std::fs::read_dir(root).map_err(|e| e.to_string())?;
        for d in others.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| *p != own && p.is_dir()) {
            found.extend(matches_in(&d));
        }
    }
    match found.len() {
        0 => Err(format!("session {id} not found")),
        1 => Ok(found.remove(0)),
        _ => {
            let list: Vec<String> = found.iter().map(|p| id_of(p)).collect();
            Err(format!("session id `{id}` is ambiguous; matches: {}", list.join(", ")))
        }
    }
}

fn jsonl_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(dir) else { return vec![] };
    rd.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.extension().is_some_and(|x| x == "jsonl")).collect()
}

/// Title, working directory and first prompt of one session file; reads only its first lines.
fn read_info(path: PathBuf) -> Option<SessionInfo> {
    let modified = std::fs::metadata(&path).ok()?.modified().ok()?;
    let f = std::io::BufReader::new(File::open(&path).ok()?);
    let mut title = None;
    let mut cwd = None;
    let mut first_prompt = String::new();
    for (i, l) in std::io::BufRead::lines(f).map_while(Result::ok).enumerate() {
        let Ok(v) = serde_json::from_str::<Value>(&l) else { continue };
        match v["type"].as_str() {
            Some("title") if i == 0 => title = slot_title(&v),
            Some("session") => cwd = v["cwd"].as_str().map(PathBuf::from),
            Some("title_change") => title = v["title"].as_str().filter(|t| !t.is_empty()).map(String::from),
            Some("message") if first_prompt.is_empty() && v["message"]["role"] == "user" => {
                if let Ok(Message::User { content, .. }) = serde_json::from_value(v["message"].clone()) {
                    first_prompt = content
                        .iter()
                        .find_map(|b| match b {
                            crate::message::Block::Text { text } => Some(text.replace('\n', " ")),
                            _ => None,
                        })
                        .unwrap_or_default()
                        .chars()
                        .take(80)
                        .collect();
                }
            }
            _ => {}
        }
        if title.is_some() && !first_prompt.is_empty() {
            break;
        }
    }
    Some(SessionInfo { path, title, first_prompt, modified, cwd })
}

/// The newest `limit` sessions (of `only_cwd`'s project, or of every project), newest first.
/// File names start with a timestamp, so they are sorted without opening anything; only the
/// files that make the cut are read.
pub fn list_recent(cwd: &Path, only_cwd: Option<&Path>, limit: usize) -> Vec<SessionInfo> {
    let own = paths::session_dir(cwd);
    let dirs: Vec<PathBuf> = match only_cwd {
        Some(c) => vec![paths::session_dir(c)],
        None => match own.parent().and_then(|r| std::fs::read_dir(r).ok()) {
            Some(rd) => rd.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.is_dir()).collect(),
            None => return vec![],
        },
    };
    let mut files: Vec<PathBuf> = dirs.iter().flat_map(|d| jsonl_files(d)).collect();
    files.sort_by(|a, b| b.file_name().cmp(&a.file_name()));
    files
        .into_iter()
        .filter_map(read_info)
        .filter(|s| only_cwd.is_none_or(|w| s.cwd.as_deref() == Some(w)))
        .take(limit)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> &'static Path {
        crate::testutil::home()
    }

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
    fn branch_and_title() {
        let d = tmp("br");
        let p = d.join("s.jsonl");
        let u = |t: &str| json!({"role":"user","content":t,"timestamp":1});
        let lines = [
            json!({"type":"title","title":"x"}),
            json!({"type":"session","version":3,"id":"a","timestamp":"t","cwd":"/"}),
            json!({"type":"message","id":"1","parentId":null,"message":u("root")}),
            json!({"type":"message","id":"2","parentId":"1","message":u("abandoned")}),
            json!({"type":"message","id":"3","parentId":"1","message":u("leaf")}),
        ];
        let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
        std::fs::write(&p, body).unwrap();
        let l = load(&p).unwrap();
        let texts: Vec<_> = l
            .messages
            .iter()
            .map(|m| match m {
                Message::User { content, .. } => match &content[0] {
                    crate::message::Block::Text { text } => text.clone(),
                    _ => unreachable!(),
                },
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(texts, ["root", "leaf"]);
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
                    crate::message::Block::Text { text } => text.clone(),
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
            content: vec![crate::message::Block::ToolCall {
                id: "t1".into(),
                name: "bash".into(),
                arguments: json!({}),
            }],
            api: "a".into(),
            provider: "p".into(),
            model: "m".into(),
            usage: Default::default(),
            stop_reason: crate::message::StopReason::ToolUse,
            timestamp: 0,
            response_id: None,
            error_message: None,
        };
        s.message(&call);
        // A tool result entry that no longer parses (missing required fields).
        s.append(EntryKind::Message, json!({"message":{"role":"toolResult","toolCallId":"t1"}}));
        let l = load(&s.path).unwrap();
        assert!(crate::message::check_tool_pairs(&l.messages).is_ok());
        assert_eq!(l.messages.len(), 3);
        assert_eq!(l.messages.len(), l.entry_ids.len());
        assert!(!l.warnings.is_empty());
    }

    #[test]
    fn id_validation_and_lookup() {
        assert!(valid_id("0198-abc"));
        for bad in ["", ".", "..", "a/b", "a.b", "a_b", "a b"] {
            assert!(!valid_id(bad), "{bad:?}");
        }
        home();
        let cwd = Path::new("/opt/find");
        let a = SessionFile::create(cwd, None).unwrap();
        let id = id_of(&a.path);
        assert_eq!(find(cwd, Path::new(&id)).unwrap(), a.path);
        assert!(find(cwd, Path::new("")).is_err());
        assert!(find(cwd, Path::new("..")).is_err());
        assert!(find(cwd, Path::new(&a.path.parent().unwrap().to_path_buf())).is_err(), "a directory is not a session");
        assert_eq!(find(cwd, &a.path).unwrap(), a.path);
        // Two sessions sharing a prefix are ambiguous.
        let b = SessionFile::create(cwd, None).unwrap();
        let (ia, ib) = (id_of(&a.path), id_of(&b.path));
        let common = ia.chars().zip(ib.chars()).take_while(|(x, y)| x == y).count();
        if common > 0 {
            let e = find_id(cwd, &ia[..common]).unwrap_err();
            assert!(e.contains("ambiguous"), "{e}");
        }
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
    fn a_second_writer_is_refused() {
        home();
        let s = SessionFile::create(Path::new("/opt/lock"), None).unwrap();
        let e = SessionFile::open_append(&s.path).err().unwrap().to_string();
        assert!(e.contains("another mu process"), "{e}");
        let path = s.path.clone();
        drop(s);
        assert!(SessionFile::open_append(&path).is_ok());
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

    #[test]
    fn list_recent_reads_only_what_it_returns() {
        home();
        let cwd = Path::new("/opt/recent");
        let mut made = Vec::new();
        for _ in 0..6 {
            let mut s = SessionFile::create(cwd, None).unwrap();
            s.message(&Message::user("hello"));
            made.push(s.path.clone());
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let got = list_recent(cwd, Some(cwd), 3);
        assert_eq!(got.len(), 3);
        // Newest first, by file name.
        assert_eq!(got[0].path, made[5]);
        assert_eq!(got[2].path, made[3]);
        assert_eq!(got[0].cwd.as_deref(), Some(cwd));
        assert!(list_recent(cwd, Some(Path::new("/opt/other")), 3).is_empty());
        assert!(list_recent(cwd, None, 100).len() >= 6);
    }
}
