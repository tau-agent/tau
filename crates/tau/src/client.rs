//! Unix socket client — connects to the tau server.

use std::os::unix::net::UnixStream;
use std::pin::Pin;

use futures::StreamExt;
use futures::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
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
        let mut line =
            serde_json::to_string(req).map_err(|e| crate::Error::Parse(e.to_string()))?;
        line.push('\n');
        self.stream
            .write_all(line.as_bytes())
            .await
            .map_err(|e| crate::Error::Io(e.to_string()))?;
        self.stream
            .flush()
            .await
            .map_err(|e| crate::Error::Io(e.to_string()))?;
        Ok(())
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
                | Response::Error { .. }
                | Response::Ok
                | Response::SessionCreated { .. }
                | Response::SessionInfo { .. }
                | Response::SessionDeleted
                | Response::Sessions { .. }
                | Response::Models { .. }
                | Response::ModelChanged { .. }
                | Response::LoginSuccess { .. }
                | Response::AuthStatus { .. }
                | Response::SubscriptionUsage { .. }
                | Response::ServerShutdown { .. } => true,
            };
            on_response(&resp);
            if is_terminal {
                break;
            }
        }
        Ok(())
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
