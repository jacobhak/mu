//! The loopback callback server and the pasted-URL path of a sign-in.

use super::{CALLBACK_PATH, CALLBACK_PORT, DYNAMIC_CLIENT_ID};
use anyhow::{Context, Result};
use reqwest::Url;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc::UnboundedSender;

const READ_LIMIT: Duration = Duration::from_secs(5);
const MAX_HEAD: usize = 16 * 1024;
const PAGE_OK: &str = "<!doctype html><meta charset=\"utf-8\"><title>mu</title>\
    <p>Signed in to ChatGPT. You can close this tab and return to mu.</p>";
const PAGE_FAILED: &str = "<!doctype html><meta charset=\"utf-8\"><title>mu</title>\
    <p>The ChatGPT sign-in did not complete. Return to mu for details.</p>";
const PAGE_NOT_FOUND: &str = "<!doctype html><meta charset=\"utf-8\"><title>mu</title><p>Not found.</p>";

/// What a callback means for the sign-in.
pub(super) enum Step {
    Done(Issued),
    /// Not this sign-in's callback (a stale tab, a wrong paste): say why and keep waiting.
    Retry(String),
    /// The attempt ends with this message.
    Fail(String),
}

pub(super) struct Issued {
    pub code: String,
    pub client_id: String,
}

/// What a callback must match: this sign-in's state, and the registration it reauthorizes.
pub(super) struct Expect {
    pub state: String,
    pub known: Option<String>,
}

/// Port 1455 for a first registration, or for a reauthorization while it is free. A reauthorization
/// falls back to any free port. `port` overrides both (tests).
pub(super) async fn bind(port: Option<u16>, reauth: bool) -> Result<TcpListener> {
    if let Some(p) = port {
        return TcpListener::bind(("127.0.0.1", p)).await.context("binding the sign-in callback port");
    }
    match TcpListener::bind(("127.0.0.1", CALLBACK_PORT)).await {
        Ok(l) => Ok(l),
        Err(_) if reauth => TcpListener::bind(("127.0.0.1", 0)).await.context("binding the sign-in callback port"),
        Err(e) => Err(anyhow::anyhow!(
            "cannot listen on 127.0.0.1:{CALLBACK_PORT} ({e}). Another mu sign-in or the Codex CLI may hold the port; \
             cancel that sign-in and try again."
        )),
    }
}

/// Maps the query of a callback URL to a step. `state` is checked first, so a request that does not
/// carry this sign-in's state (any local page can reach the port) never ends it; then `error`, then
/// `code`. The provider's `error` text is never echoed.
pub(super) fn read_callback(params: &[(String, String)], expect: &Expect) -> Step {
    let get = |name: &str| params.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str()).filter(|v| !v.is_empty());
    if get("state") != Some(expect.state.as_str()) {
        return Step::Retry("That callback belongs to another sign-in; keep waiting for this one.".into());
    }
    if let Some(err) = get("error") {
        return Step::Fail(if err == "access_denied" {
            "ChatGPT sign-in was not approved.".to_string()
        } else {
            "ChatGPT sign-in failed; the provider reported an error.".to_string()
        });
    }
    let Some(code) = get("code") else {
        return Step::Retry("The callback has no authorization code.".into());
    };
    let client_id = match (&expect.known, get("client_id")) {
        (Some(known), None) => known.clone(),
        (Some(known), Some(id)) if id == known.as_str() => id.to_string(),
        (Some(_), Some(_)) => {
            return Step::Fail(
                "ChatGPT returned a different client id than the saved registration; run /login to sign in again."
                    .into(),
            );
        }
        (None, Some(id)) => id.to_string(),
        (None, None) => {
            return Step::Fail(
                "ChatGPT did not return a client id for the new registration; the sign-in cannot complete.".into(),
            );
        }
    };
    if client_id == DYNAMIC_CLIENT_ID {
        return Step::Fail("ChatGPT returned the registration placeholder, not an issued client id.".into());
    }
    Step::Done(Issued { code: code.to_string(), client_id })
}

/// A pasted redirect URL. It must be this listener's callback; anything else is a retry, not a failure.
pub(super) fn read_paste(text: &str, port: u16, expect: &Expect) -> Step {
    let Ok(url) = Url::parse(text.trim()) else {
        return Step::Retry(paste_hint(port));
    };
    let ours = url.scheme() == "http"
        && url.host_str() == Some("127.0.0.1")
        && url.port_or_known_default() == Some(port)
        && url.path() == CALLBACK_PATH;
    if !ours {
        return Step::Retry(paste_hint(port));
    }
    let params: Vec<(String, String)> = url.query_pairs().into_owned().collect();
    read_callback(&params, expect)
}

fn paste_hint(port: u16) -> String {
    format!("Paste the full address from the browser, starting with http://127.0.0.1:{port}{CALLBACK_PATH}.")
}

/// Answers one connection. Connections that send nothing, or another path, get no step: browsers
/// open idle connections ahead of time, and those must not stall the sign-in.
pub(super) async fn serve(mut stream: TcpStream, expect: Arc<Expect>, steps: UnboundedSender<Step>) {
    let Some(head) = read_head(&mut stream).await else {
        return;
    };
    let base = Url::parse("http://127.0.0.1/").ok();
    let target = head.lines().next().and_then(|line| {
        let mut parts = line.split_whitespace();
        match (parts.next(), parts.next()) {
            (Some("GET"), Some(t)) => Some(t),
            _ => None,
        }
    });
    let url = base.zip(target).and_then(|(b, t)| b.join(t).ok()).filter(|u| u.path() == CALLBACK_PATH);
    let Some(url) = url else {
        respond(&mut stream, "404 Not Found", PAGE_NOT_FOUND).await;
        return;
    };
    let params: Vec<(String, String)> = url.query_pairs().into_owned().collect();
    let step = read_callback(&params, &expect);
    match step {
        Step::Done(_) => respond(&mut stream, "200 OK", PAGE_OK).await,
        _ => respond(&mut stream, "400 Bad Request", PAGE_FAILED).await,
    }
    let _ = steps.send(step);
}

async fn read_head(stream: &mut TcpStream) -> Option<String> {
    tokio::time::timeout(READ_LIMIT, async {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 1024];
        while buf.len() < MAX_HEAD && !buf.windows(4).any(|w| w == b"\r\n\r\n") {
            match stream.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        }
        (!buf.is_empty()).then(|| String::from_utf8_lossy(&buf).into_owned())
    })
    .await
    .ok()
    .flatten()
}

async fn respond(stream: &mut TcpStream, status: &str, body: &str) {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes()).await;
    let _ = stream.write_all(body.as_bytes()).await;
    let _ = stream.shutdown().await;
}
