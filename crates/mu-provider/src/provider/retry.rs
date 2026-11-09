use super::types::ProviderError;
use crate::auth::chatgpt;
use mu_base::events::wait_text;
use std::time::{Duration, SystemTime};

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

/// The wait the omp gateway appends to an error's text as `retry-after-ms=N` when it relays a
/// provider error (an in-stream error event has no headers to carry it).
pub fn hint(msg: &str) -> Option<Duration> {
    const KEY: &str = "retry-after-ms=";
    let mut from = 0;
    while let Some(i) = msg[from..].find(KEY) {
        let start = from + i;
        from = start + KEY.len();
        if start > 0 && !msg[..start].ends_with(char::is_whitespace) {
            continue;
        }
        let num: String = msg[from..].chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
        if let Ok(ms) = num.parse::<f64>()
            && ms.is_finite()
            && ms >= 0.0
        {
            return Some(Duration::from_secs_f64(ms / 1000.0));
        }
    }
    None
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
    let tokens = req.system.len() as u64 / 4
        + mu_base::tool::tool_tokens(req.tools)
        + req.messages.iter().map(|m| m.estimate_tokens(req.model)).sum::<u64>();
    tokens as f64 * 100.0 / window as f64
}

const LOGIN_HINT: &str = "\nIf you haven't logged in yet, run /login (or `omp login`) first.";

/// What a ChatGPT plan error code tells the user, as `(retryable, text)`; `None` for codes the plan
/// does not use. `param` is the field the API refused (`error.param`).
pub(crate) fn chatgpt_text(code: &str, msg: &str, param: &str) -> Option<(bool, String)> {
    let usage = chatgpt::USAGE_URL;
    Some(match code {
        "subscription_sharing_user_not_eligible" => {
            (false, format!("this ChatGPT account is not eligible to use its plan here: {msg}"))
        }
        "subscription_sharing_usage_limit_exceeded" => {
            (false, format!("your ChatGPT plan usage limit is reached; open Manage usage at {usage}"))
        }
        "subscription_sharing_workplace_individual_usage_limit_exceeded" => (
            false,
            "your individual ChatGPT usage limit in this workspace is reached; ask the workspace admin to raise it"
                .into(),
        ),
        "subscription_sharing_usage_unavailable" | "subscription_sharing_user_unavailable" => {
            (true, format!("ChatGPT plan usage is temporarily unavailable: {msg}"))
        }
        "subscription_sharing_unsupported_capability" => {
            let what = if param.is_empty() { "this request" } else { param };
            (false, format!("the ChatGPT plan does not support {what}: {msg}"))
        }
        "subscription_sharing_route_not_supported" => {
            (false, format!("the ChatGPT plan does not accept this request: {msg}"))
        }
        "subscription_sharing_invalid_user"
        | "chatpass_v2_scope_not_authorized"
        | "chatpass_v2_invalid_authorization_context" => {
            (false, "the ChatGPT sign-in is not accepted for this request; run /login to sign in again".into())
        }
        _ => return None,
    })
}

/// The error for a non-2xx answer over the ChatGPT plan. The plan's `error.code` (or a bare `detail`)
/// decides the class, else the status. The HTTP status, body and request id stay in the text.
pub fn chatgpt_error(
    status: u16,
    retry_after: Option<Duration>,
    body: &str,
    request_id: Option<&str>,
) -> ProviderError {
    let v: serde_json::Value = serde_json::from_str(body).unwrap_or(serde_json::Value::Null);
    let code = v["error"]["code"].as_str().unwrap_or("");
    let msg = v["error"]["message"].as_str().or(v["detail"].as_str()).unwrap_or("");
    let param = v["error"]["param"].as_str().unwrap_or("");
    let id = request_id.map(|id| format!("\nrequest id: {id}")).unwrap_or_default();
    let shown: String = body.chars().take(2000).collect();
    let named = if code.is_empty() { String::new() } else { format!(" ({code})") };
    let detail = format!("\nHTTP {status}{named}: {shown}{id}");
    match chatgpt_text(code, msg, param) {
        Some((true, t)) => ProviderError::Retryable { msg: t + &detail, retry_after },
        Some((false, t)) => ProviderError::Fatal(t + &detail),
        None => match status {
            503 => {
                ProviderError::Retryable { msg: format!("ChatGPT plan routing is unavailable{detail}"), retry_after }
            }
            401 => ProviderError::Fatal(format!(
                "the ChatGPT sign-in was not accepted; run /login to sign in again{detail}"
            )),
            403 => ProviderError::Fatal(format!("ChatGPT plan usage is not allowed here (policy or region){detail}")),
            _ => append(classify_http(status, retry_after, body, Some(chatgpt::LABEL)), &id),
        },
    }
}

