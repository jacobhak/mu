use crate::model_config::{self, Import, ModelConfig, ModelLimits, RoleMeta};
use mu_common::paths;
use serde::Deserialize;
use std::cell::RefCell;

mod mu;
mod retry;

pub use mu::MuAgent;
pub use mu::MuRemote;
pub use mu::{Cards, MuBell, MuConfig, MuPermissions, MuStatus, MuTheme};
use mu::{MuPredict, check_predict, check_status, load_permissions, load_remotes};
use retry::check_retry;
#[cfg(test)]
use retry::{DEFAULT_MAX_ATTEMPTS, DEFAULT_MAX_WAIT, RetryPolicy};
pub use retry::{MuRetry, RetryRule, parse_duration};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// How a threshold value bounds the context window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Threshold {
    /// Integer: caps the window itself.
    Window(u32),
    /// `"N%"` of the window.
    Percent(u32),
    /// `"fN"`: fixed token count.
    Fixed(u32),
}

/// Largest token count accepted in a threshold (100M); bigger values are clamped with a warning.
pub(crate) const MAX_THRESHOLD: u32 = 100_000_000;

/// Converts to `u32`, clamping to `MAX_THRESHOLD` and noting it in `warn`.
fn clamp_tokens(n: u64, key: &str, warn: &mut Vec<String>) -> u32 {
    match u32::try_from(n) {
        Ok(v) if v <= MAX_THRESHOLD => v,
        _ => {
            warn.push(format!("threshold for {key} is out of range ({n}); using {MAX_THRESHOLD}"));
            MAX_THRESHOLD
        }
    }
}

impl Threshold {
    fn from_toml(v: &toml::Value, key: &str, warn: &mut Vec<String>) -> Option<Self> {
        match v {
            toml::Value::Integer(n) => match u64::try_from(*n) {
                Ok(n) => Some(Self::Window(clamp_tokens(n, key, warn))),
                Err(_) => {
                    warn.push(format!("ignoring negative threshold for {key}"));
                    None
                }
            },
            toml::Value::String(s) => Self::from_str(s, key, warn),
            _ => None,
        }
    }

