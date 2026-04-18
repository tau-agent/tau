//! Plugin wire protocol types.
//!
//! These are the JSON-lines messages exchanged between the server and plugin
//! processes over stdin/stdout. Pure serde data — no async, no I/O.

use serde::{Deserialize, Serialize};

use crate::protocol::{Request, Response};
use crate::types::{Tool, ToolResultContent};

// ---------------------------------------------------------------------------
// Protocol messages: tau → plugin
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
pub enum PluginRequest {
    /// Initialize the plugin with session context.
    Init {
        cwd: String,
        session_id: String,
        /// Project name for this session.
        #[serde(skip_serializing_if = "Option::is_none")]
        project_name: Option<String>,
    },
    /// Call a hook.
    Hook {
        name: String,
        data: serde_json::Value,
    },
    /// Execute a tool call.
    ToolCall {
        tool_call_id: String,
        name: String,
        arguments: serde_json::Value,
        /// Working directory for tool execution.
        #[serde(skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        /// Session this tool call belongs to.
        #[serde(skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        /// Project name for this session.
        #[serde(skip_serializing_if = "Option::is_none")]
        project_name: Option<String>,
    },
    /// Cancel an in-flight tool call. The plugin should abort the tool by
    /// its `tool_call_id` (e.g. kill the bash subprocess) and return a
    /// normal `ToolResult` indicating cancellation. If the tool has already
    /// completed, this is a no-op.
    CancelToolCall { tool_call_id: String },
    /// Notify session start.
    SessionStart {
        cwd: String,
        session_id: String,
        /// Project name for this session.
        #[serde(skip_serializing_if = "Option::is_none")]
        project_name: Option<String>,
    },
    /// Notify the plugin it has been idle. Plugin may exit in response.
    Idle,
    /// Server response (server -> plugin tunnel).
    /// Response to a PluginMessage::ServerRequest.
    ServerResponse {
        request_id: String,
        response: Response,
    },
}

// ---------------------------------------------------------------------------
// Protocol messages: plugin → tau
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PluginMessage {
    /// Plugin registration (sent once on startup).
    Register(PluginRegistration),
    /// Hook result.
    HookResult(HookResult),
    /// Tool execution result (final).
    ToolResult(PluginToolResult),
    /// Tool output delta (streaming).
    OutputDelta { tool_call_id: String, text: String },
    /// Server request (plugin → server tunnel).
    /// Plugin sends a client protocol Request; server processes it and
    /// responds with ServerResponse.
    ServerRequest {
        request_id: String,
        request: Request,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginRegistration {
    /// Plugin name.
    pub name: String,
    /// Tools provided by this plugin.
    #[serde(default)]
    pub tools: Vec<PluginToolDef>,
    /// Hooks this plugin wants to receive.
    #[serde(default)]
    pub hooks: Vec<String>,
    /// Slash commands provided by this plugin.
    #[serde(default)]
    pub commands: Vec<PluginCommand>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginToolDef {
    /// Tool name.
    pub name: String,
    /// Tool description (for LLM).
    pub description: String,
    /// JSON Schema for parameters.
    pub parameters: serde_json::Value,
    /// One-line snippet for system prompt "Available tools:" list.
    #[serde(default)]
    pub prompt_snippet: Option<String>,
    /// Extra guideline bullets for system prompt.
    #[serde(default)]
    pub prompt_guidelines: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginCommand {
    /// Command name (without /).
    pub name: String,
    /// Description shown in /help.
    pub description: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HookResult {
    /// Optional message to inject before the LLM turn.
    #[serde(default)]
    pub message: Option<HookMessage>,
    /// Optional replacement system prompt.
    #[serde(default)]
    pub system_prompt: Option<String>,
    /// Optional text to append to a tool result (for after_tool_result hook).
    #[serde(default)]
    pub tool_result_append: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookMessage {
    /// Content of the injected message.
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginToolResult {
    pub tool_call_id: String,
    pub content: Vec<ToolResultContent>,
    pub is_error: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Tier-2 actions to run after this tool result is persisted to the
    /// caller's session history. Drained by the agent loop once the tool
    /// result reaches the caller's history.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub post_persist_actions: Vec<crate::types::PostPersistAction>,
}

/// Convert a `PluginToolDef` to a `Tool` (for LLM context).
impl From<&PluginToolDef> for Tool {
    fn from(def: &PluginToolDef) -> Self {
        Tool {
            name: def.name.clone(),
            description: def.description.clone(),
            parameters: def.parameters.clone(),
        }
    }
}

/// Convert a `PluginToolDef` into a `ToolPrompt`.
impl From<&PluginToolDef> for crate::tool_prompt::ToolPrompt {
    fn from(def: &PluginToolDef) -> Self {
        crate::tool_prompt::ToolPrompt {
            name: def.name.clone(),
            snippet: def.prompt_snippet.clone().unwrap_or_default(),
            guidelines: def.prompt_guidelines.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancel_tool_call_roundtrip() {
        // Wire shape for the cancellation RPC — snake_case tag + field.
        let req = PluginRequest::CancelToolCall {
            tool_call_id: "tc-42".into(),
        };
        let json = serde_json::to_string(&req).expect("serialize");
        assert_eq!(
            json,
            r#"{"type":"cancel_tool_call","tool_call_id":"tc-42"}"#
        );

        let parsed: PluginRequest = serde_json::from_str(&json).expect("deserialize");
        match parsed {
            PluginRequest::CancelToolCall { tool_call_id } => {
                assert_eq!(tool_call_id, "tc-42");
            }
            other => panic!("expected CancelToolCall, got {other:?}"),
        }
    }

    #[test]
    fn tool_call_roundtrip_unchanged_by_cancel_variant() {
        // Back-compat sanity: adding CancelToolCall must not alter the wire
        // encoding of pre-existing variants.
        let req = PluginRequest::ToolCall {
            tool_call_id: "tc-1".into(),
            name: "bash".into(),
            arguments: serde_json::json!({"command": "echo hi"}),
            cwd: Some("/tmp".into()),
            session_id: None,
            project_name: None,
        };
        let json = serde_json::to_string(&req).expect("serialize");
        assert!(json.contains(r#""type":"tool_call""#));
        assert!(json.contains(r#""tool_call_id":"tc-1""#));
        assert!(json.contains(r#""name":"bash""#));
        assert!(json.contains(r#""cwd":"/tmp""#));
        // Absent optional fields should not serialise.
        assert!(!json.contains("session_id"));
        assert!(!json.contains("project_name"));
    }
}
