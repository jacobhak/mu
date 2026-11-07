//! Persisted approval grants: per-project files under the profile home, and
//! `~/.mu/config.toml` for "always everywhere".

use mu_base::permissions::{Grant, RuleSet};
use mu_common::fs::{self, FileLock};
use mu_common::paths;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use toml_edit::{DocumentMut, Item, Table, Value};

/// Where one project's grants live. Keyed by the main checkout, so worktrees share them.
pub fn project_file(key: &Path) -> PathBuf {
    paths::profile_home().join("permissions").join(format!("{}.toml", paths::encode_cwd(key)))
}

/// The project's `[rules]`; a missing file is an empty set.
pub fn load_project(key: &Path, warn: &mut Vec<String>) -> RuleSet {
    let path = project_file(key);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == ErrorKind::NotFound => return RuleSet::default(),
        Err(e) => {
            warn.push(format!("{}: {e}; ignored", path.display()));
            return RuleSet::default();
        }
    };
    let table: toml::Table = match text.parse() {
        Ok(t) => t,
        Err(e) => {
            warn.push(format!("{}: {e}; ignored", path.display()));
            return RuleSet::default();
        }
    };
    match table.get("rules") {
        Some(toml::Value::Table(rules)) => RuleSet::parse(rules, "project permissions", warn),
        None => RuleSet::default(),
        Some(_) => {
            warn.push(format!("{}: rules is not a table; ignored", path.display()));
            RuleSet::default()
        }
    }
}

pub fn add_project(key: &Path, g: &Grant) -> Result<(), String> {
    write_grant(&project_file(key), &["rules"], Some(key), g)
}

pub fn add_global(g: &Grant) -> Result<(), String> {
    write_grant(&paths::mu_home().join("config.toml"), &["permissions", "rules"], None, g)
}

/// Adds `allow` rules for `g` under `section`, keeping everything else in the file.
fn write_grant(path: &Path, section: &[&str], root: Option<&Path>, g: &Grant) -> Result<(), String> {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let lock_path = path.with_file_name(format!("{name}.lock"));
    let _lock = FileLock::exclusive(&lock_path).map_err(|e| format!("{}: {e}", lock_path.display()))?;
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let mut doc: DocumentMut = text.parse().map_err(|e| format!("{}: {e}; fix it by hand", path.display()))?;
    if let Some(key) = root {
        doc.as_table_mut().insert("root", Item::Value(Value::from(key.to_string_lossy().into_owned())));
    }
    let mut table = doc.as_table_mut();
    for (i, key) in section.iter().enumerate() {
        let item = table.entry(key).or_insert_with(|| Item::Table(implicit()));
        table = item.as_table_mut().ok_or_else(|| {
            format!("{}: [{}] is not a table; add the rule by hand", path.display(), section[..=i].join("."))
        })?;
    }
    match table.get_mut(&g.tool) {
        // An action already decides every call of this tool.
        Some(Item::Value(Value::String(_))) => {}
        Some(Item::Value(Value::InlineTable(t))) => {
            for p in &g.patterns {
                t.entry(p).or_insert(Value::from("allow"));
            }
        }
        Some(Item::Table(t)) => {
            for p in &g.patterns {
                t.entry(p).or_insert(Item::Value(Value::from("allow")));
            }
        }
        Some(_) => {
            return Err(format!(
                "{}: [{}] {} is not an action or a table; add the rule by hand",
                path.display(),
                section.join("."),
                g.tool
            ));
        }
        None => {
            let item = if g.patterns.is_empty() {
                Item::Value(Value::from("allow"))
            } else {
                let mut t = toml_edit::InlineTable::new();
                for p in &g.patterns {
                    t.insert(p, Value::from("allow"));
                }
                Item::Value(Value::InlineTable(t))
            };
            table.insert(&g.tool, item);
        }
    }
    fs::write_atomic(path, doc.to_string().as_bytes()).map_err(|e| format!("{}: {e}", path.display()))
}

fn implicit() -> Table {
    let mut t = Table::new();
    t.set_implicit(true);
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grants_round_trip_and_keep_what_is_there() {
        mu_testutil::home();
        let config = paths::mu_home().join("config.toml");
        std::fs::create_dir_all(paths::mu_home()).unwrap();
        std::fs::write(&config, "# keep me\n[theme]\nname = \"x\"\n").unwrap();

        let g = Grant { tool: "probebash".into(), patterns: vec!["git push *".into()] };
        add_global(&g).unwrap();
        add_global(&g).unwrap();
        let text = std::fs::read_to_string(&config).unwrap();
        assert!(text.contains("# keep me"), "{text}");
        assert_eq!(text.matches("git push *").count(), 1, "not duplicated: {text}");
        let table: toml::Table = text.parse().unwrap();
        let rules = table["permissions"]["rules"]["probebash"].as_table().unwrap();
        assert_eq!(rules["git push *"].as_str(), Some("allow"));

        // A tool-wide grant is the string form.
        add_global(&Grant { tool: "probefetch".into(), patterns: vec![] }).unwrap();
        let text = std::fs::read_to_string(&config).unwrap();
        let table: toml::Table = text.parse().unwrap();
        assert_eq!(table["permissions"]["rules"]["probefetch"].as_str(), Some("allow"));

        // A hand-written rule is left alone.
        add_global(&Grant { tool: "probebash".into(), patterns: vec!["git push *".into()] }).unwrap();
        let text = std::fs::read_to_string(&config).unwrap();
        assert_eq!(text.matches("git push *").count(), 1);

        let key = std::path::Path::new("/tmp/project-grants-test");
        add_project(key, &Grant { tool: "write".into(), patterns: vec!["a/*.txt".into()] }).unwrap();
        add_project(key, &Grant { tool: "write".into(), patterns: vec!["a/*.txt".into()] }).unwrap();
        let mut warn = Vec::new();
        let rules = load_project(key, &mut warn);
        assert!(warn.is_empty(), "{warn:?}");
        assert_eq!(rules.0.len(), 1);
        assert_eq!(rules.0[0].0, "write");
        assert_eq!(rules.0[0].1.pattern, "a/*.txt");
        let text = std::fs::read_to_string(project_file(key)).unwrap();
        assert!(text.contains("root = "), "{text}");
        assert_eq!(text.matches("a/*.txt").count(), 1);
    }
}
