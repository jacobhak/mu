pub mod anthropic;
mod anthropic_native;
pub mod catalog;
pub mod discovery;
pub mod native;
pub mod openai_completions;
pub mod openai_responses;
pub mod retry;
pub mod sse;

use crate::message::Message;
use crate::models::{self, GatewayModel, ModelSpec, Thinking};
use serde_json::{Value, json};
use std::io::Write;
/// A model a native provider offers, for pickers.
#[derive(Debug, Clone, PartialEq)]
pub struct NativeModel {
    /// `provider/model`, as a selector.
    pub id: String,
    pub provider: String,
    pub name: String,
}

use std::time::Duration;

#[derive(Debug, Clone)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub schema: Value,
}

pub struct Request<'a> {
    pub model: &'a ModelSpec,
    /// Shared so retries and later requests of a turn pick up a level the user set mid-turn.
    pub thinking: Rc<Cell<Thinking>>,
    pub system: &'a str,
    pub tools: &'a [ToolDef],
    pub messages: &'a [Message],
    pub max_tokens: Option<u32>,
    /// Stable for the whole session; wires use it as a prompt-cache key.
    pub session_id: Option<&'a str>,
}

pub enum StreamEvent<'a> {
    Text(&'a str),
    Thinking(&'a str),
    ToolCallStart {
        id: &'a str,
        name: &'a str,
    },
    /// Raw fragment of the call's JSON arguments.
    ToolCallDelta {
        id: &'a str,
        json: &'a str,
    },
    Retry {
        attempt: u32,
        delay: Duration,
        reason: String,
    },
}

#[derive(Debug, Clone)]
pub enum ProviderError {
    Retryable { msg: String, retry_after: Option<Duration> },
    Overflow(String),
    Fatal(String),
    Aborted,
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Retryable { msg, .. } | Self::Overflow(msg) | Self::Fatal(msg) => f.write_str(msg),
            Self::Aborted => f.write_str("aborted"),
        }
    }
}

impl std::error::Error for ProviderError {}

/// The gateway surfaces thrown pre-flight errors (an unsupported effort, an unknown model, ...)
/// as in-stream `api_error` events. Those are permanent: retrying only burns the backoff budget.
fn permanent(kind: &str, msg: &str) -> bool {
    let m = msg.to_lowercase();
    kind == "api_error" && (m.contains("not supported") || m.contains("unsupported"))
}

/// Maps an in-stream error event to a retry class by its error type name.
pub fn stream_error(kind: &str, msg: &str) -> ProviderError {
    if permanent(kind, msg) {
        return ProviderError::Fatal(msg.into());
    }
    match kind {
        "overloaded_error" | "rate_limit_error" | "api_error" | "upstream_error" => {
            ProviderError::Retryable { msg: msg.into(), retry_after: None }
        }
        _ => ProviderError::Fatal(msg.into()),
    }
}

/// Replace characters outside `[A-Za-z0-9_-]` with `_`, then cap at 64 characters (the strictest
/// provider limit). A longer id keeps its first 47 characters plus a hash of the whole original, so
/// the same input always gives the same output and a tool call and its result still pair up.
pub fn normalize_id(id: &str) -> String {
    let clean: String =
        id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect();
    if clean.len() <= 64 {
        return clean;
    }
    format!("{}_{:016x}", &clean[..47], fnv1a(id.as_bytes()))
}

/// FNV-1a: unlike `DefaultHasher`, its output is fixed across Rust versions, so ids stay stable.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, b| (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3))
}

/// Thinking blocks are only replayable to the same api + provider that produced them.
pub fn same_origin(api: &str, provider: &str, model: &ModelSpec) -> bool {
    api == model.api.as_str() && provider == model.provider
}

/// How a request authenticates.
pub enum Auth {
    Bearer(String),
    XApiKey(String),
}

impl Auth {
    pub fn for_def(def: &native::ProviderDef, key: &str) -> Self {
        match def.auth {
            native::AuthStyle::XApiKey => Self::XApiKey(key.into()),
            native::AuthStyle::Bearer => Self::Bearer(key.into()),
        }
    }

    /// The key goes in a header marked sensitive, so it never shows in debug output.
    pub fn apply(&self, rb: reqwest::RequestBuilder) -> Result<reqwest::RequestBuilder, ProviderError> {
        Ok(match self {
            Self::Bearer(t) => rb.bearer_auth(t),
            Self::XApiKey(k) => {
                let mut v = reqwest::header::HeaderValue::from_str(k).map_err(|_| {
                    ProviderError::Fatal("the API key contains characters a header cannot carry".into())
                })?;
                v.set_sensitive(true);
                rb.header("x-api-key", v)
            }
        })
    }
}

/// Where one request goes: the gateway on loopback, or a provider's own API.
pub struct Route {
    pub base: String,
    pub auth: Auth,
    /// The provider whose own API this is; `None` for the gateway.
    pub def: Option<&'static native::ProviderDef>,
}

impl Route {
    pub fn is_native(&self) -> bool {
        self.def.is_some()
    }

    /// Names the peer in connect errors.
    fn peer(&self) -> String {
        if self.is_native() { host_of(&self.base) } else { format!("gateway at {}", self.base) }
    }

    fn idle_peer(&self) -> String {
        if self.is_native() { host_of(&self.base) } else { "the gateway".into() }
    }
}

fn host_of(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    rest.split(['/', '?', '#']).next().unwrap_or(rest).to_string()
}

/// Turns the bytes of one response stream into events and, at the end, the assistant message.
pub trait Decoder {
    fn feed(&mut self, chunk: &[u8], ev: &mut dyn FnMut(StreamEvent)) -> Result<Option<Message>, ProviderError>;
    /// The message when the stream ended without its own end marker, if the content is complete enough.
    fn finish(&mut self) -> Option<Message> {
        None
    }
}

