//! Agent loop — stream LLM response, execute tool calls, repeat.
//!
//! The loop continues until the LLM stops without tool calls, or an
//! unrecoverable error occurs, or max_turns is reached.

use crate::provider::{EventReceiver, ProviderRegistry};

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
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_turns: 50,
            max_retries: 5,
            retry_base_ms: 1000,
            should_stop: None,
            steer_rx: None,
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

/// Callback for agent events (forwarded to client).
pub type EventCallback = Box<dyn FnMut(StreamEvent) + Send>;

/// Check if a message list needs a continuation turn.
/// Returns true if the last message is a ToolResult — meaning the session
/// was interrupted after tool execution but before the LLM responded.
pub fn needs_continuation(messages: &[Message]) -> bool {
    matches!(messages.last(), Some(Message::ToolResult(_)))
}

/// Run the agent loop.
///
/// Streams LLM responses, executes tool calls via the worker subprocess,
/// and loops until the model stops or max_turns is reached.
/// All stream events are forwarded via `on_event`.
#[allow(clippy::too_many_arguments)]
pub fn run(
    registry: &ProviderRegistry,
    model: &Model,
    context: &mut Context,
    worker: &mut dyn ToolExecutor,
    options: &StreamOptions,
    config: &AgentConfig,
    extra_tools: &[Tool],
    mut on_event: EventCallback,
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
                on_event(StreamEvent::SteerMessage {
                    message: user_msg.clone(),
                });
                let msg = Message::User(user_msg);
                new_messages.push(msg.clone());
                context.messages.push(msg);
            }
        }

        // Stream LLM response (with retry)
        let message =
            match stream_with_retry(registry, model, context, options, config, &mut on_event) {
                Err(crate::Error::Cancelled) => {
                    return Ok(AgentResult {
                        new_messages,
                        max_turns_reached: false,
                    });
                }
                other => other?,
            };

        let stop_reason = message.stop_reason;
        new_messages.push(Message::Assistant(message.clone()));
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

        for tc in &tool_calls {
            // Check for cancellation/shutdown before each tool call so that
            // a cancel request received while a previous tool was running
            // prevents the remaining tools in this batch from executing.
            if config.should_stop.as_ref().is_some_and(|f| f()) {
                return Ok(AgentResult {
                    new_messages,
                    max_turns_reached: false,
                });
            }
            // Execute tool with streaming output deltas
            let tc_id = tc.id.clone();
            let result = worker.execute(tc, &mut |delta: &str| {
                on_event(StreamEvent::ToolOutputDelta {
                    tool_call_id: tc_id.clone(),
                    delta: delta.to_string(),
                });
            })?;

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
            on_event(StreamEvent::ToolResult {
                tool_call_id: tc.id.clone(),
                tool_name: result.tool_name.clone(),
                is_error: result.is_error,
                content,
            });

            new_messages.push(Message::ToolResult(result.clone()));
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

/// Stream a single LLM call with retry logic for transient errors.
fn stream_with_retry(
    registry: &ProviderRegistry,
    model: &Model,
    context: &Context,
    options: &StreamOptions,
    config: &AgentConfig,
    on_event: &mut EventCallback,
) -> crate::Result<AssistantMessage> {
    for attempt in 0..=config.max_retries {
        let rx = registry.stream(model, context, options)?;
        let should_stop = config.should_stop.as_deref();
        let message = consume_stream(rx, on_event, should_stop);

        // Propagate cancellation immediately — no retry.
        let message = match message {
            Err(crate::Error::Cancelled) => return Err(crate::Error::Cancelled),
            other => other?,
        };

        if message.stop_reason == StopReason::Error
            && let Some(ref err_msg) = message.error_message
        {
            if is_retryable(err_msg) && attempt < config.max_retries {
                // Use retry-after from the error if present, otherwise exponential backoff
                let delay = parse_retry_after(err_msg)
                    .map(|secs| secs * 1000)
                    .unwrap_or_else(|| {
                        (config.retry_base_ms * 2u64.pow(attempt as u32)).min(MAX_RETRY_DELAY_MS)
                    });
                let status_msg = format!(
                    "retryable error (attempt {}/{}), retrying in {}ms: {}",
                    attempt + 1,
                    config.max_retries,
                    delay,
                    err_msg
                );
                eprintln!("{}", status_msg);
                on_event(StreamEvent::Status {
                    message: status_msg,
                });
                std::thread::sleep(std::time::Duration::from_millis(delay));
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

/// Consume a stream, forwarding events and returning the final message.
///
/// `should_stop` is polled between events so that a cancel/shutdown signal
/// received while the stream is in-flight can abort it promptly rather than
/// waiting for the full response to finish.
fn consume_stream(
    rx: EventReceiver,
    on_event: &mut EventCallback,
    should_stop: Option<&(dyn Fn() -> bool + Send + Sync)>,
) -> crate::Result<AssistantMessage> {
    loop {
        // Check cancellation between events.
        if should_stop.is_some_and(|f| f()) {
            // Drain the channel without forwarding so the provider thread
            // can finish and clean up, then return an aborted message.
            while let Ok(event) = rx.recv_blocking() {
                if matches!(&event, StreamEvent::Done { .. } | StreamEvent::Error { .. }) {
                    break;
                }
            }
            return Err(crate::Error::Cancelled);
        }

        match rx.recv_blocking() {
            Ok(event) => {
                let is_done =
                    matches!(&event, StreamEvent::Done { .. } | StreamEvent::Error { .. });
                let final_msg = match &event {
                    StreamEvent::Done { message, .. } => Some(message.clone()),
                    StreamEvent::Error { error, .. } => Some(error.clone()),
                    _ => None,
                };
                on_event(event);
                if is_done {
                    return final_msg.ok_or(crate::Error::ChannelClosed);
                }
            }
            Err(_) => return Err(crate::Error::ChannelClosed),
        }
    }
}

/// Check if an error message indicates a transient/retryable condition.
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
        let registry = setup_registry(vec![MockResponse::Text("Hello!".into())]);
        let model = mock_model();
        let mut context = basic_context();
        let config = AgentConfig::default();
        let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let events_clone = events.clone();
        let mut worker = InProcessWorker::new();

        let result = run(
            &registry,
            &model,
            &mut context,
            &mut worker,
            &StreamOptions::default(),
            &config,
            &[],
            Box::new(move |e| events_clone.lock().unwrap().push(e)),
        )
        .unwrap();

        assert_eq!(result.new_messages.len(), 1);
        assert!(!result.max_turns_reached);
        assert!(matches!(&result.new_messages[0], Message::Assistant(a) if a.text() == "Hello!"));
        assert!(!events.lock().unwrap().is_empty());
    }

    #[test]
    fn tool_call_loop() {
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

        let result = run(
            &registry,
            &model,
            &mut context,
            &mut worker,
            &StreamOptions::default(),
            &config,
            &[],
            Box::new(|_| {}),
        )
        .unwrap();

        assert_eq!(result.new_messages.len(), 3);
        assert!(matches!(&result.new_messages[0], Message::Assistant(_)));
        assert!(matches!(&result.new_messages[1], Message::ToolResult(_)));
        assert!(
            matches!(&result.new_messages[2], Message::Assistant(a) if a.text().contains("hi"))
        );
    }

    #[test]
    fn max_turns_limit() {
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

        let result = run(
            &registry,
            &model,
            &mut context,
            &mut worker,
            &StreamOptions::default(),
            &config,
            &[],
            Box::new(|_| {}),
        )
        .unwrap();

        assert!(result.max_turns_reached);
        assert_eq!(result.new_messages.len(), 6);
    }

    #[test]
    fn error_stops_loop() {
        let registry = setup_registry(vec![MockResponse::Error("something broke".into())]);
        let model = mock_model();
        let mut context = basic_context();
        let config = AgentConfig {
            max_retries: 0,
            ..Default::default()
        };
        let mut worker = InProcessWorker::new();

        let result = run(
            &registry,
            &model,
            &mut context,
            &mut worker,
            &StreamOptions::default(),
            &config,
            &[],
            Box::new(|_| {}),
        )
        .unwrap();

        assert_eq!(result.new_messages.len(), 1);
        assert!(matches!(
            &result.new_messages[0],
            Message::Assistant(a) if a.stop_reason == StopReason::Error
        ));
    }

    #[test]
    fn unknown_tool_returns_error_result() {
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

        let result = run(
            &registry,
            &model,
            &mut context,
            &mut worker,
            &StreamOptions::default(),
            &config,
            &[],
            Box::new(|_| {}),
        )
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
    }

    #[test]
    fn resume_after_interrupted_tool_call() {
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

        assert!(needs_continuation(&context.messages));

        let result = run(
            &registry,
            &model,
            &mut context,
            &mut worker,
            &StreamOptions::default(),
            &config,
            &[],
            Box::new(|_| {}),
        )
        .unwrap();

        assert_eq!(result.new_messages.len(), 1);
        assert!(
            matches!(&result.new_messages[0], Message::Assistant(a) if a.text().contains("tool returned"))
        );
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
}
