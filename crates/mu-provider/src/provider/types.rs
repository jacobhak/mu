//! The provider contract: the request and stream types, errors, how a route authenticates, and
//! the decoder a wire hands its response bytes to.

use super::providers::{AuthStyle, ProviderDef};
use mu_base::message::Message;
use mu_base::models::{ModelSpec, Thinking};
use mu_base::tool::ToolDef;
use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

/// How a request authenticates.
pub enum Auth {
    Bearer(String),
    XApiKey(String),
}

impl Auth {
    pub fn for_def(def: &ProviderDef, key: &str) -> Self {
        match def.auth {
            AuthStyle::XApiKey => Self::XApiKey(key.into()),
            AuthStyle::Bearer => Self::Bearer(key.into()),
        }
    }

    /// The key goes in a header marked sensitive, so it never shows in debug output.
    pub fn apply(&self, rb: reqwest::RequestBuilder) -> Result<reqwest::RequestBuilder, ProviderError> {
        Ok(match self {
            Self::Bearer(t) => rb.bearer_auth(t),
            Self::XApiKey(k) => {
                let mut v = reqwest::header::HeaderValue::from_str(k).map_err(|_| {
                    ProviderError::Fatal("the API key contains characters a header cannot carry".into())
                })?;
                v.set_sensitive(true);
                rb.header("x-api-key", v)
            }
        })
    }
}

pub struct Request<'a> {
    pub model: &'a ModelSpec,
    /// Shared so retries and later requests of a turn pick up a level the user set mid-turn.
    pub thinking: Rc<Cell<Thinking>>,
    pub system: &'a str,
    pub tools: &'a [ToolDef],
    pub messages: &'a [Message],
    pub max_tokens: Option<u32>,
    /// Stable for the whole session; wires use it as a prompt-cache key.
    pub session_id: Option<&'a str>,
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

/// Turns the bytes of one response stream into events and, at the end, the assistant message.
pub trait Decoder {
    fn feed(&mut self, chunk: &[u8], ev: &mut dyn FnMut(StreamEvent)) -> Result<Option<Message>, ProviderError>;
    /// The message when the stream ended without its own end marker, if the content is complete enough.
    fn finish(&mut self) -> Option<Message> {
        None
    }
}
