//! The child side of the model relay: a worker that was asked to relay (`"relay": true` in its task
//! payload) sends its model calls to the parent as `relay` lines on stdout and reads the answers
//! from stdin. One reader thread owns stdin and sorts the lines: relay answers go to the call
//! that waits for them, every other line to `run_subagent`.

use anyhow::{Result, bail};
use mu_base::image::{blob_hash, resolve as blob_base64};
use mu_base::message::{Block, Message};
use mu_provider::provider::relay::{
    Call, Done, ErrorKind, Event, LocalFuture, Relay, RelayError, Reply, Resolved, SearchReply, StreamCall,
};
use mu_provider::provider::types::{ProviderError, Request, StreamEvent};
use serde_json::Value;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::io::BufRead;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio_util::sync::CancellationToken;

/// The id of the first call, `resolve`, made before the reader thread exists.
const FIRST_ID: u64 = 0;

fn gone() -> RelayError {
    RelayError::new(ErrorKind::Fatal, "the parent is gone")
}

#[derive(Default)]
struct Waiting {
    calls: HashMap<u64, UnboundedSender<Reply>>,
    closed: bool,
}

/// What the reader thread needs to deliver answers; shared with the relay.
#[derive(Clone, Default)]
pub struct Routes(Arc<Mutex<Waiting>>);

fn lock(w: &Mutex<Waiting>) -> MutexGuard<'_, Waiting> {
    w.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Routes {
    /// Stdin is closed: every waiting call fails, and so does every later one.
    fn close(&self) {
        let mut w = lock(&self.0);
        w.closed = true;
        w.calls.clear();
    }

    /// Hands a relay answer to its call; an answer for a call that is over is dropped.
    fn deliver(&self, id: u64, reply: Reply) {
        if let Some(tx) = lock(&self.0).calls.get(&id) {
            let _ = tx.send(reply);
        }
    }
}

/// Sorts `lines`: relay answers (when `routes` is set) go to their call, the rest to `other`.
/// Closes `routes` when the lines end.
fn pump(lines: impl Iterator<Item = String>, routes: Option<&Routes>, other: &UnboundedSender<String>) {
    for line in lines {
        if let Some(r) = routes
            && let Ok(v) = serde_json::from_str::<Value>(&line)
            && let Some((id, reply)) = Reply::parse(&v)
        {
            r.deliver(id, reply);
            continue;
        }
        // Without a relay nobody else reads stdin: stop when the consumer is gone.
        if other.send(line).is_err() && routes.is_none() {
            break;
        }
    }
    if let Some(r) = routes {
        r.close();
    }
}

/// Starts the one thread that reads stdin; the receiver gets every line that is not a relay answer.
pub fn spawn_reader(routes: Option<Routes>) -> UnboundedReceiver<String> {
    let (tx, rx) = unbounded_channel();
    std::thread::spawn(move || {
        pump(std::io::stdin().lock().lines().map_while(std::result::Result::ok), routes.as_ref(), &tx);
    });
    rx
}

/// The relay's first call, before any runtime or reader thread exists: `resolve` on stdout, then a
/// blocking read of stdin up to its answer.
pub fn resolve_first(selector: &str) -> Result<Resolved> {
    resolve_first_with(selector, &|v| println!("{v}"), &mut std::io::stdin().lock())
}

fn resolve_first_with(selector: &str, out: &dyn Fn(Value), input: &mut dyn BufRead) -> Result<Resolved> {
    out(Call::Resolve { selector: selector.into() }.line(FIRST_ID));
    let mut line = String::new();
    loop {
        line.clear();
        if input.read_line(&mut line)? == 0 {
            bail!("{}", gone().msg);
        }
        let Some((FIRST_ID, reply)) = serde_json::from_str::<Value>(&line).ok().and_then(|v| Reply::parse(&v)) else {
            continue;
        };
        return match reply {
            Reply::Done(d) => match d.spec {
                Some(spec) => Ok(Resolved { spec, thinking: d.thinking }),
                None => bail!("the parent's answer to resolve has no model"),
            },
            Reply::Error(e) => Err(e.into_anyhow()),
            Reply::Event(_) => continue,
        };
    }
}

/// A call in flight; dropping it forgets its answer channel.
struct Pending {
    id: u64,
    rx: UnboundedReceiver<Reply>,
    routes: Routes,
}

impl Drop for Pending {
    fn drop(&mut self) {
        lock(&self.routes.0).calls.remove(&self.id);
    }
}

