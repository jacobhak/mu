//! Tools for mu: the built-in tools, the plugin host and its MCP layer, background jobs, and
//! context discovery (AGENTS.md, rules, skills, agents). The plugin host serves `core/complete`
//! through the providers, which is the only reason this crate depends on `mu-provider`.

pub mod context;
pub mod jobs;
pub mod mcp;
pub mod plugin;
pub mod tools;
pub mod walk;