/// One request as a wire module builds it. `url` is complete; auth and `content-type` are added
/// by the caller.
pub struct Wire {
    pub url: String,
    pub headers: Vec<(&'static str, String)>,
    pub body: Value,
    pub decoder: Box<dyn Decoder>,
}

/// Dispatches on the model's wire protocol.
fn wire(req: &Request, route: &Route) -> Result<Wire, ProviderError> {
    match req.model.api {
        Api::AnthropicMessages => Ok(anthropic::wire(req, route)),
        Api::OpenAiResponses => openai_responses::wire(req, route),
        Api::OpenAiCompletions => openai_completions::wire(req, route),
    }
}

/// Appends one JSON line per model request to `$MU_DUMP_REQUESTS`, for debugging. Only the shape of
/// the request is written: the system prompt, messages and tools are counted, not logged.
fn dump_request(req: &Request, body: &Value) {
    let Some(path) = std::env::var_os("MU_DUMP_REQUESTS").filter(|p| !p.is_empty()) else { return };
    let mut line = json!({
        "ts": crate::message::now_ms(),
        "model": req.model.id,
        "max_tokens": body["max_tokens"],
        "thinking": body["thinking"],
        "output_config": body.get("output_config").cloned().unwrap_or(Value::Null),
        "mu_level": req.thinking.get().as_str(),
        "system_chars": req.system.len(),
        "messages": req.messages.len(),
        "tools": req.tools.len(),
    });
    if req.model.native {
        line["api"] = req.model.api.as_str().into();
        line["reasoning"] = body.get("reasoning").or(body.get("reasoning_effort")).cloned().unwrap_or(Value::Null);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{line}");
    }
}

use crate::config::Configs;
use crate::message::Block;
use crate::models::Api;
use anyhow::{Result, anyhow};
use futures_util::StreamExt;
use std::cell::{Cell, OnceCell, RefCell};
use std::rc::Rc;
use tokio_util::sync::CancellationToken;

struct Endpoint {
    base: String,
    token: String,
}

/// Installs rustls' process-default crypto provider (ring). reqwest is built with
/// `rustls-no-provider`, so a client built before this call panics. Idempotent.
pub fn install_crypto() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// No bytes for this long on a response stream: treat the connection as dead and retry.
const STREAM_IDLE: Duration = Duration::from_secs(90);
/// Ceiling for one-shot completions (titles, summaries use their own cancel token as well).
const COMPLETE_TIMEOUT: Duration = Duration::from_secs(60);

/// The active profile's key store; tests get one inside their private `MU_HOME`.
#[cfg(not(test))]
fn default_store() -> crate::auth_store::Store {
    crate::auth_store::Store::new()
}

#[cfg(test)]
fn default_store() -> crate::auth_store::Store {
    crate::auth_store::Store::at(crate::testutil::home().join("auth.json"))
}

pub struct Providers {
    pub cfg: Rc<Configs>,
    client: OnceCell<reqwest::Client>,
    gw_models: RefCell<Option<Vec<GatewayModel>>>,
    /// `gw_models` came from the running gateway, not the on-disk cache.
    gw_fresh: std::cell::Cell<bool>,
    gateway: crate::gateway::GatewaySource,
    store: crate::auth_store::Store,
    /// Test seam: scripted replies, popped in order by `stream`.
    #[cfg(test)]
    pub scripted: RefCell<std::collections::VecDeque<Result<Message, ProviderError>>>,
}

impl Providers {
    pub fn new(cfg: Rc<Configs>, gateway: crate::gateway::GatewaySource) -> Self {
        Self {
            cfg,
            client: OnceCell::new(),
            gw_models: RefCell::new(load_cache()),
            gw_fresh: std::cell::Cell::new(false),
            gateway,
            store: default_store(),
            #[cfg(test)]
            scripted: Default::default(),
        }
    }

    /// One shared client: a 10 s connect timeout, no redirects, no overall timeout (streams are
    /// long-lived; `STREAM_IDLE` guards them instead).
    fn client(&self) -> &reqwest::Client {
        self.client.get_or_init(|| {
            install_crypto();
            // Redirects off: a native request carries its key, which reqwest would send to another host.
            reqwest::Client::builder()
                .connect_timeout(CONNECT_TIMEOUT)
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap_or_else(|_| reqwest::Client::new())
        })
    }

    /// Whether `provider`'s models go to its own API with the user's key: the provider is one
    /// mu speaks natively and the key store has an entry for it.
    pub fn is_native(&self, provider: &str) -> bool {
        native::def(provider).is_some() && self.store.has(provider)
    }

    pub fn store(&self) -> &crate::auth_store::Store {
        &self.store
    }

    /// Models of the native providers that have a key: the snapshot's rows (and, for Baseten,
    /// the last discovered ones). Never touches the network. Sorted by provider, then id.
    pub fn native_models(&self) -> Vec<NativeModel> {
        let mut out = Vec::new();
        for def in native::PROVIDERS.iter().filter(|d| self.is_native(d.id)) {
            for r in catalog::rows(def.id).iter().filter(|r| models::Api::parse(&r.api).is_some()) {
                out.push(NativeModel {
                    id: format!("{}/{}", def.id, r.id),
                    provider: def.id.to_string(),
                    name: r.name.clone().unwrap_or_else(|| r.id.clone()),
                });
            }
            if discovery::discovers(def) {
                for d in
                    discovery::cached(def.id).into_iter().filter(|d| d.tools && catalog::row(def.id, &d.id).is_none())
                {
                    out.push(NativeModel {
                        id: format!("{}/{}", def.id, d.id),
                        provider: def.id.to_string(),
                        name: d.name.clone().unwrap_or_else(|| d.id.clone()),
                    });
                }
            }
        }
        out
    }

    /// Refreshes the discovered model lists of the providers that have a key and list their
    /// models, at most daily unless `force`. Failures keep the old cache. Returns how many
    /// models were fetched.
    pub async fn refresh_native_models(&self, force: bool) -> usize {
        let mut n = 0;
        for def in native::PROVIDERS.iter().filter(|d| discovery::discovers(d) && self.is_native(d.id)) {
            if !force && !discovery::stale(def.id) {
                continue;
            }
            let Ok(r) = self.store.resolve(def.id).await else { continue };
            if let Ok(c) = discovery::refresh(def, &r.key, r.base_url.as_deref()).await {
                n += c;
            }
        }
        n
    }

    pub fn resolve(&self, sel: &str) -> Result<(ModelSpec, Option<Thinking>)> {
        models::resolve(sel, &self.cfg, self.gw_models.borrow().as_deref(), &|p| self.is_native(p))
    }

    async fn endpoint(&self) -> Result<Endpoint, ProviderError> {
        let (base, token) = self.gateway.endpoint().await.map_err(|e| ProviderError::Fatal(e.to_string()))?;
        Ok(Endpoint { base, token })
    }

