//! Built-in worker plugin for tool execution.
//!
//! Speaks the plugin protocol (JSON lines over stdin/stdout) and runs
//! tools concurrently using non-blocking I/O throughout.
//!
//! Architecture:
//!
//! ```text
//! stdin → reader task (demuxes ToolCall vs ServerResponse vs Hook/Idle)
//! tool calls → concurrent async tasks (one per tool call)
//! all outbound messages → writer task → stdout
//! ```
//!
//! Can be wrapped with sandbox or other execution environments:
//!   worker_command = ["sandbox", "run", "--", "tau", "worker"]
//!
//! Usage: `tau worker` (hidden subcommand, used by daemon).

use std::collections::{HashMap, HashSet};
use std::os::unix::io::{FromRawFd, IntoRawFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use futures::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use smol::channel::{Receiver, Sender};
use smol::lock::Mutex;

use crate::plugin::{
    HookResult, PluginMessage, PluginRegistration, PluginRequest, PluginToolDef, PluginToolResult,
};
use crate::types::*;

// Re-export ToolExecutor from the plugin SDK for backward compatibility
pub use tau_agent_plugin::ToolExecutor;

// Re-export InProcessWorker from the worker crate for backward compatibility
pub use tau_agent_plugin_worker::InProcessWorker;

// ---------------------------------------------------------------------------
// Request ID generation (safe for concurrent use)
// ---------------------------------------------------------------------------

static REQUEST_COUNTER: AtomicU64 = AtomicU64::new(0);

fn next_request_id() -> String {
    let n = REQUEST_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("sr-{}-{}", crate::types::timestamp_ms(), n)
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Run the worker. Called from `tau worker`.
pub fn run() {
    // The worker runs as a subprocess (plugin); its stderr is captured and
    // forwarded to the server's tracing layer, so `eprintln!` here still
    // surfaces in the server log file.

    // Install signal handlers: on SIGTERM/SIGHUP/SIGINT, kill any tracked
    // bash process groups and exit.  Without this, an orphaned `tau worker`
    // (e.g. its parent server died) would leave `sleep`-style children
    // running indefinitely.
    if let Err(e) = crate::shutdown::install(|sig| {
        eprintln!(
            "tau worker: received {}, killing tracked bash children",
            crate::shutdown::signal_name(sig),
        );
        tau_agent_plugin_worker::tools::bash::kill_all_tracked();
        // The worker has no other graceful state to flush — exit promptly.
        // Use the conventional 128 + signal-number exit code.
        let code = match sig {
            nix::sys::signal::Signal::SIGTERM => 143,
            nix::sys::signal::Signal::SIGHUP => 129,
            nix::sys::signal::Signal::SIGINT => 130,
            _ => 1,
        };
        std::process::exit(code);
    }) {
        eprintln!("tau worker: failed to install signal handlers: {}", e);
    }

    smol::block_on(async_main()).expect("worker failed");
}

async fn async_main() -> crate::Result<()> {
    // Wrap stdin/stdout in async buffered I/O.
    // Safety: fd 0 (stdin) and fd 1 (stdout) are valid file descriptors.
    let async_stdin = unsafe { smol::Async::new(std::fs::File::from_raw_fd(0)) }
        .map_err(|e| crate::Error::Io(format!("async wrap stdin: {}", e)))?;
    let async_stdout = unsafe { smol::Async::new(std::fs::File::from_raw_fd(1)) }
        .map_err(|e| crate::Error::Io(format!("async wrap stdout: {}", e)))?;

    let reader = BufReader::new(async_stdin);
    let mut writer = BufWriter::new(async_stdout);

    // -----------------------------------------------------------------------
    // Registration: send tool list synchronously before spawning tasks
    // -----------------------------------------------------------------------

    let mut all_tools = builtin_plugin_tools();
    all_tools.extend(crate::orchestration::orchestration_tools());
    let reg = PluginRegistration {
        name: "worker".to_string(),
        tools: all_tools,
        hooks: Vec::new(),
        commands: Vec::new(),
    };
    write_message(&mut writer, &PluginMessage::Register(reg)).await?;

    // -----------------------------------------------------------------------
    // Channels
    // -----------------------------------------------------------------------

    // Outbound message channel: all tasks send PluginMessages here; the
    // writer task serialises them onto stdout.
    let (msg_tx, msg_rx): (Sender<PluginMessage>, Receiver<PluginMessage>) =
        smol::channel::unbounded();

    // Pending server-request responses: maps request_id → oneshot sender.
    let pending_responses: Arc<Mutex<HashMap<String, Sender<crate::protocol::Response>>>> =
        Arc::new(Mutex::new(HashMap::new()));

    // Shared unjoined-children set (for session_join_all / session_join_any).
    let unjoined: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));

    // In-flight tool-call cancel tokens, keyed by tool_call_id. Inserted on
    // ToolCall arrival, flipped by a CancelToolCall request from the server,
    // removed when the tool completes. This is how mid-flight cancellation
    // crosses the RPC boundary: the plugin's main loop processes
    // CancelToolCall while the tool task is blocked in bash/read/write/edit.
    let in_flight_cancels: Arc<Mutex<HashMap<String, tau_agent_plugin::CancelToken>>> =
        Arc::new(Mutex::new(HashMap::new()));

    // -----------------------------------------------------------------------
    // Writer task: drains msg_rx → stdout
    // -----------------------------------------------------------------------

    let writer_handle = smol::spawn(async move {
        while let Ok(msg) = msg_rx.recv().await {
            write_message(&mut writer, &msg).await?;
        }
        Ok::<(), crate::Error>(())
    });

    // -----------------------------------------------------------------------
    // Reader task: reads stdin, routes messages
    // -----------------------------------------------------------------------

    let reader_msg_tx = msg_tx.clone();
    let reader_pending = pending_responses.clone();
    let reader_unjoined = unjoined.clone();
    let reader_in_flight = in_flight_cancels.clone();

    let reader_handle = smol::spawn(async move {
        let mut reader = reader;
        let mut line = String::new();

        loop {
            line.clear();
            let n = reader
                .read_line(&mut line)
                .await
                .map_err(|e| crate::Error::Io(format!("stdin read: {}", e)))?;
            if n == 0 {
                break; // EOF
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
                    project_name,
                    ..
                } => {
                    // Spawn a concurrent task for each tool call.
                    let msg_tx = reader_msg_tx.clone();
                    let pending = reader_pending.clone();
                    let unjoined = reader_unjoined.clone();
                    let in_flight = reader_in_flight.clone();

                    // Register a cancel token for this tool call.
                    let cancel = tau_agent_plugin::CancelToken::new();
                    in_flight
                        .lock()
                        .await
                        .insert(tool_call_id.clone(), cancel.clone());

                    smol::spawn(async move {
                        let cwd = cwd.unwrap_or_else(|| {
                            std::env::current_dir()
                                .unwrap_or_default()
                                .to_string_lossy()
                                .to_string()
                        });

                        let result = if name.starts_with("session_") {
                            handle_session_tool(
                                &name,
                                &arguments,
                                session_id.as_deref(),
                                &msg_tx,
                                &pending,
                                &unjoined,
                            )
                            .await
                        } else if name == "bash" {
                            execute_bash_async(
                                &tool_call_id,
                                &arguments,
                                &cwd,
                                project_name.as_deref(),
                                &msg_tx,
                                &pending,
                                &cancel,
                            )
                            .await
                        } else {
                            // read, write, edit — run blocking tool on thread pool
                            let tools = crate::tools::default_tools();
                            let tc = ToolCall {
                                id: tool_call_id.clone(),
                                name: name.clone(),
                                arguments,
                            };
                            let cancel_for_blocking = cancel.clone();
                            smol::unblock(move || {
                                crate::tools::execute_tool(&tools, &tc, &cwd, &cancel_for_blocking)
                            })
                            .await
                        };

                        // Deregister the cancel token — tool is done.
                        in_flight.lock().await.remove(&tool_call_id);

                        let _ = msg_tx
                            .send(PluginMessage::ToolResult(PluginToolResult {
                                tool_call_id,
                                content: result.content,
                                is_error: result.is_error,
                                summary: result.summary,
                                post_persist_actions: Vec::new(),
                            }))
                            .await;
                    })
                    .detach();
                }

                PluginRequest::CancelToolCall { tool_call_id } => {
                    // Flip the cancel token for the in-flight tool call, if any.
                    // The tool's own cancel watcher (bash) will notice on its
                    // next poll and SIGKILL the subprocess, which unblocks the
                    // tool task and lets it return a cancelled ToolResult.
                    //
                    // If the tool_call_id is unknown (already completed or
                    // never existed), this is a no-op — race-safe.
                    let map = reader_in_flight.lock().await;
                    if let Some(token) = map.get(&tool_call_id) {
                        token.cancel();
                    }
                }

                PluginRequest::ServerResponse {
                    request_id,
                    response,
                } => {
                    let mut pending = reader_pending.lock().await;
                    if let Some(sender) = pending.remove(&request_id) {
                        let _ = sender.send(response).await;
                    }
                }

                PluginRequest::Hook { .. } => {
                    let _ = reader_msg_tx
                        .send(PluginMessage::HookResult(HookResult::default()))
                        .await;
                }

                PluginRequest::Init { .. } | PluginRequest::SessionStart { .. } => {
                    let _ = reader_msg_tx
                        .send(PluginMessage::HookResult(HookResult::default()))
                        .await;
                }

                PluginRequest::Idle => {
                    break; // exit
                }
            }
        }

        Ok::<(), crate::Error>(())
    });

    // -----------------------------------------------------------------------
    // Wait for the reader to finish (EOF or Idle), then shut down.
    // -----------------------------------------------------------------------

    // The reader task drives the lifecycle. When it exits, close the msg
    // channel so the writer drains and exits too.
    let reader_result = reader_handle.await;

    // Close outbound channel — writer will drain remaining messages and stop.
    drop(msg_tx);

    let writer_result = writer_handle.await;

    reader_result?;
    writer_result?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Plugin tool definitions
