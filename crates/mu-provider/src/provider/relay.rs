//! The model relay: what a worker sends its parent in place of provider calls, and the seam
//! `Providers` hands those calls to. The types are the NDJSON protocol lines
//! `{"type":"relay","id":N,...}`; the transport is the caller's.

use super::complete::NoModel;
use super::types::{ProviderError, Request, StreamEvent};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use mu_base::message::{Message, Usage};
use mu_base::models::{ModelSpec, Thinking};
use mu_base::tool::ToolDef;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::cell::Cell;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// The runtime is single-threaded: no `Send`, as with `DeliverFn`.
pub type LocalFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;

/// What the parent answers to `resolve`.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub spec: ModelSpec,
    pub thinking: Option<Thinking>,
}

/// What the parent answers to `search`.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchReply {
    pub text: String,
    pub is_error: bool,
}

/// Where a relayed process sends its model calls. Errors keep their kind across the hop.
pub trait Relay {
    fn resolve<'a>(&'a self, selector: &'a str) -> LocalFuture<'a, Result<Resolved, RelayError>>;

    fn stream<'a>(
        &'a self,
        req: &'a Request<'a>,
        cancel: &'a CancellationToken,
        ev: &'a mut dyn FnMut(StreamEvent),
    ) -> LocalFuture<'a, Result<Message, ProviderError>>;

    /// `target` is a model id or a role, as `Providers::complete` takes it.
    fn complete<'a>(
        &'a self,
        target: &'a str,
        system: &'a str,
        prompt: &'a str,
        max_tokens: u32,
        cancel: &'a CancellationToken,
    ) -> LocalFuture<'a, Result<String, RelayError>>;

    fn search<'a>(
        &'a self,
        args: Value,
        cancel: &'a CancellationToken,
    ) -> LocalFuture<'a, Result<SearchReply, RelayError>>;

    /// Sends one image's bytes under their hash; there is no answer.
    fn blob(&self, hash: &str, bytes: &[u8]);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ErrorKind {
    Retryable,
    Overflow,
    Fatal,
    Aborted,
    NoModel,
}

/// A failed call as it crosses: a `ProviderError`, or `NoModel` from `complete`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RelayError {
    pub kind: ErrorKind,
    pub msg: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_ms: Option<u64>,
}

impl RelayError {
    pub fn new(kind: ErrorKind, msg: impl Into<String>) -> Self {
        Self { kind, msg: msg.into(), retry_after_ms: None }
    }

    /// The error of a failed `Providers::complete`: `NoModel` keeps its kind.
    pub fn of_anyhow(e: &anyhow::Error) -> Self {
        match e.downcast_ref::<NoModel>() {
            Some(n) => Self::new(ErrorKind::NoModel, n.role()),
            None if e.to_string() == "aborted" => Self::new(ErrorKind::Aborted, "aborted"),
            None => Self::new(ErrorKind::Fatal, format!("{e:#}")),
        }
    }

    pub fn into_anyhow(self) -> anyhow::Error {
        match self.kind {
            ErrorKind::NoModel => NoModel::new(self.msg).into(),
            _ => anyhow::anyhow!(self.msg),
        }
    }
}

impl From<&ProviderError> for RelayError {
    fn from(e: &ProviderError) -> Self {
        match e {
            ProviderError::Retryable { msg, retry_after } => Self {
                kind: ErrorKind::Retryable,
                msg: msg.clone(),
                retry_after_ms: retry_after.map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX)),
            },
            ProviderError::Overflow(m) => Self::new(ErrorKind::Overflow, m.clone()),
            ProviderError::Fatal(m) => Self::new(ErrorKind::Fatal, m.clone()),
            ProviderError::Aborted => Self::new(ErrorKind::Aborted, "aborted"),
        }
    }
}

/// `NoModel` has no `ProviderError` form: it becomes `Fatal`.
impl From<RelayError> for ProviderError {
    fn from(e: RelayError) -> Self {
        match e.kind {
            ErrorKind::Retryable => {
                ProviderError::Retryable { msg: e.msg, retry_after: e.retry_after_ms.map(Duration::from_millis) }
            }
            ErrorKind::Overflow => ProviderError::Overflow(e.msg),
            ErrorKind::Aborted => ProviderError::Aborted,
            ErrorKind::Fatal | ErrorKind::NoModel => ProviderError::Fatal(e.msg),
        }
    }
}

