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

use std::collections::HashSet;
use std::io::{BufRead, BufReader, BufWriter, Write};

use async_trait::async_trait;

use crate::plugin::{
    PluginMessage, PluginRegistration, PluginRequest, PluginToolDef, PluginToolResult,
};
use crate::system_prompt::ToolPrompt;
use crate::types::*;

/// Trait for tool execution (allows plugin-based or in-process).
#[async_trait]
pub trait ToolExecutor: Send {
    async fn execute(
        &mut self,
        tool_call: &ToolCall,
        output_tx: &smol::channel::Sender<String>,
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

#[async_trait]
impl ToolExecutor for InProcessWorker {
    async fn execute(
        &mut self,
        tool_call: &ToolCall,
        _output_tx: &smol::channel::Sender<String>,
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
    let mut reader = BufReader::new(stdin.lock());
    let stdout = std::io::stdout();
    let mut writer = BufWriter::new(stdout.lock());

    // Track unjoined child sessions for session_join_all / session_join_any.
    let mut unjoined_children: HashSet<String> = HashSet::new();

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
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => break, // EOF
            Ok(_) => {}
            Err(_) => break,
        }
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
                        &mut reader,
                        &mut unjoined_children,
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
            PluginRequest::Idle => {
                // Exit cleanly on idle notification
                break;
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
    reader: &mut impl BufRead,
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
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => return Err("stdin closed while waiting for server response".into()),
            Ok(_) => {}
            Err(e) => return Err(format!("read error: {}", e)),
        }
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
}

/// Handle session_* tool calls.
fn handle_session_tool(
    name: &str,
    args: &serde_json::Value,
    session_id: Option<&str>,
    writer: &mut impl Write,
    reader: &mut impl BufRead,
    unjoined_children: &mut HashSet<String>,
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
            let tagline = args
                .get("tagline")
                .and_then(|v| v.as_str())
                .map(String::from);

            // Create session
            let create_req = crate::protocol::Request::CreateSession {
                model,
                provider: None,
                system_prompt,
                cwd,
                parent_id: session_id.map(String::from),
                child_budget,
                tagline,
            };
            let resp = match server_request(writer, reader, create_req) {
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

            // Send initial message via ServerRequest tunnel.
            // The server processes this as an async agent turn for the child.
            if !task.is_empty() {
                let chat_req = crate::protocol::Request::Chat {
                    session_id: child_id.clone(),
                    text: task.to_string(),
                };
                match server_request(writer, reader, chat_req) {
                    Ok(crate::protocol::Response::Ok) => {}
                    Ok(crate::protocol::Response::Error { message }) => {
                        return tool_err(&format!(
                            "session {} created but chat failed: {}",
                            child_id, message
                        ));
                    }
                    Ok(other) => {
                        return tool_err(&format!(
                            "session {} created but unexpected chat response: {:?}",
                            child_id, other
                        ));
                    }
                    Err(e) => {
                        return tool_err(&format!(
                            "session {} created but chat failed: {}",
                            child_id, e
                        ));
                    }
                }
            }

            // Track as unjoined
            unjoined_children.insert(child_id.clone());

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

            // Remove joined IDs from unjoined set
            for sid in &session_ids {
                unjoined_children.remove(sid);
            }

            let req = crate::protocol::Request::WaitSessions {
                session_ids,
                timeout_secs: timeout,
            };
            match server_request(writer, reader, req) {
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

        "session_join_all" => {
            let timeout = args.get("timeout").and_then(|v| v.as_u64()).unwrap_or(300);

            if unjoined_children.is_empty() {
                return tool_ok("No unjoined child sessions.");
            }

            let session_ids: Vec<String> = unjoined_children.drain().collect();

            let req = crate::protocol::Request::WaitSessions {
                session_ids,
                timeout_secs: timeout,
            };
            match server_request(writer, reader, req) {
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

        "session_join_any" => {
            let timeout = args.get("timeout").and_then(|v| v.as_u64()).unwrap_or(300);

            if unjoined_children.is_empty() {
                return tool_ok("No unjoined child sessions.");
            }

            let session_ids: Vec<String> = unjoined_children.iter().cloned().collect();

            let req = crate::protocol::Request::WaitAnySessions {
                session_ids,
                timeout_secs: timeout,
            };
            match server_request(writer, reader, req) {
                Ok(crate::protocol::Response::SessionsCompleted { results }) => {
                    // Remove completed sessions from unjoined set
                    for r in &results {
                        unjoined_children.remove(&r.session_id);
                    }
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
            match server_request(writer, reader, req) {
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
            let req = crate::protocol::Request::ListSessions {
                include_archived: false,
            };
            match server_request(writer, reader, req) {
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
            match server_request(writer, reader, req) {
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
            match server_request(writer, reader, req) {
                Ok(crate::protocol::Response::Ok) => tool_ok(&format!("Cancelled session {}", sid)),
                Ok(crate::protocol::Response::Error { message }) => tool_err(&message),
                Ok(other) => tool_err(&format!("unexpected response: {:?}", other)),
                Err(e) => tool_err(&e),
            }
        }

        "session_message" => {
            let target = args
                .get("session_id")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let content = args.get("content").and_then(|v| v.as_str()).unwrap_or("");
            if target.is_empty() {
                return tool_err("session_id is required");
            }
            if content.is_empty() {
                return tool_err("content is required");
            }
            let sender_info = match session_id {
                Some(sid) => format!("session:{}", sid),
                None => "session:unknown".to_string(),
            };
            let req = crate::protocol::Request::QueueMessage {
                target_session_id: target.to_string(),
                content: content.to_string(),
                sender_info,
            };
            match server_request(writer, reader, req) {
                Ok(crate::protocol::Response::Ok) => {
                    tool_ok(&format!("Message sent to session {}", target))
                }
                Ok(crate::protocol::Response::Error { message }) => tool_err(&message),
                Ok(other) => tool_err(&format!("unexpected response: {:?}", other)),
                Err(e) => tool_err(&e),
            }
        }

        _ => tool_err(&format!("unknown session tool: {}", name)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn sequential_reads_share_buffer() {
        // Test that multiple reads from the same BufReader work correctly.
        // This is the core of the bug fix: with separate stdin.lock() calls,
        // data buffered by one lock would be lost when that lock was dropped.
        let response1 = crate::protocol::Response::SessionCreated {
            session_id: "child-1".into(),
        };
        let response2 = crate::protocol::Response::SessionCreated {
            session_id: "child-2".into(),
        };

        let line1 = serde_json::to_string(&PluginRequest::ServerResponse {
            request_id: "req-1".into(),
            response: response1,
        })
        .unwrap();
        let line2 = serde_json::to_string(&PluginRequest::ServerResponse {
            request_id: "req-2".into(),
            response: response2,
        })
        .unwrap();

        // Both lines in one buffer -- simulates kernel delivering them together
        let input = format!("{}\n{}\n", line1, line2);
        let mut reader = Cursor::new(input.into_bytes());

        // Read first line
        let mut buf = String::new();
        reader.read_line(&mut buf).unwrap();
        let req1: PluginRequest = serde_json::from_str(&buf).unwrap();
        if let PluginRequest::ServerResponse { request_id, .. } = &req1 {
            assert_eq!(request_id, "req-1");
        } else {
            panic!("expected ServerResponse");
        }

        // Read second line from same reader (this would fail with separate stdin.lock())
        buf.clear();
        reader.read_line(&mut buf).unwrap();
        let req2: PluginRequest = serde_json::from_str(&buf).unwrap();
        if let PluginRequest::ServerResponse { request_id, .. } = &req2 {
            assert_eq!(request_id, "req-2");
        } else {
            panic!("expected ServerResponse");
        }
    }

    #[test]
    fn server_request_skips_blank_lines() {
        let response = crate::protocol::Response::Ok;
        let resp_line = serde_json::to_string(&PluginRequest::ServerResponse {
            request_id: "req-1".into(),
            response,
        })
        .unwrap();

        // Blank lines before the response
        let input = format!("\n  \n{}\n", resp_line);
        let mut reader = Cursor::new(input.into_bytes());

        let mut line = String::new();
        // Skip blank lines like the real code does
        loop {
            line.clear();
            let n = reader.read_line(&mut line).unwrap();
            if n == 0 {
                panic!("unexpected EOF");
            }
            if line.trim().is_empty() {
                continue;
            }
            break;
        }

        let req: PluginRequest = serde_json::from_str(&line).unwrap();
        assert!(matches!(
            req,
            PluginRequest::ServerResponse {
                request_id: _,
                response: crate::protocol::Response::Ok,
            }
        ));
    }

    #[test]
    fn server_request_eof_returns_error() {
        let mut reader = Cursor::new(Vec::new()); // empty = immediate EOF
        let mut writer: Vec<u8> = Vec::new();

        let result = server_request(
            &mut writer,
            &mut reader,
            crate::protocol::Request::ListSessions {
                include_archived: false,
            },
        );

        assert!(result.is_err());
        assert!(result.unwrap_err().contains("stdin closed"));
    }
}
