//! Role-fallback completion: the one-shot text internal calls such as titles and summaries use.

use super::client::Providers;
use super::types::{ProviderError, Request, StreamEvent};
use anyhow::{Result, anyhow};
use mu_base::message::{Block, Message};
use mu_base::models::{self, Thinking};
use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Ceiling for one-shot completions (titles, summaries use their own cancel token as well).
const COMPLETE_TIMEOUT: Duration = Duration::from_secs(60);

impl Providers {
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
