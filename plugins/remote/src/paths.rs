//! Where mu-remote keeps its files. Config is `$MU_REMOTE_CONFIG`, else
//! `$XDG_CONFIG_HOME/mu-remote/config.toml` (default `~/.config/mu-remote/config.toml`), with the active mu
//! profile's `profiles/<name>/config.toml` beside it merged over it. State (keys, pins,
//! the worker's own records, installed adapters) is under `$MU_REMOTE_HOME`, else
//! `$XDG_STATE_HOME/mu-remote` (default `~/.local/state/mu-remote`).

use std::path::{Path, PathBuf};

pub fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

fn xdg(var: &str, fallback: &str) -> PathBuf {
    match std::env::var_os(var) {
        Some(p) if Path::new(&p).is_absolute() => PathBuf::from(p),
        _ => home().join(fallback),
    }
}

pub fn config_file() -> PathBuf {
    match std::env::var_os("MU_REMOTE_CONFIG") {
        Some(p) => PathBuf::from(p),
        None => xdg("XDG_CONFIG_HOME", ".config").join("mu-remote/config.toml"),
    }
}

/// The active mu profile (`MU_PROFILE`, else `OMP_PROFILE`, else `PI_PROFILE`; empty is unset), only when it is
/// safe as a path component. mu-remote links no mu crate, so this repeats mu's rule.
pub fn profile() -> Option<String> {
    ["MU_PROFILE", "OMP_PROFILE", "PI_PROFILE"]
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()))
        .filter(|p| !p.contains(['/', '\\']) && p != "." && p != "..")
}

/// The active profile's own config, merged over `config_file()`: `profiles/<name>/config.toml` beside it.
pub fn profile_config_file() -> Option<PathBuf> {
    let dir = config_file().parent()?.join("profiles").join(profile()?);
    Some(dir.join("config.toml"))
}

pub fn state_dir() -> PathBuf {
    match std::env::var_os("MU_REMOTE_HOME") {
        Some(p) => PathBuf::from(p),
        None => xdg("XDG_STATE_HOME", ".local/state").join("mu-remote"),
    }
}

/// The `mu` program mu-remote runs (`mu --headless`, `mu attach --stdio`, `mu --attach`): `$MU_REMOTE_MU`,
/// else `mu` on the PATH.
pub fn mu_program() -> String {
    std::env::var("MU_REMOTE_MU").ok().filter(|p| !p.is_empty()).unwrap_or_else(|| "mu".to_string())
}
