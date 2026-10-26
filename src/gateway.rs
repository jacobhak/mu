use crate::plugin::PluginHost;
use anyhow::{Result, anyhow};
use mu_common::paths;
use serde_json::Value;
use std::rc::Rc;

/// Where gateway requests go: `MU_GATEWAY_URL` if set, else the gateway plugin.
pub struct GatewaySource {
    host: Rc<PluginHost>,
}

impl GatewaySource {
    pub fn new(host: Rc<PluginHost>) -> Self {
        Self { host }
    }

    pub async fn endpoint(&self) -> Result<(String, String)> {
        match env_endpoint() {
            Some(r) => r,
            _ => self.host.provider_endpoint("gateway").await.map_err(|e| anyhow!(e)),
        }
    }
}

/// Accepts `https://…`, or `http://` only to a loopback host. The bearer token must never go
/// over plain HTTP to anywhere else.
pub fn validate_url(url: &str) -> Result<String> {
    let url = url.trim().trim_end_matches('/');
    let (scheme, rest) = url.split_once("://").ok_or_else(|| anyhow!("gateway URL `{url}` has no scheme"))?;
    let auth = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let auth = auth.rsplit_once('@').map_or(auth, |(_, h)| h);
    let host = match auth.strip_prefix('[') {
        Some(v6) => v6.split_once(']').map_or(v6, |(h, _)| h),
        None => auth.split(':').next().unwrap_or_default(),
    };
    if host.is_empty() {
        return Err(anyhow!("gateway URL `{url}` has no host"));
    }
    let loopback =
        host.eq_ignore_ascii_case("localhost") || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback());
    match scheme.to_ascii_lowercase().as_str() {
        "https" => Ok(url.to_string()),
        "http" if loopback => Ok(url.to_string()),
        "http" => Err(anyhow!("refusing gateway URL `{url}`: http is only allowed to loopback; use https")),
        other => Err(anyhow!("refusing gateway URL `{url}`: unsupported scheme `{other}`")),
    }
}

fn token_from_env_or_file() -> Result<String> {
    match std::env::var("MU_GATEWAY_TOKEN") {
        Ok(t) if !t.trim().is_empty() => Ok(t.trim().to_string()),
        _ => {
            let p = paths::omp_config_root().join("auth-gateway.token");
            std::fs::read_to_string(&p)
                .map(|s| s.trim().to_string())
                .map_err(|_| anyhow!("gateway token not found at {}; run `omp auth-gateway token`", p.display()))
        }
    }
}

/// `MU_GATEWAY_URL` with its token, validated before any request is made. `None` when unset.
pub fn env_endpoint() -> Option<Result<(String, String)>> {
    let url = std::env::var("MU_GATEWAY_URL").ok().filter(|u| !u.is_empty())?;
    Some(validate_url(&url).and_then(|u| Ok((u, token_from_env_or_file()?))))
}

/// The gateway plugin's per-profile state directory.
pub fn dir() -> std::path::PathBuf {
    paths::profile_subdir(paths::mu_home().join("gateway"), paths::profile().as_deref())
}

/// URL and token of an already-running gateway; never starts one. `None` when there is none.
pub fn running_endpoint() -> Option<(String, String)> {
    if let Some(r) = env_endpoint() {
        return r.ok();
    }
    let state: Value = serde_json::from_str(&std::fs::read_to_string(dir().join("state.json")).ok()?).ok()?;
    let pid = libc::pid_t::try_from(state["pid"].as_i64()?).ok().filter(|p| *p > 1)?;
    // SAFETY: signal 0 only checks that the process exists.
    if unsafe { libc::kill(pid, 0) } != 0 {
        return None;
    }
    let url = validate_url(state["url"].as_str()?).ok()?;
    Some((url, token_from_env_or_file().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_https_or_loopback_http() {
        assert!(validate_url("https://gw.example.com/").is_ok());
        assert!(validate_url("http://127.0.0.1:8080").is_ok());
        assert!(validate_url("http://localhost:1").is_ok());
        assert!(validate_url("http://[::1]:1").is_ok());
        assert!(validate_url("http://gw.example.com").is_err());
        assert!(validate_url("http://127.0.0.1.evil.com").is_err());
        assert!(validate_url("ftp://localhost").is_err());
        assert!(validate_url("gw.example.com").is_err());
    }
}
