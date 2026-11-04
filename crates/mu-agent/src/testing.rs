//! Scripted agents for tests: the agent loop's tests here and the binary's headless tests build on
//! the same builder, so a scripted run behaves the same wherever it is driven from.

use crate::agent::{Agent, AgentInit, DeliverFn, RunConfig};
use crate::plans;
use mu_base::config::Configs;
use mu_base::events::AgentEvent;
use mu_base::message::{Block, Message, StopReason};
use mu_base::models::{ModelSpec, Thinking};
use mu_provider::provider::client::Providers;
use mu_provider::provider::types::ProviderError;
use mu_tools::plugin::host::PluginHost;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::rc::Rc;
use tokio::sync::mpsc::UnboundedReceiver;

pub fn assistant(blocks: Vec<Block>, stop: StopReason) -> Message {
    Message::Assistant {
        content: blocks,
        api: mu_base::models::Api::AnthropicMessages.as_str().into(),
        provider: "anthropic".into(),
        model: "m".into(),
        usage: Default::default(),
        stop_reason: stop,
        timestamp: 0,
        response_id: None,
        error_message: None,
    }
}

pub fn call(id: &str, name: &str) -> Block {
    Block::ToolCall { id: id.into(), name: name.into(), arguments: json!({}) }
}

pub fn yield_call(id: &str, args: serde_json::Value) -> Block {
    Block::ToolCall { id: id.into(), name: "yield".into(), arguments: args }
}

/// An agent whose provider replies with `script` in order, and whose deliver starter is a stub.
pub fn agent(script: Vec<Result<Message, ProviderError>>) -> (Agent, UnboundedReceiver<AgentEvent>) {
    agent_with(script, Configs::default())
}

pub fn agent_with(script: Vec<Result<Message, ProviderError>>, cfg: Configs) -> (Agent, UnboundedReceiver<AgentEvent>) {
    agent_run(script, cfg, RunConfig::default())
}

/// `agent_with` for a run configured the way a front end configures one.
pub fn agent_run(
    script: Vec<Result<Message, ProviderError>>,
    cfg: Configs,
    run: RunConfig,
) -> (Agent, UnboundedReceiver<AgentEvent>) {
    agent_in(script, cfg, run, std::env::temp_dir())
}

/// `agent_run` working in `cwd`.
pub fn agent_in(
    script: Vec<Result<Message, ProviderError>>,
    cfg: Configs,
    run: RunConfig,
    cwd: PathBuf,
) -> (Agent, UnboundedReceiver<AgentEvent>) {
    mu_testutil::home(); // sessions go to a private MU_HOME, not the real ~/.mu
    let (ntx, _nrx) = tokio::sync::mpsc::unbounded_channel();
    let plugins = PluginHost::new(vec![], vec![], ntx, Default::default());
    let providers = Rc::new(Providers::for_tests(
        Rc::new(cfg),
        mu_provider::provider::endpoint::GatewaySource::new(plugins.gateway_start()),
    ));
    providers.scripted.borrow_mut().extend(script);
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let ctx = Rc::new(mu_tools::context::Context {
        warnings: vec![],
        files: vec![],
        nested: vec![],
        rules: vec![],
        skills: vec![],
        commands: vec![],
        agents: vec![],
    });
    let model = ModelSpec::gateway("anthropic/m", 200_000, 8192);
    let deliver: DeliverFn = Rc::new(|_id: String, _session: PathBuf, _cwd: PathBuf| {
        Box::pin(async { Err("mu deliver is not started in tests".to_string()) })
            as Pin<Box<dyn std::future::Future<Output = Result<String, String>>>>
    });
    let a = Agent::new(AgentInit {
        providers,
        plugins,
        cwd,
        ctx,
        model,
        thinking: Thinking::parse("off").unwrap(),
        events: tx,
        deliver,
        run,
    });
    a.enable_panic_tool();
    (a, rx)
}

/// `(is_error, Debug text)` for each tool result in the agent's history.
pub fn results(a: &Agent) -> Vec<(bool, String)> {
    a.messages
        .iter()
        .filter_map(|m| match m {
            Message::ToolResult { is_error, content, .. } => Some((*is_error, format!("{content:?}"))),
            _ => None,
        })
        .collect()
}

pub fn drain(rx: &mut UnboundedReceiver<AgentEvent>) -> Vec<AgentEvent> {
    std::iter::from_fn(|| rx.try_recv().ok()).collect()
}

pub fn tmp_dir() -> PathBuf {
    let d = std::env::temp_dir().join(format!("mu-agent-plans-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&d).unwrap();
    d.canonicalize().unwrap()
}

/// A git repo with one commit holding `a.txt`.
pub fn git_repo() -> PathBuf {
    let d = tmp_dir();
    let sh = |args: &[&str]| {
        let o = std::process::Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .current_dir(&d)
            .output()
            .unwrap();
        assert!(o.status.success(), "{args:?}");
    };
    sh(&["init", "-q"]);
    std::fs::write(d.join("a.txt"), "one\n").unwrap();
    sh(&["add", "."]);
    sh(&["commit", "-qm", "init"]);
    d
}

/// Stores a plan `t` in `cwd`, reading `read` files.
pub async fn save_plan_in(cwd: &Path, read: &[&str]) -> plans::Store {
    let store = plans::store_for(cwd).await;
    let base = match store.root.as_deref() {
        Some(r) => plans::base_now(r).await,
        None => None,
    };
    store
        .save(plans::Save {
            slug: "t",
            title: "T",
            content: "do it\n",
            read: read.iter().map(|s| s.to_string()).collect(),
            session: None,
            base,
            adopt: None,
        })
        .unwrap();
    store
}
