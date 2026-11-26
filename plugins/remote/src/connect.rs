//! `connect`, `bridge` and `key`: the encrypted channel to an agent. `connect` runs on the client machine
//! between `mu --attach` (plain lines) and the adapter's `connect` (sealed lines); `bridge` runs on the
//! remote, ends the channel and pipes to `mu attach --stdio`, or to a worker's socket.

use crate::adapter::Adapter;
use crate::config::Config;
use crate::rpc::Framed;
use crate::secure::{self, Pinned};
use anyhow::{Context, Result, anyhow, bail};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const HANDSHAKE_WAIT: Duration = Duration::from_secs(60);
const CLOSE_WAIT: Duration = Duration::from_secs(5);

fn runtime() -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_current_thread().enable_all().build()?)
}

fn msg(e: String) -> anyhow::Error {
    anyhow!(e)
}

/// Writes one plain line (with its newline) and flushes.
async fn put<W: tokio::io::AsyncWrite + Unpin>(w: &mut W, line: &str) -> std::io::Result<()> {
    w.write_all(line.as_bytes()).await?;
    w.write_all(b"\n").await?;
    w.flush().await
}

/// The next line of `r` without its newline; `None` at the end.
async fn read_plain<R: tokio::io::AsyncBufRead + Unpin>(r: &mut R, buf: &mut Vec<u8>) -> Option<String> {
    buf.clear();
    match r.read_until(b'\n', buf).await {
        Ok(0) | Err(_) => None,
        Ok(_) => {
            while matches!(buf.last(), Some(b'\n' | b'\r')) {
                buf.pop();
            }
            Some(String::from_utf8_lossy(buf).into_owned())
        }
    }
}

pub fn connect(name: &str, id: &str) -> Result<i32> {
    let cfg = Config::load()?;
    let (name, remote) = cfg.pick(Some(name))?;
    let adapter = Adapter::from_config(name, remote)?;
    let mut child = adapter.connect(id)?;
    let rt = runtime()?;
    let out = rt.block_on(connect_stream(&mut child, name, id));
    drop(child.stdin.take());
    let ended = std::time::Instant::now();
    while child.try_wait().ok().flatten().is_none() && ended.elapsed() < Duration::from_secs(2) {
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = child.kill();
    let _ = child.wait();
    rt.shutdown_background();
    out.map(|()| 0)
}

async fn connect_stream(child: &mut std::process::Child, name: &str, id: &str) -> Result<()> {
    let (Some(stdin), Some(stdout), Some(stderr)) = (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        bail!("adapter pipes missing");
    };
    let mut to_adapter = tokio::process::ChildStdin::from_std(stdin)?;
    let mut from_adapter = Framed::new(BufReader::new(tokio::process::ChildStdout::from_std(stdout)?));
    let errors = tokio::spawn(async move {
        let mut lines = BufReader::new(tokio::process::ChildStderr::from_std(stderr)?).lines();
        while let Some(l) = lines.next_line().await? {
            eprintln!("{l}");
        }
        std::io::Result::Ok(())
    });

    let mut hs = secure::Handshake::initiator(&secure::client_key().map_err(msg)?).map_err(msg)?;
    put(&mut to_adapter, &hs.write().map_err(msg)?).await.context("the adapter's connect ended at once")?;
    let first = tokio::time::timeout(HANDSHAKE_WAIT, from_adapter.next_line())
        .await
        .map_err(|_| anyhow!("agent {id} on {name} did not answer within {}s", HANDSHAKE_WAIT.as_secs()))?;
    let reply = match first {
        Some(crate::rpc::Line::Value(v)) if v.is_string() => v,
        Some(_) => bail!("the stream to agent {id} on {name} is not a mu-remote bridge: is mu-remote installed there?"),
        None => {
            let _ = tokio::time::timeout(Duration::from_secs(1), errors).await;
            bail!("the stream to agent {id} on {name} ended without an answer");
        }
    };
    hs.read(&reply).map_err(msg)?;
    let key = hs.remote_key().context("the agent showed no key")?;
    if secure::check_pin(&secure::pins_path(), name, id, &key).map_err(msg)? == Pinned::First {
        eprintln!(
            "first contact with agent {id} on {name}: its key is {}, now pinned. Compare it with `mu-remote key` run \
             there; a later change is refused.",
            secure::fingerprint(&key)
        );
    }
    put(&mut to_adapter, &hs.write().map_err(msg)?).await?;
    let (mut sealer, mut opener, _) = hs.finish().map_err(msg)?;

    let up = async {
        let mut stdin = BufReader::new(tokio::io::stdin());
        let mut buf = Vec::new();
        while let Some(line) = read_plain(&mut stdin, &mut buf).await {
            if secure::write_line(&mut to_adapter, &mut sealer, &line).await.is_err()
                || to_adapter.flush().await.is_err()
            {
                break;
            }
        }
    };
    let down = async {
        let mut stdout = tokio::io::stdout();
        loop {
            match secure::next_line(&mut from_adapter, &mut opener).await {
                Ok(Some(line)) => {
                    if put(&mut stdout, &line).await.is_err() {
                        return Ok(());
                    }
                }
                Ok(None) => return Ok(()),
                Err(e) => return Err(msg(e)),
            }
        }
    };
    tokio::select! {
        _ = up => Ok(()),
        r = down => r,
    }
}

pub fn key() -> Result<i32> {
    println!("{}", secure::bridge_key().map_err(msg)?.fingerprint());
    Ok(0)
}

pub fn bridge(id: &str) -> Result<i32> {
    let rt = runtime()?;
    let out = rt.block_on(async {
        match crate::worker::socket_of(id) {
            Some(socket) => to_worker(&socket).await,
            None => to_agent(id).await,
        }
    });
    rt.shutdown_background();
    out.map(|()| 0)
}

/// Raw lines both ways between stdio and the worker's socket: the worker ends its own channel.
async fn to_worker(socket: &std::path::Path) -> Result<()> {
    let stream = tokio::net::UnixStream::connect(socket)
        .await
        .with_context(|| format!("cannot reach the worker at {}", socket.display()))?;
    let (mut from_worker, mut to_worker) = stream.into_split();
    let up = async {
        let mut stdin = tokio::io::stdin();
        let _ = tokio::io::copy(&mut stdin, &mut to_worker).await;
        let _ = to_worker.shutdown().await;
        tokio::time::sleep(CLOSE_WAIT).await;
    };
    let down = async {
        let mut stdout = tokio::io::stdout();
        let mut chunk = vec![0u8; 64 * 1024];
        loop {
            match tokio::io::AsyncReadExt::read(&mut from_worker, &mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if stdout.write_all(&chunk[..n]).await.is_err() || stdout.flush().await.is_err() {
                        break;
                    }
                }
            }
        }
    };
    tokio::select! {
        _ = up => {}
        _ = down => {}
    }
    Ok(())
}

