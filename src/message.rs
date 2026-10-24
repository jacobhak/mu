use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Block {
    Text {
        text: String,
    },
    #[serde(rename_all = "camelCase")]
    Thinking {
        thinking: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thinking_signature: Option<String>,
    },
    ToolCall {
        id: String,
        name: String,
        arguments: Value,
    },
    Image(Image),
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Cost {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    pub total: f64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub total_tokens: u64,
    pub cost: Cost,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StopReason {
    #[serde(rename = "stop")]
    Stop,
    #[serde(rename = "length")]
    Length,
    #[serde(rename = "toolUse")]
    ToolUse,
    #[serde(rename = "error")]
    Error,
    #[serde(rename = "aborted")]
    Aborted,
}

fn blocks_or_string<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<Block>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum C {
        S(String),
        B(Vec<Block>),
    }
    Ok(match C::deserialize(d)? {
        C::S(text) => vec![Block::text(text)],
        C::B(b) => b,
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "camelCase")]
pub enum Message {
    #[serde(rename = "user")]
    User {
        #[serde(deserialize_with = "blocks_or_string")]
        content: Vec<Block>,
        timestamp: i64,
    },
    #[serde(rename = "assistant", rename_all = "camelCase")]
    Assistant {
        content: Vec<Block>,
        api: String,
        provider: String,
        model: String,
        usage: Usage,
        stop_reason: StopReason,
        timestamp: i64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        response_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error_message: Option<String>,
    },
    #[serde(rename = "toolResult", rename_all = "camelCase")]
    ToolResult {
        tool_call_id: String,
        tool_name: String,
        content: Vec<Block>,
        is_error: bool,
        timestamp: i64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<Value>,
    },
}

pub fn now_ms() -> i64 {
    jiff::Timestamp::now().as_millisecond()
}

/// Base64 image data with its MIME type; serializes like omp's `ImageContent`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Image {
    pub data: String,
    pub mime_type: String,
}

impl Block {
    pub fn text(text: impl Into<String>) -> Self {
        Block::Text { text: text.into() }
    }
}

impl Message {
    pub fn user(text: &str) -> Self {
        Message::User { content: vec![Block::text(text)], timestamp: now_ms() }
    }

    /// A user message: the text, then one image block per attachment.
    pub fn user_input(i: &crate::events::UserInput) -> Self {
        let content = std::iter::once(Block::text(i.text.as_str())).chain(i.images.iter().cloned().map(Block::Image)).collect();
        Message::User { content, timestamp: now_ms() }
    }

    fn content(&self) -> &[Block] {
        match self {
            Message::User { content, .. } | Message::Assistant { content, .. } | Message::ToolResult { content, .. } => content,
        }
    }

    /// `(image count, total base64 bytes)`.
    pub fn image_stats(&self) -> (u64, usize) {
        self.content().iter().fold((0, 0), |(n, b), blk| match blk {
            Block::Image(i) => (n + 1, b + i.data.len()),
            _ => (n, b),
        })
    }

    /// A copy with every image replaced by a `[image]` text block, for payloads that must stay small.
    pub fn without_images(&self) -> Message {
        let mut m = self.clone();
        let (Message::User { content, .. } | Message::Assistant { content, .. } | Message::ToolResult { content, .. }) = &mut m;
        for b in content.iter_mut() {
            if matches!(b, Block::Image(_)) {
                *b = Block::text("[image]");
            }
        }
        m
    }
}
