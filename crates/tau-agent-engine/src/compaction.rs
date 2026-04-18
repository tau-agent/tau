//! Context compaction for long sessions.
//!
//! When context approaches the model's context window limit, older messages
//! are summarized by the LLM and replaced with a compact summary message.

use crate::provider::EventReceiver;
use tau_agent_base::types::*;

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

/// Compaction configuration.
#[derive(Debug, Clone)]
pub struct CompactionSettings {
    /// Whether auto-compaction is enabled.
    pub enabled: bool,
    /// Trigger compaction when context is within this many tokens of the limit.
    pub reserve_tokens: u64,
    /// Keep approximately this many tokens of recent conversation.
    pub keep_recent_tokens: u64,
}

impl Default for CompactionSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            reserve_tokens: 16_384,
            keep_recent_tokens: 20_000,
        }
    }
}

// ---------------------------------------------------------------------------
// Token estimation
// ---------------------------------------------------------------------------

/// Estimate token count for a message using chars/4 heuristic (conservative).
pub fn estimate_tokens(message: &Message) -> u64 {
    let chars = match message {
        Message::User(u) => u
            .content
            .iter()
            .map(|c| match c {
                UserContent::Text(t) => t.text.len(),
                UserContent::Image(_) => 4800, // ~1200 tokens
            })
            .sum(),
        Message::Assistant(a) => a
            .content
            .iter()
            .map(|c| match c {
                AssistantContent::Text(t) => t.text.len(),
                AssistantContent::Thinking(t) => t.thinking.len(),
                AssistantContent::ToolCall(tc) => {
                    tc.name.len()
                        + serde_json::to_string(&tc.arguments)
                            .unwrap_or_default()
                            .len()
                }
            })
            .sum(),
        Message::ToolResult(tr) => tr
            .content
            .iter()
            .map(|c| match c {
                ToolResultContent::Text(t) => t.text.len(),
                ToolResultContent::Image(_) => 4800,
            })
            .sum(),
        Message::CompactionSummary(cs) => cs.summary.len(),
        Message::Info(i) => i.text.len(),
    };
    (chars as u64).div_ceil(4) // ceil(chars / 4)
}

/// Estimate total context tokens from a message list.
/// Uses the last successful assistant's usage if available, plus estimates
/// for any messages after it.
pub fn estimate_context_tokens(messages: &[Message]) -> u64 {
    // Find last successful assistant with usage data
    let mut last_usage_idx = None;
    for (i, msg) in messages.iter().enumerate().rev() {
        if let Message::Assistant(a) = msg
            && a.stop_reason != StopReason::Error
            && a.stop_reason != StopReason::Aborted
        {
            let total = a.usage.input + a.usage.cache_read + a.usage.cache_write;
            if total > 0 {
                last_usage_idx = Some((i, total + a.usage.output));
                break;
            }
        }
    }

    match last_usage_idx {
        Some((idx, usage_tokens)) => {
            // Add estimated tokens for messages after the last usage
            let trailing: u64 = messages[idx + 1..].iter().map(estimate_tokens).sum();
            usage_tokens + trailing
        }
        None => {
            // No usage data — estimate everything
            messages.iter().map(estimate_tokens).sum()
        }
    }
}

// ---------------------------------------------------------------------------
// Compaction decision
// ---------------------------------------------------------------------------

/// Check if compaction should trigger.
pub fn should_compact(
    context_tokens: u64,
    context_window: u64,
    settings: &CompactionSettings,
) -> bool {
    if !settings.enabled || context_window == 0 {
        return false;
    }
    context_tokens > context_window.saturating_sub(settings.reserve_tokens)
}

// ---------------------------------------------------------------------------
// Cut point
// ---------------------------------------------------------------------------

