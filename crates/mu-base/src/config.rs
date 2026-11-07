use crate::model_config::{self, Import, ModelConfig, ModelLimits};
use crate::permissions::{PermissionMode, RuleSet};
use mu_common::paths;
use serde::Deserialize;
use std::cell::RefCell;
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

/// omp's `completion.notify` (`on`/`off`, default on): ring the terminal when a turn nobody typed
/// finishes. mu applies it only to those woken turns.
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

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct MuConfig {
    pub predict: MuPredict,
    pub ask: MuAsk,
    pub theme: MuTheme,
    pub update: MuUpdate,
    pub edit: MuEdit,
    pub retry: MuRetry,
    pub mcp: MuMcp,
    pub secrets: MuSecrets,
    pub status: MuStatus,
    pub bash: MuBash,
    /// `[permissions]`, parsed by `load_permissions` rather than serde.
    #[serde(skip)]
    pub permissions: MuPermissions,
}

/// `[permissions]`: which tool calls may run without asking, and the rules that override it.
#[derive(Debug, Clone)]
pub struct MuPermissions {
    pub mode: PermissionMode,
    /// The built-in read-only command set counts as tier read.
    pub readonly_bash: bool,
    pub rules: RuleSet,
}

impl Default for MuPermissions {
    fn default() -> Self {
        Self { mode: PermissionMode::Yolo, readonly_bash: true, rules: RuleSet::default() }
    }
}

fn load_permissions(v: &toml::Value, warn: &mut Vec<String>) -> MuPermissions {
    let mut p = MuPermissions::default();
    let Some(t) = v.as_table() else {
        warn.push("config.toml: [permissions] is not a table; ignored".to_string());
        return p;
    };
    match t.get("mode") {
        None => {}
        Some(v) => {
            let s = v.as_str();
            match s.and_then(PermissionMode::parse) {
                Some(m) => p.mode = m,
                None => warn.push(format!(
                    "config.toml: [permissions] mode \"{}\" is not ask, edits or yolo; using yolo",
                    s.map_or_else(|| v.to_string(), str::to_string)
                )),
            }
        }
    }
    match t.get("readonly_bash") {
        None => {}
        Some(toml::Value::Boolean(b)) => p.readonly_bash = *b,
        Some(v) => warn.push(format!("config.toml: [permissions] readonly_bash {v} is not true or false; using true")),
    }
    match t.get("rules") {
        None => {}
        Some(v) => match v.as_table() {
            Some(table) => p.rules = RuleSet::parse(table, "config.toml", warn),
            None => warn.push("config.toml: [permissions] rules is not a table; ignored".to_string()),
        },
    }
    p
}

/// `[bash]`: while a background job runs, a model's command that starts with a sleep of at least
/// `wait_sleep_secs` is not run and the turn ends (0 = only commands that do nothing but wait).
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(default)]
pub struct MuBash {
    pub wait_sleep_secs: f64,
}

impl Default for MuBash {
    fn default() -> Self {
        Self { wait_sleep_secs: 10.0 }
    }
}

fn check_bash(b: &mut MuBash, warn: &mut Vec<String>) {
    let default = MuBash::default().wait_sleep_secs;
    if !(b.wait_sleep_secs.is_finite() && b.wait_sleep_secs >= 0.0) {
        warn.push(format!(
            "config.toml: [bash] wait_sleep_secs {} is not 0 or more; using {default}",
            b.wait_sleep_secs
        ));
        b.wait_sleep_secs = default;
    }
}

/// `[status]`: when the footer's context share turns warning or error colour. A level is reached at
/// its percent of the window or its token count, whichever is lower.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(default)]
pub struct MuStatus {
    pub warn_percent: f64,
    pub warn_tokens: u64,
    pub err_percent: f64,
    pub err_tokens: u64,
}

impl Default for MuStatus {
    fn default() -> Self {
        Self { warn_percent: 50.0, warn_tokens: 270_000, err_percent: 70.0, err_tokens: 500_000 }
    }
}

/// Puts back the default for a `[status]` value that is not a percentage above 0 and up to 100, or
/// a token count of at least 1.
fn check_status(s: &mut MuStatus, warn: &mut Vec<String>) {
    let d = MuStatus::default();
    for (key, v, default) in
        [("warn_percent", &mut s.warn_percent, d.warn_percent), ("err_percent", &mut s.err_percent, d.err_percent)]
    {
        if !(v.is_finite() && *v > 0.0 && *v <= 100.0) {
            warn.push(format!(
                "config.toml: [status] {key} {v} is not a percentage above 0 and up to 100; using {default}"
            ));
            *v = default;
        }
    }
    for (key, v, default) in
        [("warn_tokens", &mut s.warn_tokens, d.warn_tokens), ("err_tokens", &mut s.err_tokens, d.err_tokens)]
    {
        if *v == 0 {
            warn.push(format!("config.toml: [status] {key} must be at least 1; using {default}"));
            *v = default;
        }
    }
}

