//! Plugin ↔ server tunnel helpers.
//!
//! Deduplicated from the three near-identical copies in tasks_scheduler,
//! tasks_merge, and worker.

use std::io::{BufRead, Write};

use tau_agent_base::plugin_protocol::{PluginMessage, PluginRequest, PluginToolResult};
use tau_agent_base::protocol::{Request, Response};
use tau_agent_base::types::{TextContent, ToolResultContent};

/// Send a `PluginMessage` as a JSON line (sync).
pub fn send_message(writer: &mut impl Write, msg: &PluginMessage) {
    if let Ok(mut line) = serde_json::to_string(msg) {
        line.push('\n');
        let _ = writer.write_all(line.as_bytes());
        let _ = writer.flush();
    }
}

/// Send a `ServerRequest` via plugin protocol and wait for the `ServerResponse`.
///
/// While waiting, any `ToolCall` messages that arrive on stdin are
/// **immediately answered with an error** so that the calling session does
/// not hang.
///
/// `prefix` is used to generate the request ID (e.g. `"task-sr"`, `"merge-sr"`).
pub fn server_request(
    writer: &mut impl Write,
    reader: &mut impl BufRead,
    request: Request,
    prefix: &str,
) -> tau_agent_base::Result<Response> {
    let request_id = format!("{}-{}", prefix, tau_agent_base::types::timestamp_ms());
    send_message(
        writer,
        &PluginMessage::ServerRequest {
            request_id: request_id.clone(),
            request,
        },
    );

    // Read lines until we get our ServerResponse.
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => {
                return Err(tau_agent_base::Error::Io(
                    "stdin closed while waiting for server response".into(),
                ));
            }
            Ok(_) => {}
            Err(e) => {
                return Err(tau_agent_base::Error::Io(format!("read error: {}", e)));
            }
        }
        if line.trim().is_empty() {
            continue;
        }
        let req: PluginRequest = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(_) => continue,
        };
        match req {
            PluginRequest::ServerResponse {
                request_id: rid,
                response,
            } if rid == request_id => {
                return Ok(response);
            }
            // A ToolCall arrived while we are mid-ServerRequest (e.g. during a
            // background merge/schedule pass). Answer it immediately with an
            // error so the calling session is not left hanging.
            PluginRequest::ToolCall { tool_call_id, .. } => {
                send_message(
                    writer,
                    &PluginMessage::ToolResult(PluginToolResult {
                        tool_call_id,
                        content: vec![ToolResultContent::Text(TextContent {
                            text: "plugin is busy with a background operation — please retry \
                                   in a moment"
                                .into(),
                            text_signature: None,
                        })],
                        is_error: true,
                        summary: None,
                        post_persist_actions: Vec::new(),
                    }),
                );
                // Continue waiting for our ServerResponse.
            }
            // Ignore other message types (ServerResponse with wrong ID, etc.)
            _ => {}
        }
    }
}
