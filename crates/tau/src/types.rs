use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Content blocks
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextContent {
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_signature: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThinkingContent {
    pub thinking: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_signature: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub redacted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageContent {
    pub data: String, // base64
    pub mime_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UserContent {
    Text(TextContent),
    Image(ImageContent),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AssistantContent {
    Text(TextContent),
    Thinking(ThinkingContent),
    ToolCall(ToolCall),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolResultContent {
    Text(TextContent),
    Image(ImageContent),
}

impl ToolResultContent {
    /// Return the text if this is a Text variant, empty string otherwise.
    pub fn text(&self) -> &str {
        match self {
            Self::Text(t) => &t.text,
            Self::Image(_) => "",
        }
    }
}

// ---------------------------------------------------------------------------
// Usage & cost
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Cost {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    pub total: f64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub total_tokens: u64,
    pub cost: Cost,
}

// ---------------------------------------------------------------------------
// Stop reason
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    Stop,
    Length,
    ToolUse,
    Error,
    Aborted,
}

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserMessage {
    pub content: Vec<UserContent>,
    pub timestamp: u64,
}

impl UserMessage {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content: vec![UserContent::Text(TextContent {
                text: text.into(),
                text_signature: None,
            })],
            timestamp: timestamp_ms(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssistantMessage {
    pub content: Vec<AssistantContent>,
    pub api: String,
    pub provider: String,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_id: Option<String>,
    pub usage: Usage,
    pub stop_reason: StopReason,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    pub timestamp: u64,
}

impl AssistantMessage {
    pub fn empty(api: &str, provider: &str, model: &str) -> Self {
        Self {
            content: Vec::new(),
            api: api.to_string(),
            provider: provider.to_string(),
            model: model.to_string(),
            response_id: None,
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp: timestamp_ms(),
        }
    }

    /// Concatenate all text content blocks.
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|c| match c {
                AssistantContent::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResultMessage {
    pub tool_call_id: String,
    pub tool_name: String,
    pub content: Vec<ToolResultContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
    pub is_error: bool,
    pub timestamp: u64,
}

impl ToolResultMessage {
    pub fn success(
        id: impl Into<String>,
        name: impl Into<String>,
        text: impl Into<String>,
    ) -> Self {
        Self {
            tool_call_id: id.into(),
            tool_name: name.into(),
            content: vec![ToolResultContent::Text(TextContent {
                text: text.into(),
                text_signature: None,
            })],
            details: None,
            is_error: false,
            timestamp: timestamp_ms(),
        }
    }

    pub fn error(id: impl Into<String>, name: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            tool_call_id: id.into(),
            tool_name: name.into(),
            content: vec![ToolResultContent::Text(TextContent {
                text: text.into(),
                text_signature: None,
            })],
            details: None,
            is_error: true,
            timestamp: timestamp_ms(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionSummaryMessage {
    pub summary: String,
    /// How many tokens the context had before compaction.
    pub tokens_before: u64,
    pub timestamp: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InfoMessage {
    pub text: String,
    pub timestamp: u64,
}

impl InfoMessage {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            timestamp: timestamp_ms(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum Message {
    User(UserMessage),
    Assistant(AssistantMessage),
    ToolResult(ToolResultMessage),
    CompactionSummary(CompactionSummaryMessage),
    Info(InfoMessage),
}

// ---------------------------------------------------------------------------
// Model
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCost {
    pub input: f64,  // $/million tokens
    pub output: f64, // $/million tokens
    pub cache_read: f64,
    pub cache_write: f64,
}

/// How the model supports extended thinking/reasoning.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingStyle {
    /// No thinking support.
    #[default]
    None,
    /// Anthropic: budget_tokens or adaptive thinking.
    Anthropic,
    /// OpenAI: reasoning_effort parameter.
    #[serde(alias = "openai")]
    OpenAi,
    /// Qwen (OpenAI-compat): enable_thinking: bool.
    Qwen,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Model {
    pub id: String,
    pub name: String,
    pub api: String,
    pub provider: String,
    pub base_url: String,
    #[serde(default)]
    pub thinking: ThinkingStyle,
    pub cost: ModelCost,
    pub context_window: u64,
    pub max_tokens: u64,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub headers: HashMap<String, String>,
}

impl Model {
    pub fn calculate_cost(&self, usage: &mut Usage) {
        usage.cost.input = (self.cost.input / 1_000_000.0) * usage.input as f64;
        usage.cost.output = (self.cost.output / 1_000_000.0) * usage.output as f64;
        usage.cost.cache_read = (self.cost.cache_read / 1_000_000.0) * usage.cache_read as f64;
        usage.cost.cache_write = (self.cost.cache_write / 1_000_000.0) * usage.cache_write as f64;
        usage.cost.total =
            usage.cost.input + usage.cost.output + usage.cost.cache_read + usage.cost.cache_write;
    }
}

// ---------------------------------------------------------------------------
// Tool definition
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tool {
    pub name: String,
    pub description: String,
    /// JSON Schema for parameters
    pub parameters: serde_json::Value,
}

// ---------------------------------------------------------------------------
// Context (what gets sent to the LLM)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Context {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    pub messages: Vec<Message>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<Tool>,
}

// ---------------------------------------------------------------------------
// Stream options
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StreamOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub headers: HashMap<String, String>,
    /// Extended thinking budget (Anthropic-specific for now)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_budget: Option<u64>,
}

// ---------------------------------------------------------------------------
// Agent phase
// ---------------------------------------------------------------------------

/// Current phase of the agent loop, broadcast to subscribers for UI display.
/// The TUI also derives phase implicitly from certain stream events
/// (see `App::update_phase_from_event` in tau-tui).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum AgentPhase {
    /// No agent turn running.
    #[default]
    Idle,
    /// Blocked waiting for session lock (another turn in progress).
    Waiting,
    /// Loading session, spawning plugins, running hooks.
    Preparing,
    /// HTTP request sent, waiting for first SSE byte from provider.
    Connecting,
    /// Receiving thinking tokens from the LLM.
    Thinking,
    /// Streaming text/tool-call tokens from the LLM.
    Responding,
    /// Executing tool calls.
    ToolExec,
    /// Running context compaction.
    Compacting,
    /// Waiting for rate limit / retry backoff.
    RateLimited,
}

impl AgentPhase {
    /// Human-readable label for the status line.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Waiting => "waiting...",
            Self::Preparing => "preparing...",
            Self::Connecting => "sending request...",
            Self::Thinking => "thinking...",
            Self::Responding => "working...",
            Self::ToolExec => "running tools...",
            Self::Compacting => "compacting...",
            Self::RateLimited => "rate limited...",
        }
    }
}

// ---------------------------------------------------------------------------
// Streaming events (mirrors pi-ai's AssistantMessageEvent)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    Start {
        partial: AssistantMessage,
    },
    TextStart {
        content_index: usize,
        partial: AssistantMessage,
    },
    TextDelta {
        content_index: usize,
        delta: String,
        partial: AssistantMessage,
    },
    TextEnd {
        content_index: usize,
        content: String,
        partial: AssistantMessage,
    },
    ThinkingStart {
        content_index: usize,
        partial: AssistantMessage,
    },
    ThinkingDelta {
        content_index: usize,
        delta: String,
        partial: AssistantMessage,
    },
    ThinkingEnd {
        content_index: usize,
        content: String,
        partial: AssistantMessage,
    },
    ToolcallStart {
        content_index: usize,
        partial: AssistantMessage,
    },
    ToolcallDelta {
        content_index: usize,
        delta: String,
        partial: AssistantMessage,
    },
    ToolcallEnd {
        content_index: usize,
        tool_call: ToolCall,
        partial: AssistantMessage,
    },
    /// Incremental tool output line (streaming, e.g. bash).
    ToolOutputDelta {
        tool_call_id: String,
        delta: String,
    },
    /// Tool execution completed.
    ToolResult {
        tool_call_id: String,
        tool_name: String,
        is_error: bool,
        /// Full text output.
        content: String,
    },
    Done {
        reason: StopReason,
        message: AssistantMessage,
    },
    Error {
        reason: StopReason,
        error: AssistantMessage,
    },
    /// A steering message was injected mid-loop.
    SteerMessage {
        message: UserMessage,
    },
    /// Agent phase transition. Only sent for phases that have no implicit
    /// stream event (Waiting, Preparing, Connecting, Compacting).
    /// Other phases are derived by the TUI from existing events:
    /// - ThinkingStart/ThinkingDelta → Thinking
    /// - TextStart/TextDelta → Responding  
    /// - ToolcallStart → Responding (still LLM output)
    /// - ToolResult → ToolExec
    /// - Start → transition from Connecting (but phase already set)
    /// - AgentDone/Cancelled/Error → Idle
    Phase {
        phase: AgentPhase,
    },
    /// Informational status message (e.g. retry notices).
    Status {
        message: String,
    },
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

pub fn timestamp_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn info_message_serde_roundtrip() {
        let msg = Message::Info(InfoMessage {
            text: "task state changed".into(),
            timestamp: 12345,
        });
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""role":"info""#));
        assert!(json.contains(r#""text":"task state changed""#));
        let deserialized: Message = serde_json::from_str(&json).unwrap();
        assert!(
            matches!(deserialized, Message::Info(i) if i.text == "task state changed" && i.timestamp == 12345)
        );
    }
}
