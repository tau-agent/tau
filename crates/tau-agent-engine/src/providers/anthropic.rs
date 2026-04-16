use std::io::{BufRead, BufReader};

use async_trait::async_trait;

use super::anthropic_types::*;
use super::common::{self, StreamCtx, send_event};
use crate::provider::{EventReceiver, EventSender, Provider};
use tau_agent_base::types::*;

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
    ) -> tau_agent_base::Result<EventReceiver> {
        let (tx, rx) = smol::channel::unbounded();

        let body = build_request_body(model, context, options)?;
        let base_url = model.base_url.clone();
        let api_key = options
            .api_key
            .clone()
            .or_else(|| std::env::var("ANTHROPIC_API_KEY").ok())
            .ok_or_else(|| tau_agent_base::Error::NoApiKey("anthropic".into()))?;
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
                // For HTTP status errors, include the structured info in the error message
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
    body: &MessagesRequest,
    tx: &EventSender,
) -> tau_agent_base::Result<()> {
    let url = format!("{}/v1/messages", ctx.base_url.trim_end_matches('/'));

    let is_oauth = tau_agent_base::subscription_usage::is_oauth_token(ctx.api_key);
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
        .config()
        .timeout_connect(Some(common::TIMEOUT_CONNECT))
        .timeout_send_request(Some(common::TIMEOUT_SEND_REQUEST))
        .timeout_send_body(Some(common::TIMEOUT_SEND_BODY))
        .timeout_recv_response(Some(common::TIMEOUT_RECV_RESPONSE))
        .http_status_as_error(false)
        .build()
        .send_json(body)
        .map_err(|e| tau_agent_base::Error::Http(e.to_string()))?;

    let status = resp.status().as_u16();
    if status >= 400 {
        let retry_after = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok());
        use std::io::Read;
        let mut body_text = String::new();
        let _ = resp.body_mut().as_reader().read_to_string(&mut body_text);
        return Err(tau_agent_base::Error::HttpStatus {
            status,
            message: body_text,
            retry_after,
        });
    }

    let reader = BufReader::new(resp.body_mut().as_reader());
    let mut output = AssistantMessage::empty(ctx.api_id, ctx.provider_name, ctx.model_id);
    send_event(
        tx,
        StreamEvent::Start {
            partial: output.clone(),
        },
    )?;

    let mut block_index_map: Vec<(u64, usize)> = Vec::new();
    let mut current_event_type = String::new();
    // Accumulate partial JSON for tool call arguments (keyed by block index)
    let mut tool_json_accum: std::collections::HashMap<u64, String> =
        std::collections::HashMap::new();

    for line in reader.lines() {
        let line = line.map_err(|e: std::io::Error| tau_agent_base::Error::Http(e.to_string()))?;

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
                let ev: MessageStartEvent = serde_json::from_str(data)
                    .map_err(|e| tau_agent_base::Error::Parse(e.to_string()))?;
                output.response_id = Some(ev.message.id);
                if let Some(usage) = ev.message.usage {
                    usage.apply_to(&mut output.usage);
                    ctx.model.calculate_cost(&mut output.usage);
                }
            }
            "content_block_start" => {
                let ev: ContentBlockStartEvent = serde_json::from_str(data)
                    .map_err(|e| tau_agent_base::Error::Parse(e.to_string()))?;

                match ev.content_block {
                    ContentBlock::Text { .. } => {
                        output.content.push(AssistantContent::Text(TextContent {
                            text: String::new(),
                            text_signature: None,
                        }));
                        let ci = output.content.len() - 1;
                        block_index_map.push((ev.index, ci));
                        send_event(
                            tx,
                            StreamEvent::TextStart {
                                content_index: ci,
                                partial: output.clone(),
                            },
                        )?;
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
                        send_event(
                            tx,
                            StreamEvent::ThinkingStart {
                                content_index: ci,
                                partial: output.clone(),
                            },
                        )?;
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
                        send_event(
                            tx,
                            StreamEvent::ThinkingStart {
                                content_index: ci,
                                partial: output.clone(),
                            },
                        )?;
                    }
                    ContentBlock::ToolUse { id, name, .. } => {
                        output.content.push(AssistantContent::ToolCall(ToolCall {
                            id,
                            name,
                            arguments: serde_json::Value::Object(Default::default()),
                        }));
                        let ci = output.content.len() - 1;
                        block_index_map.push((ev.index, ci));
                        send_event(
                            tx,
                            StreamEvent::ToolcallStart {
                                content_index: ci,
                                partial: output.clone(),
                            },
                        )?;
                    }
                }
            }
            "content_block_delta" => {
                let ev: ContentBlockDeltaEvent = serde_json::from_str(data)
                    .map_err(|e| tau_agent_base::Error::Parse(e.to_string()))?;
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
                        send_event(
                            tx,
                            StreamEvent::TextDelta {
                                content_index: ci,
                                delta: text,
                                partial: output.clone(),
                            },
                        )?;
                    }
                    Delta::ThinkingDelta { thinking } => {
                        if let Some(AssistantContent::Thinking(t)) = output.content.get_mut(ci) {
                            t.thinking.push_str(&thinking);
                        }
                        send_event(
                            tx,
                            StreamEvent::ThinkingDelta {
                                content_index: ci,
                                delta: thinking,
                                partial: output.clone(),
                            },
                        )?;
                    }
                    Delta::InputJsonDelta { partial_json } => {
                        tool_json_accum
                            .entry(ev.index)
                            .or_default()
                            .push_str(&partial_json);
                        send_event(
                            tx,
                            StreamEvent::ToolcallDelta {
                                content_index: ci,
                                delta: partial_json,
                                partial: output.clone(),
                            },
                        )?;
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
                let ev: ContentBlockStopEvent = serde_json::from_str(data)
                    .map_err(|e| tau_agent_base::Error::Parse(e.to_string()))?;
                let ci = block_index_map
                    .iter()
                    .find(|(bi, _)| *bi == ev.index)
                    .map(|(_, ci)| *ci);
                let Some(ci) = ci else { continue };

                match output.content.get(ci) {
                    Some(AssistantContent::Text(t)) => {
                        send_event(
                            tx,
                            StreamEvent::TextEnd {
                                content_index: ci,
                                content: t.text.clone(),
                                partial: output.clone(),
                            },
                        )?;
                    }
                    Some(AssistantContent::Thinking(t)) => {
                        send_event(
                            tx,
                            StreamEvent::ThinkingEnd {
                                content_index: ci,
                                content: t.thinking.clone(),
                                partial: output.clone(),
                            },
                        )?;
                    }
                    Some(AssistantContent::ToolCall(_)) => {
                        // Parse accumulated JSON into arguments
                        if let Some(json_str) = tool_json_accum.remove(&ev.index)
                            && let Ok(args) = serde_json::from_str(&json_str)
                            && let Some(AssistantContent::ToolCall(tc)) = output.content.get_mut(ci)
                        {
                            tc.arguments = args;
                        }
                        let tc = match output.content.get(ci) {
                            Some(AssistantContent::ToolCall(tc)) => tc.clone(),
                            _ => continue,
                        };
                        send_event(
                            tx,
                            StreamEvent::ToolcallEnd {
                                content_index: ci,
                                tool_call: tc,
                                partial: output.clone(),
                            },
                        )?;
                    }
                    None => {}
                }
            }
            "message_delta" => {
                let ev: MessageDeltaEvent = serde_json::from_str(data)
                    .map_err(|e| tau_agent_base::Error::Parse(e.to_string()))?;
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
                send_event(
                    tx,
                    StreamEvent::Done {
                        reason: output.stop_reason,
                        message: output,
                    },
                )?;
                return Ok(());
            }
            "error" => {
                // Anthropic SSE error event: {"type":"error","error":{"type":"...","message":"..."}}
                let error_msg = serde_json::from_str::<serde_json::Value>(data)
                    .ok()
                    .and_then(|v| {
                        v.get("error")
                            .and_then(|e| e.get("message"))
                            .and_then(|m| m.as_str())
                            .map(|s| s.to_string())
                    })
                    .unwrap_or_else(|| format!("SSE error: {}", data));
                // Drop any in-flight partial-JSON scratch buffers and reset the
                // arguments of any tool-call block that had not yet completed,
                // so the emitted error AssistantMessage never carries
                // stale/partial tool arguments. Mirrors pi-mono commit e2b40dfc.
                strip_partial_tool_state(&mut output, &mut tool_json_accum, &block_index_map);
                output.stop_reason = StopReason::Error;
                output.error_message = Some(error_msg);
                send_event(
                    tx,
                    StreamEvent::Error {
                        reason: StopReason::Error,
                        error: output,
                    },
                )?;
                return Ok(());
            }
            _ => {}
        }
    }

    // Premature stream close (no message_stop and no SSE error). Treat as an
    // error and scrub any partial-JSON scratch state so the emitted error
    // AssistantMessage never carries half-parsed tool arguments.
    strip_partial_tool_state(&mut output, &mut tool_json_accum, &block_index_map);
    output.stop_reason = StopReason::Error;
    output.error_message = Some("Stream ended unexpectedly".into());
    send_event(
        tx,
        StreamEvent::Error {
            reason: StopReason::Error,
            error: output,
        },
    )?;
    Ok(())
}

