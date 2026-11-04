use super::endpoint;
use super::gateway_models::load_cache;
use super::providers::{ProviderDef, direct_spec, native_def};
use super::retry;
use super::types::{Auth, Decoder, ProviderError, Request, StreamEvent};
use super::wire::{self, WireFormat};
use crate::net::{self, host_of};
use anyhow::Result;
use futures_util::StreamExt;
use mu_base::config::Configs;
#[cfg(any(test, feature = "test-support"))]
use mu_base::message::Block;
use mu_base::message::Message;
use mu_base::models::{self, GatewayModel, ModelSpec, Thinking};
use serde_json::{Value, json};
#[cfg(test)]
use std::cell::Cell;
use std::cell::{OnceCell, RefCell};
use std::io::Write;
use std::rc::Rc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Where one request goes: the gateway on loopback, or a provider's own API.
pub struct Route {
    pub base: String,
    pub auth: Auth,
    /// The provider whose own API this is; `None` for the gateway.
    pub def: Option<&'static ProviderDef>,
}

impl Route {
    pub fn is_direct(&self) -> bool {
        self.def.is_some()
    }

    /// Names the peer in connect errors.
    fn peer(&self) -> String {
        if self.is_direct() { host_of(&self.base) } else { format!("gateway at {}", self.base) }
    }

    fn idle_peer(&self) -> String {
        if self.is_direct() { host_of(&self.base) } else { "the gateway".into() }
    }
}

