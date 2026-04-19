//! Typed structs for the OpenAI Chat Completions API wire format.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Request types
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct ChatCompletionRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_completion_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolDef>>,
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<StreamOptions>,
    /// OpenAI reasoning models: "low", "medium", "high"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// Qwen-style thinking toggle
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enable_thinking: Option<bool>,
}

#[derive(Serialize)]
pub struct StreamOptions {
    pub include_usage: bool,
}

#[derive(Serialize)]
pub struct ChatMessage {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCallMessage>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Serialize)]
pub struct ToolCallMessage {
    pub id: String,
    #[serde(rename = "type")]
    pub call_type: String,
    pub function: ToolCallFunction,
}

#[derive(Serialize)]
pub struct ToolCallFunction {
    pub name: String,
    pub arguments: String,
}

#[derive(Serialize)]
pub struct ToolDef {
    #[serde(rename = "type")]
    pub tool_type: &'static str,
    pub function: ToolDefFunction,
}

#[derive(Serialize)]
pub struct ToolDefFunction {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

// ---------------------------------------------------------------------------
// SSE response types (deserialization)
// ---------------------------------------------------------------------------

#[derive(Deserialize, Debug)]
pub struct ChatCompletionChunk {
    #[serde(default)]
    pub id: Option<String>,
    pub choices: Vec<ChunkChoice>,
    #[serde(default)]
    pub usage: Option<ChunkUsage>,
}

#[derive(Deserialize, Debug)]
pub struct ChunkChoice {
    #[serde(default)]
    pub index: usize,
    #[serde(default)]
    pub delta: ChunkDelta,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

#[derive(Deserialize, Debug, Default)]
pub struct ChunkDelta {
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub reasoning_content: Option<String>,
    #[serde(default)]
    pub tool_calls: Option<Vec<ChunkToolCall>>,
}

#[derive(Deserialize, Debug)]
pub struct ChunkToolCall {
    #[serde(default)]
    pub index: usize,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub function: Option<ChunkFunction>,
}

#[derive(Deserialize, Debug)]
pub struct ChunkFunction {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub arguments: Option<String>,
}

#[derive(Deserialize, Debug)]
pub struct ChunkUsage {
    #[serde(default)]
    pub prompt_tokens: Option<u64>,
    #[serde(default)]
    pub completion_tokens: Option<u64>,
    #[serde(default)]
    pub total_tokens: Option<u64>,
    #[serde(default)]
    pub prompt_tokens_details: Option<PromptTokensDetails>,
    #[serde(default)]
    pub completion_tokens_details: Option<CompletionTokensDetails>,
}

#[derive(Deserialize, Debug, Default)]
pub struct PromptTokensDetails {
    #[serde(default)]
    pub cached_tokens: Option<u64>,
    #[serde(default)]
    pub cache_write_tokens: Option<u64>,
}

#[derive(Deserialize, Debug, Default)]
pub struct CompletionTokensDetails {
    #[serde(default)]
    pub reasoning_tokens: Option<u64>,
}

impl ChunkUsage {
    pub fn apply_to(&self, usage: &mut tau_agent_base::types::Usage) {
        let prompt_tokens = self.prompt_tokens.unwrap_or(0);
        let reported_cached = self
            .prompt_tokens_details
            .as_ref()
            .and_then(|d| d.cached_tokens)
            .unwrap_or(0);
        let cache_write = self
            .prompt_tokens_details
            .as_ref()
            .and_then(|d| d.cache_write_tokens)
            .unwrap_or(0);
        let reasoning = self
            .completion_tokens_details
            .as_ref()
            .and_then(|d| d.reasoning_tokens)
            .unwrap_or(0);

        // Normalize to tau semantics:
        // - cache_read: cache hits from previous requests only
        // - cache_write: tokens written to cache by this request
        // Some OpenAI-compatible providers (observed on OpenRouter) report
        // cached_tokens as (previous hits + current writes). If cache_write
        // is present, subtract it from cached_tokens so we don't double count.
        let cache_read = if cache_write > 0 {
            reported_cached.saturating_sub(cache_write)
        } else {
            reported_cached
        };

        // OpenAI reports cached tokens inside prompt_tokens; subtract both
        // cache_read and cache_write to get non-cached input.
        let input = prompt_tokens
            .saturating_sub(cache_read)
            .saturating_sub(cache_write);
        // Reasoning tokens are appended to output; some providers (e.g. Groq)
        // don't include them in total_tokens, so compute it ourselves.
        let output = self.completion_tokens.unwrap_or(0) + reasoning;

        usage.input = input;
        usage.output = output;
        usage.cache_read = cache_read;
        usage.cache_write = cache_write;
        usage.recompute_total();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tau_agent_base::types::Usage;

    fn apply(json: serde_json::Value) -> Usage {
        let u: ChunkUsage = serde_json::from_value(json).expect("parse");
        let mut usage = Usage::default();
        u.apply_to(&mut usage);
        usage
    }

    #[test]
    fn baseline_no_details() {
        // (a) No details → legacy behavior: prompt_tokens go to input, no cache.
        let usage = apply(serde_json::json!({
            "prompt_tokens": 100,
            "completion_tokens": 20,
        }));
        assert_eq!(usage.input, 100);
        assert_eq!(usage.output, 20);
        assert_eq!(usage.cache_read, 0);
        assert_eq!(usage.cache_write, 0);
        assert_eq!(usage.total_tokens, 120);
    }

    #[test]
    fn cached_tokens_no_cache_write() {
        // (b) cached_tokens reported, cache_write missing → identical to
        // pre-fix behavior: cached counted as cache_read, subtracted from input.
        let usage = apply(serde_json::json!({
            "prompt_tokens": 100,
            "completion_tokens": 20,
            "prompt_tokens_details": { "cached_tokens": 40 },
        }));
        assert_eq!(usage.input, 60);
        assert_eq!(usage.output, 20);
        assert_eq!(usage.cache_read, 40);
        assert_eq!(usage.cache_write, 0);
        assert_eq!(usage.total_tokens, 120);
    }

    #[test]
    fn openrouter_style_cached_equals_write() {
        // (c) cached_tokens == cache_write (OpenRouter-style double-count) →
        // cache_read normalizes to 0.
        let usage = apply(serde_json::json!({
            "prompt_tokens": 100,
            "completion_tokens": 20,
            "prompt_tokens_details": { "cached_tokens": 30, "cache_write_tokens": 30 },
        }));
        assert_eq!(usage.input, 70);
        assert_eq!(usage.output, 20);
        assert_eq!(usage.cache_read, 0);
        assert_eq!(usage.cache_write, 30);
        assert_eq!(usage.total_tokens, 120);
    }

    #[test]
    fn cached_greater_than_cache_write() {
        // (d) cached_tokens > cache_write → cache_read = diff.
        let usage = apply(serde_json::json!({
            "prompt_tokens": 100,
            "completion_tokens": 5,
            "prompt_tokens_details": { "cached_tokens": 50, "cache_write_tokens": 30 },
            "completion_tokens_details": { "reasoning_tokens": 0 },
        }));
        assert_eq!(usage.input, 50);
        assert_eq!(usage.output, 5);
        assert_eq!(usage.cache_read, 20);
        assert_eq!(usage.cache_write, 30);
        assert_eq!(usage.total_tokens, 105);
    }

    #[test]
    fn reasoning_tokens_added_to_output() {
        let usage = apply(serde_json::json!({
            "prompt_tokens": 100,
            "completion_tokens": 10,
            "completion_tokens_details": { "reasoning_tokens": 40 },
        }));
        assert_eq!(usage.input, 100);
        assert_eq!(usage.output, 50);
        assert_eq!(usage.total_tokens, 150);
    }
}
