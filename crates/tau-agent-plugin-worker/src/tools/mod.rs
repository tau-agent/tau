//! Tool definitions and execution for the agent loop.

pub mod bash;
pub mod edit;
pub mod get_function;
pub mod line_hash;
pub mod read;
pub mod skeleton;
pub mod tree_sitter_support;
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
/// The executor receives (arguments, cwd, cancel_token). The cancel token
/// is polled by long-running tools (bash) to abort mid-execution; tools
/// that complete quickly (read, write, edit) are free to ignore it.
///
/// `prepare_arguments` is an optional pre-validation hook that runs before
/// `execute`. It lets a tool silently accept legacy argument shapes from
/// resumed old sessions without polluting the public JSON schema (analogous
/// to pi-mono's `prepareArguments`).
pub struct ToolDef {
    pub tool: Tool,
    #[allow(clippy::type_complexity)]
    pub execute: Box<dyn Fn(serde_json::Value, &str, &CancelToken) -> ToolOutput + Send + Sync>,
    #[allow(clippy::type_complexity)]
    pub prepare_arguments:
        Option<Box<dyn Fn(serde_json::Value) -> serde_json::Value + Send + Sync>>,
}

/// Execute a tool call against the registered tools.
pub fn execute_tool(
    tools: &[ToolDef],
    tool_call: &ToolCall,
    cwd: &str,
    cancel: &CancelToken,
) -> ToolResultMessage {
    // Short-circuit: if already cancelled before we even dispatch, return
    // an error result rather than invoking the tool. This mirrors the
    // agent-loop stubbing pattern for remaining tool calls after a cancel.
    if cancel.is_cancelled() {
        return ToolResultMessage {
            tool_call_id: tool_call.id.clone(),
            tool_name: tool_call.name.clone(),
            content: vec![ToolResultContent::Text(TextContent {
                text: "error: cancelled before execution".into(),
                text_signature: None,
            })],
            details: None,
            is_error: true,
            timestamp: timestamp_ms(),
            duration_ms: None,
            summary: None,
            post_persist_actions: Vec::new(),
        };
    }

    let result = match tools.iter().find(|t| t.tool.name == tool_call.name) {
        Some(def) => {
            let args = match &def.prepare_arguments {
                Some(prepare) => prepare(tool_call.arguments.clone()),
                None => tool_call.arguments.clone(),
            };
            (def.execute)(args, cwd, cancel)
        }
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
        post_persist_actions: Vec::new(),
    }
}

/// Create the default set of coding agent tools.
pub fn default_tools() -> Vec<ToolDef> {
    vec![
        bash::tool_def(),
        read::tool_def(),
        write::tool_def(),
        edit::tool_def(),
        skeleton::tool_def(),
        get_function::tool_def(),
    ]
}

/// Extract Tool definitions (for sending to LLM) from ToolDefs.
pub fn tool_schemas(tools: &[ToolDef]) -> Vec<Tool> {
    tools.iter().map(|t| t.tool.clone()).collect()
}
