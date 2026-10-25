//! Web plugin: `web_search` and `fetch` via `omp search` / `omp read`.

use mu_common::rpc::{Request, RpcError, serve_stdio};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;
use tokio::process::Command;

const TIMEOUT: Duration = Duration::from_secs(60);
const FETCH_CAP: usize = 100 * 1024;

static CWD: OnceLock<PathBuf> = OnceLock::new();

#[tokio::main(flavor = "current_thread")]
async fn main() {
    serve_stdio(|req: Request, _client| async move { dispatch(req).await }).await;
}

async fn dispatch(req: Request) -> Result<Value, RpcError> {
    match req.method.as_str() {
        "initialize" => {
            if let Some(cwd) = req.params.get("cwd").and_then(Value::as_str) {
                let _ = CWD.set(PathBuf::from(cwd));
            }
            Ok(json!({}))
        }
        "shutdown" => Ok(json!({})),
        "tool/call" => {
            let name = req.params.get("name").and_then(Value::as_str).unwrap_or_default();
            let args = req.params.get("arguments").cloned().unwrap_or(Value::Null);
            let (content, is_error) = match name {
                "web_search" => search(&args).await,
                "fetch" => fetch(&args).await,
                other => return Err(RpcError { code: -32601, message: format!("unknown tool {other}") }),
            };
            Ok(json!({ "content": content, "isError": is_error }))
        }
        other => Err(RpcError { code: -32601, message: format!("unknown method {other}") }),
    }
}

fn str_arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str).filter(|s| !s.trim().is_empty())
}

/// Runs `omp <args>` in the session cwd with a timeout. Returns (stdout, stderr, success).
async fn run_omp(args: &[String], env: &[(&str, &str)]) -> Result<(String, String, bool), String> {
    let mut cmd = Command::new(mu_common::paths::omp_bin());
    cmd.args(args).kill_on_drop(true).stdin(std::process::Stdio::null());
    for (k, v) in env {
        cmd.env(k, v);
    }
    if let Some(cwd) = CWD.get() {
        cmd.current_dir(cwd);
    }
    let child = cmd.output();
    match tokio::time::timeout(TIMEOUT, child).await {
        Err(_) => Err("omp timed out after 60s".into()),
        Ok(Err(e)) => Err(format!("failed to run omp: {e}")),
        Ok(Ok(out)) => Ok((
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
            out.status.success(),
        )),
    }
}

fn tail(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

async fn search(args: &Value) -> (String, bool) {
    let Some(query) = str_arg(args, "query") else {
        return ("query is required".into(), true);
    };
    let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(5);
    let mut argv = vec!["search".to_string(), "--limit".into(), limit.to_string()];
    if let Some(r) = str_arg(args, "recency") {
        argv.push("--recency".into());
        argv.push(r.into());
    }
    argv.push("--".into());
    argv.push(query.into());
    match run_omp(&argv, &[("COLUMNS", "200"), ("NO_COLOR", "1")]).await {
        Err(e) => (e, true),
        Ok((_, stderr, false)) => (format!("omp search failed:\n{}", tail(&stderr, 20)), true),
        Ok((stdout, _, true)) => {
            let cleaned = clean_search(&stdout);
            if cleaned.is_empty() { ("No results.".into(), false) } else { (cleaned, false) }
        }
    }
}

async fn fetch(args: &Value) -> (String, bool) {
    let Some(url) = str_arg(args, "url") else {
        return ("url is required".into(), true);
    };
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return ("fetch only supports http(s) URLs".into(), true);
    }
    let raw = args.get("raw").and_then(Value::as_bool).unwrap_or(false);
    let target = if raw { format!("{url}:raw") } else { url.to_string() };
    match run_omp(&["read".into(), target], &[("COLUMNS", "200"), ("NO_COLOR", "1")]).await {
        Err(e) => (e, true),
        Ok((_, stderr, false)) => (format!("omp read failed:\n{}", tail(&stderr, 20)), true),
        Ok((stdout, _, true)) => (truncate(stdout), false),
    }
}

fn truncate(mut s: String) -> String {
    if s.len() > FETCH_CAP {
        let mut cut = FETCH_CAP;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
        s.push_str("\n[truncated at 100KB]");
    }
    s
}

/// Removes ANSI CSI sequences (`ESC [ … final`).
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            chars.next();
            for d in chars.by_ref() {
                if ('\x40'..='\x7e').contains(&d) {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Cleans `omp search` box-drawn output: drops frame rules, unboxes content lines,
/// and stops at the `Metadata` section.
pub fn clean_search(raw: &str) -> String {
    let stripped = strip_ansi(raw);
    let mut out = Vec::new();
    for line in stripped.lines() {
        let line = line.trim_end();
        if line.contains("─── Metadata") {
            break;
        }
        let t = line.trim_start();
        if t.starts_with(['╭', '├', '╰']) {
            continue;
        }
        let l = match t.strip_prefix('│') {
            Some(rest) => {
                let rest = rest.strip_prefix(' ').unwrap_or(rest);
                rest.strip_suffix('│').unwrap_or(rest).trim_end()
            }
            None => line,
        };
        // source-list tree markers (box chars outside the frame)
        let l = l.strip_prefix("├─ ").or_else(|| l.strip_prefix("└─ ")).unwrap_or(l);
        out.push(l);
    }
    while out.last().is_some_and(|l| l.is_empty()) {
        out.pop();
    }
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    const REAL: &str = include_str!("../fixtures/search.txt");

    #[test]
    fn cleans_real_output() {
        let c = clean_search(REAL);
        assert!(c.starts_with("Query: crossterm KeyboardEnhancementFlags"), "{c}");
        assert!(c.contains("[1] KeyboardEnhancementFlags in crossterm - Rust"));
        assert!(c.contains("https://docs.rs/libcrossterm/latest/crossterm/enum.KeyboardEnhancementFlags.html"));
        assert!(c.contains("https://github.com/crossterm-rs/crossterm/blob/master/examples/event-read.rs"));
        assert!(c.contains("KeyboardEnhancementFlags in crossterm - Rust (docs.rs)"));
        assert!(!c.contains("Metadata"));
        assert!(!c.contains("Provider:"));
        for ch in ['│', '╭', '╰', '─', '\x1b'] {
            assert!(!c.contains(ch), "leftover {ch:?}");
        }
    }

    #[test]
    fn empty_is_empty() {
        assert_eq!(clean_search("╭──╮\n╰──╯\n"), "");
    }

    #[test]
    fn truncates_on_char_boundary() {
        let s = "é".repeat(FETCH_CAP);
        let t = truncate(s);
        assert!(t.ends_with("\n[truncated at 100KB]"));
        assert!(t.len() <= FETCH_CAP + 32);
    }

    #[test]
    #[ignore]
    fn contract_omp_read() {
        let out = mu_common::paths::run_omp_blocking(&["read", "https://example.com"], &[("COLUMNS", "200"), ("NO_COLOR", "1")]);
        assert!(out.contains("Example Domain"), "{out}");
    }
}