/// `[ask]`: `nudge = false` turns off the reminder to use `ask` when a turn ends with a decision question in prose.
#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct MuAsk {
    pub nudge: bool,
}

impl Default for MuAsk {
    fn default() -> Self {
        Self { nudge: true }
    }
}

/// `[mcp]`: `lifetime` of MCP server processes (`session`, `idle:10m`, `call`); default `session`.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct MuMcp {
    pub lifetime: String,
}

/// `[edit]`: `mode = "hashline"` (default) or `"sloppy"` (find/replace by text, no tags).
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct MuEdit {
    pub mode: String,
}

/// How long a retryable error with a `Retry-After` is waited out when it is a request's turn to be
/// retried: `Retry-After` up to this is waited for, anything longer fails at once.
pub const DEFAULT_MAX_WAIT: std::time::Duration = std::time::Duration::from_secs(120);

/// Attempts per request, retries included, when `[retry] max_attempts` is not set.
pub const DEFAULT_MAX_ATTEMPTS: u32 = 5;

/// `[retry]`: when to wait out a provider's `Retry-After` (a 429, mostly). A request is retried when
/// the wait is at most `max_wait` (`"90s"`, `"30m"`, `"2h"`, `"1d"`; default `"2m"`) or when its
/// context is less than `context_below` percent full (default: never). `[retry.providers.<id>]`
/// overrides both for one provider. `max_attempts` (1 to 20, default 5) bounds the attempts per request.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct MuRetry {
    pub max_wait: Option<String>,
    pub context_below: Option<f64>,
    pub max_attempts: Option<u32>,
    pub providers: BTreeMap<String, RetryRule>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct RetryRule {
    pub max_wait: Option<String>,
    pub context_below: Option<f64>,
}

/// A `[retry]` section resolved for one provider.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RetryPolicy {
    pub max_wait: std::time::Duration,
    /// Percent of the context window.
    pub context_below: Option<f64>,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self { max_wait: DEFAULT_MAX_WAIT, context_below: None }
    }
}

impl RetryPolicy {
    /// The longest `Retry-After` worth waiting for a request whose context is `fill` percent full.
    pub fn limit(&self, fill: f64) -> std::time::Duration {
        match self.context_below {
            Some(below) if fill < below => std::time::Duration::MAX,
            _ => self.max_wait,
        }
    }
}

impl MuRetry {
    /// Attempts per request: `max_attempts` when it is 1 to 20, else the default (as `check_retry` warns).
    pub fn max_attempts(&self) -> u32 {
        self.max_attempts.filter(|n| (1..=20).contains(n)).unwrap_or(DEFAULT_MAX_ATTEMPTS)
    }

    pub fn policy(&self, provider: &str) -> RetryPolicy {
        let own = self.providers.get(provider);
        let max_wait = own
            .and_then(|r| r.max_wait.as_deref())
            .or(self.max_wait.as_deref())
            .and_then(parse_duration)
            .unwrap_or(DEFAULT_MAX_WAIT);
        let context_below =
            own.and_then(|r| r.context_below).or(self.context_below).filter(|b| b.is_finite() && *b > 0.0);
        RetryPolicy { max_wait, context_below }
    }
}

/// `"90s"`, `"30m"`, `"2h"` or `"1d"`: a whole number and one unit.
pub fn parse_duration(s: &str) -> Option<std::time::Duration> {
    let s = s.trim();
    let unit = s.chars().last()?;
    let n: u64 = s[..s.len() - unit.len_utf8()].parse().ok()?;
    let secs = match unit {
        's' => 1,
        'm' => 60,
        'h' => 3600,
        'd' => 86400,
        _ => return None,
    };
    n.checked_mul(secs).map(std::time::Duration::from_secs)
}

fn check_retry(r: &MuRetry, warn: &mut Vec<String>) {
    if let Some(n) = r.max_attempts.filter(|n| !(1..=20).contains(n)) {
        warn.push(format!("config.toml: [retry] max_attempts {n} is not between 1 and 20; using the default"));
    }
    let rules = std::iter::once(("[retry]".to_string(), r.max_wait.as_deref(), r.context_below)).chain(
        r.providers.iter().map(|(p, v)| (format!("[retry.providers.{p}]"), v.max_wait.as_deref(), v.context_below)),
    );
    for (name, wait, below) in rules {
        if let Some(w) = wait.filter(|w| parse_duration(w).is_none()) {
            warn.push(format!(
                "config.toml: {name} max_wait {w:?} is not a number and a unit (s, m, h, d); using the default"
            ));
        }
        if let Some(b) = below.filter(|b| !(b.is_finite() && *b > 0.0 && *b <= 100.0)) {
            warn.push(format!("config.toml: {name} context_below {b} is not a percentage above 0 and up to 100"));
        }
    }
}

