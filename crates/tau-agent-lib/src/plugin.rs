//! Subprocess plugin system.
//!
//! Plugins are external processes that communicate via JSON-lines on stdin/stdout.
//! They can register tools, hooks, and slash commands.
//!
//! Two scopes:
//! - **global** plugins: spawned once at server start, shared across sessions.
//! - **session** plugins: spawned per session, killed when session is destroyed.
//!   An optional `session_prefix` is prepended to all session plugin commands
//!   (e.g. `["sandbox", "run", "--"]`).

use std::collections::HashMap;
use std::io::{BufRead, BufReader, BufWriter};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::types::{Tool, ToolCall};

/// Handle used to update the name a `spawn_stderr_forwarder` thread emits
/// under. At spawn time we don't know the plugin's registered name yet,
/// so the forwarder starts with a placeholder; once registration arrives
/// we overwrite the shared name and all subsequent lines carry the real
/// plugin name.
type StderrPluginName = std::sync::Arc<std::sync::Mutex<String>>;

/// Spawn a background thread that forwards the plugin's stderr to tracing.
///
/// Each line is emitted at `info` level with `target="plugin"` and a
/// `plugin=<name>` field. When the plugin exits and closes its stderr pipe,
/// the reader's `lines()` iterator ends naturally and the thread terminates.
///
/// A dedicated OS thread (not a smol task) keeps the forwarder off the async
/// runtime; plugin stderr is low volume and one thread per plugin is cheap.
///
/// Returns a handle that lets the caller update the name the forwarder
/// emits under once registration completes.
fn spawn_stderr_forwarder(
    initial_name: String,
    stderr: std::process::ChildStderr,
) -> StderrPluginName {
    let name: StderrPluginName = std::sync::Arc::new(std::sync::Mutex::new(initial_name));
    let thread_name = name.clone();
    std::thread::Builder::new()
        .name("plugin-stderr".to_string())
        .spawn(move || {
            let reader = BufReader::new(stderr);
            for line in reader.lines().map_while(Result::ok) {
                let n = thread_name
                    .lock()
                    .map(|g| g.clone())
                    .unwrap_or_else(|_| "<unknown>".to_string());
                tracing::info!(target: "plugin", plugin = %n, "{line}");
            }
        })
        .ok();
    name
}

// Re-export wire types from tau-agent-base for backward compatibility
pub use crate::plugin_protocol::{
    HookMessage, HookResult, PluginCommand, PluginMessage, PluginRegistration, PluginRequest,
    PluginToolDef, PluginToolResult,
};

// ---------------------------------------------------------------------------
// Sandbox config: per-project sandbox prefix resolution
// ---------------------------------------------------------------------------

/// Configuration from `sandbox.toml`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SandboxConfig {
    /// Default sandbox prefix for this tier.
    #[serde(default)]
    pub prefix: Option<Vec<String>>,
    /// Named sandbox profiles.
    #[serde(default)]
    pub profiles: HashMap<String, SandboxProfile>,
}

/// A named sandbox profile with its own prefix command.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxProfile {
    pub prefix: Vec<String>,
}

/// Resolve the sandbox prefix for a session.
///
/// Resolution order:
/// 1. Load `sandbox.toml` via `config_chain::load_first` (operator > global,
///    NO project tier — security-sensitive).
/// 2. If `sandbox_profile` is specified, look up that profile in the config.
/// 3. Use the default `prefix` from `sandbox.toml`.
/// 4. Fall back to `session_prefix` from `plugins.toml` (backward compat).
///
/// The `{cwd}` placeholder is NOT expanded here — callers expand it when
/// building the final command.
pub fn resolve_sandbox_prefix(
    project_name: Option<&str>,
    sandbox_profile: Option<&str>,
    legacy_prefix: Option<&[String]>,
) -> Option<Vec<String>> {
    // Load sandbox.toml via config chain (operator > global, NO project tier)
    let config: Option<SandboxConfig> = crate::config_chain::load_first(
        project_name,
        None, // project_path — not used since allow_project_tier=false
        "sandbox.toml",
        false, // security-sensitive: skip project tier
    );

    if let Some(config) = config {
        // If a profile was requested, try to resolve it
        if let Some(profile_name) = sandbox_profile {
            if let Some(profile) = config.profiles.get(profile_name) {
                return Some(profile.prefix.clone());
            }
            tracing::warn!(
                profile = %profile_name,
                "sandbox profile not found, using default"
            );
        }
        // Use the default prefix from sandbox.toml
        if config.prefix.is_some() {
            return config.prefix;
        }
    }

    // Fall back to legacy session_prefix from plugins.toml
    legacy_prefix.map(|p| p.to_vec())
}

// ---------------------------------------------------------------------------
// Plugin handle
// ---------------------------------------------------------------------------

/// Async stdout reader type (extracted from a plugin for background reading).
pub type AsyncPluginReader = futures::io::BufReader<Box<dyn futures::io::AsyncRead + Unpin + Send>>;
/// Async stdin writer type (extracted from a plugin for background writing).
pub type AsyncPluginWriter = futures::io::BufWriter<smol::Async<std::fs::File>>;

/// A running plugin process.
pub struct PluginHandle {
    pub name: String,
    pub registration: PluginRegistration,
    child: Option<Child>,
    stdin: Option<BufWriter<ChildStdin>>,
    stdout: Option<BufReader<ChildStdout>>,
    /// Async pipe fields for non-blocking I/O (used by server-side tool execution).
    async_stdin: Option<AsyncPluginWriter>,
    async_stdout: Option<AsyncPluginReader>,
    /// Command used to spawn this plugin (for respawning).
    spawn_command: Vec<String>,
    /// Working directory used to spawn this plugin.
    spawn_cwd: String,
    /// Environment variables used to spawn this plugin.
    spawn_env: HashMap<String, String>,
    /// When the plugin last had activity (tool call, hook, etc.).
    pub last_activity: Instant,
    /// When set, a background task owns the async I/O pipes.
    /// `read_message_async` reads from this channel instead of stdout directly.
    bg_msg_rx: Option<smol::channel::Receiver<PluginMessage>>,
    /// When set, `send_async` writes to this channel, and a background
    /// writer task drains it to the real stdin.
    bg_write_tx: Option<smol::channel::Sender<PluginRequest>>,
}

/// Resolve a cwd path, falling back to the nearest existing ancestor if the
/// original directory has been removed (e.g. a cleaned-up worktree).
fn resolve_cwd(cwd: &str) -> String {
    let path = Path::new(cwd);
    if path.is_dir() {
        return cwd.to_string();
    }
    // Walk up to find the nearest existing ancestor.
    let mut ancestor = path.parent();
    while let Some(p) = ancestor {
        if p.is_dir() {
            tracing::warn!(
                cwd = %cwd,
                fallback = %p.display(),
                "plugin cwd does not exist, falling back to nearest ancestor"
            );
            return p.to_string_lossy().to_string();
        }
        ancestor = p.parent();
    }
    // Last resort.
    tracing::warn!(
        cwd = %cwd,
        "plugin cwd does not exist and no ancestor found, falling back to /tmp"
    );
    "/tmp".to_string()
}

