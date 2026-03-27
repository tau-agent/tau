use std::io::{BufRead, BufReader};

use async_trait::async_trait;

use super::anthropic_types::*;
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
fn run_stream(ctx: &StreamCtx<'_>, body: &MessagesRequest, tx: &EventSender) -> crate::Result<()> {
    let url = format!("{}/v1/messages", ctx.base_url.trim_end_matches('/'));

    let is_oauth = crate::auth::is_oauth_token(ctx.api_key);
    let mut req = ureq::post(&url)
        .header("content-type", "application/json")
        .header("anthropic-version", "2023-06-01")
        .header("accept", "application/json");

    if is_oauth {
        req = req
            .header("authorization", &format!("Bearer {}", ctx.api_key))
            .header(
                "anthropic-beta",
                "claude-code-20250219,oauth-2025-04-20,fine-grained-tool-streaming-2025-05-14",
            )
            .header("user-agent", "claude-cli/2.1.75")
            .header("x-app", "cli")
            .header("anthropic-dangerous-direct-browser-access", "true");
    } else {
        req = req
            .header("x-api-key", ctx.api_key)
            .header("anthropic-beta", "fine-grained-tool-streaming-2025-05-14");
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

        match current_event_type.as_str() {
            "message_start" => {
                let ev: MessageStartEvent =
                    serde_json::from_str(data).map_err(|e| crate::Error::Parse(e.to_string()))?;
                output.response_id = Some(ev.message.id);
                if let Some(usage) = ev.message.usage {
                    usage.apply_to(&mut output.usage);
                    ctx.model.calculate_cost(&mut output.usage);
                }
            }
            "content_block_start" => {
                let ev: ContentBlockStartEvent =
                    serde_json::from_str(data).map_err(|e| crate::Error::Parse(e.to_string()))?;

                match ev.content_block {
                    ContentBlock::Text { .. } => {
                        output.content.push(AssistantContent::Text(TextContent {
                            text: String::new(),
                            text_signature: None,
                        }));
                        let ci = output.content.len() - 1;
                        block_index_map.push((ev.index, ci));
                        tx.send_blocking(StreamEvent::TextStart {
                            content_index: ci,
                            partial: output.clone(),
                        })
                        .map_err(|_| crate::Error::ChannelClosed)?;
                    }
                    ContentBlock::Thinking { .. } => {
                        output
                            .content
                            .push(AssistantContent::Thinking(ThinkingContent {
                                thinking: String::new(),
                                thinking_signature: None,
                                redacted: false,
                            }));
                        let ci = output.content.len() - 1;
                        block_index_map.push((ev.index, ci));
                        tx.send_blocking(StreamEvent::ThinkingStart {
                            content_index: ci,
                            partial: output.clone(),
                        })
                        .map_err(|_| crate::Error::ChannelClosed)?;
                    }
                    ContentBlock::RedactedThinking { data: sig } => {
                        output
                            .content
                            .push(AssistantContent::Thinking(ThinkingContent {
                                thinking: "[Reasoning redacted]".into(),
                                thinking_signature: Some(sig),
                                redacted: true,
                            }));
                        let ci = output.content.len() - 1;
                        block_index_map.push((ev.index, ci));
                        tx.send_blocking(StreamEvent::ThinkingStart {
                            content_index: ci,
                            partial: output.clone(),
                        })
                        .map_err(|_| crate::Error::ChannelClosed)?;
                    }
                    ContentBlock::ToolUse { id, name, .. } => {
                        output.content.push(AssistantContent::ToolCall(ToolCall {
                            id,
                            name,
                            arguments: serde_json::Value::Object(Default::default()),
                        }));
                        let ci = output.content.len() - 1;
                        block_index_map.push((ev.index, ci));
                        tx.send_blocking(StreamEvent::ToolcallStart {
                            content_index: ci,
                            partial: output.clone(),
                        })
                        .map_err(|_| crate::Error::ChannelClosed)?;
                    }
                }
            }
            "content_block_delta" => {
                let ev: ContentBlockDeltaEvent =
                    serde_json::from_str(data).map_err(|e| crate::Error::Parse(e.to_string()))?;
                let ci = block_index_map
                    .iter()
                    .find(|(bi, _)| *bi == ev.index)
                    .map(|(_, ci)| *ci);
                let Some(ci) = ci else { continue };

                match ev.delta {
                    Delta::TextDelta { text } => {
                        if let Some(AssistantContent::Text(t)) = output.content.get_mut(ci) {
                            t.text.push_str(&text);
                        }
                        tx.send_blocking(StreamEvent::TextDelta {
                            content_index: ci,
                            delta: text,
                            partial: output.clone(),
                        })
                        .map_err(|_| crate::Error::ChannelClosed)?;
                    }
                    Delta::ThinkingDelta { thinking } => {
                        if let Some(AssistantContent::Thinking(t)) = output.content.get_mut(ci) {
                            t.thinking.push_str(&thinking);
                        }
                        tx.send_blocking(StreamEvent::ThinkingDelta {
                            content_index: ci,
                            delta: thinking,
                            partial: output.clone(),
                        })
                        .map_err(|_| crate::Error::ChannelClosed)?;
                    }
                    Delta::InputJsonDelta { partial_json } => {
                        tx.send_blocking(StreamEvent::ToolcallDelta {
                            content_index: ci,
                            delta: partial_json,
                            partial: output.clone(),
                        })
                        .map_err(|_| crate::Error::ChannelClosed)?;
                    }
                    Delta::SignatureDelta { signature } => {
                        if let Some(AssistantContent::Thinking(t)) = output.content.get_mut(ci) {
                            let s = t.thinking_signature.get_or_insert_with(String::new);
                            s.push_str(&signature);
                        }
                    }
                }
            }
            "content_block_stop" => {
                let ev: ContentBlockStopEvent =
                    serde_json::from_str(data).map_err(|e| crate::Error::Parse(e.to_string()))?;
                let ci = block_index_map
                    .iter()
                    .find(|(bi, _)| *bi == ev.index)
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
                let ev: MessageDeltaEvent =
                    serde_json::from_str(data).map_err(|e| crate::Error::Parse(e.to_string()))?;
                if let Some(delta) = ev.delta
                    && let Some(reason) = delta.stop_reason
                {
                    output.stop_reason = map_stop_reason(&reason);
                }
                if let Some(usage) = ev.usage {
                    usage.apply_to(&mut output.usage);
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
) -> crate::Result<MessagesRequest> {
    let mut messages = Vec::new();

    for msg in &context.messages {
        match msg {
            Message::User(u) => {
                messages.push(ApiMessage {
                    role: "user",
                    content: convert_user_content(&u.content),
                });
            }
            Message::Assistant(a) => {
                let content = convert_assistant_content(&a.content);
                if !content.is_empty() {
                    messages.push(ApiMessage {
                        role: "assistant",
                        content: serde_json::Value::Array(content),
                    });
                }
            }
            Message::ToolResult(tr) => {
                let content = convert_tool_result_content(&tr.content);
                messages.push(ApiMessage {
                    role: "user",
                    content: serde_json::json!([{
                        "type": "tool_result",
                        "tool_use_id": tr.tool_call_id,
                        "content": content,
                        "is_error": tr.is_error,
                    }]),
                });
            }
        }
    }

    // Add cache breakpoint to the last user message
    add_cache_breakpoint_to_last_user_message(&mut messages);

    let max_tokens = options
        .max_tokens
        .unwrap_or((model.max_tokens / 3).max(1024));

    let is_oauth = options
        .api_key
        .as_deref()
        .map(crate::auth::is_oauth_token)
        .unwrap_or(false);

    let cc = Some(CacheControl::ephemeral());

    let system = if is_oauth {
        let mut blocks = vec![SystemBlock {
            block_type: "text",
            text: "You are Claude Code, Anthropic's official CLI for Claude.".into(),
            cache_control: cc.clone(),
        }];
        if let Some(ref prompt) = context.system_prompt {
            blocks.push(SystemBlock {
                block_type: "text",
                text: prompt.clone(),
                cache_control: cc.clone(),
            });
        }
        Some(blocks)
    } else {
        context.system_prompt.as_ref().map(|prompt| {
            vec![SystemBlock {
                block_type: "text",
                text: prompt.clone(),
                cache_control: cc.clone(),
            }]
        })
    };

    let tools = if context.tools.is_empty() {
        None
    } else {
        Some(
            context
                .tools
                .iter()
                .map(|t| ToolDef {
                    name: t.name.clone(),
                    description: t.description.clone(),
                    input_schema: t.parameters.clone(),
                })
                .collect(),
        )
    };

    let thinking = options.thinking_budget.map(|budget| ThinkingConfig {
        thinking_type: "enabled",
        budget_tokens: budget,
    });

    Ok(MessagesRequest {
        model: model.id.clone(),
        messages: messages
            .into_iter()
            .map(|m| ApiMessage {
                role: m.role,
                content: m.content,
            })
            .collect(),
        max_tokens,
        stream: true,
        system,
        temperature: options.temperature,
        tools,
        thinking,
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

/// Add `cache_control` to the last content block of the last user message.
fn add_cache_breakpoint_to_last_user_message(messages: &mut [ApiMessage]) {
    let Some(last_user) = messages.iter_mut().rev().find(|m| m.role == "user") else {
        return;
    };

    match &mut last_user.content {
        serde_json::Value::String(text) => {
            let text = text.clone();
            last_user.content = serde_json::json!([{
                "type": "text",
                "text": text,
                "cache_control": {"type": "ephemeral"},
            }]);
        }
        serde_json::Value::Array(blocks) => {
            if let Some(last_block) = blocks.last_mut()
                && let Some(obj) = last_block.as_object_mut()
            {
                obj.insert(
                    "cache_control".into(),
                    serde_json::json!({"type": "ephemeral"}),
                );
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

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
            id: "claude-sonnet-4-6".into(),
            name: "Claude Sonnet 4.6".into(),
            api: API_ID.into(),
            provider: "anthropic".into(),
            base_url: DEFAULT_BASE_URL.into(),
            thinking: ThinkingStyle::Anthropic,
            cost: ModelCost {
                input: 3.0,
                output: 15.0,
                cache_read: 0.3,
                cache_write: 3.75,
            },
            context_window: 1_000_000,
            max_tokens: 64_000,
            headers: Default::default(),
        },
        Model {
            id: "claude-opus-4-6".into(),
            name: "Claude Opus 4.6".into(),
            api: API_ID.into(),
            provider: "anthropic".into(),
            base_url: DEFAULT_BASE_URL.into(),
            thinking: ThinkingStyle::Anthropic,
            cost: ModelCost {
                input: 5.0,
                output: 25.0,
                cache_read: 0.5,
                cache_write: 6.25,
            },
            context_window: 1_000_000,
            max_tokens: 128_000,
            headers: Default::default(),
        },
        Model {
            id: "claude-sonnet-4-5".into(),
            name: "Claude Sonnet 4.5".into(),
            api: API_ID.into(),
            provider: "anthropic".into(),
            base_url: DEFAULT_BASE_URL.into(),
            thinking: ThinkingStyle::Anthropic,
            cost: ModelCost {
                input: 3.0,
                output: 15.0,
                cache_read: 0.3,
                cache_write: 3.75,
            },
            context_window: 200_000,
            max_tokens: 64_000,
            headers: Default::default(),
        },
        Model {
            id: "claude-opus-4-5".into(),
            name: "Claude Opus 4.5".into(),
            api: API_ID.into(),
            provider: "anthropic".into(),
            base_url: DEFAULT_BASE_URL.into(),
            thinking: ThinkingStyle::Anthropic,
            cost: ModelCost {
                input: 5.0,
                output: 25.0,
                cache_read: 0.5,
                cache_write: 6.25,
            },
            context_window: 200_000,
            max_tokens: 64_000,
            headers: Default::default(),
        },
        Model {
            id: "claude-sonnet-4-20250514".into(),
            name: "Claude Sonnet 4".into(),
            api: API_ID.into(),
            provider: "anthropic".into(),
            base_url: DEFAULT_BASE_URL.into(),
            thinking: ThinkingStyle::Anthropic,
            cost: ModelCost {
                input: 3.0,
                output: 15.0,
                cache_read: 0.3,
                cache_write: 3.75,
            },
            context_window: 200_000,
            max_tokens: 64_000,
            headers: Default::default(),
        },
        Model {
            id: "claude-haiku-4-5".into(),
            name: "Claude Haiku 4.5".into(),
            api: API_ID.into(),
            provider: "anthropic".into(),
            base_url: DEFAULT_BASE_URL.into(),
            thinking: ThinkingStyle::Anthropic,
            cost: ModelCost {
                input: 1.0,
                output: 5.0,
                cache_read: 0.1,
                cache_write: 1.25,
            },
            context_window: 200_000,
            max_tokens: 64_000,
            headers: Default::default(),
        },
    ]
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn build(context: &Context, options: &StreamOptions) -> serde_json::Value {
        let model = models().into_iter().next().unwrap();
        let req = build_request_body(&model, context, options).unwrap();
        serde_json::to_value(req).unwrap()
    }

    fn simple_context(system: Option<&str>, user_text: &str) -> Context {
        Context {
            system_prompt: system.map(String::from),
            messages: vec![Message::User(UserMessage::text(user_text))],
            tools: Vec::new(),
        }
    }

    #[test]
    fn system_prompt_has_cache_control_non_oauth() {
        let body = build(
            &simple_context(Some("Be helpful."), "hi"),
            &StreamOptions::default(),
        );
        let blocks = body["system"].as_array().unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["text"], "Be helpful.");
        assert_eq!(blocks[0]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn system_prompt_has_cache_control_oauth() {
        let opts = StreamOptions {
            api_key: Some("sk-ant-oat-fake-token".into()),
            ..Default::default()
        };
        let body = build(&simple_context(Some("Be helpful."), "hi"), &opts);
        let blocks = body["system"].as_array().unwrap();
        assert_eq!(blocks.len(), 2);
        assert!(blocks[0]["text"].as_str().unwrap().contains("Claude Code"));
        assert_eq!(blocks[0]["cache_control"]["type"], "ephemeral");
        assert_eq!(blocks[1]["text"], "Be helpful.");
        assert_eq!(blocks[1]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn last_user_message_gets_cache_breakpoint_string() {
        let body = build(
            &simple_context(None, "hello world"),
            &StreamOptions::default(),
        );
        let messages = body["messages"].as_array().unwrap();
        let last = messages.last().unwrap();
        let content = last["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["text"], "hello world");
        assert_eq!(content[0]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn last_user_message_gets_cache_breakpoint_multi_turn() {
        let ctx = Context {
            system_prompt: None,
            messages: vec![
                Message::User(UserMessage::text("first")),
                Message::Assistant(AssistantMessage::empty(
                    "anthropic-messages",
                    "anthropic",
                    "claude-sonnet-4-6",
                )),
                Message::User(UserMessage::text("second")),
            ],
            tools: Vec::new(),
        };
        let body = build(&ctx, &StreamOptions::default());
        let messages = body["messages"].as_array().unwrap();

        let first_content = &messages[0]["content"];
        if let Some(arr) = first_content.as_array() {
            for block in arr {
                assert!(block.get("cache_control").is_none());
            }
        } else {
            assert!(first_content.is_string());
        }

        let last = messages.last().unwrap();
        let content = last["content"].as_array().unwrap();
        assert_eq!(
            content.last().unwrap()["cache_control"]["type"],
            "ephemeral"
        );
    }

    #[test]
    fn tool_result_message_gets_cache_breakpoint() {
        let ctx = Context {
            system_prompt: None,
            messages: vec![
                Message::User(UserMessage::text("use the tool")),
                Message::Assistant({
                    let mut a = AssistantMessage::empty(
                        "anthropic-messages",
                        "anthropic",
                        "claude-sonnet-4-6",
                    );
                    a.content.push(AssistantContent::ToolCall(ToolCall {
                        id: "tc1".into(),
                        name: "bash".into(),
                        arguments: serde_json::json!({"cmd": "ls"}),
                    }));
                    a
                }),
                Message::ToolResult(ToolResultMessage {
                    tool_call_id: "tc1".into(),
                    tool_name: "bash".into(),
                    content: vec![ToolResultContent::Text(TextContent {
                        text: "file1 file2".into(),
                        text_signature: None,
                    })],
                    details: None,
                    is_error: false,
                    timestamp: 0,
                }),
            ],
            tools: Vec::new(),
        };
        let body = build(&ctx, &StreamOptions::default());
        let messages = body["messages"].as_array().unwrap();

        let last_user = messages.iter().rev().find(|m| m["role"] == "user").unwrap();
        let content = last_user["content"].as_array().unwrap();
        let last_block = content.last().unwrap();
        assert_eq!(last_block["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn no_system_prompt_omits_system_field() {
        let body = build(&simple_context(None, "hi"), &StreamOptions::default());
        // system: None serializes as null with skip_serializing_if
        let system = body.get("system");
        assert!(
            system.is_none() || system.unwrap().is_null(),
            "system should be absent or null"
        );
    }
}
