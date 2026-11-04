//! The on-disk tool list of an MCP server, keyed by a hash of its config, so tools can be advertised
//! without starting the server. Holds no secrets: only what `tools/list` and `initialize` returned.

use super::config::McpServer;
use mu_base::jsoncache::JsonCache;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// Mixed into every hash; bump it when the cached shape changes.
const VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteTool {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub input_schema: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Cached {
    pub version: u32,
    pub server: String,
    pub tools: Vec<RemoteTool>,
    #[serde(default)]
    pub instructions: Option<String>,
}

/// `v` with object keys sorted at every level (serde_json keeps insertion order here).
pub fn canonical(v: &Value) -> Value {
    match v {
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            Value::Object(keys.into_iter().map(|k| (k.clone(), canonical(&m[k]))).collect())
        }
        Value::Array(a) => Value::Array(a.iter().map(canonical).collect()),
        other => other.clone(),
    }
}

/// SHA-256 (hex) of the server name, the expanded entry and, for a stdio server, the project
/// directory (its relative command, args and default cwd mean different things per checkout).
/// `!cmd` / env-name values are hashed as written, not resolved. `${VAR}` is expanded before
/// hashing, so a header like `Bearer ${TOKEN}` does enter the digest; SHA-256 is one-way and the
/// hash only names a file, but an OAuth token fetched with `omp token` never does.
pub fn config_hash(s: &McpServer, project: &str) -> String {
    let dir = matches!(s.kind, super::config::Kind::Stdio { .. }).then_some(project);
    let doc = canonical(&json!({"v": VERSION, "name": s.name, "entry": s.entry, "dir": dir}));
    Sha256::digest(doc.to_string().as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

pub fn dir() -> PathBuf {
    mu_common::paths::mu_home().join("mcp").join("tools")
}

fn file(dir: &Path, hash: &str) -> JsonCache {
    JsonCache::new(dir.join(format!("{hash}.json")))
}

/// A missing, corrupt or old-version file reads as a miss.
pub fn load(dir: &Path, hash: &str) -> Option<Cached> {
    file(dir, hash).load(|s| serde_json::from_str::<Cached>(s).ok()).filter(|c| c.version == VERSION)
}

pub fn save(dir: &Path, hash: &str, server: &str, tools: &[RemoteTool], instructions: Option<&str>) -> bool {
    let c = Cached {
        version: VERSION,
        server: server.into(),
        tools: tools.to_vec(),
        instructions: instructions.map(String::from),
    };
    serde_json::to_string(&c).is_ok_and(|t| file(dir, hash).save(&t))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::mcp::config::load_with;

    fn server(entry: &str) -> McpServer {
        let d = std::env::temp_dir().join(format!("mu-mcpcache-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("mcp.json"), format!(r#"{{"mcpServers":{{"s":{entry}}}}}"#)).unwrap();
        let (mut s, _) = load_with(&d.join("p"), &d, &|_| None);
        let _ = std::fs::remove_dir_all(d);
        s.remove(0)
    }

    #[test]
    fn hash_is_stable_and_follows_the_config() {
        let a = server(r#"{"command":"x","args":["1"],"env":{"A":"1","B":"2"}}"#);
        let b = server(r#"{"env":{"B":"2","A":"1"},"args":["1"],"command":"x"}"#);
        let c = server(r#"{"command":"x","args":["2"]}"#);
        assert_eq!(config_hash(&a, "/p"), config_hash(&b, "/p"), "key order does not matter");
        assert_ne!(config_hash(&a, "/p"), config_hash(&c, "/p"));
        assert_ne!(config_hash(&a, "/p"), config_hash(&a, "/q"), "another checkout, another list");
        let h = server(r#"{"url":"http://x"}"#);
        assert_eq!(config_hash(&h, "/p"), config_hash(&h, "/q"), "an http server does not depend on the project");
    }

    #[test]
    fn miss_hit_corrupt_and_version() {
        let d = std::env::temp_dir().join(format!("mu-mcpcache-{}", uuid::Uuid::new_v4()));
        assert!(load(&d, "h").is_none());
        let t = vec![RemoteTool { name: "t".into(), description: "d".into(), input_schema: json!({"type":"object"}) }];
        assert!(save(&d, "h", "srv", &t, Some("hi")));
        let c = load(&d, "h").unwrap();
        assert_eq!(c.tools, t);
        assert_eq!(c.instructions.as_deref(), Some("hi"));
        std::fs::write(d.join("h.json"), "{not json").unwrap();
        assert!(load(&d, "h").is_none());
        std::fs::write(d.join("h.json"), r#"{"version":99,"server":"s","tools":[]}"#).unwrap();
        assert!(load(&d, "h").is_none());
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn the_cache_file_holds_no_config() {
        let d = std::env::temp_dir().join(format!("mu-mcpcache-{}", uuid::Uuid::new_v4()));
        let s = server(r#"{"command":"x","env":{"TOKEN":"sekrit"},"headers":{}}"#);
        let hash = config_hash(&s, "/p");
        save(&d, &hash, "s", &[], None);
        let text = std::fs::read_to_string(d.join(format!("{hash}.json"))).unwrap();
        assert!(!text.contains("sekrit"));
        let _ = std::fs::remove_dir_all(d);
    }
}