/// `[update]`: `check = false` turns off the daily release check at startup.
#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct MuUpdate {
    pub check: bool,
}

impl Default for MuUpdate {
    fn default() -> Self {
        Self { check: true }
    }
}

/// `[secrets]`: `env` names extra environment variables whose values count as secrets. Redaction
/// itself cannot be turned off.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct MuSecrets {
    pub env: Vec<String>,
}

/// `[theme]`: palette source, accent and card style.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct MuTheme {
    /// Bundled or ~/.mu/themes name | "terminal" | "omarchy" | path to a colors.toml. Default "tokyo-night".
    pub source: String,
    /// Palette key (e.g. "blue", "magenta", "accent") or "#rrggbb".
    pub accent: Option<String>,
    pub cards: Cards,
}

impl Default for MuTheme {
    fn default() -> Self {
        Self { source: "tokyo-night".into(), accent: None, cards: Cards::Filled }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Cards {
    /// Border only, on the terminal background.
    Outline,
    /// Border on the recessed surface (`dark_background`).
    #[default]
    Filled,
}

/// `[predict]`: ghost-text prediction in the composer. `engine` and `threads` are read by the predict plugin.
#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct MuPredict {
    pub enabled: bool,
    pub threshold: f32,
    /// `ngram` (default) or `smollm`; other values warn and mean `ngram`.
    pub engine: String,
    /// CPU threads for SmolLM; 0 means all cores.
    pub threads: usize,
}

impl Default for MuPredict {
    fn default() -> Self {
        Self { enabled: true, threshold: 0.3, engine: "ngram".into(), threads: 4 }
    }
}

/// Warns about a `[predict]` value the plugin would not understand; the section itself is kept.
fn check_predict(p: &MuPredict, warn: &mut Vec<String>) {
    if !matches!(p.engine.as_str(), "ngram" | "smollm") {
        warn.push(format!("config.toml: [predict] engine {:?} is not \"ngram\" or \"smollm\"; using ngram", p.engine));
    }
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
    let predict: MuPredict = parse(section("predict"), "predict", warn);
    check_predict(&predict, warn);
    let retry: MuRetry = parse(section("retry"), "retry", warn);
    check_retry(&retry, warn);
    let mut status: MuStatus = parse(section("status"), "status", warn);
    check_status(&mut status, warn);
    let mut bash: MuBash = parse(section("bash"), "bash", warn);
    check_bash(&mut bash, warn);
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
        bash,
        permissions: load_permissions(&section("permissions"), warn),
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

    pub fn model_limits(&self, id: &str) -> Option<ModelLimits> {
        self.models.borrow().models.get(id).copied()
    }

    /// Ids with limits in models.toml.
    pub fn listed_models(&self) -> Vec<String> {
        self.models.borrow().models.keys().cloned().collect()
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
    fn bash_wait_sleep_defaults_and_rejects_negatives() {
        assert_eq!(toml::from_str::<MuConfig>("").unwrap().bash, MuBash::default());
        let mut b = toml::from_str::<MuBash>("wait_sleep_secs = 0").unwrap();
        let mut w = Vec::new();
        check_bash(&mut b, &mut w);
        assert_eq!((b.wait_sleep_secs, w.len()), (0.0, 0));
        let mut b = toml::from_str::<MuBash>("wait_sleep_secs = -3").unwrap();
        check_bash(&mut b, &mut w);
        assert_eq!((b, w.len()), (MuBash::default(), 1));
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
        assert_eq!(p.mode, PermissionMode::Yolo, "a bad mode falls back to yolo");
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
    fn durations_need_a_number_and_one_unit() {
        use std::time::Duration;
        assert_eq!(parse_duration("90s"), Some(Duration::from_secs(90)));
        assert_eq!(parse_duration("30m"), Some(Duration::from_secs(1800)));
        assert_eq!(parse_duration(" 2h "), Some(Duration::from_secs(7200)));
        assert_eq!(parse_duration("1d"), Some(Duration::from_secs(86400)));
        for bad in ["", "5", "m", "1.5h", "-1m", "2x", "1h30m", "5é"] {
            assert_eq!(parse_duration(bad), None, "{bad}");
        }
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
