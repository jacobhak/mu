//! Connect-time values for an MCP server: `!cmd` / env-name resolution, and OAuth tokens from
//! `omp token`. mu never reads omp's credential store or holds a refresh token.

use super::config::{Kind, McpServer};
use crate::proc::run_capture;
use std::path::Path;
use std::time::Duration;
use tokio::process::Command;

/// omp's rule: `!cmd` runs in `dir` (10 s cap), else an env var of that name, else the literal.
/// An empty result is `None`, which drops the key.
pub async fn resolve_value(v: &str, dir: &Path) -> Option<String> {
    let out = if let Some(cmd) = v.strip_prefix('!') {
        let mut c = Command::new("sh");
        c.arg("-c").arg(cmd).current_dir(dir);
        run_capture(c, Duration::from_secs(10)).await?
    } else if let Ok(e) = std::env::var(v) {
        e
    } else {
        v.to_string()
    };
    let out = out.trim().to_string();
    (!out.is_empty()).then_some(out)
}

pub async fn resolve_pairs(pairs: &[(String, String)], dir: &Path) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (k, v) in pairs {
        if let Some(r) = resolve_value(v, dir).await {
            out.push((k.clone(), r));
        }
    }
    out
}

/// The credential ids to ask `omp token` for, in order (omp's lookup order).
pub fn credential_ids(s: &McpServer, profile: Option<&str>) -> Vec<String> {
    if let Some(id) = s.credential_id() {
        return vec![id.to_string()];
    }
    let Kind::Http { url, headers } = &s.kind else { return Vec::new() };
    if headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("authorization")) {
        return Vec::new();
    }
    let p = profile.filter(|p| !p.is_empty()).unwrap_or("default");
    let mut urls = vec![url.clone()];
    if let Some(raw) = s.raw_url.as_ref().filter(|r| *r != url) {
        urls.push(raw.clone());
    }
    urls.into_iter().map(|u| format!("mcp_oauth:profile:{p}:{u}")).collect()
}

/// Asks omp for an access token: `omp token [--force-refresh] <id>`, with the broker environment
/// when `broker` is set so the broker is the only writer. `None` when omp has none.
pub async fn fetch_token(ids: &[String], force: bool, broker: Option<&(String, String)>) -> Option<String> {
    fetch_token_with(&mu_common::paths::omp_bin(), ids, force, broker).await
}

/// `fetch_token` with the omp binary given, so tests need no process-wide `MU_OMP`.
async fn fetch_token_with(
    omp: &Path,
    ids: &[String],
    force: bool,
    broker: Option<&(String, String)>,
) -> Option<String> {
    for id in ids {
        let mut c = Command::new(omp);
        c.arg("token");
        if force {
            c.arg("--force-refresh");
        }
        c.arg(id);
        if let Some((url, token)) = broker {
            c.env("OMP_AUTH_BROKER_URL", url).env("OMP_AUTH_BROKER_TOKEN", token);
        }
        if let Some(t) = run_capture(c, Duration::from_secs(20)).await {
            let t = t.trim().to_string();
            if !t.is_empty() {
                return Some(t);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::mcp::config::load_with;
    use mu_testutil::block_on as rt;
    use std::process::Stdio;

    #[test]
    fn values() {
        rt(async {
            let d = std::env::temp_dir();
            assert_eq!(resolve_value("!echo  x ", &d).await.as_deref(), Some("x"));
            assert_eq!(resolve_value("!true", &d).await, None, "empty drops the key");
            assert_eq!(resolve_value("!exit 3", &d).await, None);
            assert_eq!(resolve_value("literal-value", &d).await.as_deref(), Some("literal-value"));
            assert_eq!(resolve_value("PATH", &d).await, std::env::var("PATH").ok());
            assert_eq!(resolve_value("", &d).await, None);
        });
    }

    fn servers(json: &str) -> Vec<McpServer> {
        let d = std::env::temp_dir().join(format!("mu-mcpauth-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("mcp.json"), json).unwrap();
        let env = |k: &str| (k == "H").then(|| "host".to_string());
        let (s, _) = load_with(&d.join("p"), &d, &env);
        let _ = std::fs::remove_dir_all(d);
        s
    }

    #[test]
    fn tokens_come_from_omp_with_the_broker_env_and_force_flag() {
        let script = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/src/plugin/mcp/testdata/fake-omp.sh"));
        let ids = vec!["nosuch".to_string(), "good".to_string()];
        let broker = ("http://127.0.0.1:9".to_string(), "bt".to_string());
        rt(async {
            assert_eq!(fetch_token_with(script, &ids, false, None).await.as_deref(), Some("token good||"));
            let forced = fetch_token_with(script, &ids, true, Some(&broker)).await;
            assert_eq!(forced.as_deref(), Some("token --force-refresh good|http://127.0.0.1:9|bt"));
            assert_eq!(fetch_token_with(script, &["nosuch".to_string()], false, None).await, None);
        });
    }

    fn omp_output(args: &[&str]) -> std::process::Output {
        std::process::Command::new(mu_common::paths::omp_bin())
            .args(args)
            .stdin(Stdio::null())
            .output()
            .expect("cannot run omp")
    }

    /// `/login mcp:<name>` runs `omp -p --no-session '/mcp reauth <name>'`; for an unknown server omp
    /// must exit 1 saying so, without a model call. `scripts/maint` runs this on every omp bump.
    #[test]
    #[ignore = "contract: runs the real omp"]
    fn contract_omp_mcp_reauth_of_an_unknown_server_exits_1_with_not_found() {
        let o = omp_output(&["-p", "--no-session", "/mcp reauth mu-contract-nosuchserver"]);
        let text = String::from_utf8_lossy(&[o.stdout.as_slice(), o.stderr.as_slice()].concat()).into_owned();
        assert_eq!(o.status.code(), Some(1), "{text}");
        assert!(text.contains("not found"), "{text}");
    }

    /// `omp token <id>` prints the access token, and exits non-zero (printing none) without one.
    #[test]
    #[ignore = "contract: runs the real omp"]
    fn contract_omp_token_of_an_unknown_credential_exits_non_zero_without_output() {
        let o = omp_output(&["token", "mcp_oauth:profile:mu-contract:http://nosuch.invalid/mcp"]);
        assert!(!o.status.success());
        assert!(o.stdout.is_empty(), "{}", String::from_utf8_lossy(&o.stdout));
    }

    #[test]
    fn credential_ids_follow_omp() {
        let s = servers(
            r#"{"mcpServers":{
              "explicit":{"url":"http://a","auth":{"type":"oauth","credentialId":"mcp_oauth_9"}},
              "keyed":{"url":"https://${H}/mcp"},
              "hdr":{"url":"http://b","headers":{"authorization":"Bearer x"}},
              "stdio":{"command":"c"},
              "stdio2":{"command":"d","auth":{"type":"oauth","credentialId":"c2"}}}}"#,
        );
        let by = |n: &str| s.iter().find(|x| x.name == n).unwrap();
        assert_eq!(credential_ids(by("explicit"), Some("p")), vec!["mcp_oauth_9"]);
        assert_eq!(
            credential_ids(by("keyed"), Some("work")),
            vec!["mcp_oauth:profile:work:https://host/mcp", "mcp_oauth:profile:work:https://${H}/mcp"]
        );
        assert_eq!(credential_ids(by("keyed"), None)[0], "mcp_oauth:profile:default:https://host/mcp");
        assert!(credential_ids(by("hdr"), None).is_empty());
        assert!(credential_ids(by("stdio"), None).is_empty());
        assert_eq!(credential_ids(by("stdio2"), None), vec!["c2"]);
    }
}