// ---------------------------------------------------------------------------

fn builtin_plugin_tools() -> Vec<PluginToolDef> {
    let tools = crate::tools::default_tools();
    let prompts = crate::system_prompt::default_tool_prompts();

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
// Async message helpers
// ---------------------------------------------------------------------------

async fn write_message(
    writer: &mut (impl AsyncWriteExt + Unpin),
    msg: &PluginMessage,
) -> crate::Result<()> {
    crate::write_json_line_async(writer, msg)
        .await
        .map_err(|e| crate::Error::Io(format!("write: {}", e)))
}

// ---------------------------------------------------------------------------
// Server request tunnel (async)
// ---------------------------------------------------------------------------

/// Send a `Request` to the tau server via the plugin protocol tunnel and
/// wait for the corresponding `Response`. Multiple concurrent calls are
/// safe — each gets a unique request_id and its own oneshot channel.
async fn server_request(
    msg_tx: &Sender<PluginMessage>,
    pending: &Arc<Mutex<HashMap<String, Sender<crate::protocol::Response>>>>,
    request: crate::protocol::Request,
) -> Result<crate::protocol::Response, String> {
    let request_id = next_request_id();

    // Register a oneshot channel for the response.
    let (resp_tx, resp_rx) = smol::channel::bounded(1);
    pending.lock().await.insert(request_id.clone(), resp_tx);

    // Send the request.
    msg_tx
        .send(PluginMessage::ServerRequest {
            request_id: request_id.clone(),
            request,
        })
        .await
        .map_err(|e| format!("send failed: {}", e))?;

    // Wait for the response.
    resp_rx
        .recv()
        .await
        .map_err(|e| format!("recv failed: {}", e))
}

// ---------------------------------------------------------------------------
// Async bash execution
// ---------------------------------------------------------------------------

async fn execute_bash_async(
    tool_call_id: &str,
    args: &serde_json::Value,
    cwd: &str,
    project_name: Option<&str>,
    msg_tx: &Sender<PluginMessage>,
    pending: &Arc<Mutex<HashMap<String, Sender<crate::protocol::Response>>>>,
    cancel: &tau_agent_plugin::CancelToken,
) -> ToolResultMessage {
    let Some(command) = args.get("command").and_then(|v| v.as_str()) else {
        return ToolResultMessage::error(tool_call_id, "", "missing 'command' argument");
    };
    let timeout_secs = args.get("timeout").and_then(|v| v.as_u64()).unwrap_or(120);

    // If the session's `cwd` has been removed (e.g. a worktree was
    // cleaned up under a still-running session), `Command::spawn`
    // returns a bare `ENOENT` that's indistinguishable from "bash not
    // found". Disambiguate up front and — when possible — fall back to
    // the project root with a status note so the LLM can adjust
    // subsequent commands without spinning on the same broken `cwd`.
    let (effective_cwd, fallback_note) =
        match resolve_bash_cwd(cwd, project_name, msg_tx, pending).await {
            BashCwdResolution::Use(c) => (c, None),
            BashCwdResolution::Fallback { cwd: new_cwd, note } => (new_cwd, Some(note)),
            BashCwdResolution::HardError(msg) => {
                return ToolResultMessage::error(tool_call_id, "", &msg);
            }
        };
    let cwd: &str = &effective_cwd;

    // Spawn child with setsid for process-group kill.
    let child = {
        use std::os::unix::process::CommandExt;
        unsafe {
            std::process::Command::new("bash")
                .arg("-c")
                .arg(command)
                .current_dir(cwd)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .env("SUDO_ASKPASS", "/bin/false")
                .env("GIT_TERMINAL_PROMPT", "0")
                .pre_exec(|| {
                    nix::unistd::setsid().map_err(std::io::Error::other)?;
                    Ok(())
                })
                .spawn()
        }
    };

    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            return ToolResultMessage::error(
                tool_call_id,
                "",
                &format!("failed to execute command: {}", e),
            );
        }
    };

    let child_id = child.id();
    let pgid = child_id as i32;
    tau_agent_plugin_worker::tools::bash::track_pgid(pgid);

    // Extract stdout/stderr and wrap in async readers.
    let stdout = child
        .stdout
        .take()
        .expect("stdout configured with piped output");
    let stderr = child
        .stderr
        .take()
        .expect("stderr configured with piped output");

    let async_stdout =
        unsafe { smol::Async::new(std::fs::File::from_raw_fd(stdout.into_raw_fd())) };
    let async_stderr =
        unsafe { smol::Async::new(std::fs::File::from_raw_fd(stderr.into_raw_fd())) };

    let (async_stdout, async_stderr) = match (async_stdout, async_stderr) {
        (Ok(o), Ok(e)) => (o, e),
        _ => return ToolResultMessage::error(tool_call_id, "", "failed to create async pipes"),
    };

    let mut stdout_reader = BufReader::new(async_stdout);
    let mut stderr_reader = BufReader::new(async_stderr);

    // Spawn a timeout killer task.
    let timed_out = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let timed_out_for_killer = timed_out.clone();
    let killer = smol::spawn(async move {
        smol::Timer::after(std::time::Duration::from_secs(timeout_secs)).await;
        timed_out_for_killer.store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(child_id as i32),
            nix::sys::signal::Signal::SIGKILL,
        );
        // Drop the registry entry as well — the wait() path will also
        // untrack, but if the worker is racing shutdown the timeout path
        // is the last guaranteed point at which we know the PGID is
        // (about to be) dead.
        tau_agent_plugin_worker::tools::bash::untrack_pgid(child_id as i32);
    });

    // Spawn a cancel-watcher task: on cancellation, SIGKILL the process
    // group. Polls every 100 ms and exits when the `cancel_done` flag
    // is flipped by the caller after the child has exited naturally.
    let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let cancel_done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let cancelled_for_watcher = cancelled.clone();
    let cancel_done_for_watcher = cancel_done.clone();
    let cancel_for_watcher = cancel.clone();
    let cancel_watcher = smol::spawn(async move {
        loop {
            if cancel_done_for_watcher.load(std::sync::atomic::Ordering::Relaxed) {
                return;
            }
            if cancel_for_watcher.is_cancelled() {
                cancelled_for_watcher.store(true, std::sync::atomic::Ordering::Relaxed);
                let _ = nix::sys::signal::killpg(
                    nix::unistd::Pid::from_raw(child_id as i32),
                    nix::sys::signal::Signal::SIGKILL,
                );
                tau_agent_plugin_worker::tools::bash::untrack_pgid(child_id as i32);
                return;
            }
            smol::Timer::after(std::time::Duration::from_millis(100)).await;
        }
    });

    // Read stdout (with streaming deltas) and stderr concurrently.
    let tcid = tool_call_id.to_string();
    let msg_tx_clone = msg_tx.clone();

    let (collected_stdout, collected_stderr) = futures::future::join(
        async {
            let mut collected = String::new();
            let mut line = String::new();
            loop {
                line.clear();
                match stdout_reader.read_line(&mut line).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        // Stream delta to the host.
                        let _ = msg_tx_clone
                            .send(PluginMessage::OutputDelta {
                                tool_call_id: tcid.clone(),
                                text: line.clone(),
                            })
                            .await;
                        collected.push_str(&line);
                    }
                }
            }
            collected
        },
        async {
            let mut collected = String::new();
            let mut line = String::new();
            loop {
                line.clear();
                match stderr_reader.read_line(&mut line).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => collected.push_str(&line),
                }
            }
            collected
        },
    )
    .await;

    // Wait for child to exit (blocking, so offload to thread pool).
    let exit_code =
        smol::unblock(move || child.wait().ok().and_then(|s| s.code()).unwrap_or(-1)).await;

    // Cancel the killer (child already exited).
    killer.cancel().await;
    cancel_done.store(true, std::sync::atomic::Ordering::Relaxed);
    cancel_watcher.cancel().await;

    tau_agent_plugin_worker::tools::bash::untrack_pgid(pgid);

    // Format output.
    let mut output = format_bash_output(
        tool_call_id,
        collected_stdout,
        collected_stderr,
        exit_code,
        timed_out.load(std::sync::atomic::Ordering::Relaxed),
        cancelled.load(std::sync::atomic::Ordering::Relaxed),
    );

    // If we fell back to the project root because the session's `cwd`
    // had been removed, prepend a note so the LLM sees the substitution.
    // Important: we do *not* flip `is_error` to true — the command itself
    // ran in a sane directory.
    if let Some(note) = fallback_note {
        prepend_note(&mut output, &note);
    }
    output
}

