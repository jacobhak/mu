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
    pub display: MuDisplay,
    pub update: MuUpdate,
    pub edit: MuEdit,
    pub retry: MuRetry,
    pub mcp: MuMcp,
    pub gateway: MuGateway,
    pub secrets: MuSecrets,
    pub status: MuStatus,
    pub task: MuTask,
    pub agent: MuAgent,
    pub sessions: MuSessions,
    pub keys: MuKeys,
    /// `[permissions]`, parsed by `load_permissions` rather than serde.
    #[serde(skip)]
    pub permissions: MuPermissions,
    pub bell: MuBell,
}

/// `[sessions]`: `sources` are commands that list agents on other machines for the Sessions tab.
/// Each prints JSON lines in the shape of `mu agent list --json`, plus `host` and `hint` (and
/// optionally `attach`, a command that opens the row); see docs/remote.md.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct MuSessions {
    pub sources: Vec<Vec<String>>,
}

/// `[keys.<context>]`: `action = "chord"` or `action = ["chord", ...]`. The UI owns the context and
/// action names and the chord syntax (docs/keys.md); this only carries the table.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(transparent)]
pub struct MuKeys(pub std::collections::BTreeMap<String, std::collections::BTreeMap<String, KeySpec>>);

/// One binding: a chord, or a list of them (empty: the action is unbound).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum KeySpec {
    One(String),
    Many(Vec<String>),
}

impl KeySpec {
    pub fn chords(&self) -> Vec<&str> {
        match self {
            Self::One(s) => vec![s.as_str()],
            Self::Many(v) => v.iter().map(String::as_str).collect(),
        }
    }
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
/// capped at 100 however high the budget is, as in omp. `launcher` is a command (argv) that runs
/// the agent kinds listed in `launch` (`task`, `scout`, a custom agent's name) in place of
/// `mu --headless`; any other kind, and every kind when either is empty, runs here. `max_cost` is
/// the most (USD) the model calls a launched worker makes through its parent may cost, per worker; 0
/// turns the cap off.
#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct MuTask {
    pub soft_request_budget: u32,
    pub budget_notice: bool,
    pub launcher: Vec<String>,
    pub launch: Vec<String>,
    pub max_cost: f64,
}

impl Default for MuTask {
    fn default() -> Self {
        Self { soft_request_budget: 200, budget_notice: true, launcher: Vec::new(), launch: Vec::new(), max_cost: 5.0 }
    }
}

/// `[agent]`: what `mu agent` does when a question or approval waits and no client is attached.
/// `notify_command` is run (under `sh -c`) with the session id and a one-line summary as `$1` and
/// `$2`; `approval_timeout` is the seconds after which a waiting approval is answered `Unavailable`
/// (a waiting question is cancelled); 0 (the default) waits for ever. `state_key = "ssh"` encrypts the
/// agent's credentials at rest under a key a client unlocks with its SSH key; `state_key_file` and
/// `state_key_command` give an unattended 32-byte key instead (the file wins), and imply encryption.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct MuAgent {
    pub notify_command: Option<String>,
    pub approval_timeout: u64,
    pub state_key: StateKey,
    pub state_key_file: Option<String>,
    pub state_key_command: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StateKey {
    #[default]
    Off,
    Ssh,
}

/// `[mcp]`: `lifetime` of MCP server processes (`session`, `idle:10m`, `call`); default `session`.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct MuMcp {
    pub lifetime: String,
}

/// `[gateway]`: `plugin` names the installed plugin that serves as the gateway; unset, the first
/// loaded plugin with `provider = "gateway"`.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct MuGateway {
    pub plugin: Option<String>,
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

/// `[display]`: settings that otherwise come from omp's `config.yml`.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default)]
pub struct MuDisplay {
    /// Hide the model's thinking blocks; unset follows omp's `hideThinkingBlock`.
    pub hide_thinking_block: Option<bool>,
    /// Write URLs in the assistant's replies as clickable terminal hyperlinks (OSC 8).
    pub hyperlinks: bool,
}

impl Default for MuDisplay {
    fn default() -> Self {
        Self { hide_thinking_block: None, hyperlinks: true }
    }
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
