//! Trust for the MCP servers a project defines in its own files. Those servers do not start, and
//! their `${VAR}`s are not expanded, until the user trusts the file's current server set. The store
//! maps each trusted file (canonical path) to the digest of its raw `mcpServers`, so a changed
//! server set is untrusted again. Files in the agent dir are never gated.

use super::cache;
use super::{JsonError, read_json};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// The project files, relative to the cwd, in the order `candidate_files` lists them.
const PROJECT_FILES: [&str; 4] = [".omp/mcp.json", ".omp/.mcp.json", "mcp.json", ".mcp.json"];

fn store() -> PathBuf {
    mu_common::paths::profile_home().join("mcp/trusted.json")
}

/// How the store names a file: its canonical path, or the path as given when it does not exist.
fn key(file: &Path) -> String {
    std::fs::canonicalize(file).unwrap_or_else(|_| file.to_path_buf()).to_string_lossy().into_owned()
}

/// Whether `file` is one of the cwd's own config files (not the agent dir's).
pub fn is_project_file(cwd: &Path, file: &Path) -> bool {
    PROJECT_FILES.iter().any(|f| cwd.join(f) == file)
}

/// SHA-256 (hex) of a file's raw `mcpServers`, unexpanded, with keys sorted.
pub fn digest(raw_servers: &Value) -> String {
    Sha256::digest(cache::canonical(raw_servers).to_string().as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

/// A missing or unreadable store trusts nothing.
fn read_store() -> Map<String, Value> {
    match read_json(&store()) {
        Ok(Value::Object(m)) => m,
        Ok(_) | Err(JsonError::Io(_) | JsonError::Blank(_) | JsonError::Parse(_)) => Map::new(),
    }
}

pub fn is_trusted(file: &Path, digest: &str) -> bool {
    read_store().get(&key(file)).and_then(Value::as_str) == Some(digest)
}

pub fn trust(file: &Path, digest: &str) -> std::io::Result<()> {
    update(|m| {
        m.insert(key(file), Value::String(digest.into()));
    })
}

pub fn untrust(file: &Path) {
    let _ = update(|m| {
        m.remove(&key(file));
    });
}

/// Read-modify-write of the store under its lock.
fn update(edit: impl FnOnce(&mut Map<String, Value>)) -> std::io::Result<()> {
    let path = store();
    let mut lock = path.clone().into_os_string();
    lock.push(".lock");
    let _guard = mu_common::fs::FileLock::exclusive(Path::new(&lock))?;
    let mut m = read_store();
    edit(&mut m);
    let mut text = serde_json::to_string_pretty(&Value::Object(m)).map_err(std::io::Error::other)?;
    text.push('\n');
    mu_common::fs::write_atomic(&path, text.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mu_testutil::tmp;
    use serde_json::json;

    #[test]
    fn round_trip() {
        mu_testutil::home();
        let d = tmp();
        let file = d.join(".omp/mcp.json");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "{}").unwrap();
        let v1 = digest(&json!({"a": {"command": "x"}}));
        let v2 = digest(&json!({"a": {"command": "y"}}));
        assert_ne!(v1, v2);
        assert_eq!(v1, digest(&json!({"a": {"command": "x"}})), "stable");
        assert!(!is_trusted(&file, &v1), "nothing trusted yet");
        trust(&file, &v1).unwrap();
        assert!(is_trusted(&file, &v1));
        assert!(!is_trusted(&file, &v2), "a changed server set is not trusted");
        let other = d.join("other.json");
        assert!(!is_trusted(&other, &v1), "trust is per file");
        trust(&other, &v2).unwrap();
        assert!(is_trusted(&file, &v1) && is_trusted(&other, &v2), "trusting one keeps the other");
        untrust(&file);
        assert!(!is_trusted(&file, &v1));
        assert!(is_trusted(&other, &v2));
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn project_files_are_the_cwd_candidates() {
        let (cwd, agent) = (Path::new("/p"), Path::new("/a"));
        for f in ["/p/.omp/mcp.json", "/p/.omp/.mcp.json", "/p/mcp.json", "/p/.mcp.json"] {
            assert!(is_project_file(cwd, Path::new(f)), "{f}");
        }
        assert!(!is_project_file(cwd, &agent.join("mcp.json")));
        assert!(!is_project_file(cwd, Path::new("/p/sub/mcp.json")));
    }
}
