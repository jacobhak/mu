//! Custom endpoints end to end through `Providers`: resolution, routing, authentication, replay and
//! what happens when the definition changes. Servers are local fakes; no gateway ever starts.

use super::client::Providers;
use super::custom::{CustomProvider, Registry};
use super::endpoint::{GatewaySource, start_fn};
use super::types::{ProviderError, Request, StreamEvent};
use crate::auth::store::{Entry, Store};
use mu_base::config::Configs;
use mu_base::message::{Block, Message};
use mu_base::models::{Thinking, ThinkingMode};
use mu_base::tool::ToolDef;
use mu_testutil::{Reply, block_on, fake_server};
use serde_json::{Value, json};
use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio_util::sync::CancellationToken;

const KEY: &str = "custom-secret-0123456789";

const TOOL_TURN: &str = r#"data: {"id":"c1","choices":[{"index":0,"delta":{"role":"assistant","reasoning_content":"think "},"finish_reason":null}]}

data: {"id":"c1","choices":[{"index":0,"delta":{"reasoning_content":"hard"},"finish_reason":null}]}

data: {"id":"c1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"read","arguments":"{\"pa"}}]},"finish_reason":null}]}

data: {"id":"c1","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"th\":\"a.rs\"}"}}]},"finish_reason":null}]}

