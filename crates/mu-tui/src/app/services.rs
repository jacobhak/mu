//! The writes `App` makes outside the screen: model roles and sets, stored API keys, MCP server
//! definitions and session control. Reads stay with their callers.

use mu_provider::auth::store::Entry;
use mu_provider::auth::web::WebSearch;
use mu_provider::provider::client::Providers;
use mu_session::session::{control, inbox};
use std::path::Path;
use std::rc::Rc;

pub(super) struct Services {
    pub(super) providers: Rc<Providers>,
}

impl Services {
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

    pub(super) fn send_message(&self, session: &Path, cwd: &Path, text: &str) -> Result<String, String> {
        inbox::send(session, Some("mu jobs panel"), text, cwd)
    }

    pub(super) fn stop_session(&self, session: &Path, pid: u32, started: &str, kind: &str) -> Result<String, String> {
        control::stop_session(session, pid, started, kind).map_err(|e| e.to_string())
    }

    pub(super) fn terminate_session(&self, session: &Path, pid: u32, started: &str) -> Result<String, String> {
        control::terminate_session(session, pid, started).map_err(|e| e.to_string())
    }
}
