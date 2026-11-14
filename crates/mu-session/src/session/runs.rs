//! Where the jobs panel gets its list of other live mu processes: a source it asks for snapshots,
//! so the panel does not know about the run directory.
use super::list;
use super::owner::{self, LiveSession};
use mu_common::paths;
use std::path::{Path, PathBuf};

pub trait SessionSource {
    /// Every live session now.
    fn live_sessions(&self) -> Vec<LiveSession>;

    /// What to call a session: its title, else its first prompt; `None` when it has neither.
    fn title(&self, session: &Path) -> Option<String> {
        let i = list::info(session)?;
        Some(i.title.filter(|t| !t.is_empty()).unwrap_or(i.first_prompt)).filter(|t| !t.is_empty())
    }
}

/// The owner records in one run directory.
pub struct RunDir(PathBuf);

impl RunDir {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self(dir.into())
    }

    /// This profile's `run` directory.
    pub fn profile() -> Self {
        Self::new(paths::profile_home().join("run"))
    }
}

impl SessionSource for RunDir {
    fn live_sessions(&self) -> Vec<LiveSession> {
        owner::live_sessions_in(&self.0)
    }
}
