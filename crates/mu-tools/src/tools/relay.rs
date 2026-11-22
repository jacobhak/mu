//! The parent's side of the model relay: serves the `relay` lines a worker writes on its stdout
//! with this process's own `Providers`, and writes the answers to its stdin. A worker can use only
//! the models of its task, up to a spend cap, for as long as its task runs.

use super::web_gate::HEADLESS;
use crate::permissions::{Permissions, Verdict};
use crate::plugin::host::PluginHost;
use mu_base::message::Message;
use mu_base::models::{role_chain_for, split_level};
use mu_base::permissions::{ApprovalDecision, ApprovalRequest};
use mu_provider::auth::web::{WebStatus, status};
use mu_provider::model::cost::CostLedger;
use mu_provider::provider::client::Providers;
use mu_provider::provider::relay::{
    Call, Done, ErrorKind, Event, RelayError, Reply, Resolved, SearchReply, StreamCall,
};
use mu_provider::provider::types::StreamEvent;
use serde::{Deserialize, Serialize};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::{Notify, Semaphore, oneshot};
use tokio_util::sync::CancellationToken;

/// Calls one worker may have running at once; more wait for a free place.
pub const MAX_CONCURRENT: usize = 8;
/// The largest image a worker may send ahead of a call.
pub const MAX_BLOB_BYTES: usize = 20 * 1024 * 1024;

/// The models a worker's calls may name: model ids, and the roles `complete` may take. Only the
/// top parent makes it; a relayed process passes on what it was given, so a grandchild gets no more.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RelayAllow {
    pub ids: Vec<String>,
    pub roles: Vec<String>,
}

impl RelayAllow {
    /// The models of a task that runs on `role` (its fallback chain included) and the `smol` role a
    /// worker titles its session with. `extra` are the model selectors of the task's agent file.
    pub fn for_task(p: &Providers, role: &str, extra: &[String]) -> Self {
        let cfg = &p.cfg;
        let mut roles: Vec<String> = Vec::new();
        let mut selectors: Vec<String> = Vec::new();
        let mut add = |r: &str| {
            if !roles.iter().any(|x| x == r) {
                roles.push(r.to_string());
            }
        };
        role_chain_for(role, cfg).iter().chain(&role_chain_for("smol", cfg)).for_each(|r| add(r));
        for e in extra {
            match e.strip_prefix('@') {
                Some(r) => role_chain_for(&split_level(r).0, cfg).iter().for_each(|r| add(r)),
                None => selectors.push(split_level(e).0),
            }
        }
        selectors.extend(roles.iter().filter(|r| cfg.role(r).is_some()).map(|r| format!("@{r}")));
        let mut ids: Vec<String> = Vec::new();
        for s in selectors {
            if let Ok((spec, _)) = p.resolve(&s)
                && !ids.contains(&spec.id)
            {
                ids.push(spec.id);
            }
        }
        Self { ids, roles }
    }

    fn model(&self, id: &str) -> bool {
        self.ids.iter().any(|i| i == id)
    }

    /// A `complete` target: a model id, or a role (with its `@` and level, if any).
    fn target(&self, target: &str) -> bool {
        if target.contains('/') {
            return self.model(&split_level(target).0);
        }
        let role = split_level(target.trim_start_matches('@')).0;
        self.roles.contains(&role)
    }
}

/// What the relayed workers of one process have spent, by job.
#[derive(Default)]
pub struct RelaySpend {
    jobs: RefCell<HashMap<String, f64>>,
    /// Woken when something is added, for a `--max-cost` check between model turns.
    pub changed: Notify,
}

impl RelaySpend {
    pub fn add(&self, job: &str, usd: f64) {
        if usd > 0.0 {
            *self.jobs.borrow_mut().entry(job.to_string()).or_default() += usd;
            self.changed.notify_one();
        }
    }

    pub fn total(&self) -> f64 {
        self.jobs.borrow().values().sum()
    }

