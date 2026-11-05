use super::types::ProviderError;
use mu_base::events::wait_text;
use std::time::Duration;

pub const MAX_ATTEMPTS: u32 = 5;

const OVERFLOW: [&str; 6] = [
    "prompt is too long",
    "context_length_exceeded",
    "maximum context length",
    "exceeds the context window",
    "input is too long",
    "too many tokens",
];

/// Delay before attempt `attempt + 1`: `retry-after` when given (already within the user's limit,
/// see `too_long`), else 2^attempt s capped at 60 s, with ±20% jitter on the computed backoff.
pub fn backoff(attempt: u32, retry_after: Option<Duration>) -> Duration {
    match retry_after {
        Some(d) => d,
        None => jitter(Duration::from_secs(1u64 << attempt.min(6)).min(Duration::from_secs(60)), unit_random()),
    }
}

/// `d` scaled by a factor in 0.8..=1.2; `r` is in 0.0..1.0.
pub fn jitter(d: Duration, r: f64) -> Duration {
    d.mul_f64(0.8 + 0.4 * r.clamp(0.0, 1.0))
}

fn unit_random() -> f64 {
    let b = uuid::Uuid::new_v4().as_u128() as u64;
    (b >> 11) as f64 / (1u64 << 53) as f64
}

/// `Retry-After` beyond `limit`: not worth waiting for. Returns the fatal error to show.
pub fn too_long(retry_after: Option<Duration>, limit: Duration) -> Option<ProviderError> {
    let d = retry_after.filter(|d| *d > limit)?;
    Some(ProviderError::Fatal(format!(
        "rate limited; retry after {} (waits up to {} are retried; see [retry] in config.toml)",
        wait_text(d),
        wait_text(limit)
    )))
}

/// Percent of the model's context window a request is estimated to fill.
pub fn context_fill(req: &super::types::Request<'_>) -> f64 {
    let window = u64::from(req.model.context_window);
    if window == 0 {
        return 100.0;
    }
    let tokens = req.system.len() as u64 / 4 + req.messages.iter().map(|m| m.estimate_tokens()).sum::<u64>();
    tokens as f64 * 100.0 / window as f64
}

const LOGIN_HINT: &str = "\nIf you haven't logged in yet, run /login (or `omp login`) first.";

/// `retry-after-ms` (else `retry-after` in seconds) and `x-should-retry`.
pub fn headers(h: &reqwest::header::HeaderMap) -> (Option<Duration>, Option<bool>) {
    let get = |k: &str| h.get(k).and_then(|v| v.to_str().ok()).map(str::trim);
    let after = get("retry-after-ms")
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_millis)
        .or_else(|| get("retry-after").and_then(|v| v.parse::<u64>().ok()).map(Duration::from_secs));
    let should = match get("x-should-retry") {
        Some("true") => Some(true),
        Some("false") => Some(false),
        _ => None,
    };
    (after, should)
}

/// A provider that says not to retry is believed. Only native routes honour it: the gateway's
/// behaviour stays as it was.
pub fn honour_should_retry(e: ProviderError, should_retry: Option<bool>) -> ProviderError {
    match (e, should_retry) {
        (ProviderError::Retryable { msg, .. }, Some(false)) => ProviderError::Fatal(msg),
        (e, _) => e,
    }
}

/// The error for a non-2xx response. `native` is the provider's label when the request went to its
/// own API (see `classify_http`).
pub fn http_error(
    status: u16,
    retry_after: Option<Duration>,
    should_retry: Option<bool>,
    body: &str,
    native: Option<&str>,
) -> ProviderError {
    let e = classify_http(status, retry_after, body, native);
    if native.is_some() { honour_should_retry(e, should_retry) } else { e }
}

