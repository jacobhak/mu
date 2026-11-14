//! The parent's side of the model relay: serves the `relay` lines a worker writes on its stdout
//! with this process's own `Providers`, and writes the answers to its stdin.

use super::web_gate::HEADLESS;
use crate::permissions::Permissions;
use crate::plugin::host::PluginHost;
use mu_base::message::Message;
use mu_provider::auth::web::{WebStatus, status};
use mu_provider::model::cost::CostLedger;
use mu_provider::provider::client::Providers;
use mu_provider::provider::relay::{
    Call, Done, ErrorKind, Event, RelayError, Reply, Resolved, SearchReply, StreamCall,
};
use mu_provider::provider::types::StreamEvent;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

/// What serves a worker's relay calls. `permissions` is the session's, which learns the hosts a
/// relayed search returned.
#[derive(Clone)]
pub struct RelayServer {
    pub plugins: Rc<PluginHost>,
    pub permissions: Rc<Permissions>,
}

/// The calls one child has running. Dropping it, when the child ends, cancels them all.
pub struct RelayRun {
    server: RelayServer,
    out: UnboundedSender<String>,
    live: Rc<RefCell<HashMap<u64, CancellationToken>>>,
}

impl Drop for RelayRun {
    fn drop(&mut self) {
        for t in self.live.borrow().values() {
            t.cancel();
        }
    }
}

impl RelayServer {
    /// Starts serving one child; each answer line goes to `out`.
    pub fn start(&self, out: UnboundedSender<String>) -> RelayRun {
        RelayRun { server: self.clone(), out, live: Rc::default() }
    }
}

fn aborted() -> RelayError {
    RelayError::new(ErrorKind::Aborted, "aborted")
}

impl RelayRun {
    /// Handles one call line. Every call but `cancel` and `blob` runs as its own task, so a model
    /// call that takes a minute never holds up the child's other lines.
    pub fn handle(&self, id: u64, call: Call) {
        match call {
            Call::Cancel => {
                if let Some(t) = self.live.borrow().get(&id) {
                    t.cancel();
                }
            }
            // Inline, so the blob is stored before the `stream` call that follows it starts.
            Call::Blob { hash, data } => self.server.blob(&hash, &data),
            call => {
                let token = CancellationToken::new();
                self.live.borrow_mut().insert(id, token.clone());
                let (server, out, live) = (self.server.clone(), self.out.clone(), self.live.clone());
                tokio::task::spawn_local(async move {
                    let emit = |e: Event| {
                        let _ = out.send(Reply::Event(e).line(id).to_string());
                    };
                    let reply = match server.serve(call, &token, &emit).await {
                        Ok(d) => Reply::Done(Box::new(d)),
                        Err(e) => Reply::Error(e),
                    };
                    live.borrow_mut().remove(&id);
                    let _ = out.send(reply.line(id).to_string());
                });
            }
        }
    }
}

impl RelayServer {
    async fn serve(&self, call: Call, cancel: &CancellationToken, emit: &dyn Fn(Event)) -> Result<Done, RelayError> {
        let Some(p) = self.plugins.providers() else {
            return Err(RelayError::new(ErrorKind::Fatal, "the parent has no model access"));
        };
        match call {
            Call::Resolve { selector } => match p.resolve_relayed(&selector).await {
                Ok((spec, thinking)) => Ok(Done::resolved(Resolved { spec, thinking })),
                Err(e) => Err(RelayError::of_anyhow(&e)),
            },
            Call::Stream(call) => self.stream(&p, *call, cancel, emit).await,
            Call::Complete { target, system, prompt, max_tokens } => {
                match p.complete(&target, &system, &prompt, max_tokens, cancel).await {
                    Ok(text) => Ok(Done::text(text)),
                    Err(e) => Err(RelayError::of_anyhow(&e)),
                }
            }
            Call::Search { args } => self.search(&p, args, cancel).await,
            Call::Blob { .. } | Call::Cancel => Ok(Done::default()),
        }
    }

