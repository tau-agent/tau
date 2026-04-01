//! Built-in worker plugin for tool execution.
//!
//! The worker speaks the plugin protocol (JSON lines over stdin/stdout).
//! It registers the built-in tools (bash, read, write, edit) plus session
//! orchestration tools (session_spawn, session_join, etc.).
//!
//! Session tools use the ServerRequest/ServerResponse tunnel to communicate
//! with the tau server without opening a separate socket connection.
//!
//! Can be wrapped with sandbox or other execution environments:
//!   worker_command = ["sandbox", "run", "--", "tau", "worker"]
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
    let mut all_tools = builtin_plugin_tools();
    all_tools.extend(crate::orchestration::orchestration_tools());
    let registration = PluginRegistration {
        name: "worker".to_string(),
        tools: all_tools,
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
                session_id,
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

                // Session orchestration tools use ServerRequest tunnel
                if name.starts_with("session_") {
                    let result = handle_session_tool(
                        &name,
                        &tool_call.arguments,
                        session_id.as_deref(),
                        &mut writer,
                        &stdin,
                    );
                    send_message(
                        &mut writer,
                        &PluginMessage::ToolResult(PluginToolResult {
                            tool_call_id,
                            content: result.content,
                            is_error: result.is_error,
                        }),
                    );
                } else if name == "bash" {
                    // For bash, use streaming execution
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
                // Handled inline during server_request calls -- ignore stray ones
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Session orchestration tools (use ServerRequest tunnel)
// ---------------------------------------------------------------------------

use crate::types::ToolResultContent;

/// Simple tool result helper.
fn tool_ok(text: &str) -> crate::types::ToolResultMessage {
    crate::types::ToolResultMessage {
        tool_call_id: String::new(),
        tool_name: String::new(),
        content: vec![ToolResultContent::Text(crate::types::TextContent {
            text: text.to_string(),
            text_signature: None,
        })],
        details: None,
        is_error: false,
        timestamp: crate::types::timestamp_ms(),
    }
}

fn tool_err(text: &str) -> crate::types::ToolResultMessage {
    crate::types::ToolResultMessage {
        tool_call_id: String::new(),
        tool_name: String::new(),
        content: vec![ToolResultContent::Text(crate::types::TextContent {
            text: text.to_string(),
            text_signature: None,
        })],
        details: None,
        is_error: true,
        timestamp: crate::types::timestamp_ms(),
    }
}

/// Send a ServerRequest via plugin protocol and wait for the ServerResponse.
fn server_request(
    writer: &mut impl Write,
    stdin: &std::io::Stdin,
    request: crate::protocol::Request,
) -> Result<crate::protocol::Response, String> {
    let request_id = format!("sr-{}", crate::types::timestamp_ms());
    send_message(
        writer,
        &PluginMessage::ServerRequest {
            request_id: request_id.clone(),
            request,
        },
    );

    // Read lines until we get our ServerResponse
    let reader = stdin.lock();
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => return Err(format!("read error: {}", e)),
        };
        if line.trim().is_empty() {
            continue;
        }
        let req: PluginRequest = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(_) => continue,
        };
        if let PluginRequest::ServerResponse {
            request_id: rid,
            response,
        } = req
            && rid == request_id
        {
            return Ok(response);
        }
    }
    Err("stdin closed while waiting for server response".into())
}

