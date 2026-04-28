//! OpenAI Chat Completions API provider.
//!
//! Also used for OpenAI-compatible APIs (Qwen, local models, etc.)
//! via different base_url and model settings.

use std::io::BufRead;

use async_trait::async_trait;

use super::common::{self, PreparedStream, StreamCtx, open_sse_stream, send_event};
use super::openai_types;
use crate::provider::{EventReceiver, EventSender, Provider};
use tau_agent_base::types::*;

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
    ) -> tau_agent_base::Result<EventReceiver> {
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
        let recv_response_timeout = common::recv_timeout_for(model, options);

        std::thread::spawn(move || {
            let ctx = StreamCtx {
                base_url: &base_url,
                api_key: &api_key,
                api_id: &api_id,
                provider_name: &provider_name,
                model_id: &model_id,
                model: &model_clone,
                recv_response_timeout,
            };
            let result = run_stream(&ctx, &body, &tx);
            if let Err(e) = result {
                let error_message = match &e {
                    tau_agent_base::Error::HttpStatus {
                        status,
                        message,
                        retry_after,
                    } => {
                        let mut msg = format!("HTTP {}: {}", status, message);
                        if let Some(ra) = retry_after {
                            msg.push_str(&format!(" [retry-after: {}s]", ra));
                        }
                        msg
                    }
                    other => other.to_string(),
                };
                let mut msg = AssistantMessage::empty(&api_id, &provider_name, &model_id);
                msg.stop_reason = StopReason::Error;
                msg.error_message = Some(error_message);
                let _ = tx.send_blocking(StreamEvent::Error {
                    reason: StopReason::Error,
                    error: msg,
                });
            }
        });

        Ok(rx)
    }
}

