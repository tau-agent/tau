//! Session recording and replay.
//!
//! Dump a session's messages into a portable JSON recording, then replay it
//! against mock providers and tool executors to verify the agent loop
//! reproduces the same message structure.

use serde::{Deserialize, Serialize};

use crate::db::Db;
use crate::types::*;

// ---------------------------------------------------------------------------
// Recording format
// ---------------------------------------------------------------------------

/// A recorded session for replay testing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecording {
    /// Model used for the session.
    pub model: Model,
    /// System prompt, if any.
    pub system_prompt: Option<String>,
    /// Ordered turns in the conversation.
    pub turns: Vec<RecordedTurn>,
}

/// One turn in a recorded session.
///
/// A "turn" is one LLM call and its consequences:
/// - An optional user message that prompted it
/// - The assistant's response
/// - Any tool results that followed (before the next LLM call)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordedTurn {
    /// User message that started this turn (None for continuation turns,
    /// e.g. after tool results trigger another LLM call).
    pub user_message: Option<String>,
    /// The assistant's response.
    pub assistant_message: AssistantMessage,
    /// Tool results that followed (empty if no tool calls).
    pub tool_results: Vec<ToolResultMessage>,
}

// ---------------------------------------------------------------------------
// Dump: extract recording from DB
// ---------------------------------------------------------------------------

/// Extract a session recording from the database.
pub fn dump_session(db: &Db, session_id: &str) -> crate::Result<SessionRecording> {
    let session = db
        .get_session(session_id)?
        .ok_or_else(|| crate::Error::Io(format!("session not found: {}", session_id)))?;
    let messages = db.get_messages(session_id)?;

    let turns = extract_turns(&messages);

    Ok(SessionRecording {
        model: session.model,
        system_prompt: session.system_prompt,
        turns,
    })
}