    /// The model is resolved here, so the route and credential are never the child's. A relayed
    /// process passes the call up: the top parent resolves it.
    async fn stream(
        &self,
        p: &Providers,
        mut call: StreamCall,
        cancel: &CancellationToken,
        emit: &dyn Fn(Event),
    ) -> Result<Done, RelayError> {
        if !p.is_relayed() {
            call.model = p.resolve(&call.model.id).map_err(|e| RelayError::of_anyhow(&e))?.0;
        }
        let (provider, model) = (call.model.provider.clone(), call.model.id.clone());
        let req = call.request();
        let mut ev = |e: StreamEvent| {
            let mut e = Event::from(&e);
            if let Event::FailedAttemptUsage { usage } = &mut e {
                CostLedger::price(&provider, &model, usage);
            }
            emit(e);
        };
        match p.stream(&req, cancel, &mut ev).await {
            Ok(mut m) => {
                if let Message::Assistant { usage, .. } = &mut m {
                    CostLedger::price(&provider, &model, usage);
                }
                Ok(Done::message(m))
            }
            Err(e) => Err(RelayError::from(&e)),
        }
    }

    /// The gate without the wizard: a search runs only when web search is ready.
    async fn search(
        &self,
        p: &Providers,
        args: serde_json::Value,
        cancel: &CancellationToken,
    ) -> Result<Done, RelayError> {
        let reply = if let Some(relay) = p.relay() {
            relay.search(args, cancel).await?
        } else if !matches!(status(p.store()), WebStatus::Ready(_)) {
            SearchReply { text: HEADLESS.into(), is_error: true }
        } else {
            let out = self.plugins.call_tool("web_search", &args, cancel).await;
            if cancel.is_cancelled() {
                return Err(aborted());
            }
            SearchReply { text: out.text, is_error: out.is_error }
        };
        if !reply.is_error {
            self.permissions.note_search_result(&reply.text);
        }
        Ok(Done::search(reply))
    }

