//! The writes `App` makes outside the screen that have not moved to the host yet: session control.
//! Reads stay with their callers.

use mu_provider::provider::client::Providers;
use mu_session::session::{control, inbox};
use std::path::Path;
use std::rc::Rc;

pub(super) struct Services {
    pub(super) providers: Rc<Providers>,
}

impl Services {
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
