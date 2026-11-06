//! The writes `App` makes outside the screen: model roles and sets, stored API keys, MCP server
//! definitions and session control. Reads stay with their callers.

use mu_common::paths;
use mu_provider::auth::store::Entry;
use mu_provider::auth::web::WebSearch;
use mu_provider::provider::client::Providers;
use mu_session::session::{control, inbox};
use mu_tools::mcp::store::{self, Level};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::rc::Rc;

pub(super) struct Services {
    pub(super) providers: Rc<Providers>,
}

impl Services {
    pub(super) fn edit_role(&self, set: &str, role: &str, model: Option<&str>) -> Result<(), String> {
        self.providers.cfg.edit_role(set, role, model)
    }

    pub(super) fn switch_set(&self, name: &str) -> Result<(), String> {
        self.providers.cfg.switch_set(name)
    }

    /// Returns the set the new one was copied from.
    pub(super) fn new_set(&self, name: &str) -> Result<Option<String>, String> {
        self.providers.cfg.new_set(name)
    }

    pub(super) fn delete_set(&self, name: &str) -> Result<(), String> {
        self.providers.cfg.delete_set(name)
    }

    pub(super) fn rename_set(&self, old: &str, new: &str) -> Result<(), String> {
        self.providers.cfg.rename_set(old, new)
    }

    /// Copies omp's model config into models.toml, never over an existing file.
    pub(super) fn import_models(&self) -> Result<Vec<String>, String> {
        self.providers.cfg.import_models(false)
    }

    pub(super) fn save_key(&self, id: &str, entry: Entry) -> Result<(), String> {
        self.providers.store().set(id, entry).map_err(|e| e.to_string())
    }

    pub(super) fn set_chat(&self, id: &str, chat: bool) -> Result<bool, String> {
        self.providers.store().set_chat(id, chat).map_err(|e| e.to_string())
    }

    pub(super) fn set_web(&self, w: WebSearch) -> Result<(), String> {
        self.providers.store().set_web(Some(w)).map_err(|e| e.to_string())
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
