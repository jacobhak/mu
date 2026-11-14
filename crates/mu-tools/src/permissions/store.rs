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
    // `/` maps to `-` in the file name, so `a-b` and `a/b` share a file: only the project it names applies.
    if let Some(stored) = table.get("root").and_then(toml::Value::as_str)
        && Path::new(stored) != key
    {
        warn.push(format!("{}: grants for {stored}, not {}; ignored", path.display(), key.display()));
        return RuleSet::default();
    }
    match table.get("rules") {
        Some(toml::Value::Table(rules)) => RuleSet::parse(rules, "project permissions", warn),
        None => RuleSet::default(),
        Some(_) => {
            warn.push(format!("{}: rules is not a table; ignored", path.display()));
            RuleSet::default()
        }
    }
}

/// What a grant did to the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrantOutcome {
    Written,
    /// Nothing was written: the file already decides every rule of the grant, as described.
    AlreadyCovered(String),
}

pub fn add_project(key: &Path, g: &Grant) -> Result<GrantOutcome, String> {
    write_grant(&project_file(key), &["rules"], Some(key), g)
}

pub fn add_global(g: &Grant) -> Result<GrantOutcome, String> {
    write_grant(&paths::mu_home().join("config.toml"), &["permissions", "rules"], None, g)
}

/// Adds `allow` rules for `g` under `section`, keeping everything else in the file.
fn write_grant(path: &Path, section: &[&str], root: Option<&Path>, g: &Grant) -> Result<GrantOutcome, String> {
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
        if let Some(stored) = doc.get("root").and_then(Item::as_str)
            && Path::new(stored) != key
        {
            return Err(format!("{}: holds grants for {stored}, not {}; not saved", path.display(), key.display()));
        }
        doc.as_table_mut().insert("root", Item::Value(Value::from(key.to_string_lossy().into_owned())));
    }
    let mut table = doc.as_table_mut();
    for (i, key) in section.iter().enumerate() {
        let item = table.entry(key).or_insert_with(|| Item::Table(implicit()));
        table = item.as_table_mut().ok_or_else(|| {
            format!("{}: [{}] is not a table; add the rule by hand", path.display(), section[..=i].join("."))
        })?;
    }
    let patterns = super::grant::grant_patterns(&g.patterns);
    let mut covered = Vec::new();
    let mut written = false;
    for tool in super::grant::grant_tools(&g.tool) {
        written |= add_rule(table, section, tool, &patterns, path, &mut covered)?;
    }
    if !written {
        covered.dedup();
        let why = if covered.is_empty() { "the grant names no patterns".to_string() } else { covered.join("; ") };
        return Ok(GrantOutcome::AlreadyCovered(why));
    }
    fs::write_atomic(path, doc.to_string().as_bytes()).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(GrantOutcome::Written)
}

fn describe(item: &Item) -> String {
    item.as_str().map(str::to_string).unwrap_or_else(|| "a rule".into())
}

/// Adds `allow` for every pattern under `tool`, keeping what the file already says there; true when
/// something was added, else `covered` gets what already decides the calls. No patterns grant
/// nothing; the one pattern `*` is every call of the tool, written as `tool = "allow"`.
fn add_rule(
    table: &mut Table,
    section: &[&str],
    tool: &str,
    patterns: &[String],
    path: &Path,
    covered: &mut Vec<String>,
) -> Result<bool, String> {
    let mut wrote = false;
    if patterns.is_empty() {
        return Ok(false);
    }
    match table.get_mut(tool) {
        Some(Item::Value(Value::String(action))) => {
            covered.push(format!("{tool} is already \"{}\" for every call", action.value()));
        }
        Some(Item::Value(Value::InlineTable(t))) => {
            for p in patterns {
                match t.get(p) {
                    Some(v) => {
                        covered.push(format!("{tool} \"{p}\" is already \"{}\"", describe(&Item::Value(v.clone()))))
                    }
                    None => {
                        t.insert(p, Value::from("allow"));
                        wrote = true;
                    }
                }
            }
        }
        Some(Item::Table(t)) => {
            for p in patterns {
                match t.get(p) {
                    Some(v) => covered.push(format!("{tool} \"{p}\" is already \"{}\"", describe(v))),
                    None => {
                        t.insert(p, Item::Value(Value::from("allow")));
                        wrote = true;
                    }
                }
            }
        }
        Some(_) => {
            return Err(format!(
                "{}: [{}] {tool} is not an action or a table; add the rule by hand",
                path.display(),
                section.join(".")
            ));
        }
        None => {
            let item = if patterns == ["*"] {
                Item::Value(Value::from("allow"))
            } else {
                let mut t = toml_edit::InlineTable::new();
                for p in patterns {
                    t.insert(p, Value::from("allow"));
                }
                Item::Value(Value::InlineTable(t))
            };
            table.insert(tool, item);
            wrote = true;
        }
    }
    Ok(wrote)
}

