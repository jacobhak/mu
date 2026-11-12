use crate::permissions::Access;
use serde::Deserialize;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lifetime {
    Session,
    Idle(Duration),
    Call,
}

impl Lifetime {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "session" => Some(Self::Session),
            "call" => Some(Self::Call),
            _ => {
                let d = s.strip_prefix("idle:")?;
                let (cut, _) = d.char_indices().next_back()?;
                let (n, unit) = d.split_at(cut);
                let n: u64 = n.parse().ok()?;
                let secs = match unit {
                    "s" => n,
                    "m" => n.saturating_mul(60),
                    "h" => n.saturating_mul(3600),
                    _ => return None,
                };
                Some(Self::Idle(Duration::from_secs(secs)))
            }
        }
    }

    pub fn label(&self) -> String {
        match self {
            Self::Session => "session".into(),
            Self::Call => "call".into(),
            Self::Idle(d) => format!("idle:{}s", d.as_secs()),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// Where the JSON schema lives. It is read and parsed the first time the tool definitions are
    /// built for a request, not at startup.
    pub schema_path: PathBuf,
    schema: std::cell::OnceCell<Result<Value, String>>,
    /// How the permission gate judges it: `Read` only when the manifest says its tier is `read`.
    pub access: Access,
}

impl ToolSpec {
    pub fn new(name: String, description: String, schema_path: PathBuf) -> Self {
        Self { name, description, schema_path, schema: Default::default(), access: Access::Exec }
    }

    pub fn with_access(mut self, access: Access) -> Self {
        self.access = access;
        self
    }

    /// The parsed schema (cached after the first call), or why it could not be read.
    pub fn schema(&self) -> &Result<Value, String> {
        self.schema.get_or_init(|| {
            std::fs::read_to_string(&self.schema_path)
                .map_err(|e| format!("{}: {e}", self.schema_path.display()))
                .and_then(|s| serde_json::from_str(&s).map_err(|e| format!("{}: {e}", self.schema_path.display())))
        })
    }
    /// A tool whose schema is already in hand (an MCP server's cached tool list).
    pub fn inline(name: String, description: String, schema: Value) -> Self {
        let s = Self::new(name, description, PathBuf::new());
        let _ = s.schema.set(Ok(schema));
        s
    }

    #[cfg(test)]
    pub fn with_schema(name: &str, schema: Value) -> Self {
        let s = Self::new(name.into(), String::new(), PathBuf::new());
        let _ = s.schema.set(Ok(schema));
        s
    }
}

#[derive(Debug, Clone)]
pub struct Manifest {
    pub name: String,
    pub command: Vec<String>,
    pub lifetime: Lifetime,
    pub hooks: Vec<String>,
    pub provider: Option<String>,
    pub tools: Vec<ToolSpec>,
    /// Limit on one native `tool/call`.
    pub timeout: Duration,
    pub dir: PathBuf,
}

#[derive(Deserialize)]
struct Raw {
    name: String,
    command: Vec<String>,
    #[serde(default)]
    lifetime: Option<String>,
    #[serde(default)]
    hooks: Vec<String>,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    tools: Vec<RawTool>,
    #[serde(default)]
    timeout: Option<String>,
}

#[derive(Deserialize)]
struct RawTool {
    name: String,
    description: String,
    schema: String,
    /// `"read"` for a tool that only reads, `"exec"` (the default) for one that may change things.
    #[serde(default)]
    tier: Option<String>,
}

fn load_one(dir: &Path) -> Result<Manifest, String> {
    let text = std::fs::read_to_string(dir.join("plugin.toml")).map_err(|e| e.to_string())?;
    let raw: Raw = toml::from_str(&text).map_err(|e| e.to_string())?;
    if raw.command.is_empty() {
        return Err("empty command".into());
    }
    let lifetime = match raw.lifetime.as_deref() {
        None => Lifetime::Session,
        Some(l) => Lifetime::parse(l).ok_or_else(|| format!("bad lifetime `{l}`"))?,
    };
    let timeout = match raw.timeout.as_deref() {
        None => Duration::from_secs(300),
        Some(t) => mu_base::config::parse_duration(t).ok_or_else(|| format!("bad timeout `{t}`"))?,
    };
    let mut tools = Vec::new();
    for t in raw.tools {
        let p = dir.join(&t.schema);
        // Only checked for existence here; parsing waits until the schema is needed.
        if !p.is_file() {
            return Err(format!("{}: no such file", p.display()));
        }
        let access = match t.tier.as_deref() {
            None | Some("exec") => Access::Exec,
            Some("read") => Access::Read,
            Some(other) => return Err(format!("bad tier `{other}`")),
        };
        tools.push(ToolSpec::new(t.name, t.description, p).with_access(access));
    }
    Ok(Manifest {
        name: raw.name,
        command: raw.command,
        lifetime,
        hooks: raw.hooks,
        provider: raw.provider,
        tools,
        timeout,
        dir: dir.into(),
    })
}

/// Manifests under `root/*/plugin.toml`, sorted by name; invalid ones become warnings.
pub fn discover(root: &Path) -> (Vec<Manifest>, Vec<String>) {
    let mut out = Vec::new();
    let mut warnings = Vec::new();
    let Ok(rd) = std::fs::read_dir(root) else { return (out, warnings) };
    let mut dirs: Vec<PathBuf> =
        rd.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.join("plugin.toml").exists()).collect();
    dirs.sort();
    for d in dirs {
        match load_one(&d) {
            Ok(m) => out.push(m),
            Err(e) => {
                warnings.push(format!("plugin {}: skipped ({e})", d.file_name().unwrap_or_default().to_string_lossy()))
            }
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    (out, warnings)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifetimes() {
        assert_eq!(Lifetime::parse("idle:10m"), Some(Lifetime::Idle(Duration::from_secs(600))));
        assert_eq!(Lifetime::parse("idle:30s"), Some(Lifetime::Idle(Duration::from_secs(30))));
        assert_eq!(Lifetime::parse("call"), Some(Lifetime::Call));
        assert_eq!(Lifetime::parse("idle:5x"), None);
        assert_eq!(Lifetime::parse("idle:"), None);
        assert_eq!(Lifetime::parse("idle:5é"), None);
    }

    fn load_with(extra: &str) -> Result<Manifest, String> {
        let d = std::env::temp_dir().join(format!("mu-man-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("plugin.toml"), format!("name = \"x\"\ncommand = [\"x\"]\n{extra}")).unwrap();
        let m = load_one(&d);
        let _ = std::fs::remove_dir_all(d);
        m
    }

    #[test]
    fn native_tool_timeout_defaults_and_parses() {
        assert_eq!(load_with("").unwrap().timeout, Duration::from_secs(300));
        assert_eq!(load_with("timeout = \"90s\"\n").unwrap().timeout, Duration::from_secs(90));
        let err = load_with("timeout = \"soon\"\n").unwrap_err();
        assert_eq!(err, "bad timeout `soon`");
    }

    #[test]
    fn a_tool_declares_its_tier() {
        let tool = |tier: &str| {
            let d = std::env::temp_dir().join(format!("mu-man-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("s.json"), "{}").unwrap();
            std::fs::write(
                d.join("plugin.toml"),
                format!(
                    "name = \"x\"\ncommand = [\"x\"]\n[[tools]]\nname = \"t\"\ndescription = \"d\"\nschema = \"s.json\"\n{tier}"
                ),
            )
            .unwrap();
            let m = load_one(&d);
            let _ = std::fs::remove_dir_all(d);
            m.map(|m| m.tools[0].access)
        };
        assert_eq!(tool("").unwrap(), Access::Exec, "no tier is exec");
        assert_eq!(tool("tier = \"exec\"\n").unwrap(), Access::Exec);
        assert_eq!(tool("tier = \"read\"\n").unwrap(), Access::Read);
        assert_eq!(tool("tier = \"maybe\"\n").unwrap_err(), "bad tier `maybe`");
    }

    #[test]
    fn schemas_are_not_parsed_until_needed() {
        let d = std::env::temp_dir().join(format!("mu-man-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("plugin.toml"),
            "name = \"x\"\ncommand = [\"x\"]\n[[tools]]\nname = \"t\"\ndescription = \"d\"\nschema = \"s.json\"\n",
        )
        .unwrap();
        std::fs::write(d.join("s.json"), "{ not json").unwrap();
        let m = load_one(&d).expect("loads: the schema is only checked for existence");
        assert!(m.tools[0].schema().is_err(), "parsed (and failed) on first use");
        std::fs::write(d.join("s.json"), "{\"type\":\"object\"}").unwrap();
        // The first answer is cached, so the file is read once.
        assert!(m.tools[0].schema().is_err());
        let m2 = load_one(&d).unwrap();
        assert_eq!(m2.tools[0].schema().as_ref().unwrap()["type"], "object");
        let _ = std::fs::remove_dir_all(d);
    }
}