/// Find the index of the first message to keep.
/// Walks backwards from the end, accumulating token estimates until
/// `keep_recent_tokens` is reached. Only cuts at user or compaction
/// summary messages (never mid-turn at assistant/tool_result).
pub fn find_cut_point(messages: &[Message], keep_recent_tokens: u64) -> usize {
    let mut accumulated: u64 = 0;

    for i in (0..messages.len()).rev() {
        accumulated += estimate_tokens(&messages[i]);
        if accumulated >= keep_recent_tokens {
            // Find the nearest valid cut point at or after i
            // Valid = user message or compaction summary (turn boundary)
            for (j, msg) in messages.iter().enumerate().skip(i) {
                match msg {
                    Message::User(_) | Message::CompactionSummary(_) | Message::Info(_) => {
                        return j;
                    }
                    _ => continue,
                }
            }
            // No valid cut point found after i — keep everything
            return 0;
        }
    }
    // Everything fits within budget
    0
}

// ---------------------------------------------------------------------------
// Summary generation
// ---------------------------------------------------------------------------

const SUMMARIZATION_SYSTEM_PROMPT: &str =
    "You are a precise summarizer. Create structured summaries of coding conversations.";

const SUMMARIZATION_PROMPT: &str = r#"The messages above are a conversation to summarize. Create a structured context checkpoint summary that another LLM will use to continue the work.

Use this EXACT format:

## Goal
[What is the user trying to accomplish?]

## Progress
### Done
- [x] [Completed tasks/changes]

### In Progress
- [ ] [Current work]

## Key Decisions
- **[Decision]**: [Brief rationale]

## Next Steps
1. [Ordered list of what should happen next]

## Critical Context
- [Any data, file paths, or references needed to continue]

Keep each section concise. Preserve exact file paths, function names, and error messages."#;

/// Serialize messages to text for the summarization prompt.
fn serialize_messages(messages: &[Message]) -> String {
    let mut out = String::new();
    for msg in messages {
        match msg {
            Message::User(u) => {
                out.push_str("## User\n");
                for c in &u.content {
                    if let UserContent::Text(t) = c {
                        out.push_str(&t.text);
                        out.push('\n');
                    }
                }
            }
            Message::Assistant(a) => {
                out.push_str("## Assistant\n");
                for c in &a.content {
                    match c {
                        AssistantContent::Text(t) => {
                            out.push_str(&t.text);
                            out.push('\n');
                        }
                        AssistantContent::Thinking(t) => {
                            out.push_str("<thinking>\n");
                            out.push_str(&t.thinking);
                            out.push_str("\n</thinking>\n");
                        }
                        AssistantContent::ToolCall(tc) => {
                            out.push_str(&format!("[tool_call: {}({})]\n", tc.name, tc.arguments));
                        }
                    }
                }
            }
            Message::ToolResult(tr) => {
                out.push_str(&format!("## Tool Result ({})\n", tr.tool_name));
                for c in &tr.content {
                    if let ToolResultContent::Text(t) = c {
                        // Truncate very long tool results
                        if t.text.len() > 2000 {
                            out.push_str(tau_agent_base::truncate_str(&t.text, 1000));
                            out.push_str("\n... [truncated] ...\n");
                            out.push_str(tau_agent_base::truncate_str_end(&t.text, 1000));
                        } else {
                            out.push_str(&t.text);
                        }
                        out.push('\n');
                    }
                }
            }
            Message::CompactionSummary(cs) => {
                out.push_str("## Previous Summary\n");
                out.push_str(&cs.summary);
                out.push('\n');
            }
            Message::Info(_) => {
                // Info messages are display-only; skip in summarization.
            }
        }
        out.push('\n');
    }
    out
}

/// Build the context for the summarization LLM call.
/// Returns (system_prompt, user_messages) ready to send.
pub fn build_summarization_context(messages_to_summarize: &[Message]) -> Context {
    let conversation = serialize_messages(messages_to_summarize);
    let prompt = format!(
        "<conversation>\n{}</conversation>\n\n{}",
        conversation, SUMMARIZATION_PROMPT
    );

    Context {
        system_prompt: Some(SUMMARIZATION_SYSTEM_PROMPT.to_string()),
        messages: vec![Message::User(UserMessage::text(&prompt))],
        tools: Vec::new(),
    }
}