/// Drop the streaming partial-JSON scratch buffer and reset any tool-call
/// block whose arguments were still being accumulated to an empty object.
/// Used on error paths so callers replaying/persisting the error response
/// never see stale partial tool arguments.
fn strip_partial_tool_state(
    output: &mut AssistantMessage,
    tool_json_accum: &mut std::collections::HashMap<u64, String>,
    block_index_map: &[(u64, usize)],
) {
    for (block_index, _) in tool_json_accum.drain() {
        if let Some((_, ci)) = block_index_map.iter().find(|(bi, _)| *bi == block_index)
            && let Some(AssistantContent::ToolCall(tc)) = output.content.get_mut(*ci)
        {
            tc.arguments = serde_json::Value::Object(Default::default());
        }
    }
}

// ---------------------------------------------------------------------------
// Request building
// ---------------------------------------------------------------------------

fn build_request_body(
    model: &Model,
    context: &Context,
    options: &StreamOptions,
) -> tau_agent_base::Result<MessagesRequest> {
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
            Message::CompactionSummary(cs) => {
                let text = format!(
                    "[Context compacted — {} tokens before compaction]\n\n{}",
                    cs.tokens_before, cs.summary
                );
                messages.push(ApiMessage {
                    role: "user",
                    content: serde_json::Value::String(text),
                });
            }
            Message::Info(_) => {
                // Info messages are display-only; not sent to the LLM.
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
        .map(tau_agent_base::subscription_usage::is_oauth_token)
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
        let mut defs: Vec<ToolDef> = context
            .tools
            .iter()
            .map(|t| ToolDef {
                name: t.name.clone(),
                description: t.description.clone(),
                input_schema: t.parameters.clone(),
                cache_control: None,
            })
            .collect();
        // Place a cache breakpoint on the last tool so tool-list changes
        // don't invalidate the transcript cache and vice versa. Mirrors
        // the upstream pi-ai behavior (commit 1c016cb0).
        if let Some(last) = defs.last_mut() {
            last.cache_control = cc.clone();
        }
        Some(defs)
    };

    let thinking_enabled = thinking_requested(model, options);
    let (thinking, output_config) = if thinking_enabled {
        build_thinking_config(model, options)
    } else {
        (None, None)
    };

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
        output_config,
    })
}

