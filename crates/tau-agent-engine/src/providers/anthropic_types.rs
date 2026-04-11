//! Typed structs for the Anthropic Messages API wire format.
//!
//! These are serialization-only (request) or deserialization-only (SSE events)
//! types that map to the Anthropic API, distinct from tau's internal types.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Request types
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct MessagesRequest {
    pub model: String,
    pub messages: Vec<ApiMessage>,
    pub max_tokens: u64,
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<Vec<SystemBlock>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolDef>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingConfig>,
}

#[derive(Serialize)]
pub struct SystemBlock {
    #[serde(rename = "type")]
    pub block_type: &'static str,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
}

#[derive(Serialize, Clone)]
pub struct CacheControl {
    #[serde(rename = "type")]
    pub cache_type: &'static str,
}

impl CacheControl {
    pub fn ephemeral() -> Self {
        Self {
            cache_type: "ephemeral",
        }
    }
}

#[derive(Serialize)]
pub struct ApiMessage {
    pub role: &'static str,
    pub content: serde_json::Value,
}

#[derive(Serialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

#[derive(Serialize)]
pub struct ThinkingConfig {
    #[serde(rename = "type")]
    pub thinking_type: &'static str,
    pub budget_tokens: u64,
}

// ---------------------------------------------------------------------------
// SSE event types (deserialization)
// ---------------------------------------------------------------------------

#[derive(Deserialize, Debug)]
pub struct ApiUsage {
    #[serde(default)]
    pub input_tokens: Option<u64>,
    #[serde(default)]
    pub output_tokens: Option<u64>,
    #[serde(default)]
    pub cache_read_input_tokens: Option<u64>,
    #[serde(default)]
    pub cache_creation_input_tokens: Option<u64>,
}

impl ApiUsage {
    pub fn apply_to(&self, usage: &mut tau_agent_base::types::Usage) {
        if let Some(n) = self.input_tokens {
            usage.input = n;
        }
        if let Some(n) = self.output_tokens {
            usage.output = n;
        }
        if let Some(n) = self.cache_read_input_tokens {
            usage.cache_read = n;
        }
        if let Some(n) = self.cache_creation_input_tokens {
            usage.cache_write = n;
        }
        usage.total_tokens = usage.input + usage.output + usage.cache_read + usage.cache_write;
    }
}

// -- message_start --

#[derive(Deserialize, Debug)]
pub struct MessageStartEvent {
    pub message: MessageStartMessage,
}

#[derive(Deserialize, Debug)]
pub struct MessageStartMessage {
    pub id: String,
    #[serde(default)]
    pub usage: Option<ApiUsage>,
}

// -- content_block_start --

#[derive(Deserialize, Debug)]
pub struct ContentBlockStartEvent {
    pub index: u64,
    pub content_block: ContentBlock,
}

#[derive(Deserialize, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        #[serde(default)]
        text: String,
    },
    Thinking {
        #[serde(default)]
        thinking: String,
    },
    RedactedThinking {
        #[serde(default)]
        data: String,
    },
    ToolUse {
        id: String,
        name: String,
        #[serde(default)]
        input: serde_json::Value,
    },
}

// -- content_block_delta --

#[derive(Deserialize, Debug)]
pub struct ContentBlockDeltaEvent {
    pub index: u64,
    pub delta: Delta,
}

#[derive(Deserialize, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Delta {
    TextDelta { text: String },
    ThinkingDelta { thinking: String },
    InputJsonDelta { partial_json: String },
    SignatureDelta { signature: String },
}

// -- content_block_stop --

#[derive(Deserialize, Debug)]
pub struct ContentBlockStopEvent {
    pub index: u64,
}

// -- message_delta --

#[derive(Deserialize, Debug)]
pub struct MessageDeltaEvent {
    #[serde(default)]
    pub delta: Option<MessageDelta>,
    #[serde(default)]
    pub usage: Option<ApiUsage>,
}

#[derive(Deserialize, Debug)]
pub struct MessageDelta {
    pub stop_reason: Option<String>,
}