/// Result of pre-flighting a bash invocation's `cwd`.
enum BashCwdResolution {
    /// Use the (possibly already-existing) cwd as-is.
    Use(String),
    /// The original cwd was missing/unusable but we have a usable
    /// fallback (the project root). The bash command runs in `cwd` and
    /// `note` is prepended to the tool output.
    Fallback { cwd: String, note: String },
    /// The original cwd is unusable and no fallback is available.
    /// Return this string as the tool error.
    HardError(String),
}

/// Decide what `cwd` the bash invocation should actually use.
///
/// 1. If the session's `cwd` exists and is a directory, use it.
/// 2. If it's missing/non-directory and we have a `project_name` whose
///    root path exists, use the project root and emit a substitution
///    note (Option B in task 720's spec).
/// 3. Otherwise return a hard error message naming the missing cwd
///    (Option A).
async fn resolve_bash_cwd(
    cwd: &str,
    project_name: Option<&str>,
    msg_tx: &Sender<PluginMessage>,
    pending: &Arc<Mutex<HashMap<String, Sender<crate::protocol::Response>>>>,
) -> BashCwdResolution {
    let cwd_state = classify_cwd(cwd);
    if matches!(cwd_state, CwdState::Ok) {
        return BashCwdResolution::Use(cwd.to_string());
    }

    // The cwd is unusable. Try to recover via the project root.
    if let Some(name) = project_name
        && let Some(root) = lookup_project_root(name, msg_tx, pending).await
    {
        // Check the project root is itself usable.
        if matches!(classify_cwd(&root), CwdState::Ok) {
            let note = format!(
                "note: session cwd {} {}; ran in project root {} instead.",
                cwd,
                cwd_state.short_reason(),
                root,
            );
            return BashCwdResolution::Fallback { cwd: root, note };
        }
    }

    // No fallback available — return the same hard-error phrasing the
    // worker plugin uses, so users see a consistent message regardless
    // of which spawn path they hit.
    BashCwdResolution::HardError(cwd_state.error_message(cwd))
}

