pub mod anthropic;
pub mod retry;
pub mod sse;

use crate::message::Message;
use crate::models::{self, GatewayModel, ModelSpec, Thinking};
use serde_json::Value;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub schema: Value,
}

pub struct Request<'a> {
    pub model: &'a ModelSpec,
    pub thinking: Thinking,
    pub system: &'a str,
    pub tools: &'a [ToolDef],
    pub messages: &'a [Message],
    pub max_tokens: Option<u32>,
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

/// Maps an in-stream error event to a retry class by its error type name.
pub fn stream_error(kind: &str, msg: &str) -> ProviderError {
    match kind {
        "overloaded_error" | "rate_limit_error" | "api_error" | "upstream_error" => {
            ProviderError::Retryable { msg: msg.into(), retry_after: None }
        }
        _ => ProviderError::Fatal(msg.into()),
    }
}

/// Replace characters outside `[A-Za-z0-9_-]` with `_`.
pub fn normalize_id(id: &str) -> String {
    id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect()
}

/// Thinking blocks are only replayable to the same api + provider that produced them.
pub fn same_origin(api: &str, provider: &str, model: &ModelSpec) -> bool {
    api == models::API && provider == model.provider
}

use crate::config::Configs;
use crate::message::Block;
use anyhow::{Result, anyhow};
use futures_util::StreamExt;
use std::cell::{OnceCell, RefCell};
use std::rc::Rc;
use tokio_util::sync::CancellationToken;

struct Endpoint {
    base: String,
    token: String,
}

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// No bytes for this long on a response stream: treat the connection as dead and retry.
const STREAM_IDLE: Duration = Duration::from_secs(90);
/// Ceiling for one-shot completions (titles, summaries use their own cancel token as well).
const COMPLETE_TIMEOUT: Duration = Duration::from_secs(60);

pub struct Providers {
    pub cfg: Rc<Configs>,
    client: OnceCell<reqwest::Client>,
    gw_models: RefCell<Option<Vec<GatewayModel>>>,
    /// `gw_models` came from the running gateway, not the on-disk cache.
    gw_fresh: std::cell::Cell<bool>,
    gateway: crate::gateway::GatewaySource,
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
            #[cfg(test)]
            scripted: Default::default(),
        }
    }

    /// One shared client: a 10 s connect timeout, no overall timeout (streams are long-lived;
    /// `STREAM_IDLE` guards them instead).
    fn client(&self) -> &reqwest::Client {
        self.client.get_or_init(|| {
            reqwest::Client::builder()
                .connect_timeout(CONNECT_TIMEOUT)
                .build()
                .unwrap_or_else(|_| reqwest::Client::new())
        })
    }

    pub fn resolve(&self, sel: &str) -> Result<(ModelSpec, Option<Thinking>)> {
        models::resolve(sel, &self.cfg, self.gw_models.borrow().as_deref())
    }

    async fn endpoint(&self) -> Result<Endpoint, ProviderError> {
        let (base, token) = self.gateway.endpoint().await.map_err(|e| ProviderError::Fatal(e.to_string()))?;
        Ok(Endpoint { base, token })
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
            return r;
        }
        if !self.gw_fresh.get() {
            tokio::select! {
                _ = self.gateway_models() => {}
                _ = cancel.cancelled() => return Err(ProviderError::Aborted),
            }
        }
        let mut attempt = 1;
        loop {
            match self.attempt(req, cancel, ev).await {
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
    ) -> Result<Message, ProviderError> {
        let m = req.model;
        let ep = self.endpoint().await?;
        let mut dec = anthropic::Assembler::new(m);
        let rb = self
            .client()
            .post(anthropic::url(&ep.base))
            .bearer_auth(&ep.token)
            .header("content-type", "application/json")
            .header("anthropic-version", "2023-06-01");
        let resp = tokio::select! {
            r = rb.body(anthropic::body(req).to_string()).send() => r.map_err(|e| ProviderError::Retryable {
                msg: format!("cannot reach gateway at {}: {e}", ep.base),
                retry_after: None,
            })?,
            _ = cancel.cancelled() => return Err(ProviderError::Aborted),
        };
        let status = resp.status();
        if !status.is_success() {
            let retry_after = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<u64>().ok())
                .map(Duration::from_secs);
            if let Some(e) = retry::too_long(retry_after) {
                return Err(e);
            }
            let text = resp.text().await.unwrap_or_default();
            return Err(retry::classify_http(status.as_u16(), retry_after, &text));
        }
        let mut bytes = resp.bytes_stream();
        loop {
            let chunk = tokio::select! {
                c = tokio::time::timeout(STREAM_IDLE, bytes.next()) => match c {
                    Ok(c) => c,
                    Err(_) => {
                        return Err(ProviderError::Retryable {
                            msg: format!("no data from the gateway for {}s", STREAM_IDLE.as_secs()),
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
                thinking: Thinking::Off,
                system,
                tools: &[],
                messages: &msgs,
                max_tokens: Some(max_tokens.min(spec.max_output)),
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
