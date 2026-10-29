use crate::paths;
use serde::Deserialize;
use std::collections::BTreeMap;

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
pub const MAX_THRESHOLD: u32 = 100_000_000;

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
    fn from_yaml(v: &serde_yaml_ng::Value, key: &str, warn: &mut Vec<String>) -> Option<Self> {
        match v {
            serde_yaml_ng::Value::Number(n) => match n.as_u64() {
                Some(n) => Some(Self::Window(clamp_tokens(n, key, warn))),
                None => {
                    warn.push(format!("ignoring negative or fractional threshold for {key}"));
                    None
                }
            },
            serde_yaml_ng::Value::String(s) => Self::from_str(s, key, warn),
            _ => None,
        }
    }

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
    pub model_roles: BTreeMap<String, String>,
    pub cycle_order: Vec<String>,
    pub compaction: OmpCompaction,
    pub skills: OmpSkills,
    pub status_line: OmpStatusLine,
    pub composer: OmpComposer,
    pub symbol_preset: Option<String>,
    pub hide_thinking_block: bool,
    pub display: OmpDisplay,
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

impl OmpConfig {
    pub fn role_cycle(&self) -> Vec<String> {
        if self.cycle_order.is_empty() {
            ["smol", "default", "slow"].map(String::from).to_vec()
        } else {
            self.cycle_order.clone()
        }
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
pub struct OmpCompaction {
    pub model_thresholds: BTreeMap<String, serde_yaml_ng::Value>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct OmpSkills {
    pub enable_claude_user: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ModelsYml {
    pub providers: BTreeMap<String, ProviderCfg>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ProviderCfg {
    pub models: Vec<ModelCfg>,
    pub model_overrides: BTreeMap<String, ModelOverride>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ModelCfg {
    pub id: String,
    pub context_window: Option<u32>,
    pub max_tokens: Option<u32>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ModelOverride {
    pub context_window: Option<u32>,
    pub max_tokens: Option<u32>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct MuConfig {
    pub compaction: MuCompaction,
    pub predict: MuPredict,
    pub theme: MuTheme,
    pub update: MuUpdate,
    pub edit: MuEdit,
    pub mcp: MuMcp,
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

/// `[predict]`: ghost-text prediction in the composer.
#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct MuPredict {
    pub enabled: bool,
    pub threshold: f32,
}

impl Default for MuPredict {
    fn default() -> Self {
        Self { enabled: true, threshold: 0.3 }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct MuCompaction {
    pub model_thresholds: BTreeMap<String, toml::Value>,
}

#[derive(Debug, Default)]
pub struct Configs {
    pub omp: OmpConfig,
    pub models: ModelsYml,
    pub mu: MuConfig,
    /// Role assignments made this session; shadow `omp.model_roles`.
    pub role_edits: std::cell::RefCell<BTreeMap<String, String>>,
    /// Problems found while loading; shown as notices at startup.
    pub warnings: Vec<String>,
}

/// Sets `role: value` under the top-level `modelRoles:` block, leaving the rest of the file untouched.
/// Refuses anything but a single-line plain value, and keeps the file's line endings.
fn rewrite_role(text: &str, role: &str, value: &str) -> Result<String, String> {
    let eol = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let mut lines: Vec<String> = text.lines().map(String::from).collect();
    let hand = format!("edit config.yml by hand: role {role} is not a single-line value");
    // The role's current value must be a plain scalar if it exists at all.
    if let Ok(doc) = serde_yaml_ng::from_str::<serde_yaml_ng::Value>(text)
        && let Some(cur) = doc.get("modelRoles").and_then(|m| m.get(role))
        && !matches!(cur, serde_yaml_ng::Value::String(_) | serde_yaml_ng::Value::Null)
    {
        return Err(hand);
    }
    let Some(head) = lines.iter().position(|l| l.starts_with("modelRoles:")) else {
        let mut out = text.to_string();
        if !out.is_empty() && !out.ends_with('\n') {
            out.push_str(eol);
        }
        out.push_str(&format!("modelRoles:{eol}  {role}: {value}{eol}"));
        return Ok(out);
    };
    if !lines[head]["modelRoles:".len()..].trim().is_empty() {
        return Err("modelRoles is written inline in config.yml; edit it by hand".into());
    }
    let end = lines[head + 1..]
        .iter()
        .position(|l| !l.is_empty() && !l.starts_with(' '))
        .map_or(lines.len(), |i| head + 1 + i);
    let indent = lines[head + 1..end]
        .iter()
        .find(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'))
        .map_or("  ".to_string(), |l| l[..l.len() - l.trim_start().len()].to_string());
    let key = format!("{indent}{role}:");
    match lines[head + 1..end].iter().position(|l| l.starts_with(&key)) {
        Some(i) => {
            let at = head + 1 + i;
            let rest = lines[at][key.len()..].trim();
            let block = rest.is_empty()
                || rest.starts_with(['|', '>', '[', '{', '&', '*', '!'])
                || lines[at + 1..end].first().is_some_and(|n| {
                    !n.trim().is_empty()
                        && n.len() - n.trim_start().len() > indent.len()
                        && !n.trim_start().starts_with('#')
                });
            if block {
                return Err(hand);
            }
            lines[at] = format!("{key} {value}");
        }
        None => {
            let last = lines[..end].iter().rposition(|l| !l.trim().is_empty()).unwrap_or(head);
            lines.insert(last + 1, format!("{key} {value}"));
        }
    }
    let mut out = lines.join(eol);
    if text.ends_with('\n') {
        out.push_str(eol);
    }
    Ok(out)
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
        model_roles: yaml_section(&d, "modelRoles", f, warn),
        cycle_order: yaml_section(&d, "cycleOrder", f, warn),
        compaction: yaml_section(&d, "compaction", f, warn),
        skills: yaml_section(&d, "skills", f, warn),
        status_line: yaml_section(&d, "statusLine", f, warn),
        composer: yaml_section(&d, "composer", f, warn),
        symbol_preset: yaml_section(&d, "symbolPreset", f, warn),
        hide_thinking_block: yaml_section(&d, "hideThinkingBlock", f, warn),
        display: yaml_section(&d, "display", f, warn),
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
    MuConfig {
        compaction: parse(section("compaction"), "compaction", warn),
        predict: parse(section("predict"), "predict", warn),
        theme: parse(section("theme"), "theme", warn),
        update: parse(section("update"), "update", warn),
        edit: parse(section("edit"), "edit", warn),
        mcp: parse(section("mcp"), "mcp", warn),
    }
}

impl Configs {
    /// Configured `provider/model[:level]` for a role.
    pub fn role(&self, name: &str) -> Option<String> {
        self.role_edits.borrow().get(name).or_else(|| self.omp.model_roles.get(name)).cloned()
    }

    /// Points `name` at `model` in omp's `config.yml`, keeping the role's existing `:level`.
    pub fn set_role(&self, name: &str, model: &str) -> Result<String, String> {
        let level =
            self.role(name).and_then(|v| v.rsplit_once(':').map(|(_, l)| l.to_string()).filter(|l| !l.contains('/')));
        let value = match level {
            Some(l) => format!("{model}:{l}"),
            None => model.to_string(),
        };
        let path = paths::omp_agent_dir().join("config.yml");
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
        };
        let out = rewrite_role(&text, name, &value)?;
        mu_common::fs::write_atomic(&path, out.as_bytes()).map_err(|e| format!("{}: {e}", path.display()))?;
        self.role_edits.borrow_mut().insert(name.to_string(), value.clone());
        Ok(value)
    }

    pub fn load() -> Self {
        let mut warnings = Vec::new();
        let mut mu = load_mu(&mut warnings);
        // `--theme` (via MU_THEME) overrides `[theme] source` for this run.
        if let Some(t) = std::env::var("MU_THEME").ok().filter(|t| !t.is_empty()) {
            mu.theme.source = t;
        }
        let omp = load_omp(&mut warnings);
        let models: ModelsYml = {
            let d = yaml_doc("models.yml", &mut warnings);
            ModelsYml { providers: yaml_section(&d, "providers", "models.yml", &mut warnings) }
        };
        let mut cfg = Self { omp, models, mu, role_edits: Default::default(), warnings };
        let mut w = Vec::new();
        cfg.thresholds_with(&mut w);
        cfg.warnings.extend(w);
        cfg
    }

    /// omp's thresholds overlaid per key by mu's.
    pub fn thresholds(&self) -> BTreeMap<String, Threshold> {
        self.thresholds_with(&mut Vec::new())
    }

    fn thresholds_with(&self, warn: &mut Vec<String>) -> BTreeMap<String, Threshold> {
        let mut out: BTreeMap<String, Threshold> = self
            .omp
            .compaction
            .model_thresholds
            .iter()
            .filter_map(|(k, v)| Some((k.clone(), Threshold::from_yaml(v, k, warn)?)))
            .collect();
        for (k, v) in &self.mu.compaction.model_thresholds {
            if let Some(t) = Threshold::from_toml(v, k, warn) {
                out.insert(k.clone(), t);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theme_section_parses_with_defaults() {
        let c = toml::from_str::<MuConfig>("[theme]\nsource = \"gruvbox\"\ncards = \"outline\"\n").unwrap();
        assert_eq!((c.theme.source.as_str(), c.theme.cards), ("gruvbox", Cards::Outline));
        let d = toml::from_str::<MuConfig>("").unwrap();
        assert_eq!((d.theme.source.as_str(), d.theme.accent, d.theme.cards), ("tokyo-night", None, Cards::Filled));
    }

    #[test]
    fn rewrite_role_edits_in_place_or_appends() {
        let t = "a: 1\nmodelRoles:\n  default: x/y:high\n  smol: s\n\ntheme:\n  dark: d\n";
        assert_eq!(
            rewrite_role(t, "smol", "n/m").unwrap(),
            "a: 1\nmodelRoles:\n  default: x/y:high\n  smol: n/m\n\ntheme:\n  dark: d\n"
        );
        assert_eq!(
            rewrite_role(t, "plan", "p/q").unwrap(),
            "a: 1\nmodelRoles:\n  default: x/y:high\n  smol: s\n  plan: p/q\n\ntheme:\n  dark: d\n"
        );
        assert_eq!(rewrite_role("a: 1\n", "plan", "p/q").unwrap(), "a: 1\nmodelRoles:\n  plan: p/q\n");
        assert!(rewrite_role("modelRoles: {}\n", "plan", "p").is_err());
    }

    #[test]
    fn role_cycle_default_and_configured() {
        let omp: OmpConfig = serde_yaml_ng::from_str("modelRoles: {}\n").unwrap();
        assert_eq!(omp.role_cycle(), ["smol", "default", "slow"]);
        let omp: OmpConfig = serde_yaml_ng::from_str("cycleOrder: [slow, default]\n").unwrap();
        assert_eq!(omp.role_cycle(), ["slow", "default"]);
    }

    #[test]
    fn realistic_configs() {
        let omp: OmpConfig = serde_yaml_ng::from_str(
            "modelRoles:\n  default: anthropic/claude-sonnet-5-5:medium\n  smol: anthropic/claude-haiku-5-5\nunknown: {a: 1}\n\
             skills:\n  enableClaudeUser: true\ncompaction:\n  modelThresholds:\n    anthropic/claude-haiku-5-5: 800000\n    \"openai/*\": 50%\n",
        )
        .unwrap();
        assert_eq!(omp.model_roles["smol"], "anthropic/claude-haiku-5-5");
        assert!(omp.skills.enable_claude_user);
        let models: ModelsYml = serde_yaml_ng::from_str(
            "providers:\n  modal:\n    models:\n      - id: a/b\n        contextWindow: 1048576\n        maxTokens: 65536\n  openai-codex:\n    modelOverrides:\n      gpt-5.6-terra:\n        contextWindow: 272000\n",
        )
        .unwrap();
        assert_eq!(models.providers["modal"].models[0].context_window, Some(1048576));
        assert_eq!(models.providers["openai-codex"].model_overrides["gpt-5.6-terra"].context_window, Some(272000));
        let mu: MuConfig =
            toml::from_str("[compaction.model_thresholds]\n\"anthropic/claude-haiku-5-5\" = \"f20000\"\n").unwrap();
        let cfg = Configs { omp, models, mu, ..Default::default() };
        let t = cfg.thresholds();
        assert_eq!(t["anthropic/claude-haiku-5-5"], Threshold::Fixed(20000));
        assert_eq!(t["openai/*"], Threshold::Percent(50));
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
    fn rewrite_role_refuses_multiline_values_and_keeps_crlf() {
        let block = "modelRoles:\n  smol: |\n    x\n";
        assert!(rewrite_role(block, "smol", "n/m").unwrap_err().contains("by hand"));
        let cont = "modelRoles:\n  smol: a\n    b\n";
        assert!(rewrite_role(cont, "smol", "n/m").is_err());
        let crlf = "a: 1\r\nmodelRoles:\r\n  smol: s\r\n";
        assert_eq!(rewrite_role(crlf, "smol", "n/m").unwrap(), "a: 1\r\nmodelRoles:\r\n  smol: n/m\r\n");
    }

    #[test]
    fn thresholds_are_clamped_not_truncated() {
        let mut w = Vec::new();
        let big = serde_yaml_ng::Value::Number(5_000_000_000u64.into());
        assert_eq!(Threshold::from_yaml(&big, "m", &mut w), Some(Threshold::Window(MAX_THRESHOLD)));
        assert_eq!(w.len(), 1);
        assert_eq!(Threshold::from_toml(&toml::Value::Integer(-5), "m", &mut w), None);
        assert_eq!(Threshold::from_str("f999999999999", "m", &mut w), Some(Threshold::Fixed(MAX_THRESHOLD)));
    }

    #[test]
    fn one_bad_section_does_not_drop_the_rest() {
        let doc: serde_yaml_ng::Mapping = serde_yaml_ng::from_str("modelRoles: {smol: a/b}\ncycleOrder: 7\n").unwrap();
        let mut w = Vec::new();
        let roles: BTreeMap<String, String> = yaml_section(&doc, "modelRoles", "config.yml", &mut w);
        let cycle: Vec<String> = yaml_section(&doc, "cycleOrder", "config.yml", &mut w);
        assert_eq!(roles["smol"], "a/b");
        assert!(cycle.is_empty());
        assert_eq!(w.len(), 1);
        assert!(w[0].contains("cycleOrder"));
    }
}
