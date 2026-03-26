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
    },
    /// List sessions.
    ListSessions,
    /// Delete a session.
    DeleteSession { session_id: String },
    /// Shut down the server.
    Shutdown,
}

// ---------------------------------------------------------------------------
// Server → Client
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    /// Session was created.
    SessionCreated { session_id: String },
    /// List of sessions.
    Sessions { sessions: Vec<SessionInfo> },
    /// Session deleted.
    SessionDeleted,
    /// Streaming event from the LLM.
    Stream { event: Box<StreamEvent> },
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
    pub message_count: usize,
}
