use anyhow::{Result, anyhow};
use mu_common::paths;
use serde_json::Value;

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
    Some(crate::net::validate_url(&url).and_then(|u| Ok((u, token_from_env_or_file()?))))
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
    let url = crate::net::validate_url(state["url"].as_str()?).ok()?;
    Some((url, token_from_env_or_file().ok()?))
}
