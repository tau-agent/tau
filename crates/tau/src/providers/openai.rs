//! OpenAI Chat Completions API provider.
//!
//! Also used for OpenAI-compatible APIs (Qwen, local models, etc.)
//! via different base_url and model settings.

use std::io::{BufRead, BufReader};

use async_trait::async_trait;

use super::openai_types;
use crate::provider::{EventReceiver, EventSender, Provider};
use crate::types::*;

const API_ID: &str = "openai-completions";

/// OpenAI Chat Completions API provider.
pub struct OpenAi;

#[async_trait]
impl Provider for OpenAi {
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
            .or_else(|| std::env::var("OPENAI_API_KEY").ok())
            .unwrap_or_default();
        let api_id = model.api.clone();
        let provider_name = model.provider.clone();
        let model_id = model.id.clone();
        let model_clone = model.clone();

        std::thread::spawn(move || {
            let ctx = StreamCtx {
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

struct StreamCtx<'a> {
    base_url: &'a str,
    api_key: &'a str,
    api_id: &'a str,
    provider_name: &'a str,
    model_id: &'a str,
    model: &'a Model,
}

// ---------------------------------------------------------------------------
// SSE stream processing
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_lines)]
fn run_stream(
    ctx: &StreamCtx<'_>,
    body: &openai_types::ChatCompletionRequest,
    tx: &EventSender,
) -> crate::Result<()> {
    let url = format!("{}/chat/completions", ctx.base_url.trim_end_matches('/'));

    let mut req = ureq::post(&url)
        .header("content-type", "application/json")
        .header("accept", "text/event-stream");

    if !ctx.api_key.is_empty() {
        req = req.header("authorization", &format!("Bearer {}", ctx.api_key));
    }

    let mut resp = req
        .send_json(body)
        .map_err(|e| crate::Error::Http(e.to_string()))?;

    let reader = BufReader::new(resp.body_mut().as_reader());
    let mut output = AssistantMessage::empty(ctx.api_id, ctx.provider_name, ctx.model_id);

    tx.send_blocking(StreamEvent::Start {
        partial: output.clone(),
    })
    .map_err(|_| crate::Error::ChannelClosed)?;

    // Track tool calls by index (OpenAI streams them incrementally)
    struct ToolAccum {
        id: String,
        name: String,
        arguments: String,
        content_index: usize,
    }
    let mut tool_accums: Vec<ToolAccum> = Vec::new();
    let mut text_started = false;
    let mut thinking_started = false;

    for line in reader.lines() {
        let line = line.map_err(|e: std::io::Error| crate::Error::Http(e.to_string()))?;

        if line.trim().is_empty() {
            continue;
        }

        let Some(data) = line.strip_prefix("data: ") else {
            continue;
        };

        if data == "[DONE]" {
            break;
        }

        let chunk: openai_types::ChatCompletionChunk =
            serde_json::from_str(data).map_err(|e| crate::Error::Parse(e.to_string()))?;

        if output.response_id.is_none()
            && let Some(id) = chunk.id
        {
            output.response_id = Some(id);
        }

        // Process usage (typically in the final chunk)
        if let Some(usage) = chunk.usage {
            usage.apply_to(&mut output.usage);
            ctx.model.calculate_cost(&mut output.usage);
        }

        let Some(choice) = chunk.choices.first() else {
            continue;
        };

        // Handle finish_reason
        if let Some(ref reason) = choice.finish_reason {
            output.stop_reason = map_finish_reason(reason);
        }

        let delta = &choice.delta;

        // Reasoning/thinking content (OpenAI o-series, Qwen with enable_thinking)
        if let Some(ref thinking) = delta.reasoning_content {
            if !thinking_started {
                output
                    .content
                    .push(AssistantContent::Thinking(ThinkingContent {
                        thinking: String::new(),
                        thinking_signature: None,
                        redacted: false,
                    }));
                thinking_started = true;
                tx.send_blocking(StreamEvent::ThinkingStart {
                    content_index: output.content.len() - 1,
                    partial: output.clone(),
                })
                .map_err(|_| crate::Error::ChannelClosed)?;
            }
            let ci = output.content.len() - 1;
            if let Some(AssistantContent::Thinking(t)) = output.content.get_mut(ci) {
                t.thinking.push_str(thinking);
            }
            tx.send_blocking(StreamEvent::ThinkingDelta {
                content_index: ci,
                delta: thinking.clone(),
                partial: output.clone(),
            })
            .map_err(|_| crate::Error::ChannelClosed)?;
        }

        // Text content
        if let Some(ref text) = delta.content {
            // Close thinking block if we were in one
            if thinking_started {
                let ci = output.content.len() - 1;
                if let Some(AssistantContent::Thinking(t)) = output.content.get(ci) {
                    tx.send_blocking(StreamEvent::ThinkingEnd {
                        content_index: ci,
                        content: t.thinking.clone(),
                        partial: output.clone(),
                    })
                    .map_err(|_| crate::Error::ChannelClosed)?;
                }
                thinking_started = false;
            }

            if !text_started {
                output.content.push(AssistantContent::Text(TextContent {
                    text: String::new(),
                    text_signature: None,
                }));
                text_started = true;
                tx.send_blocking(StreamEvent::TextStart {
                    content_index: output.content.len() - 1,
                    partial: output.clone(),
                })
                .map_err(|_| crate::Error::ChannelClosed)?;
            }
            let ci = output.content.len() - 1;
            if let Some(AssistantContent::Text(t)) = output.content.get_mut(ci) {
                t.text.push_str(text);
            }
            tx.send_blocking(StreamEvent::TextDelta {
                content_index: ci,
                delta: text.clone(),
                partial: output.clone(),
            })
            .map_err(|_| crate::Error::ChannelClosed)?;
        }

        // Tool calls
        if let Some(ref tool_calls) = delta.tool_calls {
            // Close text block if open
            if text_started {
                let ci = output.content.len() - 1;
                if let Some(AssistantContent::Text(t)) = output.content.get(ci) {
                    tx.send_blocking(StreamEvent::TextEnd {
                        content_index: ci,
                        content: t.text.clone(),
                        partial: output.clone(),
                    })
                    .map_err(|_| crate::Error::ChannelClosed)?;
                }
                text_started = false;
            }

            for tc in tool_calls {
                // Find or create accumulator for this tool call index
                while tool_accums.len() <= tc.index {
                    // New tool call
                    output.content.push(AssistantContent::ToolCall(ToolCall {
                        id: String::new(),
                        name: String::new(),
                        arguments: serde_json::Value::Object(Default::default()),
                    }));
                    let ci = output.content.len() - 1;
                    tool_accums.push(ToolAccum {
                        id: String::new(),
                        name: String::new(),
                        arguments: String::new(),
                        content_index: ci,
                    });
                    tx.send_blocking(StreamEvent::ToolcallStart {
                        content_index: ci,
                        partial: output.clone(),
                    })
                    .map_err(|_| crate::Error::ChannelClosed)?;
                }

                let accum = &mut tool_accums[tc.index];
                if let Some(ref id) = tc.id {
                    accum.id = id.clone();
                }
                if let Some(ref func) = tc.function {
                    if let Some(ref name) = func.name {
                        accum.name.push_str(name);
                    }
                    if let Some(ref args) = func.arguments {
                        accum.arguments.push_str(args);
                        tx.send_blocking(StreamEvent::ToolcallDelta {
                            content_index: accum.content_index,
                            delta: args.clone(),
                            partial: output.clone(),
                        })
                        .map_err(|_| crate::Error::ChannelClosed)?;
                    }
                }
            }
        }
    }

    // Close any open blocks
    if thinking_started {
        let ci = output.content.len() - 1;
        if let Some(AssistantContent::Thinking(t)) = output.content.get(ci) {
            tx.send_blocking(StreamEvent::ThinkingEnd {
                content_index: ci,
                content: t.thinking.clone(),
                partial: output.clone(),
            })
            .map_err(|_| crate::Error::ChannelClosed)?;
        }
    }
    if text_started {
        let ci = output.content.len() - 1;
        if let Some(AssistantContent::Text(t)) = output.content.get(ci) {
            tx.send_blocking(StreamEvent::TextEnd {
                content_index: ci,
                content: t.text.clone(),
                partial: output.clone(),
            })
            .map_err(|_| crate::Error::ChannelClosed)?;
        }
    }

    // Finalize tool calls
    for accum in &tool_accums {
        let args: serde_json::Value = serde_json::from_str(&accum.arguments)
            .unwrap_or(serde_json::Value::Object(Default::default()));
        if let Some(AssistantContent::ToolCall(tc)) = output.content.get_mut(accum.content_index) {
            tc.id = accum.id.clone();
            tc.name = accum.name.clone();
            tc.arguments = args;
        }
        tx.send_blocking(StreamEvent::ToolcallEnd {
            content_index: accum.content_index,
            tool_call: ToolCall {
                id: accum.id.clone(),
                name: accum.name.clone(),
                arguments: serde_json::from_str(&accum.arguments)
                    .unwrap_or(serde_json::Value::Object(Default::default())),
            },
            partial: output.clone(),
        })
        .map_err(|_| crate::Error::ChannelClosed)?;
    }

    tx.send_blocking(StreamEvent::Done {
        reason: output.stop_reason,
        message: output,
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
) -> crate::Result<openai_types::ChatCompletionRequest> {
    let mut messages = Vec::new();

    // System prompt as system message
    if let Some(ref prompt) = context.system_prompt {
        messages.push(openai_types::ChatMessage {
            role: "system".into(),
            content: Some(serde_json::Value::String(prompt.clone())),
            tool_calls: None,
            tool_call_id: None,
            name: None,
        });
    }

    for msg in &context.messages {
        match msg {
            Message::User(u) => {
                let content = convert_user_content(&u.content);
                messages.push(openai_types::ChatMessage {
                    role: "user".into(),
                    content: Some(content),
                    tool_calls: None,
                    tool_call_id: None,
                    name: None,
                });
            }
            Message::Assistant(a) => {
                let (content, tool_calls) = convert_assistant_to_openai(a);
                messages.push(openai_types::ChatMessage {
                    role: "assistant".into(),
                    content,
                    tool_calls,
                    tool_call_id: None,
                    name: None,
                });
            }
            Message::ToolResult(tr) => {
                let text = tr
                    .content
                    .iter()
                    .filter_map(|c| match c {
                        ToolResultContent::Text(t) => Some(t.text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                messages.push(openai_types::ChatMessage {
                    role: "tool".into(),
                    content: Some(serde_json::Value::String(text)),
                    tool_calls: None,
                    tool_call_id: Some(tr.tool_call_id.clone()),
                    name: Some(tr.tool_name.clone()),
                });
            }
            Message::CompactionSummary(cs) => {
                let text = format!(
                    "[Context compacted — {} tokens before compaction]\n\n{}",
                    cs.tokens_before, cs.summary
                );
                messages.push(openai_types::ChatMessage {
                    role: "user".into(),
                    content: Some(serde_json::Value::String(text)),
                    tool_calls: None,
                    tool_call_id: None,
                    name: None,
                });
            }
        }
    }

    let max_tokens = options
        .max_tokens
        .unwrap_or((model.max_tokens / 3).max(1024));

    let tools = if context.tools.is_empty() {
        None
    } else {
        Some(
            context
                .tools
                .iter()
                .map(|t| openai_types::ToolDef {
                    tool_type: "function",
                    function: openai_types::ToolDefFunction {
                        name: t.name.clone(),
                        description: t.description.clone(),
                        parameters: t.parameters.clone(),
                    },
                })
                .collect(),
        )
    };

    // Thinking style
    let (reasoning_effort, enable_thinking) = match model.thinking {
        ThinkingStyle::OpenAi => (Some("medium".to_string()), None),
        ThinkingStyle::Qwen => (None, Some(true)),
        _ => (None, None),
    };

    Ok(openai_types::ChatCompletionRequest {
        model: model.id.clone(),
        messages,
        max_completion_tokens: Some(max_tokens),
        temperature: options.temperature,
        tools,
        stream: true,
        stream_options: Some(openai_types::StreamOptions {
            include_usage: true,
        }),
        reasoning_effort,
        enable_thinking,
    })
}

// ---------------------------------------------------------------------------
// Content conversion
// ---------------------------------------------------------------------------

fn convert_user_content(content: &[UserContent]) -> serde_json::Value {
    if content.len() == 1
        && let UserContent::Text(t) = &content[0]
    {
        return serde_json::Value::String(t.text.clone());
    }
    let parts: Vec<serde_json::Value> = content
        .iter()
        .map(|c| match c {
            UserContent::Text(t) => serde_json::json!({"type": "text", "text": t.text}),
            UserContent::Image(img) => serde_json::json!({
                "type": "image_url",
                "image_url": {
                    "url": format!("data:{};base64,{}", img.mime_type, img.data),
                }
            }),
        })
        .collect();
    serde_json::Value::Array(parts)
}

fn convert_assistant_to_openai(
    a: &AssistantMessage,
) -> (
    Option<serde_json::Value>,
    Option<Vec<openai_types::ToolCallMessage>>,
) {
    let mut text_parts = Vec::new();
    let mut tool_calls = Vec::new();

    for c in &a.content {
        match c {
            AssistantContent::Text(t) if !t.text.is_empty() => {
                text_parts.push(t.text.as_str());
            }
            AssistantContent::Thinking(t) if !t.thinking.is_empty() => {
                // Thinking blocks don't have a standard OpenAI representation.
                // Include as text so the model sees its prior reasoning.
                text_parts.push(t.thinking.as_str());
            }
            AssistantContent::ToolCall(tc) => {
                tool_calls.push(openai_types::ToolCallMessage {
                    id: tc.id.clone(),
                    call_type: "function".into(),
                    function: openai_types::ToolCallFunction {
                        name: tc.name.clone(),
                        arguments: serde_json::to_string(&tc.arguments).unwrap_or_default(),
                    },
                });
            }
            _ => {}
        }
    }

    let content = if text_parts.is_empty() {
        None
    } else {
        Some(serde_json::Value::String(text_parts.join("")))
    };
    let tcs = if tool_calls.is_empty() {
        None
    } else {
        Some(tool_calls)
    };
    (content, tcs)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn map_finish_reason(reason: &str) -> StopReason {
    match reason {
        "stop" => StopReason::Stop,
        "length" => StopReason::Length,
        "tool_calls" => StopReason::ToolUse,
        "content_filter" => StopReason::Error,
        _ => StopReason::Stop,
    }
}