/// `native` is the provider's label when the request went to its own API: a rejected key then
/// says so, instead of pointing at omp's login.
pub fn classify_http(status: u16, retry_after: Option<Duration>, body: &str, native: Option<&str>) -> ProviderError {
    let mut msg = format!("HTTP {status}: {}", body.chars().take(2000).collect::<String>());
    match native {
        None => {
            if matches!(status, 401 | 403) || (status == 404 && body.to_lowercase().contains("unknown model")) {
                msg.push_str(LOGIN_HINT);
            }
        }
        Some(label) if matches!(status, 401 | 403) => {
            msg.push_str(&format!("\n{label} rejected the API key; run /login to replace it"));
        }
        Some(_) => {}
    }
    match status {
        408 | 409 | 429 | 500 | 502 | 503 | 504 | 529 => ProviderError::Retryable { msg, retry_after },
        400 | 413 => {
            let lower = body.to_lowercase();
            if OVERFLOW.iter().any(|p| lower.contains(p)) {
                ProviderError::Overflow(msg)
            } else {
                ProviderError::Fatal(msg)
            }
        }
        _ => ProviderError::Fatal(msg),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_schedule() {
        for (a, base) in [(1, 2.0), (2, 4.0), (3, 8.0), (4, 16.0), (9, 60.0)] {
            let d = backoff(a, None).as_secs_f64();
            assert!(d >= base * 0.8 - 1e-9 && d <= base * 1.2 + 1e-9, "{a}: {d}");
        }
        assert_eq!(jitter(Duration::from_secs(10), 0.0), Duration::from_secs(8));
        assert_eq!(jitter(Duration::from_secs(10), 1.0), Duration::from_secs(12));
        assert_eq!(backoff(1, Some(Duration::from_secs(7))).as_secs(), 7);
        let limit = Duration::from_secs(120);
        let e = too_long(Some(Duration::from_secs(900)), limit).unwrap().to_string();
        assert!(e.contains("retry after 15m") && e.contains("up to 2m"), "{e}");
        assert!(too_long(Some(limit), limit).is_none());
        assert!(too_long(Some(Duration::from_secs(7200)), Duration::MAX).is_none());
    }

    fn gw(status: u16, body: &str) -> ProviderError {
        classify_http(status, None, body, None)
    }

    #[test]
    fn classification() {
        assert!(matches!(gw(529, "x"), ProviderError::Retryable { .. }));
        assert!(matches!(gw(400, "Prompt is too long: 300000 tokens"), ProviderError::Overflow(_)));
        assert!(matches!(gw(400, "bad field"), ProviderError::Fatal(_)));
        assert!(matches!(gw(401, "no"), ProviderError::Fatal(_)));
        let e = gw(404, r#"{"message":"Unknown model: x/y"}"#).to_string();
        assert!(e.contains("/login"), "{e}");
        assert!(!gw(400, "bad field").to_string().contains("/login"));
    }

    #[test]
    fn the_login_hint_depends_on_the_route() {
        let e = classify_http(401, None, "invalid x-api-key", Some("Anthropic")).to_string();
        assert!(e.contains("Anthropic rejected the API key; run /login to replace it"), "{e}");
        assert!(!e.contains("omp login"), "{e}");
        let e = classify_http(403, None, "", Some("Baseten")).to_string();
        assert!(e.contains("Baseten rejected the API key"), "{e}");
        assert!(gw(401, "no").to_string().contains("omp login"));
        // a native 404 is a missing model, not a login problem
        assert!(
            !classify_http(404, None, r#"{"message":"Unknown model"}"#, Some("OpenAI")).to_string().contains("/login")
        );
    }

    fn map(pairs: &[(&str, &str)]) -> reqwest::header::HeaderMap {
        let mut m = reqwest::header::HeaderMap::new();
        for (k, v) in pairs {
            m.insert(reqwest::header::HeaderName::from_bytes(k.as_bytes()).unwrap(), v.parse().unwrap());
        }
        m
    }

    #[test]
    fn retry_headers() {
        let (d, s) = headers(&map(&[("retry-after", "7")]));
        assert_eq!((d, s), (Some(Duration::from_secs(7)), None));
        // retry-after-ms is more precise and wins
        let (d, _) = headers(&map(&[("retry-after", "7"), ("retry-after-ms", "1500")]));
        assert_eq!(d, Some(Duration::from_millis(1500)));
        let (d, s) = headers(&map(&[("x-should-retry", "false")]));
        assert_eq!((d, s), (None, Some(false)));
        assert_eq!(headers(&map(&[("x-should-retry", "true")])).1, Some(true));
        assert_eq!(headers(&map(&[("retry-after", "soon")])).0, None);
    }

    #[test]
    fn x_should_retry_false_stops_retries() {
        let e = classify_http(529, None, "busy", Some("Anthropic"));
        assert!(matches!(e, ProviderError::Retryable { .. }));
        assert!(matches!(honour_should_retry(e, Some(false)), ProviderError::Fatal(_)));
        let e = classify_http(529, None, "busy", Some("Anthropic"));
        assert!(matches!(honour_should_retry(e, Some(true)), ProviderError::Retryable { .. }));
        assert!(matches!(honour_should_retry(gw(400, "x"), Some(false)), ProviderError::Fatal(_)));
    }

    #[test]
    fn only_the_native_route_honours_x_should_retry() {
        let gateway = http_error(529, None, Some(false), "busy", None);
        assert!(matches!(gateway, ProviderError::Retryable { .. }), "{gateway:?}");
        let native = http_error(529, None, Some(false), "busy", Some("Anthropic"));
        assert!(matches!(native, ProviderError::Fatal(_)), "{native:?}");
        assert!(matches!(http_error(529, None, None, "busy", Some("Anthropic")), ProviderError::Retryable { .. }));
    }
}
