//! `mu-remote check NAME --full`: runs an adapter through the verbs that need no agent of the user's (a
//! synthetic worker: start, connect, fetch, stop, gone) and, given an agent, the file and port verbs.

use crate::adapter::{Adapter, Start, Unsupported};
use crate::secure::{self, Handshake, Keypair};
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

/// How long the stream of a worker may take to answer the handshake and the first line.
const CONNECT_WAIT: Duration = Duration::from_secs(30);

enum Outcome {
    Ok(Option<String>),
    Skipped(String),
    Failed(String),
}

fn first_line(e: &anyhow::Error) -> String {
    format!("{e:#}").lines().next().unwrap_or_default().to_string()
}

fn hex8() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..8].to_string()
}

/// The steps' lines and whether none failed.
struct Steps {
    text: String,
    ok: bool,
}

impl Steps {
    fn add(&mut self, step: &str, outcome: Outcome) {
        let line = match outcome {
            Outcome::Ok(None) => format!("  {step}: ok\n"),
            Outcome::Ok(Some(note)) => format!("  {step}: ok ({note})\n"),
            Outcome::Skipped(why) => format!("  {step}: skipped ({why})\n"),
            Outcome::Failed(e) => {
                self.ok = false;
                format!("  {step}: FAILED: {e}\n")
            }
        };
        self.text.push_str(&line);
    }
}

/// A repository of one commit, bundled the way a launcher bundles a task: exactly `refs/mu/task/<launch>`.
fn synthetic_bundle(launch: &str) -> Result<(PathBuf, PathBuf), String> {
    let dir = std::env::temp_dir().join(format!("mu-remote-check-{launch}"));
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let git = |args: &[&str]| -> Result<(), String> {
        let o = Command::new("git")
            .args(["-c", "user.name=mu-remote-check", "-c", "user.email=check@invalid", "-c", "commit.gpgsign=false"])
            .args(args)
            .current_dir(&dir)
            .output()
            .map_err(|e| format!("cannot run git: {e}"))?;
        if o.status.success() {
            Ok(())
        } else {
            Err(format!("git {}: {}", args.join(" "), String::from_utf8_lossy(&o.stderr).trim()))
        }
    };
    git(&["init", "-q"])?;
    std::fs::write(dir.join("check.txt"), "mu-remote check\n").map_err(|e| e.to_string())?;
    git(&["add", "."])?;
    git(&["commit", "-q", "-m", "check"])?;
    let task_ref = format!("refs/mu/task/{launch}");
    git(&["update-ref", &task_ref, "HEAD"])?;
    let bundle = dir.join("task.bundle");
    git(&["bundle", "create", "-q", &bundle.to_string_lossy(), &task_ref])?;
    Ok((dir, bundle))
}

/// Connects to worker `id` the way a launcher does: the encrypted handshake, the launcher's `resume`, and the
/// worker's `resume` back. Nothing is started in the worker.
fn probe_connect(a: &Adapter, id: &str) -> Result<(), String> {
    let mut child = a.connect(id).map_err(|e| first_line(&e))?;
    let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        return Err("the connect process has no pipes".into());
    };
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| e.to_string())?;
    let result = rt.block_on(async {
        use tokio::io::{AsyncWriteExt, BufReader};
        let mut to = tokio::process::ChildStdin::from_std(stdin).map_err(|e| e.to_string())?;
        let mut from = crate::rpc::Framed::new(BufReader::new(
            tokio::process::ChildStdout::from_std(stdout).map_err(|e| e.to_string())?,
        ));
        let work = async {
            let mut hs = Handshake::initiator(&Keypair::generate()?)?;
            let io = |e: std::io::Error| format!("the stream failed: {e}");
            let hello = format!("{}\n", hs.write()?);
            to.write_all(hello.as_bytes()).await.map_err(io)?;
            to.flush().await.map_err(io)?;
            match from.next_line().await {
                Some(crate::rpc::Line::Value(v)) if v.is_string() => hs.read(&v)?,
                Some(_) => return Err("the stream is not a mu-remote bridge: is mu-remote installed there?".into()),
                None => return Err("the stream ended without an answer".into()),
            }
            let third = format!("{}\n", hs.write()?);
            to.write_all(third.as_bytes()).await.map_err(io)?;
            let (mut sealer, mut opener, _) = hs.finish()?;
            secure::write_line(&mut to, &mut sealer, &crate::link::wire::resume(0)).await.map_err(io)?;
            to.flush().await.map_err(io)?;
            let first = secure::next_line(&mut from, &mut opener).await?.ok_or("the worker closed the stream")?;
            let got: serde_json::Value = serde_json::from_str(&first).map_err(|e| format!("{first:?}: {e}"))?;
            let want: serde_json::Value =
                serde_json::from_str(&crate::link::wire::resume(0)).map_err(|e| e.to_string())?;
            if got == want { Ok(()) } else { Err(format!("the worker's first line was {first}, not {want}")) }
        };
        tokio::time::timeout(CONNECT_WAIT, work)
            .await
            .unwrap_or_else(|_| Err(format!("no answer within {} s", CONNECT_WAIT.as_secs())))
    });
    let _ = child.kill();
    let stderr = child.stderr.take();
    let _ = child.wait();
    result.map_err(|e| {
        let mut said = String::new();
        if let Some(mut s) = stderr {
            let _ = std::io::Read::read_to_string(&mut s, &mut said);
        }
        match said.lines().next() {
            Some(l) if !l.trim().is_empty() => format!("{e} ({})", l.trim()),
            _ => e,
        }
    })
}

