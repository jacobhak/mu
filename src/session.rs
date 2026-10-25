use crate::message::Message;
use mu_common::paths;
use anyhow::{Context, Result, bail};
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
    failed: bool,
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
    pub todo: Option<Value>,
}

pub struct SessionInfo {
    pub path: PathBuf,
    pub title: Option<String>,
    pub first_prompt: String,
    pub modified: std::time::SystemTime,
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
        let mut header = json!({"type":"session","version":3,"id":id.to_string(),"timestamp":iso_ms(),"cwd":cwd.to_string_lossy()});
        if let Some(p) = parent_session {
            header["parentSession"] = json!(p.to_string_lossy());
        }
        file.write_all(format!("{}{header}\n", title_slot("", None)).as_bytes())?;
        Ok(Self { path, file: Some(file), leaf: None, ids: Default::default(), failed: false })
    }

    /// Opens an existing session for appending after `load`.
    pub fn open_append(path: &Path) -> Result<Self> {
        let (_, leaf, ids) = parse(path)?;
        let file = OpenOptions::new().append(true).open(path)?;
        Ok(Self { path: path.into(), file: Some(file), leaf, ids, failed: false })
    }

    pub fn id(&self) -> String {
        id_of(&self.path)
    }

    /// Appends an entry and returns its id.
    pub fn append(&mut self, kind: &str, payload: Value) -> String {
        let id = loop {
            let u = uuid::Uuid::new_v4().simple().to_string();
            let id = u[u.len() - 8..].to_string();
            if self.ids.insert(id.clone()) {
                break id;
            }
        };
        let Some(file) = self.file.as_mut() else { return id };
        let mut entry = json!({"type":kind,"id":id,"parentId":self.leaf,"timestamp":iso_ms()});
        if let (Some(e), Value::Object(p)) = (entry.as_object_mut(), payload) {
            e.extend(p);
        }
        if let Err(err) = file.write_all(format!("{entry}\n").as_bytes()) {
            if !self.failed {
                eprintln!("session write failed: {err}");
                self.failed = true;
            }
            self.file = None;
            return id;
        }
        self.leaf = Some(id.clone());
        id
    }

    pub fn model_change(&mut self, model: &str) {
        self.append("model_change", json!({"model": model}));
    }
    pub fn thinking_change(&mut self, level: &str) {
        self.append("thinking_level_change", json!({"thinkingLevel": level}));
    }
    pub fn message(&mut self, m: &Message) -> String {
        self.append("message", json!({"message": m}))
    }
    pub fn compaction(&mut self, summary: &str, first_kept_entry_id: &str, tokens_before: u64, tokens_after: u64) -> String {
        self.append(
            "compaction",
            json!({"summary":summary,"firstKeptEntryId":first_kept_entry_id,"tokensBefore":tokens_before,"tokensAfter":tokens_after}),
        )
    }
    pub fn mode_change(&mut self, mode: &str, data: Value) {
        self.append("mode_change", json!({"mode":mode,"data":data}));
    }
    pub fn custom(&mut self, custom_type: &str, data: Value) {
        self.append("custom", json!({"customType":custom_type,"data":data}));
    }

    /// Rewrites the fixed title slot in place and records a `title_change` entry.
    pub fn set_title(&mut self, title: &str, source: &str) {
        use std::os::unix::fs::FileExt;
        let slot = title_slot(title, Some(source));
        let wrote = OpenOptions::new().write(true).open(&self.path).and_then(|f| f.write_all_at(slot.as_bytes(), 0));
        if let Err(e) = wrote {
            eprintln!("title write failed: {e}");
            return;
        }
        self.append("title_change", json!({"title":title,"source":source}));
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

fn parse(path: &Path) -> Result<Parsed> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let mut lines = text.lines().filter(|l| !l.trim().is_empty()).peekable();
    let mut title = None;
    if let Some(first) = lines.peek() {
        if let Some(v) = serde_json::from_str::<Value>(first).ok().filter(|v| v["type"] == "title") {
            title = slot_title(&v);
            lines.next();
        }
    }
    let header: Option<Value> = lines.next().and_then(|l| serde_json::from_str(l).ok());
    if !header.is_some_and(|h| h["type"] == "session" && h["version"] == 3) {
        bail!("not a v3 session file: {}", path.display());
    }
    let mut by_id: HashMap<String, Value> = HashMap::new();
    let mut last = None;
    for l in lines {
        let Ok(v) = serde_json::from_str::<Value>(l) else { continue };
        let Some(id) = v["id"].as_str().map(String::from) else { continue };
        last = Some(id.clone());
        by_id.insert(id, v);
    }
    let mut chain = Vec::new();
    let mut cur = last.clone();
    while let Some(id) = cur {
        let Some(v) = by_id.get(&id) else { break };
        chain.push(v);
        cur = v["parentId"].as_str().map(String::from);
        if chain.len() > by_id.len() {
            break;
        }
    }
    chain.reverse();
    let mut loaded = Loaded { title, ..Default::default() };
    // Entries before the latest compaction's first kept entry are summarized away.
    let comp = chain.iter().rposition(|v| v["type"] == "compaction");
    let start = comp.map_or(0, |c| {
        let kept = chain[c]["firstKeptEntryId"].as_str();
        chain[..c].iter().position(|v| v["id"].as_str() == kept).unwrap_or(c)
    });
    if let Some(c) = comp {
        loaded.compaction = Some(CompactionState {
            summary: chain[c]["summary"].as_str().unwrap_or_default().into(),
        });
    }
    for (i, v) in chain.iter().enumerate() {
        let id = || v["id"].as_str().unwrap_or_default().to_string();
        match v["type"].as_str() {
            Some("message") => {
                let Ok(m) = serde_json::from_value::<Message>(v["message"].clone()) else { continue };
                if let Message::ToolResult { tool_name, is_error: false, details: Some(d), .. } = &m {
                    if tool_name == "todo" && d.get("phases").is_some() {
                        loaded.todo = Some(d["phases"].clone());
                    }
                }
                if i >= start {
                    loaded.entry_ids.push(id());
                    loaded.messages.push(m);
                }
            }
            Some("custom_message") if i >= start => {
                let text = match &v["content"] {
                    Value::String(s) => s.clone(),
                    Value::Array(a) => a.iter().filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n"),
                    _ => continue,
                };
                loaded.entry_ids.push(id());
                loaded.messages.push(Message::user(&format!("<system-reminder>\n{text}\n</system-reminder>")));
            }
            Some("model_change") => loaded.model = v["model"].as_str().map(String::from),
            Some("thinking_level_change") => loaded.thinking = v["thinkingLevel"].as_str().map(String::from),
            Some("title_change") => loaded.title = v["title"].as_str().filter(|t| !t.is_empty()).map(String::from),
            Some("mode_change") => {
                loaded.mode = Some((v["mode"].as_str().unwrap_or("none").into(), v["data"].clone()));
            }
            Some("custom") if v["customType"] == "plugin_context" => {
                if let (Some(p), Some(c)) = (v["data"]["plugin"].as_str(), v["data"]["context"].as_str()) {
                    loaded.plugin_context.insert(p.into(), c.into());
                }
            }
            _ => {}
        }
    }
    loaded.plugin_context.retain(|_, c| !c.is_empty());
    Ok((loaded, last, by_id.into_keys().collect()))
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

/// Session file for `arg`: an existing path, else a session id (or unique id prefix),
/// searched in `cwd`'s sessions first and then in every other project's.
pub fn find(cwd: &Path, arg: &Path) -> Option<PathBuf> {
    if arg.exists() {
        return Some(arg.to_path_buf());
    }
    let id = arg.to_str()?;
    let own = paths::session_dir(cwd);
    let root = own.parent()?;
    let mut dirs = vec![own.clone()];
    dirs.extend(std::fs::read_dir(root).ok()?.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| *p != own && p.is_dir()));
    dirs.iter().find_map(|d| {
        std::fs::read_dir(d).ok()?.filter_map(|e| e.ok().map(|e| e.path())).find(|p| {
            let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
            p.extension().is_some_and(|x| x == "jsonl") && stem.rsplit_once('_').is_some_and(|(_, sid)| sid.starts_with(id))
        })
    })
}