/// An owned `StreamEvent`; `Retry.delay` travels as milliseconds.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Event {
    Text {
        text: String,
    },
    Thinking {
        text: String,
    },
    ToolCallStart {
        id: String,
        name: String,
    },
    ToolCallDelta {
        id: String,
        json: String,
    },
    #[serde(rename_all = "camelCase")]
    Retry {
        attempt: u32,
        max_attempts: u32,
        delay_ms: u64,
        reason: String,
    },
    Notice {
        text: String,
    },
    FailedAttemptUsage {
        usage: Usage,
    },
}

impl From<&StreamEvent<'_>> for Event {
    fn from(e: &StreamEvent<'_>) -> Self {
        match e {
            StreamEvent::Text(t) => Self::Text { text: (*t).into() },
            StreamEvent::Thinking(t) => Self::Thinking { text: (*t).into() },
            StreamEvent::ToolCallStart { id, name } => Self::ToolCallStart { id: (*id).into(), name: (*name).into() },
            StreamEvent::ToolCallDelta { id, json } => Self::ToolCallDelta { id: (*id).into(), json: (*json).into() },
            StreamEvent::Retry { attempt, max_attempts, delay, reason } => Self::Retry {
                attempt: *attempt,
                max_attempts: *max_attempts,
                delay_ms: u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
                reason: reason.clone(),
            },
            StreamEvent::Notice(n) => Self::Notice { text: n.clone() },
            StreamEvent::FailedAttemptUsage(u) => Self::FailedAttemptUsage { usage: u.clone() },
        }
    }
}

impl Event {
    pub fn to_stream(&self) -> StreamEvent<'_> {
        match self {
            Self::Text { text } => StreamEvent::Text(text),
            Self::Thinking { text } => StreamEvent::Thinking(text),
            Self::ToolCallStart { id, name } => StreamEvent::ToolCallStart { id, name },
            Self::ToolCallDelta { id, json } => StreamEvent::ToolCallDelta { id, json },
            Self::Retry { attempt, max_attempts, delay_ms, reason } => StreamEvent::Retry {
                attempt: *attempt,
                max_attempts: *max_attempts,
                delay: Duration::from_millis(*delay_ms),
                reason: reason.clone(),
            },
            Self::Notice { text } => StreamEvent::Notice(text.clone()),
            Self::FailedAttemptUsage { usage } => StreamEvent::FailedAttemptUsage(usage.clone()),
        }
    }
}

/// A `stream` call with everything owned; `request` borrows it as a `Request`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamCall {
    pub model: ModelSpec,
    pub thinking: Thinking,
    pub system: String,
    pub tools: Vec<ToolDef>,
    pub messages: Vec<Message>,
    pub max_tokens: Option<u32>,
    pub session_id: Option<String>,
}

impl StreamCall {
    pub fn of(req: &Request<'_>) -> Self {
        Self {
            model: req.model.clone(),
            thinking: req.thinking.get(),
            system: req.system.into(),
            tools: req.tools.to_vec(),
            messages: req.messages.to_vec(),
            max_tokens: req.max_tokens,
            session_id: req.session_id.map(Into::into),
        }
    }

    pub fn request(&self) -> Request<'_> {
        Request {
            model: &self.model,
            thinking: Rc::new(Cell::new(self.thinking)),
            system: &self.system,
            tools: &self.tools,
            messages: &self.messages,
            max_tokens: self.max_tokens,
            session_id: self.session_id.as_deref(),
        }
    }
}

/// Child to parent: the `call` field of a line.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "call", rename_all = "camelCase")]
pub enum Call {
    Resolve {
        selector: String,
    },
    Stream(Box<StreamCall>),
    #[serde(rename_all = "camelCase")]
    Complete {
        target: String,
        system: String,
        prompt: String,
        max_tokens: u32,
    },
    Search {
        args: Value,
    },
    /// `data` is base64.
    Blob {
        hash: String,
        data: String,
    },
    /// Cancels the running call with the line's own `id`.
    Cancel,
}

impl Call {
    pub fn blob(hash: &str, bytes: &[u8]) -> Self {
        Self::Blob { hash: hash.into(), data: STANDARD.encode(bytes) }
    }

    /// The bytes of a `Blob`'s `data`; `None` when they are not base64.
    pub fn blob_bytes(data: &str) -> Option<Vec<u8>> {
        STANDARD.decode(data).ok()
    }
}

/// Parent to child: the answer to the call with the line's `id`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Reply {
    Event(Event),
    Done(Box<Done>),
    Error(RelayError),
}

/// The `done` of any call; the fields set depend on the call.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Done {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spec: Option<ModelSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<Thinking>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<Message>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
}

impl Done {
    pub fn resolved(r: Resolved) -> Self {
        Self { spec: Some(r.spec), thinking: r.thinking, ..Default::default() }
    }