    pub fn of(&self, job: &str) -> f64 {
        self.jobs.borrow().get(job).copied().unwrap_or(0.0)
    }
}

/// What serves a worker's relay calls. `permissions` is the session's, which judges a relayed
/// search.
#[derive(Clone)]
pub struct RelayServer {
    pub plugins: Rc<PluginHost>,
    pub permissions: Rc<Permissions>,
    /// USD one worker may spend; 0 is no cap.
    pub max_cost: f64,
    pub cwd: PathBuf,
}

/// One worker's models and spend: what it may call, its running total against the cap, and the
/// process-wide ledger.
struct Meter {
    allow: RelayAllow,
    job: String,
    spent: Cell<f64>,
    cap: f64,
    capped: Cell<bool>,
    ledger: Rc<RelaySpend>,
}

impl Meter {
    fn guard(&self) -> Result<(), RelayError> {
        if self.cap > 0.0 && self.spent.get() >= self.cap {
            self.capped.set(true);
            return Err(denied("task spend cap reached"));
        }
        Ok(())
    }

    fn add(&self, usd: f64) {
        self.spent.set(self.spent.get() + usd);
        self.ledger.add(&self.job, usd);
    }
}

type Approve<'a> = Option<&'a dyn Fn(ApprovalRequest) -> oneshot::Receiver<ApprovalDecision>>;

/// How a relayed search gets past the parent's permissions.
enum SearchGate {
    Run,
    Refused(String),
    Ask(oneshot::Receiver<ApprovalDecision>),
}

/// The calls one child has running. Dropping it, when the child ends, cancels them all.
pub struct RelayRun {
    server: RelayServer,
    out: UnboundedSender<String>,
    /// Each running call's token, with the number of the run that started it: a worker that asks for
    /// the same id again (its stream was lost and the answer with it) replaces the old run, whose
    /// answers are then dropped.
    live: Rc<RefCell<HashMap<u64, (u64, CancellationToken)>>>,
    runs: Cell<u64>,
    meter: Rc<Meter>,
    places: Rc<Semaphore>,
    finished: Cell<bool>,
}

impl Drop for RelayRun {
    fn drop(&mut self) {
        for (_, t) in self.live.borrow().values() {
            t.cancel();
        }
    }
}

impl RelayServer {
    /// Starts serving one child, which may call the models of `allow`; its spend is recorded under
    /// `job`, and each answer line goes to `out`.
    pub fn start(&self, out: UnboundedSender<String>, job: &str, allow: RelayAllow) -> RelayRun {
        let meter = Meter {
            allow,
            job: job.to_string(),
            spent: Cell::new(0.0),
            cap: self.max_cost,
            capped: Cell::new(false),
            ledger: self.plugins.relay_spend(),
        };
        RelayRun {
            server: self.clone(),
            out,
            live: Rc::default(),
            runs: Cell::new(0),
            meter: Rc::new(meter),
            places: Rc::new(Semaphore::new(MAX_CONCURRENT)),
            finished: Cell::new(false),
        }
    }
}

fn aborted() -> RelayError {
    RelayError::new(ErrorKind::Aborted, "aborted")
}

fn denied(msg: impl Into<String>) -> RelayError {
    RelayError::new(ErrorKind::Denied, msg)
}

impl RelayRun {
    /// `handle_with`, where a search that needs the user's approval is refused.
    pub fn handle(&self, id: u64, call: Call) {
        self.handle_with(id, call, None);
    }