impl PluginHandle {
    /// Spawn a plugin process and read its registration.
    pub fn spawn(
        command: &[String],
        cwd: &str,
        env: &HashMap<String, String>,
    ) -> crate::Result<Self> {
        if command.is_empty() {
            return Err(crate::Error::Io("empty plugin command".into()));
        }

        let span = tracing::info_span!("plugin.spawn", cmd = ?command, cwd = %cwd);
        let _enter = span.enter();
        tracing::debug!("spawning");

        let effective_cwd = resolve_cwd(cwd);
        let mut cmd = Command::new(&command[0]);
        cmd.args(&command[1..])
            .current_dir(&effective_cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, val) in env {
            cmd.env(key, val);
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| crate::Error::Io(format!("spawn plugin {:?}: {}", command, e)))?;

        let stdin = BufWriter::new(
            child
                .stdin
                .take()
                .ok_or_else(|| crate::Error::Io("plugin stdin not available".into()))?,
        );
        let stdout = BufReader::new(
            child
                .stdout
                .take()
                .ok_or_else(|| crate::Error::Io("plugin stdout not available".into()))?,
        );

        // Forward stderr to tracing on a background thread. We don't know
        // the plugin's registered name yet, so start with a placeholder
        // derived from the binary path; once registration completes below
        // we overwrite the shared name so subsequent lines carry the real
        // plugin name.
        let stderr_name = if let Some(pipe) = child.stderr.take() {
            let placeholder = format!(
                "<spawning {:?}>",
                command.first().cloned().unwrap_or_default()
            );
            Some(spawn_stderr_forwarder(placeholder, pipe))
        } else {
            None
        };

        let mut handle = Self {
            name: String::new(),
            registration: PluginRegistration {
                name: String::new(),
                tools: Vec::new(),
                hooks: Vec::new(),
                commands: Vec::new(),
            },
            child: Some(child),
            stdin: Some(stdin),
            stdout: Some(stdout),
            async_stdin: None,
            async_stdout: None,
            spawn_command: command.to_vec(),
            spawn_cwd: cwd.to_string(),
            spawn_env: env.clone(),
            last_activity: Instant::now(),
            bg_msg_rx: None,
            bg_write_tx: None,
        };

        // Read the registration message
        tracing::debug!("reading registration");
        let msg = handle.read_message();
        match msg {
            Ok(PluginMessage::Register(reg)) => {
                tracing::info!(
                    plugin = %reg.name,
                    tools = reg.tools.len(),
                    hooks = ?reg.hooks,
                    "registered"
                );
                // Update the stderr forwarder so subsequent lines carry
                // the real plugin name instead of the placeholder.
                if let Some(ref n) = stderr_name
                    && let Ok(mut g) = n.lock()
                {
                    *g = reg.name.clone();
                }
                handle.name = reg.name.clone();
                handle.registration = reg;
            }
            Ok(other) => {
                tracing::warn!(?other, "unexpected first message");
                return Err(crate::Error::Io(
                    "plugin first message must be Register".into(),
                ));
            }
            Err(e) => {
                tracing::error!(%e, "registration failed");
                // Child likely died -- wait for it and report exit status.
                let mut diag = format!("plugin {:?} failed during registration: {}", command, e);
                std::thread::sleep(std::time::Duration::from_millis(100));
                if let Some(ref mut child) = handle.child {
                    match child.try_wait() {
                        Ok(Some(exit)) => {
                            diag.push_str(&format!("\n  exit status: {}", exit));
                        }
                        _ => {
                            // Child still running but stdout closed -- kill it
                            let _ = child.kill();
                            let _ = child.wait();
                        }
                    }
                }
                diag.push_str(
                    "\n  (plugin stderr was forwarded to tracing; see server log for details)",
                );
                return Err(crate::Error::Io(diag));
            }
        }

        Ok(handle)
    }