    pub fn message(m: Message) -> Self {
        Self { message: Some(m), ..Default::default() }
    }

    pub fn text(t: String) -> Self {
        Self { text: Some(t), ..Default::default() }
    }

    pub fn search(r: SearchReply) -> Self {
        Self { text: Some(r.text), is_error: Some(r.is_error), ..Default::default() }
    }
}

#[derive(Serialize, Deserialize)]
struct Line<T> {
    #[serde(rename = "type")]
    ty: String,
    id: u64,
    #[serde(flatten)]
    body: T,
}

fn encode<T: Serialize>(id: u64, body: &T) -> Value {
    serde_json::to_value(Line { ty: "relay".into(), id, body }).unwrap_or(Value::Null)
}

fn decode<T: for<'de> Deserialize<'de>>(line: &Value) -> Option<(u64, T)> {
    let l: Line<T> = serde_json::from_value(line.clone()).ok()?;
    (l.ty == "relay").then_some((l.id, l.body))
}

impl Call {
    pub fn line(&self, id: u64) -> Value {
        encode(id, self)
    }

    pub fn parse(line: &Value) -> Option<(u64, Self)> {
        decode(line)
    }
}

impl Reply {
    pub fn line(&self, id: u64) -> Value {
        encode(id, self)
    }

    pub fn parse(line: &Value) -> Option<(u64, Self)> {
        decode(line)
    }
}

#[cfg(test)]
mod tests {
    use super::super::client::{Providers, no_gateway};
    use super::*;
    use mu_base::config::Configs;
    use mu_base::message::{Block, Cost, StopReason};
    use mu_base::models::{AnthropicQuirks, Api, ThinkingMode, ThinkingSpec};
    use mu_testutil::block_on;
    use serde_json::json;
    use std::cell::RefCell;

    fn assistant(text: &str) -> Message {
        Message::Assistant {
            content: vec![Block::text(text)],
            api: "anthropic-messages".into(),
            provider: "p".into(),
            model: "m".into(),
            usage: Usage { input: 3, cost: Cost { total: 0.5, ..Default::default() }, ..Default::default() },
            stop_reason: StopReason::Stop,
            timestamp: 1,
            response_id: None,
            error_message: None,
        }
    }

    fn same<T: Serialize>(a: &T, b: &T) {
        assert_eq!(serde_json::to_value(a).unwrap(), serde_json::to_value(b).unwrap());
    }

    #[test]
    fn every_stream_event_round_trips() {
        let usage = Usage { input: 7, output: 2, ..Default::default() };
        let events = [
            StreamEvent::Text("t"),
            StreamEvent::Thinking("th"),
            StreamEvent::ToolCallStart { id: "i", name: "n" },
            StreamEvent::ToolCallDelta { id: "i", json: "{\"a\"" },
            StreamEvent::Retry { attempt: 1, max_attempts: 3, delay: Duration::from_millis(1500), reason: "r".into() },
            StreamEvent::Notice("n".into()),
            StreamEvent::FailedAttemptUsage(usage),
        ];
        for e in &events {
            let owned = Event::from(e);
            let line = Reply::Event(owned.clone()).line(4);
            assert_eq!(line["type"], "relay");
            assert_eq!(line["id"], 4);
            let Some((4, Reply::Event(back))) = Reply::parse(&line) else { panic!("{line}") };
            same(&back, &owned);
            same(&Event::from(&back.to_stream()), &owned);
        }
        let retry = Reply::Event(Event::from(&events[4])).line(1);
        assert_eq!(retry["event"], json!({"type":"retry","attempt":1,"maxAttempts":3,"delayMs":1500,"reason":"r"}));
    }

    #[test]
    fn every_error_kind_round_trips() {
        let errors = [
            ProviderError::Retryable { msg: "slow".into(), retry_after: Some(Duration::from_millis(2500)) },
            ProviderError::Retryable { msg: "slow".into(), retry_after: None },
            ProviderError::Overflow("too long".into()),
            ProviderError::Fatal("no".into()),
            ProviderError::Aborted,
        ];
        for e in &errors {
            let line = Reply::Error(RelayError::from(e)).line(9);
            let Some((9, Reply::Error(back))) = Reply::parse(&line) else { panic!("{line}") };
            assert_eq!(format!("{:?}", ProviderError::from(back)), format!("{e:?}"));
        }
        let line = Reply::Error(RelayError::from(&errors[0])).line(1);
        assert_eq!(line["error"], json!({"kind":"retryable","msg":"slow","retryAfterMs":2500}));
        let no_model = RelayError::new(ErrorKind::NoModel, "smol");
        let line = Reply::Error(no_model.clone()).line(1);
        let Some((_, Reply::Error(back))) = Reply::parse(&line) else { panic!() };
        assert_eq!(back, no_model);
        assert!(back.into_anyhow().is::<NoModel>());
        assert!(RelayError::of_anyhow(&NoModel::new("smol".into()).into()).kind == ErrorKind::NoModel);
    }

