use std::io::{BufRead, BufReader};

use async_trait::async_trait;

use crate::provider::{EventReceiver, EventSender, Provider};
use crate::types::*;

const API_ID: &str = "anthropic-messages";
const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";

/// Anthropic Messages API provider.
pub struct Anthropic;

#[async_trait]
impl Provider for Anthropic {
    fn api_id(&self) -> &str {
        API_ID
    }

    fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: &StreamOptions,
    ) -> crate::Result<EventReceiver> {
        let (tx, rx) = smol::channel::unbounded();

        let body = build_request_body(model, context, options)?;
        let base_url = model.base_url.clone();
        let api_key = options
            .api_key
            .clone()
            .or_else(|| std::env::var("ANTHROPIC_API_KEY").ok())
            .ok_or_else(|| crate::Error::NoApiKey("anthropic".into()))?;
        let api_id = model.api.clone();
        let provider_name = model.provider.clone();
        let model_id = model.id.clone();
        let model_clone = model.clone();

        // Spawn blocking HTTP + SSE parsing on a thread
        std::thread::spawn(move || {
            let ctx = StreamContext {
                base_url: &base_url,
                api_key: &api_key,
                api_id: &api_id,
                provider_name: &provider_name,
                model_id: &model_id,
                model: &model_clone,
            };
            let result = run_stream(&ctx, &body, &tx);
            if let Err(e) = result {
                let mut msg = AssistantMessage::empty(&api_id, &provider_name, &model_id);
                msg.stop_reason = StopReason::Error;
                msg.error_message = Some(e.to_string());
                let _ = tx.send_blocking(StreamEvent::Error {
                    reason: StopReason::Error,
                    error: msg,
                });
            }
        });

        Ok(rx)
    }
}

struct StreamContext<'a> {
    base_url: &'a str,
    api_key: &'a str,
    api_id: &'a str,
    provider_name: &'a str,
    model_id: &'a str,
    model: &'a Model,
}

