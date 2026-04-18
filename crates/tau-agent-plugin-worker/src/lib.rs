//! Default worker plugin for the tau agent.
//!
//! Provides the canonical bash/edit/read/write tool implementations.
//! Can be used as:
//! - A plugin binary (`tau worker`) that speaks the plugin protocol
//! - An in-process executor (`InProcessWorker`) for testing

pub mod orchestration;
pub mod tools;

use async_trait::async_trait;
use tau_agent_plugin::ToolExecutor;
use tau_agent_plugin::{ToolCall, ToolResultMessage};

/// In-process worker for testing (no subprocess).
pub struct InProcessWorker {
    tools: Vec<tools::ToolDef>,
}

impl Default for InProcessWorker {
    fn default() -> Self {
        Self {
            tools: tools::default_tools(),
        }
    }
}

impl InProcessWorker {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl ToolExecutor for InProcessWorker {
    async fn execute(
        &mut self,
        tool_call: &ToolCall,
        _output_tx: &smol::channel::Sender<String>,
        cancel: &tau_agent_plugin::CancelToken,
    ) -> tau_agent_plugin::Result<ToolResultMessage> {
        let result = tools::execute_tool(&self.tools, tool_call, "/tmp", cancel);
        Ok(result)
    }
}

/// Built-in tool prompts for the default tools.
///
/// Delegates to the canonical source in `tau_agent_plugin::default_tool_prompts()`.
pub fn default_tool_prompts() -> Vec<tau_agent_plugin::ToolPrompt> {
    tau_agent_plugin::default_tool_prompts()
}

/// Build a system prompt with the default tools (convenience for server).
pub fn build_default_prompt(cwd: Option<&str>) -> String {
    // We use the system prompt builder from the agent crate via a minimal
    // reimplementation that matches the original signature.
    let date = chrono::Local::now().format("%Y-%m-%d").to_string();
    let tools = default_tool_prompts();
    let tools_list = tools
        .iter()
        .map(|t| format!("- {}: {}", t.name, t.snippet))
        .collect::<Vec<_>>()
        .join("\n");

    let mut guidelines: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut add = |g: String| {
        if seen.insert(g.clone()) {
            guidelines.push(g);
        }
    };

    for tool in &tools {
        for g in &tool.guidelines {
            add(g.clone());
        }
    }
    add("Be concise in your responses".into());
    add("When referring to files, use paths relative to the current working directory (e.g. crates/foo/src/lib.rs). Use absolute paths only when the file is outside the project.".into());

    let guidelines_str = guidelines
        .iter()
        .map(|g| format!("- {}", g))
        .collect::<Vec<_>>()
        .join("\n");

    let mut prompt = format!(
        r#"You are an expert coding assistant operating inside tau, a coding agent harness. You help users by reading files, executing commands, editing code, and writing new files.

Available tools:
{tools_list}

In addition to the tools above, you may have access to other custom tools depending on the project.

Guidelines:
{guidelines_str}
Current date: {date}"#
    );

    if let Some(cwd) = cwd {
        prompt.push_str(&format!(
            "\nCurrent working directory: {cwd}\n\
             Bash commands already run in this directory — do not prefix them with `cd {cwd} && ...`.",
        ));
    }

    prompt
}
