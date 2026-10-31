use super::ProviderError;
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

/// Longest `Retry-After` we wait out; anything above fails at once.
pub const MAX_RETRY_AFTER: Duration = Duration::from_secs(120);

/// Delay before attempt `attempt + 1`: `retry-after` when given (≤ 120 s), else 2^attempt s
/// capped at 60 s, with ±20% jitter on the computed backoff.
pub fn backoff(attempt: u32, retry_after: Option<Duration>) -> Duration {
    match retry_after {
        Some(d) => d.min(MAX_RETRY_AFTER),
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

/// `Retry-After` beyond the cap: not worth waiting for. Returns the fatal error to show.
pub fn too_long(retry_after: Option<Duration>) -> Option<ProviderError> {
    let d = retry_after.filter(|d| *d > MAX_RETRY_AFTER)?;
    Some(ProviderError::Fatal(format!("rate limited; retry after {} min", d.as_secs().div_ceil(60))))
}

const LOGIN_HINT: &str = "\nIf you haven't logged in yet, run /login (or `omp login`) first.";

pub fn classify_http(status: u16, retry_after: Option<Duration>, body: &str) -> ProviderError {
    let mut msg = format!("HTTP {status}: {}", body.chars().take(2000).collect::<String>());
    if matches!(status, 401 | 403) || (status == 404 && body.to_lowercase().contains("unknown model")) {
        msg.push_str(LOGIN_HINT);
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
        assert!(too_long(Some(Duration::from_secs(900))).is_some_and(|e| e.to_string().contains("15 min")));
        assert!(too_long(Some(Duration::from_secs(120))).is_none());
    }

    #[test]
    fn classification() {
        assert!(matches!(classify_http(529, None, "x"), ProviderError::Retryable { .. }));
        assert!(matches!(classify_http(400, None, "Prompt is too long: 300000 tokens"), ProviderError::Overflow(_)));
        assert!(matches!(classify_http(400, None, "bad field"), ProviderError::Fatal(_)));
        assert!(matches!(classify_http(401, None, "no"), ProviderError::Fatal(_)));
        let e = classify_http(404, None, r#"{"message":"Unknown model: x/y"}"#).to_string();
        assert!(e.contains("/login"), "{e}");
        assert!(!classify_http(400, None, "bad field").to_string().contains("/login"));
    }
}