/// Classification of a candidate `cwd` path.
enum CwdState {
    /// Path exists and is a directory.
    Ok,
    /// Path does not exist.
    Missing,
    /// Path exists but isn't a directory.
    NotADirectory,
    /// stat() failed for some other reason (e.g. EACCES). The error
    /// kind is preserved so we can mention it in the message.
    Inaccessible(String),
}

impl CwdState {
    fn short_reason(&self) -> &'static str {
        match self {
            CwdState::Ok => "is usable",
            CwdState::Missing => "no longer exists",
            CwdState::NotADirectory => "is not a directory",
            CwdState::Inaccessible(_) => "is not accessible",
        }
    }

    fn error_message(&self, cwd: &str) -> String {
        match self {
            CwdState::Ok => format!("cwd {} is usable", cwd),
            CwdState::Missing => format!(
                "session cwd no longer exists: {} (was the worktree removed? use /cd <path> to switch to a valid directory)",
                cwd,
            ),
            CwdState::NotADirectory => format!(
                "session cwd is not a directory: {} (use /cd <path> to switch to a valid directory)",
                cwd,
            ),
            CwdState::Inaccessible(detail) => {
                format!("cannot access session cwd {}: {}", cwd, detail,)
            }
        }
    }
}

fn classify_cwd(cwd: &str) -> CwdState {
    match std::fs::metadata(std::path::Path::new(cwd)) {
        Ok(m) if m.is_dir() => CwdState::Ok,
        Ok(_) => CwdState::NotADirectory,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => CwdState::Missing,
        Err(e) => CwdState::Inaccessible(e.to_string()),
    }
}

/// Look up a project's root path via the server tunnel. Returns `None`
/// when the project is unknown or the lookup fails (we treat any error
/// as "no fallback available" rather than surfacing it).
async fn lookup_project_root(
    project_name: &str,
    msg_tx: &Sender<PluginMessage>,
    pending: &Arc<Mutex<HashMap<String, Sender<crate::protocol::Response>>>>,
) -> Option<String> {
    let req = crate::protocol::Request::GetProjectInfo {
        project_name: project_name.to_string(),
    };
    match server_request(msg_tx, pending, req).await {
        Ok(crate::protocol::Response::ProjectInfo { project: Some(p) }) => Some(p.path),
        _ => None,
    }
}

/// Prepend a one-shot status note to a bash tool result. Used when we
/// fall back from a missing session cwd to the project root — the LLM
/// needs to see the substitution so subsequent commands don't keep
/// trying the dead path.
fn prepend_note(output: &mut ToolResultMessage, note: &str) {
    for content in output.content.iter_mut() {
        if let crate::types::ToolResultContent::Text(t) = content {
            t.text = if t.text.is_empty() {
                note.to_string()
            } else {
                format!("{}\n\n{}", note, t.text)
            };
            return;
        }
    }
    // No text part yet — add one so the note is still visible.
    output.content.push(crate::types::ToolResultContent::Text(
        crate::types::TextContent {
            text: note.to_string(),
            text_signature: None,
        },
    ));
}

/// Format bash output into a `ToolResultMessage`, applying truncation for
/// very long output. Mirrors `tools::bash::format_output`.
fn format_bash_output(
    tool_call_id: &str,
    stdout: String,
    stderr: String,
    exit_code: i32,
    timed_out: bool,
    cancelled: bool,
) -> ToolResultMessage {
    let mut text = stdout;
    if !stderr.is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str("STDERR:\n");
        text.push_str(&stderr);
    }

    // Truncate very long output.
    if text.len() > 100_000 {
        let head = &text[..50_000];
        let tail = &text[text.len() - 50_000..];
        text = format!(
            "{}\n\n... [truncated {} bytes] ...\n\n{}",
            head,
            text.len() - 100_000,
            tail
        );
    }

    // Cancellation takes priority over timeout in the output marker.
    if cancelled {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str("(cancelled)");
        return ToolResultMessage::error(tool_call_id, "", text.trim_end());
    }

    if timed_out {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str("(timed out)");
        return ToolResultMessage::error(tool_call_id, "", text.trim_end());
    }

    let success = exit_code == 0;
    if text.is_empty() {
        text = format!("(exit code: {})", exit_code);
    } else if !success {
        text.push_str(&format!("\n(exit code: {})", exit_code));
    }

    let text = text.trim_end().to_string();
    if success {
        ToolResultMessage::success(tool_call_id, "", &text)
    } else {
        ToolResultMessage::error(tool_call_id, "", &text)
    }
}

// ---------------------------------------------------------------------------
// Session orchestration tools (async)
// ---------------------------------------------------------------------------

