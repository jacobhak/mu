//! The session file format: entry kinds, the fixed title slot, and `load`, which reads a session
//! back along its active chain.
use anyhow::{Context, Result, bail};
use mu_base::message::Message;
use mu_common::time::iso_ms;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::path::Path;

pub(super) const TITLE_SLOT_BYTES: usize = 256;

/// The `type` of a session entry. Serialized to the same strings omp uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EntryKind {
    Message,
    Compaction,
    ModelChange,
    ThinkingLevelChange,
    TitleChange,
    ModeChange,
    Custom,
}

impl EntryKind {
    pub(crate) fn as_str(self) -> &'static str {
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
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Auto => "auto",
        }
    }
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
pub(super) fn title_slot(title: &str, source: Option<&str>) -> String {
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

/// What `parse` returns: the loaded session, its last entry's id, and every entry id with its parent.
pub(super) type Parsed = (Loaded, Option<String>, HashMap<String, Option<String>>);

pub(super) fn slot_title(first: &Value) -> Option<String> {
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

/// The first pass over a file: an index of its entries, and the active chain's end.
struct Scan {
    title: Option<String>,
    index: Vec<Index>,
    by_id: HashMap<String, usize>,
    /// The last entry written: the leaf of the active chain.
    last: Option<usize>,
    /// 1-based line numbers, with offsets, of lines that did not parse or carry no id.
    damaged: Vec<(usize, u64)>,
}

impl Scan {
    /// Positions in `index` of the active chain, root first.
    fn chain(&self) -> Vec<usize> {
        let mut chain: Vec<usize> = Vec::new();
        let mut cur = self.last;
        while let Some(i) = cur {
            chain.push(i);
            cur = self.index[i].parent.as_deref().and_then(|p| self.by_id.get(p).copied());
            if chain.len() > self.index.len() {
                break;
            }
        }
        chain.reverse();
        chain
    }

    /// A warning when the walk from the leaf ended at a parent that is not indexed: the entries
    /// before it are cut off. Names the last damaged line before the chain's first entry.
    fn cut_warning(&self, chain: &[usize]) -> Option<String> {
        let root = &self.index[*chain.first()?];
        let parent = root.parent.as_deref()?;
        let unreachable = self.index.iter().filter(|e| e.offset < root.offset).count();
        let cause = match self.damaged.iter().rev().find(|(_, off)| *off < root.offset) {
            Some((line, _)) => format!("line {line} is damaged"),
            None => format!("parent {parent} of entry {} is missing", root.id),
        };
        Some(format!("{unreachable} earlier entries are unreachable ({cause})"))
    }
}

/// The active chain as `(id, parent id, line offset)`, root first: what a fork copies.
pub(super) fn chain_lines(path: &Path) -> Result<Vec<(String, Option<String>, u64)>> {
    let s = scan(path)?;
    Ok(s.chain()
        .into_iter()
        .map(|i| {
            let e = &s.index[i];
            (e.id.clone(), e.parent.clone(), e.offset)
        })
        .collect())
}

/// Reads line by line and keeps only an index.
fn scan(path: &Path) -> Result<Scan> {
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
    let mut damaged: Vec<(usize, u64)> = Vec::new();
    let mut line_no = 0usize;
    loop {
        buf.clear();
        let start = offset;
        let n = reader.read_until(b'\n', &mut buf).with_context(|| format!("read {}", path.display()))?;
        if n == 0 {
            break;
        }
        offset += n as u64;
        line_no += 1;
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
            // A title slot torn by a crash: full size, not JSON. The file goes on without a title.
            if !first_seen && v.is_none() && n == TITLE_SLOT_BYTES {
                first_seen = true;
                continue;
            }
            if !v.is_some_and(|h| h["type"] == "session" && h["version"] == 3) {
                bail!("not a v3 session file: {}", path.display());
            }
            header_seen = true;
            continue;
        }
        let Some(p) = serde_json::from_slice::<Probe>(&buf).ok().filter(|p| p.id.is_some()) else {
            damaged.push((line_no, start));
            continue;
        };
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
    Ok(Scan { title, index, by_id, last, damaged })
}

/// Two passes over the file. The first (`scan`) keeps only an index; the second deserializes just
/// the entries that matter: everything from the latest compaction's first kept entry onwards,
/// plus the small state entries (model, title, mode, plugin context, latest todo). Peak memory
/// follows the active chain, not the file.
pub(super) fn parse(path: &Path) -> Result<Parsed> {
    use std::io::BufReader;
    let scan = scan(path)?;
    let chain = scan.chain();
    let cut = scan.cut_warning(&chain);
    let Scan { title, index, .. } = scan;
    let mut rd = BufReader::new(File::open(path)?);
    let mut loaded = Loaded { title, ..Default::default() };
    loaded.warnings.extend(cut);
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
                // pi records its system prompt and tool list as a `system` message; mu builds its own.
                if v["message"]["role"] == "system" {
                    continue;
                }
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
            "model_change" => {
                // omp writes `model: "provider/id"`; pi writes `provider` and `modelId`.
                loaded.model = match (v["provider"].as_str(), v["modelId"].as_str()) {
                    (Some(p), Some(m)) => Some(format!("{p}/{m}")),
                    _ => v["model"].as_str().map(String::from),
                };
            }
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
    let notes = mu_base::message::repair_tool_pairs_with_ids(&mut loaded.messages, Some(&mut loaded.entry_ids));
    loaded.warnings.extend(notes);
    let parents = index.iter().map(|e| (e.id.clone(), e.parent.clone())).collect();
    Ok((loaded, index.last().map(|e| e.id.clone()), parents))
}

pub fn load(path: &Path) -> Result<Loaded> {
    Ok(parse(path)?.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mu-test-{name}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_torn_title_slot_is_skipped() {
        let d = tmp("torn");
        let p = d.join("s.jsonl");
        let header = json!({"type":"session","version":3,"id":"a","timestamp":"t","cwd":"/"});
        let msg = json!({"type":"message","id":"1","parentId":null,"message":json!({"role":"user","content":"hi","timestamp":1})});
        let garbage = format!("{}\n", "#".repeat(TITLE_SLOT_BYTES - 1));
        assert_eq!(garbage.len(), TITLE_SLOT_BYTES);
        std::fs::write(&p, format!("{garbage}{header}\n{msg}\n")).unwrap();
        let l = load(&p).unwrap();
        assert_eq!((l.title, l.messages.len()), (None, 1));
    }

    #[test]
    fn a_damaged_middle_line_is_reported_with_its_line_and_the_entries_it_cuts_off() {
        let d = tmp("damaged");
        let p = d.join("s.jsonl");
        let header = json!({"type":"session","version":3,"id":"a","timestamp":"t","cwd":"/"});
        let msg = |id: &str, parent: Option<&str>| json!({"type":"message","id":id,"parentId":parent,"message":{"role":"user","content":id,"timestamp":1}});
        let (m1, m2, m4) = (msg("1", None), msg("2", Some("1")), msg("4", Some("3")));
        std::fs::write(&p, format!("{header}\n{m1}\n{m2}\n{{\"type\":\"message\",\"id\":\"3\",\n{m4}\n")).unwrap();
        let l = load(&p).unwrap();
        assert_eq!(l.messages.len(), 1);
        assert_eq!(l.warnings, vec!["2 earlier entries are unreachable (line 4 is damaged)".to_string()]);
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
                    mu_base::message::Block::Text { text } => text.clone(),
                    _ => unreachable!(),
                },
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(texts, ["root", "leaf"]);
    }
}