    /// Where a request for `m` goes.
    async fn route(&self, m: &ModelSpec) -> Result<Route, ProviderError> {
        if m.native {
            let def = native::def(&m.provider)
                .ok_or_else(|| ProviderError::Fatal(format!("{} is not a native provider", m.provider)))?;
            let r = self.store.resolve(&m.provider).await.map_err(|e| ProviderError::Fatal(e.to_string()))?;
            let base = r.base_url.unwrap_or_else(|| def.base_url.to_string());
            return Ok(Route { base, auth: Auth::for_def(def, &r.key), def: Some(def) });
        }
        let ep = self.endpoint().await?;
        Ok(Route { base: ep.base, auth: Auth::Bearer(ep.token), def: None })
    }

    /// The last known model list (from the on-disk cache or the latest fetch); never touches the network.
    pub fn cached_models(&self) -> Vec<GatewayModel> {
        self.gw_models.borrow().clone().unwrap_or_default()
    }

    /// Gateway `/v1/models` rows (context windows). `None` when the gateway is not running or unreachable.
    pub async fn gateway_models(&self) -> Result<Vec<GatewayModel>> {
        if self.gw_fresh.get()
            && let Some(m) = self.gw_models.borrow().as_ref()
        {
            return Ok(m.clone());
        }
        let ep = self.endpoint().await.map_err(|e| anyhow!("{e}"))?;
        let text = self
            .client()
            .get(format!("{}/v1/models", ep.base))
            .bearer_auth(&ep.token)
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        let v: Value = serde_json::from_str(&text)?;
        let rows: Vec<GatewayModel> = v["data"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|m| {
                        Some(GatewayModel {
                            id: m["id"].as_str()?.into(),
                            context_length: m["context_length"].as_u64().map(|n| n as u32),
                            max_output_tokens: m["max_output_tokens"].as_u64().map(|n| n as u32),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        if rows.is_empty() {
            return Err(anyhow!("the gateway returned no models"));
        }
        save_cache(&rows);
        *self.gw_models.borrow_mut() = Some(rows.clone());
        self.gw_fresh.set(true);
        Ok(rows)
    }

    pub async fn stream(
        &self,
        req: &Request<'_>,
        cancel: &CancellationToken,
        ev: &mut dyn FnMut(StreamEvent),
    ) -> Result<Message, ProviderError> {
        #[cfg(test)]
        if let Some(r) = self.scripted.borrow_mut().pop_front() {
            // Streams text blocks as the real provider does, so rendering can be tested.
            if let Ok(Message::Assistant { content, .. }) = &r {
                for b in content {
                    if let Block::Text { text } = b {
                        ev(StreamEvent::Text(text));
                    }
                }
            }
            return r;
        }
        if !req.model.native && !self.gw_fresh.get() {
            tokio::select! {
                _ = self.gateway_models() => {}
                _ = cancel.cancelled() => return Err(ProviderError::Aborted),
            }
        }
        let mut attempt = 1;
        let mut rekeyed = false;
        loop {
            match self.attempt(req, cancel, ev, &mut rekeyed).await {
                Err(ProviderError::Retryable { msg, retry_after }) if attempt < retry::MAX_ATTEMPTS => {
                    let delay = retry::backoff(attempt, retry_after);
                    ev(StreamEvent::Retry { attempt, delay, reason: msg });
                    tokio::select! {
                        _ = tokio::time::sleep(delay) => {}
                        _ = cancel.cancelled() => return Err(ProviderError::Aborted),
                    }
                    attempt += 1;
                }
                r => return r,
            }
        }
    }

    async fn attempt(
        &self,
        req: &Request<'_>,
        cancel: &CancellationToken,
        ev: &mut dyn FnMut(StreamEvent),
        rekeyed: &mut bool,
    ) -> Result<Message, ProviderError> {
        let route = self.route(req.model).await?;
        let Wire { url, headers, body, decoder: mut dec } = wire(req, &route)?;
        dump_request(req, &body);
        let mut rb = self.client().post(url).header("content-type", "application/json");
        for (k, v) in headers {
            rb = rb.header(k, v);
        }
        rb = route.auth.apply(rb)?;
        let resp = tokio::select! {
            r = rb.body(body.to_string()).send() => r.map_err(|e| ProviderError::Retryable {
                msg: format!("cannot reach {}: {e}", route.peer()),
                retry_after: None,
            })?,
            _ = cancel.cancelled() => return Err(ProviderError::Aborted),
        };
        let status = resp.status();
        if !status.is_success() {
            let (retry_after, should_retry) = retry::headers(resp.headers());
            if let Some(e) = retry::too_long(retry_after) {
                return Err(e);
            }
            let text = crate::secrets::redact_keys(&resp.text().await.unwrap_or_default());
            if let Some(def) = route.def
                && status.as_u16() == 401
                && !*rekeyed
                && crate::auth_store::forget_cached(def.id)
            {
                // A `!command` key may have rotated: run it once more before giving up.
                *rekeyed = true;
                return Err(ProviderError::Retryable {
                    msg: format!("{} rejected the API key; running its command again", def.label),
                    retry_after: Some(Duration::ZERO),
                });
            }
            return Err(retry::http_error(
                status.as_u16(),
                retry_after,
                should_retry,
                &text,
                route.def.map(|d| d.label),
            ));
        }
        let mut bytes = resp.bytes_stream();
        loop {
            let chunk = tokio::select! {
                c = tokio::time::timeout(STREAM_IDLE, bytes.next()) => match c {
                    Ok(c) => c,
                    Err(_) => {
                        return Err(ProviderError::Retryable {
                            msg: format!("no data from {} for {}s", route.idle_peer(), STREAM_IDLE.as_secs()),
                            retry_after: None,
                        });
                    }
                },
                _ = cancel.cancelled() => return Err(ProviderError::Aborted),
            };
            match chunk {
                Some(Ok(c)) => {
                    if let Some(msg) = dec.feed(&c, ev)? {
                        return Ok(msg);
                    }
                }
                Some(Err(e)) => {
                    return Err(ProviderError::Retryable { msg: format!("stream error: {e}"), retry_after: None });
                }
                None => {
                    if let Some(msg) = dec.finish() {
                        return Ok(msg);
                    }
                    return Err(ProviderError::Retryable { msg: "stream ended early".into(), retry_after: None });
                }
            }
        }
    }

    /// One-shot completion without tools or thinking. `role_or_model` is a model id, or a role
    /// (optionally `@`-prefixed) whose fallback chain is tried until one model answers.
    pub async fn complete(
        &self,
        role_or_model: &str,
        system: &str,
        prompt: &str,
        max_tokens: u32,
        cancel: &CancellationToken,
    ) -> Result<String> {
        let sels: Vec<String> = if role_or_model.contains('/') {
            vec![role_or_model.to_string()]
        } else {
            models::role_chain(role_or_model.trim_start_matches('@'))
                .into_iter()
                .filter(|r| self.cfg.role(r).is_some())
                .map(|r| format!("@{r}"))
                .collect()
        };
        if sels.is_empty() {
            return Err(anyhow!("no model configured for role {role_or_model}"));
        }
        let mut last = anyhow!("no model");
        for sel in sels {
            let (spec, _) = match self.resolve(&sel) {
                Ok(r) => r,
                Err(e) => {
                    last = e;
                    continue;
                }
            };
            let msgs = [Message::user(prompt)];
            let req = Request {
                model: &spec,
                thinking: Rc::new(Cell::new(Thinking::Off)),
                system,
                tools: &[],
                messages: &msgs,
                max_tokens: Some(max_tokens.min(spec.max_output)),
                session_id: None,
            };
            let mut sink = |_: StreamEvent| {};
            let attempt = tokio::time::timeout(COMPLETE_TIMEOUT, self.stream(&req, cancel, &mut sink));
            let attempt = attempt.await.unwrap_or_else(|_| {
                Err(ProviderError::Retryable {
                    msg: format!("timed out after {}s", COMPLETE_TIMEOUT.as_secs()),
                    retry_after: None,
                })
            });
            match attempt {
                Ok(Message::Assistant { content, .. }) => {
                    return Ok(content
                        .iter()
                        .filter_map(|b| match b {
                            Block::Text { text, .. } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect());
                }
                Ok(_) => {}
                Err(ProviderError::Aborted) => return Err(anyhow!("aborted")),
                Err(e) => last = anyhow!("{sel}: {e}"),
            }
        }
        Err(last)
    }
}

/// Last gateway model metadata, so the first status line shows real context windows
/// without starting the gateway. Per profile: each profile's gateway serves its own models.
fn cache() -> crate::jsoncache::JsonCache {
    crate::jsoncache::JsonCache::new(mu_common::paths::profile_cache_dir().join("gateway-models.json"))
}

fn load_cache() -> Option<Vec<GatewayModel>> {
    cache().load(|text| {
        let v: Value = serde_json::from_str(text).ok()?;
        Some(
            v.as_array()?
                .iter()
                .filter_map(|m| {
                    Some(GatewayModel {
                        id: m["id"].as_str()?.into(),
                        context_length: m["context_length"].as_u64().map(|n| n as u32),
                        max_output_tokens: m["max_output_tokens"].as_u64().map(|n| n as u32),
                    })
                })
                .collect(),
        )
    })
}

fn save_cache(rows: &[GatewayModel]) {
    if rows.is_empty() {
        return;
    }
    let v: Vec<Value> = rows
        .iter()
        .map(|m| serde_json::json!({"id": m.id, "context_length": m.context_length, "max_output_tokens": m.max_output_tokens}))
        .collect();
    cache().save(&Value::Array(v).to_string());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gateway_wrapped_permanent_errors_fail_fast() {
        // omp's auth-gateway reports thrown validation errors (here: an unsupported effort) as
        // in-stream `api_error`; retrying those five times just delays the visible failure.
        let e = stream_error(
            "api_error",
            "Thinking effort medium is not supported by baseten/zai-org/GLM-5.3-Flash. Supported efforts: low, high, max",
        );
        assert!(matches!(e, ProviderError::Fatal(_)));
        assert!(matches!(stream_error("api_error", "upstream disconnected"), ProviderError::Retryable { .. }));
        assert!(matches!(stream_error("overloaded_error", "busy"), ProviderError::Retryable { .. }));
        assert!(matches!(stream_error("invalid_request_error", "not supported"), ProviderError::Fatal(_)));
    }
}

#[cfg(test)]
mod route_tests {
    use super::*;

    #[test]
    fn api_strings_are_omps_and_sessions_stay_replayable() {
        for a in [Api::AnthropicMessages, Api::OpenAiResponses, Api::OpenAiCompletions] {
            assert_eq!(Api::parse(a.as_str()), Some(a));
        }
        assert_eq!(Api::parse("anthropic-messages"), Some(Api::AnthropicMessages));
        assert_eq!(Api::parse("google-gemini"), None);
        // A message an omp-written (or earlier mu) session stores replays to the same provider only.
        let m = ModelSpec::gateway("anthropic/claude-sonnet-5-5", 1, 1);
        assert!(same_origin("anthropic-messages", "anthropic", &m));
        assert!(!same_origin("anthropic-messages", "openai", &m));
        assert!(!same_origin("openai-responses", "anthropic", &m));
    }

    #[test]
    fn peers_are_named_by_route() {
        let gw = Route { base: "http://127.0.0.1:9".into(), auth: Auth::Bearer("t".into()), def: None };
        assert_eq!(gw.peer(), "gateway at http://127.0.0.1:9");
        assert_eq!(gw.idle_peer(), "the gateway");
        let n = Route {
            base: "https://api.anthropic.com/v1".into(),
            auth: Auth::XApiKey("k".into()),
            def: native::def("anthropic"),
        };
        assert_eq!((n.peer().as_str(), n.idle_peer().as_str()), ("api.anthropic.com", "api.anthropic.com"));
    }
}

#[cfg(test)]
mod native_route_tests {
    use super::*;
    use crate::auth_store::{Entry, Store};
    use crate::testutil::{Reply, fake_server};
    use std::path::PathBuf;

    const SSE: &str = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m1\"}}\n\n\
        event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n\
        event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"pong\"}}\n\n\
        event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
        event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"input_tokens\":10,\"output_tokens\":2,\"cache_read_input_tokens\":4}}\n\n\
        event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";

    fn ok() -> Reply {
        Reply::new(200, SSE).header("content-type", "text/event-stream")
    }

    /// The `!command` key cache is per process and keyed by provider, so these tests take turns.
    fn rt<F: std::future::Future>(f: F) -> F::Output {
        static TURN: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _turn = TURN.lock().unwrap_or_else(|e| e.into_inner());
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(f)
    }

    /// Providers over a private store whose `anthropic` entry points at `base`, with no gateway.
    fn providers(entry: Entry) -> (Providers, PathBuf) {
        let d = std::env::temp_dir().join(format!("mu-route-{}", uuid::Uuid::new_v4()));
        let store = Store::at(d.join("auth.json"));
        store.set("anthropic", entry).unwrap();
        let (ntx, _nrx) = tokio::sync::mpsc::unbounded_channel();
        let plugins = crate::plugin::PluginHost::new(vec![], vec![], ntx, Default::default());
        let mut p = Providers::new(Rc::new(Configs::default()), crate::gateway::GatewaySource::new(plugins));
        p.store = store;
        (p, d)
    }

    fn entry(key: &str, base: &str) -> Entry {
        Entry { base_url: Some(base.into()), ..Entry::literal(key) }
    }

    async fn turn(p: &Providers, level: Thinking, events: &mut Vec<String>) -> Result<Message, ProviderError> {
        let (spec, _) = p.resolve("anthropic/claude-sonnet-5-5").unwrap();
        assert!(spec.native, "a stored key routes the provider natively");
        let tools = [ToolDef { name: "t".into(), description: "d".into(), schema: json!({"type":"object"}) }];
        let msgs = [Message::user("ping")];
        let req = Request {
            model: &spec,
            thinking: Rc::new(Cell::new(level)),
            system: "be brief",
            tools: &tools,
            messages: &msgs,
            max_tokens: Some(4096),
            session_id: Some("s1"),
        };
        let mut ev = |e: StreamEvent| {
            events.push(match e {
                StreamEvent::Text(t) => format!("text:{t}"),
                StreamEvent::Retry { delay, reason, .. } => format!("retry:{}ms:{reason}", delay.as_millis()),
                _ => "other".into(),
            })
        };
        p.stream(&req, &CancellationToken::new(), &mut ev).await
    }

    fn body_of(raw: &str) -> Value {
        serde_json::from_str(raw.split("\r\n\r\n").nth(1).unwrap()).unwrap()
    }

    #[test]
    fn a_native_request_carries_the_key_and_betas_and_no_authorization() {
        rt(async {
            let (base, seen) = fake_server(vec![ok()]).await;
            let (p, d) = providers(entry("sk-ant-api03-NATIVE0123456789", &base));
            let mut events = Vec::new();
            let m = turn(&p, Thinking::High, &mut events).await.unwrap();
            let Message::Assistant { content, api, provider, usage, .. } = &m else { panic!() };
            assert_eq!((api.as_str(), provider.as_str()), ("anthropic-messages", "anthropic"));
            assert!(matches!(&content[0], Block::Text { text } if text == "pong"));
            assert_eq!((usage.input, usage.cache_read), (10, 4));
            assert_eq!(events, ["text:pong"]);

            let raw = seen.lock().unwrap()[0].clone();
            let head = raw.split("\r\n\r\n").next().unwrap().to_lowercase();
            assert!(head.starts_with("post /v1/messages "), "{head}");
            assert!(head.contains("x-api-key: sk-ant-api03-native0123456789"), "{head}");
            assert!(head.contains("anthropic-version: 2023-06-01"), "{head}");
            assert!(!head.contains("authorization"), "{head}");
            let beta = head.lines().find(|l| l.starts_with("anthropic-beta:")).unwrap();
            for b in ["effort-2025-11-24", "context-management-2025-06-27", "fine-grained-tool-streaming-2025-05-14"] {
                assert!(beta.contains(b), "{beta}");
            }
            let body = body_of(&raw);
            assert_eq!(body["model"], "claude-sonnet-5-5");
            assert_eq!(body["thinking"]["type"], "adaptive");
            assert_eq!(body["output_config"]["effort"], "high");
            assert_eq!(body.to_string().matches("cache_control").count(), 3, "last tool, system, last message");
            let _ = std::fs::remove_dir_all(d);
        });
    }

    #[test]
    fn a_rejected_key_says_so_and_is_not_echoed() {
        rt(async {
            let (base, seen) =
                fake_server(vec![Reply::new(401, r#"{"error":"invalid x-api-key sk-ant-api03-NATIVE0123456789"}"#)])
                    .await;
            let (p, d) = providers(entry("sk-ant-api03-NATIVE0123456789", &base));
            let e = turn(&p, Thinking::Off, &mut Vec::new()).await.unwrap_err();
            assert!(matches!(e, ProviderError::Fatal(_)), "{e:?}");
            let msg = e.to_string();
            assert!(msg.contains("Anthropic rejected the API key; run /login to replace it"), "{msg}");
            assert!(!msg.contains("NATIVE0123456789") && !msg.contains("omp login"), "{msg}");
            assert_eq!(seen.lock().unwrap().len(), 1, "a stored key is not retried");
            let _ = std::fs::remove_dir_all(d);
        });
    }

    #[test]
    fn a_command_key_is_run_again_once_after_a_401() {
        rt(async {
            let d0 = std::env::temp_dir().join(format!("mu-rekey-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&d0).unwrap();
            let counter = d0.join("n");
            let cmd = format!(
                "!n=$(cat {c} 2>/dev/null || echo 0); echo $((n+1)) > {c}; echo rotating-key-$n",
                c = counter.display()
            );
            let (base, seen) = fake_server(vec![Reply::new(401, "stale"), ok()]).await;
            let (p, d) = providers(Entry { base_url: Some(base.clone()), ..Entry::reference(&cmd).unwrap() });
            let mut events = Vec::new();
            turn(&p, Thinking::Off, &mut events).await.unwrap();
            let reqs = seen.lock().unwrap().clone();
            assert_eq!(reqs.len(), 2);
            assert!(reqs[0].to_lowercase().contains("x-api-key: rotating-key-0"), "{}", reqs[0]);
            assert!(reqs[1].to_lowercase().contains("x-api-key: rotating-key-1"), "{}", reqs[1]);
            assert!(events.iter().any(|e| e.starts_with("retry:0ms")), "{events:?}");

            // Rejected again: one re-run only, then the error.
            let (base, seen) = fake_server(vec![Reply::new(401, "stale")]).await;
            let (p, d2) = providers(Entry {
                base_url: Some(base),
                ..Entry::reference(&cmd.replace("echo rotating", "echo again")).unwrap()
            });
            let e = turn(&p, Thinking::Off, &mut Vec::new()).await.unwrap_err();
            assert!(e.to_string().contains("rejected the API key"), "{e}");
            assert_eq!(seen.lock().unwrap().len(), 2);
            for x in [d, d2, d0] {
                let _ = std::fs::remove_dir_all(x);
            }
        });
    }

    #[test]
    fn retry_after_ms_and_x_should_retry_are_honoured() {
        rt(async {
            let (base, seen) =
                fake_server(vec![Reply::new(429, "slow down").header("retry-after-ms", "7"), ok()]).await;
            let (p, d) = providers(entry("sk-ant-api03-NATIVE0123456789", &base));
            let mut events = Vec::new();
            turn(&p, Thinking::Off, &mut events).await.unwrap();
            assert_eq!(seen.lock().unwrap().len(), 2);
            assert!(events[0].starts_with("retry:7ms:"), "{events:?}");

            let (base, seen) = fake_server(vec![Reply::new(529, "overloaded").header("x-should-retry", "false")]).await;
            let (p, d2) = providers(entry("sk-ant-api03-NATIVE0123456789", &base));
            let e = turn(&p, Thinking::Off, &mut Vec::new()).await.unwrap_err();
            assert!(matches!(e, ProviderError::Fatal(_)), "{e:?}");
            assert_eq!(seen.lock().unwrap().len(), 1);
            for x in [d, d2] {
                let _ = std::fs::remove_dir_all(x);
            }
        });
    }

    #[test]
    fn an_unreachable_host_is_named_not_called_the_gateway() {
        rt(async {
            let (p, d) = providers(entry("sk-ant-api03-NATIVE0123456789", "http://127.0.0.1:1"));
            let mut events = Vec::new();
            // Retries are quick: connection refused, five attempts at 2^n seconds would be slow, so
            // cancel after the first Retry event.
            let cancel = CancellationToken::new();
            let (spec, _) = p.resolve("anthropic/claude-sonnet-5-5").unwrap();
            let msgs = [Message::user("x")];
            let req = Request {
                model: &spec,
                thinking: Rc::new(Cell::new(Thinking::Off)),
                system: "",
                tools: &[],
                messages: &msgs,
                max_tokens: Some(100),
                session_id: None,
            };
            let c2 = cancel.clone();
            let mut ev = |e: StreamEvent| {
                if let StreamEvent::Retry { reason, .. } = e {
                    events.push(reason);
                    c2.cancel();
                }
            };
            let _ = p.stream(&req, &cancel, &mut ev).await;
            assert!(events[0].starts_with("cannot reach 127.0.0.1:1"), "{events:?}");
            assert!(!events[0].contains("gateway"), "{events:?}");
            let _ = std::fs::remove_dir_all(d);
        });
    }

    /// The key is sent only to the configured host: a redirect is returned, not followed.
    #[test]
    fn a_redirect_is_not_followed_so_the_key_stays_home() {
        rt(async {
            let (base, seen) =
                fake_server(vec![Reply::new(302, "").header("location", "http://127.0.0.1:9/elsewhere")]).await;
            let (p, d) = providers(entry("test-key", &base));
            let e = turn(&p, Thinking::Off, &mut Vec::new()).await.unwrap_err();
            assert!(e.to_string().starts_with("HTTP 302"), "{e}");
            assert_eq!(seen.lock().unwrap().len(), 1);
            let _ = std::fs::remove_dir_all(d);
        });
    }
}

#[cfg(test)]
mod contract_native_anthropic_tests {
    use super::*;
    use crate::auth_store::{Entry, Store};

    /// Two turns against the real API: thinking with a tool call, then the tool result with the
    /// thinking block replayed. Needs `ANTHROPIC_API_KEY`; `ANTHROPIC_CONTRACT_MODEL` picks the model.
    #[test]
    #[ignore = "contract: calls the real Anthropic API; set ANTHROPIC_API_KEY"]
    fn contract_native_anthropic() {
        if std::env::var("ANTHROPIC_API_KEY").is_err() {
            eprintln!("skipped contract_native_anthropic: ANTHROPIC_API_KEY is not set");
            return;
        }
        let model = std::env::var("ANTHROPIC_CONTRACT_MODEL").unwrap_or_else(|_| "claude-sonnet-5-5".into());
        let d = std::env::temp_dir().join(format!("mu-contract-{}", uuid::Uuid::new_v4()));
        let store = Store::at(d.join("auth.json"));
        store.set("anthropic", Entry::reference("$ANTHROPIC_API_KEY").unwrap()).unwrap();
        let (ntx, _nrx) = tokio::sync::mpsc::unbounded_channel();
        let plugins = crate::plugin::PluginHost::new(vec![], vec![], ntx, Default::default());
        let mut p = Providers::new(Rc::new(Configs::default()), crate::gateway::GatewaySource::new(plugins));
        p.store = store;
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let (spec, _) = p.resolve(&format!("anthropic/{model}")).unwrap();
            assert!(spec.native);
            let tools = [ToolDef {
                name: "get_word".into(),
                description: "Returns the secret word.".into(),
                schema: json!({"type":"object","properties":{},"additionalProperties":false}),
            }];
            let mut msgs = vec![Message::user("Call get_word, then tell me the word it returned.")];
            let cancel = CancellationToken::new();
            let mut last = None;
            for turn in 0..3 {
                let req = Request {
                    model: &spec,
                    thinking: Rc::new(Cell::new(Thinking::High)),
                    system: "Use tools when asked.",
                    tools: &tools,
                    messages: &msgs,
                    max_tokens: Some(8000),
                    session_id: Some("contract"),
                };
                let reply = p.stream(&req, &cancel, &mut |_| {}).await.unwrap_or_else(|e| panic!("turn {turn}: {e}"));
                let Message::Assistant { content, stop_reason, usage, .. } = &reply else { panic!() };
                if turn > 0 {
                    assert!(usage.cache_read > 0 || usage.cache_write > 0, "prompt caching shows in usage: {usage:?}");
                }
                let calls: Vec<(String, String)> = content
                    .iter()
                    .filter_map(|b| match b {
                        Block::ToolCall { id, name, .. } => Some((id.clone(), name.clone())),
                        _ => None,
                    })
                    .collect();
                last = Some((*stop_reason, content.clone()));
                msgs.push(reply.clone());
                if calls.is_empty() {
                    break;
                }
                for (id, name) in calls {
                    msgs.push(Message::ToolResult {
                        tool_call_id: id,
                        tool_name: name,
                        content: vec![Block::text("plover")],
                        is_error: false,
                        timestamp: 0,
                        details: None,
                    });
                }
            }
            let (stop, content) = last.unwrap();
            assert_eq!(stop, crate::message::StopReason::Stop);
            let text: String = content
                .iter()
                .filter_map(|b| if let Block::Text { text } = b { Some(text.as_str()) } else { None })
                .collect();
            assert!(text.to_lowercase().contains("plover"), "{text}");
        });
        let _ = std::fs::remove_dir_all(d);
    }
}

#[cfg(test)]
mod other_native_route_tests {
    use super::*;
    use crate::auth_store::{Entry, Store};
    use crate::testutil::{Reply, fake_server};

    fn providers(provider: &str, key: &str, base: &str) -> (Providers, std::path::PathBuf) {
        let d = std::env::temp_dir().join(format!("mu-route2-{}", uuid::Uuid::new_v4()));
        let store = Store::at(d.join("auth.json"));
        store.set(provider, Entry { base_url: Some(base.into()), ..Entry::literal(key) }).unwrap();
        let (ntx, _nrx) = tokio::sync::mpsc::unbounded_channel();
        let plugins = crate::plugin::PluginHost::new(vec![], vec![], ntx, Default::default());
        let mut p = Providers::new(Rc::new(Configs::default()), crate::gateway::GatewaySource::new(plugins));
        p.store = store;
        (p, d)
    }

    async fn run(p: &Providers, sel: &str, level: Thinking) -> Result<Message, ProviderError> {
        let (spec, _) = p.resolve(sel).unwrap();
        assert!(spec.native);
        let msgs = [Message::user("hi")];
        let tools = [ToolDef { name: "read".into(), description: "d".into(), schema: json!({"type":"object"}) }];
        let req = Request {
            model: &spec,
            thinking: Rc::new(Cell::new(level)),
            system: "sys",
            tools: &tools,
            messages: &msgs,
            max_tokens: Some(1000),
            session_id: Some("sess-1"),
        };
        p.stream(&req, &CancellationToken::new(), &mut |_| {}).await
    }

    fn rt<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(f)
    }

    #[test]
    fn openai_goes_to_responses_with_a_bearer_key() {
        rt(async {
            let sse = include_str!("testdata/responses/text.sse");
            let (base, seen) = fake_server(vec![Reply::new(200, sse)]).await;
            let (p, d) = providers("openai", "sk-openai-ROUTE0123456789", &base);
            let m = run(&p, "openai/gpt-5.5", Thinking::High).await.unwrap();
            let Message::Assistant { api, provider, content, .. } = &m else { panic!() };
            assert_eq!((api.as_str(), provider.as_str()), ("openai-responses", "openai"));
            assert!(matches!(&content[0], Block::Text { text } if text == "Hello ✓"));
            let raw = seen.lock().unwrap()[0].clone();
            let head = raw.split("\r\n\r\n").next().unwrap().to_lowercase();
            assert!(head.starts_with("post /responses "), "{head}");
            assert!(head.contains("authorization: bearer sk-openai-route0123456789"), "{head}");
            assert!(!head.contains("x-api-key") && !head.contains("anthropic"), "{head}");
            let body: Value = serde_json::from_str(raw.split("\r\n\r\n").nth(1).unwrap()).unwrap();
            assert_eq!(body["model"], "gpt-5.5");
            assert_eq!(body["store"], false);
            assert_eq!(body["prompt_cache_key"], "sess-1");
            assert_eq!(body["reasoning"]["effort"], "high");
            let _ = std::fs::remove_dir_all(d);
        });
    }

    #[test]
    fn baseten_goes_to_chat_completions() {
        rt(async {
            let sse = include_str!("testdata/completions/interleaved.sse");
            let (base, seen) = fake_server(vec![Reply::new(200, sse)]).await;
            let (p, d) = providers("baseten", "bt-ROUTE0123456789", &base);
            let m = run(&p, "baseten/moonshotai/Kimi-K3", Thinking::High).await.unwrap();
            let Message::Assistant { api, content, .. } = &m else { panic!() };
            assert_eq!(api, "openai-completions");
            assert!(content.iter().any(|b| matches!(b, Block::ToolCall { name, .. } if name == "read")), "{content:?}");
            let raw = seen.lock().unwrap()[0].clone();
            let head = raw.split("\r\n\r\n").next().unwrap().to_lowercase();
            assert!(head.starts_with("post /chat/completions "), "{head}");
            assert!(head.contains("authorization: bearer bt-route0123456789"), "{head}");
            let body: Value = serde_json::from_str(raw.split("\r\n\r\n").nth(1).unwrap()).unwrap();
            assert_eq!(body["model"], "moonshotai/Kimi-K3");
            assert_eq!(body["reasoning_effort"], "high");
            assert_eq!(body["stream"], true);
            let _ = std::fs::remove_dir_all(d);
        });
    }

    #[test]
    fn a_provider_without_a_key_still_goes_to_the_gateway() {
        let (p, d) = providers("anthropic", "sk-ant-api03-ONLYANTHROPIC0123", "http://127.0.0.1:1");
        let (spec, _) = p.resolve("openai/gpt-5.5").unwrap();
        assert!(!spec.native && spec.api == Api::AnthropicMessages);
        let (spec, _) = p.resolve("modal/some/model").unwrap();
        assert!(!spec.native);
        let _ = std::fs::remove_dir_all(d);
    }
}

#[cfg(test)]
mod wire_contract_tests {
    use super::*;
    use crate::message::{Block, StopReason, Usage};
    use crate::models::{Api, ThinkingMode, ThinkingSpec};
    const LONG_ID: &str = "call_0123456789abcdefghijklmn|fc_0123456789abcdef0123456789abcdef";

    fn spec(id: &str, api: Api, mode: ThinkingMode) -> ModelSpec {
        let (provider, wire_id) = id.split_once('/').unwrap();
        ModelSpec {
            id: id.into(),
            provider: provider.into(),
            context_window: 200_000,
            max_output: 8000,
            api,
            native: true,
            wire_id: wire_id.into(),
            thinking: ThinkingSpec { mode, efforts: vec!["low".into(), "high".into()] },
            images: true,
            compat: None,
            anthropic: Default::default(),
        }
    }

    fn assistant(content: Vec<Block>, api: &str, provider: &str, model: &str) -> Message {
        Message::Assistant {
            content,
            api: api.into(),
            provider: provider.into(),
            model: model.into(),
            usage: Usage::default(),
            stop_reason: StopReason::ToolUse,
            timestamp: 0,
            response_id: None,
            error_message: None,
        }
    }

    fn result(id: &str) -> Message {
        Message::ToolResult {
            tool_call_id: id.into(),
            tool_name: "read".into(),
            content: vec![Block::text("ok")],
            is_error: false,
            timestamp: 0,
            details: None,
        }
    }

    fn request<'a>(model: &'a ModelSpec, messages: &'a [Message]) -> Request<'a> {
        Request {
            model,
            thinking: Rc::new(Cell::new(Thinking::High)),
            system: "sys",
            tools: &[],
            messages,
            max_tokens: None,
            session_id: Some("s"),
        }
    }

    #[test]
    fn normalize_id_is_length_safe_and_stable() {
        assert_eq!(LONG_ID.len(), 65);
        let n = normalize_id(LONG_ID);
        assert_eq!(n.len(), 64);
        assert_eq!(n, normalize_id(LONG_ID));
        assert_ne!(n, normalize_id(&LONG_ID.replace("fc_0123", "fc_9999")));
        assert_eq!(normalize_id("toolu_01A"), "toolu_01A");
        assert_eq!(normalize_id("a b.c"), "a_b_c");

        // A tool_use and its tool_result map to the same id on both wires that strip `|`.
        let msgs = [
            assistant(
                vec![Block::ToolCall { id: LONG_ID.into(), name: "read".into(), arguments: json!({}) }],
                "anthropic-messages",
                "anthropic",
                "anthropic/claude-sonnet-5-5",
            ),
            result(LONG_ID),
        ];
        let anth = spec("anthropic/claude-sonnet-5-5", Api::AnthropicMessages, ThinkingMode::AnthropicAdaptive);
        let wire = Value::Array(anthropic::to_wire(&msgs, &anth)).to_string();
        assert!(!wire.contains(LONG_ID));
        assert_eq!(wire.matches(&format!("\"{n}\"")).count(), 2, "{wire}");

        // Chat Completions keeps a same-model id verbatim (omp does) and normalises another model's.
        let bt = spec("baseten/moonshotai/Kimi-K3", Api::OpenAiCompletions, ThinkingMode::Effort);
        let call = |api: &str, provider: &str, model: &str| {
            [
                assistant(
                    vec![Block::ToolCall { id: LONG_ID.into(), name: "read".into(), arguments: json!({}) }],
                    api,
                    provider,
                    model,
                ),
                result(LONG_ID),
            ]
        };
        let msgs = call("openai-responses", "openai", "openai/gpt-5.5");
        let wire = Value::Array(openai_completions::to_wire("", &msgs, &bt)).to_string();
        assert!(!wire.contains(LONG_ID));
        assert_eq!(wire.matches(&format!("\"{n}\"")).count(), 2, "{wire}");
        let msgs = call("openai-completions", "baseten", "baseten/moonshotai/Kimi-K3");
        let wire = Value::Array(openai_completions::to_wire("", &msgs, &bt)).to_string();
        assert_eq!(wire.matches(&format!("\"{LONG_ID}\"")).count(), 2, "{wire}");
    }

    #[test]
    fn an_omp_session_loads_and_replays_only_to_its_own_wire() {
        let dir = std::env::temp_dir().join(format!("mu-omp-session-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.jsonl");
        std::fs::write(&path, include_str!("testdata/omp_session.jsonl")).unwrap();
        let loaded = crate::session::load(&path).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
        let msgs = loaded.messages;
        assert_eq!(msgs.len(), 11);

        let anth = spec("anthropic/claude-sonnet-5-5", Api::AnthropicMessages, ThinkingMode::AnthropicAdaptive);
        let resp = spec("openai/gpt-5.5", Api::OpenAiResponses, ThinkingMode::Effort);
        let bt = spec("baseten/moonshotai/Kimi-K3", Api::OpenAiCompletions, ThinkingMode::Effort);

        // Anthropic: its own signed thinking replays; no OpenAI reasoning item leaks, native or gateway.
        let r = request(&anth, &msgs);
        let native = anthropic_native::body(&r).0.to_string();
        let gateway = anthropic::body(&r).to_string();
        for body in [&native, &gateway] {
            assert!(body.contains("EqQBCkYIARgCIiQ3Zm9vYmFyYmF6MTIzNDU2"), "own signature replays");
            assert!(!body.contains("rs_0a1b2c3d4e5f") && !body.contains("gAAAAABfakeEncrypted"), "{body}");
            assert!(!body.contains("Reading the file first") && !body.contains("Two files"), "{body}");
        }

        // Responses: its own reasoning item and the long call id replay; Anthropic and Chat
        // Completions thinking never do.
        let r = openai_responses::body(&request(&resp, &msgs)).to_string();
        assert!(r.contains("rs_0a1b2c3d4e5f") && r.contains("gAAAAABfakeEncrypted"), "{r}");
        assert!(r.contains("call_0123456789abcdefghijklmnopqr") && r.contains("fc_0123456789abcdef0123456789abcdef"));
        assert!(!r.contains("EqQBCk") && !r.contains("The user wants") && !r.contains("Two files"), "{r}");

        // Chat Completions: no reasoning item or signature; other models' thinking is demoted to
        // plain text (as omp does), never sent as `reasoning_content`.
        let c = Value::Array(openai_completions::to_wire("sys", &msgs, &bt)).to_string();
        assert!(!c.contains("rs_0a1b2c3d4e5f") && !c.contains("EqQBCk") && !c.contains("gAAAAABfake"), "{c}");
        assert!(c.contains("The user wants") && !c.contains("\"reasoning_content\":\"The user wants"), "{c}");
    }
}