/// `tail` after an error's text.
pub(crate) fn append(e: ProviderError, tail: &str) -> ProviderError {
    match e {
        ProviderError::Retryable { msg, retry_after } => ProviderError::Retryable { msg: msg + tail, retry_after },
        ProviderError::Overflow(m) => ProviderError::Overflow(m + tail),
        ProviderError::Fatal(m) => ProviderError::Fatal(m + tail),
        ProviderError::Aborted => ProviderError::Aborted,
    }
}

/// `retry-after-ms` (else `retry-after`, in seconds or as an HTTP date) and `x-should-retry`.
pub fn headers(h: &reqwest::header::HeaderMap) -> (Option<Duration>, Option<bool>) {
    let get = |k: &str| h.get(k).and_then(|v| v.to_str().ok()).map(str::trim);
    let after = get("retry-after-ms")
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_millis)
        .or_else(|| get("retry-after").and_then(retry_after_value));
    let should = match get("x-should-retry") {
        Some("true") => Some(true),
        Some("false") => Some(false),
        _ => None,
    };
    (after, should)
}

/// A `Retry-After` value: delay seconds, or an HTTP date measured from now (already past: no wait).
fn retry_after_value(v: &str) -> Option<Duration> {
    if let Ok(secs) = v.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    let at = httpdate::parse_http_date(v).ok()?;
    Some(at.duration_since(SystemTime::now()).unwrap_or(Duration::ZERO))
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
        Some(label) if matches!(status, 401 | 403) && label == chatgpt::LABEL => {
            msg.push_str("\nThe ChatGPT sign-in was not accepted; run /login to sign in again");
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
        assert_eq!(hint(r#"429 {"type":"error"} retry-after-ms=301000"#), Some(Duration::from_secs(301)));
        assert_eq!(hint("retry-after-ms=1500.5 x"), Some(Duration::from_micros(1_500_500)));
        assert_eq!(hint("x-retry-after-ms=5"), None);
        assert_eq!(hint("HTTP 429: slow down"), None);
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
    fn chatgpt_codes_map_to_the_documented_classes() {
        let usage = chatgpt_error(
            429,
            None,
            r#"{"error":{"code":"subscription_sharing_usage_limit_exceeded","message":"limit"}}"#,
            Some("req_1"),
        );
        let s = usage.to_string();
        assert!(matches!(usage, ProviderError::Fatal(_)), "{s}");
        assert!(s.contains("https://chatgpt.com/settings/usage") && s.contains("req_1"), "{s}");
        let busy = chatgpt_error(503, None, r#"{"error":{"code":"subscription_sharing_usage_unavailable"}}"#, None);
        assert!(matches!(busy, ProviderError::Retryable { .. }));
        let routing = chatgpt_error(503, None, r#"{"detail":"Direct routing unavailable"}"#, None);
        assert!(matches!(routing, ProviderError::Retryable { .. }));
        let cap = chatgpt_error(
            400,
            None,
            r#"{"error":{"code":"subscription_sharing_unsupported_capability","param":"tools[0].type"}}"#,
            None,
        )
        .to_string();
        assert!(cap.contains("tools[0].type"), "{cap}");
        assert!(matches!(chatgpt_error(403, None, r#"{"detail":"region"}"#, None), ProviderError::Fatal(_)));
        let e = chatgpt_error(401, None, r#"{"detail":"no"}"#, None).to_string();
        assert!(e.contains("/login") && !e.contains("API key"), "{e}");
        assert!(matches!(chatgpt_error(400, None, r#"{"error":{"message":"bad"}}"#, None), ProviderError::Fatal(_)));
        let v = classify_http(401, None, "x", Some(chatgpt::LABEL)).to_string();
        assert!(v.contains("sign-in was not accepted") && !v.contains("omp login"), "{v}");
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
    fn a_date_retry_after_is_the_wait_from_now() {
        let at = SystemTime::now() + Duration::from_secs(30);
        let future = httpdate::fmt_http_date(at);
        let (d, _) = headers(&map(&[("retry-after", future.as_str())]));
        let d = d.expect("a date parses");
        assert!((25..=31).contains(&d.as_secs()), "{d:?}");
        let past = httpdate::fmt_http_date(SystemTime::now() - Duration::from_secs(60));
        assert_eq!(headers(&map(&[("retry-after", past.as_str())])).0, Some(Duration::ZERO));
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
