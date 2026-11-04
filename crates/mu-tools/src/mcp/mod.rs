//! MCP servers as plugins that only provide tools. The plugin host runs the process and the
//! JSON-RPC traffic (`crate::plugin`); this module holds what is specific to the `mcp` dialect: the
//! per-server state, the tool list and the prompt instructions.

pub mod auth;
pub mod cache;
pub mod config;
pub mod convert;
pub mod discover;
pub mod store;

use crate::plugin::manifest::ToolSpec;
use cache::RemoteTool;
use config::McpServer;
use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;

/// Why a JSON file of the `mcp` layer did not load. `Blank` and `Parse` carry the parser's error.
pub(crate) enum JsonError {
    Io(std::io::Error),
    Blank(serde_json::Error),
    Parse(serde_json::Error),
}

/// The one reader of the `mcp` JSON files; each caller decides what a missing, blank or broken file means.
pub(crate) fn read_json(path: &Path) -> Result<serde_json::Value, JsonError> {
    let text = std::fs::read_to_string(path).map_err(JsonError::Io)?;
    serde_json::from_str(&text)
        .map_err(|e| if text.trim().is_empty() { JsonError::Blank(e) } else { JsonError::Parse(e) })
}

/// MCP protocol version sent at `initialize`, the same as omp.
pub const PROTOCOL_VERSION: &str = "2025-11-25";

/// A tool as advertised to the model and the name the server knows it by.
pub struct McpTool {
    pub spec: ToolSpec,
    pub remote: String,
}

pub struct McpSlot {
    pub server: RefCell<McpServer>,
    pub hash: RefCell<String>,
    pub tools: RefCell<Vec<McpTool>>,
    pub instructions: RefCell<Option<String>>,
    /// The tool list is known this session (from the cache or a listing).
    pub listed: Cell<bool>,
    /// Listing failed; the server stays out of this session's tools.
    pub failed: Cell<bool>,
    /// A token fetch failed or the server kept answering 401.
    pub needs_login: Cell<bool>,
    /// The server said its tool list changed.
    pub stale: Cell<bool>,
}

impl McpSlot {
    pub fn new(server: McpServer, project: &str) -> Self {
        Self {
            hash: RefCell::new(cache::config_hash(&server, project)),
            server: RefCell::new(server),
            tools: Default::default(),
            instructions: Default::default(),
            listed: Cell::new(false),
            failed: Cell::new(false),
            needs_login: Cell::new(false),
            stale: Cell::new(false),
        }
    }

    pub fn name(&self) -> String {
        self.server.borrow().name.clone()
    }

    /// Takes the cached tool list for the current config, if there is one.
    pub fn load_cache(&self, dir: &Path, taken: &mut HashSet<String>, reserved: fn(&str) -> bool) -> Vec<String> {
        let Some(c) = cache::load(dir, &self.hash.borrow()) else { return Vec::new() };
        *self.instructions.borrow_mut() = c.instructions;
        let w = self.set_tools(&c.tools, taken, reserved);
        self.listed.set(true);
        w
    }

    /// Replaces the advertised tools. `taken` holds every advertised name in the host; names that
    /// clash with another tool are skipped with a warning.
    pub fn set_tools(
        &self,
        remote: &[RemoteTool],
        taken: &mut HashSet<String>,
        reserved: fn(&str) -> bool,
    ) -> Vec<String> {
        for t in self.tools.borrow().iter() {
            taken.remove(&t.spec.name);
        }
        let server = self.name();
        let mut out: Vec<McpTool> = Vec::new();
        let mut warnings = Vec::new();
        for r in remote {
            let name = convert::tool_name(&server, &r.name);
            if reserved(&name) || !taken.insert(name.clone()) {
                warnings.push(format!("mcp {server}: tool `{}` skipped (name collision)", r.name));
                continue;
            }
            out.push(McpTool {
                spec: ToolSpec::inline(name, r.description.clone(), convert::normalize_schema(&r.input_schema)),
                remote: r.name.clone(),
            });
        }
        out.sort_by(|a, b| a.spec.name.cmp(&b.spec.name));
        *self.tools.borrow_mut() = out;
        warnings
    }

    /// The remote tools in the shape the cache stores them.
    pub fn remote_tools(&self) -> Vec<RemoteTool> {
        self.tools
            .borrow()
            .iter()
            .map(|t| RemoteTool {
                name: t.remote.clone(),
                description: t.spec.description.clone(),
                input_schema: t.spec.schema().clone().unwrap_or_default(),
            })
            .collect()
    }
}

/// omp's call timeout: `OMP_MCP_TIMEOUT_MS`, else the server's `timeout`, else 30 s; 0 disables.
pub fn timeout_for(s: &McpServer) -> Option<Duration> {
    let ms = std::env::var("OMP_MCP_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .or(s.timeout_ms)
        .unwrap_or(30_000);
    (ms > 0).then(|| Duration::from_millis(ms))
}

/// How long connecting and listing may take. omp's `timeout` is a call timeout (0 = none); starting a
/// server must stay bounded either way, so no `timeout` means 30 s (npx downloads) and 0 means 60 s.
pub fn connect_timeout(s: &McpServer) -> Duration {
    timeout_for(s).unwrap_or(Duration::from_secs(60))
}

const INSTRUCTIONS_CAP: usize = 4000;

/// omp's prompt block: a heading, then one `### <server>` per server, sorted by name.
pub fn render_instructions(mut items: Vec<(String, String)>) -> Option<String> {
    items.retain(|(_, t)| !t.trim().is_empty());
    if items.is_empty() {
        return None;
    }
    items.sort();
    let mut out = String::from("## MCP Server Instructions\n");
    for (name, text) in items {
        out.push_str(&format!("\n### {name}\n"));
        let text = text.trim();
        match text.char_indices().nth(INSTRUCTIONS_CAP) {
            Some((i, _)) => {
                out.push_str(&text[..i]);
                out.push_str("\n[truncated]");
            }
            None => out.push_str(text),
        }
        out.push('\n');
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instructions_block() {
        assert_eq!(render_instructions(vec![("a".into(), "  ".into())]), None);
        let s = render_instructions(vec![("b".into(), "bb".into()), ("a".into(), "x".repeat(4100))]).unwrap();
        assert!(s.starts_with("## MCP Server Instructions\n\n### a\n"));
        assert!(s.contains("\n[truncated]\n"));
        assert!(s.find("### a").unwrap() < s.find("### b").unwrap());
    }
}
