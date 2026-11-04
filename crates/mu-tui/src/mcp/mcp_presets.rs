//! Popular MCP servers the `/mcp add` wizard offers, each with the few values it needs asked.

use crate::mcp::mcp_cmd::{http_entry, stdio_entry};
use serde_json::{Map, Value};
use std::path::Path;

pub struct Param {
    pub key: &'static str,
    /// Shown verbatim; states the default.
    pub prompt: String,
    pub default: Option<String>,
    /// The value is a credential: a literal one ends up in the config file.
    pub secret: bool,
}

pub struct Preset {
    /// A valid server name; the default name.
    pub id: &'static str,
    pub label: &'static str,
    pub description: &'static str,
    /// Asked in order; `build` gets the answers in the same order.
    pub params: Vec<Param>,
    builder: fn(&[String]) -> Value,
}

impl Preset {
    pub fn build(&self, answers: &[String]) -> Value {
        (self.builder)(answers)
    }
}

fn param(key: &'static str, prompt: &str, default: Option<String>, secret: bool) -> Param {
    let prompt = match &default {
        Some(d) => format!("{prompt} [{d}]"),
        None => prompt.to_string(),
    };
    Param { key, prompt, default, secret }
}

fn answer(a: &[String], i: usize) -> String {
    a.get(i).cloned().unwrap_or_default()
}

fn npx(package: &str, extra: &[String]) -> Value {
    let mut args = vec!["-y".to_string(), package.to_string()];
    args.extend_from_slice(extra);
    stdio_entry("npx", &args, Map::new(), None)
}

fn uvx(package: &str, extra: &[String]) -> Value {
    let mut args = vec![package.to_string()];
    args.extend_from_slice(extra);
    stdio_entry("uvx", &args, Map::new(), None)
}

fn github(a: &[String]) -> Value {
    let mut headers = Map::new();
    headers.insert("Authorization".into(), Value::String(format!("Bearer {}", answer(a, 0))));
    http_entry("https://api.githubcopilot.com/mcp/", headers, None)
}

