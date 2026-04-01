//! Agent loop — stream LLM response, execute tool calls, repeat.
//!
//! The loop continues until the LLM stops without tool calls, or an
//! unrecoverable error occurs, or max_turns is reached.

use crate::provider::{EventReceiver, EventSender, ProviderRegistry};

use crate::types::*;
use crate::worker::ToolExecutor;

/// Configuration for the agent loop.
pub struct AgentConfig {
    /// Maximum number of LLM turns (each tool-call-and-response is one turn).
    pub max_turns: usize,
    /// Maximum retries for transient errors (429, 529, 5xx).
    pub max_retries: usize,
    /// Base delay for retry backoff in milliseconds.
    pub retry_base_ms: u64,
    /// Optional shutdown check — if returns true, stop after current turn.
    pub should_stop: Option<Box<dyn Fn() -> bool + Send + Sync>>,
    /// Channel for receiving steering messages injected mid-loop.
    /// Checked at the top of each turn (after tool results, before next LLM call).
    pub steer_rx: Option<smol::channel::Receiver<String>>,
    /// Called for each message produced by the agent (assistant, tool result, steering).
    /// Used for incremental persistence so partial progress survives errors.
    /// Wrapped in Mutex so AgentConfig is Send+Sync across async boundaries.
    #[allow(clippy::type_complexity)]
    pub on_message: Option<std::sync::Mutex<Box<dyn FnMut(&Message) + Send>>>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_turns: 50,
            max_retries: 5,
            retry_base_ms: 1000,
            should_stop: None,
            steer_rx: None,
            on_message: None,
        }
    }
}

/// Result of an agent loop run.
pub struct AgentResult {
    /// All new messages added during the loop (assistant + tool results).
    pub new_messages: Vec<Message>,
    /// Whether the loop was stopped due to max turns.
    pub max_turns_reached: bool,
}

/// Check if a message list needs a continuation turn.
/// Returns true if the last message is a ToolResult — meaning the session
/// was interrupted after tool execution but before the LLM responded.
pub fn needs_continuation(messages: &[Message]) -> bool {
    matches!(messages.last(), Some(Message::ToolResult(_)))
}

/// Repair a message history that was corrupted by a crash or kill.
///
/// Two cases:
/// 1. Last message is Assistant with StopReason::ToolUse but no ToolResult
///    messages follow (daemon killed before any tool executed).
/// 2. Last message is ToolResult but the preceding Assistant had more tool_use
///    blocks than there are ToolResult messages (partial execution).
///
/// Returns the stub messages that were synthesized (caller should persist them).
/// Returns an empty vec if no repair was needed.
pub fn repair_messages(messages: &[Message]) -> Vec<Message> {
    if messages.is_empty() {
        return Vec::new();
    }

    // Find the last Assistant message and collect any trailing ToolResults
    let mut last_assistant_idx = None;
    for (i, msg) in messages.iter().enumerate().rev() {
        match msg {
            Message::ToolResult(_) => continue,
            Message::Assistant(_) => {
                last_assistant_idx = Some(i);
                break;
            }
            _ => break, // User or CompactionSummary — no repair needed
        }
    }

    let Some(assistant_idx) = last_assistant_idx else {
        return Vec::new();
    };

    let assistant = match &messages[assistant_idx] {
        Message::Assistant(a) if a.stop_reason == StopReason::ToolUse => a,
        _ => return Vec::new(),
    };

    // Collect tool_use IDs from the assistant message
    let tool_call_ids: Vec<(&str, &str)> = assistant
        .content
        .iter()
        .filter_map(|c| match c {
            AssistantContent::ToolCall(tc) => Some((tc.id.as_str(), tc.name.as_str())),
            _ => None,
        })
        .collect();

    if tool_call_ids.is_empty() {
        return Vec::new();
    }

    // Collect tool_result IDs that follow the assistant message
    let existing_result_ids: std::collections::HashSet<&str> = messages[assistant_idx + 1..]
        .iter()
        .filter_map(|m| match m {
            Message::ToolResult(tr) => Some(tr.tool_call_id.as_str()),
            _ => None,
        })
        .collect();

    // Synthesize stubs for any missing tool results
    let mut stubs = Vec::new();
    for (id, name) in &tool_call_ids {
        if !existing_result_ids.contains(id) {
            stubs.push(Message::ToolResult(ToolResultMessage {
                tool_call_id: id.to_string(),
                tool_name: name.to_string(),
                content: vec![ToolResultContent::Text(TextContent {
                    text: "error: session interrupted before execution".into(),
                    text_signature: None,
                })],
                details: None,
                is_error: true,
                timestamp: crate::types::timestamp_ms(),
            }));
        }
    }

    stubs
}

/// Async cancellable sleep.
/// Returns `Err(Cancelled)` if should_stop returns true during the sleep.
async fn cancellable_sleep(
    delay: std::time::Duration,
    should_stop: Option<&(dyn Fn() -> bool + Send + Sync)>,
) -> crate::Result<()> {
    let deadline = std::time::Instant::now() + delay;
    loop {
        if should_stop.is_some_and(|f| f()) {
            return Err(crate::Error::Cancelled);
        }
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Ok(());
        }
        let chunk = remaining.min(std::time::Duration::from_millis(100));
        smol::Timer::after(chunk).await;
    }
}

