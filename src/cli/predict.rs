//! `mu predict setup <engine>`: runs the predict plugin's `setup`, which owns the download and verification.

use anyhow::{Result, anyhow};
use mu_common::paths::self_exe;
use std::path::PathBuf;
use std::process::Command;

#[derive(clap::Subcommand)]
pub enum Cmd {
    /// Download, verify and cache the weights of an engine. An interrupted download resumes; a verified copy is
    /// never downloaded again.
    Setup {
        /// The engine to set up.
        #[arg(value_parser = ["smollm"])]
        engine: String,
        /// Report whether the weights are present and verified; download nothing.
        #[arg(long)]
        status: bool,
    },
}

pub fn run(cmd: Cmd) -> Result<i32> {
    let Cmd::Setup { engine, status } = cmd;
    let bin = plugin_bin();
    let mut c = Command::new(&bin);
    c.args(["setup", &engine]);
    if status {
        c.arg("--status");
    }
    let st = c.status().map_err(|e| anyhow!("cannot run {}: {e}", bin.display()))?;
    Ok(st.code().unwrap_or(1))
}

/// `mu-predict` next to this binary (as installed), else from `PATH`.
fn plugin_bin() -> PathBuf {
    self_exe()
        .ok()
        .and_then(|e| e.parent().map(|d| d.join("mu-predict")))
        .filter(|p| p.is_file())
        .unwrap_or_else(|| PathBuf::from("mu-predict"))
}
