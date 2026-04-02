//! Session orchestration tools.
//!
//! Provides tool schemas and prompt guidelines for session management tools
//! (session_spawn, session_join, session_status, session_list_children,
//! session_read, session_cancel, session_id). These tools communicate with the server
//! via the plugin protocol's ServerRequest/ServerResponse tunnel.

use crate::plugin::{PluginRegistration, PluginToolDef};
use crate::system_prompt::ToolPrompt;

/// Tool definitions for session orchestration.
pub fn orchestration_tools() -> Vec<PluginToolDef> {
    vec![
        PluginToolDef {
            name: "session_spawn".into(),
            description: "Spawn a child session to work on a subtask in parallel. Returns the session ID. The child runs independently with its own tools and context.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "task": {
                        "type": "string",
                        "description": "Initial message/task for the child session"
                    },
                    "model": {
                        "type": "string",
                        "description": "Model ID for the child (omit to inherit parent's model)"
                    },
                    "system_prompt": {
                        "type": "string",
                        "description": "Custom system prompt (omit for default)"
                    },
                    "cwd": {
                        "type": "string",
                        "description": "Working directory (omit to inherit parent's)"
                    },
                    "child_budget": {
                        "type": "integer",
                        "description": "Max descendant sessions the child can spawn (default 0 = leaf)"
                    },
                    "tagline": {
                        "type": "string",
                        "description": "Short description of what the child session will work on (shown in session picker)"
                    }
                },
                "required": ["task"]
            }),
            prompt_snippet: Some("Use session_spawn to delegate independent subtasks to child sessions that run in parallel. Each child has its own tools and conversation context. Use session_join to wait for results.".into()),
            prompt_guidelines: vec![
                "Spawn children for parallelizable work (e.g., reviewing multiple files)".into(),
                "Each child is a leaf by default (child_budget=0). Set child_budget>0 to allow grandchildren.".into(),
                "The task should be self-contained -- the child has no access to the parent's conversation.".into(),
            ],
        },
        PluginToolDef {
            name: "session_join".into(),
            description: "Wait for child sessions to complete and get their results. Blocks until all specified sessions are done or timeout is reached.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_ids": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Session IDs to wait for"
                    },
                    "timeout": {
                        "type": "integer",
                        "description": "Timeout in seconds (default 300)"
                    }
                },
                "required": ["session_ids"]
            }),
            prompt_snippet: None,
            prompt_guidelines: vec![
                "Use after spawning children to collect their results.".into(),
                "Returns status and summary (last assistant message) for each session.".into(),
            ],
        },
        PluginToolDef {
            name: "session_join_all".into(),
            description: "Wait for all unjoined child sessions to complete. Blocks until every child spawned (that hasn't been joined yet) is done or timeout is reached.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "timeout": {
                        "type": "integer",
                        "description": "Timeout in seconds (default 300)"
                    }
                }
            }),
            prompt_snippet: None,
            prompt_guidelines: vec![
                "Waits for all children spawned since the last join. No need to track session IDs.".into(),
                "Returns status and summary for each child session.".into(),
            ],
        },
        PluginToolDef {
            name: "session_join_any".into(),
            description: "Wait for any unjoined child session to complete. Returns as soon as at least one child finishes, with results for all completed children.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "timeout": {
                        "type": "integer",
                        "description": "Timeout in seconds (default 300)"
                    }
                }
            }),
            prompt_snippet: None,
            prompt_guidelines: vec![
                "Use for fan-out patterns where children have variable completion times.".into(),
                "Returns results for all children that completed, remaining stay unjoined.".into(),
            ],
        },
        PluginToolDef {
            name: "session_status".into(),
            description: "Check the status of a child session without blocking.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": {
                        "type": "string",
                        "description": "Session ID to check"
                    }
                },
                "required": ["session_id"]
            }),
            prompt_snippet: None,
            prompt_guidelines: vec![],
        },
        PluginToolDef {
            name: "session_list_children".into(),
            description: "List direct child sessions of the current session.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {}
            }),
            prompt_snippet: None,
            prompt_guidelines: vec![],
        },
        PluginToolDef {
            name: "session_read".into(),
            description: "Read messages from a child session's conversation.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": {
                        "type": "string",
                        "description": "Session ID to read from"
                    },
                    "last_n": {
                        "type": "integer",
                        "description": "Number of recent messages to return (omit for all)"
                    }
                },
                "required": ["session_id"]
            }),
            prompt_snippet: None,
            prompt_guidelines: vec![],
        },
        PluginToolDef {
            name: "session_cancel".into(),
            description: "Cancel a running child session.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": {
                        "type": "string",
                        "description": "Session ID to cancel"
                    }
                },
                "required": ["session_id"]
            }),
            prompt_snippet: None,
            prompt_guidelines: vec![],
        },
        PluginToolDef {
            name: "session_message".into(),
            description: "Send a message to another session. The message is injected as a user message into the target session's conversation. If the target is idle, it will resume processing. Fire-and-forget: does not wait for a response.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": {
                        "type": "string",
                        "description": "Target session ID to send the message to"
                    },
                    "content": {
                        "type": "string",
                        "description": "Message content to send"
                    }
                },
                "required": ["session_id", "content"]
            }),
            prompt_snippet: Some("Use session_message to send information to another session (parent, child, or sibling).".into()),
            prompt_guidelines: vec![
                "The message appears as a user message in the target session's conversation.".into(),
                "Fire-and-forget: returns immediately, does not wait for a response. Use session_read to check for responses later.".into(),
            ],
        },
        PluginToolDef {
            name: "session_id".into(),
            description: "Get the current session's ID.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {}
            }),
            prompt_snippet: None,
            prompt_guidelines: vec![],
        },
    ]
}

/// Registration for the orchestration plugin.
pub fn registration() -> PluginRegistration {
    PluginRegistration {
        name: "orchestration".into(),
        tools: orchestration_tools(),
        hooks: vec![],
        commands: vec![],
    }
}

/// Tool prompt entries for system prompt generation.
pub fn tool_prompts() -> Vec<ToolPrompt> {
    orchestration_tools()
        .into_iter()
        .filter_map(|t| {
            t.prompt_snippet.map(|snippet| ToolPrompt {
                name: t.name,
                snippet,
                guidelines: t.prompt_guidelines,
            })
        })
        .collect()
}
