//! `launch NAME`: the `[task] launcher` (mu's launcher contract 2). Runs the task as a worker on remote NAME:
//! the adapter starts the worker from the task's bundle, `link.rs` carries mu's stream to it and back
//! (reconnecting as needed), and when the worker is done the launcher fetches its result into
//! `MU_TASK_RESULT_BUNDLE` and stops it.

use crate::adapter::{Adapter, Start};
use crate::config::Config;
use crate::link::{End, Link, Opts, Stream};
use crate::trust;
use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncBufReadExt;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc::unbounded_channel;
use tokio_util::sync::CancellationToken;

/// What mu tells a launcher.
struct Task {
    name: String,
    root: PathBuf,
    agent_dir: PathBuf,
    bundle: PathBuf,
    result_bundle: PathBuf,
}

fn var(name: &str) -> Result<String> {
    std::env::var(name)
        .ok()
        .filter(|v| !v.is_empty())
        .with_context(|| format!("{name} is not set: `mu-remote launch` runs as mu's [task] launcher (contract 2)"))
}

impl Task {
    fn from_env() -> Result<Task> {
        if let Ok(c) = std::env::var("MU_TASK_CONTRACT")
            && c != "2"
        {
            bail!("mu speaks launcher contract {c}; this mu-remote speaks 2");
        }
        Ok(Task {
            name: var("MU_TASK_NAME")?,
            root: var("MU_TASK_ROOT")?.into(),
            agent_dir: var("MU_TASK_AGENT_DIR")?.into(),
            bundle: var("MU_TASK_BUNDLE")?.into(),
            result_bundle: var("MU_TASK_RESULT_BUNDLE")?.into(),
        })
    }
}

fn progress(text: &str) {
    eprintln!("{}", serde_json::json!({ "progress": text }));
}

pub fn run(name: &str) -> Result<i32> {
    let task = Task::from_env()?;
    let cfg = Config::load()?;
    let (name, remote) = cfg.pick(Some(name))?;
    if remote.adapter.trim() == "ssh" {
        bail!("remote {name} uses the ssh adapter, which cannot run workers; use another adapter");
    }
    trust::admit(name, remote, &task.root, &task.agent_dir).map_err(|e| anyhow!(e))?;
    let adapter = Arc::new(Adapter::from_config(name, remote)?);
    let give_up = remote.reconnect_timeout();
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().context("cannot start the runtime")?;
    let code = tokio::task::LocalSet::new().block_on(&rt, launch(adapter, task, give_up));
    // A blocked read of mu's stdin must not hold the exit.
    rt.shutdown_background();
    code
}

async fn launch(adapter: Arc<Adapter>, task: Task, give_up: Duration) -> Result<i32> {
    let (mut term, mut int) = (signal(SignalKind::terminate())?, signal(SignalKind::interrupt())?);
    let cancel = CancellationToken::new();
    tokio::task::spawn_local({
        let cancel = cancel.clone();
        async move {
            tokio::select! {
                _ = term.recv() => {}
                _ = int.recv() => {}
            }
            cancel.cancel();
        }
    });
    // mu's lines are read from the start: the payload is waiting while the worker starts.
    let (tx, from_mu) = unbounded_channel::<String>();
    tokio::task::spawn_local(async move {
        let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
        while let Ok(Some(l)) = lines.next_line().await {
            if !l.trim().is_empty() && tx.send(l).is_err() {
                break;
            }
        }
    });

    let id = start(&adapter, &task, &cancel).await?;
    progress(&format!("worker {id} started"));

    let connect = || adapter.connect(&id).map_err(|e| format!("{e:#}")).and_then(Stream::from_child);
    let link =
        Link { connect: &connect, opts: Opts { give_up, secure: true }, id: &id, from_mu, cancel: cancel.clone() };
    let mut result_seen = false;
    let mut stdout = std::io::stdout();
    let end = link
        .run(&mut |line| {
            if line.contains("\"result\"") && serde_json::from_str::<Value>(line).is_ok_and(|v| v["type"] == "result") {
                result_seen = true;
            }
            let _ = writeln!(stdout, "{line}").and_then(|()| stdout.flush());
        })
        .await;

    match &end {
        End::Exit { stderr, .. } if !stderr.is_empty() && !result_seen => eprintln!("{stderr}"),
        End::GaveUp(note) | End::Refused(note) => eprintln!("{note}"),
        End::Exit { .. } | End::Cancelled => {}
    }
    if matches!(end, End::Refused(_)) {
        return Ok(0);
    }
    let fetched = fetch(&adapter, &id, &task.result_bundle);
    match &fetched {
        Ok(()) => {}
        Err(e) => eprintln!("could not fetch the result of worker {id}: {e:#}"),
    }
    if fetched.is_ok() || matches!(end, End::Cancelled) {
        if let Err(e) = adapter.stop(&id) {
            eprintln!("could not stop worker {id}: {e:#}");
        }
    } else {
        eprintln!("worker {id} is left running on remote {}", adapter.name());
    }
    Ok(0)
}

/// Starts the worker and returns its id. A cancel while it starts waits a little for the id so the worker can
/// still be stopped.
async fn start(adapter: &Arc<Adapter>, task: &Task, cancel: &CancellationToken) -> Result<String> {
    let what =
        Start::Worker { launch: task.name.clone(), bundle: task.bundle.clone(), dir: worker_dir(adapter, &task.name) };
    let a = adapter.clone();
    let mut starting = tokio::task::spawn_blocking(move || a.start(&what, &mut progress));
    let joined = tokio::select! {
        r = &mut starting => r,
        () = cancel.cancelled() => match tokio::time::timeout(Duration::from_secs(25), starting).await {
            Ok(r) => r,
            Err(_) => bail!("cancelled while the worker was starting"),
        },
    };
    joined
        .map_err(|e| anyhow!("the adapter thread failed: {e}"))?
        .map_err(|e| anyhow!("cannot start a worker on remote {}: {e:#}", adapter.name()))
}

/// Where the caller wants worker `name`'s checkout: under the remote's `work_dir` setting (default `/work`).
fn worker_dir(adapter: &Adapter, name: &str) -> String {
    let base = adapter.setting("work_dir").unwrap_or_else(|| crate::client::DEFAULT_WORK_DIR.to_string());
    format!("{}/{name}", base.trim_end_matches('/'))
}

/// Writes the worker's result bundle to `path`: empty when it changed nothing, and when the fetch fails.
fn fetch(adapter: &Adapter, id: &str, path: &Path) -> Result<()> {
    let mut file = std::fs::File::create(path).with_context(|| path.display().to_string())?;
    let r = adapter.fetch(id, &mut file);
    if r.is_err() {
        let _ = file.set_len(0);
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Remote;

    fn adapter(work_dir: Option<&str>) -> Adapter {
        let mut settings = toml::Table::new();
        if let Some(w) = work_dir {
            settings.insert("work_dir".into(), w.into());
        }
        Adapter::from_config("far", &Remote { adapter: "/bin/true".into(), settings, ..Default::default() }).unwrap()
    }

    #[test]
    fn a_worker_checks_out_under_the_remotes_work_dir() {
        assert_eq!(worker_dir(&adapter(None), "fix-ab12cd34"), "/work/fix-ab12cd34");
        assert_eq!(worker_dir(&adapter(Some("/w/")), "fix-ab12cd34"), "/w/fix-ab12cd34");
    }
}