// ---------------------------------------------------------------------------
// SSE stream processing
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_lines)]
fn run_stream(
    ctx: &StreamCtx<'_>,
    body: &openai_types::ChatCompletionRequest,
    tx: &EventSender,
) -> tau_agent_base::Result<()> {
    let url = format!("{}/chat/completions", ctx.base_url.trim_end_matches('/'));

    // Local must outlive the &str borrow in `extra_headers`.
    let bearer;
    let mut extra_headers: Vec<(&str, &str)> = Vec::with_capacity(2);
    extra_headers.push(("accept", "text/event-stream"));
    if !ctx.api_key.is_empty() {
        bearer = format!("Bearer {}", ctx.api_key);
        extra_headers.push(("authorization", bearer.as_str()));
    }

    let PreparedStream {
        body_reader,
        initial_message: mut output,
    } = open_sse_stream(ctx, &url, &extra_headers, body, tx)?;
    let reader = body_reader;

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
        let line = line.map_err(|e: std::io::Error| tau_agent_base::Error::Http(e.to_string()))?;

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
            serde_json::from_str(data).map_err(|e| tau_agent_base::Error::Parse(e.to_string()))?;

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
                send_event(
                    tx,
                    StreamEvent::ThinkingStart {
                        content_index: output.content.len() - 1,
                        partial: output.clone(),
                    },
                )?;
            }
            let ci = output.content.len() - 1;
            if let Some(AssistantContent::Thinking(t)) = output.content.get_mut(ci) {
                t.thinking.push_str(thinking);
            }
            send_event(
                tx,
                StreamEvent::ThinkingDelta {
                    content_index: ci,
                    delta: thinking.clone(),
                    partial: output.clone(),
                },
            )?;
        }

        // Text content
        if let Some(ref text) = delta.content {
            // Close thinking block if we were in one
            if thinking_started {
                let ci = output.content.len() - 1;
                if let Some(AssistantContent::Thinking(t)) = output.content.get(ci) {
                    send_event(
                        tx,
                        StreamEvent::ThinkingEnd {
                            content_index: ci,
                            content: t.thinking.clone(),
                            partial: output.clone(),
                        },
                    )?;
                }
                thinking_started = false;
            }

            if !text_started {
                output.content.push(AssistantContent::Text(TextContent {
                    text: String::new(),
                    text_signature: None,
                }));
                text_started = true;
                send_event(
                    tx,
                    StreamEvent::TextStart {
                        content_index: output.content.len() - 1,
                        partial: output.clone(),
                    },
                )?;
            }
            let ci = output.content.len() - 1;
            if let Some(AssistantContent::Text(t)) = output.content.get_mut(ci) {
                t.text.push_str(text);
            }
            send_event(
                tx,
                StreamEvent::TextDelta {
                    content_index: ci,
                    delta: text.clone(),
                    partial: output.clone(),
                },
            )?;
        }

        // Tool calls
        if let Some(ref tool_calls) = delta.tool_calls {
            // Close text block if open
            if text_started {
                let ci = output.content.len() - 1;
                if let Some(AssistantContent::Text(t)) = output.content.get(ci) {
                    send_event(
                        tx,
                        StreamEvent::TextEnd {
                            content_index: ci,
                            content: t.text.clone(),
                            partial: output.clone(),
                        },
                    )?;
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
                    send_event(
                        tx,
                        StreamEvent::ToolcallStart {
                            content_index: ci,
                            partial: output.clone(),
                        },
                    )?;
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
                        send_event(
                            tx,
                            StreamEvent::ToolcallDelta {
                                content_index: accum.content_index,
                                delta: args.clone(),
                                partial: output.clone(),
                            },
                        )?;
                    }
                }
            }
        }
    }

    // Close any open blocks
    if thinking_started {
        let ci = output.content.len() - 1;
        if let Some(AssistantContent::Thinking(t)) = output.content.get(ci) {
            send_event(
                tx,
                StreamEvent::ThinkingEnd {
                    content_index: ci,
                    content: t.thinking.clone(),
                    partial: output.clone(),
                },
            )?;
        }
    }
    if text_started {
        let ci = output.content.len() - 1;
        if let Some(AssistantContent::Text(t)) = output.content.get(ci) {
            send_event(
                tx,
                StreamEvent::TextEnd {
                    content_index: ci,
                    content: t.text.clone(),
                    partial: output.clone(),
                },
            )?;
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
        send_event(
            tx,
            StreamEvent::ToolcallEnd {
                content_index: accum.content_index,
                tool_call: ToolCall {
                    id: accum.id.clone(),
                    name: accum.name.clone(),
                    arguments: serde_json::from_str(&accum.arguments)
                        .unwrap_or(serde_json::Value::Object(Default::default())),
                },
                partial: output.clone(),
            },
        )?;
    }

    send_event(
        tx,
        StreamEvent::Done {
            reason: output.stop_reason,
            message: output,
        },
    )?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Request building
// ---------------------------------------------------------------------------

fn build_request_body(
    model: &Model,
    context: &Context,
    options: &StreamOptions,
) -> tau_agent_base::Result<openai_types::ChatCompletionRequest> {
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
            Message::Info(_) => {
                // Info messages are display-only; not sent to the LLM.
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

    // Prompt caching: only send to api.openai.com directly. OpenAI-compatible
    // backends (LiteLLM, Anthropic-via-proxy, Groq, OpenRouter, …) reject
    // these fields. Mirrors pi-ai upstream.
    let is_openai = model.base_url.contains("api.openai.com");
    let retention = CacheRetention::resolve_with_env(options.cache_retention);
    let prompt_cache_key = if is_openai && retention != CacheRetention::None {
        options.session_id.clone()
    } else {
        None
    };
    let prompt_cache_retention = if is_openai && retention == CacheRetention::Long {
        Some("24h")
    } else {
        None
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
        prompt_cache_key,
        prompt_cache_retention,
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn test_model(base_url: &str) -> Model {
        Model {
            id: "gpt-test".into(),
            name: "GPT Test".into(),
            api: API_ID.into(),
            provider: "openai".into(),
            base_url: base_url.into(),
            thinking: ThinkingStyle::None,
            cost: ModelCost::default(),
            context_window: 128_000,
            max_tokens: 16_000,
            headers: Default::default(),
        }
    }

    fn ctx() -> Context {
        Context {
            system_prompt: None,
            messages: vec![Message::User(UserMessage::text("hi"))],
            tools: Vec::new(),
        }
    }

    fn build(base_url: &str, options: &StreamOptions) -> serde_json::Value {
        let model = test_model(base_url);
        let req = build_request_body(&model, &ctx(), options).expect("build_request_body");
        serde_json::to_value(req).expect("serialize")
    }

    #[test]
    fn prompt_cache_key_set_for_openai_with_session_id() {
        let opts = StreamOptions {
            session_id: Some("sess-abc".into()),
            ..Default::default()
        };
        let body = build("https://api.openai.com/v1", &opts);
        assert_eq!(body["prompt_cache_key"], "sess-abc");
    }

    #[test]
    fn prompt_cache_key_omitted_for_compatible_backend() {
        let opts = StreamOptions {
            session_id: Some("sess-abc".into()),
            cache_retention: Some(CacheRetention::Long),
            ..Default::default()
        };
        let body = build("https://api.litellm.example/v1", &opts);
        assert!(
            body.get("prompt_cache_key").is_none(),
            "non-OpenAI backends must not receive prompt_cache_key, got {body}"
        );
        assert!(
            body.get("prompt_cache_retention").is_none(),
            "non-OpenAI backends must not receive prompt_cache_retention, got {body}"
        );
    }

    #[test]
    fn prompt_cache_retention_long_emits_24h() {
        let opts = StreamOptions {
            session_id: Some("sess-abc".into()),
            cache_retention: Some(CacheRetention::Long),
            ..Default::default()
        };
        let body = build("https://api.openai.com/v1", &opts);
        assert_eq!(body["prompt_cache_retention"], "24h");
        assert_eq!(body["prompt_cache_key"], "sess-abc");
    }

    #[test]
    fn prompt_cache_retention_short_omits_field() {
        let opts = StreamOptions {
            session_id: Some("sess-abc".into()),
            cache_retention: Some(CacheRetention::Short),
            ..Default::default()
        };
        let body = build("https://api.openai.com/v1", &opts);
        assert!(
            body.get("prompt_cache_retention").is_none(),
            "Short retention should omit prompt_cache_retention, got {body}"
        );
        // session_id is still wired through as the cache key.
        assert_eq!(body["prompt_cache_key"], "sess-abc");
    }

    #[test]
    fn cache_retention_none_disables_key() {
        let opts = StreamOptions {
            session_id: Some("sess-abc".into()),
            cache_retention: Some(CacheRetention::None),
            ..Default::default()
        };
        let body = build("https://api.openai.com/v1", &opts);
        assert!(
            body.get("prompt_cache_key").is_none(),
            "CacheRetention::None must suppress prompt_cache_key, got {body}"
        );
        assert!(
            body.get("prompt_cache_retention").is_none(),
            "CacheRetention::None must suppress prompt_cache_retention, got {body}"
        );
    }

    #[test]
    fn prompt_cache_key_omitted_without_session_id() {
        // Default options on api.openai.com: no session_id supplied, so no
        // cache key (but no retention either since retention defaults to Short).
        let body = build("https://api.openai.com/v1", &StreamOptions::default());
        assert!(
            body.get("prompt_cache_key").is_none(),
            "no session_id means no prompt_cache_key, got {body}"
        );
        assert!(
            body.get("prompt_cache_retention").is_none(),
            "default retention is Short -> no prompt_cache_retention, got {body}"
        );
    }
}
