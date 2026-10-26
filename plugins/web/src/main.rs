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
    serve_stdio(|req: Request| async move { dispatch(req).await }).await;
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
    if let Err(e) = check_destination(url).await {
        return (e, true);
    }
    let raw = args.get("raw").and_then(Value::as_bool).unwrap_or(false);
    let target = if raw { format!("{url}:raw") } else { url.to_string() };
    match run_omp(&["read".into(), target], &[("COLUMNS", "200"), ("NO_COLOR", "1")]).await {
        Err(e) => (e, true),
        Ok((_, stderr, false)) => (format!("omp read failed:\n{}", tail(&stderr, 20)), true),
        Ok((stdout, _, true)) => (truncate(stdout), false),
    }
}

/// Splits `scheme://[user@]host[:port]/…` into host and port.
fn host_port(url: &str) -> Option<(String, u16)> {
    let (scheme, rest) = url.split_once("://")?;
    let default = if scheme.eq_ignore_ascii_case("https") { 443 } else { 80 };
    let auth = rest.split(['/', '?', '#']).next()?;
    let auth = auth.rsplit_once('@').map_or(auth, |(_, h)| h);
    let (host, port) = if let Some(v6) = auth.strip_prefix('[') {
        let (h, after) = v6.split_once(']')?;
        (h, after.strip_prefix(':'))
    } else {
        match auth.split_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (auth, None),
        }
    };
    if host.is_empty() {
        return None;
    }
    let port = match port.filter(|p| !p.is_empty()) {
        Some(p) => p.parse().ok()?,
        None => default,
    };
    Some((host.to_string(), port))
}

/// Addresses a fetch must not reach: loopback, link-local, private, unique-local, unspecified,
/// CGNAT and multicast.
fn is_internal(ip: std::net::IpAddr) -> bool {
    use std::net::IpAddr;
    match ip {
        IpAddr::V4(v) => {
            let o = v.octets();
            v.is_loopback()
                || v.is_private()
                || v.is_link_local()
                || v.is_unspecified()
                || v.is_broadcast()
                || v.is_multicast()
                || (o[0] == 100 && (64..128).contains(&o[1]))
        }
        IpAddr::V6(v) => {
            if let Some(m) = v.to_ipv4_mapped() {
                return is_internal(IpAddr::V4(m));
            }
            let s = v.segments();
            v.is_loopback()
                || v.is_unspecified()
                || v.is_multicast()
                || (s[0] & 0xfe00) == 0xfc00 // unique local
                || (s[0] & 0xffc0) == 0xfe80 // link local
        }
    }
}

/// Refuses URLs that resolve to internal addresses unless `MU_WEB_ALLOW_PRIVATE=1`.
/// Limit: `omp read` follows redirects itself, so a redirect target is not checked, and a DNS
/// answer can change between this check and the fetch.
async fn check_destination(url: &str) -> Result<(), String> {
    if std::env::var("MU_WEB_ALLOW_PRIVATE").is_ok_and(|v| v == "1") {
        return Ok(());
    }
    let (host, port) = host_port(url).ok_or("invalid URL")?;
    let addrs =
        tokio::net::lookup_host((host.as_str(), port)).await.map_err(|e| format!("cannot resolve {host}: {e}"))?;
    for a in addrs {
        if is_internal(a.ip()) {
            return Err(format!(
                "refusing to fetch {host}: it resolves to an internal address ({}). Set MU_WEB_ALLOW_PRIVATE=1 to allow.",
                a.ip()
            ));
        }
    }
    Ok(())
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
        let out = mu_common::paths::run_omp_blocking(
            &["read", "https://example.com"],
            &[("COLUMNS", "200"), ("NO_COLOR", "1")],
        );
        assert!(out.contains("Example Domain"), "{out}");
    }
}

#[cfg(test)]
mod ssrf_tests {
    use super::*;

    #[test]
    fn internal_addresses_are_refused() {
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "0.0.0.0",
            "100.64.0.1",
            "::1",
            "fd00::1",
            "fe80::1",
            "::ffff:127.0.0.1",
            "::",
        ] {
            assert!(is_internal(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["8.8.8.8", "1.1.1.1", "2606:4700::1111"] {
            assert!(!is_internal(ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn host_port_parsing() {
        assert_eq!(host_port("http://a.com/x"), Some(("a.com".into(), 80)));
        assert_eq!(host_port("https://u:p@a.com:8443/x?y"), Some(("a.com".into(), 8443)));
        assert_eq!(host_port("http://[::1]:9000/"), Some(("::1".into(), 9000)));
        assert_eq!(host_port("http:///x"), None);
    }

    #[test]
    fn localhost_fetch_is_blocked() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        assert!(rt.block_on(check_destination("http://127.0.0.1:1/")).is_err());
        assert!(rt.block_on(check_destination("http://169.254.169.254/latest")).is_err());
    }
}