/// Run the agent loop.
///
/// Streams LLM responses, executes tool calls via the worker,
/// and loops until the model stops or max_turns is reached.
/// All stream events are sent to `event_tx`.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    registry: &ProviderRegistry,
    model: &Model,
    context: &mut Context,
    worker: &mut dyn ToolExecutor,
    options: &StreamOptions,
    config: &AgentConfig,
    extra_tools: &[Tool],
    event_tx: EventSender,
) -> crate::Result<AgentResult> {
    let mut new_messages = Vec::new();

    // All tools come from plugins (session + global)
    context.tools = extra_tools.to_vec();

    for turn in 0..config.max_turns {
        // Drain any pending steering messages before the next LLM call.
        // These are user messages injected mid-loop via Request::Steer.
        if let Some(ref rx) = config.steer_rx {
            while let Ok(text) = rx.try_recv() {
                let user_msg = UserMessage::text(&text);
                let _ = event_tx.try_send(StreamEvent::SteerMessage {
                    message: user_msg.clone(),
                });
                let msg = Message::User(user_msg);
                if let Some(ref on_msg) = config.on_message {
                    on_msg.lock().unwrap()(&msg);
                }
                new_messages.push(msg.clone());
                context.messages.push(msg);
            }
        }

        // Signal that we're about to call the LLM (so the UI can update the phase).
        let _ = event_tx.try_send(StreamEvent::Phase {
            phase: crate::types::AgentPhase::Connecting,
        });

        // Stream LLM response (with retry)
        let message =
            match stream_with_retry(registry, model, context, options, config, &event_tx).await {
                Err(crate::Error::Cancelled) => {
                    return Ok(AgentResult {
                        new_messages,
                        max_turns_reached: false,
                    });
                }
                other => other?,
            };

        let stop_reason = message.stop_reason;
        let assistant_msg = Message::Assistant(message.clone());
        if let Some(ref on_msg) = config.on_message {
            on_msg.lock().unwrap()(&assistant_msg);
        }
        new_messages.push(assistant_msg);
        context.messages.push(Message::Assistant(message.clone()));

        // If error or aborted, stop
        if stop_reason == StopReason::Error || stop_reason == StopReason::Aborted {
            return Ok(AgentResult {
                new_messages,
                max_turns_reached: false,
            });
        }

        // If no tool calls, we're done
        if stop_reason != StopReason::ToolUse {
            return Ok(AgentResult {
                new_messages,
                max_turns_reached: false,
            });
        }

        // Execute tool calls
        let tool_calls: Vec<ToolCall> = message
            .content
            .iter()
            .filter_map(|c| match c {
                AssistantContent::ToolCall(tc) => Some(tc.clone()),
                _ => None,
            })
            .collect();

        if tool_calls.is_empty() {
            // ToolUse stop reason but no tool calls — shouldn't happen, but handle gracefully
            return Ok(AgentResult {
                new_messages,
                max_turns_reached: false,
            });
        }

        for (tc_idx, tc) in tool_calls.iter().enumerate() {
            // Check for cancellation/shutdown before each tool call.
            // If cancelled, emit stub ToolResult for all remaining tool calls
            // so the message history stays valid (every tool_use needs a tool_result).
            if config.should_stop.as_ref().is_some_and(|f| f()) {
                for remaining_tc in &tool_calls[tc_idx..] {
                    let stub = crate::types::ToolResultMessage {
                        tool_call_id: remaining_tc.id.clone(),
                        tool_name: remaining_tc.name.clone(),
                        content: vec![crate::types::ToolResultContent::Text(
                            crate::types::TextContent {
                                text: "error: cancelled before execution".into(),
                                text_signature: None,
                            },
                        )],
                        details: None,
                        is_error: true,
                        timestamp: crate::types::timestamp_ms(),
                    };
                    let _ = event_tx.try_send(StreamEvent::ToolResult {
                        tool_call_id: remaining_tc.id.clone(),
                        tool_name: remaining_tc.name.clone(),
                        is_error: true,
                        content: "error: cancelled before execution".into(),
                    });
                    let tool_msg = Message::ToolResult(stub.clone());
                    if let Some(ref on_msg) = config.on_message {
                        on_msg.lock().unwrap()(&tool_msg);
                    }
                    new_messages.push(tool_msg);
                    context.messages.push(Message::ToolResult(stub));
                }
                return Ok(AgentResult {
                    new_messages,
                    max_turns_reached: false,
                });
            }
            // Execute tool with streaming output deltas via channel.
            // Errors (e.g. unknown tool) become error ToolResultMessages so
            // the LLM can see them and the agent loop continues.
            let (tool_output_tx, tool_output_rx) = smol::channel::unbounded::<String>();

            // Spawn tool execution and output forwarding concurrently.
            // The execute future must drop tool_output_tx when done so the
            // forward loop sees channel-closed and terminates.
            let tool_future = async {
                let res = worker.execute(tc, &tool_output_tx).await;
                drop(tool_output_tx);
                res
            };
            let event_tx_ref = &event_tx;
            let tc_id = tc.id.clone();
            let forward_future = async {
                while let Ok(delta) = tool_output_rx.recv().await {
                    let _ = event_tx_ref.try_send(StreamEvent::ToolOutputDelta {
                        tool_call_id: tc_id.clone(),
                        delta,
                    });
                }
            };
            let (result, _) = futures::future::join(tool_future, forward_future).await;
            let result = match result {
                Ok(r) => r,
                Err(e) => crate::types::ToolResultMessage {
                    tool_call_id: tc.id.clone(),
                    tool_name: tc.name.clone(),
                    content: vec![crate::types::ToolResultContent::Text(
                        crate::types::TextContent {
                            text: format!("error: {}", e),
                            text_signature: None,
                        },
                    )],
                    details: None,
                    is_error: true,
                    timestamp: crate::types::timestamp_ms(),
                },
            };

            // Emit full tool result
            let content = result
                .content
                .iter()
                .filter_map(|c| match c {
                    crate::types::ToolResultContent::Text(t) => Some(t.text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            let _ = event_tx.try_send(StreamEvent::ToolResult {
                tool_call_id: tc.id.clone(),
                tool_name: result.tool_name.clone(),
                is_error: result.is_error,
                content,
            });

            let tool_msg = Message::ToolResult(result.clone());
            if let Some(ref on_msg) = config.on_message {
                on_msg.lock().unwrap()(&tool_msg);
            }
            new_messages.push(tool_msg);
            context.messages.push(Message::ToolResult(result));
        }

        // Check if we should stop (shutdown or max turns)
        if config.should_stop.as_ref().is_some_and(|f| f()) {
            return Ok(AgentResult {
                new_messages,
                max_turns_reached: false,
            });
        }
        if turn + 1 >= config.max_turns {
            return Ok(AgentResult {
                new_messages,
                max_turns_reached: true,
            });
        }
    }

    Ok(AgentResult {
        new_messages,
        max_turns_reached: true,
    })
}

/// Maximum retry delay in milliseconds (60 seconds).
const MAX_RETRY_DELAY_MS: u64 = 60_000;

/// Maximum retries for timeout errors (fewer than rate limits, since each
/// timeout already consumed 30-120s of waiting).
const MAX_TIMEOUT_RETRIES: usize = 2;

/// Fixed delay between timeout retries in milliseconds (first retry is immediate).
const TIMEOUT_RETRY_DELAY_MS: u64 = 5_000;

/// Stream a single LLM call with retry logic for transient errors.
async fn stream_with_retry(
    registry: &ProviderRegistry,
    model: &Model,
    context: &Context,
    options: &StreamOptions,
    config: &AgentConfig,
    event_tx: &EventSender,
) -> crate::Result<AssistantMessage> {
    let max_attempts = config.max_retries + 1;
    for attempt in 0..max_attempts {
        let rx = registry.stream(model, context, options)?;
        let should_stop = config.should_stop.as_deref();
        let message = consume_stream(rx, event_tx, should_stop).await;

        // Propagate cancellation immediately — no retry.
        let message = match message {
            Err(crate::Error::Cancelled) => return Err(crate::Error::Cancelled),
            other => other?,
        };

        if message.stop_reason == StopReason::Error
            && let Some(ref err_msg) = message.error_message
        {
            let timeout = is_timeout(err_msg);
            let retryable = timeout || is_retryable(err_msg);

            // Timeouts get fewer retries since each one already burned 30-120s.
            let max_retries_for_error = if timeout {
                MAX_TIMEOUT_RETRIES
            } else {
                config.max_retries
            };

            if retryable && attempt < max_retries_for_error {
                let delay_ms = if timeout {
                    // First timeout retry is immediate, subsequent use fixed delay.
                    if attempt == 0 {
                        0
                    } else {
                        TIMEOUT_RETRY_DELAY_MS
                    }
                } else {
                    // Rate limit / 5xx: use retry-after header or exponential backoff.
                    parse_retry_after(err_msg)
                        .map(|secs| secs * 1000)
                        .unwrap_or_else(|| {
                            (config.retry_base_ms * 2u64.pow(attempt as u32))
                                .min(MAX_RETRY_DELAY_MS)
                        })
                };

                let delay_human = if delay_ms == 0 {
                    "immediately".to_string()
                } else {
                    format!("in {}", format_duration_human(delay_ms))
                };
                let status_msg = format!(
                    "{} (attempt {}/{}), retrying {}: {}",
                    if timeout {
                        "timeout"
                    } else {
                        "retryable error"
                    },
                    attempt + 1,
                    max_retries_for_error,
                    delay_human,
                    err_msg
                );
                eprintln!("{}", status_msg);
                let _ = event_tx.try_send(StreamEvent::Status {
                    message: status_msg,
                });
                let _ = event_tx.try_send(StreamEvent::Phase {
                    phase: crate::types::AgentPhase::RateLimited,
                });
                if delay_ms > 0 {
                    cancellable_sleep(
                        std::time::Duration::from_millis(delay_ms),
                        config.should_stop.as_deref(),
                    )
                    .await?;
                }
                continue;
            }
            if is_context_overflow(err_msg) {
                // Return the error — caller (server) handles overflow recovery
                return Ok(message);
            }
        }

        return Ok(message);
    }

    Err(crate::Error::Http("max retries exceeded".into()))
}

/// Extract retry-after seconds from an error message containing "[retry-after: Ns]".
fn parse_retry_after(err_msg: &str) -> Option<u64> {
    let marker = "[retry-after: ";
    let start = err_msg.find(marker)? + marker.len();
    let rest = &err_msg[start..];
    let end = rest.find('s')?;
    rest[..end].parse().ok()
}

/// Format milliseconds as a human-readable duration string.
pub fn format_duration_human(ms: u64) -> String {
    let total_secs = ms / 1000;
    if total_secs < 60 {
        format!("{}s", total_secs)
    } else if total_secs < 3600 {
        let m = total_secs / 60;
        let s = total_secs % 60;
        if s == 0 {
            format!("{}m", m)
        } else {
            format!("{}m{}s", m, s)
        }
    } else {
        let h = total_secs / 3600;
        let m = (total_secs % 3600) / 60;
        if m == 0 {
            format!("{}h", h)
        } else {
            format!("{}h{}m", h, m)
        }
    }
}

/// Consume a stream, forwarding events to the channel and returning the final message.
///
/// `should_stop` is polled between events so that a cancel/shutdown signal
/// received while the stream is in-flight can abort it promptly.
async fn consume_stream(
    rx: EventReceiver,
    event_tx: &EventSender,
    should_stop: Option<&(dyn Fn() -> bool + Send + Sync)>,
) -> crate::Result<AssistantMessage> {
    loop {
        // Check cancellation between events.
        if should_stop.is_some_and(|f| f()) {
            // Drain the channel without forwarding so the provider thread
            // can finish and clean up, then return an aborted message.
            while let Ok(event) = rx.recv().await {
                if matches!(&event, StreamEvent::Done { .. } | StreamEvent::Error { .. }) {
                    break;
                }
            }
            return Err(crate::Error::Cancelled);
        }

        match rx.recv().await {
            Ok(event) => {
                let is_done =
                    matches!(&event, StreamEvent::Done { .. } | StreamEvent::Error { .. });
                let final_msg = match &event {
                    StreamEvent::Done { message, .. } => Some(message.clone()),
                    StreamEvent::Error { error, .. } => Some(error.clone()),
                    _ => None,
                };
                let _ = event_tx.try_send(event);
                if is_done {
                    return final_msg.ok_or(crate::Error::ChannelClosed);
                }
            }
            Err(_) => return Err(crate::Error::ChannelClosed),
        }
    }
}

/// Check if an error message indicates a timeout.
fn is_timeout(err_msg: &str) -> bool {
    let lower = err_msg.to_lowercase();
    lower.contains("timeout")
}

/// Check if an error message indicates a transient/retryable condition
/// (excluding timeouts, which are handled separately).
fn is_retryable(err_msg: &str) -> bool {
    let lower = err_msg.to_lowercase();
    lower.contains("529")
        || lower.contains("overloaded")
        || lower.contains("rate limit")
        || lower.contains("429")
        || lower.contains("500")
        || lower.contains("502")
        || lower.contains("503")
        || lower.contains("service unavailable")
        || lower.contains("internal server error")
}

/// Check if an error indicates context window overflow.
pub fn is_context_overflow(err_msg: &str) -> bool {
    let lower = err_msg.to_lowercase();
    lower.contains("context_length_exceeded")
        || lower.contains("maximum context length")
        || lower.contains("too many tokens")
        || lower.contains("prompt is too long")
        || lower.contains("prompt too long")
        || lower.contains("exceeded max context length")
        || lower.contains("exceeded context length")
        || lower.contains("token limit exceeded")
        || lower.contains("model_context_window_exceeded")
        || (lower.contains("max_tokens")
            && (lower.contains("exceed") || lower.contains("too long")))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::mock::*;
    use crate::worker::InProcessWorker;

    fn setup_registry(responses: Vec<MockResponse>) -> ProviderRegistry {
        let mut registry = ProviderRegistry::new();
        registry.register(MockProvider::new(responses));
        registry
    }

    fn basic_context() -> Context {
        Context {
            system_prompt: Some("You are helpful.".into()),
            messages: vec![Message::User(UserMessage::text("hello"))],
            tools: Vec::new(),
        }
    }

    #[test]
    fn simple_text_response() {
        smol::block_on(async {
            let registry = setup_registry(vec![MockResponse::Text("Hello!".into())]);
            let model = mock_model();
            let mut context = basic_context();
            let config = AgentConfig::default();
            let mut worker = InProcessWorker::new();
            let (tx, rx) = smol::channel::unbounded();

            let result = run(
                &registry,
                &model,
                &mut context,
                &mut worker,
                &StreamOptions::default(),
                &config,
                &[],
                tx,
            )
            .await
            .unwrap();

            assert_eq!(result.new_messages.len(), 1);
            assert!(!result.max_turns_reached);
            assert!(
                matches!(&result.new_messages[0], Message::Assistant(a) if a.text() == "Hello!")
            );
            // Drain and check events were sent
            let mut events = Vec::new();
            while let Ok(e) = rx.try_recv() {
                events.push(e);
            }
            assert!(!events.is_empty());
        });
    }

    #[test]
    fn tool_call_loop() {
        smol::block_on(async {
            let registry = setup_registry(vec![
                MockResponse::ToolCalls(vec![ToolCall {
                    id: "tc1".into(),
                    name: "bash".into(),
                    arguments: serde_json::json!({"command": "echo hi"}),
                }]),
                MockResponse::Text("The command output 'hi'.".into()),
            ]);
            let model = mock_model();
            let mut context = basic_context();
            let config = AgentConfig::default();
            let mut worker = InProcessWorker::new();
            let (tx, _rx) = smol::channel::unbounded();

            let result = run(
                &registry,
                &model,
                &mut context,
                &mut worker,
                &StreamOptions::default(),
                &config,
                &[],
                tx,
            )
            .await
            .unwrap();

            assert_eq!(result.new_messages.len(), 3);
            assert!(matches!(&result.new_messages[0], Message::Assistant(_)));
            assert!(matches!(&result.new_messages[1], Message::ToolResult(_)));
            assert!(
                matches!(&result.new_messages[2], Message::Assistant(a) if a.text().contains("hi"))
            );
        });
    }

    #[test]
    fn max_turns_limit() {
        smol::block_on(async {
            let mut responses = Vec::new();
            for i in 0..10 {
                responses.push(MockResponse::ToolCalls(vec![ToolCall {
                    id: format!("tc{}", i),
                    name: "bash".into(),
                    arguments: serde_json::json!({"command": "echo loop"}),
                }]));
            }
            let registry = setup_registry(responses);
            let model = mock_model();
            let mut context = basic_context();
            let config = AgentConfig {
                max_turns: 3,
                ..Default::default()
            };
            let mut worker = InProcessWorker::new();
            let (tx, _rx) = smol::channel::unbounded();

            let result = run(
                &registry,
                &model,
                &mut context,
                &mut worker,
                &StreamOptions::default(),
                &config,
                &[],
                tx,
            )
            .await
            .unwrap();

            assert!(result.max_turns_reached);
            assert_eq!(result.new_messages.len(), 6);
        });
    }

    #[test]
    fn error_stops_loop() {
        smol::block_on(async {
            let registry = setup_registry(vec![MockResponse::Error("something broke".into())]);
            let model = mock_model();
            let mut context = basic_context();
            let config = AgentConfig {
                max_retries: 0,
                ..Default::default()
            };
            let mut worker = InProcessWorker::new();
            let (tx, _rx) = smol::channel::unbounded();

            let result = run(
                &registry,
                &model,
                &mut context,
                &mut worker,
                &StreamOptions::default(),
                &config,
                &[],
                tx,
            )
            .await
            .unwrap();

            assert_eq!(result.new_messages.len(), 1);
            assert!(matches!(
                &result.new_messages[0],
                Message::Assistant(a) if a.stop_reason == StopReason::Error
            ));
        });
    }

    #[test]
    fn unknown_tool_returns_error_result() {
        smol::block_on(async {
            let registry = setup_registry(vec![
                MockResponse::ToolCalls(vec![ToolCall {
                    id: "tc1".into(),
                    name: "nonexistent_tool".into(),
                    arguments: serde_json::json!({}),
                }]),
                MockResponse::Text("I see the tool wasn't found.".into()),
            ]);
            let model = mock_model();
            let mut context = basic_context();
            let config = AgentConfig::default();
            let mut worker = InProcessWorker::new();
            let (tx, _rx) = smol::channel::unbounded();

            let result = run(
                &registry,
                &model,
                &mut context,
                &mut worker,
                &StreamOptions::default(),
                &config,
                &[],
                tx,
            )
            .await
            .unwrap();

            assert_eq!(result.new_messages.len(), 3);
            if let Message::ToolResult(tr) = &result.new_messages[1] {
                assert!(tr.is_error);
                assert!(tr.content.iter().any(|c| matches!(c,
                    ToolResultContent::Text(t) if t.text.contains("unknown tool")
                )));
            } else {
                panic!("expected tool result");
            }
        });
    }

    #[test]
    fn resume_after_interrupted_tool_call() {
        smol::block_on(async {
            let registry = setup_registry(vec![MockResponse::Text(
                "The tool returned some output.".into(),
            )]);
            let model = mock_model();
            let mut worker = InProcessWorker::new();

            let mut context = Context {
                system_prompt: Some("You are helpful.".into()),
                messages: vec![
                    Message::User(UserMessage::text("run echo hello")),
                    Message::Assistant({
                        let mut a = AssistantMessage::empty("mock", "mock", "mock-model");
                        a.content.push(AssistantContent::ToolCall(ToolCall {
                            id: "tc1".into(),
                            name: "bash".into(),
                            arguments: serde_json::json!({"command": "echo hello"}),
                        }));
                        a.stop_reason = StopReason::ToolUse;
                        a
                    }),
                    Message::ToolResult(ToolResultMessage {
                        tool_call_id: "tc1".into(),
                        tool_name: "bash".into(),
                        content: vec![ToolResultContent::Text(TextContent {
                            text: "hello\n".into(),
                            text_signature: None,
                        })],
                        details: None,
                        is_error: false,
                        timestamp: 0,
                    }),
                ],
                tools: Vec::new(),
            };
            let config = AgentConfig::default();
            let (tx, _rx) = smol::channel::unbounded();

            assert!(needs_continuation(&context.messages));

            let result = run(
                &registry,
                &model,
                &mut context,
                &mut worker,
                &StreamOptions::default(),
                &config,
                &[],
                tx,
            )
            .await
            .unwrap();

            assert_eq!(result.new_messages.len(), 1);
            assert!(
                matches!(&result.new_messages[0], Message::Assistant(a) if a.text().contains("tool returned"))
            );
        });
    }

    #[test]
    fn no_continuation_needed_after_assistant() {
        let messages = vec![
            Message::User(UserMessage::text("hi")),
            Message::Assistant(AssistantMessage::empty("mock", "mock", "mock-model")),
        ];
        assert!(!needs_continuation(&messages));
    }

    #[test]
    fn no_continuation_needed_empty() {
        let messages: Vec<Message> = vec![];
        assert!(!needs_continuation(&messages));
    }

    #[test]
    fn retryable_errors() {
        assert!(is_retryable("HTTP 529 overloaded"));
        assert!(is_retryable("rate limit exceeded (429)"));
        assert!(is_retryable("503 Service Unavailable"));
        assert!(!is_retryable("invalid api key"));
        // Timeouts are NOT matched by is_retryable (separate function)
        assert!(!is_retryable("timeout: connect timed out"));
    }

    #[test]
    fn timeout_errors() {
        assert!(is_timeout("timeout: connect timed out"));
        assert!(is_timeout("timeout: recv_response timed out"));
        assert!(is_timeout("Timeout: operation took too long"));
        assert!(!is_timeout("HTTP 429 rate limit"));
        assert!(!is_timeout("invalid api key"));
    }

    #[test]
    fn overflow_detection() {
        // Anthropic
        assert!(is_context_overflow(
            "prompt is too long: 250000 tokens > 200000 maximum"
        ));
        // OpenAI
        assert!(is_context_overflow(
            "context_length_exceeded: maximum context length is 200000"
        ));
        assert!(is_context_overflow("maximum context length exceeded"));
        // Ollama
        assert!(is_context_overflow(
            "prompt too long; exceeded max context length by 5000 tokens"
        ));
        assert!(is_context_overflow(
            "prompt too long; exceeded context length of 131072"
        ));
        // Generic
        assert!(is_context_overflow("too many tokens in the prompt"));
        assert!(is_context_overflow("token limit exceeded"));
        assert!(is_context_overflow("model_context_window_exceeded"));
        // Negative
        assert!(!is_context_overflow("invalid request"));
        assert!(!is_context_overflow("rate limit exceeded"));
    }

    #[test]
    fn cancel_mid_tool_calls_emits_stub_results() {
        // When cancelled between tool calls, all remaining tool_use blocks
        // must get corresponding tool_result messages to avoid API errors
        // on session resume.
        smol::block_on(async {
            let registry = setup_registry(vec![MockResponse::ToolCalls(vec![
                ToolCall {
                    id: "tc1".into(),
                    name: "bash".into(),
                    arguments: serde_json::json!({"command": "echo first"}),
                },
                ToolCall {
                    id: "tc2".into(),
                    name: "bash".into(),
                    arguments: serde_json::json!({"command": "echo second"}),
                },
                ToolCall {
                    id: "tc3".into(),
                    name: "bash".into(),
                    arguments: serde_json::json!({"command": "echo third"}),
                },
            ])]);
            let model = mock_model();
            let mut context = basic_context();
            let mut worker = InProcessWorker::new();
            let (tx, _rx) = smol::channel::unbounded();

            // Use an AtomicBool that starts false (allow stream to complete)
            // and gets set to true after stream_with_retry returns the
            // assistant message. We detect this by checking from a wrapper
            // that only fires after enough calls -- consume_stream polls
            // should_stop between each event (8 events for 3 tool calls),
            // plus once at the top of the tool call loop.

            // Set cancel after stream consumption completes.
            // The mock sends 8 events; consume_stream checks should_stop
            // before each event. We need it to return false for all of those,
            // then true when checked in the tool-call loop.
            let poll_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let poll_count_clone = poll_count.clone();
            let config = AgentConfig {
                should_stop: Some(Box::new(move || {
                    let n = poll_count_clone.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    // The stream has 8 events (Start, 3xToolcallStart, 3xToolcallEnd, Done).
                    // consume_stream checks should_stop before each recv = 8 checks.
                    // After stream completes, the tool-call loop checks should_stop
                    // before each tool call. We want to cancel at that point.
                    n >= 8
                })),
                ..Default::default()
            };

            let result = run(
                &registry,
                &model,
                &mut context,
                &mut worker,
                &StreamOptions::default(),
                &config,
                &[],
                tx,
            )
            .await
            .unwrap();

            // Should have: 1 assistant + 3 stub tool results = 4 messages
            assert_eq!(
                result.new_messages.len(),
                4,
                "expected assistant + 3 stub tool results, got {:?}",
                result
                    .new_messages
                    .iter()
                    .map(|m| match m {
                        Message::Assistant(_) => "assistant",
                        Message::ToolResult(_) => "tool_result",
                        Message::User(_) => "user",
                        Message::CompactionSummary(_) => "summary",
                    })
                    .collect::<Vec<_>>()
            );
            assert!(matches!(&result.new_messages[0], Message::Assistant(_)));

            // All 3 tool results present with correct IDs
            for (i, expected_id) in ["tc1", "tc2", "tc3"].iter().enumerate() {
                if let Message::ToolResult(tr) = &result.new_messages[i + 1] {
                    assert_eq!(tr.tool_call_id, *expected_id);
                    assert!(tr.is_error);
                    assert!(tr.content.iter().any(|c| matches!(c,
                        ToolResultContent::Text(t) if t.text.contains("cancelled")
                    )));
                } else {
                    panic!("expected tool result at index {}", i + 1);
                }
            }
        });
    }

    #[test]
    fn cancel_after_first_tool_stubs_remaining() {
        // Cancel after first tool executes -- first tool gets real result,
        // remaining tools get stub results.
        smol::block_on(async {
            let registry = setup_registry(vec![MockResponse::ToolCalls(vec![
                ToolCall {
                    id: "tc1".into(),
                    name: "bash".into(),
                    arguments: serde_json::json!({"command": "echo first"}),
                },
                ToolCall {
                    id: "tc2".into(),
                    name: "bash".into(),
                    arguments: serde_json::json!({"command": "echo second"}),
                },
            ])]);
            let model = mock_model();
            let mut context = basic_context();
            let mut worker = InProcessWorker::new();
            let (tx, _rx) = smol::channel::unbounded();

            // 8 events for 2 tool calls (Start, 2xToolcallStart, 2xToolcallEnd, Done = 6 events).
            // Then tool-call loop: check before tc1 (pass), execute tc1, check before tc2 (cancel).
            let poll_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let poll_count_clone = poll_count.clone();
            let config = AgentConfig {
                should_stop: Some(Box::new(move || {
                    let n = poll_count_clone.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    // 6 stream events + 1 check before tc1 (pass) = 7 checks before tc1 runs.
                    // Check 7 (0-indexed) is before tc2 -- should cancel.
                    n >= 7
                })),
                ..Default::default()
            };

            let result = run(
                &registry,
                &model,
                &mut context,
                &mut worker,
                &StreamOptions::default(),
                &config,
                &[],
                tx,
            )
            .await
            .unwrap();

            // 1 assistant + 1 real tool result (tc1) + 1 stub tool result (tc2) = 3
            assert_eq!(
                result.new_messages.len(),
                3,
                "got {:?}",
                result
                    .new_messages
                    .iter()
                    .map(|m| match m {
                        Message::Assistant(_) => "assistant",
                        Message::ToolResult(_) => "tool_result",
                        Message::User(_) => "user",
                        Message::CompactionSummary(_) => "summary",
                    })
                    .collect::<Vec<_>>()
            );

            // tc1 got a real result (not cancelled -- it was executed)
            if let Message::ToolResult(tr) = &result.new_messages[1] {
                assert_eq!(tr.tool_call_id, "tc1");
                assert!(!tr.content.iter().any(|c| matches!(c,
                    ToolResultContent::Text(t) if t.text.contains("cancelled")
                )));
            } else {
                panic!("expected tool result at index 1");
            }

            // tc2 got a stub cancelled result
            if let Message::ToolResult(tr) = &result.new_messages[2] {
                assert_eq!(tr.tool_call_id, "tc2");
                assert!(tr.is_error);
                assert!(tr.content.iter().any(|c| matches!(c,
                    ToolResultContent::Text(t) if t.text.contains("cancelled")
                )));
            } else {
                panic!("expected tool result at index 2");
            }
        });
    }

    #[test]
    fn parse_retry_after_from_error_msg() {
        assert_eq!(
            parse_retry_after("HTTP 429: rate limit [retry-after: 30s]"),
            Some(30)
        );
        assert_eq!(
            parse_retry_after("HTTP 429: too many requests [retry-after: 120s] please wait"),
            Some(120)
        );
        assert_eq!(parse_retry_after("HTTP 500: internal error"), None);
        assert_eq!(parse_retry_after("no retry info here"), None);
    }

    #[test]
    fn timeout_retry_succeeds_on_second_attempt() {
        smol::block_on(async {
            // First call times out, second succeeds
            let registry = setup_registry(vec![
                MockResponse::Error("timeout: recv_response timed out".into()),
                MockResponse::Text("recovered!".into()),
            ]);
            let model = mock_model();
            let mut context = basic_context();
            let config = AgentConfig::default();
            let mut worker = InProcessWorker::new();
            let (tx, rx) = smol::channel::unbounded();

            let result = run(
                &registry,
                &model,
                &mut context,
                &mut worker,
                &StreamOptions::default(),
                &config,
                &[],
                tx,
            )
            .await
            .unwrap();

            assert_eq!(result.new_messages.len(), 1);
            assert!(
                matches!(&result.new_messages[0], Message::Assistant(a) if a.text() == "recovered!")
            );

            // Check that a status event was emitted about the timeout retry
            let mut events = Vec::new();
            while let Ok(e) = rx.try_recv() {
                events.push(e);
            }
            let has_timeout_status = events.iter().any(
                |e| matches!(e, StreamEvent::Status { message } if message.contains("timeout")),
            );
            assert!(has_timeout_status, "should emit status about timeout retry");
        });
    }

    #[test]
    fn timeout_retry_exhausted_returns_error() {
        smol::block_on(async {
            // All 3 attempts (1 initial + 2 retries) time out
            let registry = setup_registry(vec![
                MockResponse::Error("timeout: connect timed out".into()),
                MockResponse::Error("timeout: connect timed out".into()),
                MockResponse::Error("timeout: connect timed out".into()),
            ]);
            let model = mock_model();
            let mut context = basic_context();
            let config = AgentConfig::default();
            let mut worker = InProcessWorker::new();
            let (tx, _rx) = smol::channel::unbounded();

            let result = run(
                &registry,
                &model,
                &mut context,
                &mut worker,
                &StreamOptions::default(),
                &config,
                &[],
                tx,
            )
            .await
            .unwrap();

            // After exhausting retries, the error message is returned
            assert_eq!(result.new_messages.len(), 1);
            assert!(
                matches!(&result.new_messages[0], Message::Assistant(a) if a.stop_reason == StopReason::Error)
            );
        });
    }

    // ------- repair_messages tests -------

    #[test]
    fn repair_no_messages() {
        assert!(repair_messages(&[]).is_empty());
    }

    #[test]
    fn repair_clean_history_no_op() {
        let messages = vec![
            Message::User(UserMessage::text("hi")),
            Message::Assistant(AssistantMessage::empty("mock", "mock", "mock-model")),
        ];
        assert!(repair_messages(&messages).is_empty());
    }

    #[test]
    fn repair_complete_tool_cycle_no_op() {
        let mut a = AssistantMessage::empty("mock", "mock", "mock-model");
        a.stop_reason = StopReason::ToolUse;
        a.content.push(AssistantContent::ToolCall(ToolCall {
            id: "tc1".into(),
            name: "bash".into(),
            arguments: serde_json::json!({}),
        }));
        let messages = vec![
            Message::User(UserMessage::text("hi")),
            Message::Assistant(a),
            Message::ToolResult(ToolResultMessage {
                tool_call_id: "tc1".into(),
                tool_name: "bash".into(),
                content: vec![ToolResultContent::Text(TextContent {
                    text: "ok".into(),
                    text_signature: None,
                })],
                details: None,
                is_error: false,
                timestamp: 0,
            }),
        ];
        assert!(repair_messages(&messages).is_empty());
    }

    #[test]
    fn repair_assistant_with_no_tool_results() {
        // Daemon killed right after persisting assistant with tool_use
        let mut a = AssistantMessage::empty("mock", "mock", "mock-model");
        a.stop_reason = StopReason::ToolUse;
        a.content.push(AssistantContent::ToolCall(ToolCall {
            id: "tc1".into(),
            name: "bash".into(),
            arguments: serde_json::json!({}),
        }));
        a.content.push(AssistantContent::ToolCall(ToolCall {
            id: "tc2".into(),
            name: "read".into(),
            arguments: serde_json::json!({}),
        }));
        let messages = vec![
            Message::User(UserMessage::text("hi")),
            Message::Assistant(a),
        ];
        let stubs = repair_messages(&messages);
        assert_eq!(stubs.len(), 2);
        for (i, expected_id) in ["tc1", "tc2"].iter().enumerate() {
            if let Message::ToolResult(tr) = &stubs[i] {
                assert_eq!(tr.tool_call_id, *expected_id);
                assert!(tr.is_error);
                assert!(tr.content.iter().any(|c| matches!(c,
                    ToolResultContent::Text(t) if t.text.contains("interrupted")
                )));
            } else {
                panic!("expected ToolResult stub");
            }
        }
    }

    #[test]
    fn repair_partial_tool_results() {
        // Daemon killed after first tool result but before second
        let mut a = AssistantMessage::empty("mock", "mock", "mock-model");
        a.stop_reason = StopReason::ToolUse;
        a.content.push(AssistantContent::ToolCall(ToolCall {
            id: "tc1".into(),
            name: "bash".into(),
            arguments: serde_json::json!({}),
        }));
        a.content.push(AssistantContent::ToolCall(ToolCall {
            id: "tc2".into(),
            name: "read".into(),
            arguments: serde_json::json!({}),
        }));
        let messages = vec![
            Message::User(UserMessage::text("hi")),
            Message::Assistant(a),
            Message::ToolResult(ToolResultMessage {
                tool_call_id: "tc1".into(),
                tool_name: "bash".into(),
                content: vec![ToolResultContent::Text(TextContent {
                    text: "ok".into(),
                    text_signature: None,
                })],
                details: None,
                is_error: false,
                timestamp: 0,
            }),
        ];
        let stubs = repair_messages(&messages);
        assert_eq!(stubs.len(), 1);
        if let Message::ToolResult(tr) = &stubs[0] {
            assert_eq!(tr.tool_call_id, "tc2");
            assert_eq!(tr.tool_name, "read");
            assert!(tr.is_error);
        } else {
            panic!("expected ToolResult stub");
        }
    }

    #[test]
    fn repair_ignores_non_tooluse_assistant() {
        // Assistant with StopReason::Stop should not trigger repair
        let a = AssistantMessage::empty("mock", "mock", "mock-model");
        let messages = vec![
            Message::User(UserMessage::text("hi")),
            Message::Assistant(a),
        ];
        assert!(repair_messages(&messages).is_empty());
    }
}
