//! Tool definitions and execution for the agent loop.

pub mod bash;
pub mod edit;
pub mod read;
pub mod write;

use std::path::{Path, PathBuf};

use tau_agent_plugin::*;

/// Resolve a potentially relative path against the working directory.
pub(crate) fn resolve_path(cwd: &str, path: &str) -> PathBuf {
    let p = Path::new(path);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        Path::new(cwd).join(p)
    }
}

/// Output from executing a tool.
#[derive(Debug, Clone)]
pub struct ToolOutput {
    pub content: Vec<ToolResultContent>,
    pub is_error: bool,
    pub summary: Option<String>,
}

impl ToolOutput {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content: vec![ToolResultContent::Text(TextContent {
                text: text.into(),
                text_signature: None,
            })],
            is_error: false,
            summary: None,
        }
    }

    pub fn error(text: impl Into<String>) -> Self {
        Self {
            content: vec![ToolResultContent::Text(TextContent {
                text: text.into(),
                text_signature: None,
            })],
            is_error: true,
            summary: None,
        }
    }

    pub fn with_summary(mut self, summary: impl Into<String>) -> Self {
        self.summary = Some(summary.into());
        self
    }
}

/// A registered tool with its definition and executor.
/// The executor receives (arguments, cwd).
pub struct ToolDef {
    pub tool: Tool,
    #[allow(clippy::type_complexity)]
    pub execute: Box<dyn Fn(serde_json::Value, &str) -> ToolOutput + Send + Sync>,
}

/// Execute a tool call against the registered tools.
pub fn execute_tool(tools: &[ToolDef], tool_call: &ToolCall, cwd: &str) -> ToolResultMessage {
    let result = match tools.iter().find(|t| t.tool.name == tool_call.name) {
        Some(def) => (def.execute)(tool_call.arguments.clone(), cwd),
        None => ToolOutput::error(format!("unknown tool: {}", tool_call.name)),
    };

    ToolResultMessage {
        tool_call_id: tool_call.id.clone(),
        tool_name: tool_call.name.clone(),
        content: result.content,
        details: None,
        is_error: result.is_error,
        timestamp: timestamp_ms(),
        duration_ms: None,
        summary: result.summary,
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
