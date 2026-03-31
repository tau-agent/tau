//! Built-in worker plugin for tool execution.
//!
//! The worker speaks the plugin protocol (JSON lines over stdin/stdout).
//! It registers the built-in tools (bash, read, write, edit) and handles
//! tool calls from the server.
//!
//! Can be wrapped with sandbox or other execution environments:
//!   worker_command = ["sandbox", "run", "--", "tau", "worker"]

use std::io::{BufRead, BufWriter, Write};

use crate::plugin::{
    PluginMessage, PluginRegistration, PluginRequest, PluginToolDef, PluginToolResult,
};
use crate::system_prompt::ToolPrompt;
use crate::types::*;

/// Trait for tool execution (allows plugin-based or in-process).
pub trait ToolExecutor {
    fn execute(
        &mut self,
        tool_call: &ToolCall,
        on_output: &mut dyn FnMut(&str),
    ) -> crate::Result<ToolResultMessage>;
}

/// In-process worker for testing (no subprocess).
pub struct InProcessWorker {
    tools: Vec<crate::tools::ToolDef>,
}

impl Default for InProcessWorker {
    fn default() -> Self {
        Self {
            tools: crate::tools::default_tools(),
        }
    }
}

impl InProcessWorker {
    pub fn new() -> Self {
        Self::default()
    }
}

impl ToolExecutor for InProcessWorker {
    fn execute(
        &mut self,
        tool_call: &ToolCall,
        _on_output: &mut dyn FnMut(&str),
    ) -> crate::Result<ToolResultMessage> {
        let result = crate::tools::execute_tool(&self.tools, tool_call, "/tmp");
        Ok(result)
    }
}

// ---------------------------------------------------------------------------
// Built-in tool prompt definitions (for system prompt via plugin registration)
// ---------------------------------------------------------------------------

fn builtin_tool_prompts() -> Vec<ToolPrompt> {
    crate::system_prompt::default_tool_prompts()
}

fn builtin_plugin_tools() -> Vec<PluginToolDef> {
    let tools = crate::tools::default_tools();
    let prompts = builtin_tool_prompts();

    tools
        .iter()
        .map(|t| {
            let prompt = prompts.iter().find(|p| p.name == t.tool.name);
            PluginToolDef {
                name: t.tool.name.clone(),
                description: t.tool.description.clone(),
                parameters: t.tool.parameters.clone(),
                prompt_snippet: prompt.map(|p| p.snippet.clone()),
                prompt_guidelines: prompt.map(|p| p.guidelines.clone()).unwrap_or_default(),
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Worker main loop (runs in the subprocess, speaks plugin protocol)
// ---------------------------------------------------------------------------

/// Helper to send a plugin message to stdout.
fn send_message(writer: &mut impl Write, msg: &PluginMessage) {
    if let Ok(mut line) = serde_json::to_string(msg) {
        line.push('\n');
        let _ = writer.write_all(line.as_bytes());
        let _ = writer.flush();
    }
}

/// Run the worker loop: register tools, then handle tool calls.
/// Called from `tau worker` subcommand.
pub fn run_worker_loop() {
    let tools = crate::tools::default_tools();

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut writer = BufWriter::new(stdout.lock());

    // Send registration (plugin protocol)
    let registration = PluginRegistration {
        name: "worker".to_string(),
        tools: builtin_plugin_tools(),
        hooks: Vec::new(),
        commands: Vec::new(),
    };
    send_message(&mut writer, &PluginMessage::Register(registration));

    // Handle requests
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if line.trim().is_empty() {
            continue;
        }

        let req: PluginRequest = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("worker: bad request: {}", e);
                continue;
            }
        };

        match req {
            PluginRequest::ToolCall {
                tool_call_id,
                name,
                arguments,
                cwd,
                session_id: _,
            } => {
                let cwd = cwd.unwrap_or_else(|| {
                    std::env::current_dir()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_string()
                });

                let tool_call = ToolCall {
                    id: tool_call_id.clone(),
                    name: name.clone(),
                    arguments,
                };

                // For bash, use streaming execution
                if name == "bash" {
                    let result = crate::tools::bash::execute_streaming(
                        &tool_call.arguments,
                        &cwd,
                        |delta| {
                            send_message(
                                &mut writer,
                                &PluginMessage::OutputDelta {
                                    tool_call_id: tool_call_id.clone(),
                                    text: delta.to_string(),
                                },
                            );
                        },
                    );

                    send_message(
                        &mut writer,
                        &PluginMessage::ToolResult(PluginToolResult {
                            tool_call_id,
                            content: result.content,
                            is_error: result.is_error,
                        }),
                    );
                } else {
                    // Non-streaming tools
                    let result = crate::tools::execute_tool(&tools, &tool_call, &cwd);

                    send_message(
                        &mut writer,
                        &PluginMessage::ToolResult(PluginToolResult {
                            tool_call_id,
                            content: result.content,
                            is_error: result.is_error,
                        }),
                    );
                }
            }
            PluginRequest::Init { .. } | PluginRequest::SessionStart { .. } => {
                // Acknowledge with empty hook result
                send_message(
                    &mut writer,
                    &PluginMessage::HookResult(crate::plugin::HookResult::default()),
                );
            }
            PluginRequest::Hook { .. } => {
                send_message(
                    &mut writer,
                    &PluginMessage::HookResult(crate::plugin::HookResult::default()),
                );
            }
            PluginRequest::ServerResponse { .. } => {
                // Worker doesn't handle server responses -- ignore
            }
        }
    }
}
