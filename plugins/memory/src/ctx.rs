//! What `initialize` told us about the session this process serves.

use std::path::PathBuf;
use std::sync::Mutex;

#[derive(Clone, Debug, Default)]
pub struct Ctx {
    pub cwd: PathBuf,
    pub session_id: String,
    pub session_file: Option<PathBuf>,
    pub subagent: bool,
}

static CTX: Mutex<Option<Ctx>> = Mutex::new(None);

pub fn set(c: Ctx) {
    *CTX.lock().unwrap_or_else(|e| e.into_inner()) = Some(c);
}

pub fn get() -> Ctx {
    CTX.lock().unwrap_or_else(|e| e.into_inner()).clone().unwrap_or_default()
}