    /// Bytes that do not hash to `hash`, or are not base64, are dropped: a blob has no answer.
    /// A relayed process keeps the blob too: its own relay sends it on from this store.
    fn blob(&self, hash: &str, data: &str) {
        if let Some(bytes) = Call::blob_bytes(data) {
            let _ = mu_base::image::store_blob(hash, &bytes);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permissions::Access;
    use crate::plugin::manifest::{Lifetime, Manifest, ToolSpec};
    use mu_base::config::Configs;
    use mu_base::message::{Block, StopReason, Usage};
    use mu_base::models::ModelSpec;
    use mu_provider::auth::web::{SearchBackend, WebSearch};
    use mu_provider::provider::relay::{LocalFuture, Relay};
    use mu_provider::provider::types::{ProviderError, Request};
    use std::cell::Cell;
    use tokio::sync::mpsc::UnboundedReceiver;

    fn assistant(text: &str, input: u64) -> Message {
        Message::Assistant {
            content: vec![Block::text(text)],
            api: "anthropic-messages".into(),
            provider: "anthropic".into(),
            model: "m".into(),
            usage: Usage { input, ..Default::default() },
            stop_reason: StopReason::Stop,
            timestamp: 1,
            response_id: None,
            error_message: None,
        }
    }

    struct Rig {
        run: RelayRun,
        rx: UnboundedReceiver<String>,
        providers: Rc<Providers>,
        plugins: Rc<PluginHost>,
        permissions: Rc<Permissions>,
    }

    fn rig() -> Rig {
        rig_with(vec![])
    }

    fn rig_with(manifests: Vec<Manifest>) -> Rig {
        mu_testutil::home();
        let dir = std::env::temp_dir().join(format!("mu-relay-{}", uuid::Uuid::new_v4()));
        let (ntx, _nrx) = tokio::sync::mpsc::unbounded_channel();
        let plugins = PluginHost::new(manifests, vec![], ntx, Default::default());
        let providers = Rc::new(Providers::for_tests_with_auth(
            Rc::new(Configs::default()),
            mu_provider::provider::endpoint::GatewaySource::new(plugins.gateway_start()),
            dir.join("auth.json"),
        ));
        plugins.set_providers(&providers);
        let permissions = Rc::new(Permissions::default());
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let run = RelayServer { plugins: plugins.clone(), permissions: permissions.clone() }.start(tx);
        Rig { run, rx, providers, plugins, permissions }
    }

    impl Rig {
        async fn next(&mut self) -> (u64, Reply) {
            let line = self.rx.recv().await.expect("an answer");
            Reply::parse(&serde_json::from_str(&line).unwrap()).expect("a relay line")
        }

        /// The answers up to and including the call's own end.
        async fn until_end(&mut self, id: u64) -> (Vec<Event>, Reply) {
            let mut events = Vec::new();
            loop {
                let (got, r) = self.next().await;
                assert_eq!(got, id);
                match r {
                    Reply::Event(e) => events.push(e),
                    end => return (events, end),
                }
            }
        }

        fn stream_call(&self, system: &str) -> Call {
            let spec = ModelSpec::gateway("anthropic/m", 200_000, 8192);
            let req = Request {
                model: &spec,
                thinking: Rc::new(Cell::new(mu_base::models::Thinking::Off)),
                system,
                tools: &[],
                messages: &[mu_base::message::Message::user("hi")],
                max_tokens: None,
                session_id: None,
            };
            Call::Stream(Box::new(StreamCall::of(&req)))
        }
    }

    fn local<F: std::future::Future>(f: F) -> F::Output {
        mu_testutil::block_on_local(f)
    }

    fn end_error(r: Reply) -> RelayError {
        match r {
            Reply::Error(e) => e,
            _ => panic!("expected an error"),
        }
    }

    #[test]
    fn resolve_answers_with_the_parents_spec() {
        local(async {
            let mut rig = rig();
            rig.run.handle(1, Call::Resolve { selector: "anthropic/m".into() });
            let (_, end) = rig.until_end(1).await;
            let Reply::Done(d) = end else { panic!("{end:?}") };
            assert_eq!(d.spec.unwrap().id, "anthropic/m");
            rig.run.handle(2, Call::Resolve { selector: "".into() });
            let (_, end) = rig.until_end(2).await;
            assert_eq!(end_error(end).kind, ErrorKind::Fatal);
        });
    }

    #[test]
    fn stream_forwards_text_and_prices_usage_before_done() {
        local(async {
            let mut rig = rig();
            rig.providers.scripted.borrow_mut().push_back(Ok(assistant("hello", 10)));
            let call = rig.stream_call("sys");
            rig.run.handle(3, call);
            let (events, end) = rig.until_end(3).await;
            assert!(matches!(&events[0], Event::Text { text } if text == "hello"), "{events:?}");
            let Reply::Done(d) = end else { panic!("{end:?}") };
            let Some(Message::Assistant { content, usage, .. }) = d.message else { panic!() };
            assert!(matches!(&content[0], Block::Text { text } if text == "hello"));
            assert_eq!(usage.input, 10);
        });
    }

    #[test]
    fn stream_usage_is_priced_before_done() {
        local(async {
            let mut rig = rig();
            rig.providers.scripted.borrow_mut().push_back(Ok(assistant("hello", 1_000_000)));
            let Call::Stream(mut s) = rig.stream_call("sys") else { panic!() };
            s.model = ModelSpec::gateway("anthropic/claude-haiku-5-5", 200_000, 8192);
            rig.run.handle(1, Call::Stream(s));
            let (_, end) = rig.until_end(1).await;
            let Reply::Done(d) = end else { panic!("{end:?}") };
            let Some(Message::Assistant { usage, .. }) = d.message else { panic!() };
            assert!(usage.cost.total > 0.0, "{usage:?}");
        });
    }

    #[test]
    fn a_child_process_gets_its_answers_on_stdin() {
        use std::os::unix::fs::PermissionsExt;
        local(async {
            let rig = rig();
            let base = std::env::temp_dir().join(format!("mu-relay-child-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&base).unwrap();
            let exe = base.join("fake-mu");
            let script = r#"#!/bin/sh
IFS= read -r task
printf '%s\n' '{"type":"relay","id":7,"call":"complete","target":"ghost","system":"s","prompt":"p","maxTokens":5}'
IFS= read -r line
escaped=$(printf '%s' "$line" | sed 's/"/\\"/g')
printf '{"type":"result","text":"%s"}\n' "$escaped"
"#;
            std::fs::write(&exe, script).unwrap();
            std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
            let server = RelayServer { plugins: rig.plugins.clone(), permissions: rig.permissions.clone() };
            let out = crate::tools::subagent::run_child_observed(
                crate::tools::subagent::ChildCmd::Local(exe),
                &base,
                &serde_json::json!({"task": "go", "relay": true}),
                &CancellationToken::new(),
                |_, _| {},
                |_| {},
                |_| {},
                None,
                crate::tools::subagent::Mirror::Off,
                Some(&server),
            )
            .await;
            let _ = std::fs::remove_dir_all(&base);
            assert!(out.ok, "{}", out.text);
            assert!(out.text.contains(r#""id":7"#) && out.text.contains("noModel"), "{}", out.text);
        });
    }

    #[test]
    fn every_error_kind_arrives_as_that_kind() {
        local(async {
            let mut rig = rig();
            let errors = [
                (ProviderError::Retryable { msg: "slow".into(), retry_after: None }, ErrorKind::Retryable),
                (ProviderError::Overflow("big".into()), ErrorKind::Overflow),
                (ProviderError::Fatal("no".into()), ErrorKind::Fatal),
                (ProviderError::Aborted, ErrorKind::Aborted),
            ];
            for (i, (e, kind)) in errors.into_iter().enumerate() {
                rig.providers.scripted.borrow_mut().push_back(Err(e));
                let id = 10 + i as u64;
                let call = rig.stream_call("sys");
                rig.run.handle(id, call);
                let (_, end) = rig.until_end(id).await;
                assert_eq!(end_error(end).kind, kind);
            }
        });
    }

    #[test]
    fn a_stream_for_a_model_the_parent_cannot_resolve_fails() {
        local(async {
            let mut rig = rig();
            let Call::Stream(mut s) = rig.stream_call("sys") else { panic!() };
            s.model = ModelSpec::gateway("", 1, 1);
            rig.run.handle(1, Call::Stream(s));
            let (_, end) = rig.until_end(1).await;
            assert_eq!(end_error(end).kind, ErrorKind::Fatal);
        });
    }

    #[test]
    fn complete_answers_and_no_model_keeps_its_kind() {
        local(async {
            let mut rig = rig();
            rig.run.handle(
                1,
                Call::Complete { target: "ghost".into(), system: "s".into(), prompt: "p".into(), max_tokens: 5 },
            );
            let (_, end) = rig.until_end(1).await;
            assert_eq!(end_error(end).kind, ErrorKind::NoModel);
        });
    }

    /// A relay whose `complete` answers after a turn of the scheduler, and whose `stream` waits for
    /// the cancel.
    struct Slow;

    impl Relay for Slow {
        fn resolve<'a>(&'a self, _: &'a str) -> LocalFuture<'a, Result<Resolved, RelayError>> {
            Box::pin(async { Err(RelayError::new(ErrorKind::Fatal, "no")) })
        }

        fn stream<'a>(
            &'a self,
            _: &'a Request<'a>,
            cancel: &'a CancellationToken,
            _: &'a mut dyn FnMut(StreamEvent),
        ) -> LocalFuture<'a, Result<Message, ProviderError>> {
            Box::pin(async move {
                cancel.cancelled().await;
                Err(ProviderError::Aborted)
            })
        }

        fn complete<'a>(
            &'a self,
            _: &'a str,
            _: &'a str,
            prompt: &'a str,
            _: u32,
            _: &'a CancellationToken,
        ) -> LocalFuture<'a, Result<String, RelayError>> {
            Box::pin(async move {
                tokio::task::yield_now().await;
                Ok(format!("re: {prompt}"))
            })
        }

        fn search<'a>(
            &'a self,
            _: serde_json::Value,
            _: &'a CancellationToken,
        ) -> LocalFuture<'a, Result<SearchReply, RelayError>> {
            Box::pin(async {
                Ok(SearchReply { text: "[1] T\n    https://a.example/p\n    s".into(), is_error: false })
            })
        }

        fn blob(&self, _: &str, _: &[u8]) {}
    }