    #[test]
    fn calls_and_done_round_trip_and_the_spec_keeps_its_quirks() {
        let mut spec = ModelSpec::gateway("anthropic/x", 200_000, 32_000);
        spec.api = Api::AnthropicMessages;
        spec.thinking = ThinkingSpec { mode: ThinkingMode::BudgetEffort, efforts: vec!["low".into()] };
        spec.anthropic = AnthropicQuirks { supports_display: Some(true), ..Default::default() };
        let back: ModelSpec = serde_json::from_value(serde_json::to_value(&spec).unwrap()).unwrap();
        assert_eq!(back.anthropic, spec.anthropic);
        assert_eq!(back.anthropic.supports_display, Some(true));
        assert_eq!((back.api, back.thinking), (spec.api, spec.thinking.clone()));

        let msgs = [Message::user("hi"), assistant("yo")];
        let tools = [ToolDef { name: "t".into(), description: "d".into(), schema: json!({"type":"object"}) }];
        let req = Request {
            model: &spec,
            thinking: Rc::new(Cell::new(Thinking::XHigh)),
            system: "sys",
            tools: &tools,
            messages: &msgs,
            max_tokens: Some(99),
            session_id: Some("s1"),
        };
        let line = Call::Stream(Box::new(StreamCall::of(&req))).line(2);
        assert_eq!((line["type"].as_str(), line["call"].as_str()), (Some("relay"), Some("stream")));
        assert_eq!(line["thinking"], "xhigh");
        assert_eq!(line["maxTokens"], 99);
        let Some((2, Call::Stream(s))) = Call::parse(&line) else { panic!("{line}") };
        let r = s.request();
        assert_eq!(
            (r.system, r.max_tokens, r.session_id, r.thinking.get()),
            ("sys", Some(99), Some("s1"), Thinking::XHigh)
        );
        same(&r.messages, &msgs.as_slice());

        let line =
            Call::Complete { target: "smol".into(), system: "s".into(), prompt: "p".into(), max_tokens: 5 }.line(3);
        assert_eq!(line["maxTokens"], 5);
        assert!(matches!(Call::parse(&line), Some((3, Call::Complete { max_tokens: 5, .. }))));
        let line = Call::blob("ab", b"bytes").line(4);
        let Some((4, Call::Blob { hash, data })) = Call::parse(&line) else { panic!() };
        assert_eq!((hash.as_str(), Call::blob_bytes(&data).as_deref()), ("ab", Some(&b"bytes"[..])));
        assert!(matches!(Call::parse(&Call::Cancel.line(5)), Some((5, Call::Cancel))));
        assert!(Call::parse(&json!({"type":"other","id":1,"call":"cancel"})).is_none());

        let line = Reply::Done(Done::search(SearchReply { text: "x".into(), is_error: true }).into()).line(6);
        assert_eq!(line["done"], json!({"text":"x","isError":true}));
        let line = Reply::Done(Done::message(assistant("yo")).into()).line(7);
        let Some((7, Reply::Done(d))) = Reply::parse(&line) else { panic!() };
        same(&d.message.unwrap(), &assistant("yo"));
    }

    #[derive(Default)]
    struct Fake {
        seen: RefCell<Vec<String>>,
    }

    impl Relay for Fake {
        fn resolve<'a>(&'a self, selector: &'a str) -> LocalFuture<'a, Result<Resolved, RelayError>> {
            self.seen.borrow_mut().push(format!("resolve {selector}"));
            Box::pin(async move {
                if selector == "bad" {
                    return Err(RelayError::new(ErrorKind::Fatal, "unknown model"));
                }
                let spec = ModelSpec::gateway("p/m", 1000, 100);
                Ok(Resolved { spec, thinking: Some(Thinking::Low) })
            })
        }

