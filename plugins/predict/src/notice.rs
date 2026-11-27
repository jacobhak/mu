//! Warnings for the user. The plugin process sends them to mu; the daemon has no host, so it keeps them and
//! hands each client the ones it has not seen.

use serde_json::json;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

static KEEP: AtomicBool = AtomicBool::new(false);
static NOTES: Mutex<Vec<String>> = Mutex::new(Vec::new());

pub fn keep_for_clients() {
    KEEP.store(true, Ordering::SeqCst);
}

pub fn warn(message: String) {
    if KEEP.load(Ordering::SeqCst) {
        NOTES.lock().unwrap_or_else(|e| e.into_inner()).push(message);
    } else {
        send(&message);
    }
}

pub fn send(message: &str) {
    mu_common::rpc::notify("ui/notify", json!({ "level": "warn", "message": message }));
}

/// The notes after the first `seen`, and how many exist in all.
pub fn since(seen: usize) -> (Vec<String>, usize) {
    let g = NOTES.lock().unwrap_or_else(|e| e.into_inner());
    (g.iter().skip(seen).cloned().collect(), g.len())
}