    /// Send a request to the plugin.
    ///
    /// When the handle has been upgraded to background I/O (global plugins
    /// after `PluginManager::setup_background_io`), the sync stdin pipe is
    /// no longer held by this handle. In that case, forward the request
    /// through the background writer channel so that hooks / session_start
    /// notifications sent from sync call sites still reach the plugin.
    pub fn send(&mut self, req: &PluginRequest) -> crate::Result<()> {
        self.last_activity = Instant::now();

        // Prefer the background writer channel when sync pipes have been
        // handed over to the background I/O tasks.
        if let Some(ref tx) = self.bg_write_tx {
            return smol::block_on(tx.send(req.clone())).map_err(|e| {
                crate::Error::Io(format!(
                    "plugin {} background write channel closed: {}",
                    self.name, e
                ))
            });
        }

        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| crate::Error::Io(format!("plugin {} is not running", self.name)))?;
        crate::write_json_line(stdin, req)
            .map_err(|e| crate::Error::Io(format!("write to plugin {}: {}", self.name, e)))
    }

    /// Read a single message from the plugin.
    ///
    /// When background I/O is installed, read from the background message
    /// channel instead of the sync stdout pipe (which has been handed over
    /// to the background reader task).
    pub fn read_message(&mut self) -> crate::Result<PluginMessage> {
        // Prefer the background message channel for global plugins whose
        // stdout has been handed over to a background reader task.
        if let Some(ref rx) = self.bg_msg_rx {
            return smol::block_on(rx.recv()).map_err(|_| {
                crate::Error::Io(format!(
                    "plugin {} background message channel closed",
                    self.name
                ))
            });
        }

        let stdout = self
            .stdout
            .as_mut()
            .ok_or_else(|| crate::Error::Io(format!("plugin {} is not running", self.name)))?;
        match crate::read_json_line(stdout).map_err(|e| match e {
            crate::Error::Parse(msg) => {
                crate::Error::Parse(format!("plugin {} message: {}", self.name, msg))
            }
            other => crate::Error::Io(format!("read from plugin {}: {}", self.name, other)),
        })? {
            Some(val) => Ok(val),
            None => {
                let mut msg = format!("plugin {} closed unexpectedly", self.name);
                // Wait briefly for child to fully exit so we can log the code.
                if let Some(ref mut child) = self.child {
                    let _ = child.try_wait();
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
                if let Some(exit) = self.child_exit_status() {
                    msg.push_str(&format!(" (exit status: {})", exit));
                }
                // Plugin stderr is forwarded to tracing in real time; hint
                // to anyone looking at the error message where to find it.
                msg.push_str(
                    "\n  (plugin stderr was forwarded to tracing; see server log for details)",
                );
                // Mark as dead
                self.child = None;
                self.stdin = None;
                self.stdout = None;
                self.async_stdin = None;
                self.async_stdout = None;
                Err(crate::Error::Io(msg))
            }
        }
    }

    // -------------------------------------------------------------------
    // Async I/O methods (for server-side tool execution without blocking)
    // -------------------------------------------------------------------

    /// Convert sync I/O pipes to async. After this, `send_async` and
    /// `read_message_async` are available, and the sync `send`/`read_message`
    /// will no longer work (the sync pipes are consumed).
    ///
    /// This uses `smol::Async` to wrap the raw file descriptors for
    /// non-blocking I/O on the smol executor.
    pub fn upgrade_to_async(&mut self) -> crate::Result<()> {
        use std::os::unix::io::{FromRawFd, IntoRawFd};

        // Take the sync pipes (consuming them).
        let sync_stdin = self
            .stdin
            .take()
            .ok_or_else(|| crate::Error::Io("no sync stdin to upgrade".into()))?
            .into_inner()
            .map_err(|e| crate::Error::Io(format!("flush stdin: {}", e.error())))?;
        let sync_stdout_buf = self
            .stdout
            .take()
            .ok_or_else(|| crate::Error::Io("no sync stdout to upgrade".into()))?;

        // Preserve any data already buffered by the sync BufReader.
        // BufReader::into_inner() discards its internal buffer, so we must
        // extract the leftover bytes first.
        let leftover = sync_stdout_buf.buffer().to_vec();
        let sync_stdout = sync_stdout_buf.into_inner();

        // Convert to raw fds and wrap in smol::Async for non-blocking I/O.
        let raw_in = sync_stdin.into_raw_fd();
        let raw_out = sync_stdout.into_raw_fd();

        // Safety: these are valid fds from the child process pipes.
        let async_stdin = unsafe { smol::Async::new(std::fs::File::from_raw_fd(raw_in)) }
            .map_err(|e| crate::Error::Io(format!("async wrap stdin: {}", e)))?;
        let async_stdout = unsafe { smol::Async::new(std::fs::File::from_raw_fd(raw_out)) }
            .map_err(|e| crate::Error::Io(format!("async wrap stdout: {}", e)))?;

        self.async_stdin = Some(futures::io::BufWriter::new(async_stdin));

        // If the sync BufReader had leftover data, chain it in front of the
        // async reader so it gets processed first.
        if leftover.is_empty() {
            let boxed: Box<dyn futures::io::AsyncRead + Unpin + Send> = Box::new(async_stdout);
            self.async_stdout = Some(futures::io::BufReader::new(boxed));
        } else {
            use futures::io::AsyncReadExt;
            let cursor = futures::io::Cursor::new(leftover);
            let chained: Box<dyn futures::io::AsyncRead + Unpin + Send> =
                Box::new(cursor.chain(async_stdout));
            self.async_stdout = Some(futures::io::BufReader::new(chained));
        }
        Ok(())
    }

    /// Send a request to the plugin asynchronously.
    ///
    /// If a background writer channel is installed (via [`set_background_channels`]),
    /// the request is sent through the channel and a background task writes it
    /// to the real stdin.  Otherwise, writes directly to the async stdin pipe.
    pub async fn send_async(&mut self, req: &PluginRequest) -> crate::Result<()> {
        self.last_activity = Instant::now();

        // If a background writer channel is installed, route through it.
        if let Some(ref tx) = self.bg_write_tx {
            return tx.send(req.clone()).await.map_err(|e| {
                crate::Error::Io(format!(
                    "plugin {} background write channel closed: {}",
                    self.name, e
                ))
            });
        }

        let stdin = self.async_stdin.as_mut().ok_or_else(|| {
            crate::Error::Io(format!("plugin {} async stdin not available", self.name))
        })?;
        crate::write_json_line_async(stdin, req)
            .await
            .map_err(|e| crate::Error::Io(format!("write to plugin {}: {}", self.name, e)))
    }

    /// Read a single message from the plugin asynchronously.
    ///
    /// If a background reader channel is installed (via [`set_background_channels`]),
    /// reads from that channel.  Otherwise, reads directly from the async stdout pipe.
    pub async fn read_message_async(&mut self) -> crate::Result<PluginMessage> {
        // If a background reader channel is installed, read from it.
        if let Some(ref rx) = self.bg_msg_rx {
            return rx.recv().await.map_err(|_| {
                crate::Error::Io(format!(
                    "plugin {} background message channel closed",
                    self.name
                ))
            });
        }

        let stdout = self.async_stdout.as_mut().ok_or_else(|| {
            crate::Error::Io(format!("plugin {} async stdout not available", self.name))
        })?;
        match crate::read_json_line_async(stdout)
            .await
            .map_err(|e| match e {
                crate::Error::Parse(msg) => {
                    crate::Error::Parse(format!("plugin {} message: {}", self.name, msg))
                }
                other => crate::Error::Io(format!("read from plugin {}: {}", self.name, other)),
            })? {
            Some(val) => Ok(val),
            None => {
                let msg = format!("plugin {} closed unexpectedly", self.name);
                self.async_stdin = None;
                self.async_stdout = None;
                Err(crate::Error::Io(msg))
            }
        }
    }

    /// Check if this handle has async I/O pipes available.
    ///
    /// Returns true if either the direct async pipes or background channels
    /// are available.
    pub fn has_async_io(&self) -> bool {
        // Background channels count as async I/O.
        if self.bg_msg_rx.is_some() && self.bg_write_tx.is_some() {
            return true;
        }
        self.async_stdin.is_some() && self.async_stdout.is_some()
    }

    /// Extract the async I/O pipes from this handle for use by background tasks.
    ///
    /// Returns `(reader, writer)`.  After this call, direct async I/O is no
    /// longer possible on this handle — callers must install background
    /// channels via [`set_background_channels`].
    pub fn take_async_io(&mut self) -> crate::Result<(AsyncPluginReader, AsyncPluginWriter)> {
        let reader = self
            .async_stdout
            .take()
            .ok_or_else(|| crate::Error::Io("no async stdout to take".into()))?;
        let writer = self
            .async_stdin
            .take()
            .ok_or_else(|| crate::Error::Io("no async stdin to take".into()))?;
        Ok((reader, writer))
    }

    /// Extract just the async writer, leaving the async reader in place.
    ///
    /// Used by [`super::server::agent_runner::PluginExecutor::execute`] to
    /// install a per-tool-call writer task that can serve concurrent senders
    /// (main loop, ServerResponse path, cancel watcher). The writer is
    /// returned to the handle via [`restore_async_writer`] when the call
    /// completes.
    pub fn take_async_writer(&mut self) -> crate::Result<AsyncPluginWriter> {
        self.async_stdin
            .take()
            .ok_or_else(|| crate::Error::Io("no async stdin to take".into()))
    }

    /// Put an async writer back onto the handle. Counterpart of
    /// [`take_async_writer`].
    pub fn restore_async_writer(&mut self, writer: AsyncPluginWriter) {
        self.async_stdin = Some(writer);
    }

    /// Install background channels for reading and writing.
    ///
    /// After this, [`read_message_async`] receives from `msg_rx` and
    /// [`send_async`] sends through `write_tx`.
    pub fn set_background_channels(
        &mut self,
        msg_rx: smol::channel::Receiver<PluginMessage>,
        write_tx: smol::channel::Sender<PluginRequest>,
    ) {
        self.bg_msg_rx = Some(msg_rx);
        self.bg_write_tx = Some(write_tx);
    }

    /// Return a clone of this handle's background write channel, if one is
    /// installed. Used by per-tool-call cancel watchers that need to send
    /// [`PluginRequest::CancelToolCall`] to the plugin *while* a tool is
    /// mid-execution — the main loop is blocked inside `read_message_async`
    /// so it cannot write through the handle directly.
    ///
    /// Returns `None` for handles that still use sync I/O.
    pub fn background_write_tx(&self) -> Option<smol::channel::Sender<PluginRequest>> {
        self.bg_write_tx.clone()
    }

    /// Execute a tool call, calling on_output for streaming deltas.
    pub fn execute_tool(
        &mut self,
        tool_call: &ToolCall,
        cwd: Option<&str>,
        session_id: Option<&str>,
        on_output: &mut dyn FnMut(&str),
    ) -> crate::Result<crate::types::ToolResultMessage> {
        self.execute_tool_with_server(tool_call, cwd, session_id, on_output, None, None)
    }

    /// Execute a tool call with optional server request handler.
    /// `on_server_request` is called when the plugin sends a ServerRequest.
    /// It receives the Request and returns a Response.
    pub fn execute_tool_with_server(
        &mut self,
        tool_call: &ToolCall,
        cwd: Option<&str>,
        session_id: Option<&str>,
        on_output: &mut dyn FnMut(&str),
        mut on_server_request: Option<
            &mut dyn FnMut(&crate::protocol::Request) -> crate::protocol::Response,
        >,
        project_name: Option<&str>,
    ) -> crate::Result<crate::types::ToolResultMessage> {
        self.send(&PluginRequest::ToolCall {
            tool_call_id: tool_call.id.clone(),
            name: tool_call.name.clone(),
            arguments: tool_call.arguments.clone(),
            cwd: cwd.map(String::from),
            session_id: session_id.map(String::from),
            project_name: project_name.map(String::from),
        })?;

        loop {
            let msg = self.read_message()?;
            match msg {
                PluginMessage::OutputDelta { text, .. } => {
                    on_output(&text);
                }
                PluginMessage::ToolResult(result) => {
                    return Ok(crate::types::ToolResultMessage {
                        tool_call_id: result.tool_call_id,
                        tool_name: tool_call.name.clone(),
                        content: result.content,
                        details: None,
                        is_error: result.is_error,
                        timestamp: crate::types::timestamp_ms(),
                        duration_ms: None,
                        summary: result.summary,
                        post_persist_actions: result.post_persist_actions,
                    });
                }
                PluginMessage::ServerRequest {
                    request_id,
                    request,
                } => {
                    let response = match on_server_request {
                        Some(ref mut handler) => handler(&request),
                        None => crate::protocol::Response::Error {
                            message: "server requests not supported in this context".into(),
                        },
                    };
                    self.send(&PluginRequest::ServerResponse {
                        request_id,
                        response,
                    })?;
                }
                _ => {
                    // Ignore unexpected messages during tool execution
                }
            }
        }
    }

    /// Call a hook on this plugin.
    pub fn call_hook(&mut self, name: &str, data: serde_json::Value) -> crate::Result<HookResult> {
        let span = tracing::info_span!("plugin.hook", plugin = %self.name, hook = name);
        let _enter = span.enter();
        tracing::debug!("sending");
        self.send(&PluginRequest::Hook {
            name: name.to_string(),
            data,
        })?;

        let msg = self.read_message()?;
        tracing::debug!("returned");
        match msg {
            PluginMessage::HookResult(result) => Ok(result),
            _ => Ok(HookResult::default()),
        }
    }

    /// Try to get the child exit status without blocking.
    fn child_exit_status(&mut self) -> Option<std::process::ExitStatus> {
        self.child.as_mut()?.try_wait().ok().flatten()
    }

    /// Check if the plugin process is alive.
    pub fn is_alive(&mut self) -> bool {
        match self.child {
            Some(ref mut child) => match child.try_wait() {
                Ok(Some(_)) => {
                    // Process exited -- clean up
                    self.child = None;
                    self.stdin = None;
                    self.stdout = None;
                    self.async_stdin = None;
                    self.async_stdout = None;
                    false
                }
                Ok(None) => true, // Still running
                Err(_) => false,
            },
            None => false,
        }
    }

    /// Send an Idle notification. If the plugin exits in response, mark it dead.
    pub fn send_idle(&mut self) {
        if self.stdin.is_none() {
            return; // Already dead
        }
        // Send the idle message; ignore errors (plugin may have already exited)
        let _ = self.send(&PluginRequest::Idle);
        // Give the plugin a moment to exit if it wants to
        std::thread::sleep(std::time::Duration::from_millis(50));
        // Check if it exited
        self.is_alive();
    }

    /// Respawn the plugin process using the original command and cwd.
    /// Preserves the existing registration.
    pub fn respawn(&mut self) -> crate::Result<()> {
        if self.is_alive() {
            return Ok(()); // Already running
        }

        let cmd = &self.spawn_command;
        let span = tracing::info_span!("plugin.respawn", plugin = %self.name, cmd = ?cmd);
        let _enter = span.enter();

        let effective_cwd = resolve_cwd(&self.spawn_cwd);

        let mut cmd_proc = Command::new(&cmd[0]);
        cmd_proc
            .args(&cmd[1..])
            .current_dir(&effective_cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, val) in &self.spawn_env {
            cmd_proc.env(key, val);
        }
        let mut child = cmd_proc
            .spawn()
            .map_err(|e| crate::Error::Io(format!("respawn plugin {:?}: {}", cmd, e)))?;

        let stdin = BufWriter::new(
            child
                .stdin
                .take()
                .ok_or_else(|| crate::Error::Io("plugin stdin not available".into()))?,
        );
        let stdout = BufReader::new(
            child
                .stdout
                .take()
                .ok_or_else(|| crate::Error::Io("plugin stdout not available".into()))?,
        );

        // Start a new stderr forwarder for this process lifetime.
        // The plugin name is already known here, so no placeholder dance.
        if let Some(pipe) = child.stderr.take() {
            let _ = spawn_stderr_forwarder(self.name.clone(), pipe);
        }
        self.child = Some(child);
        self.stdin = Some(stdin);
        self.stdout = Some(stdout);
        self.async_stdin = None;
        self.async_stdout = None;
        self.last_activity = Instant::now();

        // Read registration (must match original, but we don't enforce that)
        let msg = self.read_message();
        match msg {
            Ok(PluginMessage::Register(_reg)) => {
                tracing::info!("respawned");
                Ok(())
            }
            Ok(_) => Err(crate::Error::Io(
                "respawned plugin first message must be Register".into(),
            )),
            Err(e) => {
                tracing::warn!(%e, "respawn failed");
                self.child = None;
                self.stdin = None;
                self.stdout = None;
                self.async_stdin = None;
                self.async_stdout = None;
                Err(crate::Error::Io(format!(
                    "respawn plugin '{}' failed: {}",
                    self.name, e
                )))
            }
        }
    }

    /// Ensure this plugin is running (respawn if dead).
    pub fn ensure_alive(&mut self) -> crate::Result<()> {
        if self.is_alive() {
            Ok(())
        } else {
            self.respawn()
        }
    }

    /// Kill the plugin process.
    pub fn kill(&mut self) {
        if let Some(ref mut child) = self.child {
            match child.try_wait() {
                Ok(Some(_)) => {}
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                }
            }
        }
        self.child = None;
        self.stdin = None;
        self.stdout = None;
        self.async_stdin = None;
        self.async_stdout = None;
        // Close background channels (causes bg tasks to exit).
        self.bg_msg_rx = None;
        self.bg_write_tx = None;
    }

    /// Check if plugin wants a specific hook.
    pub fn wants_hook(&self, name: &str) -> bool {
        self.registration.hooks.iter().any(|h| h == name)
    }

    /// Get Tool definitions for LLM.
    pub fn tool_schemas(&self) -> Vec<Tool> {
        self.registration
            .tools
            .iter()
            .map(|t| Tool {
                name: t.name.clone(),
                description: t.description.clone(),
                parameters: t.parameters.clone(),
            })
            .collect()
    }

    /// Get tool prompt contributions.
    pub fn tool_prompts(&self) -> Vec<crate::system_prompt::ToolPrompt> {
        self.registration
            .tools
            .iter()
            .filter_map(|t| {
                t.prompt_snippet
                    .as_ref()
                    .map(|snippet| crate::system_prompt::ToolPrompt {
                        name: t.name.clone(),
                        snippet: snippet.clone(),
                        guidelines: t.prompt_guidelines.clone(),
                    })
            })
            .collect()
    }

    /// Check if this plugin provides a given tool.
    pub fn has_tool(&self, name: &str) -> bool {
        self.registration.tools.iter().any(|t| t.name == name)
    }
}

