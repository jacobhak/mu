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

/// Time allowed for a one-shot completion: 60 s, or 20 tokens/s of the requested output when that is
/// longer (titles and summaries also take their own cancel token).
fn ceiling(max_tokens: u32) -> Duration {
    Duration::from_secs(60u64.max(u64::from(max_tokens) / 20))
}

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
            models::role_chain_for(role_or_model.trim_start_matches('@'), &self.cfg)
                .into_iter()
                .filter(|r| self.cfg.role(r).is_some())
                .map(|r| format!("@{r}"))
                .collect()
        };
        if sels.is_empty() {
            return Err(anyhow!("no model configured for role {role_or_model}"));
        }
        let limit = ceiling(max_tokens);
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
            let attempt = tokio::time::timeout(limit, self.stream(&req, cancel, &mut sink));
            let attempt = attempt.await.unwrap_or_else(|_| {
                Err(ProviderError::Retryable {
                    msg: format!("timed out after {}s", limit.as_secs()),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ceiling_scales_with_the_requested_output() {
        assert_eq!(ceiling(64), Duration::from_secs(60));
        assert_eq!(ceiling(13_107), Duration::from_secs(655));
    }
}
