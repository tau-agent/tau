//! Unix socket client — connects to the tau server.

use std::os::unix::net::UnixStream;
use std::pin::Pin;

use futures::StreamExt;
use futures::io::{AsyncBufReadExt, BufReader};
use smol::Async;

use crate::protocol::{Request, Response};
use crate::server::socket_path;

/// A connection to the tau server.
pub struct Client {
    stream: Pin<Box<Async<UnixStream>>>,
}

impl Client {
    /// Connect to a running server.
    pub async fn connect() -> crate::Result<Self> {
        let path = socket_path();
        let stream = Async::<UnixStream>::connect(&path)
            .await
            .map_err(|e| crate::Error::Io(format!("connect {}: {}", path.display(), e)))?;
        Ok(Self {
            stream: Box::pin(stream),
        })
    }

    /// Connect, auto-starting the server if needed.
    pub async fn connect_or_start() -> crate::Result<Self> {
        if !crate::server::is_running() {
            start_server_daemon()?;
            // Wait for socket to appear
            for _ in 0..50 {
                smol::Timer::after(std::time::Duration::from_millis(100)).await;
                if crate::server::is_running() {
                    break;
                }
            }
            if !crate::server::is_running() {
                return Err(crate::Error::Io("server failed to start".into()));
            }
        }
        Self::connect().await
    }

    /// Send a request.
    pub async fn send(&mut self, req: &Request) -> crate::Result<()> {
        crate::write_json_line_async(&mut self.stream, req).await
    }

    /// Read all responses until a terminal one arrives.
    /// Calls `on_response` for each response.
    pub async fn recv_streaming<F>(&mut self, mut on_response: F) -> crate::Result<()>
    where
        F: FnMut(&Response),
    {
        let reader = BufReader::new(&mut *self.stream);
        let mut lines = reader.lines();
        while let Some(line) = lines.next().await {
            let line = line.map_err(|e: std::io::Error| crate::Error::Io(e.to_string()))?;
            if line.trim().is_empty() {
                continue;
            }
            let resp: Response =
                serde_json::from_str(&line).map_err(|e| crate::Error::Parse(e.to_string()))?;
            let is_terminal = match &resp {
                Response::Stream { event } => {
                    // Stream errors are terminal (agent won't continue)
                    matches!(event.as_ref(), crate::types::StreamEvent::Error { .. })
                }
                Response::AgentDone
                | Response::Cancelled
                | Response::Error { .. }
                | Response::Ok
                | Response::SessionCreated { .. }
                | Response::SessionInfo { .. }
                | Response::SessionDeleted
                | Response::SessionArchived
                | Response::SessionRestored
                | Response::Sessions { .. }
                | Response::Models { .. }
                | Response::ModelChanged { .. }
                | Response::Messages { .. }
                | Response::UserMessage { .. }
                | Response::LoginSuccess { .. }
                | Response::AuthStatus { .. }
                | Response::SubscriptionUsage { .. }
                | Response::SessionsCompleted { .. }
                | Response::MessageReply { .. }
                | Response::GcComplete { .. }
                | Response::ToolExecuted { .. }
                | Response::ServerShutdown { .. } => true,
            };
            on_response(&resp);
            if is_terminal {
                break;
            }
        }
        Ok(())
    }
    /// Read all responses indefinitely (no terminal detection).
    /// Used for long-lived subscription connections.
    pub async fn recv_lines<F>(&mut self, mut on_response: F) -> crate::Result<()>
    where
        F: FnMut(&Response),
    {
        let reader = BufReader::new(&mut *self.stream);
        let mut lines = reader.lines();
        while let Some(line) = lines.next().await {
            let line = line.map_err(|e: std::io::Error| crate::Error::Io(e.to_string()))?;
            if line.trim().is_empty() {
                continue;
            }
            let resp: Response =
                serde_json::from_str(&line).map_err(|e| crate::Error::Parse(e.to_string()))?;
            on_response(&resp);
        }
        Ok(())
    }

    /// Return an async stream of parsed responses.
    /// Consumes self — the stream owns the connection.
    /// Used for long-lived subscriptions where the caller needs async control
    /// (e.g. to apply backpressure via channel send().await).
    pub fn response_stream(self) -> impl futures::Stream<Item = crate::Result<Response>> {
        let reader = BufReader::new(self.stream);
        reader.lines().filter_map(|line_result| {
            let result = match line_result {
                Ok(line) if line.trim().is_empty() => None,
                Ok(line) => Some(
                    serde_json::from_str(&line)
                        .map_err(|e: serde_json::Error| crate::Error::Parse(e.to_string())),
                ),
                Err(e) => Some(Err(crate::Error::Io(e.to_string()))),
            };
            async { result }
        })
    }
    /// Execute a tool directly on a session (no LLM involved).
    /// Returns `(content, is_error)`.
    pub async fn execute_tool(
        &mut self,
        session_id: &str,
        tool_name: &str,
        arguments: serde_json::Value,
    ) -> crate::Result<(String, bool)> {
        self.send(&Request::ExecuteTool {
            session_id: session_id.to_string(),
            tool_name: tool_name.to_string(),
            arguments,
        })
        .await?;
        let mut result = None;
        self.recv_streaming(|resp| {
            if let Response::ToolExecuted { content, is_error } = resp {
                result = Some((content.clone(), *is_error));
            }
        })
        .await?;
        result.ok_or_else(|| crate::Error::Io("no ToolExecuted response received".into()))
    }
}

/// Spawn the server as a background daemon process.
fn start_server_daemon() -> crate::Result<()> {
    let exe = std::env::current_exe().map_err(|e| crate::Error::Io(e.to_string()))?;
    std::process::Command::new(exe)
        .args(["server", "start", "--foreground"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| crate::Error::Io(format!("spawn server: {}", e)))?;
    Ok(())
}