/// `Relay` over this process's stdout and the `Routes` its reader thread fills.
pub struct StdioRelay {
    out: Box<dyn Fn(Value)>,
    routes: Routes,
    next: Cell<u64>,
    /// Hashes of the images already sent on this stream.
    sent: RefCell<HashSet<String>>,
}

impl StdioRelay {
    pub fn new() -> Self {
        Self::with_out(Box::new(|v| println!("{v}")))
    }

    fn with_out(out: Box<dyn Fn(Value)>) -> Self {
        Self { out, routes: Routes::default(), next: Cell::new(FIRST_ID + 1), sent: RefCell::default() }
    }

    pub fn routes(&self) -> Routes {
        self.routes.clone()
    }

    fn id(&self) -> u64 {
        let id = self.next.get();
        self.next.set(id + 1);
        id
    }

    /// Registers the answer channel, then writes the call: no answer can arrive unregistered.
    fn start(&self, call: Call) -> Result<Pending, RelayError> {
        let id = self.id();
        let (tx, rx) = unbounded_channel();
        {
            let mut w = lock(&self.routes.0);
            if w.closed {
                return Err(gone());
            }
            w.calls.insert(id, tx);
        }
        (self.out)(call.line(id));
        Ok(Pending { id, rx, routes: self.routes.clone() })
    }

    /// Waits for the call's own end. A fired `cancel` sends `cancel` once and keeps waiting: the
    /// parent ends the call, normally with `aborted`.
    async fn finish(
        &self,
        p: &mut Pending,
        cancel: &CancellationToken,
        mut on_event: impl FnMut(&Event),
    ) -> Result<Done, RelayError> {
        let mut cancelled = false;
        loop {
            tokio::select! {
                reply = p.rx.recv() => match reply {
                    Some(Reply::Event(e)) => on_event(&e),
                    Some(Reply::Done(d)) => return Ok(*d),
                    Some(Reply::Error(e)) => return Err(e),
                    None => return Err(gone()),
                },
                () = cancel.cancelled(), if !cancelled => {
                    cancelled = true;
                    (self.out)(Call::Cancel.line(p.id));
                }
            }
        }
    }

    async fn call(&self, call: Call, cancel: &CancellationToken) -> Result<Done, RelayError> {
        let mut p = self.start(call)?;
        self.finish(&mut p, cancel, |_| {}).await
    }

    /// Sends each image the messages reference that this stream has not carried yet.
    fn send_blobs(&self, messages: &[Message]) {
        for m in messages {
            let (Message::User { content, .. }
            | Message::Assistant { content, .. }
            | Message::ToolResult { content, .. }) = m;
            for b in content {
                let Block::Image(img) = b else { continue };
                let Some(hash) = blob_hash(&img.data) else { continue };
                if self.sent.borrow().contains(hash) {
                    continue;
                }
                let Some(data) = blob_base64(&img.data) else { continue };
                self.sent.borrow_mut().insert(hash.to_string());
                (self.out)(Call::Blob { hash: hash.into(), data }.line(self.id()));
            }
        }
    }
}

impl Relay for StdioRelay {
    fn resolve<'a>(&'a self, selector: &'a str) -> LocalFuture<'a, Result<Resolved, RelayError>> {
        Box::pin(async move {
            let done = self.call(Call::Resolve { selector: selector.into() }, &CancellationToken::new()).await?;
            match done.spec {
                Some(spec) => Ok(Resolved { spec, thinking: done.thinking }),
                None => Err(RelayError::new(ErrorKind::Fatal, "the parent's answer to resolve has no model")),
            }
        })
    }

