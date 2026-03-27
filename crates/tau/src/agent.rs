//! Agent loop — stream LLM response, execute tool calls, repeat.
//!
//! The loop continues until the LLM stops without tool calls, or an
//! unrecoverable error occurs, or max_turns is reached.

use crate::provider::{EventReceiver, ProviderRegistry};
use crate::tools::{self, ToolDef};
use crate::types::*;

/// Configuration for the agent loop.
pub struct AgentConfig {
    /// Maximum number of LLM turns (each tool-call-and-response is one turn).
    pub max_turns: usize,
    /// Maximum retries for transient errors (429, 529, 5xx).
    pub max_retries: usize,
    /// Base delay for retry backoff in milliseconds.
    pub retry_base_ms: u64,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_turns: 50,
            max_retries: 5,
            retry_base_ms: 1000,
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

/// Run the agent loop.
///
/// Streams LLM responses, executes tool calls, and loops until the model
/// stops or max_turns is reached. All stream events are forwarded via `on_event`.
pub fn run(
    registry: &ProviderRegistry,
    model: &Model,
    context: &mut Context,
    tool_defs: &[ToolDef],
    options: &StreamOptions,
    config: &AgentConfig,
    mut on_event: EventCallback,
) -> crate::Result<AgentResult> {
    let mut new_messages = Vec::new();

    // Add tool schemas to context
    context.tools = tools::tool_schemas(tool_defs);

    for turn in 0..config.max_turns {
        // Stream LLM response (with retry)
        let message = stream_with_retry(registry, model, context, options, config, &mut on_event)?;

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
            let result = tools::execute_tool(tool_defs, tc);
            new_messages.push(Message::ToolResult(result.clone()));
            context.messages.push(Message::ToolResult(result));
        }

        // Check if this was the last turn
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
        let message = consume_stream(rx, on_event)?;

        if message.stop_reason == StopReason::Error
            && let Some(ref err_msg) = message.error_message
        {
            if is_retryable(err_msg) && attempt < config.max_retries {
                let delay = config.retry_base_ms * 2u64.pow(attempt as u32);
                eprintln!(
                    "retryable error (attempt {}/{}), retrying in {}ms: {}",
                    attempt + 1,
                    config.max_retries,
                    delay,
                    err_msg
                );
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

/// Consume a stream, forwarding events and returning the final message.
fn consume_stream(
    rx: EventReceiver,
    on_event: &mut EventCallback,
) -> crate::Result<AssistantMessage> {
    loop {
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
        || lower.contains("max_tokens") && (lower.contains("exceed") || lower.contains("too long"))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::mock::*;

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

        let result = run(
            &registry,
            &model,
            &mut context,
            &[],
            &StreamOptions::default(),
            &config,
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
            // Turn 1: model calls a tool
            MockResponse::ToolCalls(vec![ToolCall {
                id: "tc1".into(),
                name: "bash".into(),
                arguments: serde_json::json!({"command": "echo hi"}),
            }]),
            // Turn 2: model responds with text after seeing tool result
            MockResponse::Text("The command output 'hi'.".into()),
        ]);
        let model = mock_model();
        let mut context = basic_context();
        let config = AgentConfig::default();
        let tools = crate::tools::default_tools();

        let result = run(
            &registry,
            &model,
            &mut context,
            &tools,
            &StreamOptions::default(),
            &config,
            Box::new(|_| {}),
        )
        .unwrap();

        // Should have: assistant(tool_call) + tool_result + assistant(text)
        assert_eq!(result.new_messages.len(), 3);
        assert!(matches!(&result.new_messages[0], Message::Assistant(_)));
        assert!(matches!(&result.new_messages[1], Message::ToolResult(_)));
        assert!(
            matches!(&result.new_messages[2], Message::Assistant(a) if a.text().contains("hi"))
        );
    }

    #[test]
    fn max_turns_limit() {
        // Model always returns tool calls — should hit max_turns
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
        let tools = crate::tools::default_tools();

        let result = run(
            &registry,
            &model,
            &mut context,
            &tools,
            &StreamOptions::default(),
            &config,
            Box::new(|_| {}),
        )
        .unwrap();

        assert!(result.max_turns_reached);
        // 3 turns × (assistant + tool_result) = 6 messages
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

        let result = run(
            &registry,
            &model,
            &mut context,
            &[],
            &StreamOptions::default(),
            &config,
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

        let result = run(
            &registry,
            &model,
            &mut context,
            &[],
            &StreamOptions::default(),
            &config,
            Box::new(|_| {}),
        )
        .unwrap();

        // assistant(tool_call) + tool_result(error) + assistant(text)
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
    fn retryable_errors() {
        assert!(is_retryable("HTTP 529 overloaded"));
        assert!(is_retryable("rate limit exceeded (429)"));
        assert!(is_retryable("503 Service Unavailable"));
        assert!(!is_retryable("invalid api key"));
    }

    #[test]
    fn overflow_detection() {
        assert!(is_context_overflow(
            "context_length_exceeded: maximum context length is 200000"
        ));
        assert!(is_context_overflow("maximum context length exceeded"));
        assert!(!is_context_overflow("invalid request"));
    }
}
