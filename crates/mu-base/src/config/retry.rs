//! `[retry]` in config.toml: how long a provider's `Retry-After` is waited out and how often a request is retried.

use serde::Deserialize;
use std::collections::BTreeMap;

/// How long a retryable error with a `Retry-After` is waited out when it is a request's turn to be
/// retried: `Retry-After` up to this is waited for, anything longer fails at once.
pub(crate) const DEFAULT_MAX_WAIT: std::time::Duration = std::time::Duration::from_secs(120);

/// Attempts per request, retries included, when `[retry] max_attempts` is not set.
pub(crate) const DEFAULT_MAX_ATTEMPTS: u32 = 5;

/// `[retry]`: when to wait out a provider's `Retry-After` (a 429, mostly). A request is retried when
/// the wait is at most `max_wait` (`"90s"`, `"30m"`, `"2h"`, `"1d"`, `"1h30m"`; default `"2m"`) or
/// when its context is less than `context_below` percent full (default: never). `[retry.providers.<id>]`
/// overrides both for one provider. `max_attempts` (1 to 20, default 5) bounds the attempts per request.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct MuRetry {
    pub max_wait: Option<String>,
    pub context_below: Option<f64>,
    pub max_attempts: Option<u32>,
    pub providers: BTreeMap<String, RetryRule>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct RetryRule {
    pub max_wait: Option<String>,
    pub context_below: Option<f64>,
}

/// A `[retry]` section resolved for one provider.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RetryPolicy {
    pub max_wait: std::time::Duration,
    /// Percent of the context window.
    pub context_below: Option<f64>,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self { max_wait: DEFAULT_MAX_WAIT, context_below: None }
    }
}

impl RetryPolicy {
    /// The longest `Retry-After` worth waiting for a request whose context is `fill` percent full.
    pub fn limit(&self, fill: f64) -> std::time::Duration {
        match self.context_below {
            Some(below) if fill < below => std::time::Duration::MAX,
            _ => self.max_wait,
        }
    }
}

impl MuRetry {
    /// Attempts per request: `max_attempts` when it is 1 to 20, else the default (as `check_retry` warns).
    pub fn max_attempts(&self) -> u32 {
        self.max_attempts.filter(|n| (1..=20).contains(n)).unwrap_or(DEFAULT_MAX_ATTEMPTS)
    }

    pub fn policy(&self, provider: &str) -> RetryPolicy {
        let own = self.providers.get(provider);
        let max_wait = own
            .and_then(|r| r.max_wait.as_deref())
            .or(self.max_wait.as_deref())
            .and_then(parse_duration)
            .unwrap_or(DEFAULT_MAX_WAIT);
        let context_below =
            own.and_then(|r| r.context_below).or(self.context_below).filter(|b| b.is_finite() && *b > 0.0);
        RetryPolicy { max_wait, context_below }
    }
}

/// `"90s"`, `"30m"`, `"2h"`, `"1d"`, or several of them run together (`"1h30m"`, `"2d3h"`): groups
/// of a whole number and one unit, no space inside.
pub fn parse_duration(s: &str) -> Option<std::time::Duration> {
    let mut rest = s.trim();
    if rest.is_empty() {
        return None;
    }
    let mut total = 0u64;
    while !rest.is_empty() {
        let digits = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
        let n: u64 = rest.get(..digits)?.parse().ok()?;
        let unit = rest.get(digits..)?.chars().next()?;
        let secs = match unit {
            's' => 1,
            'm' => 60,
            'h' => 3600,
            'd' => 86400,
            _ => return None,
        };
        total = total.checked_add(n.checked_mul(secs)?)?;
        rest = rest.get(digits + unit.len_utf8()..)?;
    }
    Some(std::time::Duration::from_secs(total))
}

pub(super) fn check_retry(r: &MuRetry, warn: &mut Vec<String>) {
    if let Some(n) = r.max_attempts.filter(|n| !(1..=20).contains(n)) {
        warn.push(format!("config.toml: [retry] max_attempts {n} is not between 1 and 20; using the default"));
    }
    let rules = std::iter::once(("[retry]".to_string(), r.max_wait.as_deref(), r.context_below)).chain(
        r.providers.iter().map(|(p, v)| (format!("[retry.providers.{p}]"), v.max_wait.as_deref(), v.context_below)),
    );
    for (name, wait, below) in rules {
        if let Some(w) = wait.filter(|w| parse_duration(w).is_none()) {
            warn.push(format!(
                "config.toml: {name} max_wait {w:?} is not a duration such as 90s, 30m or 1h30m; using the default"
            ));
        }
        if let Some(b) = below.filter(|b| !(b.is_finite() && *b > 0.0 && *b <= 100.0)) {
            warn.push(format!("config.toml: {name} context_below {b} is not a percentage above 0 and up to 100"));
        }
    }
}
