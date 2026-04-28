use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

// ---------------------------------------------------------------------------
// Cancellation
// ---------------------------------------------------------------------------

/// Shared cancellation flag for tool execution.
///
/// A thin wrapper around `Arc<AtomicBool>`. Tools (bash, long-running shells)
/// poll [`is_cancelled`] at short intervals and abort when it becomes true;
/// the server flips the flag on Ctrl-C / cancel RPC.
///
/// Tools that complete in microseconds (read, write, edit) are free to ignore
/// the token entirely, or check it once at the top to return a `cancelled`
/// error if the user cancelled before execution began.
///
/// Clones share the same underlying atomic.
#[derive(Clone, Debug, Default)]
pub struct CancelToken {
    flag: Arc<AtomicBool>,
}

impl CancelToken {
    /// Create a new, un-cancelled token.
    pub fn new() -> Self {
        Self::default()
    }

    /// Wrap an existing shared flag. Useful when the server already owns an
    /// `Arc<AtomicBool>` (per-session cancel flag) and wants to expose it to
    /// tool-execution paths without re-wrapping.
    pub fn from_flag(flag: Arc<AtomicBool>) -> Self {
        Self { flag }
    }

    /// Return the underlying shared flag (same `Arc`).
    pub fn flag(&self) -> Arc<AtomicBool> {
        self.flag.clone()
    }

    /// True if the token has been cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::Relaxed)
    }

    /// Set the cancel flag. Idempotent.
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::Relaxed);
    }
}

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

impl Usage {
    /// Recompute `total_tokens` as the sum of `input + output + cache_read + cache_write`.
    pub fn recompute_total(&mut self) {
        self.total_tokens = self.input + self.output + self.cache_read + self.cache_write;
    }
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Tier-2 actions to run after this tool result is persisted to the
    /// caller's session history, still inside the caller's turn. Ordering:
    /// actions run in vec order, each strictly after `emit_message` has
    /// persisted this tool result.
    ///
    /// Not serialised as part of the permanent message history — these are
    /// transient side effects attached to the returned tool result and
    /// dropped once drained by the agent loop.
    #[serde(default, skip_serializing, skip_deserializing)]
    pub post_persist_actions: Vec<PostPersistAction>,
}

/// Tier-2 actions the server performs after persisting a tool result,
/// still inside the calling session's agent loop (lock held).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PostPersistAction {
    /// Append an info message to any session's history. Use this for side
    /// effects that must render after the tool result (typically, info
    /// messages going to the caller's own session).
    EmitInfoMessage {
        target_session_id: String,
        text: String,
    },
}

/// Tier-3 actions the server performs after the calling session's lock is
/// released — i.e. after the agent loop exits. Used for side effects that
/// need exclusive access to the caller's session or its subtree.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PostIdleAction {
    /// Archive all archivable sessions for a task (worker, planner, refiner,
    /// reviewer, log roles). Retries up to 20 times on "session busy"
    /// errors, 1s apart, before giving up.
    ArchiveTaskSessions { task_id: i64 },
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
            duration_ms: None,
            summary: None,
            post_persist_actions: Vec::new(),
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
            duration_ms: None,
            summary: None,
            post_persist_actions: Vec::new(),
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

impl Default for ModelCost {
    fn default() -> Self {
        Self {
            input: 0.0,
            output: 0.0,
            cache_read: 0.0,
            cache_write: 0.0,
        }
    }
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

/// Effort level for adaptive thinking (Anthropic Opus 4.6+, Sonnet 4.6).
///
/// Passed through to provider-specific effort strings (e.g. Anthropic's
/// `output_config.effort`). The `XHigh` level is mapped per-model: on
/// `claude-opus-4-6` it becomes `"max"`, on `claude-opus-4-7` it becomes
/// `"xhigh"`, and on other adaptive models it falls back to `"high"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingEffort {
    Low,
    Medium,
    High,
    XHigh,
    Max,
}

/// How thinking content is returned in the response.
///
/// Anthropic's API defaults to `Omitted` for `claude-opus-4-7` and Mythos
/// Preview; tau defaults to `Summarized` when thinking is enabled so the
/// behaviour matches older Claude 4 models.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingDisplay {
    /// Thinking blocks contain summarized thinking text.
    Summarized,
    /// Thinking blocks return an empty thinking field; the encrypted
    /// signature still travels back for multi-turn continuity.
    Omitted,
}