impl Drop for PluginHandle {
    fn drop(&mut self) {
        self.kill();
    }
}

// ---------------------------------------------------------------------------
// Helper: collect tool info from a list of plugin handles
// ---------------------------------------------------------------------------

fn collect_tool_schemas(plugins: &[PluginHandle]) -> Vec<Tool> {
    plugins.iter().flat_map(|p| p.tool_schemas()).collect()
}

fn collect_tool_prompts(plugins: &[PluginHandle]) -> Vec<crate::system_prompt::ToolPrompt> {
    plugins.iter().flat_map(|p| p.tool_prompts()).collect()
}

fn find_tool_plugin<'a>(
    plugins: &'a mut [PluginHandle],
    tool_name: &str,
) -> Option<&'a mut PluginHandle> {
    plugins.iter_mut().find(|p| p.has_tool(tool_name))
}

fn call_hook_all(
    plugins: &mut [PluginHandle],
    name: &str,
    data: &serde_json::Value,
) -> Vec<HookResult> {
    call_hook_all_excluding(plugins, name, data, None)
}

fn call_hook_all_excluding(
    plugins: &mut [PluginHandle],
    name: &str,
    data: &serde_json::Value,
    exclude_plugin: Option<&str>,
) -> Vec<HookResult> {
    let mut results = Vec::new();
    for plugin in plugins {
        if exclude_plugin == Some(plugin.name.as_str()) {
            continue;
        }
        if plugin.wants_hook(name) {
            match plugin.call_hook(name, data.clone()) {
                Ok(result) => results.push(result),
                Err(e) => tracing::warn!(
                    plugin = %plugin.name,
                    hook = name,
                    %e,
                    "plugin hook failed"
                ),
            }
        }
    }
    results
}

// ---------------------------------------------------------------------------
// Session plugins: per-session plugin handles
// ---------------------------------------------------------------------------

/// Per-session plugin set. Spawned when the session is first used.
pub struct SessionPlugins {
    plugins: Vec<PluginHandle>,
}

impl SessionPlugins {
    /// Spawn session plugins from config entries, prepending the sandbox prefix.
    /// Returns the plugin set and a list of failure messages for any plugins
    /// that failed to start.
    ///
    /// `sandbox_prefix` overrides `config.session_prefix` when provided.
    pub fn spawn(
        config: &PluginsConfig,
        cwd: &str,
        sandbox_prefix: Option<&[String]>,
    ) -> crate::Result<(Self, Vec<String>)> {
        let mut plugins = Vec::new();
        let mut failures = Vec::new();
        let prefix =
            sandbox_prefix.unwrap_or_else(|| config.session_prefix.as_deref().unwrap_or(&[]));

        let entries: Vec<_> = if config.session.is_empty() && !config.no_default_worker {
            // No config: use default built-in worker
            let exe = std::env::current_exe()
                .map_err(|e| crate::Error::Io(e.to_string()))?
                .to_string_lossy()
                .to_string();
            vec![(
                "worker".to_string(),
                PluginEntry {
                    command: vec![exe, "worker".to_string()],
                    env: HashMap::new(),
                },
            )]
        } else {
            config
                .session
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        };

        for (name, entry) in &entries {
            let mut cmd: Vec<String> = prefix.iter().map(|s| s.replace("{cwd}", cwd)).collect();
            cmd.extend(entry.command.iter().cloned());

            tracing::info!(plugin = %name, cmd = ?cmd, "spawning session plugin");
            match PluginHandle::spawn(&cmd, cwd, &entry.env) {
                Ok(handle) => {
                    let tools: Vec<&str> = handle
                        .registration
                        .tools
                        .iter()
                        .map(|t| t.name.as_str())
                        .collect();
                    tracing::info!(
                        plugin = %handle.name,
                        tools = tools.len(),
                        tool_names = ?tools,
                        "session plugin ready"
                    );
                    plugins.push(handle);
                }
                Err(e) => {
                    tracing::warn!(plugin = %name, %e, "session plugin failed to spawn");
                    failures.push(format!("\u{26a0} Plugin '{}' failed to start: {}", name, e));
                }
            }
        }

        Ok((Self { plugins }, failures))
    }