    fn from_str(s: &str, key: &str, warn: &mut Vec<String>) -> Option<Self> {
        let s = s.trim();
        if let Some(p) = s.strip_suffix('%') {
            return p.trim().parse().ok().map(Self::Percent);
        }
        if let Some(n) = s.strip_prefix('f') {
            return n.trim().parse::<u64>().ok().map(|n| Self::Fixed(clamp_tokens(n, key, warn)));
        }
        s.parse::<u64>().ok().map(|n| Self::Window(clamp_tokens(n, key, warn)))
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct OmpConfig {
    pub skills: OmpSkills,
    pub status_line: OmpStatusLine,
    pub composer: OmpComposer,
    pub symbol_preset: Option<String>,
    pub hide_thinking_block: bool,
    pub display: OmpDisplay,
    pub completion: OmpCompletion,
}

#[derive(Debug, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct OmpDisplay {
    pub smooth_streaming: bool,
}

impl Default for OmpDisplay {
    fn default() -> Self {
        Self { smooth_streaming: true }
    }
}

/// omp's `completion.notify` (`on`/`off`, default on): ring the terminal when a turn you typed ends
/// with nothing running in the background, or when a running turn needs you (`ask`, an approval,
/// plan review), whoever started it.
#[derive(Debug, Clone, Copy)]
pub struct OmpCompletion {
    pub notify: bool,
}

impl Default for OmpCompletion {
    fn default() -> Self {
        Self { notify: true }
    }
}

impl<'de> Deserialize<'de> for OmpCompletion {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize, Default)]
        #[serde(default)]
        struct Raw {
            notify: Option<serde_yaml_ng::Value>,
        }
        let raw = Raw::deserialize(d)?;
        let notify = match raw.notify {
            Some(serde_yaml_ng::Value::Bool(b)) => b,
            Some(serde_yaml_ng::Value::String(s)) => {
                !matches!(s.trim().to_ascii_lowercase().as_str(), "off" | "false" | "no")
            }
            _ => true,
        };
        Ok(Self { notify })
    }
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct OmpStatusLine {
    pub preset: Option<String>,
    pub left_segments: Vec<String>,
    pub right_segments: Vec<String>,
    pub segment_options: BTreeMap<String, serde_yaml_ng::Value>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct OmpComposer {
    pub shape: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct OmpSkills {
    pub enable_claude_user: bool,
}

#[derive(Debug, Default)]
pub struct Configs {
    pub omp: OmpConfig,
    pub mu: MuConfig,
    pub models: RefCell<ModelConfig>,
    /// What `mu models import` would copy; set at load when models.toml is missing and omp has model config.
    pub import_offer: RefCell<Option<Import>>,
    /// Problems found while loading; shown as notices at startup.
    pub warnings: Vec<String>,
}

/// This profile's `models.toml`.
pub fn models_path() -> PathBuf {
    paths::profile_home().join("models.toml")
}

/// mu's old `[compaction.model_thresholds]` in `config.toml`, which an import carries over.
fn old_mu_thresholds() -> BTreeMap<String, toml::Value> {
    let Ok(text) = std::fs::read_to_string(paths::mu_home().join("config.toml")) else { return Default::default() };
    let Ok(table) = text.parse::<toml::Table>() else { return Default::default() };
    table
        .get("compaction")
        .and_then(|c| c.get("model_thresholds"))
        .and_then(toml::Value::as_table)
        .map(|t| t.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default()
}

/// What copying omp's model config (and mu's old thresholds) into `models.toml` would write.
pub fn omp_import() -> Option<Import> {
    let mut ignored = Vec::new();
    let config = yaml_doc("config.yml", &mut ignored);
    let models = yaml_doc("models.yml", &mut ignored);
    model_config::build_import(&config, &models, &old_mu_thresholds())
}

/// One top-level YAML section; a bad one falls back to its default and warns, naming the section only.
fn yaml_section<T: Default + for<'de> Deserialize<'de>>(
    doc: &serde_yaml_ng::Mapping,
    key: &str,
    file: &str,
    warn: &mut Vec<String>,
) -> T {
    match doc.get(key) {
        None | Some(serde_yaml_ng::Value::Null) => T::default(),
        Some(v) => serde_yaml_ng::from_value(v.clone()).unwrap_or_else(|e| {
            warn.push(format!("{file}: ignoring `{key}`: {e}"));
            T::default()
        }),
    }
}

fn yaml_doc(name: &str, warn: &mut Vec<String>) -> serde_yaml_ng::Mapping {
    let p = paths::omp_agent_dir().join(name);
    let Ok(text) = std::fs::read_to_string(&p) else { return Default::default() };
    match serde_yaml_ng::from_str::<serde_yaml_ng::Value>(&text) {
        Ok(serde_yaml_ng::Value::Mapping(m)) => m,
        Ok(serde_yaml_ng::Value::Null) => Default::default(),
        Ok(_) => {
            warn.push(format!("{}: expected a mapping at the top level; ignored", p.display()));
            Default::default()
        }
        Err(e) => {
            warn.push(format!("{}: {e}; ignored", p.display()));
            Default::default()
        }
    }
}

fn load_omp(warn: &mut Vec<String>) -> OmpConfig {
    let d = yaml_doc("config.yml", warn);
    let f = "config.yml";
    OmpConfig {
        skills: yaml_section(&d, "skills", f, warn),
        status_line: yaml_section(&d, "statusLine", f, warn),
        composer: yaml_section(&d, "composer", f, warn),
        symbol_preset: yaml_section(&d, "symbolPreset", f, warn),
        hide_thinking_block: yaml_section(&d, "hideThinkingBlock", f, warn),
        display: yaml_section(&d, "display", f, warn),
        completion: yaml_section(&d, "completion", f, warn),
    }
}

fn load_mu(warn: &mut Vec<String>) -> MuConfig {
    let p = paths::mu_home().join("config.toml");
    let Ok(text) = std::fs::read_to_string(&p) else { return MuConfig::default() };
    let table: toml::Table = match text.parse() {
        Ok(t) => t,
        Err(e) => {
            warn.push(format!("{}: {e}; ignored", p.display()));
            return MuConfig::default();
        }
    };
    let section =
        |key: &str| -> toml::Value { table.get(key).cloned().unwrap_or(toml::Value::Table(Default::default())) };
    fn parse<T: Default + for<'de> Deserialize<'de>>(v: toml::Value, key: &str, warn: &mut Vec<String>) -> T {
        v.try_into().unwrap_or_else(|e| {
            warn.push(format!("config.toml: ignoring [{key}]: {e}"));
            T::default()
        })
    }
    if table.contains_key("compaction") {
        warn.push(format!(
            "config.toml: [compaction] is no longer read; thresholds now live in [compaction.thresholds] of {}",
            models_path().display()
        ));
    }
    if table.contains_key("bash") {
        warn.push(
            "config.toml: [bash] is no longer read; an ordinary `bash` call waits for its own result instead of \
             being refused (see docs/background-jobs.md)"
                .to_string(),
        );
    }
    if table.contains_key("experimental") {
        warn.push("config.toml: [experimental] is no longer read; `interruptible_wait` is now always on".to_string());
    }
    let predict: MuPredict = parse(section("predict"), "predict", warn);
    check_predict(&predict, warn);
    let retry: MuRetry = parse(section("retry"), "retry", warn);
    check_retry(&retry, warn);
    let mut status: MuStatus = parse(section("status"), "status", warn);
    check_status(&mut status, warn);
    MuConfig {
        predict,
        ask: parse(section("ask"), "ask", warn),
        theme: parse(section("theme"), "theme", warn),
        update: parse(section("update"), "update", warn),
        edit: parse(section("edit"), "edit", warn),
        retry,
        mcp: parse(section("mcp"), "mcp", warn),
        secrets: parse(section("secrets"), "secrets", warn),
        status,
        task: parse(section("task"), "task", warn),
        agent: parse(section("agent"), "agent", warn),
        permissions: load_permissions(&section("permissions"), warn),
        bell: parse(section("bell"), "bell", warn),
        default_remote: table.get("default_remote").and_then(|v| v.as_str()).map(str::to_string),
        remote: load_remotes(section("remote"), warn),
    }
}

pub fn nothing_to_import() -> String {
    format!("nothing to import: omp has no model config in {}", paths::omp_agent_dir().display())
}

/// An import must not silently replace an existing `models.toml`.
pub fn check_import_target(force: bool) -> Result<(), String> {
    let path = models_path();
    if path.exists() && !force {
        return Err(format!("{} already exists; `mu models import --force` replaces it", path.display()));
    }
    Ok(())
}

fn invalid_set_name(name: &str) -> String {
    format!("invalid set name `{name}`: use a letter, then letters, digits, - or _")
}

impl Configs {
    /// Test and tool helper: a `Configs` holding just this `models.toml` text.
    pub fn from_models_toml(text: &str) -> Self {
        Self { models: RefCell::new(model_config::parse(text).0), ..Default::default() }
    }

    /// Configured `provider/model[:level]` for a role in the active set.
    pub fn role(&self, name: &str) -> Option<String> {
        self.models.borrow().role(name).cloned()
    }

    pub fn role_cycle(&self) -> Vec<String> {
        self.models.borrow().role_cycle()
    }

    /// What `[roles.<name>]` says about a custom role.
    pub fn role_meta(&self, name: &str) -> Option<RoleMeta> {
        self.models.borrow().roles.get(name).cloned()
    }

    /// Every role mu does not use itself: the active set's keys and every `[roles.<name>]`, sorted.
    pub(crate) fn custom_role_names(&self) -> Vec<String> {
        let models = self.models.borrow();
        let mut names: Vec<String> = models
            .active()
            .and_then(|s| models.sets.get(s))
            .map(|roles| roles.keys().cloned().collect())
            .unwrap_or_default();
        names.extend(models.roles.keys().cloned());
        names.retain(|n| !crate::roles::is_builtin(n));
        names.sort_unstable();
        names.dedup();
        names
    }

    pub(crate) fn model_limits(&self, id: &str) -> Option<ModelLimits> {
        self.models.borrow().models.get(id).copied()
    }

    /// Ids with limits in models.toml.
    pub fn listed_models(&self) -> Vec<String> {
        self.models.borrow().models.keys().cloned().collect()
    }

    /// Every set that assigns the role, sorted.
    pub(crate) fn sets_with_role(&self, name: &str) -> Vec<String> {
        let models = self.models.borrow();
        models.sets.iter().filter(|(_, roles)| roles.contains_key(name)).map(|(set, _)| set.clone()).collect()
    }

    pub fn set_names(&self) -> Vec<String> {
        self.models.borrow().sets.keys().cloned().collect()
    }

    pub fn active_set(&self) -> Option<String> {
        self.models.borrow().active().map(String::from)
    }

    /// Applies `f` to the current text of `models.toml` (read fresh, so another session's changes are
    /// kept), writes the result and reloads from it.
    fn edit_models(&self, f: impl FnOnce(&str) -> Result<String, String>) -> Result<(), String> {
        let path = models_path();
        // Held to the end: another session's read-modify-write must not interleave with ours.
        let mut lock_name = path.file_name().unwrap_or_default().to_os_string();
        lock_name.push(".lock");
        let _lock = mu_common::fs::FileLock::exclusive(&path.with_file_name(lock_name))
            .map_err(|e| format!("{}: locking: {e}", path.display()))?;
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        let out = f(&text)?;
        if out != text {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
            }
            mu_common::fs::write_atomic(&path, out.as_bytes()).map_err(|e| format!("{}: {e}", path.display()))?;
        }
        *self.models.borrow_mut() = model_config::parse(&out).0;
        *self.import_offer.borrow_mut() = None;
        Ok(())
    }

    /// Assigns or removes a role in a specific set, using the current on-disk configuration.
    pub fn edit_role(&self, set: &str, name: &str, model: Option<&str>) -> Result<(), String> {
        self.edit_models(|text| {
            model_config::validate_role_target(text, set)?;
            match model {
                Some(model) => model_config::write_role(text, set, name, model),
                None => model_config::remove_role(text, set, name),
            }
        })
    }

    /// Assigns or removes a custom role in `set` and writes its `[roles.<name>]` settings in the
    /// same edit; empty settings remove the table, so deleting a role leaves nothing behind.
    pub fn edit_custom_role(&self, set: &str, name: &str, model: Option<&str>, meta: &RoleMeta) -> Result<(), String> {
        self.edit_models(|text| {
            model_config::validate_role_target(text, set)?;
            let text = model_config::write_role_meta(text, name, meta)?;
            match model {
                Some(model) => model_config::write_role(&text, set, name, model),
                None => model_config::remove_role(&text, set, name),
            }
        })
    }

    fn no_such_set(&self, name: &str) -> String {
        let names = self.set_names();
        if names.is_empty() {
            format!("no model set `{name}`: there are no model sets yet")
        } else {
            format!("no model set `{name}` (sets: {})", names.join(", "))
        }
    }

    /// Makes `name` the active set, for this profile.
    pub fn switch_set(&self, name: &str) -> Result<(), String> {
        if !self.set_names().iter().any(|n| n == name) {
            return Err(self.no_such_set(name));
        }
        self.edit_models(|t| model_config::write_active(t, name))
    }

    /// Adds set `name` holding a copy of the active set's roles and makes it active. Returns the set
    /// it was copied from.
    pub fn new_set(&self, name: &str) -> Result<Option<String>, String> {
        if !model_config::valid_set_name(name) {
            return Err(invalid_set_name(name));
        }
        if self.set_names().iter().any(|n| n == name) {
            return Err(format!("model set `{name}` already exists"));
        }
        let from = self.active_set();
        let roles = from.as_ref().and_then(|f| self.models.borrow().sets.get(f).cloned()).unwrap_or_default();
        self.edit_models(|t| {
            let t = model_config::write_set(t, name, &roles)?;
            model_config::write_active(&t, name)
        })?;
        Ok(from)
    }

    pub fn delete_set(&self, name: &str) -> Result<(), String> {
        if !self.set_names().iter().any(|n| n == name) {
            return Err(self.no_such_set(name));
        }
        if self.active_set().as_deref() == Some(name) {
            return Err(format!("`{name}` is the active model set; switch to another first"));
        }
        self.edit_models(|t| model_config::remove_set(t, name))
    }

    pub fn rename_set(&self, old: &str, new: &str) -> Result<(), String> {
        if !self.set_names().iter().any(|n| n == old) {
            return Err(self.no_such_set(old));
        }
        if old == new {
            return Err(format!("model set `{old}` already has that name"));
        }
        if !model_config::valid_set_name(new) {
            return Err(invalid_set_name(new));
        }
        if self.set_names().iter().any(|n| n == new) {
            return Err(format!("model set `{new}` already exists"));
        }
        let active = self.active_set().as_deref() == Some(old);
        self.edit_models(|t| model_config::rename_set(t, old, new, active))
    }

    /// Copies omp's model config into `models.toml`; returns what was copied, one line per item.
    pub fn import_models(&self, force: bool) -> Result<Vec<String>, String> {
        check_import_target(force)?;
        let import = omp_import().ok_or_else(nothing_to_import)?;
        self.write_import(&import, force)?;
        Ok(import.lines)
    }

    /// Writes `import` as `models.toml`; refuses to replace an existing file unless `force`.
    pub fn write_import(&self, import: &Import, force: bool) -> Result<(), String> {
        check_import_target(force)?;
        let text = model_config::render(&import.config);
        self.edit_models(|_| Ok(text))
    }

    pub fn load() -> Self {
        let mut warnings = Vec::new();
        let mut mu = load_mu(&mut warnings);
        // `--theme` (via MU_THEME) overrides `[theme] source` for this run.
        if let Some(t) = std::env::var("MU_THEME").ok().filter(|t| !t.is_empty()) {
            mu.theme.source = t;
        }
        let omp = load_omp(&mut warnings);
        let path = models_path();
        let mut models = ModelConfig::default();
        let mut import_offer = None;
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                let (cfg, w) = model_config::parse(&text);
                models = cfg;
                warnings.extend(w);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => import_offer = omp_import(),
            Err(e) => warnings.push(format!("{}: {e}", path.display())),
        }
        let mut cfg =
            Self { omp, mu, models: RefCell::new(models), import_offer: RefCell::new(import_offer), warnings };
        let mut w = Vec::new();
        cfg.thresholds_with(&mut w);
        cfg.warnings.extend(w);
        cfg
    }

    pub fn thresholds(&self) -> BTreeMap<String, Threshold> {
        self.thresholds_with(&mut Vec::new())
    }

    fn thresholds_with(&self, warn: &mut Vec<String>) -> BTreeMap<String, Threshold> {
        self.models
            .borrow()
            .compaction
            .thresholds
            .iter()
            .filter_map(|(k, v)| Some((k.clone(), Threshold::from_toml(v, k, warn)?)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permissions::PermissionMode;

    #[test]
    fn edit_role_reads_disk_and_skips_unchanged_writes() {
        use std::os::unix::fs::MetadataExt;
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = LOCK.lock().unwrap();
        let home = mu_testutil::home();
        let path = home.join("models.toml");
        let cfg = Configs::from_models_toml("[sets.main]\ndefault = \"stale/model:high\"\n");
        let disk = "# disk\n[sets.main]\ndefault = \"disk/model:high\"\nsmol = \"disk/small\"\n";
        std::fs::write(&path, disk).unwrap();
        cfg.edit_role("main", "default", Some("new/model")).unwrap();
        let out = std::fs::read_to_string(&path).unwrap();
        assert!(out.contains("# disk") && out.contains("smol = \"disk/small\""));
        assert_eq!(cfg.role("default").as_deref(), Some("new/model"));
        let inode = std::fs::metadata(&path).unwrap().ino();
        *cfg.models.borrow_mut() = ModelConfig::default();
        cfg.edit_role("main", "absent", None).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().ino(), inode);
        assert_eq!(cfg.role("default").as_deref(), Some("new/model"));
        cfg.edit_role("main", "default", None).unwrap();
        assert_eq!(cfg.role("default"), None);
        assert!(std::fs::read_to_string(&path).unwrap().contains("[sets.main]"));
        let before = cfg.models.borrow().clone();
        std::fs::write(&path, "sets = 3\n").unwrap();
        assert!(cfg.edit_role("main", "default", Some("x/y")).is_err());
        assert_eq!(*cfg.models.borrow(), before);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "sets = 3\n");
        std::fs::write(&path, "[sets.other]\nx = \"x/y\"\n").unwrap();
        assert!(cfg.edit_role("main", "default", Some("x/y")).is_err());
        assert_eq!(*cfg.models.borrow(), before);

        std::fs::write(&path, disk).unwrap();
        let meta = RoleMeta { description: Some("Docs".into()), fallback: vec!["smol".into()], delegate: true };
        cfg.edit_custom_role("main", "writer", Some("w/m"), &meta).unwrap();
        let out = std::fs::read_to_string(&path).unwrap();
        assert!(out.contains("[sets.main]") && out.contains("[roles.writer]"), "{out}");
        assert_eq!((cfg.role("writer").as_deref(), cfg.role_meta("writer")), (Some("w/m"), Some(meta.clone())));
        assert_eq!(cfg.sets_with_role("writer"), ["main"]);
        cfg.edit_custom_role("main", "writer", None, &RoleMeta::default()).unwrap();
        let out = std::fs::read_to_string(&path).unwrap();
        assert!(!out.contains("[roles") && !out.contains("writer"), "{out}");
        assert_eq!((cfg.role("writer"), cfg.sets_with_role("writer")), (None, vec![]));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn predict_engine_and_threads_parse_and_unknown_engine_warns() {
        let p = toml::from_str::<MuPredict>("engine = \"smollm\"\nthreads = 2\n").unwrap();
        assert_eq!((p.engine.as_str(), p.threads), ("smollm", 2));
        let d = MuPredict::default();
        assert_eq!((d.engine.as_str(), d.threads, d.threshold), ("ngram", 4, 0.3));
        let mut w = Vec::new();
        check_predict(&toml::from_str::<MuPredict>("engine = \"bogus\"").unwrap(), &mut w);
        check_predict(&MuPredict::default(), &mut w);
        check_predict(&p, &mut w);
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("bogus"), "{}", w[0]);
    }

    #[test]
    fn completion_notify_accepts_omp_spellings() {
        let on = |y: &str| serde_yaml_ng::from_str::<OmpCompletion>(y).unwrap().notify;
        assert!(on("{}"));
        assert!(on("notify: on"));
        assert!(!on("notify: off"));
        assert!(!on("notify: \"off\""));
        assert!(!on("notify: false"));
        assert!(on("notify: true"));
        assert!(OmpCompletion::default().notify);
    }

    #[test]
    fn theme_section_parses_with_defaults() {
        let c = toml::from_str::<MuConfig>("[theme]\nsource = \"gruvbox\"\ncards = \"outline\"\n").unwrap();
        assert_eq!((c.theme.source.as_str(), c.theme.cards), ("gruvbox", Cards::Outline));
        let d = toml::from_str::<MuConfig>("").unwrap();
        assert_eq!((d.theme.source.as_str(), d.theme.accent, d.theme.cards), ("tokyo-night", None, Cards::Filled));
    }

    #[test]
    fn retry_policy_resolves_per_provider_and_decides_by_context() {
        use std::time::Duration;
        let c = toml::from_str::<MuConfig>(
            "[retry]\nmax_wait = \"10m\"\n[retry.providers.anthropic]\nmax_wait = \"3h\"\ncontext_below = 40\n",
        )
        .unwrap();
        let a = c.retry.policy("anthropic");
        assert_eq!((a.max_wait, a.context_below), (Duration::from_secs(3 * 3600), Some(40.0)));
        assert_eq!(c.retry.policy("openai").max_wait, Duration::from_secs(600));
        assert_eq!(MuConfig::default().retry.policy("x"), RetryPolicy::default());
        // Either condition retries: a long wait with a small context, or a long context with a short wait.
        assert_eq!(a.limit(10.0), Duration::MAX);
        assert_eq!(a.limit(60.0), Duration::from_secs(3 * 3600));
        assert_eq!(RetryPolicy::default().limit(0.0), DEFAULT_MAX_WAIT);
    }

    #[test]
    fn status_thresholds_default_and_reject_bad_values() {
        let d = toml::from_str::<MuConfig>("").unwrap().status;
        assert_eq!((d.warn_tokens, d.err_tokens, d.warn_percent, d.err_percent), (270_000, 500_000, 50.0, 70.0));
        let mut s = toml::from_str::<MuStatus>("warn_tokens = 100000\nerr_percent = 150\nerr_tokens = 0").unwrap();
        let mut w = Vec::new();
        check_status(&mut s, &mut w);
        assert_eq!(s, MuStatus { warn_tokens: 100_000, ..MuStatus::default() });
        assert_eq!(w.len(), 2, "{w:?}");
    }

    #[test]
    fn task_budget_defaults_and_overrides() {
        let d = toml::from_str::<MuConfig>("").unwrap().task;
        assert_eq!((d.soft_request_budget, d.budget_notice), (200, true));
        assert!(d.launch.is_empty() && d.remote.is_none());
        assert_eq!(d.reconnect_timeout, 600);
        assert_eq!(d.max_cost, 5.0);
        assert_eq!(toml::from_str::<MuConfig>("[task]\nmax_cost = 0").unwrap().task.max_cost, 0.0);
        let t = toml::from_str::<MuConfig>("[task]\nlaunch = [\"task\"]\nremote = \"c\"\nreconnect_timeout = 30")
            .unwrap()
            .task;
        assert_eq!(
            (t.launch.as_slice(), t.remote.as_deref(), t.reconnect_timeout),
            (&["task".to_string()][..], Some("c"), 30)
        );
        let t = toml::from_str::<MuConfig>("[task]\nsoft_request_budget = 50\nbudget_notice = false").unwrap().task;
        assert_eq!((t.soft_request_budget, t.budget_notice), (50, false));
    }

    fn remotes(text: &str) -> (MuConfig, Vec<String>) {
        let mut c = toml::from_str::<MuConfig>(text).unwrap();
        let mut w = Vec::new();
        let table: toml::Table = text.parse().unwrap();
        c.remote = load_remotes(table.get("remote").cloned().unwrap_or(toml::Value::Table(Default::default())), &mut w);
        (c, w)
    }

    #[test]
    fn remotes_parse_with_defaults() {
        let (c, w) = remotes(
            "default_remote = \"cluster\"\n\
             [remote.cluster]\nadapter = \"pod\"\ntrusted = true\n\
             [remote.cluster.settings]\nnamespace = \"ns\"\nsccache_secret_name = \"s\"\n\
             [remote.devbox]\nadapter = \"ssh\"\nhost = \"devbox\"\nrepos = [\"/src/a\"]\n",
        );
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(c.default_remote.as_deref(), Some("cluster"));
        let cluster = &c.remote["cluster"];
        assert!(cluster.trusted);
        assert_eq!(cluster.settings_json(), r#"{"namespace":"ns","sccache_secret_name":"s"}"#);
        let devbox = &c.remote["devbox"];
        assert!(!devbox.trusted);
        assert_eq!(
            (devbox.host.as_deref(), devbox.repos.as_slice()),
            (Some("devbox"), ["/src/a".to_string()].as_slice())
        );
        assert_eq!(devbox.settings_json(), "{}");
        assert!(MuConfig::default().remote.is_empty() && MuConfig::default().default_remote.is_none());
    }

    #[test]
    fn invalid_remotes_are_dropped_with_a_reason() {
        let (c, w) = remotes(
            "[remote.a]\n[remote.b]\nadapter = \"ssh\"\n[remote.c]\nadapter = \"ssh\"\nhost = \"-oProxyCommand=x\"\n\
             [remote.d]\nadapter = \"pod\"\n[remote.d.settings.auth]\nApi_Key = \"x\"\n\
             [remote.e]\nadapter = \"pod\"\n[remote.e.settings]\ntoken = \"x\"\n\
             [remote.f]\nadapter = \"pod\"\n[remote.f.settings]\nkubeconfig_path = \"/x\"\n",
        );
        assert_eq!(c.remote.keys().collect::<Vec<_>>(), ["f"]);
        assert_eq!(w.len(), 5, "{w:?}");
        assert!(w[0].contains("needs an adapter"), "{w:?}");
        assert!(w[1].contains("needs a host"), "{w:?}");
        assert!(w[2].contains("must not start with -"), "{w:?}");
        assert!(w[3].contains("remote.d.settings.auth.Api_Key") && w[3].contains("secret"), "{w:?}");
        assert!(w[4].contains("remote.e.settings.token"), "{w:?}");
    }

    #[test]
    fn bell_defaults_on_and_switches_off_per_trigger() {
        assert_eq!(toml::from_str::<MuConfig>("").unwrap().bell, MuBell::default());
        let b = toml::from_str::<MuConfig>("[bell]\nturn_end = false").unwrap().bell;
        assert_eq!(b, MuBell { turn_end: false, attention: true });
    }

    #[test]
    fn the_removed_wait_sections_warn_and_other_sections_still_load() {
        let path = mu_testutil::home().join("config.toml");
        let text = "[bash]\nwait_sleep_secs = 0\n[experimental]\ninterruptible_wait = true\n\
                    [ask]\nnudge = false\n[bell]\nattention = false\n";
        std::fs::write(&path, text).unwrap();
        let mut warnings = Vec::new();
        let config = load_mu(&mut warnings);
        assert!(!config.ask.nudge && !config.bell.attention, "other sections still load");
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(warnings.iter().any(|w| w.contains("[bash] is no longer read")), "{warnings:?}");
        assert!(warnings.iter().any(|w| w.contains("[experimental] is no longer read")), "{warnings:?}");
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn permissions_default_to_yolo_and_warn_on_bad_values() {
        let default = load_permissions(&toml::Value::Table(Default::default()), &mut Vec::new());
        assert_eq!(default.mode, PermissionMode::Yolo);
        assert!(default.readonly_bash);
        assert!(default.rules.is_empty());

        let mut w = Vec::new();
        let text: toml::Table =
            "mode = \"nope\"\nreadonly_bash = 3\n[rules]\nbash = { \"git push *\" = \"deny\" }".parse().unwrap();
        let p = load_permissions(&toml::Value::Table(text), &mut w);
        assert_eq!(p.mode, PermissionMode::Ask, "a bad mode falls back to ask");
        assert!(p.readonly_bash, "a bad readonly_bash falls back to true");
        assert_eq!(p.rules.0.len(), 1);
        assert_eq!(w.len(), 2, "{w:?}");
        assert!(w[0].contains("mode"));

        let mut w = Vec::new();
        let text: toml::Table = "[rules]\nbash = \"push\"".parse().unwrap();
        let p = load_permissions(&toml::Value::Table(text), &mut w);
        assert!(p.rules.is_empty(), "an unknown action never becomes allow");
        assert_eq!(w.len(), 1);

        let mut w = Vec::new();
        let text: toml::Table = "[rules]\n\"mcp__github_*\" = \"allow\"".parse().unwrap();
        let p = load_permissions(&toml::Value::Table(text), &mut w);
        assert!(w.is_empty());
        assert_eq!(p.rules.0[0].1.pattern, "*", "a string value means every call of the tool");
    }

    #[test]
    fn durations_are_groups_of_a_number_and_one_unit() {
        use std::time::Duration;
        assert_eq!(parse_duration("90s"), Some(Duration::from_secs(90)));
        assert_eq!(parse_duration("30m"), Some(Duration::from_secs(1800)));
        assert_eq!(parse_duration(" 2h "), Some(Duration::from_secs(7200)));
        assert_eq!(parse_duration("1d"), Some(Duration::from_secs(86400)));
        assert_eq!(parse_duration("1h30m"), Some(Duration::from_secs(5400)));
        assert_eq!(parse_duration("2d3h"), Some(Duration::from_secs(183_600)));
        assert_eq!(parse_duration("1s1s"), Some(Duration::from_secs(2)));
        for bad in ["", "  ", "5", "m", "1.5h", "-1m", "2x", "5é", "1h30", "1h 30m", "h1", "1h-30m"] {
            assert_eq!(parse_duration(bad), None, "{bad}");
        }
        assert_eq!(parse_duration("18446744073709551615d"), None, "an overflowing count is not a duration");
        let r = toml::from_str::<MuRetry>("max_wait = \"soon\"\ncontext_below = 150\n[providers.x]\nmax_wait = \"1h\"")
            .unwrap();
        let mut w = Vec::new();
        check_retry(&r, &mut w);
        assert_eq!(w.len(), 2, "{w:?}");
        assert_eq!(r.policy("y").max_wait, DEFAULT_MAX_WAIT);
    }

    #[test]
    fn max_attempts_is_kept_between_1_and_20() {
        let kept = toml::from_str::<MuRetry>("max_attempts = 7").unwrap();
        let mut w = Vec::new();
        check_retry(&kept, &mut w);
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(kept.max_attempts(), 7);
        assert_eq!(MuRetry::default().max_attempts(), DEFAULT_MAX_ATTEMPTS);

        for bad in [0, 21] {
            let r = toml::from_str::<MuRetry>(&format!("max_attempts = {bad}")).unwrap();
            let mut w = Vec::new();
            check_retry(&r, &mut w);
            assert_eq!(
                w,
                [format!("config.toml: [retry] max_attempts {bad} is not between 1 and 20; using the default")]
            );
            assert_eq!(r.max_attempts(), DEFAULT_MAX_ATTEMPTS);
        }
    }

    #[test]
    fn realistic_configs() {
        let omp: OmpConfig = serde_yaml_ng::from_str("unknown: {a: 1}\nskills:\n  enableClaudeUser: true\n").unwrap();
        assert!(omp.skills.enable_claude_user);
        let cfg = Configs::from_models_toml(
            "[compaction.thresholds]\n\"anthropic/claude-haiku-5-5\" = \"f20000\"\n\"openai/*\" = \"50%\"\nbig = 800000\n",
        );
        let t = cfg.thresholds();
        assert_eq!(t["anthropic/claude-haiku-5-5"], Threshold::Fixed(20000));
        assert_eq!(t["openai/*"], Threshold::Percent(50));
        assert_eq!(t["big"], Threshold::Window(800000));
    }

    #[test]
    fn display_flags() {
        let omp: OmpConfig =
            serde_yaml_ng::from_str("hideThinkingBlock: true\ndisplay:\n  smoothStreaming: false\n").unwrap();
        assert!(omp.hide_thinking_block);
        assert!(!omp.display.smooth_streaming);
        let omp: OmpConfig = serde_yaml_ng::from_str("{}").unwrap();
        assert!(!omp.hide_thinking_block);
        assert!(omp.display.smooth_streaming);
    }

    #[test]
    fn thresholds_are_clamped_not_truncated() {
        let mut w = Vec::new();
        let big = toml::Value::Integer(5_000_000_000);
        assert_eq!(Threshold::from_toml(&big, "m", &mut w), Some(Threshold::Window(MAX_THRESHOLD)));
        assert_eq!(w.len(), 1);
        assert_eq!(Threshold::from_toml(&toml::Value::Integer(-5), "m", &mut w), None);
        assert_eq!(Threshold::from_str("f999999999999", "m", &mut w), Some(Threshold::Fixed(MAX_THRESHOLD)));
    }

    #[test]
    fn one_bad_section_does_not_drop_the_rest() {
        let doc: serde_yaml_ng::Mapping = serde_yaml_ng::from_str("composer: {shape: box}\nskills: 7\n").unwrap();
        let mut w = Vec::new();
        let composer: OmpComposer = yaml_section(&doc, "composer", "config.yml", &mut w);
        let skills: OmpSkills = yaml_section(&doc, "skills", "config.yml", &mut w);
        assert_eq!(composer.shape.as_deref(), Some("box"));
        assert!(!skills.enable_claude_user);
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("skills"));
    }
}
