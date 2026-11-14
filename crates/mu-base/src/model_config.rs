//! `models.toml`: model roles grouped in named sets, per-model limits and compaction thresholds.
//! Parsing and text edits are pure; `Configs` does the reading and writing.

use serde::{Deserialize, Serialize};
use serde_yaml_ng::{Mapping, Value as Yaml};
use std::collections::BTreeMap;
use toml_edit::{Array, DocumentMut, Item, Table, TableLike, value};

/// The set that roles go into when nothing else exists, and what omp's `modelRoles` imports as.
pub const MAIN_SET: &str = "main";

#[derive(Debug, Default, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct ModelConfig {
    pub set: Option<String>,
    pub cycle_order: Vec<String>,
    pub sets: BTreeMap<String, BTreeMap<String, String>>,
    /// What a custom role is for; `[roles.<name>]`.
    pub roles: BTreeMap<String, RoleMeta>,
    /// Keyed by the full `provider/model` id.
    pub models: BTreeMap<String, ModelLimits>,
    pub compaction: ModelCompaction,
}

/// What `[roles.<name>]` says about a custom role beyond its models.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RoleMeta {
    pub description: Option<String>,
    /// Roles tried in order when this one is unset; flat, not followed transitively.
    pub fallback: Vec<String>,
    /// The model may pick this role for a `task` subagent.
    pub delegate: bool,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Deserialize)]
#[serde(default)]
pub struct ModelLimits {
    pub context_window: Option<u32>,
    pub max_tokens: Option<u32>,
}

#[derive(Debug, Default, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct ModelCompaction {
    pub thresholds: BTreeMap<String, toml::Value>,
}

/// What copying omp's model config would write, and a line per thing copied.
#[derive(Debug, Clone, PartialEq)]
pub struct Import {
    pub config: ModelConfig,
    pub lines: Vec<String>,
}

impl ModelConfig {
    /// The set roles are read from and written to; see the plan's rule: `set` if it exists, the
    /// only set, `main`, else the first by name.
    pub fn active(&self) -> Option<&str> {
        if let Some(s) = self.set.as_deref()
            && self.sets.contains_key(s)
        {
            return Some(s);
        }
        if self.sets.len() > 1 && self.sets.contains_key(MAIN_SET) {
            return Some(MAIN_SET);
        }
        self.sets.keys().next().map(String::as_str)
    }

    pub fn role(&self, name: &str) -> Option<&String> {
        self.sets.get(self.active()?)?.get(name)
    }

    pub fn role_cycle(&self) -> Vec<String> {
        if self.cycle_order.is_empty() {
            ["smol", "default", "slow"].map(String::from).to_vec()
        } else {
            self.cycle_order.clone()
        }
    }
}