    pub fn tool_schemas(&self) -> Vec<Tool> {
        collect_tool_schemas(&self.plugins)
    }

    pub fn tool_prompts(&self) -> Vec<crate::system_prompt::ToolPrompt> {
        collect_tool_prompts(&self.plugins)
    }

    pub fn has_tool(&self, name: &str) -> bool {
        self.plugins.iter().any(|p| p.has_tool(name))
    }

    /// Execute a tool call, routing to the right session plugin.
    /// Respawns the plugin if it has exited.
    pub fn execute_tool(
        &mut self,
        tool_call: &ToolCall,
        cwd: &str,
        session_id: Option<&str>,
        on_output: &mut dyn FnMut(&str),
    ) -> crate::Result<crate::types::ToolResultMessage> {
        // Ensure the target plugin is alive (respawn if idle-exited)
        if let Some(p) = find_tool_plugin(&mut self.plugins, &tool_call.name) {
            p.ensure_alive()?;
            p.execute_tool(tool_call, Some(cwd), session_id, on_output)
        } else {
            Err(crate::Error::Io(format!(
                "no session plugin provides tool '{}'",
                tool_call.name
            )))
        }
    }

    pub fn call_hook(&mut self, name: &str, data: &serde_json::Value) -> Vec<HookResult> {
        // Ensure plugins are alive before calling hooks
        for p in &mut self.plugins {
            if p.wants_hook(name) && p.ensure_alive().is_err() {
                tracing::warn!(plugin = %p.name, hook = name, "plugin respawn for hook failed");
            }
        }
        call_hook_all(&mut self.plugins, name, data)
    }

    /// Send idle notification to all plugins. Plugins may exit in response.
    pub fn send_idle_all(&mut self) {
        for p in &mut self.plugins {
            p.send_idle();
        }
    }

    /// Check the oldest last_activity across all plugins.
    pub fn last_activity(&self) -> Option<Instant> {
        self.plugins.iter().map(|p| p.last_activity).min()
    }

    /// Check if any plugin is still alive.
    pub fn any_alive(&mut self) -> bool {
        self.plugins.iter_mut().any(|p| p.is_alive())
    }

    pub fn kill_all(&mut self) {
        for p in &mut self.plugins {
            p.kill();
        }
    }
}

impl Drop for SessionPlugins {
    fn drop(&mut self) {
        self.kill_all();
    }
}

// ---------------------------------------------------------------------------
// Plugin manager: global plugins + per-session plugin tracking
// ---------------------------------------------------------------------------

/// Where a borrowed plugin handle came from (for returning it).
pub enum PluginSource {
    /// From session plugins, at this Vec index.
    Session { session_id: String, index: usize },
    /// From global plugins, at this Vec index.
    Global { index: usize },
}

/// Manages global plugins and per-session plugin sets.
pub struct PluginManager {
    /// Global plugins (spawned once at server start).
    global_plugins: Vec<PluginHandle>,
    /// Cached tool schemas from global plugins.
    ///
    /// Populated by `load_global_plugins` and never modified by
    /// `take_tool_plugin`/`return_tool_plugin`. This ensures that
    /// `tool_schemas` always returns the full set of global tools even
    /// when a plugin handle is temporarily taken for tool execution.
    ///
    /// Without this cache, a race condition exists: `task_dispatch` takes
    /// the tasks plugin handle, spawns a child session via ServerRequest,
    /// and the child session's `tool_schemas` call runs before the handle
    /// is returned — resulting in task tools being absent from the LLM
    /// context.
    global_tool_cache: Vec<(Vec<Tool>, Vec<crate::system_prompt::ToolPrompt>)>,
    /// Per-session plugin sets, keyed by session ID.
    session_plugins: HashMap<String, SessionPlugins>,
    /// Sessions that have already received session_start.
    initialized_sessions: std::collections::HashSet<String>,
    /// Config for spawning session plugins.
    config: PluginsConfig,
}

impl PluginManager {
    pub fn new(config: PluginsConfig) -> Self {
        Self {
            global_plugins: Vec::new(),
            global_tool_cache: Vec::new(),
            session_plugins: HashMap::new(),
            initialized_sessions: std::collections::HashSet::new(),
            config,
        }
    }

    /// Reload the plugin configuration from disk.
    pub fn reload_config(&mut self) {
        self.config = load_plugins_config();
    }

    /// Load global plugins from config.
    /// Kills any existing global plugins first.
    pub fn load_global_plugins(&mut self, cwd: &str) {
        let span = tracing::info_span!("plugin.load_global", cwd = %cwd);
        let _enter = span.enter();
        // Kill existing global plugins before reloading
        for p in &mut self.global_plugins {
            p.kill();
        }
        self.global_plugins.clear();

        for (name, entry) in &self.config.global {
            match PluginHandle::spawn(&entry.command, cwd, &entry.env) {
                Ok(handle) => {
                    let tools: Vec<&str> = handle
                        .registration
                        .tools
                        .iter()
                        .map(|t| t.name.as_str())
                        .collect();
                    let hooks = &handle.registration.hooks;
                    tracing::info!(
                        plugin = %handle.name,
                        tools = tools.len(),
                        tool_names = ?tools,
                        hooks = hooks.len(),
                        hook_names = ?hooks,
                        "global plugin ready"
                    );
                    self.global_plugins.push(handle);
                }
                Err(e) => {
                    tracing::warn!(plugin = %name, %e, "global plugin failed to load");
                }
            }
        }

        // Auto-spawn built-in tasks plugin if not already configured
        if !self.global_plugins.iter().any(|p| p.name == "tasks")
            && let Ok(exe) = std::env::current_exe()
        {
            let exe = exe.to_string_lossy().to_string();
            let cmd = vec![exe, "plugin-tasks".to_string()];
            match PluginHandle::spawn(&cmd, cwd, &HashMap::new()) {
                Ok(handle) => {
                    tracing::info!(
                        tools = handle.registration.tools.len(),
                        "auto-spawned tasks plugin"
                    );
                    self.global_plugins.push(handle);
                }
                Err(e) => {
                    tracing::warn!(%e, "failed to auto-spawn tasks plugin");
                }
            }
        }

        // Rebuild the global tool cache so that tool_schemas / tool_prompts
        // always return the complete set even when a handle is temporarily
        // taken for tool execution.
        self.rebuild_global_tool_cache();
    }

    /// Rebuild the cached tool schemas/prompts from the current global plugins.
    fn rebuild_global_tool_cache(&mut self) {
        self.global_tool_cache = self
            .global_plugins
            .iter()
            .map(|p| (p.tool_schemas(), p.tool_prompts()))
            .collect();
    }

    /// Set up background I/O for global plugins.
    ///
    /// For each global plugin, upgrades its pipes to async (if not already),
    /// extracts the raw async reader/writer, installs channel-based I/O on
    /// the handle, and returns the extracted I/O pairs along with plugin names.
    ///
    /// The caller should spawn background reader/writer tasks for each
    /// returned pair.
    #[allow(clippy::type_complexity)]
    pub fn setup_background_io(
        &mut self,
    ) -> Vec<(
        String,
        AsyncPluginReader,
        AsyncPluginWriter,
        smol::channel::Sender<PluginMessage>,
        smol::channel::Receiver<PluginRequest>,
    )> {
        let mut result = Vec::new();
        for handle in &mut self.global_plugins {
            // Upgrade to async if needed.
            if !handle.has_async_io()
                && let Err(e) = handle.upgrade_to_async()
            {
                tracing::warn!(
                    plugin = %handle.name,
                    %e,
                    "failed to upgrade global plugin to async"
                );
                continue;
            }

            // Extract the raw async I/O.
            let (reader, writer) = match handle.take_async_io() {
                Ok(io) => io,
                Err(e) => {
                    tracing::warn!(
                        plugin = %handle.name,
                        %e,
                        "failed to take async IO for global plugin"
                    );
                    continue;
                }
            };

            // Create channels: bg reader → handle, handle → bg writer.
            let (msg_tx, msg_rx) = smol::channel::unbounded::<PluginMessage>();
            let (write_tx, write_rx) = smol::channel::unbounded::<PluginRequest>();
            handle.set_background_channels(msg_rx, write_tx);

            result.push((handle.name.clone(), reader, writer, msg_tx, write_rx));
        }
        result
    }

