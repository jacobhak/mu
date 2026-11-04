//! Subscription usage windows (`5h`, `7d`) from the gateway's `GET /v1/usage`.
//!
//! Only an already-running gateway is queried: the lookup reads `state.json` and checks its pid, so
//! refreshing never starts one. State lives in `$MU_HOME/gateway/` (`profiles/<name>/` under an omp profile).
//! The gateway caches each account's usage for 5 minutes and fetches it upstream on a miss, so a call can
//! take seconds; callers run it off the agent loop.
use mu_base::events::{ProviderUsage, QuotaWindow};
use serde_json::Value;
use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;
use std::time::Instant;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

const TIMEOUT: Duration = Duration::from_secs(15);

/// The windows reported for `provider`. Tiered (per-model) limits are skipped.
pub fn parse_usage(body: &str, provider: &str, now_ms: i64) -> Option<ProviderUsage> {
    let v: Value = serde_json::from_str(body).ok()?;
    let report = v["reports"].as_array()?.iter().find(|r| r["provider"].as_str() == Some(provider))?;
    let mut windows: Vec<QuotaWindow> = Vec::new();
    for limit in report["limits"].as_array()? {
        if !limit["scope"]["tier"].is_null() {
            continue;
        }
        // One malformed limit must not hide the others.
        let Some(id) = limit["window"]["id"].as_str() else { continue };
        let label = match id {
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

pub enum Fetch {
    Got(ProviderUsage),
    /// No gateway is running; checking again is a local file read.
    NoGateway,
    /// The request failed, timed out, or the report has no windows for the provider.
    Failed,
}

pub async fn fetch(provider: &str) -> Fetch {
    let Some((url, token)) = crate::gateway::running_endpoint() else { return Fetch::NoGateway };
    let body = async {
        crate::provider::install_crypto();
        let client = reqwest::Client::builder().timeout(TIMEOUT).build().ok()?;
        client
            .get(format!("{url}/v1/usage"))
            .bearer_auth(token)
            .send()
            .await
            .ok()?
            .error_for_status()
            .ok()?
            .text()
            .await
            .ok()
    };
    let body = body.await;
    let now_ms = jiff::Timestamp::now().as_millisecond();
    match body.and_then(|b| parse_usage(&b, provider, now_ms)) {
        Some(u) => Fetch::Got(u),
        None => Fetch::Failed,
    }
}

/// Quota refresh cadence: a minute after success; 5s, 10s, 20s, 40s, then a minute after failures.
fn interval(failures: u32) -> Duration {
    match failures {
        0 => Duration::from_secs(60),
        n => Duration::from_secs(5u64 << (n - 1).min(4)).min(Duration::from_secs(60)),
    }
}

/// While no gateway runs, check again this often (a local file read), so quota shows soon after the
/// first request starts one.
const NO_GATEWAY_INTERVAL: Duration = Duration::from_secs(5);

type Slot = Rc<RefCell<Option<(String, Fetch)>>>;

/// Keeps the provider's usage windows fresh in the background and answers what the status line needs.
/// It never waits on the network: a fetch runs as a task, and its result is picked up by `tick`.
pub struct QuotaTracker {
    usage: Option<ProviderUsage>,
    /// When the last fetch finished; `None` means one is due now.
    at: Option<Instant>,
    failures: u32,
    /// The last fetch found no running gateway (cheap to re-check, so retried on a short fixed interval).
    no_gateway: bool,
    inflight: bool,
    /// Native models are metered by API key: nothing to fetch, and nothing to wake for.
    paused: bool,
    /// Quota is fetched only for the top-level interactive agent.
    pub enabled: bool,
    /// The background fetch writes its result here and pings `wake`.
    slot: Slot,
    wake: UnboundedSender<()>,
    wake_rx: Option<UnboundedReceiver<()>>,
}

impl Default for QuotaTracker {
    fn default() -> Self {
        let (wake, wake_rx) = unbounded_channel();
        Self {
            usage: None,
            at: None,
            failures: 0,
            no_gateway: false,
            inflight: false,
            paused: false,
            enabled: false,
            slot: Rc::new(RefCell::new(None)),
            wake,
            wake_rx: Some(wake_rx),
        }
    }
}

impl QuotaTracker {
    /// The receiver that is pinged when a fetch finishes; taken once by the run loop.
    pub fn take_wake(&mut self) -> Option<UnboundedReceiver<()>> {
        self.wake_rx.take()
    }

    /// The windows for `provider`, if the last fetch was for it.
    pub fn usage_for(&self, provider: &str) -> Option<ProviderUsage> {
        self.usage.clone().filter(|q| q.provider == provider)
    }

    /// Whether `provider` bills a subscription. Every request goes through the gateway, which reports
    /// usage windows only for subscription (OAuth) accounts; a provider that keeps answering without
    /// any is metered by API key. Assumed subscription until that is known.
    pub fn is_subscription(&self, provider: &str) -> bool {
        self.usage_for(provider).is_some() || self.failures < 3
    }

    /// The provider changed: what was learned about the old one no longer applies.
    pub fn provider_changed(&mut self) {
        self.at = None;
        self.failures = 0;
    }

    /// Pausing (a native model) stops fetching and makes `wait` long, so the run loop idles. Resuming
    /// fetches at once.
    pub fn set_paused(&mut self, paused: bool) {
        if self.paused == paused {
            return;
        }
        self.paused = paused;
        if !paused {
            self.at = None;
        }
    }

    /// Time until the next fetch is due. Long while one is in flight: its completion wakes the run loop.
    pub fn wait(&self) -> Duration {
        if !self.enabled || self.paused || self.inflight {
            return Duration::from_secs(3600);
        }
        let Some(at) = self.at else { return Duration::ZERO };
        let every = if self.no_gateway { NO_GATEWAY_INTERVAL } else { interval(self.failures) };
        every.saturating_sub(at.elapsed())
    }

    /// Applies a finished fetch, then starts the next one if due.
    pub fn tick(&mut self, provider: &str) {
        if !self.enabled || self.paused {
            return;
        }
        if let Some((fetched_for, got)) = self.slot.borrow_mut().take() {
            self.inflight = false;
            self.at = Some(Instant::now());
            match got {
                Fetch::Got(q) => {
                    self.failures = 0;
                    self.no_gateway = false;
                    self.usage = Some(q);
                }
                Fetch::NoGateway => self.no_gateway = true,
                Fetch::Failed => {
                    self.no_gateway = false;
                    self.failures = self.failures.saturating_add(1);
                }
            }
            // Fetched for a provider the session has since switched away from: fetch again now.
            if fetched_for != provider {
                self.at = None;
            }
        }
        if self.inflight || !self.wait().is_zero() {
            return;
        }
        self.inflight = true;
        let (provider, slot, wake) = (provider.to_string(), self.slot.clone(), self.wake.clone());
        tokio::task::spawn_local(async move {
            let got = fetch(&provider).await;
            *slot.borrow_mut() = Some((provider, got));
            let _ = wake.send(());
        });
    }

    #[cfg(test)]
    pub fn set_usage(&mut self, usage: ProviderUsage) {
        self.usage = Some(usage);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_limit_without_a_window_id_is_skipped() {
        let body = r#"{"reports":[{"provider":"anthropic","limits":[
            {"scope":{},"window":{},"amount":{"usedFraction":0.5}},
            {"scope":{},"window":{"id":"5h","resetsAt":0},"amount":{"usedFraction":0.5}}]}]}"#;
        assert_eq!(parse_usage(body, "anthropic", 0).unwrap().windows.len(), 1);
    }

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

    #[test]
    fn retries_fast_then_settles() {
        let s: Vec<u64> = (0..7).map(|n| interval(n).as_secs()).collect();
        assert_eq!(s, [60, 5, 10, 20, 40, 60, 60]);
        assert_eq!(interval(u32::MAX).as_secs(), 60);
    }

    #[test]
    fn subscription_until_the_provider_keeps_answering_without_windows() {
        let mut q = QuotaTracker::default();
        assert!(q.is_subscription("anthropic"));
        q.failures = 3;
        assert!(!q.is_subscription("anthropic"));
        q.usage = Some(ProviderUsage { provider: "anthropic".into(), windows: vec![] });
        assert!(q.is_subscription("anthropic"), "windows reported: a subscription");
        assert!(!q.is_subscription("openai"));
        q.provider_changed();
        assert!(q.is_subscription("openai"));
        assert_eq!(q.wait(), Duration::from_secs(3600), "disabled: nothing to wait for");
    }

    #[test]
    fn a_paused_tracker_sleeps_and_fetches_nothing() {
        let mut q = QuotaTracker { enabled: true, ..Default::default() };
        assert_eq!(q.wait(), Duration::ZERO, "unpaused and due: fetch now");
        q.set_paused(true);
        assert!(q.wait() >= Duration::from_secs(3600), "paused: the run loop must not spin");
        q.tick("anthropic");
        assert!(!q.inflight, "paused: no fetch is started, so no task is spawned");
        q.set_paused(false);
        assert_eq!(q.wait(), Duration::ZERO, "resuming fetches at once");
    }
}