        fn stream<'a>(
            &'a self,
            req: &'a Request<'a>,
            _: &'a CancellationToken,
            ev: &'a mut dyn FnMut(StreamEvent),
        ) -> LocalFuture<'a, Result<Message, ProviderError>> {
            self.seen.borrow_mut().push(format!("stream {}", req.system));
            Box::pin(async move {
                match req.system {
                    "overflow" => Err(ProviderError::Overflow("too long".into())),
                    "retry" => Err(ProviderError::Retryable { msg: "slow".into(), retry_after: None }),
                    _ => {
                        ev(StreamEvent::Text("hel"));
                        Ok(assistant("hello"))
                    }
                }
            })
        }

        fn complete<'a>(
            &'a self,
            target: &'a str,
            _: &'a str,
            prompt: &'a str,
            _: u32,
            _: &'a CancellationToken,
        ) -> LocalFuture<'a, Result<String, RelayError>> {
            self.seen.borrow_mut().push(format!("complete {target}"));
            Box::pin(async move {
                match target {
                    "ghost" => Err(RelayError::new(ErrorKind::NoModel, "ghost")),
                    _ => Ok(format!("re: {prompt}")),
                }
            })
        }

        fn search<'a>(
            &'a self,
            _: Value,
            _: &'a CancellationToken,
        ) -> LocalFuture<'a, Result<SearchReply, RelayError>> {
            Box::pin(async { Ok(SearchReply { text: "r".into(), is_error: false }) })
        }

        fn blob(&self, hash: &str, _: &[u8]) {
            self.seen.borrow_mut().push(format!("blob {hash}"));
        }
    }

    fn providers() -> Providers {
        Providers::for_tests(Rc::new(Configs::default()), no_gateway())
    }

    fn request<'a>(spec: &'a ModelSpec, system: &'a str) -> Request<'a> {
        Request {
            model: spec,
            thinking: Rc::new(Cell::new(Thinking::Off)),
            system,
            tools: &[],
            messages: &[],
            max_tokens: None,
            session_id: None,
        }
    }

    #[test]
    fn a_relay_serves_stream_complete_and_resolve() {
        let p = providers();
        let fake = Rc::new(Fake::default());
        let spec = ModelSpec::gateway("p/m", 1000, 100);
        let cancel = CancellationToken::new();
        block_on(async {
            // Without a relay a scripted reply is used, as ever.
            p.scripted.borrow_mut().push_back(Ok(assistant("scripted")));
            let m = p.stream(&request(&spec, "x"), &cancel, &mut |_| {}).await.unwrap();
            same(&m, &assistant("scripted"));

            // A relayed process cannot resolve locally.
            assert!(!p.is_relayed());
            p.set_relay(fake.clone());
            assert!(p.is_relayed());
            let e = p.resolve("anthropic/x").unwrap_err().to_string();
            assert_eq!(e, "model anthropic/x is not resolved in a relayed worker");
            let (got, level) = p.resolve_relayed("@fast").await.unwrap();
            assert_eq!((got.id.as_str(), level), ("p/m", Some(Thinking::Low)));
            assert_eq!(p.resolve("@fast").unwrap().0.id, "p/m");
            assert_eq!(p.resolve("p/m").unwrap().0.id, "p/m");
            assert_eq!(p.resolve_relayed("bad").await.unwrap_err().to_string(), "unknown model");
            assert!(p.resolve("bad").is_err());

            let mut texts = Vec::new();
            let mut ev = |e: StreamEvent| {
                if let StreamEvent::Text(t) = e {
                    texts.push(t.to_string());
                }
            };
            let m = p.stream(&request(&spec, "x"), &cancel, &mut ev).await.unwrap();
            same(&m, &assistant("hello"));
            assert_eq!(texts, ["hel"]);
            assert!(matches!(
                p.stream(&request(&spec, "overflow"), &cancel, &mut |_| {}).await,
                Err(ProviderError::Overflow(m)) if m == "too long"
            ));
            assert!(matches!(
                p.stream(&request(&spec, "retry"), &cancel, &mut |_| {}).await,
                Err(ProviderError::Retryable { .. })
            ));

            // The whole role goes over, not one call per model of its chain.
            assert_eq!(p.complete("@smol", "s", "ping", 10, &cancel).await.unwrap(), "re: ping");
            assert!(p.complete("ghost", "s", "ping", 10, &cancel).await.unwrap_err().is::<NoModel>());
        });
        let seen = fake.seen.borrow();
        assert_eq!(seen.iter().filter(|s| s.starts_with("complete")).count(), 2);
        assert_eq!(seen.iter().filter(|s| s.starts_with("stream")).count(), 3);
    }

    #[test]
    fn a_relay_turns_the_model_lists_off() {
        let p = providers();
        p.set_relay(Rc::new(Fake::default()));
        block_on(async {
            assert_eq!(p.refresh_direct_models(true).await, 0);
            assert!(p.gateway_models().await.is_ok());
        });
        assert!(!p.gw_fresh.get());
    }
}
