//! JSON-lines wire protocol over unix domain socket.

use serde::{Deserialize, Serialize};

use crate::types::StreamEvent;

// ---------------------------------------------------------------------------
// Client → Server
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    /// Send a chat message in a session.
    Chat { session_id: String, text: String },
    /// Create a new session.
    CreateSession {
        #[serde(skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        provider: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        system_prompt: Option<String>,
        /// Working directory for tool execution.
        #[serde(skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
    },
    /// Get info about a specific session.
    GetSessionInfo { session_id: String },
    /// List sessions.
    ListSessions,
    /// Delete a session.
    DeleteSession { session_id: String },
    /// List available models.
    ListModels,
    /// Change model for a session.
    SetModel {
        session_id: String,
        model_id: String,
    },
    /// Change working directory for a session.
    SetCwd { session_id: String, cwd: String },
    /// Start OAuth login for a provider.
    Login { provider: String },
    /// Query authentication status.
    AuthStatus,
    /// Fetch subscription usage (OAuth only, cached 5 min).
    GetSubscriptionUsage,
    /// Get message history for a session.
    GetMessages { session_id: String },
    /// Cancel an in-progress chat (agent loop) for a session.
    CancelChat { session_id: String },
    /// Shut down the server.
    Shutdown {
        /// If true, server is restarting (clients should reconnect).
        #[serde(default)]
        restart: bool,
    },
}

// ---------------------------------------------------------------------------
// Server → Client
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    /// Session was created.
    SessionCreated { session_id: String },
    /// Info about a single session.
    SessionInfo { info: SessionInfo },
    /// List of sessions.
    Sessions { sessions: Vec<SessionInfo> },
    /// Session deleted.
    SessionDeleted,
    /// Available models.
    Models { models: Vec<ModelInfo> },
    /// Model changed.
    ModelChanged { model: ModelInfo },
    /// Streaming event from the LLM.
    Stream { event: Box<StreamEvent> },
    /// OAuth login succeeded.
    LoginSuccess { provider: String },
    /// Authentication status.
    AuthStatus { providers: Vec<String> },
    /// Subscription usage data.
    SubscriptionUsage {
        usage: crate::auth::SubscriptionUsage,
    },
    /// Server is shutting down. Clients should reconnect if restart=true.
    ServerShutdown { restart: bool },
    /// Agent loop was cancelled by the user.
    Cancelled,
    /// Message history for a session.
    Messages {
        messages: Vec<crate::types::Message>,
    },
    /// Agent loop completed (all turns done).
    AgentDone,
    /// Success (generic ack).
    Ok,
    /// Error.
    Error { message: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    pub model: String,
    pub provider: String,
    pub cwd: Option<String>,
    pub message_count: usize,
    pub stats: SessionStats,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
    pub provider: String,
    pub thinking: crate::types::ThinkingStyle,
    pub context_window: u64,
    pub max_tokens: u64,
}

/// Cumulative session usage statistics.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionStats {
    pub user_messages: usize,
    pub assistant_messages: usize,
    pub tool_calls: usize,
    pub tool_results: usize,
    pub tokens: TokenStats,
    pub cost: f64,
    /// Whether credentials are OAuth (subscription).
    pub is_subscription: bool,
    /// Context window info from the model.
    pub context_window: u64,
    /// Estimated context usage from last assistant response (input tokens).
    pub context_tokens: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TokenStats {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

impl TokenStats {
    pub fn total(&self) -> u64 {
        self.input + self.output + self.cache_read + self.cache_write
    }
}

/// Format a token count for display: 1234 → "1.2K", 1234567 → "1.2M".
pub fn format_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

/// Format session stats as a compact one-line summary like pi's footer:
/// `↑12K ↓81K R18M W353K $13.434 (sub) 18.4%/200K`
#[allow(clippy::cast_precision_loss)]
pub fn format_stats(stats: &SessionStats) -> String {
    let mut parts = Vec::new();

    if stats.tokens.input > 0 {
        parts.push(format!("↑{}", format_tokens(stats.tokens.input)));
    }
    if stats.tokens.output > 0 {
        parts.push(format!("↓{}", format_tokens(stats.tokens.output)));
    }
    if stats.tokens.cache_read > 0 {
        parts.push(format!("R{}", format_tokens(stats.tokens.cache_read)));
    }
    if stats.tokens.cache_write > 0 {
        parts.push(format!("W{}", format_tokens(stats.tokens.cache_write)));
    }

    let cost_str = if stats.is_subscription {
        format!("${:.3} (sub)", stats.cost)
    } else if stats.cost > 0.0 {
        format!("${:.3}", stats.cost)
    } else {
        String::new()
    };
    if !cost_str.is_empty() {
        parts.push(cost_str);
    }

    if stats.context_window > 0 {
        let ctx = match stats.context_tokens {
            Some(t) => {
                let pct = (t as f64 / stats.context_window as f64) * 100.0;
                format!("{:.1}%/{}", pct, format_tokens(stats.context_window))
            }
            None => format!("?/{}", format_tokens(stats.context_window)),
        };
        parts.push(ctx);
    }

    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_tokens_units() {
        assert_eq!(format_tokens(0), "0");
        assert_eq!(format_tokens(999), "999");
        assert_eq!(format_tokens(1_000), "1.0K");
        assert_eq!(format_tokens(12_345), "12.3K");
        assert_eq!(format_tokens(999_999), "1000.0K");
        assert_eq!(format_tokens(1_000_000), "1.0M");
        assert_eq!(format_tokens(18_500_000), "18.5M");
    }

    #[test]
    fn format_stats_empty() {
        let stats = SessionStats::default();
        assert_eq!(format_stats(&stats), "");
    }

    #[test]
    fn format_stats_basic() {
        let stats = SessionStats {
            tokens: TokenStats {
                input: 12_000,
                output: 81_000,
                cache_read: 18_000_000,
                cache_write: 353_000,
            },
            cost: 13.434,
            is_subscription: true,
            context_window: 200_000,
            context_tokens: Some(36_800),
            ..Default::default()
        };
        let s = format_stats(&stats);
        assert!(s.contains("↑12.0K"), "got: {s}");
        assert!(s.contains("↓81.0K"), "got: {s}");
        assert!(s.contains("R18.0M"), "got: {s}");
        assert!(s.contains("W353.0K"), "got: {s}");
        assert!(s.contains("$13.434 (sub)"), "got: {s}");
        assert!(s.contains("18.4%/200.0K"), "got: {s}");
    }

    #[test]
    fn format_stats_unknown_context() {
        let stats = SessionStats {
            context_window: 200_000,
            context_tokens: None,
            ..Default::default()
        };
        let s = format_stats(&stats);
        assert!(s.contains("?/200.0K"), "got: {s}");
    }

    #[test]
    fn format_stats_no_subscription() {
        let stats = SessionStats {
            tokens: TokenStats {
                input: 500,
                output: 200,
                ..Default::default()
            },
            cost: 0.005,
            is_subscription: false,
            ..Default::default()
        };
        let s = format_stats(&stats);
        assert!(s.contains("$0.005"), "got: {s}");
        assert!(!s.contains("(sub)"), "got: {s}");
    }
}
