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

impl Threshold {
    fn from_yaml(v: &serde_yaml_ng::Value) -> Option<Self> {
        match v {
            serde_yaml_ng::Value::Number(n) => n.as_u64().map(|n| Self::Window(n as u32)),
            serde_yaml_ng::Value::String(s) => Self::from_str(s),
            _ => None,
        }
    }

    fn from_toml(v: &toml::Value) -> Option<Self> {
        match v {
            toml::Value::Integer(n) if *n >= 0 => Some(Self::Window(*n as u32)),
            toml::Value::String(s) => Self::from_str(s),
            _ => None,
        }
    }

    fn from_str(s: &str) -> Option<Self> {
        let s = s.trim();
        if let Some(p) = s.strip_suffix('%') {
            return p.trim().parse().ok().map(Self::Percent);
        }
        if let Some(n) = s.strip_prefix('f') {
            return n.trim().parse().ok().map(Self::Fixed);
        }
        s.parse().ok().map(Self::Window)
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct OmpConfig {
    pub model_roles: BTreeMap<String, String>,
    pub compaction: OmpCompaction,
    pub skills: OmpSkills,
    pub theme: OmpTheme,
    pub status_line: OmpStatusLine,
    pub composer: OmpComposer,
    pub symbol_preset: Option<String>,
    #[allow(dead_code)]
    pub mnemopi: Option<serde_yaml_ng::Value>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct OmpTheme {
    pub dark: Option<String>,
    pub light: Option<String>,
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
}

/// Sets `role: value` under the top-level `modelRoles:` block, leaving the rest of the file untouched.
fn rewrite_role(text: &str, role: &str, value: &str) -> Result<String, String> {
    let mut lines: Vec<String> = text.lines().map(String::from).collect();
    let Some(head) = lines.iter().position(|l| l.starts_with("modelRoles:")) else {
        let mut out = text.to_string();
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&format!("modelRoles:\n  {role}: {value}\n"));
        return Ok(out);
    };
    if !lines[head]["modelRoles:".len()..].trim().is_empty() {
        return Err("modelRoles is written inline in config.yml; edit it by hand".into());
    }
    let end = lines[head + 1..].iter().position(|l| !l.is_empty() && !l.starts_with(' ')).map_or(lines.len(), |i| head + 1 + i);
    let indent = lines[head + 1..end]
        .iter()
        .find(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'))
        .map_or("  ".to_string(), |l| l[..l.len() - l.trim_start().len()].to_string());
    let key = format!("{indent}{role}:");
    match lines[head + 1..end].iter().position(|l| l.starts_with(&key)) {
        Some(i) => lines[head + 1 + i] = format!("{key} {value}"),
        None => {
            let last = lines[..end].iter().rposition(|l| !l.trim().is_empty()).unwrap_or(head);
            lines.insert(last + 1, format!("{key} {value}"));
        }
    }
    let mut out = lines.join("\n");
    if text.ends_with('\n') {
        out.push('\n');
    }
    Ok(out)
}

fn read_yaml<T: Default + for<'de> Deserialize<'de>>(name: &str) -> T {
    let p = paths::omp_agent_dir().join(name);
    let Ok(text) = std::fs::read_to_string(&p) else { return T::default() };
    serde_yaml_ng::from_str(&text).unwrap_or_else(|e| {
        eprintln!("warning: {}: {e}", p.display());
        T::default()
    })
}

impl Configs {
    /// Configured `provider/model[:level]` for a role.
    pub fn role(&self, name: &str) -> Option<String> {
        self.role_edits.borrow().get(name).or_else(|| self.omp.model_roles.get(name)).cloned()
    }

    /// Points `name` at `model` in omp's `config.yml`, keeping the role's existing `:level`.
    pub fn set_role(&self, name: &str, model: &str) -> Result<String, String> {
        let level = self.role(name).and_then(|v| v.rsplit_once(':').map(|(_, l)| l.to_string()).filter(|l| !l.contains('/')));
        let value = match level {
            Some(l) => format!("{model}:{l}"),
            None => model.to_string(),
        };
        let path = paths::omp_agent_dir().join("config.yml");
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        let out = rewrite_role(&text, name, &value)?;
        std::fs::write(&path, out).map_err(|e| format!("{}: {e}", path.display()))?;
        self.role_edits.borrow_mut().insert(name.to_string(), value.clone());
        Ok(value)
    }

    pub fn load() -> Self {
        let p = paths::mu_home().join("config.toml");
        let mu = match std::fs::read_to_string(&p) {
            Ok(t) => toml::from_str(&t).unwrap_or_else(|e| {
                eprintln!("warning: {}: {e}", p.display());
                MuConfig::default()
            }),
            Err(_) => MuConfig::default(),
        };
        Self { omp: read_yaml("config.yml"), models: read_yaml("models.yml"), mu, role_edits: Default::default() }
    }

    /// omp's thresholds overlaid per key by mu's.
    pub fn thresholds(&self) -> BTreeMap<String, Threshold> {
        let mut out: BTreeMap<String, Threshold> = self
            .omp
            .compaction
            .model_thresholds
            .iter()
            .filter_map(|(k, v)| Some((k.clone(), Threshold::from_yaml(v)?)))
            .collect();
        for (k, v) in &self.mu.compaction.model_thresholds {
            if let Some(t) = Threshold::from_toml(v) {
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
    fn rewrite_role_edits_in_place_or_appends() {
        let t = "a: 1\nmodelRoles:\n  default: x/y:high\n  smol: s\n\ntheme:\n  dark: d\n";
        assert_eq!(rewrite_role(t, "smol", "n/m").unwrap(), "a: 1\nmodelRoles:\n  default: x/y:high\n  smol: n/m\n\ntheme:\n  dark: d\n");
        assert_eq!(rewrite_role(t, "plan", "p/q").unwrap(), "a: 1\nmodelRoles:\n  default: x/y:high\n  smol: s\n  plan: p/q\n\ntheme:\n  dark: d\n");
        assert_eq!(rewrite_role("a: 1\n", "plan", "p/q").unwrap(), "a: 1\nmodelRoles:\n  plan: p/q\n");
        assert!(rewrite_role("modelRoles: {}\n", "plan", "p").is_err());
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
        let mu: MuConfig = toml::from_str("[compaction.model_thresholds]\n\"anthropic/claude-haiku-5-5\" = \"f20000\"\n").unwrap();
        let cfg = Configs { omp, models, mu, role_edits: Default::default() };
        let t = cfg.thresholds();
        assert_eq!(t["anthropic/claude-haiku-5-5"], Threshold::Fixed(20000));
        assert_eq!(t["openai/*"], Threshold::Percent(50));
    }
}
