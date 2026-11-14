//! Reading and writing the host files the UI edits: a plan under review, a memory file (table rows
//! "Plan review, /plans" and the file part of "/memory"). The host answers only for a path it handed
//! out itself this session (a `memory/files` reply, a plan-review event): the UI cannot use these calls
//! to reach any other file.

use super::local::{Local, Registry};
use super::{Answer, ready};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;

pub const READ_FILE: &str = "read_file";
pub const WRITE_FILE: &str = "write_file";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReadFile {
    pub path: PathBuf,
    /// A file that does not exist yet reads as empty (a memory file nothing was written to).
    #[serde(default)]
    pub missing_ok: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WriteFile {
    pub path: PathBuf,
    pub text: String,
}

thread_local! {
    static HANDED_OUT: RefCell<HashSet<PathBuf>> = RefCell::new(HashSet::new());
}

/// Records a path the host sent to the UI, so that `read_file` and `write_file` answer for it.
pub fn hand_out(path: &Path) {
    HANDED_OUT.with(|h| h.borrow_mut().insert(path.to_path_buf()));
}

fn check(path: &Path) -> Result<(), String> {
    if HANDED_OUT.with(|h| h.borrow().contains(path)) {
        Ok(())
    } else {
        Err(format!("{} is not a file the host offered for editing", path.display()))
    }
}

pub(super) fn register(r: &mut Registry) {
    r.add(READ_FILE, read_file);
    r.add(WRITE_FILE, write_file);
}

fn read_file(_: &Rc<Local>, args: serde_json::Value) -> Answer {
    ready(args, |a: ReadFile| {
        check(&a.path)?;
        match std::fs::read_to_string(&a.path) {
            Err(e) if a.missing_ok && e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
            r => r.map_err(|e| e.to_string()),
        }
    })
}

fn write_file(_: &Rc<Local>, args: serde_json::Value) -> Answer {
    ready(args, |a: WriteFile| {
        check(&a.path)?;
        std::fs::write(&a.path, a.text).map_err(|e| e.to_string())
    })
}