/// Parse a flat message list into turns.
fn extract_turns(messages: &[Message]) -> Vec<RecordedTurn> {
    let mut turns = Vec::new();
    let mut current_user: Option<String> = None;

    for msg in messages {
        match msg {
            Message::User(u) => {
                let text = u
                    .content
                    .iter()
                    .filter_map(|c| match c {
                        UserContent::Text(t) => Some(t.text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("");
                current_user = Some(text);
            }
            Message::Assistant(a) => {
                turns.push(RecordedTurn {
                    user_message: current_user.take(),
                    assistant_message: a.clone(),
                    tool_results: Vec::new(),
                });
            }
            Message::ToolResult(tr) => {
                if let Some(turn) = turns.last_mut() {
                    turn.tool_results.push(tr.clone());
                }
            }
            Message::CompactionSummary(_) => {
                // Skip compaction summaries — they're an implementation detail
            }
            Message::Info(_) => {
                // Skip info messages — they're display-only notifications
            }
        }
    }

    turns
}

// ---------------------------------------------------------------------------
// Replay: build mocks from recording and run through agent loop
// ---------------------------------------------------------------------------

/// Result of replaying a session recording.
#[derive(Debug)]
pub struct ReplayResult {
    /// Whether the replay matched the recording.
    pub success: bool,
    /// Detailed comparison of each turn.
    pub turn_results: Vec<TurnComparison>,
    /// Overall error, if any.
    pub error: Option<String>,
}

/// Comparison of one turn between recorded and replayed.
#[derive(Debug)]
pub struct TurnComparison {
    pub turn_index: usize,
    /// Whether the assistant response text matched.
    pub text_match: bool,
    /// Whether the tool calls matched (same names and count).
    pub tool_calls_match: bool,
    /// Whether the tool results matched (same count and is_error flags).
    pub tool_results_match: bool,
    /// Details of any mismatch.
    pub details: Option<String>,
}

/// Replay a recorded session and verify the agent loop reproduces it.
///
/// This builds a MockProvider from the recorded assistant messages and a
/// MockToolExecutor from the recorded tool results, then runs the agent
/// loop and compares the output.
pub async fn replay_session(recording: &SessionRecording) -> ReplayResult {
    use crate::providers::mock::*;

    // Build mock provider responses from recording
    let mock_responses: Vec<MockResponse> = recording
        .turns
        .iter()
        .map(|turn| {
            let tool_calls: Vec<ToolCall> = turn
                .assistant_message
                .content
                .iter()
                .filter_map(|c| match c {
                    AssistantContent::ToolCall(tc) => Some(tc.clone()),
                    _ => None,
                })
                .collect();

            if !tool_calls.is_empty() {
                MockResponse::ToolCalls(tool_calls)
            } else {
                MockResponse::Text(turn.assistant_message.text())
            }
        })
        .collect();

    // Build mock tool responses from recording
    let mock_executor = MockToolExecutor::new();
    let tool_handle = mock_executor.handle();
    let mut tool_names = std::collections::HashSet::new();
    for turn in &recording.turns {
        for tr in &turn.tool_results {
            tool_names.insert(tr.tool_name.clone());
            let text = tr
                .content
                .iter()
                .filter_map(|c| match c {
                    ToolResultContent::Text(t) => Some(t.text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            let response = if tr.is_error {
                MockToolResponse::ToolError(text)
            } else {
                MockToolResponse::Success(text)
            };
            tool_handle.on_tool(&tr.tool_name, response);
        }
    }

    // Build tool schemas for all tools that appeared
    let tool_schemas: Vec<Tool> = tool_names
        .iter()
        .map(|name| mock_tool(name, &format!("Replayed tool: {}", name)))
        .collect();

    // Set up provider with capture
    let provider = MockProvider::new(mock_responses);
    let _provider_handle = provider.handle();
    let mut registry = crate::provider::ProviderRegistry::new();
    registry.register(provider);

    // Build context
    let mut context = Context {
        system_prompt: recording.system_prompt.clone(),
        messages: Vec::new(),
        tools: Vec::new(),
    };

    // Add initial user message from first turn
    if let Some(first_turn) = recording.turns.first()
        && let Some(text) = &first_turn.user_message
    {
        context
            .messages
            .push(Message::User(UserMessage::text(text)));
    }

    // Run agent loop
    let config = crate::agent::AgentConfig::default();
    let mut executor = tool_handle.executor();
    let options = StreamOptions::default();
    let (event_tx, _event_rx) = smol::channel::unbounded();

    let result = crate::agent::run(
        &registry,
        &recording.model,
        &mut context,
        &mut executor,
        &options,
        &config,
        &tool_schemas,
        event_tx,
    )
    .await;

    match result {
        Ok(agent_result) => {
            // Compare recorded turns with replayed messages
            let replayed_turns =
                extract_turns_from_new_messages(&agent_result.new_messages, &recording.turns);
            let turn_results = compare_turns(&recording.turns, &replayed_turns);
            let success = turn_results
                .iter()
                .all(|tc| tc.text_match && tc.tool_calls_match && tc.tool_results_match);
            ReplayResult {
                success,
                turn_results,
                error: None,
            }
        }
        Err(e) => ReplayResult {
            success: false,
            turn_results: Vec::new(),
            error: Some(format!("agent error: {}", e)),
        },
    }
}

/// Extract turns from the agent's new_messages output, using the recording
/// as reference for which messages are user messages vs continuations.
fn extract_turns_from_new_messages(
    new_messages: &[Message],
    _recorded_turns: &[RecordedTurn],
) -> Vec<RecordedTurn> {
    extract_turns(new_messages)
}

/// Compare recorded turns with replayed turns.
fn compare_turns(recorded: &[RecordedTurn], replayed: &[RecordedTurn]) -> Vec<TurnComparison> {
    let max_len = recorded.len().max(replayed.len());
    let mut results = Vec::new();

    for i in 0..max_len {
        let rec = recorded.get(i);
        let rep = replayed.get(i);

        match (rec, rep) {
            (Some(rec), Some(rep)) => {
                let rec_text = rec.assistant_message.text();
                let rep_text = rep.assistant_message.text();
                let text_match = rec_text == rep_text;

                let rec_tool_names: Vec<&str> = rec
                    .assistant_message
                    .content
                    .iter()
                    .filter_map(|c| match c {
                        AssistantContent::ToolCall(tc) => Some(tc.name.as_str()),
                        _ => None,
                    })
                    .collect();
                let rep_tool_names: Vec<&str> = rep
                    .assistant_message
                    .content
                    .iter()
                    .filter_map(|c| match c {
                        AssistantContent::ToolCall(tc) => Some(tc.name.as_str()),
                        _ => None,
                    })
                    .collect();
                let tool_calls_match = rec_tool_names == rep_tool_names;

                let tool_results_match = rec.tool_results.len() == rep.tool_results.len()
                    && rec
                        .tool_results
                        .iter()
                        .zip(rep.tool_results.iter())
                        .all(|(r, p)| r.is_error == p.is_error && r.tool_name == p.tool_name);

                let details = if !text_match || !tool_calls_match || !tool_results_match {
                    let mut d = Vec::new();
                    if !text_match {
                        d.push(format!(
                            "text mismatch: recorded={:?}, replayed={:?}",
                            rec_text, rep_text
                        ));
                    }
                    if !tool_calls_match {
                        d.push(format!(
                            "tool calls mismatch: recorded={:?}, replayed={:?}",
                            rec_tool_names, rep_tool_names
                        ));
                    }
                    if !tool_results_match {
                        d.push(format!(
                            "tool results mismatch: recorded={} results, replayed={} results",
                            rec.tool_results.len(),
                            rep.tool_results.len()
                        ));
                    }
                    Some(d.join("; "))
                } else {
                    None
                };

                results.push(TurnComparison {
                    turn_index: i,
                    text_match,
                    tool_calls_match,
                    tool_results_match,
                    details,
                });
            }
            (Some(_), None) => {
                results.push(TurnComparison {
                    turn_index: i,
                    text_match: false,
                    tool_calls_match: false,
                    tool_results_match: false,
                    details: Some("turn missing in replay".to_string()),
                });
            }
            (None, Some(_)) => {
                results.push(TurnComparison {
                    turn_index: i,
                    text_match: false,
                    tool_calls_match: false,
                    tool_results_match: false,
                    details: Some("extra turn in replay".to_string()),
                });
            }
            (None, None) => unreachable!(),
        }
    }

    results
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::mock::*;

    #[test]
    fn extract_turns_simple_text() {
        let messages = vec![
            Message::User(UserMessage::text("hello")),
            Message::Assistant({
                let mut a = AssistantMessage::empty("mock", "mock", "mock-model");
                a.content.push(AssistantContent::Text(TextContent {
                    text: "Hi there!".into(),
                    text_signature: None,
                }));
                a
            }),
        ];
        let turns = extract_turns(&messages);
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].user_message.as_deref(), Some("hello"));
        assert_eq!(turns[0].assistant_message.text(), "Hi there!");
        assert!(turns[0].tool_results.is_empty());
    }

    #[test]
    fn extract_turns_with_tool_call() {
        let messages = vec![
            Message::User(UserMessage::text("read file")),
            Message::Assistant({
                let mut a = AssistantMessage::empty("mock", "mock", "mock-model");
                a.content.push(AssistantContent::ToolCall(ToolCall {
                    id: "tc1".into(),
                    name: "read_file".into(),
                    arguments: serde_json::json!({"path": "test.txt"}),
                }));
                a.stop_reason = StopReason::ToolUse;
                a
            }),
            Message::ToolResult(ToolResultMessage {
                tool_call_id: "tc1".into(),
                tool_name: "read_file".into(),
                content: vec![ToolResultContent::Text(TextContent {
                    text: "file contents".into(),
                    text_signature: None,
                })],
                details: None,
                is_error: false,
                timestamp: 0,
                duration_ms: None,
            }),
            Message::Assistant({
                let mut a = AssistantMessage::empty("mock", "mock", "mock-model");
                a.content.push(AssistantContent::Text(TextContent {
                    text: "The file says: file contents".into(),
                    text_signature: None,
                }));
                a
            }),
        ];
        let turns = extract_turns(&messages);
        assert_eq!(turns.len(), 2);

        // Turn 1: user + assistant(tool_call) + tool_result
        assert_eq!(turns[0].user_message.as_deref(), Some("read file"));
        assert_eq!(turns[0].tool_results.len(), 1);
        assert_eq!(turns[0].tool_results[0].tool_name, "read_file");

        // Turn 2: continuation (no user message) + assistant(text)
        assert!(turns[1].user_message.is_none());
        assert_eq!(
            turns[1].assistant_message.text(),
            "The file says: file contents"
        );
        assert!(turns[1].tool_results.is_empty());
    }

    #[test]
    fn extract_turns_skips_compaction() {
        let messages = vec![
            Message::CompactionSummary(CompactionSummaryMessage {
                summary: "previous conversation".into(),
                tokens_before: 1000,
                timestamp: 0,
            }),
            Message::User(UserMessage::text("continue")),
            Message::Assistant({
                let mut a = AssistantMessage::empty("mock", "mock", "mock-model");
                a.content.push(AssistantContent::Text(TextContent {
                    text: "Sure!".into(),
                    text_signature: None,
                }));
                a
            }),
        ];
        let turns = extract_turns(&messages);
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].user_message.as_deref(), Some("continue"));
    }

    #[test]
    fn recording_serialization_roundtrip() {
        let recording = SessionRecording {
            model: mock_model(),
            system_prompt: Some("You are helpful.".into()),
            turns: vec![RecordedTurn {
                user_message: Some("hello".into()),
                assistant_message: {
                    let mut a = AssistantMessage::empty("mock", "mock", "mock-model");
                    a.content.push(AssistantContent::Text(TextContent {
                        text: "Hi!".into(),
                        text_signature: None,
                    }));
                    a
                },
                tool_results: Vec::new(),
            }],
        };

        let json = serde_json::to_string_pretty(&recording).unwrap();
        let parsed: SessionRecording = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.turns.len(), 1);
        assert_eq!(parsed.turns[0].assistant_message.text(), "Hi!");
        assert_eq!(parsed.system_prompt.as_deref(), Some("You are helpful."));
    }

    #[test]
    fn replay_simple_text() {
        smol::block_on(async {
            let recording = SessionRecording {
                model: mock_model(),
                system_prompt: Some("test".into()),
                turns: vec![RecordedTurn {
                    user_message: Some("hello".into()),
                    assistant_message: {
                        let mut a = AssistantMessage::empty("mock", "mock", "mock-model");
                        a.content.push(AssistantContent::Text(TextContent {
                            text: "Hello back!".into(),
                            text_signature: None,
                        }));
                        a.stop_reason = StopReason::Stop;
                        a
                    },
                    tool_results: Vec::new(),
                }],
            };

            let result = replay_session(&recording).await;
            assert!(
                result.success,
                "replay should succeed: {:?}",
                result.turn_results
            );
            assert_eq!(result.turn_results.len(), 1);
            assert!(result.turn_results[0].text_match);
        });
    }

    #[test]
    fn replay_with_tool_call() {
        smol::block_on(async {
            let recording = SessionRecording {
                model: mock_model(),
                system_prompt: Some("test".into()),
                turns: vec![
                    RecordedTurn {
                        user_message: Some("read test.txt".into()),
                        assistant_message: {
                            let mut a = AssistantMessage::empty("mock", "mock", "mock-model");
                            a.content.push(AssistantContent::ToolCall(ToolCall {
                                id: "tc1".into(),
                                name: "read_file".into(),
                                arguments: serde_json::json!({"path": "test.txt"}),
                            }));
                            a.stop_reason = StopReason::ToolUse;
                            a
                        },
                        tool_results: vec![ToolResultMessage {
                            tool_call_id: "tc1".into(),
                            tool_name: "read_file".into(),
                            content: vec![ToolResultContent::Text(TextContent {
                                text: "file contents here".into(),
                                text_signature: None,
                            })],
                            details: None,
                            is_error: false,
                            timestamp: 0,
                            duration_ms: None,
                        }],
                    },
                    RecordedTurn {
                        user_message: None,
                        assistant_message: {
                            let mut a = AssistantMessage::empty("mock", "mock", "mock-model");
                            a.content.push(AssistantContent::Text(TextContent {
                                text: "The file says: file contents here".into(),
                                text_signature: None,
                            }));
                            a.stop_reason = StopReason::Stop;
                            a
                        },
                        tool_results: Vec::new(),
                    },
                ],
            };

            let result = replay_session(&recording).await;
            assert!(
                result.success,
                "replay should succeed: {:?}",
                result.turn_results
            );
            assert_eq!(result.turn_results.len(), 2);
            assert!(result.turn_results[0].tool_calls_match);
            assert!(result.turn_results[0].tool_results_match);
            assert!(result.turn_results[1].text_match);
        });
    }
}
