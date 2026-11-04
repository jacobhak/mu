//! Plugins: native processes that speak JSON-RPC over stdio, and MCP servers run as plugins that
//! only provide tools. `host` is the registry and dispatch, `process` one running process,
//! `mcp_runtime` the MCP lifecycle.

pub mod host;
pub mod manifest;
mod mcp_runtime;
pub mod process;
#[cfg(test)]
mod tests;