async fn to_agent(id: &str) -> Result<()> {
    let key = secure::bridge_key().map_err(msg)?;
    let mut lines = Framed::new(BufReader::new(tokio::io::stdin()));
    let mut stdout = tokio::io::stdout();
    let (mut sealer, mut opener, _) =
        tokio::time::timeout(HANDSHAKE_WAIT, secure::accept(&key, &mut lines, &mut stdout))
            .await
            .map_err(|_| anyhow!("no handshake within {}s", HANDSHAKE_WAIT.as_secs()))?
            .map_err(msg)?;

    let mut child = tokio::process::Command::new(crate::paths::mu_program())
        .args(["attach", "--stdio", id])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("cannot run mu attach --stdio")?;
    let (Some(mut agent_in), Some(agent_out)) = (child.stdin.take(), child.stdout.take()) else {
        bail!("mu attach pipes missing");
    };
    let up = async {
        loop {
            match secure::next_line(&mut lines, &mut opener).await {
                Ok(Some(line)) => {
                    if put(&mut agent_in, &line).await.is_err() {
                        return;
                    }
                }
                Ok(None) | Err(_) => return,
            }
        }
    };
    let down = async {
        let mut reader = BufReader::new(agent_out);
        let mut buf = Vec::new();
        while let Some(line) = read_plain(&mut reader, &mut buf).await {
            if secure::write_line(&mut stdout, &mut sealer, &line).await.is_err() || stdout.flush().await.is_err() {
                return;
            }
        }
    };
    let channel_ended = tokio::select! {
        _ = up => true,
        _ = down => false,
    };
    drop(agent_in);
    if channel_ended && tokio::time::timeout(CLOSE_WAIT, child.wait()).await.is_err() {
        let _ = child.kill().await;
    }
    let _ = child.kill().await;
    Ok(())
}