#[allow(clippy::too_many_lines)]
fn run_stream(
    ctx: &StreamContext<'_>,
    body: &serde_json::Value,
    tx: &EventSender,
) -> crate::Result<()> {
    let url = format!("{}/v1/messages", ctx.base_url.trim_end_matches('/'));

    let resp = ureq::post(&url)
        .set("x-api-key", ctx.api_key)
        .set("content-type", "application/json")
        .set("anthropic-version", "2023-06-01")
        .send_json(body)
        .map_err(|e: ureq::Error| crate::Error::Http(e.to_string()))?;

    let reader = BufReader::new(resp.into_reader());
    let mut output = AssistantMessage::empty(ctx.api_id, ctx.provider_name, ctx.model_id);
    tx.send_blocking(StreamEvent::Start {
        partial: output.clone(),
    })
    .map_err(|_| crate::Error::ChannelClosed)?;

    // Track block index → content index mapping
    let mut block_index_map: Vec<(u64, usize)> = Vec::new();

    let mut current_event_type = String::new();

    for line in reader.lines() {
        let line = line.map_err(|e: std::io::Error| crate::Error::Http(e.to_string()))?;

        if let Some(event_type) = line.strip_prefix("event: ") {
            current_event_type = event_type.to_string();
            continue;
        }

        if !line.starts_with("data: ") {
            continue;
        }

        let data = &line[6..];
        let event: serde_json::Value =
            serde_json::from_str(data).map_err(|e| crate::Error::Parse(e.to_string()))?;

        match current_event_type.as_str() {
            "message_start" => {
                if let Some(msg) = event.get("message") {
                    if let Some(id) = msg.get("id").and_then(|v| v.as_str()) {
                        output.response_id = Some(id.to_string());
                    }
                    if let Some(usage) = msg.get("usage") {
                        parse_usage(usage, &mut output.usage);
                        ctx.model.calculate_cost(&mut output.usage);
                    }
                }
            }
            "content_block_start" => {
                let block_idx = event.get("index").and_then(|v| v.as_u64()).unwrap_or(0);
                let block = event.get("content_block");
                let block_type = block
                    .and_then(|b| b.get("type"))
                    .and_then(|t| t.as_str())
                    .unwrap_or("");

                match block_type {
                    "text" => {
                        output.content.push(AssistantContent::Text(TextContent {
                            text: String::new(),
                            text_signature: None,
                        }));
                        let ci = output.content.len() - 1;
                        block_index_map.push((block_idx, ci));
                        tx.send_blocking(StreamEvent::TextStart {
                            content_index: ci,
                            partial: output.clone(),
                        })
                        .map_err(|_| crate::Error::ChannelClosed)?;
                    }
                    "thinking" => {
                        output
                            .content
                            .push(AssistantContent::Thinking(ThinkingContent {
                                thinking: String::new(),
                                thinking_signature: None,
                                redacted: false,
                            }));
                        let ci = output.content.len() - 1;
                        block_index_map.push((block_idx, ci));
                        tx.send_blocking(StreamEvent::ThinkingStart {
                            content_index: ci,
                            partial: output.clone(),
                        })
                        .map_err(|_| crate::Error::ChannelClosed)?;
                    }
                    "redacted_thinking" => {
                        let sig = block
                            .and_then(|b| b.get("data"))
                            .and_then(|d| d.as_str())
                            .unwrap_or("")
                            .to_string();
                        output
                            .content
                            .push(AssistantContent::Thinking(ThinkingContent {
                                thinking: "[Reasoning redacted]".into(),
                                thinking_signature: Some(sig),
                                redacted: true,
                            }));
                        let ci = output.content.len() - 1;
                        block_index_map.push((block_idx, ci));
                        tx.send_blocking(StreamEvent::ThinkingStart {
                            content_index: ci,
                            partial: output.clone(),
                        })
                        .map_err(|_| crate::Error::ChannelClosed)?;
                    }
                    "tool_use" => {
                        let id = block
                            .and_then(|b| b.get("id"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let name = block
                            .and_then(|b| b.get("name"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        output.content.push(AssistantContent::ToolCall(ToolCall {
                            id,
                            name,
                            arguments: serde_json::Value::Object(Default::default()),
                        }));
                        let ci = output.content.len() - 1;
                        block_index_map.push((block_idx, ci));
                        tx.send_blocking(StreamEvent::ToolcallStart {
                            content_index: ci,
                            partial: output.clone(),
                        })
                        .map_err(|_| crate::Error::ChannelClosed)?;
                    }
                    _ => {}
                }
            }
            "content_block_delta" => {
                let block_idx = event.get("index").and_then(|v| v.as_u64()).unwrap_or(0);
                let ci = block_index_map
                    .iter()
                    .find(|(bi, _)| *bi == block_idx)
                    .map(|(_, ci)| *ci);
                let Some(ci) = ci else { continue };

                if let Some(delta) = event.get("delta") {
                    let delta_type = delta.get("type").and_then(|t| t.as_str()).unwrap_or("");
                    match delta_type {
                        "text_delta" => {
                            let text = delta.get("text").and_then(|t| t.as_str()).unwrap_or("");
                            if let Some(AssistantContent::Text(t)) = output.content.get_mut(ci) {
                                t.text.push_str(text);
                            }
                            tx.send_blocking(StreamEvent::TextDelta {
                                content_index: ci,
                                delta: text.to_string(),
                                partial: output.clone(),
                            })
                            .map_err(|_| crate::Error::ChannelClosed)?;
                        }
                        "thinking_delta" => {
                            let text = delta.get("thinking").and_then(|t| t.as_str()).unwrap_or("");
                            if let Some(AssistantContent::Thinking(t)) = output.content.get_mut(ci)
                            {
                                t.thinking.push_str(text);
                            }
                            tx.send_blocking(StreamEvent::ThinkingDelta {
                                content_index: ci,
                                delta: text.to_string(),
                                partial: output.clone(),
                            })
                            .map_err(|_| crate::Error::ChannelClosed)?;
                        }
                        "input_json_delta" => {
                            let json_str = delta
                                .get("partial_json")
                                .and_then(|t| t.as_str())
                                .unwrap_or("");
                            tx.send_blocking(StreamEvent::ToolcallDelta {
                                content_index: ci,
                                delta: json_str.to_string(),
                                partial: output.clone(),
                            })
                            .map_err(|_| crate::Error::ChannelClosed)?;
                        }
                        "signature_delta" => {
                            if let Some(sig) = delta.get("signature").and_then(|s| s.as_str())
                                && let Some(AssistantContent::Thinking(t)) =
                                    output.content.get_mut(ci)
                            {
                                let s = t.thinking_signature.get_or_insert_with(String::new);
                                s.push_str(sig);
                            }
                        }
                        _ => {}
                    }
                }
            }
            "content_block_stop" => {
                let block_idx = event.get("index").and_then(|v| v.as_u64()).unwrap_or(0);
                let ci = block_index_map
                    .iter()
                    .find(|(bi, _)| *bi == block_idx)
                    .map(|(_, ci)| *ci);
                let Some(ci) = ci else { continue };

                match output.content.get(ci) {
                    Some(AssistantContent::Text(t)) => {
                        tx.send_blocking(StreamEvent::TextEnd {
                            content_index: ci,
                            content: t.text.clone(),
                            partial: output.clone(),
                        })
                        .map_err(|_| crate::Error::ChannelClosed)?;
                    }
                    Some(AssistantContent::Thinking(t)) => {
                        tx.send_blocking(StreamEvent::ThinkingEnd {
                            content_index: ci,
                            content: t.thinking.clone(),
                            partial: output.clone(),
                        })
                        .map_err(|_| crate::Error::ChannelClosed)?;
                    }
                    Some(AssistantContent::ToolCall(tc)) => {
                        tx.send_blocking(StreamEvent::ToolcallEnd {
                            content_index: ci,
                            tool_call: tc.clone(),
                            partial: output.clone(),
                        })
                        .map_err(|_| crate::Error::ChannelClosed)?;
                    }
                    None => {}
                }
            }
            "message_delta" => {
                if let Some(delta) = event.get("delta")
                    && let Some(reason) = delta.get("stop_reason").and_then(|r| r.as_str())
                {
                    output.stop_reason = map_stop_reason(reason);
                }
                if let Some(usage) = event.get("usage") {
                    parse_usage(usage, &mut output.usage);
                    ctx.model.calculate_cost(&mut output.usage);
                }
            }
            "message_stop" => {
                tx.send_blocking(StreamEvent::Done {
                    reason: output.stop_reason,
                    message: output,
                })
                .map_err(|_| crate::Error::ChannelClosed)?;
                return Ok(());
            }
            _ => {}
        }
    }

    // Stream ended without message_stop — treat as error
    output.stop_reason = StopReason::Error;
    output.error_message = Some("Stream ended unexpectedly".into());
    tx.send_blocking(StreamEvent::Error {
        reason: StopReason::Error,
        error: output,
    })
    .map_err(|_| crate::Error::ChannelClosed)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Request building
// ---------------------------------------------------------------------------

fn build_request_body(
    model: &Model,
    context: &Context,
    options: &StreamOptions,
) -> crate::Result<serde_json::Value> {
    let mut messages = Vec::new();

    for msg in &context.messages {
        match msg {
            Message::User(u) => {
                let content = convert_user_content(&u.content);
                messages.push(serde_json::json!({
                    "role": "user",
                    "content": content,
                }));
            }
            Message::Assistant(a) => {
                let content = convert_assistant_content(&a.content);
                if !content.is_empty() {
                    messages.push(serde_json::json!({
                        "role": "assistant",
                        "content": content,
                    }));
                }
            }
            Message::ToolResult(tr) => {
                let content = convert_tool_result_content(&tr.content);
                messages.push(serde_json::json!({
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": tr.tool_call_id,
                        "content": content,
                        "is_error": tr.is_error,
                    }],
                }));
            }
        }
    }

    let max_tokens = options
        .max_tokens
        .unwrap_or((model.max_tokens / 3).max(1024));

    let mut body = serde_json::json!({
        "model": model.id,
        "messages": messages,
        "max_tokens": max_tokens,
        "stream": true,
    });

    if let Some(ref prompt) = context.system_prompt {
        body["system"] = serde_json::json!(prompt);
    }

    if let Some(temp) = options.temperature {
        body["temperature"] = serde_json::json!(temp);
    }

    if !context.tools.is_empty() {
        let tools: Vec<serde_json::Value> = context
            .tools
            .iter()
            .map(|t| {
                serde_json::json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": t.parameters,
                })
            })
            .collect();
        body["tools"] = serde_json::json!(tools);
    }

    if let Some(budget) = options.thinking_budget {
        body["thinking"] = serde_json::json!({
            "type": "enabled",
            "budget_tokens": budget,
        });
    }

    Ok(body)
}

fn convert_user_content(content: &[UserContent]) -> serde_json::Value {
    if content.len() == 1
        && let UserContent::Text(t) = &content[0]
    {
        return serde_json::Value::String(t.text.clone());
    }
    let blocks: Vec<serde_json::Value> = content
        .iter()
        .map(|c| match c {
            UserContent::Text(t) => serde_json::json!({"type": "text", "text": t.text}),
            UserContent::Image(img) => serde_json::json!({
                "type": "image",
                "source": {
                    "type": "base64",
                    "media_type": img.mime_type,
                    "data": img.data,
                }
            }),
        })
        .collect();
    serde_json::Value::Array(blocks)
}

fn convert_assistant_content(content: &[AssistantContent]) -> Vec<serde_json::Value> {
    content
        .iter()
        .filter_map(|c| match c {
            AssistantContent::Text(t) if !t.text.is_empty() => {
                Some(serde_json::json!({"type": "text", "text": t.text}))
            }
            AssistantContent::Thinking(t) if t.redacted => Some(serde_json::json!({
                "type": "redacted_thinking",
                "data": t.thinking_signature.as_deref().unwrap_or(""),
            })),
            AssistantContent::Thinking(t) if !t.thinking.is_empty() => {
                if let Some(ref sig) = t.thinking_signature
                    && !sig.is_empty()
                {
                    return Some(serde_json::json!({
                        "type": "thinking",
                        "thinking": t.thinking,
                        "signature": sig,
                    }));
                }
                // No signature — convert to plain text
                Some(serde_json::json!({"type": "text", "text": t.thinking}))
            }
            AssistantContent::ToolCall(tc) => Some(serde_json::json!({
                "type": "tool_use",
                "id": tc.id,
                "name": tc.name,
                "input": tc.arguments,
            })),
            _ => None,
        })
        .collect()
}

fn convert_tool_result_content(content: &[ToolResultContent]) -> serde_json::Value {
    if content.len() == 1
        && let ToolResultContent::Text(t) = &content[0]
    {
        return serde_json::Value::String(t.text.clone());
    }
    let blocks: Vec<serde_json::Value> = content
        .iter()
        .map(|c| match c {
            ToolResultContent::Text(t) => serde_json::json!({"type": "text", "text": t.text}),
            ToolResultContent::Image(img) => serde_json::json!({
                "type": "image",
                "source": {
                    "type": "base64",
                    "media_type": img.mime_type,
                    "data": img.data,
                }
            }),
        })
        .collect();
    serde_json::Value::Array(blocks)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn parse_usage(v: &serde_json::Value, usage: &mut Usage) {
    if let Some(n) = v.get("input_tokens").and_then(|n| n.as_u64()) {
        usage.input = n;
    }
    if let Some(n) = v.get("output_tokens").and_then(|n| n.as_u64()) {
        usage.output = n;
    }
    if let Some(n) = v.get("cache_read_input_tokens").and_then(|n| n.as_u64()) {
        usage.cache_read = n;
    }
    if let Some(n) = v
        .get("cache_creation_input_tokens")
        .and_then(|n| n.as_u64())
    {
        usage.cache_write = n;
    }
    usage.total_tokens = usage.input + usage.output + usage.cache_read + usage.cache_write;
}

fn map_stop_reason(reason: &str) -> StopReason {
    match reason {
        "end_turn" | "pause_turn" | "stop_sequence" => StopReason::Stop,
        "max_tokens" => StopReason::Length,
        "tool_use" => StopReason::ToolUse,
        _ => StopReason::Error,
    }
}

/// Predefined Anthropic models.
pub fn models() -> Vec<Model> {
    vec![
        Model {
            id: "claude-sonnet-4-20250514".into(),
            name: "Claude Sonnet 4".into(),
            api: API_ID.into(),
            provider: "anthropic".into(),
            base_url: DEFAULT_BASE_URL.into(),
            reasoning: true,
            cost: ModelCost {
                input: 3.0,
                output: 15.0,
                cache_read: 0.3,
                cache_write: 3.75,
            },
            context_window: 200_000,
            max_tokens: 16_384,
            headers: Default::default(),
        },
        Model {
            id: "claude-opus-4-20250514".into(),
            name: "Claude Opus 4".into(),
            api: API_ID.into(),
            provider: "anthropic".into(),
            base_url: DEFAULT_BASE_URL.into(),
            reasoning: true,
            cost: ModelCost {
                input: 15.0,
                output: 75.0,
                cache_read: 1.5,
                cache_write: 18.75,
            },
            context_window: 200_000,
            max_tokens: 32_768,
            headers: Default::default(),
        },
        Model {
            id: "claude-3-5-haiku-20241022".into(),
            name: "Claude 3.5 Haiku".into(),
            api: API_ID.into(),
            provider: "anthropic".into(),
            base_url: DEFAULT_BASE_URL.into(),
            reasoning: false,
            cost: ModelCost {
                input: 0.8,
                output: 4.0,
                cache_read: 0.08,
                cache_write: 1.0,
            },
            context_window: 200_000,
            max_tokens: 8_192,
            headers: Default::default(),
        },
    ]
}