data: {"id":"c1","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}

data: {"id":"c1","choices":[],"usage":{"prompt_tokens":12,"completion_tokens":5,"total_tokens":17}}

data: [DONE]

"#;

const TEXT_TURN: &str = r#"data: {"id":"c2","choices":[{"index":0,"delta":{"role":"assistant","content":"pong"},"finish_reason":null}]}

data: {"id":"c2","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}

data: {"id":"c2","choices":[],"usage":{"prompt_tokens":3,"completion_tokens":1,"total_tokens":4}}

data: [DONE]

"#;

fn sse(body: &str) -> Reply {
    Reply::new(200, body).header("content-type", "text/event-stream")
}

fn definition(base: &str, auth: &str) -> String {
    format!(
        r#"[providers.vllm]
label = "vLLM"
base_url = "{base}/v1"
auth = "{auth}"
discover = false

[providers.vllm.models."org/model"]
context_window = 8192
max_tokens = 1024
reasoning = "template"

[providers.vllm.models."org/model".compat]
reasoningContentField = "reasoning_content"
requiresReasoningContentForToolCalls = true

[providers.vllm.models."org/model".extra_body_on.chat_template_kwargs]
enable_thinking = true
[providers.vllm.models."org/model".extra_body_off.chat_template_kwargs]
enable_thinking = false

[providers.vllm.models."org/effort"]
reasoning = "effort"
efforts = ["low", "high"]
"#
    )
}

struct Setup {
    p: Providers,
    dir: PathBuf,
    gateway_starts: Arc<AtomicUsize>,
}

impl Setup {
    fn new(base: &str, auth: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("mu-custom-e2e-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("providers.toml"), definition(base, auth)).unwrap();
        let starts = Arc::new(AtomicUsize::new(0));
        let p = Self::providers(&dir, &starts);
        Self { p, dir, gateway_starts: starts }
    }

    fn providers(dir: &std::path::Path, starts: &Arc<AtomicUsize>) -> Providers {
        Self::providers_with(dir, starts, Configs::default())
    }

    fn providers_with(dir: &std::path::Path, starts: &Arc<AtomicUsize>, cfg: Configs) -> Providers {
        let counter = starts.clone();
        let gateway = GatewaySource::new(start_fn(move || {
            counter.fetch_add(1, Ordering::SeqCst);
            async { Err::<(String, String), String>("the gateway must not start".into()) }
        }));
        Providers::with_store(Rc::new(cfg), gateway, Store::at(dir.join("auth.json")))
    }

    fn key(&self) {
        self.p.store().set("vllm", Entry { chat: Some(true), ..Entry::literal(KEY) }).unwrap();
    }

    fn gateway_started(&self) -> bool {
        self.gateway_starts.load(Ordering::SeqCst) > 0
    }
}

impl Drop for Setup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn tools() -> Vec<ToolDef> {
    vec![ToolDef { name: "read".into(), description: "Read".into(), schema: json!({"type":"object"}) }]
}

async fn run(
    p: &Providers,
    sel: &str,
    level: Thinking,
    msgs: &[Message],
    events: &mut Vec<String>,
) -> Result<Message, ProviderError> {
    let (spec, _) = p.resolve(sel).unwrap();
    let tools = tools();
    let req = Request {
        model: &spec,
        thinking: Rc::new(Cell::new(level)),
        system: "be brief",
        tools: &tools,
        messages: msgs,
        max_tokens: Some(4096),
        session_id: Some("s1"),
    };
    let mut ev = |e: StreamEvent| {
        events.push(match e {
            StreamEvent::Text(t) => format!("text:{t}"),
            StreamEvent::Thinking(t) => format!("thinking:{t}"),
            StreamEvent::ToolCallStart { name, .. } => format!("call:{name}"),
            StreamEvent::ToolCallDelta { json, .. } => format!("args:{json}"),
            _ => "other".into(),
        })
    };
    p.stream(&req, &CancellationToken::new(), &mut ev).await
}

fn head(raw: &str) -> String {
    raw.split("\r\n\r\n").next().unwrap().to_lowercase()
}

fn body(raw: &str) -> Value {
    serde_json::from_str(raw.split("\r\n\r\n").nth(1).unwrap()).unwrap()
}

#[test]
fn a_bearer_endpoint_is_called_at_its_own_v1_path_with_the_key_and_never_the_gateway() {
    block_on(async {
        let (base, seen) = fake_server(vec![sse(TEXT_TURN)]).await;
        let s = Setup::new(&base, "bearer");
        s.key();
        let (spec, _) = s.p.resolve("vllm/org/model").unwrap();
        assert_eq!(
            (spec.native, spec.wire_id.as_str(), spec.context_window, spec.max_output),
            (true, "org/model", 8192, 1024)
        );
        assert_eq!(spec.thinking.mode, ThinkingMode::Template);
        let mut events = Vec::new();
        let m = run(&s.p, "vllm/org/model", Thinking::Medium, &[Message::user("ping")], &mut events).await.unwrap();
        let Message::Assistant { provider, model, api, .. } = &m else { panic!() };
        assert_eq!((provider.as_str(), model.as_str(), api.as_str()), ("vllm", "vllm/org/model", "openai-completions"));
        assert_eq!(events, ["text:pong"]);
        let raw = seen.lock().unwrap()[0].clone();
        let h = head(&raw);
        assert!(h.starts_with("post /v1/chat/completions "), "{h}");
        assert!(h.contains(&format!("authorization: bearer {KEY}")), "{h}");
        let b = body(&raw);
        assert_eq!(b["model"], "org/model");
        assert_eq!(b["max_tokens"], 1024, "capped by the model's configured output limit");
        assert_eq!(b["tools"].as_array().unwrap().len(), 1);
        assert_eq!(b["chat_template_kwargs"]["enable_thinking"], true);
        assert!(b.get("reasoning_effort").is_none());
        assert!(!s.gateway_started());
    });
}

#[test]
fn a_keyless_endpoint_sends_no_authorization() {
    block_on(async {
        let (base, seen) = fake_server(vec![sse(TEXT_TURN)]).await;
        let s = Setup::new(&base, "none");
        run(&s.p, "vllm/org/model", Thinking::Off, &[Message::user("ping")], &mut Vec::new()).await.unwrap();
        let raw = seen.lock().unwrap()[0].clone();
        let h = head(&raw);
        assert!(h.starts_with("post /v1/chat/completions "), "{h}");
        assert!(!h.contains("authorization") && !h.contains("x-api-key"), "{h}");
        assert_eq!(body(&raw)["chat_template_kwargs"]["enable_thinking"], false);
        assert!(!s.gateway_started());
    });
}

#[test]
fn a_missing_unresolvable_or_disabled_key_fails_clearly_without_the_gateway() {
    block_on(async {
        let (base, seen) = fake_server(vec![sse(TEXT_TURN)]).await;
        let s = Setup::new(&base, "bearer");
        let msgs = [Message::user("ping")];
        let fail = |e: ProviderError| e.to_string();
        let e = fail(run(&s.p, "vllm/org/model", Thinking::Off, &msgs, &mut Vec::new()).await.unwrap_err());
        assert!(e.contains("no API key for custom provider `vllm`") && e.contains("/login vllm"), "{e}");

        s.p.store().set("vllm", Entry::reference("$MU_TEST_CUSTOM_KEY_NEVER_SET").unwrap()).unwrap();
        let e = fail(run(&s.p, "vllm/org/model", Thinking::Off, &msgs, &mut Vec::new()).await.unwrap_err());
        assert!(e.contains("MU_TEST_CUSTOM_KEY_NEVER_SET") && e.contains("not set"), "{e}");

        s.p.store().set("vllm", Entry::reference("!exit 3").unwrap()).unwrap();
        let e = fail(run(&s.p, "vllm/org/model", Thinking::Off, &msgs, &mut Vec::new()).await.unwrap_err());
        assert!(e.contains("failed"), "{e}");

        s.p.store().set("vllm", Entry { chat: Some(false), ..Entry::literal(KEY) }).unwrap();
        let e = fail(run(&s.p, "vllm/org/model", Thinking::Off, &msgs, &mut Vec::new()).await.unwrap_err());
        assert!(e.contains("not enabled for chat"), "{e}");

        s.p.store()
            .set("vllm", Entry { base_url: Some("https://elsewhere.example/v1".into()), ..Entry::literal(KEY) })
            .unwrap();
        let e = fail(run(&s.p, "vllm/org/model", Thinking::Off, &msgs, &mut Vec::new()).await.unwrap_err());
        assert!(e.contains("different base URL"), "{e}");

        assert!(seen.lock().unwrap().is_empty(), "nothing was sent");
        assert!(!s.gateway_started());
    });
}

#[test]
fn a_redirect_is_not_followed_and_the_key_goes_nowhere_else() {
    block_on(async {
        let (other, other_seen) = fake_server(vec![sse(TEXT_TURN)]).await;
        let (base, seen) =
            fake_server(vec![Reply::new(307, "").header("location", &format!("{other}/v1/chat/completions"))]).await;
        let s = Setup::new(&base, "bearer");
        s.key();
        let r = run(&s.p, "vllm/org/model", Thinking::Off, &[Message::user("ping")], &mut Vec::new()).await;
        assert!(r.is_err());
        assert!(!seen.lock().unwrap().is_empty());
        assert!(other_seen.lock().unwrap().is_empty(), "the redirect target saw nothing");
    });
}

#[test]
fn effort_models_clamp_and_internal_completions_use_the_off_template() {
    block_on(async {
        let (base, seen) = fake_server(vec![sse(TEXT_TURN)]).await;
        let s = Setup::new(&base, "none");
        run(&s.p, "vllm/org/effort", Thinking::Max, &[Message::user("hi")], &mut Vec::new()).await.unwrap();
        assert_eq!(body(&seen.lock().unwrap()[0])["reasoning_effort"], "high");
        // Role-fallback completions ask for thinking off.
        let out = s.p.complete("vllm/org/model", "sys", "title this", 64, &CancellationToken::new()).await.unwrap();
        assert_eq!(out, "pong");
        let b = body(&seen.lock().unwrap()[1]);
        assert_eq!(b["chat_template_kwargs"]["enable_thinking"], false);
        assert!(b.get("tools").is_none());
        assert!(!s.gateway_started());
    });
}

#[test]
fn a_tool_turn_replays_reasoning_as_configured_and_survives_a_restart() {
    block_on(async {
        let (base, seen) = fake_server(vec![sse(TOOL_TURN), sse(TEXT_TURN), sse(TEXT_TURN), sse(TEXT_TURN)]).await;
        let s = Setup::new(&base, "none");
        let mut events = Vec::new();
        let first =
            run(&s.p, "vllm/org/model", Thinking::High, &[Message::user("read a.rs")], &mut events).await.unwrap();
        assert_eq!(events, ["thinking:think ", "thinking:hard", "call:read", "args:{\"pa", "args:th\":\"a.rs\"}"]);
        let Message::Assistant { content, .. } = &first else { panic!() };
        assert!(content.iter().any(
            |b| matches!(b, Block::ToolCall { id, arguments, .. } if id == "call_1" && arguments["path"] == "a.rs")
        ));
        let result = Message::ToolResult {
            tool_call_id: "call_1".into(),
            tool_name: "read".into(),
            content: vec![Block::text("fn main() {}")],
            is_error: false,
            timestamp: 0,
            details: None,
        };
        let history = [Message::user("read a.rs"), first.clone(), result];
        run(&s.p, "vllm/org/model", Thinking::High, &history, &mut Vec::new()).await.unwrap();
        let second = body(&seen.lock().unwrap()[1]);
        let msgs = second["messages"].as_array().unwrap();
        let asst = msgs.iter().find(|m| m["role"] == "assistant").unwrap();
        assert_eq!(asst["reasoning_content"], "think hard", "{asst}");
        assert_eq!(asst["tool_calls"][0]["id"], "call_1");
        let tool = msgs.iter().find(|m| m["role"] == "tool").unwrap();
        assert_eq!(tool["tool_call_id"], "call_1");

        // A fresh Providers over the same files replays the same request.
        let again = Setup::providers(&s.dir, &s.gateway_starts);
        run(&again, "vllm/org/model", Thinking::High, &history, &mut Vec::new()).await.unwrap();
        assert_eq!(body(&seen.lock().unwrap()[2])["messages"], second["messages"]);

        // Another model on the same endpoint gets the reasoning demoted to text, not replayed natively.
        run(&again, "vllm/org/effort", Thinking::High, &history, &mut Vec::new()).await.unwrap();
        let other = body(&seen.lock().unwrap()[3]);
        let asst = other["messages"].as_array().unwrap().iter().find(|m| m["role"] == "assistant").unwrap().clone();
        assert!(asst.get("reasoning_content").is_none(), "{asst}");
        assert!(asst["content"].as_str().unwrap().contains("<think>"), "{asst}");
    });
}

#[test]
fn a_removed_definition_fails_locally_and_changes_reach_a_live_instance() {
    block_on(async {
        let (base, seen) = fake_server(vec![sse(TEXT_TURN)]).await;
        let s = Setup::new(&base, "none");
        // A definition added after the instance exists is visible at once.
        let reg: &Registry = s.p.custom_registry();
        let mut extra = CustomProvider::new("https://other.example/v1");
        extra.auth = super::custom::AuthMode::None;
        reg.add("later", extra).unwrap();
        assert!(s.p.is_direct("later"));
        assert!(s.p.resolve("later/some/model").unwrap().0.native);
        assert!(s.p.direct_models().unwrap().iter().any(|m| m.id == "vllm/org/model" && m.key.is_some()));

        let (spec, _) = s.p.resolve("vllm/org/model").unwrap();
        assert!(reg.remove("vllm").unwrap());
        let req = Request {
            model: &spec,
            thinking: Rc::new(Cell::new(Thinking::Off)),
            system: "",
            tools: &[],
            messages: &[Message::user("hi")],
            max_tokens: None,
            session_id: None,
        };
        let e = s.p.stream(&req, &CancellationToken::new(), &mut |_| {}).await.unwrap_err().to_string();
        assert!(e.contains("custom provider `vllm` is no longer configured"), "{e}");
        assert!(seen.lock().unwrap().is_empty() && !s.gateway_started());
        assert!(!s.p.is_direct("vllm"));
    });
}

#[test]
fn a_broken_definition_file_blocks_resolution_instead_of_reading_as_empty() {
    block_on(async {
        let s = Setup::new("http://127.0.0.1:9", "none");
        std::fs::write(s.dir.join("providers.toml"), "providers = [").unwrap();
        let e = s.p.resolve("vllm/org/model").unwrap_err().to_string();
        assert!(e.contains("providers.toml") || e.contains("custom-provider configuration"), "{e}");
        assert!(s.p.resolve("anthropic/claude-sonnet-5-5").is_err(), "no route is chosen until the file is fixed");
        assert!(s.p.direct_models().is_err());
        assert!(!s.gateway_started());
    });
}

#[test]
fn listed_limits_sit_between_configuration_and_defaults_and_models_toml_wins() {
    block_on(async {
        mu_testutil::home();
        let (base, _) = fake_server(vec![sse(TEXT_TURN)]).await;
        let s = Setup::new(&base, "none");
        let c = s.p.custom_registry().get("vllm").unwrap().unwrap();
        assert!(crate::model::discovery::custom_commit(
            &c,
            crate::model::discovery::parse_generic(
                r#"{"data":[{"id":"org/listed","max_model_len":50000},{"id":"org/model","context_length":99}]}"#
            )
        ));
        let (listed, _) = s.p.resolve("vllm/org/listed").unwrap();
        assert_eq!((listed.context_window, listed.max_output), (50_000, 4_096));
        let (configured, _) = s.p.resolve("vllm/org/model").unwrap();
        assert_eq!(configured.context_window, 8192, "explicit configuration beats the listing");
        let (unknown, _) = s.p.resolve("vllm/org/unlisted").unwrap();
        assert_eq!((unknown.context_window, unknown.max_output, unknown.images), (32_768, 4_096, false));
        assert_eq!(unknown.thinking.mode, ThinkingMode::None);

        let cfg = Configs::from_models_toml("[models.\"vllm/org/listed\"]\ncontext_window = 1000\nmax_tokens = 2000\n");
        let p = Setup::providers_with(&s.dir, &s.gateway_starts, cfg);
        let e = p.resolve("vllm/org/listed").unwrap_err().to_string();
        assert!(e.contains("output limit"), "{e}");
    });
}
