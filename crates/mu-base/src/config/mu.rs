//! mu's own `config.toml` sections (`[retry]` is in `retry.rs`).

use super::retry::MuRetry;
use crate::permissions::{PermissionMode, RuleSet};
use serde::Deserialize;

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
    pub task: MuTask,
    /// `[permissions]`, parsed by `load_permissions` rather than serde.
    #[serde(skip)]
    pub permissions: MuPermissions,
    pub bell: MuBell,
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

pub(super) fn load_permissions(v: &toml::Value, warn: &mut Vec<String>) -> MuPermissions {
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
                None => {
                    p.mode = PermissionMode::Ask;
                    warn.push(format!(
                        "config.toml: [permissions] mode \"{}\" is not ask, edits or yolo; using ask",
                        s.map_or_else(|| v.to_string(), str::to_string)
                    ));
                }
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

/// `[bell]`: which events ring the terminal. `turn_end`: a turn you typed ends with no background
/// command or subagent running. `attention`: a running turn needs you (`ask`, an approval, plan
/// review). omp's `completion.notify = off` silences both.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(default)]
pub struct MuBell {
    pub turn_end: bool,
    pub attention: bool,
}

impl Default for MuBell {
    fn default() -> Self {
        Self { turn_end: true, attention: true }
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
pub(super) fn check_status(s: &mut MuStatus, warn: &mut Vec<String>) {
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

/// `[task]`: a subagent's soft request budget. Crossing it steers a wrap-up notice (`budget_notice`);
/// at 1.5x the run is stopped and asked for its partial result; 0 turns the guard off. A `scout` is
/// capped at 100 however high the budget is, as in omp. `launcher` is a command (program and
/// arguments) run in place of `mu --headless` for every `task` subagent; empty spawns locally.
#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct MuTask {
    pub soft_request_budget: u32,
    pub budget_notice: bool,
    pub launcher: Vec<String>,
}

impl Default for MuTask {
    fn default() -> Self {
        Self { soft_request_budget: 200, budget_notice: true, launcher: Vec::new() }
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
    /// Also learn from omp's and Claude Code's prompt histories; read by the predict plugin.
    pub other_histories: bool,
}

impl Default for MuPredict {
    fn default() -> Self {
        Self { enabled: true, threshold: 0.3, engine: "ngram".into(), threads: 4, other_histories: true }
    }
}

/// Warns about a `[predict]` value the plugin would not understand; the section itself is kept.
pub(super) fn check_predict(p: &MuPredict, warn: &mut Vec<String>) {
    if !matches!(p.engine.as_str(), "ngram" | "smollm") {
        warn.push(format!("config.toml: [predict] engine {:?} is not \"ngram\" or \"smollm\"; using ngram", p.engine));
    }
}
