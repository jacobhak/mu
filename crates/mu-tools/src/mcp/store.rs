//! Edits to omp's `mcp.json` files: add, remove and switch servers on or off. Everything else in
//! a file (other servers, other keys, key order) is kept. A file that does not parse is never
//! overwritten.

use super::config::{as_bool, candidate_files, is_pi_file};
use super::trust;
use super::{JsonError, read_json};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Level {
    /// `<agentDir>/mcp.json`
    User,
    /// `<cwd>/.omp/mcp.json`
    Project,
}

pub fn file_for(level: Level, cwd: &Path, agent: &Path) -> PathBuf {
    match level {
        Level::User => agent.join("mcp.json"),
        Level::Project => cwd.join(".omp/mcp.json"),
    }
}

/// Names become part of tool names (`mcp__<server>_<tool>`), so keep them plain.
pub fn check_name(name: &str) -> Result<(), String> {
    let ok =
        !name.is_empty() && name.len() <= 48 && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if ok {
        Ok(())
    } else {
        Err(format!("`{name}` is not a valid server name (letters, digits, `_` and `-`, up to 48)"))
    }
}

/// The root object of a config file; a missing file is an empty one.
fn read_root(path: &Path) -> Result<Map<String, Value>, String> {
    match read_json(path) {
        Ok(Value::Object(m)) => Ok(m),
        Ok(_) => Err(format!("{}: not a JSON object; left unchanged", path.display())),
        Err(JsonError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => Ok(Map::new()),
        Err(JsonError::Io(e)) => Err(format!("{}: {e}", path.display())),
        Err(JsonError::Blank(_)) => Ok(Map::new()),
        Err(JsonError::Parse(e)) => Err(format!("{}: {e}; left unchanged", path.display())),
    }
}

/// Writes through `write_atomic_with`: an existing file keeps its permissions, a new one is private
/// (headers and env may hold tokens).
fn write_root(path: &Path, root: &Map<String, Value>) -> Result<(), String> {
    let err = |e: std::io::Error| format!("{}: {e}", path.display());
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir).map_err(err)?;
    let mut text = serde_json::to_string_pretty(root).map_err(|e| e.to_string())?;
    text.push('\n');
    mu_common::fs::write_atomic_with(path, text.as_bytes(), 0o600).map_err(err)
}

fn list(root: &Map<String, Value>, key: &str) -> Vec<String> {
    match root.get(key) {
        Some(Value::Array(a)) => a.iter().filter_map(|v| v.as_str().map(String::from)).collect(),
        _ => Vec::new(),
    }
}

/// Removes `name` from the list under `key`; true when it was there.
fn unlist(root: &mut Map<String, Value>, key: &str, name: &str) -> bool {
    let Some(Value::Array(a)) = root.get_mut(key) else { return false };
    let before = a.len();
    a.retain(|v| v.as_str() != Some(name));
    a.len() != before
}

fn enlist(root: &mut Map<String, Value>, key: &str, name: &str) -> bool {
    if list(root, key).iter().any(|n| n == name) {
        return false;
    }
    match root.get_mut(key) {
        Some(Value::Array(a)) => a.push(Value::String(name.into())),
        _ => {
            root.insert(key.into(), Value::Array(vec![Value::String(name.into())]));
        }
    }
    true
}

/// The config files that define `name`, in precedence order.
pub fn defined_in(cwd: &Path, agent: &Path, name: &str) -> Result<Vec<PathBuf>, String> {
    let mut out = Vec::new();
    for f in candidate_files(cwd, agent) {
        let root = read_root(&f)?;
        if root.get("mcpServers").and_then(Value::as_object).is_some_and(|m| m.contains_key(name)) {
            out.push(f);
        }
    }
    Ok(out)
}