    #[test]
    fn two_concurrent_completes_both_answer() {
        local(async {
            let mut rig = rig();
            rig.providers.set_relay(Rc::new(Slow));
            for (id, p) in [(1, "a"), (2, "b")] {
                rig.run.handle(
                    id,
                    Call::Complete { target: "smol".into(), system: "s".into(), prompt: p.into(), max_tokens: 5 },
                );
            }
            let mut got = Vec::new();
            for _ in 0..2 {
                let (id, r) = rig.next().await;
                let Reply::Done(d) = r else { panic!("{r:?}") };
                got.push((id, d.text.unwrap()));
            }
            got.sort();
            assert_eq!(got, [(1, "re: a".to_string()), (2, "re: b".to_string())]);
        });
    }

    #[test]
    fn a_cancel_cancels_the_call_and_it_ends_aborted() {
        local(async {
            let mut rig = rig();
            rig.providers.set_relay(Rc::new(Slow));
            let call = rig.stream_call("sys");
            rig.run.handle(5, call);
            tokio::task::yield_now().await;
            assert!(rig.rx.try_recv().is_err(), "the call is still running");
            rig.run.handle(5, Call::Cancel);
            let (_, end) = rig.until_end(5).await;
            assert_eq!(end_error(end).kind, ErrorKind::Aborted);
            assert!(rig.run.live.borrow().is_empty());
        });
    }

