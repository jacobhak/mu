use anyhow::{Result, anyhow};

/// Accepts `https://…`, or `http://` only to a loopback host. The bearer token must never go
/// over plain HTTP to anywhere else.
pub fn validate_url(url: &str) -> Result<String> {
    let url = url.trim().trim_end_matches('/');
    let (scheme, rest) = url.split_once("://").ok_or_else(|| anyhow!("URL `{url}` has no scheme"))?;
    let auth = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let auth = auth.rsplit_once('@').map_or(auth, |(_, h)| h);
    let host = match auth.strip_prefix('[') {
        Some(v6) => v6.split_once(']').map_or(v6, |(h, _)| h),
        None => auth.split(':').next().unwrap_or_default(),
    };
    if host.is_empty() {
        return Err(anyhow!("URL `{url}` has no host"));
    }
    let loopback =
        host.eq_ignore_ascii_case("localhost") || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback());
    match scheme.to_ascii_lowercase().as_str() {
        "https" => Ok(url.to_string()),
        "http" if loopback => Ok(url.to_string()),
        "http" => Err(anyhow!("refusing URL `{url}`: http is only allowed to loopback; use https")),
        other => Err(anyhow!("refusing URL `{url}`: unsupported scheme `{other}`")),
    }
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
