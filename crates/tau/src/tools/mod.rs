//! Tool definitions and execution for the agent loop.

pub mod bash;
pub mod edit;
pub mod read;
pub mod write;

use crate::types::*;

/// Output from executing a tool.
#[derive(Debug, Clone)]
pub struct ToolOutput {
    pub content: Vec<ToolResultContent>,
    pub is_error: bool,
}

impl ToolOutput {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content: vec![ToolResultContent::Text(TextContent {
                text: text.into(),
                text_signature: None,
            })],
            is_error: false,
        }
    }

    pub fn error(text: impl Into<String>) -> Self {
        Self {
            content: vec![ToolResultContent::Text(TextContent {
                text: text.into(),
                text_signature: None,
            })],
            is_error: true,
        }
    }
}

/// A registered tool with its definition and executor.
pub struct ToolDef {
    pub tool: Tool,
    pub execute: Box<dyn Fn(serde_json::Value) -> ToolOutput + Send + Sync>,
}

/// Execute a tool call against the registered tools.
pub fn execute_tool(tools: &[ToolDef], tool_call: &ToolCall) -> ToolResultMessage {
    let result = match tools.iter().find(|t| t.tool.name == tool_call.name) {
        Some(def) => (def.execute)(tool_call.arguments.clone()),
        None => ToolOutput::error(format!("unknown tool: {}", tool_call.name)),
    };

    ToolResultMessage {
        tool_call_id: tool_call.id.clone(),
        tool_name: tool_call.name.clone(),
        content: result.content,
        details: None,
        is_error: result.is_error,
        timestamp: timestamp_ms(),
    }
}

/// Create the default set of coding agent tools.
pub fn default_tools() -> Vec<ToolDef> {
    vec![
        bash::tool_def(),
        read::tool_def(),
        write::tool_def(),
        edit::tool_def(),
    ]
}

/// Extract Tool definitions (for sending to LLM) from ToolDefs.
pub fn tool_schemas(tools: &[ToolDef]) -> Vec<Tool> {
    tools.iter().map(|t| t.tool.clone()).collect()
}
