//! Helpers for tests: temporary directories and git repositories.

use std::path::{Path, PathBuf};

/// A fresh, canonical temporary directory.
pub fn tmp() -> PathBuf {
    let d = std::env::temp_dir().join(format!("mu-remote-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&d).unwrap();
    d.canonicalize().unwrap()
}

/// Runs `git` in `dir` with a fixed identity, panics on failure, returns stdout.
pub fn sh(dir: &Path, args: &[&str]) -> String {
    let o = std::process::Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(o.status.success(), "{args:?}: {}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8_lossy(&o.stdout).into_owned()
}

/// A fresh git repo whose initial commit holds `a.txt` and `b.txt`, both `one\n`.
pub fn repo() -> PathBuf {
    let d = tmp();
    sh(&d, &["init", "-q"]);
    std::fs::write(d.join("a.txt"), "one\n").unwrap();
    std::fs::write(d.join("b.txt"), "one\n").unwrap();
    sh(&d, &["add", "."]);
    sh(&d, &["commit", "-qm", "init"]);
    d
}