/// Prompt-cache retention hint. Mirrors pi-ai's `CacheRetention`.
///
/// `Short` is the default and matches OpenAI's ~5-minute prefix cache TTL.
/// `Long` requests the 24h retention tier (sent to OpenAI as
/// `prompt_cache_retention: "24h"`). `None` opts out of provider-side prompt
/// caching where applicable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CacheRetention {
    None,
    Short,
    Long,
}

impl Default for CacheRetention {
    fn default() -> Self {
        Self::Short
    }
}

impl CacheRetention {
    /// Resolve an optional retention hint into a concrete value.
    ///
    /// `None` falls back to `Short`. Pure: callers that want to honour the
    /// `PI_CACHE_RETENTION=long` env override should call
    /// [`Self::resolve_with_env`] instead.
    pub fn resolve(opt: Option<Self>) -> Self {
        opt.unwrap_or_default()
    }

    /// Resolve an optional retention hint, honouring `PI_CACHE_RETENTION=long`
    /// from the environment when `opt` is `None`.
    ///
    /// Mirrors pi-ai's `resolveCacheRetention` helper. An explicit
    /// `Some(...)` always wins over the env var.
    pub fn resolve_with_env(opt: Option<Self>) -> Self {
        if let Some(v) = opt {
            return v;
        }
        match std::env::var("PI_CACHE_RETENTION").ok().as_deref() {
            Some("long") => Self::Long,
            _ => Self::Short,
        }
    }
}

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
    /// Extended thinking budget (Anthropic-specific non-adaptive path).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_budget: Option<u64>,
    /// Explicit on/off for extended thinking.
    ///
    /// `None` means provider default: if `thinking_budget` is set or (for
    /// adaptive-thinking models) an effort is set, thinking is enabled.
    /// `Some(true)` forces thinking on; on adaptive-thinking models this
    /// triggers the adaptive path even without a budget. `Some(false)`
    /// forces thinking off regardless of the other fields.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_enabled: Option<bool>,
    /// Effort level for adaptive thinking (Anthropic-specific for now).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_effort: Option<ThinkingEffort>,
    /// Controls how thinking content is returned in the response
    /// (Anthropic-specific for now). Defaults to `Summarized` when thinking
    /// is enabled on the Anthropic provider.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_display: Option<ThinkingDisplay>,
    /// Opaque session identifier for provider-side prompt-cache affinity.
    ///
    /// Currently used by the OpenAI provider as `prompt_cache_key` so
    /// consecutive turns of the same conversation hit the same cache shard.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Prompt-cache retention hint. `None` defers to the provider default
    /// (currently `Short`, with `PI_CACHE_RETENTION=long` env override
    /// honoured by the OpenAI provider).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_retention: Option<CacheRetention>,
}

// ---------------------------------------------------------------------------
// Agent phase
// ---------------------------------------------------------------------------