/// Appends one JSON line per model request to `$MU_DUMP_REQUESTS`, for debugging. Only the shape of
/// the request is written: the system prompt, messages and tools are counted, not logged.
fn dump_request(req: &Request, body: &Value) {
    let Some(path) = std::env::var_os("MU_DUMP_REQUESTS").filter(|p| !p.is_empty()) else { return };
    let mut line = json!({
        "ts": mu_base::message::now_ms(),
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

pub(super) struct Endpoint {
    pub(super) base: String,
    pub(super) token: String,
}

/// No bytes for this long on a response stream: treat the connection as dead and retry.
const STREAM_IDLE: Duration = Duration::from_secs(90);

#[cfg(test)]
fn no_gateway() -> endpoint::GatewaySource {
    endpoint::GatewaySource::new(endpoint::start_fn(|| async {
        Err::<(String, String), String>("no gateway in tests".into())
    }))
}

pub struct Providers {
    pub cfg: Rc<Configs>,
    pub(super) client: OnceCell<reqwest::Client>,
    pub(super) gw_models: RefCell<Option<Vec<GatewayModel>>>,
    /// `gw_models` came from the running gateway, not the on-disk cache.
    pub(super) gw_fresh: std::cell::Cell<bool>,
    pub(super) gateway: endpoint::GatewaySource,
    pub(super) store: crate::auth::store::Store,
    /// Test seam: scripted replies, popped in order by `stream`.
    #[cfg(any(test, feature = "test-support"))]
    pub scripted: RefCell<std::collections::VecDeque<Result<Message, ProviderError>>>,
}

impl Providers {
    pub fn new(cfg: Rc<Configs>, gateway: endpoint::GatewaySource) -> Self {
        Self::with_store(cfg, gateway, crate::auth::store::Store::new())
    }

    /// `new` with the key store inside the test `MU_HOME`, never the real profile's.
    #[cfg(any(test, feature = "test-support"))]
    pub fn for_tests(cfg: Rc<Configs>, gateway: endpoint::GatewaySource) -> Self {
        Self::with_store(cfg, gateway, crate::auth::store::Store::at(mu_testutil::home().join("auth.json")))
    }

    fn with_store(cfg: Rc<Configs>, gateway: endpoint::GatewaySource, store: crate::auth::store::Store) -> Self {
        Self {
            cfg,
            client: OnceCell::new(),
            gw_models: RefCell::new(load_cache()),
            gw_fresh: std::cell::Cell::new(false),
            gateway,
            store,
            #[cfg(any(test, feature = "test-support"))]
            scripted: Default::default(),
        }
    }

    /// One shared client: a 10 s connect timeout, no redirects, no overall timeout (streams are
    /// long-lived; `STREAM_IDLE` guards them instead).
    pub(super) fn client(&self) -> &reqwest::Client {
        self.client.get_or_init(|| net::http_client().unwrap_or_else(|_| reqwest::Client::new()))
    }

    pub fn store(&self) -> &crate::auth::store::Store {
        &self.store
    }

    pub fn resolve(&self, sel: &str) -> Result<(ModelSpec, Option<Thinking>)> {
        let direct = |p: &str, m: &str| self.is_direct(p).then(|| direct_spec(p, m)).flatten();
        models::resolve(sel, &self.cfg, self.gw_models.borrow().as_deref(), &direct)
    }

    pub(super) async fn endpoint(&self) -> Result<Endpoint, ProviderError> {
        let (base, token) = self.gateway.endpoint().await.map_err(|e| ProviderError::Fatal(e.to_string()))?;
        Ok(Endpoint { base, token })
    }

    /// Where a request for `m` goes.
    async fn route(&self, m: &ModelSpec) -> Result<Route, ProviderError> {
        if m.native {
            let def = native_def(&m.provider)
                .ok_or_else(|| ProviderError::Fatal(format!("{} is not a native provider", m.provider)))?;
            let r = self.store.resolve(&m.provider).await.map_err(|e| ProviderError::Fatal(e.to_string()))?;
            let base = r.base_url.unwrap_or_else(|| def.base_url.to_string());
            return Ok(Route { base, auth: Auth::for_def(def, &r.key), def: Some(def) });
        }
        let ep = self.endpoint().await?;
        Ok(Route { base: ep.base, auth: Auth::Bearer(ep.token), def: None })
    }

    pub async fn stream(
        &self,
        req: &Request<'_>,
        cancel: &CancellationToken,
        ev: &mut dyn FnMut(StreamEvent),
    ) -> Result<Message, ProviderError> {
        #[cfg(any(test, feature = "test-support"))]
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
        let format = wire::select(req.model, &route);
        let body = format.body(req);
        dump_request(req, &body);
        let rb = self.build(req, &route, format.as_ref(), &body)?;
        let resp = send(rb, &route, cancel).await?;
        let resp = check_status(resp, &route, rekeyed).await?;
        pump_sse(resp, format.decoder(req), &route, cancel, ev).await
    }

    /// The POST for `body`: the wire's URL and headers, then the route's auth.
    fn build(
        &self,
        req: &Request<'_>,
        route: &Route,
        format: &dyn WireFormat,
        body: &Value,
    ) -> Result<reqwest::RequestBuilder, ProviderError> {
        let mut rb = self.client().post(format.url(&route.base)).header("content-type", "application/json");
        for (k, v) in format.headers(req) {
            rb = rb.header(k, v);
        }
        route.auth.apply(rb.body(body.to_string()))
    }
}

/// Sends `rb`, or gives up when `cancel` fires. A failed connect is retried and names the peer.
async fn send(
    rb: reqwest::RequestBuilder,
    route: &Route,
    cancel: &CancellationToken,
) -> Result<reqwest::Response, ProviderError> {
    tokio::select! {
        r = rb.send() => r.map_err(|e| ProviderError::Retryable {
            msg: format!("cannot reach {}: {e}", route.peer()),
            retry_after: None,
        }),
        _ = cancel.cancelled() => Err(ProviderError::Aborted),
    }
}

/// The response of a success status. An error status becomes the error to return or retry; a
/// 401 from a `!command` key runs the command once more (`rekeyed` records that it did).
async fn check_status(
    resp: reqwest::Response,
    route: &Route,
    rekeyed: &mut bool,
) -> Result<reqwest::Response, ProviderError> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let (retry_after, should_retry) = retry::headers(resp.headers());
    if let Some(e) = retry::too_long(retry_after) {
        return Err(e);
    }
    let text = mu_base::secrets::redact_keys(&resp.text().await.unwrap_or_default());
    if let Some(def) = route.def
        && status.as_u16() == 401
        && !*rekeyed
        && crate::auth::store::forget_cached(def.id)
    {
        *rekeyed = true;
        return Err(ProviderError::Retryable {
            msg: format!("{} rejected the API key; running its command again", def.label),
            retry_after: Some(Duration::ZERO),
        });
    }
    Err(retry::http_error(status.as_u16(), retry_after, should_retry, &text, route.def.map(|d| d.label)))
}

/// Feeds the response's bytes to `dec` until it completes the message. Silence for `STREAM_IDLE`
/// or a cancel ends the turn.
async fn pump_sse(
    resp: reqwest::Response,
    mut dec: Box<dyn Decoder>,
    route: &Route,
    cancel: &CancellationToken,
    ev: &mut dyn FnMut(StreamEvent),
) -> Result<Message, ProviderError> {
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

#[cfg(test)]
mod route_tests {
    use super::*;
    use crate::provider::wire::convert::same_origin;
    use mu_base::models::Api;

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
            def: native_def("anthropic"),
        };
        assert_eq!((n.peer().as_str(), n.idle_peer().as_str()), ("api.anthropic.com", "api.anthropic.com"));
    }
}

#[cfg(test)]
mod native_route_tests {
    use super::*;
    use crate::auth::store::{Entry, Store};
    use mu_base::tool::ToolDef;
    use mu_testutil::{Reply, fake_server};
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
        mu_testutil::block_on(f)
    }

    /// Providers over a private store whose `anthropic` entry points at `base`, with no gateway.
    fn providers(entry: Entry) -> (Providers, PathBuf) {
        let d = std::env::temp_dir().join(format!("mu-route-{}", uuid::Uuid::new_v4()));
        let store = Store::at(d.join("auth.json"));
        store.set("anthropic", entry).unwrap();
        let p = Providers::with_store(Rc::new(Configs::default()), no_gateway(), store);
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
    use crate::auth::store::{Entry, Store};
    use mu_base::tool::ToolDef;

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
        let p = Providers::with_store(Rc::new(Configs::default()), no_gateway(), store);
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
            assert_eq!(stop, mu_base::message::StopReason::Stop);
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
    use crate::auth::store::{Entry, Store};
    use mu_base::models::Api;
    use mu_base::tool::ToolDef;
    use mu_testutil::block_on as rt;
    use mu_testutil::{Reply, fake_server};

    fn providers(provider: &str, key: &str, base: &str) -> (Providers, std::path::PathBuf) {
        let d = std::env::temp_dir().join(format!("mu-route2-{}", uuid::Uuid::new_v4()));
        let store = Store::at(d.join("auth.json"));
        store.set(provider, Entry { base_url: Some(base.into()), ..Entry::literal(key) }).unwrap();
        let p = Providers::with_store(Rc::new(Configs::default()), no_gateway(), store);
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

    #[test]
    fn openai_goes_to_responses_with_a_bearer_key() {
        rt(async {
            let sse = include_str!("wire/testdata/responses/text.sse");
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
            let sse = include_str!("wire/testdata/completions/interleaved.sse");
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