fn worker_steps(a: &Adapter, work_dir: &str, steps: &mut Steps) {
    let launch = format!("check-{}", hex8());
    let (tmp, bundle) = match synthetic_bundle(&launch) {
        Ok(b) => b,
        Err(e) => return steps.add("worker start", Outcome::Failed(e)),
    };
    let dir = format!("{}/{launch}", work_dir.trim_end_matches('/'));
    let started = a.start(&Start::Worker { launch, bundle, dir }, &mut |_| {});
    let _ = std::fs::remove_dir_all(&tmp);
    let id = match started {
        Ok(id) => {
            steps.add("worker start", Outcome::Ok(None));
            id
        }
        Err(e) => {
            steps.add("worker start", Outcome::Failed(first_line(&e)));
            for s in ["worker connect", "worker fetch", "worker stop", "worker gone"] {
                steps.add(s, Outcome::Skipped("no worker started".into()));
            }
            return;
        }
    };
    steps.add("worker connect", probe_connect(a, &id).map_or_else(Outcome::Failed, |()| Outcome::Ok(None)));
    let mut bundle = Vec::new();
    steps.add(
        "worker fetch",
        match a.fetch(&id, &mut bundle) {
            Err(e) => Outcome::Failed(first_line(&e)),
            Ok(()) if bundle.is_empty() => Outcome::Ok(None),
            Ok(()) => Outcome::Failed(format!("{} bytes came back for a worker that changed nothing", bundle.len())),
        },
    );
    steps.add("worker stop", a.stop(&id).map_or_else(|e| Outcome::Failed(first_line(&e)), |()| Outcome::Ok(None)));
    steps.add(
        "worker gone",
        match a.list(Some(Duration::from_secs(30))) {
            Err(e) => Outcome::Failed(first_line(&e)),
            Ok(l) if l.iter().any(|w| w.id == id) => Outcome::Failed(format!("`list` still shows {id}")),
            Ok(_) => Outcome::Ok(None),
        },
    );
}

fn unsupported(e: &anyhow::Error) -> bool {
    e.downcast_ref::<Unsupported>().is_some()
}

fn file_and_port_steps(a: &Adapter, agent: Option<&str>, steps: &mut Steps) {
    let Some(agent) = agent else {
        for s in ["files", "ports"] {
            steps.add(s, Outcome::Skipped("pass --agent ID to try them on an agent".into()));
        }
        return;
    };
    if !a.trusted() {
        for s in ["files", "ports"] {
            steps.add(s, Outcome::Skipped("not trusted".into()));
        }
        return;
    }
    let path = format!("/tmp/mu-remote-check-{}", hex8());
    let bytes = *uuid::Uuid::new_v4().as_bytes();
    let wrote = a.write(agent, &path, &mut &bytes[..]);
    let files = match wrote {
        Err(e) if unsupported(&e) => Outcome::Skipped("not supported".into()),
        Err(e) => Outcome::Failed(first_line(&e)),
        Ok(()) => {
            let mut back = Vec::new();
            match a.read(agent, &path, &mut back) {
                Err(e) => Outcome::Failed(first_line(&e)),
                Ok(()) if back == bytes => Outcome::Ok(Some(format!(
                    "wrote and read back {path}; there is no delete verb, remove it yourself"
                ))),
                Ok(()) => Outcome::Failed(format!("{path} read back as other bytes")),
            }
        }
    };
    steps.add("files", files);
    let ports = match crate::helper::pick_local(0, false) {
        Err(e) => Outcome::Failed(first_line(&e)),
        Ok(local) => match a.forward(agent, 9, local, Some(Duration::from_secs(30))) {
            Err(e) if unsupported(&e) => Outcome::Skipped("not supported".into()),
            Err(e) => Outcome::Failed(first_line(&e)),
            Ok(()) => match a.unforward(agent, 9, local, Some(Duration::from_secs(30))) {
                Err(e) => Outcome::Failed(format!("unforward: {}", first_line(&e))),
                Ok(()) => Outcome::Ok(None),
            },
        },
    };
    steps.add("ports", ports);
}

/// The lines `check --full` adds after `version` and `list`, and whether none failed.
pub fn full(a: &Adapter, agent: Option<&str>) -> (String, bool) {
    let mut steps = Steps { text: String::new(), ok: true };
    let work_dir = a.setting("work_dir").unwrap_or_else(|| crate::client::DEFAULT_WORK_DIR.to_string());
    worker_steps(a, &work_dir, &mut steps);
    file_and_port_steps(a, agent, &mut steps);
    (steps.text, steps.ok)
}