async fn handle_session_tool(
    name: &str,
    args: &serde_json::Value,
    session_id: Option<&str>,
    msg_tx: &Sender<PluginMessage>,
    pending: &Arc<Mutex<HashMap<String, Sender<crate::protocol::Response>>>>,
    unjoined: &Arc<Mutex<HashSet<String>>>,
) -> ToolResultMessage {
    // We use an empty tool_call_id for session tool results since the
    // dispatch loop overwrites it from the PluginRequest anyway.
    let tcid = "";

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
                .unwrap_or(16) as u32;
            let tagline = args
                .get("tagline")
                .and_then(|v| v.as_str())
                .map(String::from);

            // Create session.
            let create_req = crate::protocol::Request::CreateSession {
                model,
                provider: None,
                system_prompt,
                cwd,
                parent_id: session_id.map(String::from),
                child_budget,
                tagline,
                auto_archive: args
                    .get("auto_archive")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                notify_parent: args
                    .get("notify_parent")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true),
                project_name: None,
                sandbox_profile: None,
            };
            let resp = match server_request(msg_tx, pending, create_req).await {
                Ok(r) => r,
                Err(e) => {
                    return ToolResultMessage::error(
                        tcid,
                        "",
                        &format!("server request failed: {}", e),
                    );
                }
            };
            let child_id = match resp {
                crate::protocol::Response::SessionCreated { session_id } => session_id,
                crate::protocol::Response::Error { message } => {
                    return ToolResultMessage::error(
                        tcid,
                        "",
                        &format!("spawn failed: {}", message),
                    );
                }
                other => {
                    return ToolResultMessage::error(
                        tcid,
                        "",
                        &format!("unexpected response: {:?}", other),
                    );
                }
            };

            // Send initial message.
            if !task.is_empty() {
                let chat_req = crate::protocol::Request::Chat {
                    session_id: child_id.clone(),
                    text: task.to_string(),
                };
                match server_request(msg_tx, pending, chat_req).await {
                    Ok(crate::protocol::Response::Ok) => {}
                    Ok(crate::protocol::Response::Error { message }) => {
                        return ToolResultMessage::error(
                            tcid,
                            "",
                            format!("session {} created but chat failed: {}", child_id, message),
                        );
                    }
                    Ok(other) => {
                        return ToolResultMessage::error(
                            tcid,
                            "",
                            format!(
                                "session {} created but unexpected chat response: {:?}",
                                child_id, other
                            ),
                        );
                    }
                    Err(e) => {
                        return ToolResultMessage::error(
                            tcid,
                            "",
                            format!("session {} created but chat failed: {}", child_id, e),
                        );
                    }
                }
            }

            // Track as unjoined.
            unjoined.lock().await.insert(child_id.clone());

            ToolResultMessage::success(tcid, "", &format!("Spawned session {}", child_id))
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
                return ToolResultMessage::error(tcid, "", "session_ids is required");
            }

            // Remove joined IDs from unjoined set.
            {
                let mut uj = unjoined.lock().await;
                for sid in &session_ids {
                    uj.remove(sid);
                }
            }

            let req = crate::protocol::Request::WaitSessions {
                session_ids,
                timeout_secs: timeout,
            };
            match server_request(msg_tx, pending, req).await {
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
                    let mut result = ToolResultMessage::success(tcid, "", text.trim_end());
                    result.summary = Some(format!(
                        "session_join: {} sessions completed",
                        results.len()
                    ));
                    result
                }
                Ok(crate::protocol::Response::Error { message }) => {
                    ToolResultMessage::error(tcid, "", &message)
                }
                Ok(other) => {
                    ToolResultMessage::error(tcid, "", &format!("unexpected response: {:?}", other))
                }
                Err(e) => ToolResultMessage::error(tcid, "", &e),
            }
        }

        "session_join_all" => {
            let timeout = args.get("timeout").and_then(|v| v.as_u64()).unwrap_or(300);

            let session_ids: Vec<String> = {
                let mut uj = unjoined.lock().await;
                if uj.is_empty() {
                    return ToolResultMessage::success(tcid, "", "No unjoined child sessions.");
                }
                uj.drain().collect()
            };

            let req = crate::protocol::Request::WaitSessions {
                session_ids,
                timeout_secs: timeout,
            };
            match server_request(msg_tx, pending, req).await {
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
                    let mut result = ToolResultMessage::success(tcid, "", text.trim_end());
                    result.summary = Some(format!(
                        "session_join_all: {} sessions completed",
                        results.len()
                    ));
                    result
                }
                Ok(crate::protocol::Response::Error { message }) => {
                    ToolResultMessage::error(tcid, "", &message)
                }
                Ok(other) => {
                    ToolResultMessage::error(tcid, "", &format!("unexpected response: {:?}", other))
                }
                Err(e) => ToolResultMessage::error(tcid, "", &e),
            }
        }

        "session_join_any" => {
            let timeout = args.get("timeout").and_then(|v| v.as_u64()).unwrap_or(300);

            let session_ids: Vec<String> = {
                let uj = unjoined.lock().await;
                if uj.is_empty() {
                    return ToolResultMessage::success(tcid, "", "No unjoined child sessions.");
                }
                uj.iter().cloned().collect()
            };

            let req = crate::protocol::Request::WaitAnySessions {
                session_ids,
                timeout_secs: timeout,
            };
            match server_request(msg_tx, pending, req).await {
                Ok(crate::protocol::Response::SessionsCompleted { results }) => {
                    // Remove completed sessions from unjoined set.
                    {
                        let mut uj = unjoined.lock().await;
                        for r in &results {
                            uj.remove(&r.session_id);
                        }
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
                    let mut result = ToolResultMessage::success(tcid, "", text.trim_end());
                    result.summary = Some(format!(
                        "session_join_any: {} sessions completed",
                        results.len()
                    ));
                    result
                }
                Ok(crate::protocol::Response::Error { message }) => {
                    ToolResultMessage::error(tcid, "", &message)
                }
                Ok(other) => {
                    ToolResultMessage::error(tcid, "", &format!("unexpected response: {:?}", other))
                }
                Err(e) => ToolResultMessage::error(tcid, "", &e),
            }
        }

        "session_status" => {
            let sid = args
                .get("session_id")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if sid.is_empty() {
                return ToolResultMessage::error(tcid, "", "session_id is required");
            }
            let req = crate::protocol::Request::GetSessionInfo {
                session_id: sid.to_string(),
            };
            match server_request(msg_tx, pending, req).await {
                Ok(crate::protocol::Response::SessionInfo { info }) => {
                    let status = if info.is_live {
                        format!("LIVE ({})", info.state)
                    } else if let Some(ref exit) = info.last_exit_status {
                        format!("idle (last: {})", exit)
                    } else {
                        "idle".to_string()
                    };
                    ToolResultMessage::success(
                        tcid,
                        "",
                        format!(
                            "Session {}: {} — {}/{}, {} messages, {} children",
                            info.id,
                            status,
                            info.provider,
                            info.model,
                            info.message_count,
                            info.child_count
                        ),
                    )
                }
                Ok(crate::protocol::Response::Error { message }) => {
                    ToolResultMessage::error(tcid, "", &message)
                }
                Ok(other) => {
                    ToolResultMessage::error(tcid, "", &format!("unexpected response: {:?}", other))
                }
                Err(e) => ToolResultMessage::error(tcid, "", &e),
            }
        }

        "session_list_children" => {
            let parent = session_id.unwrap_or("");
            if parent.is_empty() {
                return ToolResultMessage::error(tcid, "", "no session context available");
            }
            let req = crate::protocol::Request::ListSessions {
                include_archived: false,
                project_name: None,
            };
            match server_request(msg_tx, pending, req).await {
                Ok(crate::protocol::Response::Sessions { sessions }) => {
                    let children: Vec<_> = sessions
                        .iter()
                        .filter(|s| s.parent_id.as_deref() == Some(parent))
                        .collect();
                    if children.is_empty() {
                        let mut result = ToolResultMessage::success(tcid, "", "No child sessions");
                        result.summary = Some("session_list_children: 0 sessions".to_string());
                        result
                    } else {
                        let mut text = String::new();
                        for c in &children {
                            text.push_str(&format!(
                                "{}\t{}/{}\t{} msgs\n",
                                c.id, c.provider, c.model, c.message_count
                            ));
                        }
                        let mut result = ToolResultMessage::success(tcid, "", text.trim_end());
                        result.summary = Some(format!(
                            "session_list_children: {} sessions",
                            children.len()
                        ));
                        result
                    }
                }
                Ok(crate::protocol::Response::Error { message }) => {
                    ToolResultMessage::error(tcid, "", &message)
                }
                Ok(other) => {
                    ToolResultMessage::error(tcid, "", &format!("unexpected response: {:?}", other))
                }
                Err(e) => ToolResultMessage::error(tcid, "", &e),
            }
        }

        "session_read" => {
            let sid = args
                .get("session_id")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let last_n = args.get("last_n").and_then(|v| v.as_u64());
            if sid.is_empty() {
                return ToolResultMessage::error(tcid, "", "session_id is required");
            }
            let req = crate::protocol::Request::GetMessages {
                session_id: sid.to_string(),
            };
            match server_request(msg_tx, pending, req).await {
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
                            crate::types::Message::Info(i) => {
                                text.push_str(&format!("[info] {}\n", i.text));
                            }
                            _ => {}
                        }
                    }
                    let msg_count = msgs.len();
                    if text.is_empty() {
                        let mut result = ToolResultMessage::success(tcid, "", "(no messages)");
                        result.summary = Some(format!("session_read: {} (0 messages)", sid));
                        result
                    } else {
                        let mut result = ToolResultMessage::success(tcid, "", text.trim_end());
                        result.summary =
                            Some(format!("session_read: {} ({} messages)", sid, msg_count));
                        result
                    }
                }
                Ok(crate::protocol::Response::Error { message }) => {
                    ToolResultMessage::error(tcid, "", &message)
                }
                Ok(other) => {
                    ToolResultMessage::error(tcid, "", &format!("unexpected response: {:?}", other))
                }
                Err(e) => ToolResultMessage::error(tcid, "", &e),
            }
        }

        "session_cancel" => {
            let sid = args
                .get("session_id")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if sid.is_empty() {
                return ToolResultMessage::error(tcid, "", "session_id is required");
            }
            let req = crate::protocol::Request::CancelChat {
                session_id: sid.to_string(),
                caller_session_id: session_id.map(String::from),
            };
            match server_request(msg_tx, pending, req).await {
                Ok(crate::protocol::Response::Ok) => {
                    ToolResultMessage::success(tcid, "", &format!("Cancelled session {}", sid))
                }
                Ok(crate::protocol::Response::Error { message }) => {
                    ToolResultMessage::error(tcid, "", &message)
                }
                Ok(other) => {
                    ToolResultMessage::error(tcid, "", &format!("unexpected response: {:?}", other))
                }
                Err(e) => ToolResultMessage::error(tcid, "", &e),
            }
        }

        "session_archive" => {
            let sids: Vec<String> = match args.get("session_id") {
                Some(serde_json::Value::String(s)) if !s.is_empty() => vec![s.clone()],
                Some(serde_json::Value::Array(arr)) => arr
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect(),
                _ => {
                    return ToolResultMessage::error(
                        tcid,
                        "",
                        "session_id is required (string or array of strings)",
                    );
                }
            };
            if sids.is_empty() {
                return ToolResultMessage::error(
                    tcid,
                    "",
                    "session_id is required (string or array of strings)",
                );
            }
            let mut archived = Vec::new();
            let mut errors = Vec::new();
            for sid in &sids {
                let req = crate::protocol::Request::ArchiveSession {
                    session_id: sid.clone(),
                    require_ancestor: session_id.map(|s| s.to_string()),
                };
                match server_request(msg_tx, pending, req).await {
                    Ok(crate::protocol::Response::SessionArchived) => archived.push(sid.as_str()),
                    Ok(crate::protocol::Response::Error { message }) => {
                        errors.push(format!("{}: {}", sid, message));
                    }
                    Ok(other) => {
                        errors.push(format!("{}: unexpected response: {:?}", sid, other));
                    }
                    Err(e) => {
                        errors.push(format!("{}: {}", sid, e));
                    }
                }
            }
            if errors.is_empty() {
                if archived.len() == 1 {
                    ToolResultMessage::success(
                        tcid,
                        "",
                        &format!("Archived session {}", archived[0]),
                    )
                } else {
                    ToolResultMessage::success(
                        tcid,
                        "",
                        &format!("Archived {} sessions", archived.len()),
                    )
                }
            } else if archived.is_empty() {
                ToolResultMessage::error(tcid, "", &errors.join("; "))
            } else {
                ToolResultMessage::success(
                    tcid,
                    "",
                    format!(
                        "Archived {} session(s); {} failed: {}",
                        archived.len(),
                        errors.len(),
                        errors.join("; ")
                    ),
                )
            }
        }

        "session_restore" => {
            let sid = match args.get("session_id").and_then(|v| v.as_str()) {
                Some(s) if !s.is_empty() => s.to_string(),
                _ => return ToolResultMessage::error(tcid, "", "session_id is required"),
            };
            let req = crate::protocol::Request::RestoreSession {
                session_id: sid.clone(),
            };
            match server_request(msg_tx, pending, req).await {
                Ok(crate::protocol::Response::SessionRestored) => {
                    ToolResultMessage::success(tcid, "", &format!("Restored session {}", sid))
                }
                Ok(crate::protocol::Response::Error { message }) => {
                    ToolResultMessage::error(tcid, "", &message)
                }
                Ok(other) => {
                    ToolResultMessage::error(tcid, "", &format!("unexpected response: {:?}", other))
                }
                Err(e) => ToolResultMessage::error(tcid, "", &e.to_string()),
            }
        }

        "session_message" => {
            let target = args
                .get("session_id")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let content = args.get("content").and_then(|v| v.as_str()).unwrap_or("");
            if target.is_empty() {
                return ToolResultMessage::error(tcid, "", "session_id is required");
            }
            if content.is_empty() {
                return ToolResultMessage::error(tcid, "", "content is required");
            }
            let sender_info = match session_id {
                Some(sid) => format!("session:{}", sid),
                None => "session:unknown".to_string(),
            };
            let req = crate::protocol::Request::QueueMessage {
                target_session_id: target.to_string(),
                content: content.to_string(),
                sender_info,
                await_reply: false,
                reply_to: None,
            };
            match server_request(msg_tx, pending, req).await {
                Ok(crate::protocol::Response::Ok) => ToolResultMessage::success(
                    tcid,
                    "",
                    &format!("Message sent to session {}", target),
                ),
                Ok(crate::protocol::Response::OkWithNote { note }) => {
                    // The server accepted the message but wanted to tell
                    // us something about how it was handled (e.g. target
                    // is a placeholder/log-provider session — message
                    // recorded but no agent loop ran). Surface the note
                    // so the caller LLM can act on it.
                    ToolResultMessage::success(tcid, "", &note)
                }
                Ok(crate::protocol::Response::Error { message }) => {
                    ToolResultMessage::error(tcid, "", &message)
                }
                Ok(other) => {
                    ToolResultMessage::error(tcid, "", &format!("unexpected response: {:?}", other))
                }
                Err(e) => ToolResultMessage::error(tcid, "", &e),
            }
        }

        "session_id" => match session_id {
            Some(sid) => ToolResultMessage::success(
                tcid,
                "",
                &serde_json::json!({"session_id": sid}).to_string(),
            ),
            None => ToolResultMessage::error(tcid, "", "session_id not available"),
        },

        _ => ToolResultMessage::error(tcid, "", &format!("unknown session tool: {}", name)),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_cwd_ok_for_existing_dir() {
        assert!(matches!(classify_cwd("/tmp"), CwdState::Ok));
    }

    #[test]
    fn classify_cwd_missing_for_bogus_path() {
        let bogus = format!("/tmp/tau-bash-bogus-cwd-classify-{}", std::process::id());
        let _ = std::fs::remove_dir_all(&bogus);
        assert!(matches!(classify_cwd(&bogus), CwdState::Missing));
    }

    #[test]
    fn classify_cwd_not_a_directory_for_regular_file() {
        let path = if std::path::Path::new("/etc/hostname").is_file() {
            "/etc/hostname".to_string()
        } else {
            std::env::current_exe()
                .expect("current_exe")
                .to_string_lossy()
                .into_owned()
        };
        assert!(matches!(classify_cwd(&path), CwdState::NotADirectory));
    }

    #[test]
    fn cwd_state_error_messages_name_the_path_and_omit_os_errno() {
        let path = "/tmp/totally-bogus-path-for-error-message";
        let msg = CwdState::Missing.error_message(path);
        assert!(msg.contains(path));
        assert!(msg.contains("no longer exists"));
        assert!(!msg.contains("os error"));

        let msg = CwdState::NotADirectory.error_message(path);
        assert!(msg.contains(path));
        assert!(msg.contains("not a directory"));
    }

    #[test]
    fn prepend_note_adds_note_before_existing_text() {
        let mut msg = ToolResultMessage::success("tc", "bash", "hello\n");
        prepend_note(&mut msg, "note: ran in /root instead.");
        let text = msg.content[0].text();
        assert!(text.starts_with("note: ran in /root instead."));
        assert!(text.contains("hello"));
        // We must not flip is_error.
        assert!(!msg.is_error);
    }

    #[test]
    fn prepend_note_handles_empty_existing_text() {
        let mut msg = ToolResultMessage::success("tc", "bash", "");
        prepend_note(&mut msg, "note: ran in /root.");
        assert_eq!(msg.content[0].text(), "note: ran in /root.");
    }

    #[test]
    fn prepend_note_preserves_error_state() {
        let mut msg = ToolResultMessage::error("tc", "bash", "boom\n(exit code: 1)");
        prepend_note(&mut msg, "note: substituted cwd.");
        let text = msg.content[0].text();
        assert!(text.starts_with("note: substituted cwd."));
        assert!(text.contains("(exit code: 1)"));
        // Errors stay errors.
        assert!(msg.is_error);
    }

    /// Spawn a fake "server" task that pulls `ServerRequest`s off
    /// `msg_rx` and answers them by completing the corresponding entry
    /// in `pending`. The provided closure decides what to send back for
    /// each request.
    fn spawn_fake_server(
        msg_rx: smol::channel::Receiver<PluginMessage>,
        pending: Arc<Mutex<HashMap<String, smol::channel::Sender<crate::protocol::Response>>>>,
        respond: impl Fn(crate::protocol::Request) -> crate::protocol::Response + Send + Sync + 'static,
    ) -> smol::Task<()> {
        smol::spawn(async move {
            while let Ok(msg) = msg_rx.recv().await {
                if let PluginMessage::ServerRequest {
                    request_id,
                    request,
                } = msg
                {
                    let resp = respond(request);
                    let tx = pending.lock().await.remove(&request_id);
                    if let Some(tx) = tx {
                        let _ = tx.send(resp).await;
                    }
                }
            }
        })
    }

    #[test]
    fn resolve_bash_cwd_uses_existing_cwd_unchanged() {
        smol::block_on(async {
            let (msg_tx, msg_rx) = smol::channel::unbounded::<PluginMessage>();
            let pending: Arc<
                Mutex<HashMap<String, smol::channel::Sender<crate::protocol::Response>>>,
            > = Arc::new(Mutex::new(HashMap::new()));

            // Server should never be asked, but spawn one so the test
            // doesn't hang if the production code accidentally queries.
            let server = spawn_fake_server(msg_rx, pending.clone(), |_req| {
                crate::protocol::Response::Error {
                    message: "unexpected request".into(),
                }
            });

            let res = resolve_bash_cwd("/tmp", Some("tau"), &msg_tx, &pending).await;
            match res {
                BashCwdResolution::Use(c) => assert_eq!(c, "/tmp"),
                other => panic!("expected Use, got {:?}", debug_resolution(&other)),
            }
            drop(msg_tx);
            let _ = server.cancel().await;
        });
    }

    #[test]
    fn resolve_bash_cwd_falls_back_to_project_root_when_cwd_missing() {
        smol::block_on(async {
            // Use the temp dir as a stand-in project root — it always exists.
            let project_root = std::env::temp_dir().to_string_lossy().into_owned();
            let bogus_cwd = format!(
                "/tmp/tau-bash-bogus-cwd-fallback-{}-{}",
                std::process::id(),
                rand_suffix(),
            );
            let _ = std::fs::remove_dir_all(&bogus_cwd);
            assert!(!std::path::Path::new(&bogus_cwd).exists());

            let (msg_tx, msg_rx) = smol::channel::unbounded::<PluginMessage>();
            let pending: Arc<
                Mutex<HashMap<String, smol::channel::Sender<crate::protocol::Response>>>,
            > = Arc::new(Mutex::new(HashMap::new()));

            let project_root_clone = project_root.clone();
            let server = spawn_fake_server(msg_rx, pending.clone(), move |req| match req {
                crate::protocol::Request::GetProjectInfo { project_name } => {
                    assert_eq!(project_name, "tau");
                    crate::protocol::Response::ProjectInfo {
                        project: Some(crate::protocol::ProjectInfoEntry {
                            name: "tau".into(),
                            path: project_root_clone.clone(),
                        }),
                    }
                }
                other => crate::protocol::Response::Error {
                    message: format!("unexpected: {:?}", other),
                },
            });

            let res = resolve_bash_cwd(&bogus_cwd, Some("tau"), &msg_tx, &pending).await;
            match res {
                BashCwdResolution::Fallback { cwd, note } => {
                    assert_eq!(cwd, project_root);
                    assert!(
                        note.contains(&bogus_cwd),
                        "note should mention the dead cwd: {}",
                        note
                    );
                    assert!(
                        note.contains(&project_root),
                        "note should mention the project root: {}",
                        note
                    );
                    assert!(
                        note.contains("no longer exists"),
                        "note should explain why we substituted: {}",
                        note
                    );
                }
                other => panic!("expected Fallback, got {:?}", debug_resolution(&other)),
            }
            drop(msg_tx);
            let _ = server.cancel().await;
        });
    }

    #[test]
    fn resolve_bash_cwd_hard_errors_when_no_project_name() {
        smol::block_on(async {
            let bogus_cwd = format!(
                "/tmp/tau-bash-bogus-cwd-noproj-{}-{}",
                std::process::id(),
                rand_suffix(),
            );
            let _ = std::fs::remove_dir_all(&bogus_cwd);

            let (msg_tx, msg_rx) = smol::channel::unbounded::<PluginMessage>();
            let pending: Arc<
                Mutex<HashMap<String, smol::channel::Sender<crate::protocol::Response>>>,
            > = Arc::new(Mutex::new(HashMap::new()));
            let server = spawn_fake_server(msg_rx, pending.clone(), |_| {
                crate::protocol::Response::Error {
                    message: "unexpected".into(),
                }
            });

            let res = resolve_bash_cwd(&bogus_cwd, None, &msg_tx, &pending).await;
            match res {
                BashCwdResolution::HardError(msg) => {
                    assert!(msg.contains(&bogus_cwd));
                    assert!(!msg.contains("os error"));
                }
                other => panic!("expected HardError, got {:?}", debug_resolution(&other)),
            }
            drop(msg_tx);
            let _ = server.cancel().await;
        });
    }

    #[test]
    fn resolve_bash_cwd_hard_errors_when_project_unknown() {
        smol::block_on(async {
            let bogus_cwd = format!(
                "/tmp/tau-bash-bogus-cwd-unknownproj-{}-{}",
                std::process::id(),
                rand_suffix(),
            );
            let _ = std::fs::remove_dir_all(&bogus_cwd);

            let (msg_tx, msg_rx) = smol::channel::unbounded::<PluginMessage>();
            let pending: Arc<
                Mutex<HashMap<String, smol::channel::Sender<crate::protocol::Response>>>,
            > = Arc::new(Mutex::new(HashMap::new()));
            let server = spawn_fake_server(msg_rx, pending.clone(), |_req| {
                crate::protocol::Response::ProjectInfo { project: None }
            });

            let res =
                resolve_bash_cwd(&bogus_cwd, Some("unknown-project"), &msg_tx, &pending).await;
            match res {
                BashCwdResolution::HardError(msg) => assert!(msg.contains(&bogus_cwd)),
                other => panic!("expected HardError, got {:?}", debug_resolution(&other)),
            }
            drop(msg_tx);
            let _ = server.cancel().await;
        });
    }

    #[test]
    fn resolve_bash_cwd_hard_errors_when_project_root_also_missing() {
        smol::block_on(async {
            let bogus_cwd = format!(
                "/tmp/tau-bash-bogus-cwd-bothdead-{}-{}",
                std::process::id(),
                rand_suffix(),
            );
            let bogus_root = format!(
                "/tmp/tau-bash-bogus-root-bothdead-{}-{}",
                std::process::id(),
                rand_suffix(),
            );
            let _ = std::fs::remove_dir_all(&bogus_cwd);
            let _ = std::fs::remove_dir_all(&bogus_root);

            let (msg_tx, msg_rx) = smol::channel::unbounded::<PluginMessage>();
            let pending: Arc<
                Mutex<HashMap<String, smol::channel::Sender<crate::protocol::Response>>>,
            > = Arc::new(Mutex::new(HashMap::new()));
            let bogus_root_clone = bogus_root.clone();
            let server = spawn_fake_server(msg_rx, pending.clone(), move |_req| {
                crate::protocol::Response::ProjectInfo {
                    project: Some(crate::protocol::ProjectInfoEntry {
                        name: "tau".into(),
                        path: bogus_root_clone.clone(),
                    }),
                }
            });

            let res = resolve_bash_cwd(&bogus_cwd, Some("tau"), &msg_tx, &pending).await;
            match res {
                BashCwdResolution::HardError(msg) => assert!(msg.contains(&bogus_cwd)),
                other => panic!("expected HardError, got {:?}", debug_resolution(&other)),
            }
            drop(msg_tx);
            let _ = server.cancel().await;
        });
    }

    fn debug_resolution(r: &BashCwdResolution) -> String {
        match r {
            BashCwdResolution::Use(c) => format!("Use({})", c),
            BashCwdResolution::Fallback { cwd, note } => {
                format!("Fallback {{ cwd: {}, note: {} }}", cwd, note)
            }
            BashCwdResolution::HardError(m) => format!("HardError({})", m),
        }
    }

    fn rand_suffix() -> String {
        // Cheap, dependency-free unique suffix — not security-sensitive.
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos().to_string())
            .unwrap_or_else(|_| "0".into())
    }
}
