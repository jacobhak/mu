//! Helpers shared by tests.

use std::path::{Path, PathBuf};

/// A private `MU_HOME` for this test process, created once. Tests that touch mu's state
/// (sessions, blobs) call it first so they never write to the real `~/.mu`.
pub fn home() -> &'static Path {
    static H: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    H.get_or_init(|| {
        let d = std::env::temp_dir().join(format!("mu-test-home-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        // SAFETY: set once, before any test in this process reads it through `home()`.
        unsafe { std::env::set_var("MU_HOME", &d) };
        d
    })
}
