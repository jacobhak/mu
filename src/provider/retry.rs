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

/// Delay before attempt `attempt + 1`: `retry-after` when sane, else 2^attempt s, capped at 60 s.
pub fn backoff(attempt: u32, retry_after: Option<Duration>) -> Duration {
    match retry_after {
        Some(d) if d <= Duration::from_secs(300) => d,
        _ => Duration::from_secs(1u64 << attempt.min(6)).min(Duration::from_secs(60)),
    }
}

pub fn classify_http(status: u16, retry_after: Option<Duration>, body: &str) -> ProviderError {
    let msg = format!("HTTP {status}: {}", body.chars().take(2000).collect::<String>());
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
        let d = |a| backoff(a, None).as_secs();
        assert_eq!([d(1), d(2), d(3), d(4), d(9)], [2, 4, 8, 16, 60]);
        assert_eq!(backoff(1, Some(Duration::from_secs(7))).as_secs(), 7);
        assert_eq!(backoff(1, Some(Duration::from_secs(900))).as_secs(), 2);
    }

    #[test]
    fn classification() {
        assert!(matches!(classify_http(529, None, "x"), ProviderError::Retryable { .. }));
        assert!(matches!(classify_http(400, None, "Prompt is too long: 300000 tokens"), ProviderError::Overflow(_)));
        assert!(matches!(classify_http(400, None, "bad field"), ProviderError::Fatal(_)));
        assert!(matches!(classify_http(401, None, "no"), ProviderError::Fatal(_)));
    }
}
