use crate::message::Message;
use crate::paths;
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

pub struct SessionFile {
    pub path: PathBuf,
    file: Option<File>,
    leaf: Option<String>,
    ids: std::collections::HashSet<String>,
    failed: bool,
}

pub struct Loaded {
    pub messages: Vec<Message>,
    pub model: Option<String>,
    pub thinking: Option<String>,
}

pub fn iso_ms() -> String {
    jiff::Timestamp::now().strftime("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

impl SessionFile {
    /// Creates file and header.
    pub fn create(cwd: &Path) -> Result<Self> {
        let dir = paths::session_dir(cwd);
        std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        let id = uuid::Uuid::now_v7();
        let ts = jiff::Timestamp::now().strftime("%Y-%m-%dT%H-%M-%S-%3fZ").to_string();
        let path = dir.join(format!("{ts}_{id}.jsonl"));
        let mut file = OpenOptions::new().create_new(true).append(true).open(&path)?;
        let header = json!({"type":"session","version":3,"id":id.to_string(),"timestamp":iso_ms(),"cwd":cwd.to_string_lossy()});
        file.write_all(format!("{header}\n").as_bytes())?;
        Ok(Self { path, file: Some(file), leaf: None, ids: Default::default(), failed: false })
    }

    /// Opens an existing session for appending after `load`.
    pub fn open_append(path: &Path) -> Result<Self> {
        let (_, leaf, ids) = parse(path)?;
        let file = OpenOptions::new().append(true).open(path)?;
        Ok(Self { path: path.into(), file: Some(file), leaf, ids, failed: false })
    }

    pub fn append(&mut self, kind: &str, payload: Value) {
        let Some(file) = self.file.as_mut() else { return };
        let id = loop {
            let u = uuid::Uuid::new_v4().simple().to_string();
            let id = u[u.len() - 8..].to_string();
            if self.ids.insert(id.clone()) {
                break id;
            }
        };
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
            return;
        }
        self.leaf = Some(id);
    }

    pub fn model_change(&mut self, model: &str) {
        self.append("model_change", json!({"model": model}));
    }
    pub fn thinking_change(&mut self, level: &str) {
        self.append("thinking_level_change", json!({"thinkingLevel": level}));
    }
    pub fn message(&mut self, m: &Message) {
        self.append("message", json!({"message": m}));
    }
}

type Parsed = (Loaded, Option<String>, std::collections::HashSet<String>);

fn parse(path: &Path) -> Result<Parsed> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let mut lines = text.lines().filter(|l| !l.trim().is_empty()).peekable();
    if let Some(first) = lines.peek() {
        if serde_json::from_str::<Value>(first).ok().is_some_and(|v| v["type"] == "title") {
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
    let mut loaded = Loaded { messages: vec![], model: None, thinking: None };
    for v in chain {
        match v["type"].as_str() {
            Some("message") => {
                if let Ok(m) = serde_json::from_value(v["message"].clone()) {
                    loaded.messages.push(m);
                }
            }
            Some("model_change") => loaded.model = v["model"].as_str().map(String::from),
            Some("thinking_level_change") => {
                loaded.thinking = v["thinkingLevel"].as_str().map(String::from)
            }
            _ => {}
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mu-test-{name}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn roundtrip_and_resume_append() {
        let d = tmp("rt");
        unsafe { std::env::set_var("MU_HOME", &d) };
        let mut s = SessionFile::create(Path::new("/opt/proj")).unwrap();
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
}