pub fn presets(cwd: &Path, env_set: fn(&str) -> bool) -> Vec<Preset> {
    let cwd = cwd.display().to_string();
    let token_default = if env_set("GITHUB_PERSONAL_ACCESS_TOKEN") {
        Some("${GITHUB_PERSONAL_ACCESS_TOKEN}".to_string())
    } else if env_set("GITHUB_TOKEN") {
        Some("${GITHUB_TOKEN}".to_string())
    } else {
        None
    };
    vec![
        Preset {
            id: "filesystem",
            label: "Filesystem",
            description: "read and write files in one directory",
            params: vec![param("dir", "Directory the server may access", Some(cwd.clone()), false)],
            builder: |a| npx("@modelcontextprotocol/server-filesystem", &[answer(a, 0)]),
        },
        Preset {
            id: "git",
            label: "Git",
            description: "inspect and operate on a git repository",
            params: vec![param("dir", "Repository directory", Some(cwd), false)],
            builder: |a| uvx("mcp-server-git", &["--repository".to_string(), answer(a, 0)]),
        },
        Preset {
            id: "fetch",
            label: "Fetch",
            description: "fetch web pages as markdown",
            params: vec![],
            builder: |_| uvx("mcp-server-fetch", &[]),
        },
        Preset {
            id: "memory",
            label: "Memory",
            description: "a persistent knowledge graph",
            params: vec![],
            builder: |_| npx("@modelcontextprotocol/server-memory", &[]),
        },
        Preset {
            id: "sequential-thinking",
            label: "Sequential Thinking",
            description: "step-by-step problem solving",
            params: vec![],
            builder: |_| npx("@modelcontextprotocol/server-sequential-thinking", &[]),
        },
        Preset {
            id: "playwright",
            label: "Playwright",
            description: "drive a browser",
            params: vec![],
            builder: |_| npx("@playwright/mcp@latest", &[]),
        },
        Preset {
            id: "context7",
            label: "Context7",
            description: "up-to-date library documentation",
            params: vec![],
            builder: |_| npx("@upstash/context7-mcp", &[]),
        },
        Preset {
            id: "github",
            label: "GitHub",
            description: "issues, pull requests and code on GitHub (remote server, needs a token)",
            params: vec![param(
                "token",
                "GitHub token, or a ${ENV_VAR} reference (a reference is stored as-is and read when mu starts the \
                 server, which keeps the token out of the config file)",
                token_default,
                true,
            )],
            builder: github,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use mu_tools::mcp::store::check_name;
    use serde_json::json;

    fn none_set(_: &str) -> bool {
        false
    }

    fn find(ps: &[Preset], id: &str) -> usize {
        ps.iter().position(|p| p.id == id).unwrap_or_else(|| panic!("no preset {id}"))
    }

    #[test]
    fn ids_are_valid_names_and_unique() {
        let ps = presets(Path::new("/w"), none_set);
        let mut seen = std::collections::HashSet::new();
        for p in &ps {
            assert!(check_name(p.id).is_ok(), "{}", p.id);
            assert!(seen.insert(p.id), "duplicate {}", p.id);
            assert!(!p.label.is_empty() && !p.description.is_empty());
        }
    }

    #[test]
    fn every_build_has_a_type() {
        let ps = presets(Path::new("/w"), none_set);
        for p in &ps {
            let answers: Vec<String> =
                p.params.iter().map(|q| q.default.clone().unwrap_or_else(|| "x".into())).collect();
            let e = p.build(&answers);
            let t = e["type"].as_str().unwrap_or_else(|| panic!("{} has no type", p.id));
            assert!(t == "stdio" || t == "http", "{}", p.id);
            if t == "stdio" {
                assert!(e["command"].is_string(), "{}", p.id);
            } else {
                assert!(e["url"].as_str().unwrap().starts_with("https://"), "{}", p.id);
            }
        }
    }

    #[test]
    fn prompts_state_their_default() {
        let ps = presets(Path::new("/work/proj"), none_set);
        let fs = &ps[find(&ps, "filesystem")];
        assert_eq!(fs.params[0].prompt, "Directory the server may access [/work/proj]");
        assert_eq!(fs.params[0].default.as_deref(), Some("/work/proj"));
        assert!(!fs.params[0].secret);
    }

    #[test]
    fn builds_match_the_documented_commands() {
        let ps = presets(Path::new("/work/proj"), none_set);
        let b = |id: &str, a: &[&str]| ps[find(&ps, id)].build(&a.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(
            b("filesystem", &["/d"]),
            json!({"type": "stdio", "command": "npx", "args": ["-y", "@modelcontextprotocol/server-filesystem", "/d"]})
        );
        assert_eq!(
            b("git", &["/r"]),
            json!({"type": "stdio", "command": "uvx", "args": ["mcp-server-git", "--repository", "/r"]})
        );
        assert_eq!(b("fetch", &[]), json!({"type": "stdio", "command": "uvx", "args": ["mcp-server-fetch"]}));
        assert_eq!(
            b("memory", &[]),
            json!({"type": "stdio", "command": "npx", "args": ["-y", "@modelcontextprotocol/server-memory"]})
        );
        assert_eq!(
            b("sequential-thinking", &[]),
            json!({"type": "stdio", "command": "npx", "args": ["-y", "@modelcontextprotocol/server-sequential-thinking"]})
        );
        assert_eq!(
            b("playwright", &[]),
            json!({"type": "stdio", "command": "npx", "args": ["-y", "@playwright/mcp@latest"]})
        );
        assert_eq!(
            b("context7", &[]),
            json!({"type": "stdio", "command": "npx", "args": ["-y", "@upstash/context7-mcp"]})
        );
        assert_eq!(
            b("github", &["${T}"]),
            json!({
                "type": "http",
                "url": "https://api.githubcopilot.com/mcp/",
                "headers": {"Authorization": "Bearer ${T}"}
            })
        );
    }

    #[test]
    fn github_token_default_follows_the_environment() {
        let token = |env_set: fn(&str) -> bool| {
            let ps = presets(Path::new("/w"), env_set);
            let p = &ps[find(&ps, "github")].params[0];
            assert!(p.secret);
            assert!(p.prompt.contains("${ENV_VAR}"));
            (p.default.clone(), p.prompt.clone())
        };
        let (d, prompt) = token(none_set);
        assert_eq!(d, None);
        assert!(!prompt.contains('['), "{prompt}");
        assert_eq!(token(|v| v == "GITHUB_TOKEN").0.as_deref(), Some("${GITHUB_TOKEN}"));
        assert_eq!(
            token(|v| v == "GITHUB_PERSONAL_ACCESS_TOKEN").0.as_deref(),
            Some("${GITHUB_PERSONAL_ACCESS_TOKEN}")
        );
        let (d, prompt) = token(|_| true);
        assert_eq!(d.as_deref(), Some("${GITHUB_PERSONAL_ACCESS_TOKEN}"));
        assert!(prompt.ends_with("[${GITHUB_PERSONAL_ACCESS_TOKEN}]"));
    }
}