    /// Handles one call line; `approve` asks the user about a search the permissions do not allow.
    /// Every call but `cancel` and `blob` runs as its own task, so a model call that takes a minute
    /// never holds up the child's other lines.
    pub fn handle_with(&self, id: u64, call: Call, approve: Approve<'_>) {
        match call {
            Call::Cancel => {
                if let Some((_, t)) = self.live.borrow().get(&id) {
                    t.cancel();
                }
            }
            // Inline, so the blob is stored before the `stream` call that follows it starts.
            Call::Blob { hash, data } => {
                if !self.finished.get() {
                    self.server.blob(&hash, &data);
                }
            }
            call => {
                if self.finished.get() {
                    let e = denied("the task has already returned its result");
                    let _ = self.out.send(Reply::Error(e).line(id).to_string());
                    return;
                }
                let gate = match &call {
                    Call::Search { args } => Some(self.search_gate(args, approve)),
                    _ => None,
                };
                let token = CancellationToken::new();
                let run = self.runs.get() + 1;
                self.runs.set(run);
                if let Some((_, old)) = self.live.borrow_mut().insert(id, (run, token.clone())) {
                    old.cancel();
                }
                let (server, out, live) = (self.server.clone(), self.out.clone(), self.live.clone());
                let (meter, places) = (self.meter.clone(), self.places.clone());
                tokio::task::spawn_local(async move {
                    let current = || live.borrow().get(&id).is_some_and(|(r, _)| *r == run);
                    let emit = |e: Event| {
                        if current() {
                            let _ = out.send(Reply::Event(e).line(id).to_string());
                        }
                    };
                    let reply = async {
                        let _place = tokio::select! {
                            p = places.acquire() => p.map_err(|_| aborted())?,
                            () = token.cancelled() => return Err(aborted()),
                        };
                        meter.guard()?;
                        if let Some(g) = gate
                            && let Some(refusal) = server.search_verdict(g, &token).await?
                        {
                            return Ok(Done::search(refusal));
                        }
                        server.serve(call, &token, &emit, &meter).await
                    }
                    .await;
                    let reply = match reply {
                        Ok(d) => Reply::Done(Box::new(d)),
                        Err(e) => Reply::Error(e),
                    };
                    if current() {
                        live.borrow_mut().remove(&id);
                        let _ = out.send(reply.line(id).to_string());
                    }
                });
            }
        }
    }

    fn search_gate(&self, args: &serde_json::Value, approve: Approve<'_>) -> SearchGate {
        match self.server.permissions.check_relayed_search(&self.server.cwd, args) {
            Verdict::Allow => SearchGate::Run,
            Verdict::Deny(why) => SearchGate::Refused(why),
            Verdict::Ask(req) => match approve {
                Some(f) => SearchGate::Ask(f(req)),
                None => SearchGate::Refused("this web search needs approval and no one can give it".into()),
            },
        }
    }

    /// The worker has sent its `result`: calls still running are cancelled, later ones refused.
    pub fn finish(&self) {
        self.finished.set(true);
        for (_, t) in self.live.borrow().values() {
            t.cancel();
        }
    }

    /// The cap this worker ran into, if it did.
    pub fn capped(&self) -> Option<f64> {
        self.meter.capped.get().then_some(self.meter.cap)
    }
}

impl RelayServer {
    /// `None` when the search may run; else the refusal, as the search's own error text.
    async fn search_verdict(
        &self,
        gate: SearchGate,
        cancel: &CancellationToken,
    ) -> Result<Option<SearchReply>, RelayError> {
        let refuse = |text: String| Ok(Some(SearchReply { text, is_error: true }));
        match gate {
            SearchGate::Run => Ok(None),
            SearchGate::Refused(text) => refuse(text),
            SearchGate::Ask(rx) => {
                let decision = tokio::select! {
                    d = rx => d.unwrap_or(ApprovalDecision::Deny { note: None }),
                    () = cancel.cancelled() => return Err(aborted()),
                };
                match decision {
                    ApprovalDecision::Allow { .. } => Ok(None),
                    ApprovalDecision::Deny { note } => {
                        refuse(format!("Web search was denied{}", note.map(|n| format!(": {n}")).unwrap_or_default()))
                    }
                    ApprovalDecision::Unavailable => refuse("Web search needs approval and no one can give it.".into()),
                }
            }
        }
    }