    fn stream<'a>(
        &'a self,
        req: &'a Request<'a>,
        cancel: &'a CancellationToken,
        ev: &'a mut dyn FnMut(StreamEvent),
    ) -> LocalFuture<'a, Result<Message, ProviderError>> {
        Box::pin(async move {
            self.send_blobs(req.messages);
            let mut p = self.start(Call::Stream(Box::new(StreamCall::of(req))))?;
            let done = self.finish(&mut p, cancel, |e| ev(e.to_stream())).await?;
            done.message.ok_or_else(|| ProviderError::Fatal("the parent's answer to stream has no message".into()))
        })
    }

    fn complete<'a>(
        &'a self,
        target: &'a str,
        system: &'a str,
        prompt: &'a str,
        max_tokens: u32,
        cancel: &'a CancellationToken,
    ) -> LocalFuture<'a, Result<String, RelayError>> {
        Box::pin(async move {
            let call =
                Call::Complete { target: target.into(), system: system.into(), prompt: prompt.into(), max_tokens };
            let done = self.call(call, cancel).await?;
            done.text.ok_or_else(|| RelayError::new(ErrorKind::Fatal, "the parent's answer to complete has no text"))
        })
    }

    fn search<'a>(
        &'a self,
        args: Value,
        cancel: &'a CancellationToken,
    ) -> LocalFuture<'a, Result<SearchReply, RelayError>> {
        Box::pin(async move {
            let done = self.call(Call::Search { args }, cancel).await?;
            let text = done
                .text
                .ok_or_else(|| RelayError::new(ErrorKind::Fatal, "the parent's answer to search has no text"))?;
            Ok(SearchReply { text, is_error: done.is_error.unwrap_or(false) })
        })
    }

    fn blob(&self, hash: &str, bytes: &[u8]) {
        (self.out)(Call::blob(hash, bytes).line(self.id()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mu_base::config::Configs;
    use mu_base::message::{Image, StopReason, Usage};
    use mu_base::models::{ModelSpec, Thinking};
    use mu_provider::provider::client::Providers;
    use mu_provider::provider::endpoint::{GatewaySource, start_fn};
    use mu_testutil::block_on;
    use serde_json::json;
    use std::rc::Rc;
    use std::sync::mpsc;

    fn assistant(text: &str) -> Message {
        Message::Assistant {
            content: vec![Block::text(text)],
            api: "anthropic-messages".into(),
            provider: "p".into(),
            model: "m".into(),
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            timestamp: 1,
            response_id: None,
            error_message: None,
        }
    }

    fn user_text(m: &Message) -> String {
        match m {
            Message::User { content, .. } => mu_base::message::text_of(content),
            _ => String::new(),
        }
    }

    /// The fake parent: reads the child's calls, writes lines to the child's stdin.
    struct Parent {
        calls: mpsc::Receiver<Value>,
        answer: mpsc::Sender<String>,
    }

    impl Parent {
        fn say(&self, id: u64, r: Reply) {
            self.raw(r.line(id).to_string());
        }

        fn raw(&self, line: String) {
            let _ = self.answer.send(line);
        }

        fn next_call(&self) -> (u64, Call) {
            loop {
                let v = self.calls.recv_timeout(std::time::Duration::from_secs(10)).expect("a call");
                if let Some(c) = Call::parse(&v) {
                    return c;
                }
            }
        }
    }

    /// The child: its relay, the real `pump` over the parent's lines, and what the pump passes on.
    struct Wire {
        relay: Rc<StdioRelay>,
        others: UnboundedReceiver<String>,
    }

    impl Wire {
        fn new() -> (Self, Parent) {
            let (ctx, calls) = mpsc::channel();
            let relay = Rc::new(StdioRelay::with_out(Box::new(move |v| {
                let _ = ctx.send(v);
            })));
            let (answer, lines) = mpsc::channel::<String>();
            let (otx, others) = unbounded_channel();
            let routes = relay.routes();
            std::thread::spawn(move || pump(lines.into_iter(), Some(&routes), &otx));
            (Self { relay, others }, Parent { calls, answer })
        }

        fn providers(&self) -> Providers {
            let p = Providers::for_tests(
                Rc::new(Configs::default()),
                GatewaySource::new(start_fn(|| async { Err::<(String, String), String>("none".into()) })),
            );
            p.set_relay(self.relay.clone());
            p
        }
    }

    fn request<'a>(spec: &'a ModelSpec, msgs: &'a [Message]) -> Request<'a> {
        Request {
            model: spec,
            thinking: Rc::new(Cell::new(Thinking::Low)),
            system: "sys",
            tools: &[],
            messages: msgs,
            max_tokens: None,
            session_id: None,
        }
    }

    fn spec() -> ModelSpec {
        ModelSpec::gateway("a/b", 1000, 100)
    }

    #[test]
    fn a_stream_yields_the_scripted_events_and_message() {
        let (w, parent) = Wire::new();
        let providers = w.providers();
        let h = std::thread::spawn(move || {
            let (id, call) = parent.next_call();
            let Call::Stream(c) = call else { panic!("not a stream call") };
            assert_eq!(
                (c.system.as_str(), c.thinking, user_text(&c.messages[0]).as_str()),
                ("sys", Thinking::Low, "hi")
            );
            parent.say(id, Reply::Event(Event::Text { text: "he".into() }));
            parent.say(id, Reply::Event(Event::Text { text: "llo".into() }));
            parent.say(id, Reply::Done(Box::new(Done::message(assistant("done")))));
        });
        let spec = spec();
        let msgs = [Message::user("hi")];
        let mut seen = String::new();
        let got = block_on(providers.stream(&request(&spec, &msgs), &CancellationToken::new(), &mut |e| {
            if let StreamEvent::Text(t) = e {
                seen.push_str(t);
            }
        }));
        h.join().expect("parent");
        assert_eq!(seen, "hello");
        let m = got.expect("a message");
        assert_eq!(serde_json::to_value(&m).expect("json")["content"][0]["text"], "done");
    }

    #[test]
    fn an_error_of_each_kind_arrives_as_that_kind() {
        let (w, parent) = Wire::new();
        let providers = w.providers();
        let spec = spec();
        let msgs = [Message::user("hi")];
        let cases = [
            RelayError { kind: ErrorKind::Retryable, msg: "slow".into(), retry_after_ms: Some(1500) },
            RelayError::new(ErrorKind::Overflow, "long"),
            RelayError::new(ErrorKind::Fatal, "no"),
            RelayError::new(ErrorKind::Aborted, "aborted"),
        ];
        let mut parent = parent;
        for e in cases {
            let sent = e.clone();
            let h = std::thread::spawn(move || {
                let (id, _) = parent.next_call();
                parent.say(id, Reply::Error(sent));
                parent
            });
            let got = block_on(providers.stream(&request(&spec, &msgs), &CancellationToken::new(), &mut |_| {}));
            parent = h.join().expect("parent");
            let want = ProviderError::from(e);
            assert_eq!(format!("{:?}", got.expect_err("an error")), format!("{want:?}"));
        }
    }

    #[test]
    fn a_cancel_sends_cancel_and_returns_what_the_parent_ends_the_call_with() {
        let (w, parent) = Wire::new();
        let providers = w.providers();
        let spec = spec();
        let msgs = [Message::user("hi")];
        let cancel = CancellationToken::new();
        let h = std::thread::spawn(move || {
            let (id, _) = parent.next_call();
            let (cid, call) = parent.next_call();
            assert!(matches!(call, Call::Cancel) && cid == id);
            parent.say(id, Reply::Error(RelayError::new(ErrorKind::Aborted, "aborted")));
        });
        let c = cancel.clone();
        let req = request(&spec, &msgs);
        let mut ignore = |_: StreamEvent| {};
        let got = block_on(async {
            let call = providers.stream(&req, &cancel, &mut ignore);
            tokio::join!(call, async move {
                tokio::task::yield_now().await;
                c.cancel();
            })
            .0
        });
        h.join().expect("parent");
        assert!(matches!(got, Err(ProviderError::Aborted)));
    }

    #[test]
    fn closing_stdin_fails_waiting_and_later_calls() {
        let (w, parent) = Wire::new();
        let providers = w.providers();
        let spec = spec();
        let msgs = [Message::user("hi")];
        let h = std::thread::spawn(move || {
            let _ = parent.next_call();
            drop(parent);
        });
        let got = block_on(providers.stream(&request(&spec, &msgs), &CancellationToken::new(), &mut |_| {}));
        h.join().expect("closer");
        assert!(matches!(&got, Err(ProviderError::Fatal(m)) if m == "the parent is gone"), "{got:?}");
        let again = block_on(w.relay.search(json!({}), &CancellationToken::new()));
        assert_eq!(again.expect_err("closed").msg, "the parent is gone");
    }

    #[test]
    fn complete_calls_may_overlap_and_each_gets_its_own_answer() {
        let (w, parent) = Wire::new();
        let cancel = CancellationToken::new();
        let h = std::thread::spawn(move || {
            let first = parent.next_call();
            let second = parent.next_call();
            // Answered in reverse order.
            for (id, call) in [second, first] {
                let Call::Complete { prompt, target, .. } = call else { panic!("not a complete call") };
                assert_eq!(target, "smol");
                parent.say(id, Reply::Done(Box::new(Done::text(format!("re:{prompt}")))));
            }
        });
        let got = block_on(async {
            tokio::join!(
                w.relay.complete("smol", "s", "one", 5, &cancel),
                w.relay.complete("smol", "s", "two", 5, &cancel)
            )
        });
        h.join().expect("parent");
        assert_eq!((got.0.expect("one"), got.1.expect("two")), ("re:one".to_string(), "re:two".to_string()));
    }

    #[test]
    fn search_returns_text_and_the_error_flag() {
        let (w, parent) = Wire::new();
        let cancel = CancellationToken::new();
        let h = std::thread::spawn(move || {
            let (id, call) = parent.next_call();
            assert!(matches!(call, Call::Search { args } if args["q"] == "x"));
            parent.say(id, Reply::Done(Box::new(Done::search(SearchReply { text: "off".into(), is_error: true }))));
        });
        let r = block_on(w.relay.search(json!({"q": "x"}), &cancel)).expect("a reply");
        h.join().expect("parent");
        assert_eq!(r, SearchReply { text: "off".into(), is_error: true });
    }

    #[test]
    fn a_blob_goes_once_per_reference() {
        mu_testutil::home();
        let stored = |bytes: &[u8]| {
            let data = {
                use mu_base::image::externalize;
                // "abc" and "abd" as base64.
                let b64 = if bytes == b"abc" { "YWJj" } else { "YWJk" };
                externalize(Image { data: b64.into(), mime_type: "image/png".into() }).data
            };
            Message::User {
                content: vec![Block::Image(Image { data, mime_type: "image/png".into() })],
                timestamp: 0,
                synthetic: false,
                command: None,
            }
        };
        let (a, b) = (stored(b"abc"), stored(b"abd"));
        let (w, parent) = Wire::new();
        w.relay.send_blobs(&[a.clone(), b.clone()]);
        w.relay.send_blobs(std::slice::from_ref(&a));
        w.relay.send_blobs(&[a, b]);
        let hashes: Vec<String> = parent
            .calls
            .try_iter()
            .filter_map(|v| match Call::parse(&v) {
                Some((_, Call::Blob { hash, data })) => {
                    assert!(Call::blob_bytes(&data).is_some());
                    Some(hash)
                }
                _ => None,
            })
            .collect();
        assert_eq!(hashes.len(), 2);
        assert_ne!(hashes[0], hashes[1]);
    }

    #[test]
    fn the_reader_gives_relay_lines_to_calls_and_the_rest_to_run_subagent() {
        let (mut w, parent) = Wire::new();
        let cancel = CancellationToken::new();
        let approval = json!({"type": "approval", "id": 3, "decision": {}}).to_string();
        let sent = approval.clone();
        let h = std::thread::spawn(move || {
            let (id, _) = parent.next_call();
            parent.raw(sent);
            parent.raw("not json".into());
            parent.say(id + 100, Reply::Done(Box::new(Done::text("stray".into()))));
            parent.say(id, Reply::Done(Box::new(Done::text("mine".into()))));
        });
        assert_eq!(block_on(w.relay.complete("t", "s", "p", 1, &cancel)).expect("text"), "mine");
        h.join().expect("parent");
        block_on(async {
            assert_eq!(w.others.recv().await.as_deref(), Some(approval.as_str()));
            assert_eq!(w.others.recv().await.as_deref(), Some("not json"));
            assert_eq!(w.others.recv().await, None);
        });
    }

    #[test]
    fn the_first_call_resolves_with_a_blocking_read_and_skips_other_lines() {
        let spec = spec();
        let ok = Reply::Done(Box::new(Done::resolved(Resolved { spec: spec.clone(), thinking: Some(Thinking::High) })));
        let input = format!("{{\"type\":\"approval\"}}\n{}\n", ok.line(FIRST_ID));
        let sent = RefCell::new(Vec::new());
        let r = resolve_first_with("@task", &|v| sent.borrow_mut().push(v), &mut input.as_bytes()).expect("resolved");
        assert_eq!((r.spec.id.as_str(), r.thinking), ("a/b", Some(Thinking::High)));
        let sent = sent.into_inner();
        assert_eq!(sent[0]["call"], "resolve");
        assert_eq!(sent[0]["selector"], "@task");

        let err = Reply::Error(RelayError::new(ErrorKind::Fatal, "no such model")).line(FIRST_ID).to_string();
        let e = resolve_first_with("x", &|_| {}, &mut format!("{err}\n").as_bytes()).unwrap_err();
        assert_eq!(e.to_string(), "no such model");
        let e = resolve_first_with("x", &|_| {}, &mut "".as_bytes()).unwrap_err();
        assert_eq!(e.to_string(), "the parent is gone");
    }
}