/// Handle session_* tool calls.
fn handle_session_tool(
    name: &str,
    args: &serde_json::Value,
    session_id: Option<&str>,
    writer: &mut impl Write,
    stdin: &std::io::Stdin,
) -> crate::types::ToolResultMessage {
    match name {
        "session_spawn" => {
            let task = args.get("task").and_then(|v| v.as_str()).unwrap_or("");
            let model = args.get("model").and_then(|v| v.as_str()).map(String::from);
            let system_prompt = args
                .get("system_prompt")
                .and_then(|v| v.as_str())
                .map(String::from);
            let cwd = args.get("cwd").and_then(|v| v.as_str()).map(String::from);
            let child_budget = args
                .get("child_budget")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32;

            // Create session
            let create_req = crate::protocol::Request::CreateSession {
                model,
                provider: None,
                system_prompt,
                cwd,
                parent_id: session_id.map(String::from),
                child_budget,
            };
            let resp = match server_request(writer, stdin, create_req) {
                Ok(r) => r,
                Err(e) => return tool_err(&format!("server request failed: {}", e)),
            };
            let child_id = match resp {
                crate::protocol::Response::SessionCreated { session_id } => session_id,
                crate::protocol::Response::Error { message } => {
                    return tool_err(&format!("spawn failed: {}", message));
                }
                other => return tool_err(&format!("unexpected response: {:?}", other)),
            };

            // Send initial message via unix socket (fire-and-forget).
            // This must go through the real server connection to trigger
            // an agent turn -- the ServerRequest tunnel only handles sync ops.
            if !task.is_empty()
                && let Err(e) = fire_chat_via_socket(&child_id, task)
            {
                return tool_err(&format!(
                    "session {} created but chat failed: {}",
                    child_id, e
                ));
            }

            tool_ok(&format!("Spawned session {}", child_id))
        }

        "session_join" => {
            let session_ids: Vec<String> = args
                .get("session_ids")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            let timeout = args.get("timeout").and_then(|v| v.as_u64()).unwrap_or(300);

            if session_ids.is_empty() {
                return tool_err("session_ids is required");
            }

            let req = crate::protocol::Request::WaitSessions {
                session_ids,
                timeout_secs: timeout,
            };
            match server_request(writer, stdin, req) {
                Ok(crate::protocol::Response::SessionsCompleted { results }) => {
                    let mut text = String::new();
                    for r in &results {
                        text.push_str(&format!(
                            "Session {}: {} | {}\n",
                            r.session_id,
                            r.status,
                            if r.summary.is_empty() {
                                "(no output)"
                            } else {
                                &r.summary
                            }
                        ));
                    }
                    tool_ok(text.trim_end())
                }
                Ok(crate::protocol::Response::Error { message }) => tool_err(&message),
                Ok(other) => tool_err(&format!("unexpected response: {:?}", other)),
                Err(e) => tool_err(&e),
            }
        }

        "session_status" => {
            let sid = args
                .get("session_id")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if sid.is_empty() {
                return tool_err("session_id is required");
            }
            let req = crate::protocol::Request::GetSessionInfo {
                session_id: sid.to_string(),
            };
            match server_request(writer, stdin, req) {
                Ok(crate::protocol::Response::SessionInfo { info }) => tool_ok(&format!(
                    "Session {}: {}/{}, {} messages, {} children",
                    info.id, info.provider, info.model, info.message_count, info.child_count
                )),
                Ok(crate::protocol::Response::Error { message }) => tool_err(&message),
                Ok(other) => tool_err(&format!("unexpected response: {:?}", other)),
                Err(e) => tool_err(&e),
            }
        }

        "session_list_children" => {
            let parent = session_id.unwrap_or("");
            if parent.is_empty() {
                return tool_err("no session context available");
            }
            let req = crate::protocol::Request::ListSessions;
            match server_request(writer, stdin, req) {
                Ok(crate::protocol::Response::Sessions { sessions }) => {
                    let children: Vec<_> = sessions
                        .iter()
                        .filter(|s| s.parent_id.as_deref() == Some(parent))
                        .collect();
                    if children.is_empty() {
                        tool_ok("No child sessions")
                    } else {
                        let mut text = String::new();
                        for c in &children {
                            text.push_str(&format!(
                                "{}\t{}/{}\t{} msgs\n",
                                c.id, c.provider, c.model, c.message_count
                            ));
                        }
                        tool_ok(text.trim_end())
                    }
                }
                Ok(crate::protocol::Response::Error { message }) => tool_err(&message),
                Ok(other) => tool_err(&format!("unexpected response: {:?}", other)),
                Err(e) => tool_err(&e),
            }
        }

        "session_read" => {
            let sid = args
                .get("session_id")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let last_n = args.get("last_n").and_then(|v| v.as_u64());
            if sid.is_empty() {
                return tool_err("session_id is required");
            }
            let req = crate::protocol::Request::GetMessages {
                session_id: sid.to_string(),
            };
            match server_request(writer, stdin, req) {
                Ok(crate::protocol::Response::Messages { messages }) => {
                    let msgs = if let Some(n) = last_n {
                        let skip = messages.len().saturating_sub(n as usize);
                        &messages[skip..]
                    } else {
                        &messages
                    };
                    let mut text = String::new();
                    for msg in msgs {
                        match msg {
                            crate::types::Message::User(u) => {
                                let t: String = u
                                    .content
                                    .iter()
                                    .filter_map(|c| match c {
                                        crate::types::UserContent::Text(t) => Some(t.text.as_str()),
                                        _ => None,
                                    })
                                    .collect::<Vec<_>>()
                                    .join("\n");
                                text.push_str(&format!("[user] {}\n", t));
                            }
                            crate::types::Message::Assistant(a) => {
                                let t: String = a
                                    .content
                                    .iter()
                                    .filter_map(|c| match c {
                                        crate::types::AssistantContent::Text(t) => {
                                            Some(t.text.as_str())
                                        }
                                        _ => None,
                                    })
                                    .collect::<Vec<_>>()
                                    .join("\n");
                                text.push_str(&format!("[assistant] {}\n", t));
                            }
                            crate::types::Message::ToolResult(tr) => {
                                text.push_str(&format!("[tool:{}] ...\n", tr.tool_name));
                            }
                            _ => {}
                        }
                    }
                    if text.is_empty() {
                        tool_ok("(no messages)")
                    } else {
                        tool_ok(text.trim_end())
                    }
                }
                Ok(crate::protocol::Response::Error { message }) => tool_err(&message),
                Ok(other) => tool_err(&format!("unexpected response: {:?}", other)),
                Err(e) => tool_err(&e),
            }
        }

        "session_cancel" => {
            let sid = args
                .get("session_id")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if sid.is_empty() {
                return tool_err("session_id is required");
            }
            let req = crate::protocol::Request::CancelChat {
                session_id: sid.to_string(),
            };
            match server_request(writer, stdin, req) {
                Ok(crate::protocol::Response::Ok) => tool_ok(&format!("Cancelled session {}", sid)),
                Ok(crate::protocol::Response::Error { message }) => tool_err(&message),
                Ok(other) => tool_err(&format!("unexpected response: {:?}", other)),
                Err(e) => tool_err(&e),
            }
        }

        _ => tool_err(&format!("unknown session tool: {}", name)),
    }
}

/// Fire a Chat request via unix socket (fire-and-forget).
/// Opens a direct connection to the server, sends the Chat request,
/// and returns without waiting for the agent turn to complete.
fn fire_chat_via_socket(session_id: &str, text: &str) -> Result<(), String> {
    use std::os::unix::net::UnixStream;

    let sock_path = crate::server::socket_path();
    let mut stream = UnixStream::connect(&sock_path).map_err(|e| format!("connect: {}", e))?;

    let req = crate::protocol::Request::Chat {
        session_id: session_id.to_string(),
        text: text.to_string(),
    };
    let mut line = serde_json::to_string(&req).map_err(|e| format!("serialize: {}", e))?;
    line.push('\n');
    stream
        .write_all(line.as_bytes())
        .map_err(|e| format!("write: {}", e))?;
    stream.flush().map_err(|e| format!("flush: {}", e))?;

    // Don't read response -- fire and forget.
    // The server will process the Chat and run the agent turn.
    // The connection dropping is fine -- responses go to subscribers.
    Ok(())
}