    async fn serve(
        &self,
        call: Call,
        cancel: &CancellationToken,
        emit: &dyn Fn(Event),
        meter: &Meter,
    ) -> Result<Done, RelayError> {
        let Some(p) = self.plugins.providers() else {
            return Err(RelayError::new(ErrorKind::Fatal, "the parent has no model access"));
        };
        match call {
            Call::Resolve { selector } => match p.resolve_relayed(&selector).await {
                Ok((spec, thinking)) if meter.allow.model(&spec.id) => Ok(Done::resolved(Resolved { spec, thinking })),
                Ok((spec, _)) => Err(not_allowed(&spec.id)),
                Err(e) => Err(RelayError::of_anyhow(&e)),
            },
            Call::Stream(call) => self.stream(&p, *call, cancel, emit, meter).await,
            Call::Complete { target, system, prompt, max_tokens } => {
                if !meter.allow.target(&target) {
                    return Err(not_allowed(&target));
                }
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
        meter: &Meter,
    ) -> Result<Done, RelayError> {
        if !meter.allow.model(&call.model.id) {
            return Err(not_allowed(&call.model.id));
        }
        if !p.is_relayed() {
            call.model = p.resolve(&call.model.id).map_err(|e| RelayError::of_anyhow(&e))?.0;
        }
        let (provider, model) = (call.model.provider.clone(), call.model.id.clone());
        let req = call.request();
        let mut ev = |e: StreamEvent| {
            let mut e = Event::from(&e);
            if let Event::FailedAttemptUsage { usage } = &mut e {
                CostLedger::price(&provider, &model, usage);
                meter.add(usage.cost.total);
            }
            emit(e);
        };
        match p.stream(&req, cancel, &mut ev).await {
            Ok(mut m) => {
                if let Message::Assistant { usage, .. } = &mut m {
                    CostLedger::price(&provider, &model, usage);
                    meter.add(usage.cost.total);
                }
                Ok(Done::message(m))
            }
            Err(e) => Err(RelayError::from(&e)),
        }
    }

    /// The gate without the wizard: a search runs only when web search is ready. The hosts the
    /// result lists travel back in its text; the worker notes them in its own list, not this one.
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
        Ok(Done::search(reply))
    }

    /// Bytes that do not hash to `hash`, or are not base64, or are over `MAX_BLOB_BYTES`, are dropped:
    /// a blob has no answer. A relayed process keeps the blob too: its own relay sends it on from
    /// this store.
    fn blob(&self, hash: &str, data: &str) {
        if data.len() > MAX_BLOB_BYTES / 3 * 4 + 4 {
            return;
        }
        if let Some(bytes) = Call::blob_bytes(data)
            && bytes.len() <= MAX_BLOB_BYTES
        {
            let _ = mu_base::image::store_blob(hash, &bytes);
        }
    }
}

fn not_allowed(model: &str) -> RelayError {
    denied(format!("model {model} is not one of this task's models"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permissions::Access;
    use crate::plugin::manifest::{Lifetime, Manifest, ToolSpec};
    use mu_base::config::Configs;
    use mu_base::config::MuPermissions;
    use mu_base::message::{Block, StopReason, Usage};
    use mu_base::models::ModelSpec;
    use mu_base::permissions::{Action, ApprovalScope, PermissionMode, Rule, RuleSet};
    use mu_provider::auth::web::{SearchBackend, WebSearch};
    use mu_provider::provider::relay::{LocalFuture, Relay};
    use mu_provider::provider::types::{ProviderError, Request};
    use std::cell::Cell;
    use tokio::sync::mpsc::UnboundedReceiver;

    fn assistant(text: &str, input: u64) -> Message {
        priced(text, input, 0.0)
    }

    fn priced(text: &str, input: u64, usd: f64) -> Message {
        Message::Assistant {
            content: vec![Block::text(text)],
            api: "anthropic-messages".into(),
            provider: "anthropic".into(),
            model: "m".into(),
            usage: Usage {
                input,
                cost: mu_base::message::Cost { total: usd, ..Default::default() },
                ..Default::default()
            },
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
        server: RelayServer,
    }

    const JOB: &str = "job-1";

    fn rig() -> Rig {
        rig_with(vec![])
    }

    fn rig_with(manifests: Vec<Manifest>) -> Rig {
        rig_full(manifests, Permissions::default(), 0.0)
    }

    fn allow() -> RelayAllow {
        RelayAllow {
            ids: vec!["anthropic/m".into(), "anthropic/claude-haiku-5-5".into()],
            roles: vec!["smol".into(), "ghost".into()],
        }
    }

    fn rig_full(manifests: Vec<Manifest>, permissions: Permissions, max_cost: f64) -> Rig {
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
        let permissions = Rc::new(permissions);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let server = RelayServer {
            plugins: plugins.clone(),
            permissions: permissions.clone(),
            max_cost,
            cwd: std::env::temp_dir(),
        };
        let run = server.start(tx, JOB, allow());
        Rig { run, rx, providers, plugins, permissions, server }
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
            let server = rig.server.clone();
            let out = crate::tools::subagent::run_child_observed(
                crate::tools::subagent::ChildCmd::Local(exe),
                &base,
                &serde_json::json!({"task": "go", "relay": true, "relayModels": allow()}),
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
    fn a_model_outside_the_tasks_set_is_denied_on_every_call() {
        local(async {
            let mut rig = rig();
            rig.providers.scripted.borrow_mut().push_back(Ok(assistant("never", 1)));
            for (id, model) in [(1, "anthropic/other"), (2, "ghost/unknown"), (3, "")] {
                let Call::Stream(mut s) = rig.stream_call("sys") else { panic!() };
                s.model = ModelSpec::gateway(model, 1, 1);
                rig.run.handle(id, Call::Stream(s));
                let (_, end) = rig.until_end(id).await;
                let e = end_error(end);
                assert_eq!(e.kind, ErrorKind::Denied, "{model}: {e:?}");
            }
            rig.run.handle(4, Call::Resolve { selector: "anthropic/other".into() });
            let (_, end) = rig.until_end(4).await;
            assert_eq!(end_error(end).kind, ErrorKind::Denied);
            for (id, target) in [(5, "memory"), (6, "anthropic/other"), (7, "@plan:high")] {
                let call =
                    Call::Complete { target: target.into(), system: "s".into(), prompt: "p".into(), max_tokens: 5 };
                rig.run.handle(id, call);
                let (_, end) = rig.until_end(id).await;
                assert_eq!(end_error(end).kind, ErrorKind::Denied, "{target}");
            }
            assert_eq!(rig.providers.scripted.borrow().len(), 1, "no denied call reached the provider");
            // What is in the set still works, a role with its `@` and level included.
            rig.run.handle(8, Call::Resolve { selector: "anthropic/m".into() });
            assert!(matches!(rig.until_end(8).await.1, Reply::Done(_)));
            rig.run.handle(
                9,
                Call::Complete { target: "@smol:low".into(), system: "s".into(), prompt: "p".into(), max_tokens: 5 },
            );
            assert_eq!(end_error(rig.until_end(9).await.1).kind, ErrorKind::NoModel, "allowed, then unset");
        });
    }

    #[test]
    fn the_allowed_set_of_a_task_is_its_role_chain_and_smol() {
        mu_testutil::home();
        let cfgs = Configs::from_models_toml(
            "[sets.main]\ndefault = \"anthropic/d\"\nsmol = \"anthropic/s\"\ntask = \"anthropic/t\"\nplan = \"anthropic/p\"\n",
        );
        let plugins = PluginHost::new(vec![], vec![], tokio::sync::mpsc::unbounded_channel().0, Default::default());
        let p = Providers::for_tests(
            Rc::new(cfgs),
            mu_provider::provider::endpoint::GatewaySource::new(plugins.gateway_start()),
        );
        let a = RelayAllow::for_task(&p, "task", &[]);
        let mut ids = a.ids.clone();
        ids.sort();
        assert_eq!(ids, ["anthropic/d", "anthropic/s", "anthropic/t"], "plan is not in the set");
        assert!(a.roles.contains(&"task".to_string()) && a.roles.contains(&"smol".to_string()));
        let b = RelayAllow::for_task(&p, "task", &["anthropic/x:high".into(), "@plan".into()]);
        assert!(b.ids.contains(&"anthropic/x".to_string()) && b.ids.contains(&"anthropic/p".to_string()), "{b:?}");
    }

    #[test]
    fn a_spend_over_the_cap_ends_the_workers_calls_and_the_ledger_counts_it() {
        local(async {
            let mut rig = rig_full(vec![], Permissions::default(), 1.0);
            rig.providers.scripted.borrow_mut().push_back(Ok(priced("a", 1, 0.75)));
            rig.providers.scripted.borrow_mut().push_back(Ok(priced("b", 1, 0.75)));
            rig.providers.scripted.borrow_mut().push_back(Ok(priced("c", 1, 0.75)));
            for id in [1, 2] {
                let call = rig.stream_call("sys");
                rig.run.handle(id, call);
                assert!(matches!(rig.until_end(id).await.1, Reply::Done(_)));
            }
            assert!(rig.run.capped().is_none());
            let call = rig.stream_call("sys");
            rig.run.handle(3, call);
            let e = end_error(rig.until_end(3).await.1);
            assert_eq!((e.kind, e.msg.as_str()), (ErrorKind::Denied, "task spend cap reached"));
            // Every kind of call, not only streams.
            rig.run.handle(4, Call::Resolve { selector: "anthropic/m".into() });
            assert_eq!(end_error(rig.until_end(4).await.1).kind, ErrorKind::Denied);
            assert_eq!(rig.run.capped(), Some(1.0));
            assert_eq!(rig.providers.scripted.borrow().len(), 1, "the third reply was never asked for");
            let ledger = rig.plugins.relay_spend();
            assert!((ledger.of(JOB) - 1.5).abs() < 1e-9 && (ledger.total() - 1.5).abs() < 1e-9);
        });
    }

    #[test]
    fn no_cap_when_max_cost_is_zero() {
        local(async {
            let mut rig = rig_full(vec![], Permissions::default(), 0.0);
            for id in 1..=3 {
                rig.providers.scripted.borrow_mut().push_back(Ok(priced("a", 1, 10.0)));
                let call = rig.stream_call("sys");
                rig.run.handle(id, call);
                assert!(matches!(rig.until_end(id).await.1, Reply::Done(_)));
            }
            assert!(rig.run.capped().is_none());
            assert!((rig.plugins.relay_spend().total() - 30.0).abs() < 1e-9);
        });
    }

    #[test]
    fn after_the_result_calls_are_cancelled_and_refused() {
        local(async {
            let mut rig = rig();
            rig.providers.set_relay(Rc::new(Slow));
            let call = rig.stream_call("sys");
            rig.run.handle(1, call);
            tokio::task::yield_now().await;
            rig.run.finish();
            let (_, end) = rig.until_end(1).await;
            assert_eq!(end_error(end).kind, ErrorKind::Aborted, "the running call is cancelled");
            let call = rig.stream_call("sys");
            rig.run.handle(2, call);
            let e = end_error(rig.until_end(2).await.1);
            assert_eq!(e.kind, ErrorKind::Denied);
            assert!(e.msg.contains("already returned its result"), "{}", e.msg);
            assert!(rig.run.live.borrow().is_empty());
        });
    }

    /// A relay that counts the streams that started and waits for each one's cancel.
    struct Counting(Rc<Cell<usize>>);

    impl Relay for Counting {
        fn resolve<'a>(&'a self, _: &'a str) -> LocalFuture<'a, Result<Resolved, RelayError>> {
            Slow.resolve("")
        }

        fn stream<'a>(
            &'a self,
            _: &'a Request<'a>,
            cancel: &'a CancellationToken,
            _: &'a mut dyn FnMut(StreamEvent),
        ) -> LocalFuture<'a, Result<Message, ProviderError>> {
            self.0.set(self.0.get() + 1);
            Box::pin(async move {
                cancel.cancelled().await;
                Err(ProviderError::Aborted)
            })
        }

        fn complete<'a>(
            &'a self,
            _: &'a str,
            _: &'a str,
            _: &'a str,
            _: u32,
            _: &'a CancellationToken,
        ) -> LocalFuture<'a, Result<String, RelayError>> {
            Box::pin(async { Ok(String::new()) })
        }

        fn search<'a>(
            &'a self,
            args: serde_json::Value,
            cancel: &'a CancellationToken,
        ) -> LocalFuture<'a, Result<SearchReply, RelayError>> {
            Slow.search(args, cancel)
        }

        fn blob(&self, _: &str, _: &[u8]) {}
    }