// ---------------------------------------------------------------------------
// Thinking / adaptive thinking
// ---------------------------------------------------------------------------

/// Whether a model supports Anthropic "adaptive" thinking.
///
/// Substring checks match both the hyphen- and dot-form model IDs (e.g.
/// `claude-opus-4-6` and `claude-opus-4.6`). Ports `supportsAdaptiveThinking`
/// from pi-mono (d1c6cb1e).
fn supports_adaptive_thinking(model_id: &str) -> bool {
    model_id.contains("opus-4-6")
        || model_id.contains("opus-4.6")
        || model_id.contains("opus-4-7")
        || model_id.contains("opus-4.7")
        || model_id.contains("sonnet-4-6")
        || model_id.contains("sonnet-4.6")
}

/// Map tau's `ThinkingEffort` to Anthropic's effort string, taking the
/// model into account. Mirrors pi-mono's `mapThinkingLevelToEffort`:
/// * `XHigh` on `opus-4-6` → `"max"`
/// * `XHigh` on `opus-4-7` → `"xhigh"`
/// * `XHigh` on other adaptive models → `"high"`
/// * everything else maps identity.
fn map_effort(effort: ThinkingEffort, model_id: &str) -> &'static str {
    match effort {
        ThinkingEffort::Low => "low",
        ThinkingEffort::Medium => "medium",
        ThinkingEffort::High => "high",
        ThinkingEffort::Max => "max",
        ThinkingEffort::XHigh => {
            if model_id.contains("opus-4-6") || model_id.contains("opus-4.6") {
                "max"
            } else if model_id.contains("opus-4-7") || model_id.contains("opus-4.7") {
                "xhigh"
            } else {
                "high"
            }
        }
    }
}