/// A letter, then letters, digits, `-` or `_`.
pub fn valid_set_name(name: &str) -> bool {
    let mut c = name.chars();
    c.next().is_some_and(|f| f.is_ascii_alphabetic()) && c.all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Parses `models.toml`. A bad section falls back to its default and warns; a file that does not
/// parse at all is ignored.
pub fn parse(text: &str) -> (ModelConfig, Vec<String>) {
    let mut warn = Vec::new();
    let table: toml::Table = match text.parse() {
        Ok(t) => t,
        Err(e) => return (ModelConfig::default(), vec![format!("models.toml: {e}; ignored")]),
    };
    fn section<T: Default + for<'de> Deserialize<'de>>(t: &toml::Table, key: &str, warn: &mut Vec<String>) -> T {
        let Some(v) = t.get(key) else { return T::default() };
        v.clone().try_into().unwrap_or_else(|e| {
            warn.push(format!("models.toml: ignoring `{key}`: {e}"));
            T::default()
        })
    }
    let mut cfg = ModelConfig {
        set: section(&table, "set", &mut warn),
        cycle_order: section(&table, "cycle_order", &mut warn),
        sets: section(&table, "sets", &mut warn),
        roles: section(&table, "roles", &mut warn),
        models: section(&table, "models", &mut warn),
        compaction: section(&table, "compaction", &mut warn),
    };
    let builtins: Vec<String> = cfg.roles.keys().filter(|n| crate::roles::is_builtin(n)).cloned().collect();
    for name in builtins {
        cfg.roles.remove(&name);
        warn.push(format!("models.toml: [roles.{name}] is ignored: @{name} is built in"));
    }
    for (name, meta) in cfg.roles.iter_mut() {
        meta.fallback =
            meta.fallback.iter().map(|f| f.strip_prefix('@').unwrap_or(f).to_string()).filter(|f| f != name).collect();
        if meta.delegate && meta.description.is_none() {
            warn.push(format!(
                "models.toml: [roles.{name}] has delegate = true but no description; the model sees only its name"
            ));
        }
    }
    if let Some(active) = cfg.active() {
        match cfg.set.as_deref() {
            Some(s) if !cfg.sets.contains_key(s) => {
                warn.push(format!("models.toml: set `{s}` does not exist; using `{active}`"));
            }
            None if cfg.sets.len() > 1 => {
                warn.push(format!("models.toml: no `set =` names the active set; using `{active}`"));
            }
            _ => {}
        }
    }
    (cfg, warn)
}

fn parse_doc(text: &str) -> Result<DocumentMut, String> {
    text.parse().map_err(|e| format!("models.toml: {e}; fix it by hand"))
}

fn not_a_table(key: &str) -> String {
    format!("models.toml: `{key}` is not a table; fix it by hand")
}

/// `[sets]`, created as an implicit table so only `[sets.<name>]` headers are written.
fn sets_mut(doc: &mut DocumentMut) -> Result<&mut dyn TableLike, String> {
    doc.as_table_mut()
        .entry("sets")
        .or_insert_with(|| {
            let mut t = Table::new();
            t.set_implicit(true);
            Item::Table(t)
        })
        .as_table_like_mut()
        .ok_or_else(|| not_a_table("sets"))
}

/// Sets `sets.<set>.<role> = value`, creating what is missing.
pub(crate) fn write_role(text: &str, set: &str, role: &str, v: &str) -> Result<String, String> {
    let mut doc = parse_doc(text)?;
    sets_mut(&mut doc)?
        .entry(set)
        .or_insert_with(|| Item::Table(Table::new()))
        .as_table_like_mut()
        .ok_or_else(|| not_a_table(&format!("sets.{set}")))?
        .insert(role, value(v));
    Ok(doc.to_string())
}

/// Removes a role without creating a missing set or removing its table.
pub(crate) fn remove_role(text: &str, set: &str, role: &str) -> Result<String, String> {
    let mut doc = parse_doc(text)?;
    let Some(sets) = doc.get_mut("sets") else { return Ok(text.to_string()) };
    let sets = sets.as_table_like_mut().ok_or_else(|| not_a_table("sets"))?;
    for (name, item) in sets.iter() {
        if !item.is_table_like() {
            return Err(not_a_table(&format!("sets.{name}")));
        }
    }
    let Some(roles) = sets.get_mut(set) else { return Ok(text.to_string()) };
    let roles = roles.as_table_like_mut().ok_or_else(|| not_a_table(&format!("sets.{set}")))?;
    if roles.remove(role).is_none() {
        return Ok(text.to_string());
    }
    Ok(doc.to_string())
}

/// `meta` as `parse` holds it: `@` stripped, the role itself dropped, blank descriptions gone.
fn canonical_meta(name: &str, meta: &RoleMeta) -> RoleMeta {
    let mut fallback: Vec<String> = Vec::new();
    for f in meta.fallback.iter().map(|f| f.strip_prefix('@').unwrap_or(f)) {
        if f != name && !fallback.iter().any(|r| r == f) {
            fallback.push(f.to_string());
        }
    }
    RoleMeta {
        description: meta.description.as_deref().map(str::trim).filter(|d| !d.is_empty()).map(String::from),
        fallback,
        delegate: meta.delegate,
    }
}

/// Writes `[roles.<name>]`: description, fallback list and delegate. Settings that read as they
/// already do leave the file untouched; an empty `meta` removes the table. Other tables and
/// comments are kept.
pub(crate) fn write_role_meta(text: &str, name: &str, meta: &RoleMeta) -> Result<String, String> {
    let meta = canonical_meta(name, meta);
    if parse(text).0.roles.get(name).cloned().unwrap_or_default() == meta {
        return Ok(text.to_string());
    }
    let mut doc = parse_doc(text)?;
    let roles = doc
        .as_table_mut()
        .entry("roles")
        .or_insert_with(|| Item::Table(implicit()))
        .as_table_like_mut()
        .ok_or_else(|| not_a_table("roles"))?;
    if meta.description.is_none() && meta.fallback.is_empty() && !meta.delegate {
        roles.remove(name);
        return Ok(doc.to_string());
    }
    let mut t = Table::new();
    if let Some(d) = &meta.description {
        t.insert("description", value(d.as_str()));
    }
    if !meta.fallback.is_empty() {
        t.insert("fallback", value(meta.fallback.iter().map(String::as_str).collect::<Array>()));
    }
    if meta.delegate {
        t.insert("delegate", value(true));
    }
    roles.insert(name, Item::Table(t));
    Ok(doc.to_string())
}

/// Checks the on-disk sets before editing a specific target.
pub(crate) fn validate_role_target(text: &str, set: &str) -> Result<(), String> {
    let doc = parse_doc(text)?;
    let Some(item) = doc.get("sets") else {
        return if set == MAIN_SET { Ok(()) } else { Err(format!("no model set `{set}`")) };
    };
    let sets = item.as_table_like().ok_or_else(|| not_a_table("sets"))?;
    for (name, item) in sets.iter() {
        if !item.is_table_like() {
            return Err(not_a_table(&format!("sets.{name}")));
        }
    }
    if sets.contains_key(set) || (set == MAIN_SET && sets.is_empty()) {
        Ok(())
    } else {
        Err(format!("no model set `{set}`"))
    }
}

/// Sets the top-level `set = "<name>"`.
pub(crate) fn write_active(text: &str, name: &str) -> Result<String, String> {
    let mut doc = parse_doc(text)?;
    doc.insert("set", value(name));
    Ok(doc.to_string())
}

/// Adds `[sets.<name>]` holding `roles`.
pub(crate) fn write_set(text: &str, name: &str, roles: &BTreeMap<String, String>) -> Result<String, String> {
    let mut doc = parse_doc(text)?;
    let sets = sets_mut(&mut doc)?;
    if sets.contains_key(name) {
        return Err(format!("model set `{name}` already exists"));
    }
    let mut t = Table::new();
    for (k, v) in roles {
        t.insert(k, value(v.as_str()));
    }
    sets.insert(name, Item::Table(t));
    Ok(doc.to_string())
}

pub(crate) fn remove_set(text: &str, name: &str) -> Result<String, String> {
    let mut doc = parse_doc(text)?;
    sets_mut(&mut doc)?.remove(name).ok_or_else(|| format!("no model set `{name}`"))?;
    Ok(doc.to_string())
}

/// Renames a set, and points `set` at the new name when `make_active`.
pub fn rename_set(text: &str, old: &str, new: &str, make_active: bool) -> Result<String, String> {
    let mut doc = parse_doc(text)?;
    let sets = sets_mut(&mut doc)?;
    if sets.contains_key(new) {
        return Err(format!("model set `{new}` already exists"));
    }
    let item = sets.remove(old).ok_or_else(|| format!("no model set `{old}`"))?;
    sets.insert(new, item);
    if make_active {
        doc.insert("set", value(new));
    }
    Ok(doc.to_string())
}

fn implicit() -> Table {
    let mut t = Table::new();
    t.set_implicit(true);
    t
}

/// A fresh document for `cfg`: `set`, `cycle_order`, then sets, models and thresholds.
pub fn render(cfg: &ModelConfig) -> String {
    let mut doc = DocumentMut::new();
    if let Some(s) = &cfg.set {
        doc.insert("set", value(s.as_str()));
    }
    if !cfg.cycle_order.is_empty() {
        doc.insert("cycle_order", value(cfg.cycle_order.iter().map(String::as_str).collect::<Array>()));
    }
    if !cfg.sets.is_empty() {
        let mut sets = implicit();
        for (name, roles) in &cfg.sets {
            let mut t = Table::new();
            for (k, v) in roles {
                t.insert(k, value(v.as_str()));
            }
            sets.insert(name, Item::Table(t));
        }
        doc.insert("sets", Item::Table(sets));
    }
    if !cfg.roles.is_empty() {
        let mut roles = implicit();
        for (name, meta) in &cfg.roles {
            let mut t = Table::new();
            if let Some(d) = &meta.description {
                t.insert("description", value(d.as_str()));
            }
            if !meta.fallback.is_empty() {
                t.insert("fallback", value(meta.fallback.iter().map(String::as_str).collect::<Array>()));
            }
            if meta.delegate {
                t.insert("delegate", value(true));
            }
            roles.insert(name, Item::Table(t));
        }
        doc.insert("roles", Item::Table(roles));
    }
    if !cfg.models.is_empty() {
        let mut models = implicit();
        for (id, lim) in &cfg.models {
            let mut t = Table::new();
            if let Some(c) = lim.context_window {
                t.insert("context_window", value(i64::from(c)));
            }
            if let Some(m) = lim.max_tokens {
                t.insert("max_tokens", value(i64::from(m)));
            }
            models.insert(id, Item::Table(t));
        }
        doc.insert("models", Item::Table(models));
    }
    if !cfg.compaction.thresholds.is_empty() {
        let mut th = Table::new();
        for (k, v) in &cfg.compaction.thresholds {
            match v {
                toml::Value::Integer(n) => th.insert(k, value(*n)),
                toml::Value::String(s) => th.insert(k, value(s.as_str())),
                _ => continue,
            };
        }
        let mut c = implicit();
        c.insert("thresholds", Item::Table(th));
        doc.insert("compaction", Item::Table(c));
    }
    doc.to_string()
}

fn string_map(v: Option<&Yaml>) -> BTreeMap<String, String> {
    let Some(Yaml::Mapping(m)) = v else { return Default::default() };
    m.iter().filter_map(|(k, v)| Some((k.as_str()?.to_string(), v.as_str()?.to_string()))).collect()
}

fn yaml_u32(v: &Yaml, key: &str) -> Option<u32> {
    u32::try_from(v.get(key)?.as_u64()?).ok()
}

/// Reads omp's model config into a `models.toml` config. `mu_thresholds` (from mu's old
/// `config.toml`) win over omp's per key. `None` when there is nothing to copy.
pub(crate) fn build_import(
    config: &Mapping,
    models: &Mapping,
    mu_thresholds: &BTreeMap<String, toml::Value>,
) -> Option<Import> {
    let mut cfg = ModelConfig::default();
    let mut skipped = Vec::new();

    let roles = string_map(config.get("modelRoles"));
    if !roles.is_empty() {
        cfg.sets.insert(MAIN_SET.into(), roles);
    }
    if let Some(Yaml::Mapping(presets)) = config.get("modelPresets") {
        for (name, preset) in presets {
            let Some(name) = name.as_str() else { continue };
            let roles = string_map(preset.get("modelRoles"));
            if roles.is_empty() {
                continue;
            }
            if !valid_set_name(name) {
                skipped
                    .push(format!("skipped omp preset `{name}`: set names are a letter, then letters, digits, - or _"));
            } else if name == MAIN_SET {
                skipped.push(format!("skipped omp preset `{name}`: `main` holds omp's modelRoles"));
            } else {
                cfg.sets.insert(name.to_string(), roles);
            }
        }
    }
    if cfg.sets.len() > 1 {
        cfg.set = Some(if cfg.sets.contains_key(MAIN_SET) {
            MAIN_SET.to_string()
        } else {
            cfg.sets.keys().next().cloned().unwrap_or_default()
        });
    }
    if let Some(Yaml::Sequence(order)) = config.get("cycleOrder") {
        cfg.cycle_order = order.iter().filter_map(|v| v.as_str().map(String::from)).collect();
    }
    if let Some(Yaml::Mapping(th)) = config.get("compaction").and_then(|c| c.get("modelThresholds")) {
        for (k, v) in th {
            let Some(k) = k.as_str() else { continue };
            let v = match v {
                Yaml::Number(n) => n.as_u64().and_then(|n| i64::try_from(n).ok()).map(toml::Value::Integer),
                Yaml::String(s) => Some(toml::Value::String(s.clone())),
                _ => None,
            };
            if let Some(v) = v {
                cfg.compaction.thresholds.insert(k.to_string(), v);
            }
        }
    }
    cfg.compaction.thresholds.extend(mu_thresholds.iter().map(|(k, v)| (k.clone(), v.clone())));

    if let Some(Yaml::Mapping(providers)) = models.get("providers") {
        for (p, pc) in providers {
            let Some(p) = p.as_str() else { continue };
            let mut found: BTreeMap<String, ModelLimits> = BTreeMap::new();
            if let Some(Yaml::Mapping(ov)) = pc.get("modelOverrides") {
                for (id, o) in ov {
                    let Some(id) = id.as_str() else { continue };
                    let l = found.entry(id.to_string()).or_default();
                    l.context_window = yaml_u32(o, "contextWindow");
                    l.max_tokens = yaml_u32(o, "maxTokens");
                }
            }
            if let Some(Yaml::Sequence(entries)) = pc.get("models") {
                for m in entries {
                    let Some(id) = m.get("id").and_then(Yaml::as_str) else { continue };
                    let l = found.entry(id.to_string()).or_default();
                    l.context_window = yaml_u32(m, "contextWindow").or(l.context_window);
                    l.max_tokens = yaml_u32(m, "maxTokens").or(l.max_tokens);
                }
            }
            for (id, l) in found {
                if l != ModelLimits::default() {
                    cfg.models.insert(format!("{p}/{id}"), l);
                }
            }
        }
    }

    if cfg.sets.is_empty()
        && cfg.cycle_order.is_empty()
        && cfg.models.is_empty()
        && cfg.compaction.thresholds.is_empty()
    {
        return None;
    }
    let mut lines: Vec<String> =
        cfg.sets.iter().map(|(name, roles)| format!("set {name}: {} roles", roles.len())).collect();
    if !cfg.cycle_order.is_empty() {
        lines.push(format!("cycle_order: {}", cfg.cycle_order.join(", ")));
    }
    if !cfg.models.is_empty() {
        lines.push(format!("{} model limits (models.yml)", cfg.models.len()));
    }
    if !cfg.compaction.thresholds.is_empty() {
        lines.push(format!("{} compaction thresholds", cfg.compaction.thresholds.len()));
    }
    lines.extend(skipped);
    Some(Import { config: cfg, lines })
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = r#"set = "openai"
cycle_order = ["smol", "default", "slow"]

[sets.main]
default = "anthropic/claude-opus-5-5:high"
smol = "anthropic/claude-haiku-5-5"

[sets.openai]
default = "openai/gpt-5.6:high"

[roles.writer]
description = "Long-form docs and prose"
fallback = ["smol"]
delegate = true

[models."modal/a/b"]
context_window = 1048576
max_tokens = 65536

[compaction.thresholds]
"openai/*" = "50%"
"#;

    fn yaml(s: &str) -> Mapping {
        serde_yaml_ng::from_str(s).unwrap()
    }

    #[test]
    fn parses_the_example() {
        let (c, w) = parse(EXAMPLE);
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(c.active(), Some("openai"));
        assert_eq!(c.role("default").map(String::as_str), Some("openai/gpt-5.6:high"));
        assert_eq!(c.role("smol"), None);
        assert_eq!(c.role_cycle(), ["smol", "default", "slow"]);
        assert_eq!(c.models["modal/a/b"], ModelLimits { context_window: Some(1048576), max_tokens: Some(65536) });
        assert_eq!(c.compaction.thresholds["openai/*"], toml::Value::String("50%".into()));
        assert_eq!(parse("cycle_order = [\"slow\"]\n").0.role_cycle(), ["slow"]);
        assert_eq!(parse("").0.role_cycle(), ["smol", "default", "slow"]);
    }

    #[test]
    fn role_metadata_warns_about_builtins_trailing_at_and_missing_descriptions() {
        let (c, w) = parse("[roles.default]\ndescription = \"x\"\n");
        assert!(c.roles.is_empty() && w.len() == 1 && w[0].contains("is built in"), "{w:?}");
        let (c, w) = parse("[roles.w]\ndelegate = true\n");
        assert!(c.roles["w"].delegate);
        assert!(w.len() == 1 && w[0].contains("no description"), "{w:?}");
        let (c, w) = parse("[roles.w]\ndescription = \"d\"\nfallback = [\"@smol\", \"w\"]\n");
        assert_eq!(c.roles["w"].fallback, ["smol"]);
        assert!(w.is_empty(), "{w:?}");
        let (c, w) = parse("[roles.w]\nnope = true\n");
        assert!(c.roles.is_empty() && w.len() == 1 && w[0].contains("ignoring `roles`"), "{w:?}");
    }

    #[test]
    fn active_set_rules_and_warnings() {
        let (c, w) = parse("[sets.a]\nx = \"1\"\n[sets.b]\nx = \"2\"\n");
        assert_eq!(c.active(), Some("a"));
        assert!(w[0].contains("no `set =`") && w[0].contains("`a`"), "{w:?}");
        let (c, w) = parse("[sets.z]\nx = \"1\"\n[sets.main]\nx = \"2\"\n");
        assert_eq!((c.active(), w.len()), (Some("main"), 1));
        let (c, w) = parse("[sets.only]\nx = \"1\"\n");
        assert_eq!((c.active(), w.len()), (Some("only"), 0));
        let (c, w) = parse("set = \"gone\"\n[sets.only]\nx = \"1\"\n");
        assert_eq!(c.active(), Some("only"));
        assert!(w[0].contains("`gone` does not exist") && w[0].contains("using `only`"), "{w:?}");
        let (c, w) = parse("set = \"b\"\n[sets.a]\nx = \"1\"\n[sets.b]\nx = \"2\"\n");
        assert_eq!((c.active(), w.len()), (Some("b"), 0));
        let (c, w) = parse("set = \"nope\"\n");
        assert_eq!((c.active(), c.role("x"), w.len()), (None, None, 0));
    }

    #[test]
    fn bad_sections_are_dropped_one_by_one() {
        let (c, w) = parse("cycle_order = 7\n[sets.main]\nsmol = \"a/b\"\n");
        assert_eq!(c.role("smol").map(String::as_str), Some("a/b"));
        assert!(c.cycle_order.is_empty());
        assert!(w.len() == 1 && w[0].contains("cycle_order"), "{w:?}");
        let (c, w) = parse("not toml [");
        assert_eq!(c, ModelConfig::default());
        assert!(w[0].ends_with("; ignored"), "{w:?}");
    }

    #[test]
    fn set_names() {
        for ok in ["a", "openai", "gpt-5_x", "A1"] {
            assert!(valid_set_name(ok), "{ok}");
        }
        for bad in ["", "1a", "-a", "a b", "a.b", "é"] {
            assert!(!valid_set_name(bad), "{bad}");
        }
    }

    #[test]
    fn write_role_keeps_comments_and_other_tables() {
        let t = "# mine\n[sets.main]\ndefault = \"a/b\" # keep\n\n[models.\"x/y\"]\nmax_tokens = 5\n";
        let out = write_role(t, "main", "smol", "c/d").unwrap();
        assert!(out.contains("# mine") && out.contains("# keep") && out.contains("max_tokens = 5"), "{out}");
        let (c, _) = parse(&out);
        assert_eq!((c.role("default").unwrap().as_str(), c.role("smol").unwrap().as_str()), ("a/b", "c/d"));
        let out = write_role(&out, "main", "smol", "e/f").unwrap();
        assert_eq!(parse(&out).0.role("smol").map(String::as_str), Some("e/f"));
        assert_eq!(out.matches("smol").count(), 1);
    }

    #[test]
    fn remove_role_preserves_other_text_and_empty_sets() {
        let text = "# top\n[sets.main]\nsmol = \"a/b\"\n# keep\ndefault = \"c/d\"\n\n[sets.other]\nx = \"e/f\"\n";
        let out = remove_role(text, "main", "smol").unwrap();
        assert_eq!(out, text.replace("smol = \"a/b\"\n", ""));
        assert_eq!(remove_role(text, "main", "missing").unwrap(), text);
        assert_eq!(remove_role(text, "missing", "smol").unwrap(), text);
        assert_eq!(remove_role("# only\n", "main", "smol").unwrap(), "# only\n");
        assert_eq!(remove_role("[sets.main]\nsmol = \"a/b\"\n", "main", "smol").unwrap(), "[sets.main]\n");
        assert_eq!(
            remove_role("sets = { main = { smol = \"a/b\", default = \"c/d\" } }\n", "main", "smol").unwrap(),
            "sets = { main = { default = \"c/d\" } }\n"
        );
        assert!(remove_role("x = [", "main", "smol").is_err());
        assert!(remove_role("sets = 3\n", "main", "smol").is_err());
        assert!(remove_role("[sets]\nmain = 3\n", "main", "smol").is_err());
    }

    #[test]
    fn role_meta_writes_only_the_rows_that_differ() {
        let meta = RoleMeta {
            description: Some("Long-form docs".into()),
            fallback: vec!["@smol".into(), "default".into()],
            delegate: true,
        };
        let text = "# keep\n[sets.main]\nwriter = \"a/b\"\n";
        let out = write_role_meta(text, "writer", &meta).unwrap();
        assert!(out.starts_with("# keep\n") && out.contains("[sets.main]"), "{out}");
        assert_eq!(parse(&out).0.roles["writer"], canonical_meta("writer", &meta));
        assert_eq!(write_role_meta(&out, "writer", &meta).unwrap(), out, "unchanged settings keep the file");
        assert_eq!(
            write_role_meta(&out, "writer", &RoleMeta { description: Some("Docs".into()), ..meta.clone() })
                .unwrap()
                .matches("[roles.writer]")
                .count(),
            1
        );
        let cleared = write_role_meta(&out, "writer", &RoleMeta::default()).unwrap();
        assert!(!cleared.contains("[roles") && cleared.contains("[sets.main]"), "{cleared}");

        let bare = write_role_meta("", "writer", &meta).unwrap();
        assert_eq!(
            parse(&bare).0.roles["writer"],
            RoleMeta {
                description: Some("Long-form docs".into()),
                fallback: vec!["smol".into(), "default".into()],
                delegate: true
            }
        );
        assert!(write_role_meta("x = [", "writer", &meta).unwrap_err().contains("fix it by hand"));
        assert!(write_role_meta("roles = 3\n", "writer", &meta).unwrap_err().contains("not a table"));
        assert_eq!(write_role_meta("# only\n", "writer", &RoleMeta::default()).unwrap(), "# only\n");
    }

    #[test]
    fn role_targets_are_checked_against_valid_disk_structure() {
        assert!(validate_role_target("", "main").is_ok());
        assert!(validate_role_target("[sets]\n", "main").is_ok());
        assert!(validate_role_target("", "other").is_err());
        assert!(validate_role_target("[sets.other]\n", "main").is_err());
        assert!(validate_role_target("[sets.other]\n", "other").is_ok());
        assert!(validate_role_target("sets = 3\n", "main").unwrap_err().contains("not a table"));
        assert!(validate_role_target("[sets]\nother = 3\n", "main").unwrap_err().contains("not a table"));
        assert!(validate_role_target("not valid [", "other").unwrap_err().contains("fix it by hand"));
    }

    #[test]
    fn creating_sets_writes_no_bare_sets_header() {
        let out = write_role("", "main", "default", "a/b").unwrap();
        assert!(out.contains("[sets.main]") && !out.contains("[sets]"), "{out}");
        assert_eq!(out.trim(), "[sets.main]\ndefault = \"a/b\"");
        let roles = BTreeMap::from([("default".to_string(), "c/d".to_string())]);
        let out = write_set(&out, "oa", &roles).unwrap();
        assert!(out.contains("[sets.oa]") && !out.contains("[sets]"), "{out}");
        assert_eq!(parse(&out).0.sets.len(), 2);
        assert!(write_set(&out, "oa", &roles).unwrap_err().contains("already exists"));
        assert!(write_set("# c\n", "n", &roles).unwrap().contains("# c"));
    }

    #[test]
    fn active_remove_and_rename() {
        let t = "# top\n[sets.main]\na = \"1\"\n[sets.oa]\na = \"2\"\n";
        let out = write_active(t, "oa").unwrap();
        assert!(out.contains("# top") && parse(&out).0.active() == Some("oa"), "{out}");
        let out = write_active(&out, "main").unwrap();
        assert_eq!(out.matches("set =").count(), 1);
        assert_eq!(parse(&out).0.active(), Some("main"));

        let removed = remove_set(&out, "oa").unwrap();
        assert!(!removed.contains("[sets.oa]") && removed.contains("[sets.main]") && removed.contains("# top"));
        assert!(remove_set(&out, "zz").is_err());

        let renamed = rename_set(&out, "oa", "gpt", false).unwrap();
        let (c, _) = parse(&renamed);
        assert_eq!(c.sets.keys().collect::<Vec<_>>(), ["gpt", "main"]);
        assert_eq!(c.active(), Some("main"));
        let active = rename_set(&write_active(t, "oa").unwrap(), "oa", "gpt", true).unwrap();
        let (c, w) = parse(&active);
        assert_eq!((c.active(), c.set.as_deref(), w.len()), (Some("gpt"), Some("gpt"), 0));
        assert!(rename_set(t, "oa", "main", false).unwrap_err().contains("already exists"));
        assert!(rename_set(t, "zz", "q", false).is_err());
    }

    #[test]
    fn edits_refuse_a_broken_file() {
        for r in [write_role("x = [", "main", "a", "b"), write_active("x = [", "a")] {
            assert!(r.unwrap_err().ends_with("fix it by hand"));
        }
        assert!(write_role("sets = 3\n", "main", "a", "b").unwrap_err().contains("not a table"));
    }

    #[test]
    fn import_from_omp_files() {
        let cfg = yaml(
            "modelRoles:\n  default: a/b:high\n  smol: c/d\n  odd: 7\ncycleOrder: [slow, default]\n\
             modelPresets:\n  openai:\n    modelRoles:\n      default: o/p\n  main:\n    modelRoles:\n      default: x/y\n  \"bad name\":\n    modelRoles:\n      default: x/y\n\
             compaction:\n  modelThresholds:\n    anthropic/claude-haiku-5-5: 800000\n    \"openai/*\": 50%\n    junk: [1]\n",
        );
        let models = yaml(
            "providers:\n  modal:\n    models:\n      - id: a/b\n        contextWindow: 1000\n    modelOverrides:\n      a/b:\n        contextWindow: 5\n        maxTokens: 9\n  openai-codex:\n    modelOverrides:\n      g:\n        contextWindow: 272000\n",
        );
        let mu = BTreeMap::from([("anthropic/claude-haiku-5-5".to_string(), toml::Value::String("f20000".into()))]);
        let imp = build_import(&cfg, &models, &mu).unwrap();
        let c = &imp.config;
        assert_eq!(c.sets.keys().collect::<Vec<_>>(), ["main", "openai"]);
        assert_eq!(c.sets["main"].len(), 2);
        assert_eq!(c.set.as_deref(), Some("main"));
        assert_eq!(c.cycle_order, ["slow", "default"]);
        assert_eq!(c.models["modal/a/b"], ModelLimits { context_window: Some(1000), max_tokens: Some(9) });
        assert_eq!(c.models["openai-codex/g"].context_window, Some(272000));
        assert_eq!(c.compaction.thresholds["anthropic/claude-haiku-5-5"], toml::Value::String("f20000".into()));
        assert_eq!(c.compaction.thresholds["openai/*"], toml::Value::String("50%".into()));
        assert!(!c.compaction.thresholds.contains_key("junk"));
        let l = imp.lines.join("\n");
        assert!(l.contains("set main: 2 roles") && l.contains("set openai: 1 roles"), "{l}");
        assert!(l.contains("cycle_order: slow, default") && l.contains("2 model limits (models.yml)"), "{l}");
        assert!(l.contains("2 compaction thresholds"), "{l}");
        assert!(l.contains("skipped omp preset `main`") && l.contains("skipped omp preset `bad name`"), "{l}");
        let (back, w) = parse(&render(c));
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(&back, c);
    }

    #[test]
    fn import_of_nothing_is_none_and_single_set_has_no_active() {
        assert!(build_import(&Mapping::new(), &Mapping::new(), &BTreeMap::new()).is_none());
        let imp = build_import(&yaml("modelRoles: {default: a/b}\n"), &Mapping::new(), &BTreeMap::new()).unwrap();
        assert_eq!(imp.config.set, None);
        let only_presets =
            yaml("modelPresets:\n  b: {modelRoles: {default: x/y}}\n  a: {modelRoles: {default: x/z}}\n");
        let imp = build_import(&only_presets, &Mapping::new(), &BTreeMap::new()).unwrap();
        assert_eq!(imp.config.set.as_deref(), Some("a"));
    }

    #[test]
    fn render_parse_round_trip() {
        let (c, _) = parse(EXAMPLE);
        let text = render(&c);
        assert!(!text.contains("[sets]") && !text.contains("[compaction]") && !text.contains("[models]"), "{text}");
        assert!(text.find("set =").unwrap() < text.find("[sets.main]").unwrap());
        assert_eq!(parse(&text), (c, Vec::new()));
        assert_eq!(render(&ModelConfig::default()), "");
    }
}
