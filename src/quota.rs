//! Subscription usage windows (`5h`, `7d`) from the gateway's `GET /v1/usage`.
//!
//! Only an already-running gateway is queried: the lookup reads `state.json` and checks its pid, so
//! refreshing never starts one. State lives in `$MU_HOME/gateway/` (`profiles/<name>/` under an omp profile).
//! The gateway caches each account's usage for 5 minutes and fetches it upstream on a miss, so a call can
//! take seconds; callers run it off the agent loop.
use mu_common::paths;
use serde_json::Value;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Debug, PartialEq)]
pub struct QuotaWindow {
    /// `5h`, `1d` or `7d`.
    pub label: String,
    pub used_percent: f64,
    pub resets_in: Duration,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProviderUsage {
    pub provider: String,
    pub windows: Vec<QuotaWindow>,
}

/// The windows reported for `provider`. Tiered (per-model) limits are skipped.
pub fn parse_usage(body: &str, provider: &str, now_ms: i64) -> Option<ProviderUsage> {
    let v: Value = serde_json::from_str(body).ok()?;
    let report = v["reports"].as_array()?.iter().find(|r| r["provider"].as_str() == Some(provider))?;
    let mut windows: Vec<QuotaWindow> = Vec::new();
    for limit in report["limits"].as_array()? {
        if !limit["scope"]["tier"].is_null() {
            continue;
        }
        let label = match limit["window"]["id"].as_str()? {
            "5h" => "5h",
            "1d" => "1d",
            "7d" => "7d",
            _ => continue,
        };
        if windows.iter().any(|w| w.label == label) {
            continue;
        }
        let amount = &limit["amount"];
        let used_percent = match (amount["usedFraction"].as_f64(), amount["used"].as_f64(), amount["limit"].as_f64()) {
            (Some(f), _, _) => f * 100.0,
            (None, Some(u), Some(l)) if l > 0.0 => u / l * 100.0,
            _ => continue,
        };
        let resets_at = limit["window"]["resetsAt"].as_i64().unwrap_or(now_ms);
        let resets_in = Duration::from_millis((resets_at - now_ms).max(0) as u64);
        windows.push(QuotaWindow { label: label.into(), used_percent, resets_in });
    }
    (!windows.is_empty()).then(|| ProviderUsage { provider: provider.into(), windows })
}

/// Mirrors the gateway plugin's per-profile state directory.
fn gateway_dir() -> std::path::PathBuf {
    let base = paths::mu_home().join("gateway");
    match paths::profile() {
        Some(p) if !p.contains(['/', '\\']) && p != "." && p != ".." => base.join("profiles").join(p),
        _ => base,
    }
}

/// Gateway URL and bearer token of a running gateway, or `None` when none is running.
fn running_gateway() -> Option<(String, String)> {
    if let Some(url) = std::env::var("MU_GATEWAY_URL").ok().filter(|u| !u.is_empty()) {
        let token = std::env::var("MU_GATEWAY_TOKEN")
            .ok()
            .filter(|t| !t.trim().is_empty())
            .or_else(|| std::fs::read_to_string(paths::omp_config_root().join("auth-gateway.token")).ok())?;
        return Some((url.trim_end_matches('/').into(), token.trim().into()));
    }
    let state: Value = serde_json::from_str(&std::fs::read_to_string(gateway_dir().join("state.json")).ok()?).ok()?;
    let pid = state["pid"].as_i64()? as libc::pid_t;
    // SAFETY: signal 0 only checks that the process exists.
    if unsafe { libc::kill(pid, 0) } != 0 {
        return None;
    }
    let url = state["url"].as_str()?.trim_end_matches('/').to_string();
    let token = std::fs::read_to_string(paths::omp_config_root().join("auth-gateway.token")).ok()?;
    Some((url, token.trim().into()))
}

pub enum Fetch {
    Got(ProviderUsage),
    /// No gateway is running; checking again is a local file read.
    NoGateway,
    /// The request failed, timed out, or the report has no windows for the provider.
    Failed,
}

pub async fn fetch(provider: &str) -> Fetch {
    let Some((url, token)) = running_gateway() else { return Fetch::NoGateway };
    let body = async {
        let client = reqwest::Client::builder().timeout(TIMEOUT).build().ok()?;
        client.get(format!("{url}/v1/usage")).bearer_auth(token).send().await.ok()?.error_for_status().ok()?.text().await.ok()
    };
    let body = body.await;
    let now_ms = jiff::Timestamp::now().as_millisecond();
    match body.and_then(|b| parse_usage(&b, provider, now_ms)) {
        Some(u) => Fetch::Got(u),
        None => Fetch::Failed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_windows_for_one_provider() {
        let body = r#"{"generatedAt":0,"reports":[
          {"provider":"openai-codex","limits":[]},
          {"provider":"anthropic","limits":[
            {"scope":{"provider":"anthropic","windowId":"5h","shared":true},"window":{"id":"5h","resetsAt":8200000},
             "amount":{"used":87,"limit":100,"usedFraction":0.87,"unit":"percent"}},
            {"scope":{"provider":"anthropic","windowId":"7d","tier":"fable"},"window":{"id":"7d","resetsAt":0},
             "amount":{"used":99,"limit":100,"usedFraction":0.99,"unit":"percent"}},
            {"scope":{"provider":"anthropic","windowId":"7d","shared":true},"window":{"id":"7d","resetsAt":346600000},
             "amount":{"used":17,"limit":100,"usedFraction":0.17,"unit":"percent"}}
          ]}]}"#;
        let u = parse_usage(body, "anthropic", 1_000_000).unwrap();
        assert_eq!(u.windows.len(), 2);
        assert_eq!(u.windows[0].label, "5h");
        assert!((u.windows[0].used_percent - 87.0).abs() < 1e-9);
        assert_eq!(u.windows[0].resets_in, Duration::from_secs(7200));
        assert_eq!(u.windows[1].label, "7d");
        assert!((u.windows[1].used_percent - 17.0).abs() < 1e-9);
        assert!(parse_usage(body, "google", 0).is_none());
    }
}