    /// Ensure session plugins are spawned for the given session.
    /// Returns a list of failure messages for any plugins that failed to start.
    ///
    /// `project_name` is used to resolve per-project sandbox prefix from
    /// `sandbox.toml` via the config chain (operator > global).
    pub fn ensure_session_plugins(
        &mut self,
        session_id: &str,
        cwd: &str,
        project_name: Option<&str>,
        sandbox_profile: Option<&str>,
    ) -> crate::Result<Vec<String>> {
        if self.session_plugins.contains_key(session_id) {
            return Ok(Vec::new());
        }
        let span = tracing::info_span!(
            "plugin.ensure_session",
            session_id = %session_id,
            cwd = %cwd,
            project = project_name,
        );
        let _enter = span.enter();
        let sandbox_prefix = resolve_sandbox_prefix(
            project_name,
            sandbox_profile,
            self.config.session_prefix.as_deref(),
        );
        let (sp, failures) = SessionPlugins::spawn(&self.config, cwd, sandbox_prefix.as_deref())?;
        self.session_plugins.insert(session_id.to_string(), sp);
        Ok(failures)
    }

    /// Destroy session plugins for a given session.
    pub fn destroy_session_plugins(&mut self, session_id: &str) {
        self.session_plugins.remove(session_id);
        self.initialized_sessions.remove(session_id);
    }

    /// Get all tool schemas (global + session).
    /// When `child_budget` is 0, session orchestration tools (session_*) are excluded.
    pub fn tool_schemas(&self, session_id: &str, child_budget: u32) -> Vec<Tool> {
        let mut schemas = Vec::new();
        if let Some(sp) = self.session_plugins.get(session_id) {
            schemas.extend(sp.tool_schemas());
        }
        // Use the cached global tool schemas so that tools are always
        // present even when a plugin handle is temporarily taken for
        // tool execution (see take_tool_plugin / return_tool_plugin).
        for (tool_schemas, _) in &self.global_tool_cache {
            schemas.extend(tool_schemas.iter().cloned());
        }
        if child_budget == 0 {
            schemas.retain(|t| !t.name.starts_with("session_"));
        }
        schemas
    }

    /// Get all tool prompt contributions (global + session).
    /// When `child_budget` is 0, session orchestration tools (session_*) are excluded.
    pub fn tool_prompts(
        &self,
        session_id: &str,
        child_budget: u32,
    ) -> Vec<crate::system_prompt::ToolPrompt> {
        let mut prompts = Vec::new();
        if let Some(sp) = self.session_plugins.get(session_id) {
            prompts.extend(sp.tool_prompts());
        }
        // Use the cached global tool prompts (same reason as tool_schemas).
        for (_, tool_prompts) in &self.global_tool_cache {
            prompts.extend(tool_prompts.iter().cloned());
        }
        if child_budget == 0 {
            prompts.retain(|t| !t.name.starts_with("session_"));
        }
        prompts
    }

    /// Execute a tool call: try session plugins first, then global.
    /// Runs after_tool_result hooks on all plugins afterward.
    pub fn execute_tool(
        &mut self,
        session_id: &str,
        tool_call: &ToolCall,
        cwd: &str,
        on_output: &mut dyn FnMut(&str),
    ) -> crate::Result<crate::types::ToolResultMessage> {
        // Try session plugins first
        if let Some(sp) = self.session_plugins.get_mut(session_id)
            && sp.has_tool(&tool_call.name)
        {
            let mut result = sp.execute_tool(tool_call, cwd, Some(session_id), on_output)?;
            self.run_after_tool_hooks(session_id, tool_call, &mut result);
            return Ok(result);
        }

        // Fall through to global plugins
        let plugin = find_tool_plugin(&mut self.global_plugins, &tool_call.name);
        match plugin {
            Some(p) => {
                let mut result =
                    p.execute_tool(tool_call, Some(cwd), Some(session_id), on_output)?;
                self.run_after_tool_hooks(session_id, tool_call, &mut result);
                Ok(result)
            }
            None => Err(crate::Error::Io(format!(
                "no plugin provides tool '{}'",
                tool_call.name
            ))),
        }
    }

    /// Execute a tool call with server request handler.
    /// Like execute_tool but passes a server request callback to the plugin handle.
    pub fn execute_tool_with_server(
        &mut self,
        session_id: &str,
        tool_call: &ToolCall,
        cwd: &str,
        on_output: &mut dyn FnMut(&str),
        on_server_request: Option<
            &mut dyn FnMut(&crate::protocol::Request) -> crate::protocol::Response,
        >,
        project_name: Option<&str>,
    ) -> crate::Result<crate::types::ToolResultMessage> {
        // Try session plugins first
        if let Some(sp) = self.session_plugins.get_mut(session_id)
            && sp.has_tool(&tool_call.name)
        {
            let plugin = find_tool_plugin(&mut sp.plugins, &tool_call.name);
            if let Some(p) = plugin {
                p.ensure_alive()?;
                let mut result = p.execute_tool_with_server(
                    tool_call,
                    Some(cwd),
                    Some(session_id),
                    on_output,
                    on_server_request,
                    project_name,
                )?;
                self.run_after_tool_hooks(session_id, tool_call, &mut result);
                return Ok(result);
            }
        }

        // Fall through to global plugins
        let plugin = find_tool_plugin(&mut self.global_plugins, &tool_call.name);
        match plugin {
            Some(p) => {
                let mut result = p.execute_tool_with_server(
                    tool_call,
                    Some(cwd),
                    Some(session_id),
                    on_output,
                    on_server_request,
                    project_name,
                )?;
                self.run_after_tool_hooks(session_id, tool_call, &mut result);
                Ok(result)
            }
            None => Err(crate::Error::Io(format!(
                "no plugin provides tool '{}'",
                tool_call.name
            ))),
        }
    }

    /// Take a plugin handle out of the manager for tool execution.
    /// Returns the handle and a source token for returning it.
    /// While taken, no other caller can use this specific plugin.
    /// Respawns the plugin if it has exited.
    pub fn take_tool_plugin(
        &mut self,
        session_id: &str,
        tool_name: &str,
    ) -> Option<(PluginHandle, PluginSource)> {
        // Try session plugins first
        if let Some(sp) = self.session_plugins.get_mut(session_id)
            && let Some(idx) = sp.plugins.iter().position(|p| p.has_tool(tool_name))
        {
            let handle = &mut sp.plugins[idx];
            if let Err(e) = handle.ensure_alive() {
                tracing::warn!(plugin = %handle.name, %e, "plugin respawn failed");
                return None;
            }
            let handle = sp.plugins.remove(idx);
            return Some((
                handle,
                PluginSource::Session {
                    session_id: session_id.to_string(),
                    index: idx,
                },
            ));
        }
        // Try global plugins
        if let Some(idx) = self
            .global_plugins
            .iter()
            .position(|p| p.has_tool(tool_name))
        {
            let handle = self.global_plugins.remove(idx);
            return Some((handle, PluginSource::Global { index: idx }));
        }
        None
    }

    /// Return a previously taken plugin handle.
    pub fn return_tool_plugin(&mut self, source: PluginSource, handle: PluginHandle) {
        match source {
            PluginSource::Session { session_id, index } => {
                if let Some(sp) = self.session_plugins.get_mut(&session_id) {
                    let idx = index.min(sp.plugins.len());
                    sp.plugins.insert(idx, handle);
                }
                // If session was destroyed while plugin was taken, handle drops
            }
            PluginSource::Global { index } => {
                let idx = index.min(self.global_plugins.len());
                self.global_plugins.insert(idx, handle);
            }
        }
    }

