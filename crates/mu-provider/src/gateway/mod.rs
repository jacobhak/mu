pub mod quota;

use anyhow::{Result, anyhow};

fn token_from_env() -> Result<String> {
    match std::env::var("MU_GATEWAY_TOKEN") {
        Ok(t) if !t.trim().is_empty() => Ok(t.trim().to_string()),
        _ => Err(anyhow!("MU_GATEWAY_URL is set but MU_GATEWAY_TOKEN is not")),
    }
}

/// `MU_GATEWAY_URL` with its token, validated before any request is made. `None` when unset.
pub fn env_endpoint() -> Option<Result<(String, String)>> {
    let url = std::env::var("MU_GATEWAY_URL").ok().filter(|u| !u.is_empty())?;
    Some(crate::net::validate_url(&url).and_then(|u| Ok((u, token_from_env()?))))
}
