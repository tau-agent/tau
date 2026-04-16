//! Agent loop -- stream LLM response, execute tool calls, repeat.
//!
//! The loop continues until the LLM stops without tool calls, or an
//! unrecoverable error occurs. Every `review_interval` turns, an inline
//! LLM call reviews the last few messages to detect stuck loops. If stuck,
//! a nudge message is injected; otherwise the loop continues uninterrupted.

use crate::provider::{EventReceiver, EventSender, ProviderRegistry};

use tau_agent_base::types::*;
use tau_agent_plugin::ToolExecutor;

/// Configuration for the agent loop.
pub struct AgentConfig {
    /// Number of turns between loop-review checkpoints.
    /// Every `review_interval` turns, the agent pauses and asks a reviewer LLM
    /// whether the session is making progress or stuck in a loop.
    pub review_interval: usize,
    /// Maximum retries for transient errors (429, 529, 5xx).
    pub max_retries: usize,
    /// Base delay for retry backoff in milliseconds.
    pub retry_base_ms: u64,
    /// Idle timeout for SSE stream chunks in seconds.
    /// If no event arrives within this period, the stream is aborted and
    /// the request is eligible for retry.
    pub idle_timeout_secs: u64,
    /// Optional shutdown check — if returns true, stop after current turn.
    pub should_stop: Option<Box<dyn Fn() -> bool + Send + Sync>>,
    /// Callback to drain queued messages for this session.
    /// Called at the top of each turn (after tool results, before next LLM call).
    /// Returns persisted `Message::User` entries that should be added to context.
    #[allow(clippy::type_complexity)]
    pub drain_queued: Option<Box<dyn Fn() -> Vec<Message> + Send + Sync>>,
    /// Called for each message produced by the agent (assistant, tool result, steering).
    /// Used for incremental persistence so partial progress survives errors.
    /// Wrapped in Mutex so AgentConfig is Send+Sync across async boundaries.
    #[allow(clippy::type_complexity)]
    pub on_message: Option<std::sync::Mutex<Box<dyn FnMut(&Message) + Send>>>,
    /// Callback to refresh the API key after an auth error (e.g. expired OAuth token).
    /// Called on 401/auth errors before retrying. Returns the new API key if refresh succeeded.
    pub refresh_api_key: Option<Box<dyn Fn() -> Option<String> + Send + Sync>>,
    /// Model to use for loop-review checkpoints. If `None`, uses the session's own model.
    pub review_model: Option<Model>,
}

/// Default idle timeout for SSE stream chunks (90 seconds).
const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 90;

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            review_interval: 50,
            max_retries: 10,
            retry_base_ms: 500,
            idle_timeout_secs: DEFAULT_IDLE_TIMEOUT_SECS,
            should_stop: None,
            drain_queued: None,
            on_message: None,
            refresh_api_key: None,
            review_model: None,
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
            stubs.push(Message::ToolResult(ToolResultMessage::error(
                *id,
                *name,
                "error: session interrupted before execution",
            )));
        }
    }

    stubs
}

/// Async cancellable sleep.
/// Returns `Err(Cancelled)` if should_stop returns true during the sleep.
async fn cancellable_sleep(
    delay: std::time::Duration,
    should_stop: Option<&(dyn Fn() -> bool + Send + Sync)>,
) -> tau_agent_base::Result<()> {
    let deadline = std::time::Instant::now() + delay;
    loop {
        if should_stop.is_some_and(|f| f()) {
            return Err(tau_agent_base::Error::Cancelled);
        }
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Ok(());
        }
        let chunk = remaining.min(std::time::Duration::from_millis(100));
        smol::Timer::after(chunk).await;
    }
}

/// Emit a message via the on_message callback, if configured.
fn emit_message(config: &AgentConfig, msg: &Message) {
    if let Some(ref on_msg) = config.on_message {
        on_msg.lock().expect("on_message mutex poisoned")(msg);
    }
}