    #[test]
    fn ending_the_child_cancels_every_call() {
        local(async {
            let rig = rig();
            rig.providers.set_relay(Rc::new(Slow));
            for id in [1, 2] {
                let call = rig.stream_call("sys");
                rig.run.handle(id, call);
            }
            tokio::task::yield_now().await;
            let tokens: Vec<_> = rig.run.live.borrow().values().cloned().collect();
            assert_eq!(tokens.len(), 2);
            let Rig { run, mut rx, .. } = rig;
            drop(run);
            assert!(tokens.iter().all(CancellationToken::is_cancelled));
            let mut kinds = Vec::new();
            for _ in 0..2 {
                let line = rx.recv().await.unwrap();
                kinds.push(end_error(Reply::parse(&serde_json::from_str(&line).unwrap()).unwrap().1).kind);
            }
            assert_eq!(kinds, [ErrorKind::Aborted, ErrorKind::Aborted]);
        });
    }

    #[test]
    fn blob_stores_and_refuses_a_hash_mismatch() {
        local(async {
            let rig = rig();
            let bytes = format!("relay-blob-{}", uuid::Uuid::new_v4()).into_bytes();
            let hash = {
                use sha2::{Digest, Sha256};
                Sha256::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect::<String>()
            };
            let path = mu_common::paths::profile_home().join("blobs").join(&hash);
            rig.run.handle(1, Call::blob("0000", &bytes));
            rig.run.handle(2, Call::blob(&hash, &bytes));
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
            assert!(!mu_common::paths::profile_home().join("blobs").join("0000").exists());
            let wrong = hash.replace(&hash[..2], if &hash[..2] == "ab" { "cd" } else { "ab" });
            rig.run.handle(3, Call::blob(&wrong, &bytes));
            assert!(!mu_common::paths::profile_home().join("blobs").join(wrong).exists());
        });
    }

    #[test]
    fn search_with_web_search_off_answers_the_not_set_up_error() {
        local(async {
            let mut rig = rig();
            rig.run.handle(1, Call::Search { args: serde_json::json!({"query": "x"}) });
            let (_, end) = rig.until_end(1).await;
            let Reply::Done(d) = end else { panic!("{end:?}") };
            assert_eq!((d.is_error, d.text.as_deref()), (Some(true), Some(HEADLESS)));
        });
    }