    #[test]
    fn at_most_eight_calls_run_at_once_and_the_rest_wait() {
        local(async {
            let mut rig = rig();
            let started = Rc::new(Cell::new(0));
            rig.providers.set_relay(Rc::new(Counting(started.clone())));
            for id in 1..=10 {
                let call = rig.stream_call("sys");
                rig.run.handle(id, call);
            }
            for _ in 0..20 {
                tokio::task::yield_now().await;
            }
            assert_eq!(started.get(), MAX_CONCURRENT);
            rig.run.handle(1, Call::Cancel);
            let (_, end) = rig.until_end(1).await;
            assert_eq!(end_error(end).kind, ErrorKind::Aborted);
            for _ in 0..20 {
                tokio::task::yield_now().await;
            }
            assert_eq!(started.get(), MAX_CONCURRENT + 1, "a freed place starts the next waiting call");
            rig.run.finish();
        });
    }

    #[test]
    fn a_blob_over_the_limit_is_refused() {
        local(async {
            use sha2::{Digest, Sha256};
            let rig = rig();
            let hash_of = |b: &[u8]| Sha256::digest(b).iter().map(|x| format!("{x:02x}")).collect::<String>();
            let blobs = mu_common::paths::profile_home().join("blobs");
            let mut big = format!("relay-big-{}", uuid::Uuid::new_v4()).into_bytes();
            big.resize(MAX_BLOB_BYTES + 1, b'x');
            let big_hash = hash_of(&big);
            rig.run.handle(1, Call::blob(&big_hash, &big));
            assert!(!blobs.join(&big_hash).exists(), "over the limit");
            big.truncate(MAX_BLOB_BYTES);
            let ok_hash = hash_of(&big);
            rig.run.handle(2, Call::blob(&ok_hash, &big));
            assert_eq!(std::fs::metadata(blobs.join(&ok_hash)).unwrap().len(), MAX_BLOB_BYTES as u64);
            std::fs::remove_file(blobs.join(&ok_hash)).ok();
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
    fn a_call_asked_again_replaces_the_old_run_and_only_the_new_one_answers() {
        local(async {
            let mut rig = rig();
            rig.providers.set_relay(Rc::new(Slow));
            // The worker's stream was lost and the answer with it: it sends the same id again.
            rig.run.handle(5, rig.stream_call("sys"));
            tokio::task::yield_now().await;
            rig.run.handle(5, rig.stream_call("sys"));
            tokio::task::yield_now().await;
            assert!(rig.rx.try_recv().is_err(), "the cancelled run says nothing");
            rig.run.handle(5, Call::Cancel);
            let (_, end) = rig.until_end(5).await;
            assert_eq!(end_error(end).kind, ErrorKind::Aborted);
            tokio::task::yield_now().await;
            assert!(rig.rx.try_recv().is_err(), "one answer for the id, not two");
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
            let tokens: Vec<_> = rig.run.live.borrow().values().map(|(_, t)| t.clone()).collect();
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
    fn a_relayed_process_passes_a_search_up_and_the_hosts_stay_with_the_worker() {
        local(async {
            let mut rig = rig();
            rig.providers.set_relay(Rc::new(Slow));
            rig.run.handle(1, Call::Search { args: serde_json::json!({}) });
            let (_, end) = rig.until_end(1).await;
            let Reply::Done(d) = end else { panic!("{end:?}") };
            assert_eq!(d.is_error, Some(false));
            assert!(d.text.unwrap().contains("https://a.example/p"), "the reply carries the hosts to the worker");
            assert_eq!(rig.permissions.inherit_json()["searched"], serde_json::json!([]), "not the parent's list");
        });
    }

    fn ask_mode() -> Permissions {
        Permissions::new(&MuPermissions { mode: PermissionMode::Ask, readonly_bash: true, rules: RuleSet::default() })
    }

    fn search_call() -> Call {
        Call::Search { args: serde_json::json!({"query": "rust news"}) }
    }

    #[test]
    fn in_ask_mode_a_relayed_search_prompts_and_runs_only_when_allowed() {
        local(async {
            let mut rig = rig_full(vec![], ask_mode(), 0.0);
            rig.providers.set_relay(Rc::new(Slow));
            let asked = std::cell::RefCell::new(Vec::new());
            let answer = |d: ApprovalDecision| {
                let asked = &asked;
                move |req: ApprovalRequest| {
                    asked.borrow_mut().push((req.tool.clone(), req.title.clone()));
                    let (tx, rx) = oneshot::channel();
                    let _ = tx.send(d.clone());
                    rx
                }
            };
            let yes = answer(ApprovalDecision::Allow { scope: ApprovalScope::Once, note: None, patterns: None });
            rig.run.handle_with(1, search_call(), Some(&yes));
            let (_, end) = rig.until_end(1).await;
            let Reply::Done(d) = end else { panic!("{end:?}") };
            assert_eq!(d.is_error, Some(false), "{:?}", d.text);
            let no = answer(ApprovalDecision::Deny { note: Some("nope".into()) });
            rig.run.handle_with(2, search_call(), Some(&no));
            let Reply::Done(d) = rig.until_end(2).await.1 else { panic!() };
            assert_eq!((d.is_error, d.text.as_deref()), (Some(true), Some("Web search was denied: nope")));
            rig.run.handle(3, search_call());
            let Reply::Done(d) = rig.until_end(3).await.1 else { panic!() };
            assert_eq!(d.is_error, Some(true), "nobody to ask");
            let want = ("web_search".to_string(), "web_search: rust news".to_string());
            assert_eq!(
                *asked.borrow(),
                [want.clone(), want],
                "asked for the first two, not the one with no one to ask"
            );
            // A host the search returned is not one the parent has searched: its `fetch` still asks.
            assert_eq!(rig.permissions.inherit_json()["searched"], serde_json::json!([]));
            let t = crate::tools::Tools::new(std::env::temp_dir(), rig.plugins.clone());
            let v = rig.permissions.check(&t, "fetch", &serde_json::json!({"url": "https://a.example/p"}));
            assert!(matches!(v, Verdict::Ask(_)));
        });
    }

    #[test]
    fn a_deny_rule_for_web_search_refuses_a_relayed_search() {
        local(async {
            let rule = Rule { pattern: "*".into(), action: Action::Deny };
            let perms = Permissions::new(&MuPermissions {
                mode: PermissionMode::Yolo,
                readonly_bash: true,
                rules: RuleSet(vec![("web_search".into(), rule)]),
            });
            let mut rig = rig_full(vec![], perms, 0.0);
            rig.providers.set_relay(Rc::new(Slow));
            rig.run.handle(1, search_call());
            let Reply::Done(d) = rig.until_end(1).await.1 else { panic!() };
            assert_eq!(d.is_error, Some(true));
            assert!(d.text.unwrap().contains("permission rule"), "a rule decides before the search runs");
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
                mode: None,
            };
            let out = tools.call("web_search", &serde_json::json!({"query": "x"}), &ctx).await;
            assert!(!out.is_error && out.text.contains("https://a.example/p"), "{}", out.text);
        });
    }
}