/// Extract the summary text from a completed summarization response.
pub fn extract_summary(rx: &EventReceiver) -> tau_agent_base::Result<String> {
    // This is called synchronously — drain the receiver
    loop {
        match rx.recv_blocking() {
            Ok(StreamEvent::Done { message, .. }) => {
                return Ok(message.text());
            }
            Ok(StreamEvent::Error { error, .. }) => {
                return Err(tau_agent_base::Error::Http(
                    error
                        .error_message
                        .unwrap_or_else(|| "summarization failed".into()),
                ));
            }
            Ok(_) => continue, // skip deltas
            Err(_) => return Err(tau_agent_base::Error::ChannelClosed),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn user(text: &str) -> Message {
        Message::User(UserMessage::text(text))
    }

    fn assistant(text: &str, input_tokens: u64) -> Message {
        let mut a = AssistantMessage::empty("test", "test", "test");
        a.content.push(AssistantContent::Text(TextContent {
            text: text.to_string(),
            text_signature: None,
        }));
        a.usage.input = input_tokens;
        a.usage.output = 100;
        Message::Assistant(a)
    }

    fn compaction_summary(text: &str) -> Message {
        Message::CompactionSummary(CompactionSummaryMessage {
            summary: text.to_string(),
            tokens_before: 50000,
            timestamp: 0,
        })
    }

    #[test]
    fn estimate_tokens_basic() {
        // "hello" = 5 chars → ceil(5/4) = 2 tokens
        let msg = user("hello");
        assert_eq!(estimate_tokens(&msg), 2);

        // 400 chars → 100 tokens
        let msg = user(&"x".repeat(400));
        assert_eq!(estimate_tokens(&msg), 100);
    }

    #[test]
    fn should_compact_thresholds() {
        let settings = CompactionSettings {
            enabled: true,
            reserve_tokens: 16_000,
            keep_recent_tokens: 20_000,
        };
        // 190K context, 200K window → 10K headroom < 16K reserve → compact
        assert!(should_compact(190_000, 200_000, &settings));
        // 180K context → 20K headroom > 16K reserve → don't compact
        assert!(!should_compact(180_000, 200_000, &settings));
        // Disabled
        let disabled = CompactionSettings {
            enabled: false,
            ..settings
        };
        assert!(!should_compact(190_000, 200_000, &disabled));
    }

    #[test]
    fn find_cut_point_keeps_recent() {
        // Each "x" * 400 message ≈ 100 tokens
        let big = "x".repeat(400);
        let messages = vec![
            user(&big),            // 0: ~100 tok
            assistant(&big, 500),  // 1: ~100 tok
            user(&big),            // 2: ~100 tok
            assistant(&big, 800),  // 3: ~100 tok
            user(&big),            // 4: ~100 tok
            assistant(&big, 1000), // 5: ~100 tok
        ];
        // keep_recent_tokens=250 → should keep ~3 messages from end
        let cut = find_cut_point(&messages, 250);
        // Cut at index 4 (user message, turn boundary)
        assert_eq!(cut, 4);
    }

    #[test]
    fn find_cut_point_never_cuts_at_tool_result() {
        let big = "x".repeat(400);
        let messages = vec![
            user(&big),
            assistant(&big, 500),
            Message::ToolResult(ToolResultMessage {
                tool_call_id: "tc1".into(),
                tool_name: "bash".into(),
                content: vec![ToolResultContent::Text(TextContent {
                    text: big.clone(),
                    text_signature: None,
                })],
                details: None,
                is_error: false,
                timestamp: 0,
                duration_ms: None,
                summary: None,
                post_persist_actions: Vec::new(),
            }),
            user(&big),
            assistant(&big, 1000),
        ];
        // Even if cut would land on the tool result, it should skip to next user msg
        let cut = find_cut_point(&messages, 250);
        assert_eq!(cut, 3); // user message at index 3
    }

    #[test]
    fn find_cut_point_with_compaction_summary() {
        let big = "x".repeat(400);
        let messages = vec![
            compaction_summary("previous summary"),
            user(&big),
            assistant(&big, 500),
            user(&big),
            assistant(&big, 1000),
        ];
        let cut = find_cut_point(&messages, 250);
        assert_eq!(cut, 3); // user at index 3
    }

    #[test]
    fn estimate_context_tokens_uses_usage() {
        let messages = vec![
            user("hello"),
            assistant("world", 5000), // usage.input=5000, output=100
            user("followup"),
        ];
        let est = estimate_context_tokens(&messages);
        // 5000 + 100 (from usage) + estimate("followup") ≈ 5102
        assert!(est > 5000);
        assert!(est < 5200);
    }

    #[test]
    fn estimate_context_tokens_no_usage() {
        let messages = vec![user("hello"), user("world")];
        let est = estimate_context_tokens(&messages);
        // Pure heuristic: "hello" ≈ 2, "world" ≈ 2
        assert_eq!(est, 4);
    }

    #[test]
    fn serialize_messages_roundtrip() {
        let messages = vec![user("write a test"), assistant("here's the code", 100)];
        let text = serialize_messages(&messages);
        assert!(text.contains("## User"));
        assert!(text.contains("write a test"));
        assert!(text.contains("## Assistant"));
        assert!(text.contains("here's the code"));
    }

    #[test]
    fn repeated_compaction_preserves_recent() {
        // Simulate a session that was already compacted once:
        // [compaction_summary, user, assistant, user, assistant, user, assistant]
        // When compacting again, the compaction summary is a valid cut point,
        // and recent messages should be preserved.
        let big = "x".repeat(400); // ~100 tokens each
        let messages = vec![
            compaction_summary("previous work summary"), // 0: ~5 tokens
            user(&big),                                  // 1: ~100 tokens
            assistant(&big, 500),                        // 2: ~100 tokens
            user(&big),                                  // 3: ~100 tokens
            assistant(&big, 800),                        // 4: ~100 tokens
            user(&big),                                  // 5: ~100 tokens
            assistant(&big, 1000),                       // 6: ~100 tokens
        ];

        // keep_recent_tokens=250 → should keep ~3 messages from end
        let cut = find_cut_point(&messages, 250);
        // Should cut at index 5 (user message) — keeping msgs 5,6
        // Index 3 is also valid. Either way, compaction summary + old messages
        // get summarized, recent messages kept.
        assert!(cut >= 3, "cut={cut} should be >= 3 (keep recent)");
        assert!(cut <= 5, "cut={cut} should be <= 5");
        // The cut point must be a valid boundary (user or compaction summary)
        assert!(
            matches!(
                &messages[cut],
                Message::User(_) | Message::CompactionSummary(_)
            ),
            "cut point must be at a turn boundary"
        );
    }

    #[test]
    fn should_compact_after_previous_compaction() {
        // After a compaction, context is smaller. Verify should_compact
        // correctly handles sessions starting with CompactionSummary.
        let messages = vec![
            compaction_summary("summary of earlier work"),
            user("continue working"),
            assistant("ok", 190_000), // usage says 190K tokens
        ];
        let settings = CompactionSettings {
            enabled: true,
            reserve_tokens: 16_000,
            keep_recent_tokens: 20_000,
        };
        let ctx_tokens = estimate_context_tokens(&messages);
        // 190K + 100 output ≈ 190100 tokens
        assert!(ctx_tokens > 180_000);
        // With 200K window and 16K reserve → should compact
        assert!(should_compact(ctx_tokens, 200_000, &settings));
        // With 1M window → should not compact
        assert!(!should_compact(ctx_tokens, 1_000_000, &settings));
    }

    #[test]
    fn estimate_tokens_info() {
        // "hello" = 5 chars → ceil(5/4) = 2 tokens
        let msg = Message::Info(tau_agent_base::types::InfoMessage {
            text: "hello".into(),
            timestamp: 0,
        });
        assert_eq!(estimate_tokens(&msg), 2);
    }

    #[test]
    fn find_cut_point_info_is_valid_boundary() {
        let big = "x".repeat(400);
        let messages = vec![
            user(&big),
            assistant(&big, 500),
            Message::Info(tau_agent_base::types::InfoMessage {
                text: "notification".into(),
                timestamp: 0,
            }),
            user(&big),
            assistant(&big, 1000),
        ];
        let cut = find_cut_point(&messages, 250);
        // Cut should be at index 2 (Info) or 3 (user) — both are valid boundaries
        assert!(cut >= 2 && cut <= 3, "cut={cut} should be 2 or 3");
        assert!(
            matches!(&messages[cut], Message::User(_) | Message::Info(_)),
            "cut point must be at a valid boundary"
        );
    }
}