    /// Run after_tool_result hooks on all plugins (global + session).
    pub fn run_after_tool_hooks(
        &mut self,
        session_id: &str,
        tool_call: &ToolCall,
        result: &mut crate::types::ToolResultMessage,
    ) {
        let result_text: String = result
            .content
            .iter()
            .filter_map(|c| match c {
                crate::types::ToolResultContent::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");

        let hook_data = serde_json::json!({
            "tool_name": tool_call.name,
            "arguments": tool_call.arguments,
            "content": result_text,
            "is_error": result.is_error,
        });

        let mut hook_results =
            call_hook_all(&mut self.global_plugins, "after_tool_result", &hook_data);
        if let Some(sp) = self.session_plugins.get_mut(session_id) {
            hook_results.extend(sp.call_hook("after_tool_result", &hook_data));
        }

        for hook_result in hook_results {
            if let Some(append) = hook_result.tool_result_append
                && !append.is_empty()
            {
                result.content.push(crate::types::ToolResultContent::Text(
                    crate::types::TextContent {
                        text: append,
                        text_signature: None,
                    },
                ));
            }
        }
    }

    /// Call a hook on all plugins (global + session). Returns merged results.
    pub fn call_hook(
        &mut self,
        session_id: &str,
        name: &str,
        data: &serde_json::Value,
    ) -> Vec<HookResult> {
        let mut results = call_hook_all(&mut self.global_plugins, name, data);
        if let Some(sp) = self.session_plugins.get_mut(session_id) {
            results.extend(sp.call_hook(name, data));
        }
        results
    }

    /// Call a hook on all plugins except the one named `exclude_plugin`.
    /// Used by FireHook to avoid sending the hook back to the originating plugin.
    pub fn call_hook_excluding(
        &mut self,
        session_id: &str,
        name: &str,
        data: &serde_json::Value,
        exclude_plugin: Option<&str>,
    ) -> Vec<HookResult> {
        let mut results =
            call_hook_all_excluding(&mut self.global_plugins, name, data, exclude_plugin);
        if let Some(sp) = self.session_plugins.get_mut(session_id) {
            // Ensure plugins are alive before calling hooks
            for p in &mut sp.plugins {
                if p.wants_hook(name)
                    && exclude_plugin != Some(p.name.as_str())
                    && p.ensure_alive().is_err()
                {
                    tracing::warn!(plugin = %p.name, hook = name, "plugin respawn for hook failed");
                }
            }
            results.extend(call_hook_all_excluding(
                &mut sp.plugins,
                name,
                data,
                exclude_plugin,
            ));
        }
        results
    }

    /// Notify session start to all plugins (only once per session).
    pub fn notify_session_start_once(
        &mut self,
        cwd: &str,
        session_id: &str,
        project_name: Option<&str>,
    ) {
        if !self.initialized_sessions.insert(session_id.to_string()) {
            return; // already notified
        }
        let req = PluginRequest::SessionStart {
            cwd: cwd.to_string(),
            session_id: session_id.to_string(),
            project_name: project_name.map(String::from),
        };
        for plugin in &mut self.global_plugins {
            if plugin.wants_hook("session_start") {
                let span = tracing::info_span!(
                    "plugin.hook",
                    plugin = %plugin.name,
                    hook = "session_start",
                    session_id = %session_id,
                );
                let _enter = span.enter();
                tracing::debug!("sending");
                if let Err(e) = plugin.send(&req) {
                    tracing::warn!(%e, "failed to send session_start");
                    continue;
                }
                match plugin.read_message() {
                    Ok(_) => tracing::debug!("returned"),
                    Err(e) => tracing::warn!(%e, "session_start response failed"),
                }
            }
        }
        if let Some(sp) = self.session_plugins.get_mut(session_id) {
            for plugin in &mut sp.plugins {
                if plugin.wants_hook("session_start") {
                    let span = tracing::info_span!(
                        "plugin.hook",
                        plugin = %plugin.name,
                        hook = "session_start",
                        session_id = %session_id,
                    );
                    let _enter = span.enter();
                    tracing::debug!("sending");
                    if let Err(e) = plugin.send(&req) {
                        tracing::warn!(%e, "failed to send session_start");
                        continue;
                    }
                    match plugin.read_message() {
                        Ok(_) => tracing::debug!("returned"),
                        Err(e) => tracing::warn!(%e, "session_start response failed"),
                    }
                }
            }
        }
    }

    /// Get all slash commands from all plugins (global + all session).
    pub fn commands(&self) -> Vec<(String, String)> {
        let mut cmds: Vec<(String, String)> = self
            .global_plugins
            .iter()
            .flat_map(|p| {
                p.registration
                    .commands
                    .iter()
                    .map(|c| (c.name.clone(), c.description.clone()))
            })
            .collect();
        for sp in self.session_plugins.values() {
            for p in &sp.plugins {
                cmds.extend(
                    p.registration
                        .commands
                        .iter()
                        .map(|c| (c.name.clone(), c.description.clone())),
                );
            }
        }
        cmds
    }

    /// Kill all plugins.
    pub fn kill_all(&mut self) {
        let span = tracing::info_span!(
            "plugin.kill_all",
            global = self.global_plugins.len(),
            sessions = self.session_plugins.len(),
        );
        let _enter = span.enter();
        for p in &mut self.global_plugins {
            p.kill();
        }
        for sp in self.session_plugins.values_mut() {
            sp.kill_all();
        }
    }

    /// Send idle notifications to session plugins that have been inactive
    /// and have no active subscribers. Returns list of session IDs that were idled.
    pub fn idle_sweep(
        &mut self,
        idle_timeout: std::time::Duration,
        has_subscriber: &dyn Fn(&str) -> bool,
    ) -> Vec<String> {
        let now = Instant::now();
        let mut idled = Vec::new();
        for (session_id, sp) in &mut self.session_plugins {
            // Skip sessions with active subscribers
            if has_subscriber(session_id) {
                continue;
            }
            // Skip sessions with no alive plugins
            if !sp.any_alive() {
                continue;
            }
            // Check if idle long enough
            if let Some(last) = sp.last_activity()
                && now.duration_since(last) >= idle_timeout
            {
                tracing::info!(session_id = %session_id, "sending idle to session plugins");
                sp.send_idle_all();
                idled.push(session_id.clone());
            }
        }
        idled
    }

    /// Get the configured idle timeout.
    pub fn idle_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.config.idle_timeout_secs)
    }

    /// Get a clone of a global plugin's background write channel sender.
    ///
    /// Returns `None` if the plugin has no background channels installed or
    /// if no global plugin with the given name exists.
    pub fn get_global_write_tx(&self, name: &str) -> Option<smol::channel::Sender<PluginRequest>> {
        self.global_plugins
            .iter()
            .find(|p| p.name == name)
            .and_then(|p| p.bg_write_tx.clone())
    }
}

impl Drop for PluginManager {
    fn drop(&mut self) {
        self.kill_all();
    }
}

// ---------------------------------------------------------------------------
// Plugin config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginsConfig {
    /// Prefix prepended to all session plugin commands.
    /// Example: `["sandbox", "run", "--"]`
    #[serde(default)]
    pub session_prefix: Option<Vec<String>>,
    /// Global plugins (spawned once at server start).
    #[serde(default)]
    pub global: HashMap<String, PluginEntry>,
    /// Session plugins (spawned per session).
    #[serde(default)]
    pub session: HashMap<String, PluginEntry>,
    /// If true, don't spawn the default built-in worker when session is empty.
    /// Used in tests where the worker binary isn't available.
    #[serde(default)]
    pub no_default_worker: bool,
    /// Idle timeout in seconds for session plugins. After this duration of
    /// inactivity (no tool calls/hooks) with no connected subscribers,
    /// plugins receive an Idle notification and may exit.
    /// Default: 30 seconds. Set to 0 to disable.
    #[serde(default = "default_idle_timeout_secs")]
    pub idle_timeout_secs: u64,
}

fn default_idle_timeout_secs() -> u64 {
    30
}