/// Sessions for `cwd`, newest first. Reads only the title slot, header and first user message.
pub fn list_for(cwd: &Path) -> Vec<SessionInfo> {
    let Ok(rd) = std::fs::read_dir(paths::session_dir(cwd)) else { return vec![] };
    let mut out: Vec<SessionInfo> = rd
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
        .filter_map(|e| {
            let path = e.path();
            let modified = e.metadata().ok()?.modified().ok()?;
            let f = std::io::BufReader::new(File::open(&path).ok()?);
            let mut title = None;
            let mut first_prompt = String::new();
            for (i, l) in std::io::BufRead::lines(f).map_while(Result::ok).enumerate() {
                let Ok(v) = serde_json::from_str::<Value>(&l) else { continue };
                match v["type"].as_str() {
                    Some("title") if i == 0 => title = slot_title(&v),
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
            Some(SessionInfo { path, title, first_prompt, modified })
        })
        .collect();
    out.sort_by(|a, b| b.path.file_name().cmp(&a.path.file_name()));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> &'static Path {
        static H: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
        H.get_or_init(|| {
            let d = std::env::temp_dir().join(format!("mu-test-home-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&d).unwrap();
            unsafe { std::env::set_var("MU_HOME", &d) };
            d
        })
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
        let mut r = SessionFile::open_append(&s.path).unwrap();
        r.message(&Message::user("third"));
        assert_eq!(load(&s.path).unwrap().messages.len(), 3);
        assert_eq!(latest_for(Path::new("/opt/proj")), Some(s.path.clone()));
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
        s.set_title(&"é".repeat(400), "auto");
        let after = std::fs::read(&s.path).unwrap();
        assert_eq!(after[255], b'\n');
        assert_eq!(&before[256..], &after[256..before.len()]);
        let slot: Value = serde_json::from_slice(&after[..255]).unwrap();
        let t = slot["title"].as_str().unwrap();
        assert!(t.starts_with('é') && t.chars().count() < 400);
        assert_eq!(load(&s.path).unwrap().title.as_deref(), Some("é".repeat(400).as_str()));
        let info = list_for(Path::new("/opt/titles"));
        assert_eq!(info[0].first_prompt, "hello there");
        assert_eq!(info[0].title.as_deref(), Some(t));
    }

    #[test]
    fn compaction_keeps_tail_and_extracts_state() {
        home();
        let mut s = SessionFile::create(Path::new("/opt/comp"), None).unwrap();
        let _a = s.message(&Message::user("old"));
        let b = s.message(&Message::user("kept"));
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
}