/// Adds `entry` as server `name` in the level's file and returns that file. A name that any
/// config file already defines is refused: the higher-precedence definition would win silently.
pub fn add(level: Level, cwd: &Path, agent: &Path, name: &str, entry: Value) -> Result<PathBuf, String> {
    check_name(name)?;
    if let Some(f) = defined_in(cwd, agent, name)?.first() {
        return Err(format!("`{name}` is already defined in {}; remove it first", f.display()));
    }
    let path = file_for(level, cwd, agent);
    let mut root = read_root(&path)?;
    // A project file the user trusted, or a new one, stays trusted with the new server in it.
    let was_trusted = match root.get("mcpServers") {
        Some(old) => trust::is_trusted(&path, &trust::digest(old)),
        None => true,
    };
    let servers = root.entry("mcpServers").or_insert_with(|| Value::Object(Map::new()));
    let Value::Object(servers) = servers else {
        return Err(format!("{}: `mcpServers` is not an object; left unchanged", path.display()));
    };
    servers.insert(name.into(), entry);
    write_root(&path, &root)?;
    if level == Level::Project
        && was_trusted
        && let Some(servers) = root.get("mcpServers")
    {
        trust::trust(&path, &trust::digest(servers)).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    // A stale switch from an earlier server of the same name would silence this one.
    clear_switches(agent, name)?;
    Ok(path)
}

/// Drops `name` from the user file's `disabledServers` and `enabledServers`.
fn clear_switches(agent: &Path, name: &str) -> Result<(), String> {
    let path = agent.join("mcp.json");
    let mut root = read_root(&path)?;
    let a = unlist(&mut root, "disabledServers", name);
    let b = unlist(&mut root, "enabledServers", name);
    if a || b { write_root(&path, &root) } else { Ok(()) }
}

/// Removes `name` from every file that defines it, and from the on/off lists. Returns the files
/// that held the definition.
pub fn remove(cwd: &Path, agent: &Path, name: &str) -> Result<Vec<PathBuf>, String> {
    let files = defined_in(cwd, agent, name)?;
    if files.is_empty() {
        return Err(format!("no MCP server `{name}` is configured"));
    }
    if let Some(f) = files.iter().find(|f| is_pi_file(cwd, agent, f)) {
        return Err(format!(
            "`{name}` is defined in {}, a pi file that mu does not edit; remove it there, or switch it off with /mcp disable",
            f.display()
        ));
    }
    for f in &files {
        let mut root = read_root(f)?;
        let was_trusted = root.get("mcpServers").is_some_and(|s| trust::is_trusted(f, &trust::digest(s)));
        if let Some(Value::Object(m)) = root.get_mut("mcpServers") {
            m.shift_remove(name);
        }
        write_root(f, &root)?;
        if was_trusted
            && trust::is_project_file(cwd, f)
            && let Some(s) = root.get("mcpServers")
        {
            trust::trust(f, &trust::digest(s)).map_err(|e| format!("{}: {e}", f.display()))?;
        }
    }
    clear_switches(agent, name)?;
    Ok(files)
}

/// Switches a server on or off the way omp does: through the user file's `disabledServers` and
/// `enabledServers`, whichever file defines the server. Returns whether anything changed.
pub fn set_enabled(cwd: &Path, agent: &Path, name: &str, on: bool) -> Result<bool, String> {
    let files = defined_in(cwd, agent, name)?;
    let Some(def) = files.first() else { return Err(format!("no MCP server `{name}` is configured")) };
    let path = agent.join("mcp.json");
    let mut root = read_root(&path)?;
    let changed = if on {
        let mut c = unlist(&mut root, "disabledServers", name);
        // An entry that says `enabled: false` itself needs the allow list to be switched on.
        let off_in_entry = read_root(def)?
            .get("mcpServers")
            .and_then(|m| m.get(name))
            .and_then(|e| e.get("enabled"))
            .and_then(as_bool)
            == Some(false);
        if off_in_entry {
            c |= enlist(&mut root, "enabledServers", name);
        }
        c
    } else {
        unlist(&mut root, "enabledServers", name) | enlist(&mut root, "disabledServers", name)
    };
    if changed {
        write_root(&path, &root)?;
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::config::load_all_with;
    use mu_testutil::tmp;
    use serde_json::json;

    fn put(p: &Path, text: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    fn state(cwd: &Path, agent: &Path) -> Vec<(String, bool)> {
        mu_testutil::home();
        let (all, _) = load_all_with(cwd, agent, &|_| None);
        all.into_iter().map(|e| (e.server.name, e.enabled)).collect()
    }

    fn trusted_state(cwd: &Path, agent: &Path) -> Vec<(String, bool)> {
        mu_testutil::home();
        let (all, _) = load_all_with(cwd, agent, &|_| None);
        all.into_iter().map(|e| (e.server.name, e.trusted)).collect()
    }

    #[test]
    fn adding_a_project_server_keeps_the_file_trusted_only_if_it_was() {
        mu_testutil::home();
        let d = tmp();
        let (cwd, agent) = (d.join("p"), d.join("a"));
        add(Level::Project, &cwd, &agent, "one", json!({"command": "x"})).unwrap();
        assert_eq!(trusted_state(&cwd, &agent), vec![("one".into(), true)], "a new file is trusted");
        add(Level::Project, &cwd, &agent, "two", json!({"url": "http://t"})).unwrap();
        assert_eq!(trusted_state(&cwd, &agent), vec![("one".into(), true), ("two".into(), true)]);
        // A file changed by hand is not trusted, so adding to it does not trust it either.
        put(&cwd.join(".omp/mcp.json"), r#"{"mcpServers":{"evil":{"command":"rm"}}}"#);
        add(Level::Project, &cwd, &agent, "three", json!({"command": "y"})).unwrap();
        assert_eq!(trusted_state(&cwd, &agent), vec![("evil".into(), false), ("three".into(), false)]);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn a_server_pi_defines_is_switched_off_but_never_edited_out() {
        mu_testutil::home();
        let d = tmp();
        let (cwd, agent) = (d.join("p"), d.join("a"));
        let pi = cwd.join(".pi/mcp.json");
        put(&pi, r#"{"mcpServers":{"theirs":{"command":"x"}}}"#);
        let before = std::fs::read_to_string(&pi).unwrap();
        let err = remove(&cwd, &agent, "theirs").unwrap_err();
        assert!(err.contains("pi file") && err.contains("/mcp disable"), "{err}");
        assert_eq!(std::fs::read_to_string(&pi).unwrap(), before);
        assert!(
            add(Level::User, &cwd, &agent, "theirs", json!({"command": "y"})).unwrap_err().contains("already defined")
        );
        assert!(set_enabled(&cwd, &agent, "theirs", false).unwrap());
        assert_eq!(std::fs::read_to_string(&pi).unwrap(), before);
        assert_eq!(state(&cwd, &agent), vec![("theirs".into(), false)]);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn names() {
        assert!(check_name("my-server_2").is_ok());
        for bad in ["", "a b", "a/b", "a.b", &"x".repeat(49)] {
            assert!(check_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn add_keeps_everything_else_and_refuses_duplicates() {
        mu_testutil::home();
        let d = tmp();
        let (cwd, agent) = (d.join("p"), d.join("a"));
        put(&agent.join("mcp.json"), r#"{"zeta":1,"mcpServers":{"old":{"command":"o"}},"disabledServers":["new"]}"#);
        let f = add(Level::User, &cwd, &agent, "new", json!({"command": "x", "args": ["1"]})).unwrap();
        assert_eq!(f, agent.join("mcp.json"));
        let text = std::fs::read_to_string(&f).unwrap();
        assert!(text.ends_with("}\n") && text.find("zeta").unwrap() < text.find("mcpServers").unwrap(), "{text}");
        // The stale deny entry for the new name is gone, so the server is on.
        assert_eq!(state(&cwd, &agent), vec![("old".into(), true), ("new".into(), true)]);
        assert!(
            add(Level::Project, &cwd, &agent, "old", json!({"command": "y"})).unwrap_err().contains("already defined")
        );
        let p = add(Level::Project, &cwd, &agent, "proj", json!({"url": "http://h/mcp"})).unwrap();
        assert_eq!(p, cwd.join(".omp/mcp.json"));
        assert_eq!(state(&cwd, &agent).len(), 3);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn a_broken_file_is_never_overwritten() {
        let d = tmp();
        let (cwd, agent) = (d.join("p"), d.join("a"));
        put(&agent.join("mcp.json"), "{ not json");
        assert!(add(Level::User, &cwd, &agent, "s", json!({"command": "x"})).is_err());
        assert_eq!(std::fs::read_to_string(agent.join("mcp.json")).unwrap(), "{ not json");
        put(&agent.join("mcp.json"), "[1]");
        assert!(remove(&cwd, &agent, "s").is_err());
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn remove_clears_definitions_and_switches() {
        let d = tmp();
        let (cwd, agent) = (d.join("p"), d.join("a"));
        put(&cwd.join(".omp/mcp.json"), r#"{"mcpServers":{"s":{"command":"x"},"keep":{"command":"k"}}}"#);
        put(
            &agent.join("mcp.json"),
            r#"{"mcpServers":{"s":{"command":"y"}},"disabledServers":["s","other"],"enabledServers":["s"]}"#,
        );
        assert_eq!(remove(&cwd, &agent, "s").unwrap().len(), 2);
        assert_eq!(state(&cwd, &agent), vec![("keep".into(), true)]);
        let user: Value = serde_json::from_str(&std::fs::read_to_string(agent.join("mcp.json")).unwrap()).unwrap();
        assert_eq!(user["disabledServers"], json!(["other"]));
        assert_eq!(user["enabledServers"], json!([]));
        assert!(remove(&cwd, &agent, "s").unwrap_err().contains("no MCP server"));
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn enable_and_disable_round_trip() {
        let d = tmp();
        let (cwd, agent) = (d.join("p"), d.join("a"));
        put(
            &cwd.join(".omp/mcp.json"),
            r#"{"mcpServers":{"on":{"command":"x"},"off":{"command":"y","enabled":false}}}"#,
        );
        assert_eq!(state(&cwd, &agent), vec![("on".into(), true), ("off".into(), false)]);
        assert!(set_enabled(&cwd, &agent, "on", false).unwrap());
        assert!(!set_enabled(&cwd, &agent, "on", false).unwrap(), "already off");
        assert!(set_enabled(&cwd, &agent, "off", true).unwrap());
        assert_eq!(state(&cwd, &agent), vec![("on".into(), false), ("off".into(), true)]);
        assert!(set_enabled(&cwd, &agent, "on", true).unwrap());
        assert!(set_enabled(&cwd, &agent, "off", false).unwrap());
        assert_eq!(state(&cwd, &agent), vec![("on".into(), true), ("off".into(), false)]);
        assert!(set_enabled(&cwd, &agent, "nope", true).is_err());
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn new_files_are_private_and_existing_modes_are_kept() {
        use std::os::unix::fs::PermissionsExt;
        let d = tmp();
        let (cwd, agent) = (d.join("p"), d.join("a"));
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        let f = add(Level::User, &cwd, &agent, "s", json!({"command": "x"})).unwrap();
        assert_eq!(mode(&f), 0o600);
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();
        add(Level::User, &cwd, &agent, "t", json!({"command": "y"})).unwrap();
        assert_eq!(mode(&f), 0o644);
        let _ = std::fs::remove_dir_all(d);
    }
}