/// Current phase of the agent loop, broadcast to subscribers for UI display.
/// The TUI also derives phase implicitly from certain stream events
/// (see `App::update_phase_from_event` in tau-agent-tui).
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
        summary: Option<String>,
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
    ///
    /// `turn_started_at_ms` is the server-stamped wall-clock (Unix ms)
    /// when the current non-Idle turn began. It is preserved across
    /// phase→phase transitions within a single turn and cleared on
    /// transition to Idle. Used by clients to anchor the "Working... Xs"
    /// counter so it survives UI mode flicker and late subscribe.
    ///
    /// `phase_started_at_ms` is the server-stamped wall-clock (Unix ms)
    /// when the *current* phase began. Re-stamped on every phase
    /// transition (Idle→Thinking, Thinking→ToolExec, etc.) and cleared
    /// on Idle. Used by clients to render a per-phase elapsed counter
    /// alongside the total turn elapsed so a slow tool call doesn't
    /// keep climbing once the LLM resumes responding.
    Phase {
        phase: AgentPhase,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_started_at_ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        phase_started_at_ms: Option<u64>,
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
    fn usage_recompute_total_sums_fields() {
        let mut u = Usage {
            input: 10,
            output: 20,
            cache_read: 3,
            cache_write: 4,
            total_tokens: 0,
            cost: Cost::default(),
        };
        u.recompute_total();
        assert_eq!(u.total_tokens, 37);

        // Idempotent when fields unchanged.
        u.recompute_total();
        assert_eq!(u.total_tokens, 37);

        // Overwrites stale values.
        u.total_tokens = 999;
        u.recompute_total();
        assert_eq!(u.total_tokens, 37);
    }

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

    #[test]
    fn tool_result_message_duration_ms_roundtrip() {
        let msg = ToolResultMessage {
            tool_call_id: "tc1".into(),
            tool_name: "bash".into(),
            content: vec![ToolResultContent::Text(TextContent {
                text: "ok".into(),
                text_signature: None,
            })],
            details: None,
            is_error: false,
            timestamp: 1000,
            duration_ms: Some(1234),
            summary: None,
            post_persist_actions: Vec::new(),
        };
        let json = serde_json::to_string(&msg).expect("serialize");
        assert!(json.contains("\"duration_ms\":1234"));
        let deserialized: ToolResultMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(deserialized.duration_ms, Some(1234));
    }

    #[test]
    fn tool_result_message_duration_ms_backward_compat() {
        // Old messages without duration_ms should deserialize to None
        let json = r#"{"tool_call_id":"tc1","tool_name":"bash","content":[{"type":"text","text":"ok"}],"is_error":false,"timestamp":1000}"#;
        let msg: ToolResultMessage = serde_json::from_str(json).expect("deserialize");
        assert_eq!(msg.duration_ms, None);
    }

    #[test]
    fn tool_result_message_duration_ms_none_not_serialized() {
        let msg = ToolResultMessage::success("tc1", "bash", "ok");
        let json = serde_json::to_string(&msg).expect("serialize");
        assert!(
            !json.contains("duration_ms"),
            "duration_ms: None should not appear in JSON"
        );
    }

    #[test]
    fn tool_result_message_summary_roundtrip() {
        let mut msg = ToolResultMessage::success("tc1", "read", "file contents...");
        msg.summary = Some("read: src/main.rs (42 lines)".into());
        let json = serde_json::to_string(&msg).expect("serialize");
        assert!(json.contains("\"summary\":\"read: src/main.rs (42 lines)\""));
        let deserialized: ToolResultMessage = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(
            deserialized.summary,
            Some("read: src/main.rs (42 lines)".into())
        );
    }

    #[test]
    fn tool_result_message_summary_backward_compat() {
        let json = r#"{"tool_call_id":"tc1","tool_name":"bash","content":[{"type":"text","text":"ok"}],"is_error":false,"timestamp":1000}"#;
        let msg: ToolResultMessage = serde_json::from_str(json).expect("deserialize");
        assert_eq!(msg.summary, None);
    }

    #[test]
    fn cache_retention_resolve_defaults_to_short() {
        assert_eq!(CacheRetention::resolve(None), CacheRetention::Short);
        assert_eq!(
            CacheRetention::resolve(Some(CacheRetention::Long)),
            CacheRetention::Long
        );
        assert_eq!(
            CacheRetention::resolve(Some(CacheRetention::None)),
            CacheRetention::None
        );
    }

    #[test]
    fn cache_retention_resolve_with_env_explicit_wins() {
        // An explicit Some(...) must always beat the env var, regardless of
        // what PI_CACHE_RETENTION says in this process.
        assert_eq!(
            CacheRetention::resolve_with_env(Some(CacheRetention::None)),
            CacheRetention::None
        );
        assert_eq!(
            CacheRetention::resolve_with_env(Some(CacheRetention::Short)),
            CacheRetention::Short
        );
        assert_eq!(
            CacheRetention::resolve_with_env(Some(CacheRetention::Long)),
            CacheRetention::Long
        );
    }

    #[test]

    fn tool_result_message_summary_none_not_serialized() {
        let msg = ToolResultMessage::success("tc1", "bash", "ok");
        let json = serde_json::to_string(&msg).expect("serialize");
        assert!(
            !json.contains("summary"),
            "summary: None should not appear in JSON"
        );
    }
}