impl Default for PluginsConfig {
    fn default() -> Self {
        Self {
            session_prefix: None,
            global: HashMap::new(),
            session: HashMap::new(),
            no_default_worker: false,
            idle_timeout_secs: default_idle_timeout_secs(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginEntry {
    /// Command to spawn the plugin (e.g. ["node", "/path/to/plugin.js"]).
    pub command: Vec<String>,
    /// Environment variables to set on the plugin subprocess.
    #[serde(default)]
    pub env: HashMap<String, String>,
}

/// Load plugins config from `~/.config/tau/plugins.toml`.
pub fn load_plugins_config() -> PluginsConfig {
    let path = crate::paths::config_dir().join("plugins.toml");

    if !path.exists() {
        return PluginsConfig::default();
    }

    std::fs::read_to_string(&path)
        .ok()
        .and_then(|content| toml::from_str(&content).ok())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// Guard that overrides XDG_CONFIG_HOME and restores it on drop.
    struct XdgGuard {
        prev_xdg: Option<String>,
    }

    impl Drop for XdgGuard {
        fn drop(&mut self) {
            match &self.prev_xdg {
                Some(v) => unsafe { std::env::set_var("XDG_CONFIG_HOME", v) },
                None => unsafe { std::env::remove_var("XDG_CONFIG_HOME") },
            }
        }
    }

    fn set_xdg(dir: &std::path::Path) -> XdgGuard {
        let guard = XdgGuard {
            prev_xdg: std::env::var("XDG_CONFIG_HOME").ok(),
        };
        unsafe { std::env::set_var("XDG_CONFIG_HOME", dir) };
        guard
    }

    // -- SandboxConfig deserialization tests --

    #[test]
    fn sandbox_config_deserialize_prefix_only() {
        let toml = r#"prefix = ["sandbox", "run", "--"]"#;
        let config: SandboxConfig = toml::from_str(toml).expect("parse failed");
        assert_eq!(
            config.prefix,
            Some(vec![
                "sandbox".to_string(),
                "run".to_string(),
                "--".to_string()
            ])
        );
        assert!(config.profiles.is_empty());
    }

    #[test]
    fn sandbox_config_deserialize_with_profiles() {
        let toml = r#"
prefix = ["sandbox", "run", "--profile", "rust-dev", "--"]

[profiles.strict]
prefix = ["sandbox", "run", "--profile", "strict", "--"]

[profiles.network-disabled]
prefix = ["sandbox", "run", "--no-network", "--"]
"#;
        let config: SandboxConfig = toml::from_str(toml).expect("parse failed");
        assert!(config.prefix.is_some());
        assert_eq!(config.profiles.len(), 2);
        assert_eq!(
            config.profiles["strict"].prefix,
            vec![
                "sandbox".to_string(),
                "run".to_string(),
                "--profile".to_string(),
                "strict".to_string(),
                "--".to_string()
            ]
        );
        assert_eq!(
            config.profiles["network-disabled"].prefix,
            vec![
                "sandbox".to_string(),
                "run".to_string(),
                "--no-network".to_string(),
                "--".to_string()
            ]
        );
    }

    #[test]
    fn sandbox_config_deserialize_empty() {
        let toml = "";
        let config: SandboxConfig = toml::from_str(toml).expect("parse failed");
        assert!(config.prefix.is_none());
        assert!(config.profiles.is_empty());
    }

    // -- resolve_sandbox_prefix tests --

    #[test]
    fn resolve_sandbox_prefix_no_config_returns_legacy() {
        let _g = crate::TEST_ENV_MUTEX
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let config_tmp = TempDir::new().expect("temp dir");
        let _xdg = set_xdg(config_tmp.path());
        // No sandbox.toml on disk

        let legacy = vec!["legacy-prefix".to_string(), "--".to_string()];
        let result = resolve_sandbox_prefix(None, None, Some(&legacy));
        assert_eq!(result, Some(legacy));
    }

    #[test]
    fn resolve_sandbox_prefix_no_config_no_legacy_returns_none() {
        let _g = crate::TEST_ENV_MUTEX
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let config_tmp = TempDir::new().expect("temp dir");
        let _xdg = set_xdg(config_tmp.path());

        let result = resolve_sandbox_prefix(None, None, None);
        assert!(result.is_none());
    }

    #[test]
    fn resolve_sandbox_prefix_global_sandbox_toml_overrides_legacy() {
        let _g = crate::TEST_ENV_MUTEX
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let config_tmp = TempDir::new().expect("temp dir");
        let _xdg = set_xdg(config_tmp.path());

        let global_dir = config_tmp.path().join("tau");
        fs::create_dir_all(&global_dir).expect("create dir");
        fs::write(
            global_dir.join("sandbox.toml"),
            r#"prefix = ["sandbox", "run", "--"]"#,
        )
        .expect("write");

        let legacy = vec!["old-prefix".to_string()];
        let result = resolve_sandbox_prefix(None, None, Some(&legacy));
        assert_eq!(
            result,
            Some(vec![
                "sandbox".to_string(),
                "run".to_string(),
                "--".to_string()
            ])
        );
    }

    #[test]
    fn resolve_sandbox_prefix_operator_overrides_global() {
        let _g = crate::TEST_ENV_MUTEX
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let config_tmp = TempDir::new().expect("temp dir");
        let _xdg = set_xdg(config_tmp.path());

        // Global sandbox.toml
        let global_dir = config_tmp.path().join("tau");
        fs::create_dir_all(&global_dir).expect("create dir");
        fs::write(
            global_dir.join("sandbox.toml"),
            r#"prefix = ["global-sandbox", "--"]"#,
        )
        .expect("write");

        // Operator sandbox.toml for project "myproj"
        let operator_dir = global_dir.join("projects").join("myproj");
        fs::create_dir_all(&operator_dir).expect("create dir");
        fs::write(
            operator_dir.join("sandbox.toml"),
            r#"prefix = ["operator-sandbox", "--"]"#,
        )
        .expect("write");

        let result = resolve_sandbox_prefix(Some("myproj"), None, None);
        assert_eq!(
            result,
            Some(vec!["operator-sandbox".to_string(), "--".to_string()])
        );
    }

    #[test]
    fn resolve_sandbox_prefix_named_profile() {
        let _g = crate::TEST_ENV_MUTEX
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let config_tmp = TempDir::new().expect("temp dir");
        let _xdg = set_xdg(config_tmp.path());

        let global_dir = config_tmp.path().join("tau");
        fs::create_dir_all(&global_dir).expect("create dir");
        fs::write(
            global_dir.join("sandbox.toml"),
            r#"
prefix = ["default-sandbox", "--"]

[profiles.strict]
prefix = ["strict-sandbox", "--"]
"#,
        )
        .expect("write");

        let result = resolve_sandbox_prefix(None, Some("strict"), None);
        assert_eq!(
            result,
            Some(vec!["strict-sandbox".to_string(), "--".to_string()])
        );
    }

    #[test]
    fn resolve_sandbox_prefix_missing_profile_falls_back_to_default() {
        let _g = crate::TEST_ENV_MUTEX
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let config_tmp = TempDir::new().expect("temp dir");
        let _xdg = set_xdg(config_tmp.path());

        let global_dir = config_tmp.path().join("tau");
        fs::create_dir_all(&global_dir).expect("create dir");
        fs::write(
            global_dir.join("sandbox.toml"),
            r#"prefix = ["default-sandbox", "--"]"#,
        )
        .expect("write");

        // Request a profile that doesn't exist
        let result = resolve_sandbox_prefix(None, Some("nonexistent"), None);
        assert_eq!(
            result,
            Some(vec!["default-sandbox".to_string(), "--".to_string()])
        );
    }

    #[test]
    fn resolve_sandbox_prefix_sandbox_toml_no_prefix_falls_to_legacy() {
        let _g = crate::TEST_ENV_MUTEX
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let config_tmp = TempDir::new().expect("temp dir");
        let _xdg = set_xdg(config_tmp.path());

        // sandbox.toml with profiles but no default prefix
        let global_dir = config_tmp.path().join("tau");
        fs::create_dir_all(&global_dir).expect("create dir");
        fs::write(
            global_dir.join("sandbox.toml"),
            r#"
[profiles.strict]
prefix = ["strict-sandbox", "--"]
"#,
        )
        .expect("write");

        let legacy = vec!["legacy".to_string()];
        let result = resolve_sandbox_prefix(None, None, Some(&legacy));
        // No default prefix in sandbox.toml, falls through to legacy
        assert_eq!(result, Some(vec!["legacy".to_string()]));
    }
}