    #[test]
    fn a_relayed_process_passes_a_search_up_and_notes_its_hosts() {
        local(async {
            let mut rig = rig();
            rig.providers.set_relay(Rc::new(Slow));
            rig.run.handle(1, Call::Search { args: serde_json::json!({}) });
            let (_, end) = rig.until_end(1).await;
            let Reply::Done(d) = end else { panic!("{end:?}") };
            assert_eq!(d.is_error, Some(false));
            assert_eq!(rig.permissions.inherit_json()["searched"], serde_json::json!(["a.example"]));
        });
    }

    fn web_plugin() -> Manifest {
        let tool =
            ToolSpec::inline("web_search".into(), "Search the web.".into(), serde_json::json!({"type": "object"}))
                .with_access(Access::Read);
        Manifest {
            name: "web".into(),
            command: vec!["true".into()],
            lifetime: Lifetime::Session,
            hooks: vec![],
            required_hooks: vec![],
            provider: None,
            tools: vec![tool],
            timeout: std::time::Duration::from_secs(5),
            dir: std::env::temp_dir(),
        }
    }

    #[test]
    fn relay_tools_are_offered_only_while_web_search_is_ready() {
        let rig = rig_with(vec![web_plugin()]);
        assert!(rig.plugins.relay_tools_payload().is_empty(), "not set up");
        rig.providers.store().set_web(Some(WebSearch::keyed(SearchBackend::Off))).unwrap();
        assert!(rig.plugins.relay_tools_payload().is_empty(), "off");
        rig.providers
            .store()
            .set_web(Some(WebSearch::keyless(SearchBackend::Parallel, "2026-01-01T00:00:00Z".into())))
            .unwrap();
        let tools = rig.plugins.relay_tools_payload();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "web_search");
        assert_eq!(tools[0]["access"], "read");
        assert_eq!(tools[0]["parameters"], serde_json::json!({"type": "object"}));
    }

    #[test]
    fn a_worker_given_relay_tools_lists_them_and_one_given_none_does_not() {
        let rig = rig();
        let payload = serde_json::json!([{"name": "web_search", "description": "d", "parameters": {"type": "object"}, "access": "read"}]);
        assert!(rig.plugins.relay_tool_defs().is_empty());
        rig.plugins.set_relay_tools(Some(&payload));
        let defs = rig.plugins.relay_tool_defs();
        assert_eq!(defs.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(), ["web_search"]);
        let tools = crate::tools::Tools::new(std::env::temp_dir(), rig.plugins.clone());
        assert_eq!(tools.access("web_search"), Access::Read);
        // A relayed process passes what it was given on to its own children.
        assert_eq!(rig.plugins.relay_tools_payload(), payload.as_array().unwrap().clone());
        rig.plugins.set_relay_tools(None);
        assert!(rig.plugins.relay_tool_defs().is_empty());
        assert_eq!(tools.access("web_search"), Access::Exec);
    }

    #[test]
    fn a_relayed_tool_call_goes_over_the_relay_as_a_search() {
        local(async {
            let rig = rig();
            rig.providers.set_relay(Rc::new(Slow));
            rig.plugins.set_relay_tools(Some(&serde_json::json!([
                {"name": "web_search", "description": "d", "parameters": {}, "access": "read"}
            ])));
            let tools = crate::tools::Tools::new(std::env::temp_dir(), rig.plugins.clone());
            let (events, _rx) = tokio::sync::mpsc::unbounded_channel();
            let ctx = crate::tools::ToolCtx {
                events,
                cancel: CancellationToken::new(),
                wait_interrupt: None,
                call_id: "c".into(),
                plan: false,
            };
            let out = tools.call("web_search", &serde_json::json!({"query": "x"}), &ctx).await;
            assert!(!out.is_error && out.text.contains("https://a.example/p"), "{}", out.text);
        });
    }
}
