//! `mu agent --detach` and `mu worker run --detach`: start the process in the background and return
//! once it listens.
//!
//! The agent is a second `mu agent --detached` process in a session of its own (`setsid`), so closing
//! the terminal or the ssh connection that ran this command does not reach it. Its output goes to
//! `<profile home>/logs/agent-<id>.log`. This process prints `{"id":ID}` when the agent's socket is up.

use anyhow::{Context, Result, bail};
use mu_common::paths;
use std::io::{BufRead, BufReader};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::Duration;

const START_WAIT: Duration = Duration::from_secs(60);

/// `kind` is `agent` or `worker`: the command that is started again with `--detached`, and the log's name.
pub fn run(kind: &str) -> Result<i32> {
    let logs = paths::profile_home().join("logs");
    mu_common::fs::create_private_dir(&logs).with_context(|| logs.display().to_string())?;
    let starting = logs.join(format!("{kind}-starting-{}.log", std::process::id()));
    let log = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(&starting)
        .with_context(|| starting.display().to_string())?;
    let exe = paths::self_exe().context("cannot locate mu")?;
    let mut command = Command::new(exe);
    command
        .args(std::env::args_os().skip(1).filter(|a| a != "--detach"))
        .arg("--detached")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(log);
    // SAFETY: setsid is async-signal-safe.
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        })
    };
    let mut child = command.spawn().context("cannot start the agent")?;
    let stdout = child.stdout.take().context("the agent has no stdout")?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let _ = BufReader::new(stdout).read_line(&mut line);
        let _ = tx.send(line);
    });
    let line = rx.recv_timeout(START_WAIT).unwrap_or_default();
    let id = serde_json::from_str::<serde_json::Value>(&line)
        .ok()
        .and_then(|v| v["session"].as_str().or(v["id"].as_str()).map(String::from));
    let Some(id) = id else {
        let _ = child.kill();
        let _ = child.wait();
        let tail: Vec<String> = std::fs::read_to_string(&starting)
            .unwrap_or_default()
            .lines()
            .rev()
            .take(5)
            .map(String::from)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        bail!(
            "the agent did not start ({}){}",
            starting.display(),
            if tail.is_empty() { String::new() } else { format!(": {}", tail.join(" | ")) }
        );
    };
    // The open descriptor follows the rename.
    let _ = std::fs::rename(&starting, logs.join(format!("{kind}-{id}.log")));
    println!("{}", serde_json::json!({ "id": id }));
    Ok(0)
}
