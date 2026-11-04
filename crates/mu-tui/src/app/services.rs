//! The writes `App` makes outside the screen: omp's config roles, stored API keys, MCP server
//! definitions and session control. Reads stay with their callers.

use mu_base::models::ProviderDef;
use mu_common::paths;
use mu_provider::auth::store::Entry;
use mu_provider::provider::Providers;
use mu_session::{control, inbox};
use mu_tools::mcp::store::{self, Level};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::rc::Rc;

pub(super) struct Services {
    pub(super) providers: Rc<Providers>,
}

impl Services {
    /// Points `role` at `model` in omp's config; returns the value the role now has.
    pub(super) fn set_role(&self, role: &str, model: &str) -> Result<String, String> {
        self.providers.cfg.set_role(role, model)
    }

    pub(super) fn save_key(&self, def: &ProviderDef, entry: Entry) -> Result<(), String> {
        self.providers.store().set(def.id, entry).map_err(|e| e.to_string())
    }

    pub(super) fn remove_key(&self, provider: &str) -> Result<bool, String> {
        self.providers.store().remove(provider).map_err(|e| e.to_string())
    }

    pub(super) fn mcp_add(&self, level: Level, cwd: &Path, name: &str, entry: Value) -> Result<PathBuf, String> {
        store::add(level, cwd, &paths::omp_agent_dir(), name, entry)
    }

    pub(super) fn mcp_remove(&self, cwd: &Path, name: &str) -> Result<Vec<PathBuf>, String> {
        store::remove(cwd, &paths::omp_agent_dir(), name)
    }

    pub(super) fn mcp_set_enabled(&self, cwd: &Path, name: &str, on: bool) -> Result<bool, String> {
        store::set_enabled(cwd, &paths::omp_agent_dir(), name, on)
    }

    pub(super) fn send_message(&self, session: &Path, cwd: &Path, text: &str) -> Result<String, String> {
        inbox::send(session, Some("mu jobs panel"), text, cwd)
    }

    pub(super) fn stop_session(&self, session: &Path, pid: u32, started: &str, kind: &str) -> Result<String, String> {
        control::stop_session(session, pid, started, kind)
    }

    pub(super) fn terminate_session(&self, session: &Path, pid: u32, started: &str) -> Result<String, String> {
        control::terminate_session(session, pid, started)
    }
}