/// Is thinking on for this request? `thinking_enabled` always wins; otherwise
/// we infer from whether the caller passed a budget or (on adaptive-capable
/// models) an effort. Models without `ThinkingStyle::Anthropic` never get a
/// thinking block.
fn thinking_requested(model: &Model, options: &StreamOptions) -> bool {
    if model.thinking != ThinkingStyle::Anthropic {
        return false;
    }
    match options.thinking_enabled {
        Some(v) => v,
        None => {
            options.thinking_budget.is_some()
                || (supports_adaptive_thinking(&model.id) && options.thinking_effort.is_some())
        }
    }
}

/// Build the `thinking` and (optional) `output_config` fields.
///
/// Callers must only invoke this once they've decided thinking is on — see
/// `thinking_requested`. Branches:
/// * adaptive model → `{type: "adaptive", display}` + optional `output_config`
/// * other reasoning model → `{type: "enabled", budget_tokens, display}`
fn build_thinking_config(
    model: &Model,
    options: &StreamOptions,
) -> (Option<ThinkingConfig>, Option<OutputConfig>) {
    let display = match options
        .thinking_display
        .unwrap_or(ThinkingDisplay::Summarized)
    {
        ThinkingDisplay::Summarized => "summarized",
        ThinkingDisplay::Omitted => "omitted",
    };

    if supports_adaptive_thinking(&model.id) {
        let output_config = options.thinking_effort.map(|e| OutputConfig {
            effort: map_effort(e, &model.id),
        });
        (
            Some(ThinkingConfig {
                thinking_type: "adaptive",
                budget_tokens: None,
                display: Some(display),
            }),
            output_config,
        )
    } else {
        let budget = options.thinking_budget.unwrap_or(1024);
        (
            Some(ThinkingConfig {
                thinking_type: "enabled",
                budget_tokens: Some(budget),
                display: Some(display),
            }),
            None,
        )
    }
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
            id: "claude-opus-4-7".into(),
            name: "Claude Opus 4.7".into(),
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
                    duration_ms: None,
                    summary: None,
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

    #[test]
    fn last_tool_has_cache_control_only() {
        let ctx = Context {
            system_prompt: None,
            messages: vec![Message::User(UserMessage::text("hi"))],
            tools: vec![
                Tool {
                    name: "first".into(),
                    description: "first tool".into(),
                    parameters: serde_json::json!({"type": "object"}),
                },
                Tool {
                    name: "second".into(),
                    description: "second tool".into(),
                    parameters: serde_json::json!({"type": "object"}),
                },
                Tool {
                    name: "third".into(),
                    description: "third tool".into(),
                    parameters: serde_json::json!({"type": "object"}),
                },
            ],
        };
        let body = build(&ctx, &StreamOptions::default());
        let tools = body["tools"].as_array().expect("tools array");
        assert_eq!(tools.len(), 3);
        assert!(tools[0].get("cache_control").is_none());
        assert!(tools[1].get("cache_control").is_none());
        assert_eq!(tools[2]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn no_tools_omits_tools_field() {
        let body = build(&simple_context(None, "hi"), &StreamOptions::default());
        let tools = body.get("tools");
        assert!(
            tools.is_none() || tools.unwrap().is_null(),
            "tools should be absent when empty"
        );
    }

    #[test]
    fn strip_partial_tool_state_resets_in_progress_arguments() {
        // Simulate the state mid-stream: one completed tool call (with real
        // arguments) and one whose partial JSON is still in the scratch buffer.
        let mut output = AssistantMessage::empty("anthropic-messages", "anthropic", "claude");
        output.content.push(AssistantContent::ToolCall(ToolCall {
            id: "tc_complete".into(),
            name: "done".into(),
            arguments: serde_json::json!({"ok": true}),
        }));
        output.content.push(AssistantContent::ToolCall(ToolCall {
            id: "tc_in_flight".into(),
            name: "in_flight".into(),
            // Block_start always initializes arguments to {}; callers only
            // assign a parsed value on content_block_stop.
            arguments: serde_json::Value::Object(Default::default()),
        }));
        let block_index_map: Vec<(u64, usize)> = vec![(0, 0), (1, 1)];
        let mut tool_json_accum: std::collections::HashMap<u64, String> =
            std::collections::HashMap::new();
        // Scratch buffer has a half-finished JSON fragment for block index 1.
        tool_json_accum.insert(1, "{\"partial\": \"val".to_string());

        strip_partial_tool_state(&mut output, &mut tool_json_accum, &block_index_map);

        // Scratch buffer is drained.
        assert!(tool_json_accum.is_empty());
        // Completed tool call is untouched.
        if let AssistantContent::ToolCall(tc) = &output.content[0] {
            assert_eq!(tc.arguments, serde_json::json!({"ok": true}));
        } else {
            panic!("expected ToolCall at index 0");
        }
        // In-flight tool call has empty arguments (no partial-JSON residue).
        if let AssistantContent::ToolCall(tc) = &output.content[1] {
            assert_eq!(tc.arguments, serde_json::Value::Object(Default::default()));
        } else {
            panic!("expected ToolCall at index 1");
        }

        // Re-serializing the error message must not expose any partial-JSON
        // scratch fields (our wire type has none, but assert the shape).
        let wire = serde_json::to_value(&output).expect("serialize");
        let content = wire["content"].as_array().expect("content array");
        for block in content {
            let obj = block.as_object().expect("object");
            assert!(!obj.contains_key("partial_json"));
            assert!(!obj.contains_key("partialJson"));
        }
    }

    // -----------------------------------------------------------------
    // Adaptive thinking / thinkingDisplay
    // -----------------------------------------------------------------

    fn model_by_id(id: &str) -> Model {
        models()
            .into_iter()
            .find(|m| m.id == id)
            .unwrap_or_else(|| panic!("model {id} not in predefined list"))
    }

    fn build_for(model: &Model, options: &StreamOptions) -> serde_json::Value {
        let ctx = simple_context(None, "hi");
        let req = build_request_body(model, &ctx, options).expect("build_request_body");
        serde_json::to_value(req).expect("serialize")
    }

    #[test]
    fn supports_adaptive_thinking_matches_known_models() {
        assert!(supports_adaptive_thinking("claude-opus-4-6"));
        assert!(supports_adaptive_thinking("claude-opus-4.6"));
        assert!(supports_adaptive_thinking("claude-opus-4-7"));
        assert!(supports_adaptive_thinking("claude-opus-4.7"));
        assert!(supports_adaptive_thinking("claude-sonnet-4-6"));
        assert!(supports_adaptive_thinking("claude-sonnet-4.6"));
        // Variants with date suffixes still match on substring.
        assert!(supports_adaptive_thinking("claude-opus-4-7-20260101"));

        assert!(!supports_adaptive_thinking("claude-opus-4-5"));
        assert!(!supports_adaptive_thinking("claude-sonnet-4-5"));
        assert!(!supports_adaptive_thinking("claude-sonnet-3-5"));
        assert!(!supports_adaptive_thinking("claude-haiku-4-5"));
    }

    #[test]
    fn opus_4_7_xhigh_emits_adaptive_with_xhigh_effort() {
        let model = model_by_id("claude-opus-4-7");
        let opts = StreamOptions {
            thinking_enabled: Some(true),
            thinking_effort: Some(ThinkingEffort::XHigh),
            ..Default::default()
        };
        let body = build_for(&model, &opts);
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(body["thinking"]["display"], "summarized");
        assert!(
            body["thinking"].get("budget_tokens").is_none(),
            "adaptive mode must not send budget_tokens: {body}"
        );
        assert_eq!(body["output_config"]["effort"], "xhigh");
    }

    #[test]
    fn opus_4_6_xhigh_maps_to_max_effort() {
        let model = model_by_id("claude-opus-4-6");
        let opts = StreamOptions {
            thinking_enabled: Some(true),
            thinking_effort: Some(ThinkingEffort::XHigh),
            ..Default::default()
        };
        let body = build_for(&model, &opts);
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(body["output_config"]["effort"], "max");
    }

    #[test]
    fn sonnet_4_6_xhigh_falls_back_to_high() {
        let model = model_by_id("claude-sonnet-4-6");
        let opts = StreamOptions {
            thinking_enabled: Some(true),
            thinking_effort: Some(ThinkingEffort::XHigh),
            ..Default::default()
        };
        let body = build_for(&model, &opts);
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(body["output_config"]["effort"], "high");
    }

    #[test]
    fn adaptive_model_without_effort_omits_output_config() {
        let model = model_by_id("claude-opus-4-7");
        let opts = StreamOptions {
            thinking_enabled: Some(true),
            ..Default::default()
        };
        let body = build_for(&model, &opts);
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(body["thinking"]["display"], "summarized");
        let output_config = body.get("output_config");
        assert!(
            output_config.is_none() || output_config.unwrap().is_null(),
            "output_config should be absent when no effort set: {body}"
        );
    }

    #[test]
    fn older_model_with_budget_uses_enabled_branch() {
        let model = model_by_id("claude-opus-4-5");
        let opts = StreamOptions {
            thinking_enabled: Some(true),
            thinking_budget: Some(1024),
            ..Default::default()
        };
        let body = build_for(&model, &opts);
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["thinking"]["budget_tokens"], 1024);
        assert_eq!(body["thinking"]["display"], "summarized");
        let output_config = body.get("output_config");
        assert!(output_config.is_none() || output_config.unwrap().is_null());
    }

    #[test]
    fn thinking_enabled_false_omits_thinking_field() {
        let model = model_by_id("claude-opus-4-7");
        let opts = StreamOptions {
            thinking_enabled: Some(false),
            thinking_effort: Some(ThinkingEffort::High),
            thinking_budget: Some(1024),
            ..Default::default()
        };
        let body = build_for(&model, &opts);
        assert!(
            body.get("thinking").map_or(true, |v| v.is_null()),
            "thinking must be absent when thinking_enabled=Some(false): {body}"
        );
        assert!(
            body.get("output_config").map_or(true, |v| v.is_null()),
            "output_config must be absent when thinking_enabled=Some(false): {body}"
        );
    }

    #[test]
    fn thinking_display_omitted_serializes_as_omitted() {
        let model = model_by_id("claude-opus-4-7");
        let opts = StreamOptions {
            thinking_enabled: Some(true),
            thinking_display: Some(ThinkingDisplay::Omitted),
            ..Default::default()
        };
        let body = build_for(&model, &opts);
        assert_eq!(body["thinking"]["display"], "omitted");
    }

    #[test]
    fn thinking_budget_without_enabled_still_triggers_on_legacy_model() {
        // Backward-compat: existing callers that only pass thinking_budget
        // should still get thinking enabled.
        let model = model_by_id("claude-opus-4-5");
        let opts = StreamOptions {
            thinking_budget: Some(2048),
            ..Default::default()
        };
        let body = build_for(&model, &opts);
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["thinking"]["budget_tokens"], 2048);
    }

    #[test]
    fn thinking_budget_on_adaptive_model_promotes_to_adaptive() {
        // thinking_budget is ignored in adaptive mode; a budget alone must
        // not emit the legacy `{type: "enabled"}` shape on an adaptive model.
        let model = model_by_id("claude-opus-4-7");
        let opts = StreamOptions {
            thinking_budget: Some(2048),
            ..Default::default()
        };
        let body = build_for(&model, &opts);
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert!(body["thinking"].get("budget_tokens").is_none());
    }

    #[test]
    fn non_anthropic_thinking_style_suppresses_thinking() {
        // Even with thinking_enabled=true, a model whose ThinkingStyle isn't
        // Anthropic must not emit a thinking block (e.g. for OpenAI-compat
        // models routed through this provider in theory).
        let mut model = model_by_id("claude-opus-4-7");
        model.thinking = ThinkingStyle::None;
        let opts = StreamOptions {
            thinking_enabled: Some(true),
            thinking_effort: Some(ThinkingEffort::High),
            ..Default::default()
        };
        let body = build_for(&model, &opts);
        assert!(
            body.get("thinking").map_or(true, |v| v.is_null()),
            "non-anthropic ThinkingStyle must suppress thinking: {body}"
        );
    }

    #[test]
    fn no_thinking_options_omits_thinking_field() {
        let model = model_by_id("claude-opus-4-7");
        let body = build_for(&model, &StreamOptions::default());
        assert!(
            body.get("thinking").map_or(true, |v| v.is_null()),
            "default StreamOptions must not emit thinking: {body}"
        );
        assert!(body.get("output_config").map_or(true, |v| v.is_null()));
    }

    #[test]
    fn opus_4_7_model_entry_matches_pi_mono() {
        // Sanity: cost + context-window + max_tokens should match pi-mono
        // a91978cf (Anthropic override for Opus 4.7).
        let m = model_by_id("claude-opus-4-7");
        assert_eq!(m.cost.input, 5.0);
        assert_eq!(m.cost.output, 25.0);
        assert_eq!(m.cost.cache_read, 0.5);
        assert_eq!(m.cost.cache_write, 6.25);
        assert_eq!(m.context_window, 1_000_000);
        assert_eq!(m.max_tokens, 128_000);
        assert_eq!(m.thinking, ThinkingStyle::Anthropic);
    }
}