fn implicit() -> Table {
    let mut t = Table::new();
    t.set_implicit(true);
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grant(tool: &str, patterns: &[&str]) -> Grant {
        Grant { tool: tool.into(), patterns: patterns.iter().map(|p| p.to_string()).collect(), wider: Vec::new() }
    }

    #[test]
    fn grants_round_trip_and_keep_what_is_there() {
        mu_testutil::home();
        let config = paths::mu_home().join("config.toml");
        std::fs::create_dir_all(paths::mu_home()).unwrap();
        std::fs::write(&config, "# keep me\n[theme]\nname = \"x\"\n").unwrap();

        let g = grant("probebash", &["git push *"]);
        add_global(&g).unwrap();
        add_global(&g).unwrap();
        let text = std::fs::read_to_string(&config).unwrap();
        assert!(text.contains("# keep me"), "{text}");
        assert_eq!(text.matches("git push *").count(), 1, "not duplicated: {text}");
        let table: toml::Table = text.parse().unwrap();
        let rules = table["permissions"]["rules"]["probebash"].as_table().unwrap();
        assert_eq!(rules["git push *"].as_str(), Some("allow"));

        // A tool-wide grant is the string form.
        add_global(&grant("probefetch", &["*"])).unwrap();
        let text = std::fs::read_to_string(&config).unwrap();
        let table: toml::Table = text.parse().unwrap();
        assert_eq!(table["permissions"]["rules"]["probefetch"].as_str(), Some("allow"));

        // A grant of nothing leaves the file as it is.
        add_global(&grant("probenothing", &[])).unwrap();
        assert_eq!(std::fs::read_to_string(&config).unwrap(), text);

        // A hand-written rule is left alone.
        add_global(&grant("probebash", &["git push *"])).unwrap();
        let text = std::fs::read_to_string(&config).unwrap();
        assert_eq!(text.matches("git push *").count(), 1);

        let key = std::path::Path::new("/tmp/project-grants-test");
        add_project(key, &grant("write", &["a/*.txt"])).unwrap();
        add_project(key, &grant("write", &["a/*.txt"])).unwrap();
        let mut warn = Vec::new();
        let rules = load_project(key, &mut warn);
        assert!(warn.is_empty(), "{warn:?}");
        assert_eq!(rules.0.len(), 2, "the change family, and no directory to add: {rules:?}");
        assert_eq!(rules.0[0].0, "edit");
        assert_eq!(rules.0[0].1.pattern, "a/*.txt");
        assert_eq!(rules.0[1].0, "write");
        let text = std::fs::read_to_string(project_file(key)).unwrap();
        assert!(text.contains("root = "), "{text}");
        assert_eq!(text.matches("a/*.txt").count(), 2, "once per tool, not once per add: {text}");
    }

    #[test]
    fn grants_apply_only_to_the_project_they_were_stored_for() {
        mu_testutil::home();
        let (a, b) = (Path::new("/tmp/mu-grants/a-b"), Path::new("/tmp/mu-grants/a/b"));
        assert_eq!(project_file(a), project_file(b), "the directory names collide");
        add_project(a, &grant("write", &["x/*"])).unwrap();
        let mut warn = Vec::new();
        assert_eq!(load_project(a, &mut warn).0.len(), 4);
        assert!(warn.is_empty(), "{warn:?}");
        assert!(load_project(b, &mut warn).0.is_empty(), "another project's grants do not apply");
        assert!(warn.iter().any(|w| w.contains("/tmp/mu-grants/a-b")), "{warn:?}");
        let err = add_project(b, &grant("write", &["y/*"])).unwrap_err();
        assert!(err.contains("not saved"), "{err}");
        assert!(!std::fs::read_to_string(project_file(a)).unwrap().contains("y/*"));
    }

    #[test]
    fn a_grant_the_file_already_decides_says_so_and_writes_nothing() {
        mu_testutil::home();
        let key = std::path::Path::new("/tmp/project-grants-covered-test");
        let file = project_file(key);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        let text = "[rules]\nprobeask = \"ask\"\nprobebash = { \"git push *\" = \"ask\" }\n";
        std::fs::write(&file, text).unwrap();

        let whole = add_project(key, &grant("probeask", &["x *"])).unwrap();
        assert!(
            matches!(&whole, GrantOutcome::AlreadyCovered(w) if w.contains("probeask") && w.contains("ask")),
            "{whole:?}"
        );
        let same = add_project(key, &grant("probebash", &["git push *"])).unwrap();
        assert!(matches!(&same, GrantOutcome::AlreadyCovered(w) if w.contains("git push *")), "{same:?}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), text);

        assert_eq!(add_project(key, &grant("probebash", &["git pull *"])).unwrap(), GrantOutcome::Written);
        assert!(std::fs::read_to_string(&file).unwrap().contains("git pull *"));
    }

    #[test]
    fn a_grant_is_saved_for_every_tool_of_its_family() {
        mu_testutil::home();
        let key = std::path::Path::new("/tmp/project-grants-family-test");
        add_project(key, &grant("read", &["/etc/*"])).unwrap();
        let mut warn = Vec::new();
        let rules = load_project(key, &mut warn);
        assert!(warn.is_empty(), "{warn:?}");
        // A parsed table stores its tools alphabetically.
        let saved: Vec<(&str, &str)> = rules.0.iter().map(|(t, r)| (t.as_str(), r.pattern.as_str())).collect();
        assert_eq!(
            saved,
            [
                ("glob", "/etc"),
                ("glob", "/etc/*"),
                ("grep", "/etc"),
                ("grep", "/etc/*"),
                ("read", "/etc"),
                ("read", "/etc/*"),
            ]
        );

        // Changes are their own family, kept apart from reads.
        let key = std::path::Path::new("/tmp/project-grants-change-test");
        add_project(key, &grant("write", &["src/*"])).unwrap();
        let rules = load_project(key, &mut warn);
        assert!(warn.is_empty(), "{warn:?}");
        let saved: Vec<(&str, &str)> = rules.0.iter().map(|(t, r)| (t.as_str(), r.pattern.as_str())).collect();
        assert_eq!(saved, [("edit", "src"), ("edit", "src/*"), ("write", "src"), ("write", "src/*")]);
    }
}