/// Run the agent loop.
///
/// Streams LLM responses, executes tool calls via the worker,
/// and loops until the model stops or is cancelled.
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
) -> tau_agent_base::Result<AgentResult> {
    let mut new_messages = Vec::new();

    // All tools come from plugins (session + global)
    context.tools = extra_tools.to_vec();

    let mut turn: usize = 0;
    loop {
        // Drain any pending queued messages before the next LLM call.
        // These are user messages injected mid-loop via Request::Steer or
        // child session completion notifications.
        if let Some(ref drain_fn) = config.drain_queued {
            let queued = drain_fn();
            for msg in queued {
                if let Message::User(ref user_msg) = msg {
                    let _ = event_tx.try_send(StreamEvent::SteerMessage {
                        message: user_msg.clone(),
                    });
                }
                // Messages are already persisted by drain_queued_messages,
                // so do NOT call on_message here.
                new_messages.push(msg.clone());
                context.messages.push(msg);
            }
        }

        // Signal that we're about to call the LLM (so the UI can update the phase).
        let _ = event_tx.try_send(StreamEvent::Phase {
            phase: tau_agent_base::types::AgentPhase::Connecting,
        });

        // Stream LLM response (with retry)
        let message =
            match stream_with_retry(registry, model, context, options, config, &event_tx).await {
                Err(tau_agent_base::Error::Cancelled) => {
                    return Ok(AgentResult {
                        new_messages,
                        max_turns_reached: false,
                    });
                }
                other => other?,
            };

        let stop_reason = message.stop_reason;
        let assistant_msg = Message::Assistant(message.clone());
        emit_message(config, &assistant_msg);
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
                    let stub = tau_agent_base::types::ToolResultMessage::error(
                        remaining_tc.id.clone(),
                        remaining_tc.name.clone(),
                        "error: cancelled before execution",
                    );
                    let _ = event_tx.try_send(StreamEvent::ToolResult {
                        tool_call_id: remaining_tc.id.clone(),
                        tool_name: remaining_tc.name.clone(),
                        is_error: true,
                        content: "error: cancelled before execution".into(),
                        summary: None,
                    });
                    let tool_msg = Message::ToolResult(stub.clone());
                    emit_message(config, &tool_msg);
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
            let started_at = std::time::Instant::now();
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
            let elapsed_ms = started_at.elapsed().as_millis() as u64;
            let mut result = match result {
                Ok(r) => r,
                Err(e) => tau_agent_base::types::ToolResultMessage::error(
                    tc.id.clone(),
                    tc.name.clone(),
                    format!("error: {}", e),
                ),
            };
            result.duration_ms = Some(elapsed_ms);

            // Emit full tool result
            let content = result
                .content
                .iter()
                .filter_map(|c| match c {
                    tau_agent_base::types::ToolResultContent::Text(t) => Some(t.text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            let _ = event_tx.try_send(StreamEvent::ToolResult {
                tool_call_id: tc.id.clone(),
                tool_name: result.tool_name.clone(),
                is_error: result.is_error,
                content,
                summary: result.summary.clone(),
            });

            let tool_msg = Message::ToolResult(result.clone());
            emit_message(config, &tool_msg);
            new_messages.push(tool_msg);
            context.messages.push(Message::ToolResult(result));
        }

        // Check if we should stop (shutdown)
        if config.should_stop.as_ref().is_some_and(|f| f()) {
            return Ok(AgentResult {
                new_messages,
                max_turns_reached: false,
            });
        }

        turn += 1;

        // Loop-review checkpoint: every review_interval turns, ask a reviewer
        // LLM whether the session is making progress or stuck in a loop.
        if config.review_interval > 0 && turn.is_multiple_of(config.review_interval) {
            let review_model = config.review_model.as_ref().unwrap_or(model);
            let is_stuck = run_loop_review(
                registry,
                review_model,
                &context.messages,
                options,
                config,
                &event_tx,
            )
            .await;

            if is_stuck {
                // Inject a nudge message so the LLM knows it should change approach.
                let nudge = Message::User(UserMessage {
                    content: vec![UserContent::Text(TextContent {
                        text: "You seem to be stuck in a loop repeating the same actions. \
                               Step back, reassess your approach, and try a different strategy."
                            .into(),
                        text_signature: None,
                    })],
                    timestamp: tau_agent_base::types::timestamp_ms(),
                });
                emit_message(config, &nudge);
                let _ = event_tx.try_send(StreamEvent::Status {
                    message: format!(
                        "Loop review at turn {}: session appears stuck, injecting nudge.",
                        turn
                    ),
                });
                new_messages.push(nudge.clone());
                context.messages.push(nudge);
            } else {
                let _ = event_tx.try_send(StreamEvent::Status {
                    message: format!(
                        "Loop review at turn {}: session making progress, continuing.",
                        turn
                    ),
                });
            }
        }
    }
}

/// System prompt for the loop-review LLM call.
const LOOP_REVIEW_SYSTEM_PROMPT: &str = "\
You are a progress reviewer. You will be shown the last few messages from an AI coding assistant session.

Your job: determine whether the session is making progress or is stuck in a loop (repeating the same actions without advancing).

Respond with EXACTLY one word:
- PROGRESS if the session is making meaningful progress
- STUCK if the session is repeating itself or going in circles";

/// Number of recent messages to include in the loop review context.
const LOOP_REVIEW_MESSAGE_COUNT: usize = 6;

/// Run an inline LLM call to review whether the session is stuck.
/// Returns `true` if the session appears stuck.
async fn run_loop_review(
    registry: &ProviderRegistry,
    review_model: &Model,
    messages: &[Message],
    options: &StreamOptions,
    config: &AgentConfig,
    event_tx: &EventSender,
) -> bool {
    let _ = event_tx.try_send(StreamEvent::Phase {
        phase: tau_agent_base::types::AgentPhase::Compacting,
    });

    // Extract the last N messages for review.
    let start = messages.len().saturating_sub(LOOP_REVIEW_MESSAGE_COUNT);
    let recent: Vec<Message> = messages[start..].to_vec();

    // Format them as a single user message for the reviewer.
    let mut review_text = String::from("Here are the last messages from the session:\n\n");
    for (i, msg) in recent.iter().enumerate() {
        review_text.push_str(&format!("--- Message {} ---\n", i + 1));
        match msg {
            Message::User(u) => {
                review_text.push_str("Role: User\n");
                for c in &u.content {
                    if let UserContent::Text(t) = c {
                        review_text.push_str(&t.text);
                        review_text.push('\n');
                    }
                }
            }
            Message::Assistant(a) => {
                review_text.push_str("Role: Assistant\n");
                for c in &a.content {
                    match c {
                        AssistantContent::Text(t) => {
                            review_text.push_str(&t.text);
                            review_text.push('\n');
                        }
                        AssistantContent::ToolCall(tc) => {
                            review_text
                                .push_str(&format!("Tool call: {}({})\n", tc.name, tc.arguments));
                        }
                        _ => {}
                    }
                }
            }
            Message::ToolResult(tr) => {
                review_text.push_str(&format!("Role: ToolResult ({})\n", tr.tool_name));
                let content: String = tr
                    .content
                    .iter()
                    .filter_map(|c| match c {
                        ToolResultContent::Text(t) => Some(t.text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                // Truncate long tool results for the review.
                if content.len() > 1000 {
                    review_text.push_str(&content[..1000]);
                    review_text.push_str("\n[...truncated...]\n");
                } else {
                    review_text.push_str(&content);
                    review_text.push('\n');
                }
            }
            Message::CompactionSummary(s) => {
                review_text.push_str("Role: CompactionSummary\n");
                review_text.push_str(&s.summary);
                review_text.push('\n');
            }
            Message::Info(_) => {
                // Info messages are display-only; skip in loop review.
            }
        }
        review_text.push('\n');
    }
    review_text.push_str("Is this session making PROGRESS or is it STUCK?");

    let review_context = Context {
        system_prompt: Some(LOOP_REVIEW_SYSTEM_PROMPT.into()),
        messages: vec![Message::User(UserMessage {
            content: vec![UserContent::Text(TextContent {
                text: review_text,
                text_signature: None,
            })],
            timestamp: tau_agent_base::types::timestamp_ms(),
        })],
        tools: vec![],
    };

    // Use a minimal StreamOptions — no thinking, just a quick text response.
    let review_options = StreamOptions {
        api_key: options.api_key.clone(),
        headers: options.headers.clone(),
        max_tokens: Some(16),
        temperature: Some(0.0),
        thinking_budget: None,
        thinking_enabled: Some(false),
        thinking_effort: None,
        thinking_display: None,
    };

    // Single-shot LLM call with retry.
    let (discard_tx, _discard_rx) = smol::channel::unbounded();
    let result = stream_with_retry(
        registry,
        review_model,
        &review_context,
        &review_options,
        config,
        &discard_tx,
    )
    .await;

    match result {
        Ok(msg) => {
            let text: String = msg
                .content
                .iter()
                .filter_map(|c| match c {
                    AssistantContent::Text(t) => Some(t.text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("");
            let verdict = text.trim().to_uppercase();
            verdict.contains("STUCK")
        }
        Err(e) => {
            // If the review call fails, log and assume progress (don't block the loop).
            eprintln!("loop review failed, assuming progress: {}", e);
            false
        }
    }
}

/// Maximum retry delay in milliseconds (32 seconds).
const MAX_RETRY_DELAY_MS: u64 = 32_000;

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
) -> tau_agent_base::Result<AssistantMessage> {
    let max_attempts = config.max_retries + 1;
    /// Maximum number of auth-refresh retries before giving up.
    const MAX_AUTH_RETRIES: usize = 3;
    // Track how many times we've retried after auth refresh.
    let mut auth_retry_count: usize = 0;
    // Owned copy of options so we can update api_key on auth refresh.
    let mut options = options.clone();

    for attempt in 0..max_attempts {
        let rx = registry.stream(model, context, &options)?;
        let should_stop = config.should_stop.as_deref();
        let idle_timeout = std::time::Duration::from_secs(config.idle_timeout_secs);
        let message = consume_stream(rx, event_tx, should_stop, idle_timeout).await;

        // Propagate cancellation immediately — no retry.
        // Convert idle-timeout / transport errors into an error AssistantMessage
        // so the retry logic below (which checks stop_reason + error_message)
        // can handle them uniformly with other timeout/retryable errors.
        //
        // Track whether the error message was *synthesised* from a transport-
        // level failure (i.e. `consume_stream` returned Err and never forwarded
        // a terminating StreamEvent on `event_tx`). In that case we must emit
        // a `StreamEvent::Error` ourselves before returning, so the TUI / other
        // subscribers see a terminator for the in-flight streaming item.
        let mut synthesised_from_transport_error = false;
        let message = match message {
            Err(tau_agent_base::Error::Cancelled) => return Err(tau_agent_base::Error::Cancelled),
            Err(tau_agent_base::Error::Http(ref msg)) if is_timeout(msg) => {
                let err_msg = msg.clone();
                let mut m = AssistantMessage::empty(&model.api, &model.provider, &model.id);
                m.stop_reason = StopReason::Error;
                m.error_message = Some(err_msg);
                synthesised_from_transport_error = true;
                m
            }
            other => other?,
        };

        if message.stop_reason == StopReason::Error
            && let Some(ref err_msg) = message.error_message
        {
            // Auth errors (401 / expired token): refresh and retry up to MAX_AUTH_RETRIES times.
            if auth_retry_count < MAX_AUTH_RETRIES
                && is_auth_error(err_msg)
                && let Some(ref refresh_fn) = config.refresh_api_key
                && let Some(new_key) = refresh_fn()
            {
                auth_retry_count += 1;
                options.api_key = Some(new_key);
                let status_msg = format!(
                    "auth error, refreshed token, retrying ({}/{}): {}",
                    auth_retry_count, MAX_AUTH_RETRIES, err_msg
                );
                eprintln!("{}", status_msg);
                let _ = event_tx.try_send(StreamEvent::Status {
                    message: status_msg,
                });
                continue;
            }

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
                    // Rate limit / 5xx: use retry-after header or exponential backoff
                    // with 25% subtractive jitter.
                    parse_retry_after(err_msg)
                        .map(|secs| secs * 1000)
                        .unwrap_or_else(|| {
                            let raw = (config.retry_base_ms * 2u64.pow(attempt as u32))
                                .min(MAX_RETRY_DELAY_MS);
                            let jitter = (raw as f64
                                * 0.25
                                * (std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .expect("system clock before unix epoch")
                                    .subsec_nanos() as f64
                                    / 1_000_000_000.0))
                                as u64;
                            raw - jitter
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
                    phase: tau_agent_base::types::AgentPhase::RateLimited,
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
                // Return the error — caller (server) handles overflow recovery.
                // No need to synthesise a StreamEvent::Error here: the server
                // recovers from overflow and the TUI never sees this path.
                return Ok(message);
            }
        }

        // If we synthesised this error from a transport-level failure (i.e.
        // `consume_stream` returned Err before any Done/Error event was
        // forwarded), the channel never received a terminator for the
        // in-flight streaming item. Emit one now so the TUI can finalise
        // the placeholder and show the error to the user instead of leaving
        // a stuck spinner.
        if synthesised_from_transport_error && message.stop_reason == StopReason::Error {
            let _ = event_tx.try_send(StreamEvent::Error {
                reason: StopReason::Error,
                error: message.clone(),
            });
        }

        return Ok(message);
    }

    // The retry loop always returns from inside its body — this is unreachable
    // in practice, but if we ever fall through, surface a clear error.
    Err(tau_agent_base::Error::Http("max retries exceeded".into()))
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
    idle_timeout: std::time::Duration,
) -> tau_agent_base::Result<AssistantMessage> {
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
            return Err(tau_agent_base::Error::Cancelled);
        }

        // Race recv() against an idle timer so we detect stalled streams.
        let recv_or_timeout = smol::future::or(async { Some(rx.recv().await) }, async {
            smol::Timer::after(idle_timeout).await;
            None
        })
        .await;

        match recv_or_timeout {
            None => {
                // Idle timeout — no SSE event arrived in time.
                return Err(tau_agent_base::Error::Http(format!(
                    "idle timeout: no SSE event received within {}s",
                    idle_timeout.as_secs()
                )));
            }
            Some(Ok(event)) => {
                let is_done =
                    matches!(&event, StreamEvent::Done { .. } | StreamEvent::Error { .. });
                let final_msg = match &event {
                    StreamEvent::Done { message, .. } => Some(message.clone()),
                    StreamEvent::Error { error, .. } => Some(error.clone()),
                    _ => None,
                };
                let _ = event_tx.try_send(event);
                if is_done {
                    return final_msg.ok_or(tau_agent_base::Error::ChannelClosed);
                }
            }
            Some(Err(_)) => return Err(tau_agent_base::Error::ChannelClosed),
        }
    }
}

/// Check if an error message indicates an authentication/authorization error
/// (expired token, invalid credentials, HTTP 401).
fn is_auth_error(err_msg: &str) -> bool {
    let lower = err_msg.to_lowercase();
    lower.contains("401")
        || lower.contains("authentication_error")
        || lower.contains("token has expired")
        || lower.contains("token expired")
        || lower.contains("unauthorized")
        || lower.contains("invalid.*token")
        || lower.contains("expired token")
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
        || lower.contains("ended without")
}

/// Check if an error indicates context window overflow.
pub fn is_context_overflow(err_msg: &str) -> bool {
    let lower = err_msg.to_lowercase();
    lower.contains("context_length_exceeded")
        || lower.contains("maximum context length")
        || lower.contains("too many tokens")
        || lower.contains("prompt is too long")
        || lower.contains("prompt too long")
        || lower.contains("request_too_large")
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
    use tau_agent_plugin_worker::InProcessWorker;

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
    fn loop_review_progress_continues() {
        // review_interval=3: 3 tool-call turns, then review says PROGRESS,
        // then 1 more turn that ends with Text (natural stop).
        smol::block_on(async {
            let mut responses: Vec<MockResponse> = Vec::new();
            // Turns 0, 1, 2 — tool calls
            for i in 0..3 {
                responses.push(MockResponse::ToolCalls(vec![ToolCall {
                    id: format!("tc{}", i),
                    name: "bash".into(),
                    arguments: serde_json::json!({"command": "echo loop"}),
                }]));
            }
            // Review call after turn 2 — says PROGRESS
            responses.push(MockResponse::Text("PROGRESS".into()));
            // Turn 3 — natural stop
            responses.push(MockResponse::Text("All done.".into()));

            let registry = setup_registry(responses);
            let model = mock_model();
            let mut context = basic_context();
            let config = AgentConfig {
                review_interval: 3,
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

            assert!(!result.max_turns_reached);
            // 3 tool turns (assistant + tool_result each = 6) + 1 final assistant = 7
            assert_eq!(result.new_messages.len(), 7);
        });
    }

    #[test]
    fn loop_review_stuck_injects_nudge() {
        // review_interval=2: 2 tool-call turns, then review says STUCK,
        // nudge is injected, then 1 more turn that ends with Text.
        smol::block_on(async {
            let mut responses: Vec<MockResponse> = Vec::new();
            // Turns 0, 1 — tool calls
            for i in 0..2 {
                responses.push(MockResponse::ToolCalls(vec![ToolCall {
                    id: format!("tc{}", i),
                    name: "bash".into(),
                    arguments: serde_json::json!({"command": "echo loop"}),
                }]));
            }
            // Review call — says STUCK
            responses.push(MockResponse::Text("STUCK".into()));
            // Turn 2 — natural stop (after nudge was injected)
            responses.push(MockResponse::Text("Trying different approach.".into()));

            let registry = setup_registry(responses);
            let model = mock_model();
            let mut context = basic_context();
            let config = AgentConfig {
                review_interval: 2,
                ..Default::default()
            };
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

            assert!(!result.max_turns_reached);
            // 2 tool turns (4 msgs) + 1 nudge + 1 final assistant = 6
            assert_eq!(result.new_messages.len(), 6);
            // The nudge should be a User message
            assert!(matches!(result.new_messages[4], Message::User(_)));
            if let Message::User(ref u) = result.new_messages[4] {
                let text = match &u.content[0] {
                    UserContent::Text(t) => &t.text,
                    _ => panic!("expected text content"),
                };
                assert!(text.contains("stuck in a loop"));
            }

            // Check that a Status event was emitted mentioning "stuck"
            let mut found_stuck_status = false;
            while let Ok(event) = rx.try_recv() {
                if let StreamEvent::Status { message } = event
                    && message.contains("stuck")
                {
                    found_stuck_status = true;
                }
            }
            assert!(
                found_stuck_status,
                "expected a Status event about being stuck"
            );
        });
    }

    #[test]
    fn loop_review_disabled_when_interval_zero() {
        // review_interval=0 means no review — loop should run until
        // there are no more responses (which causes an error and stops).
        smol::block_on(async {
            let responses = vec![
                MockResponse::ToolCalls(vec![ToolCall {
                    id: "tc0".into(),
                    name: "bash".into(),
                    arguments: serde_json::json!({"command": "echo hi"}),
                }]),
                MockResponse::Text("Done.".into()),
            ];
            let registry = setup_registry(responses);
            let model = mock_model();
            let mut context = basic_context();
            let config = AgentConfig {
                review_interval: 0,
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

            assert!(!result.max_turns_reached);
            // 1 tool turn (2 msgs) + 1 final text = 3
            assert_eq!(result.new_messages.len(), 3);
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
                    Message::ToolResult(ToolResultMessage::success("tc1", "bash", "hello\n")),
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
        // Provider-agnostic premature-close ("ended without sending chunks") —
        // observed on both Anthropic and OpenAI streams.
        assert!(is_retryable("request ended without sending chunks"));
        assert!(is_retryable("stream Ended Without response"));
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
    fn auth_errors() {
        assert!(is_auth_error("HTTP 401 Unauthorized"));
        assert!(is_auth_error("authentication_error: invalid token"));
        assert!(is_auth_error("OAuth token has expired"));
        assert!(is_auth_error("token expired"));
        assert!(is_auth_error("401: unauthorized"));
        assert!(is_auth_error("expired token"));
        // Should not match unrelated errors
        assert!(!is_auth_error("HTTP 429 rate limit"));
        assert!(!is_auth_error("503 Service Unavailable"));
        assert!(!is_auth_error("timeout: connect timed out"));
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
        // Anthropic HTTP 413 request_too_large — byte-size overflow, not token count.
        assert!(is_context_overflow(
            "HTTP 413: {\"type\":\"error\",\"error\":{\"type\":\"request_too_large\",\"message\":\"Request body is too large\"}}"
        ));
        assert!(is_context_overflow("Request_Too_Large"));
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
                        Message::Info(_) => "info",
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
                        Message::Info(_) => "info",
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
            Message::ToolResult(ToolResultMessage::success("tc1", "bash", "ok")),
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
            Message::ToolResult(ToolResultMessage::success("tc1", "bash", "ok")),
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

    // -----------------------------------------------------------------------
    // Context capture tests (using MockProviderHandle)
    // -----------------------------------------------------------------------

    fn setup_registry_with_handle(
        responses: Vec<MockResponse>,
    ) -> (ProviderRegistry, MockProviderHandle) {
        let provider = MockProvider::new(responses);
        let handle = provider.handle();
        let mut registry = ProviderRegistry::new();
        registry.register(provider);
        (registry, handle)
    }

    #[test]
    fn capture_context_on_each_turn() {
        smol::block_on(async {
            let (registry, handle) = setup_registry_with_handle(vec![
                MockResponse::ToolCalls(vec![ToolCall {
                    id: "tc1".into(),
                    name: "bash".into(),
                    arguments: serde_json::json!({"command": "echo hi"}),
                }]),
                MockResponse::Text("Done.".into()),
            ]);
            let model = mock_model();
            let mut context = basic_context();
            let config = AgentConfig::default();
            let mut worker = InProcessWorker::new();
            let (tx, _rx) = smol::channel::unbounded();

            run(
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

            let captures = handle.captures();
            assert_eq!(
                captures.len(),
                2,
                "should capture context for each LLM call"
            );

            // First call: just user message
            assert_eq!(captures[0].index, 0);
            assert_eq!(captures[0].context.messages.len(), 1);
            assert!(matches!(&captures[0].context.messages[0], Message::User(_)));

            // Second call: user + assistant(tool_call) + tool_result
            assert_eq!(captures[1].index, 1);
            assert!(captures[1].context.messages.len() >= 3);
            assert!(matches!(
                &captures[1].context.messages[1],
                Message::Assistant(_)
            ));
            assert!(matches!(
                &captures[1].context.messages[2],
                Message::ToolResult(_)
            ));

            // Turn duration should be measurable
            let durations = handle.turn_durations();
            assert_eq!(durations.len(), 1);
            assert!(durations[0] < std::time::Duration::from_secs(10));
        });
    }

    #[test]
    fn capture_system_prompt_in_context() {
        smol::block_on(async {
            let (registry, handle) =
                setup_registry_with_handle(vec![MockResponse::Text("Hello!".into())]);
            let model = mock_model();
            let mut context = Context {
                system_prompt: Some("You are a test assistant.".into()),
                messages: vec![Message::User(UserMessage::text("hi"))],
                tools: Vec::new(),
            };
            let config = AgentConfig::default();
            let mut worker = InProcessWorker::new();
            let (tx, _rx) = smol::channel::unbounded();

            run(
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

            let captures = handle.captures();
            assert_eq!(captures.len(), 1);
            assert_eq!(
                captures[0].context.system_prompt.as_deref(),
                Some("You are a test assistant.")
            );
        });
    }

    #[test]
    fn test_idle_timeout() {
        smol::block_on(async {
            // Provide enough Hang responses for the initial attempt plus
            // MAX_TIMEOUT_RETRIES (2) retries, totalling 3 attempts.
            let registry = setup_registry(vec![
                MockResponse::Hang,
                MockResponse::Hang,
                MockResponse::Hang,
            ]);
            let model = mock_model();
            let mut context = basic_context();
            let config = AgentConfig {
                idle_timeout_secs: 0, // Use 0s so the timer fires on the next poll
                ..Default::default()
            };
            let mut worker = InProcessWorker::new();
            let (tx, _rx) = smol::channel::unbounded();

            let start = std::time::Instant::now();
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

            let elapsed = start.elapsed();
            // Should complete quickly — all timeouts fire immediately.
            assert!(
                elapsed < std::time::Duration::from_secs(10),
                "idle timeout should fire quickly, took {:?}",
                elapsed
            );
            // After exhausting timeout retries, the error message is returned.
            assert_eq!(result.new_messages.len(), 1);
            assert!(matches!(&result.new_messages[0], Message::Assistant(a)
                    if a.stop_reason == StopReason::Error
                    && a.error_message.as_ref().unwrap().contains("idle timeout")));
        });
    }

    /// Regression test for the "stuck spinner on transport timeout" bug.
    ///
    /// When `consume_stream` returns `Err(Error::Http(...))` due to an idle
    /// timeout (i.e. the provider stalled mid-stream and never sent a Done /
    /// Error event), `stream_with_retry` synthesises an error AssistantMessage
    /// to drive its retry logic. After exhausting `MAX_TIMEOUT_RETRIES`, the
    /// agent loop persists that synthesised message and returns Ok — but it
    /// must *also* deliver a `StreamEvent::Error` on the event channel, so
    /// the TUI can finalise its in-flight streaming placeholder. Without
    /// the terminator the TUI shows a stuck spinner forever.
    #[test]
    fn timeout_exhaustion_emits_stream_event_error() {
        smol::block_on(async {
            // 3 hangs = initial attempt + MAX_TIMEOUT_RETRIES (2) retries.
            let registry = setup_registry(vec![
                MockResponse::Hang,
                MockResponse::Hang,
                MockResponse::Hang,
            ]);
            let model = mock_model();
            let mut context = basic_context();
            let config = AgentConfig {
                idle_timeout_secs: 0, // fire on next poll
                ..Default::default()
            };
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
            .expect("agent run should return Ok with synthesised error message");

            // The errored AssistantMessage was persisted via emit_message and
            // is also reflected in result.new_messages.
            assert_eq!(result.new_messages.len(), 1);
            let persisted_err = match &result.new_messages[0] {
                Message::Assistant(a) => a,
                _ => panic!("expected an Assistant message"),
            };
            assert_eq!(persisted_err.stop_reason, StopReason::Error);
            assert!(
                persisted_err
                    .error_message
                    .as_deref()
                    .is_some_and(|m| m.contains("idle timeout")),
                "expected an idle-timeout error_message, got {:?}",
                persisted_err.error_message
            );

            // The whole point of the fix: a StreamEvent::Error must have been
            // delivered on the event channel before run returned, carrying the
            // same error text. Otherwise the TUI's streaming placeholder is
            // never finalised and the spinner appears stuck.
            let mut events = Vec::new();
            while let Ok(e) = rx.try_recv() {
                events.push(e);
            }
            let error_events: Vec<&AssistantMessage> = events
                .iter()
                .filter_map(|e| match e {
                    StreamEvent::Error { error, .. } => Some(error),
                    _ => None,
                })
                .collect();
            assert_eq!(
                error_events.len(),
                1,
                "expected exactly one StreamEvent::Error after timeout exhaustion, got {}",
                error_events.len()
            );
            let err_msg = error_events[0]
                .error_message
                .as_deref()
                .expect("error event AssistantMessage should carry an error_message");
            assert!(
                err_msg.contains("idle timeout"),
                "StreamEvent::Error should carry the idle-timeout text, got {:?}",
                err_msg
            );

            // Sanity: retries should still emit Status events for UX.
            let status_events: Vec<&String> = events
                .iter()
                .filter_map(|e| match e {
                    StreamEvent::Status { message } => Some(message),
                    _ => None,
                })
                .collect();
            assert!(
                status_events.iter().any(|m| m.contains("timeout")),
                "expected at least one Status event mentioning timeout, got {:?}",
                status_events
            );
        });
    }

    #[test]
    fn test_idle_timeout_triggers_retry() {
        smol::block_on(async {
            // First call hangs (idle timeout), second succeeds.
            let registry = setup_registry(vec![
                MockResponse::Hang,
                MockResponse::Text("recovered after idle timeout!".into()),
            ]);
            let model = mock_model();
            let mut context = basic_context();
            // Use a small but non-zero timeout so the Text response (sent
            // instantly by the mock thread) arrives before the watchdog fires.
            let config = AgentConfig {
                idle_timeout_secs: 1,
                ..Default::default()
            };
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
            assert!(matches!(&result.new_messages[0], Message::Assistant(a)
                    if a.text() == "recovered after idle timeout!"));

            // Check that a status event was emitted about the timeout retry.
            let mut events = Vec::new();
            while let Ok(e) = rx.try_recv() {
                events.push(e);
            }
            let has_timeout_status = events.iter().any(
                |e| matches!(e, StreamEvent::Status { message } if message.contains("timeout")),
            );
            assert!(
                has_timeout_status,
                "should emit status about idle timeout retry"
            );
        });
    }

    #[test]
    fn no_continuation_needed_after_info() {
        let messages = vec![
            Message::User(UserMessage::text("hi")),
            Message::Assistant(AssistantMessage::empty("mock", "mock", "mock-model")),
            Message::Info(tau_agent_base::types::InfoMessage {
                text: "task state changed".into(),
                timestamp: 0,
            }),
        ];
        assert!(!needs_continuation(&messages));
    }

    #[test]
    fn repair_ignores_trailing_info() {
        // Trailing Info message should not trigger repair
        let messages = vec![
            Message::User(UserMessage::text("hi")),
            Message::Assistant(AssistantMessage::empty("mock", "mock", "mock-model")),
            Message::Info(tau_agent_base::types::InfoMessage {
                text: "some notification".into(),
                timestamp: 0,
            }),
        ];
        assert!(repair_messages(&messages).is_empty());
    }
}
