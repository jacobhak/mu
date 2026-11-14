//! `mu attach`: joins this process's stdin and stdout to a running `mu agent`'s socket. It knows nothing
//! of the lines it copies, so `ssh host mu attach` and `kubectl exec -i pod -- mu attach` carry the
//! protocol to a client on another machine.

use anyhow::{Result, bail};
use mu_common::paths;
use mu_session::session::{list, owner};
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;

/// The socket of the agent `session` (an id prefix) names, or of the one agent running in this directory.
fn find_socket(session: Option<&str>, cwd: &Path) -> Result<PathBuf> {
    let run = paths::profile_home().join("run");
    let sockets = |keep: &dyn Fn(&str) -> bool| -> Vec<PathBuf> {
        let mut found: Vec<PathBuf> = std::fs::read_dir(&run)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                let id = name.strip_suffix(".sock")?;
                keep(id).then(|| e.path())
            })
            .collect();
        found.sort();
        found
    };
    if let Some(prefix) = session {
        let mut found = sockets(&|id| id.starts_with(prefix));
        return match found.len() {
            0 => bail!("no agent with a session starting {prefix} is running"),
            1 => Ok(found.remove(0)),
            _ => bail!("several agents match {prefix}: {}", ids(&found)),
        };
    }
    let here: Vec<String> = owner::live_sessions()
        .into_iter()
        .filter(|s| s.kind.as_deref() == Some("agent") && s.cwd.as_deref() == Some(cwd))
        .map(|s| list::id_of(&s.path))
        .collect();
    let mut found = sockets(&|id| here.iter().any(|h| h == id));
    match found.len() {
        0 => bail!("no agent is running in {}; start one with `mu agent`", cwd.display()),
        1 => Ok(found.remove(0)),
        _ => bail!("several agents run in {}: {}; name one", cwd.display(), ids(&found)),
    }
}

fn ids(sockets: &[PathBuf]) -> String {
    sockets
        .iter()
        .filter_map(|p| p.file_stem())
        .map(|s| s.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(", ")
}

pub fn run(session: Option<&str>, socket: Option<PathBuf>) -> Result<i32> {
    let socket = match socket {
        Some(s) => s,
        None => find_socket(session, &std::env::current_dir()?)?,
    };
    let rt = mu_base::runtime::current_thread_runtime()?;
    rt.block_on(async move {
        let stream = match UnixStream::connect(&socket).await {
            Ok(s) => s,
            Err(e) => bail!("cannot connect to {}: {e}", socket.display()),
        };
        let (mut from_agent, mut to_agent) = stream.into_split();
        let up = async {
            let _ = tokio::io::copy(&mut tokio::io::stdin(), &mut to_agent).await;
            let _ = to_agent.shutdown().await;
        };
        let down = async {
            let mut out = tokio::io::stdout();
            let r = tokio::io::copy(&mut from_agent, &mut out).await;
            let _ = out.flush().await;
            r
        };
        // The agent closing the connection ends the attach; so does the client closing its stdin once
        // the agent has answered.
        let (_, down) = tokio::join!(up, down);
        match down {
            Ok(_) => Ok(0),
            Err(e) => bail!("connection to {} lost: {e}", socket.display()),
        }
    })
}
