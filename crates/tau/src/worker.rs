//! Worker subprocess for tool execution.
//!
//! The daemon spawns a worker process (`tau worker`) that executes tool calls.
//! Communication is JSON lines over stdin/stdout.
//! Supports streaming output: worker sends `OutputDelta` lines during execution,
//! then a final `Done` message.

use std::io::{BufRead, BufReader, BufWriter, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde::{Deserialize, Serialize};

use crate::types::*;

// ---------------------------------------------------------------------------
// Wire protocol
// ---------------------------------------------------------------------------

/// Request sent to worker (daemon → worker stdin).
#[derive(Debug, Serialize, Deserialize)]
pub struct WorkerRequest {
    pub tool_call_id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

/// Message from worker (worker stdout → daemon).
/// Tagged enum: either incremental output or final result.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkerMessage {
    /// Incremental output line during tool execution (for UI streaming).
    OutputDelta { tool_call_id: String, text: String },
    /// Tool execution completed — final result.
    Done {
        tool_call_id: String,
        content: Vec<ToolResultContent>,
        is_error: bool,
    },
}

// Keep the old type as an alias for backward compat
pub type WorkerResponse = WorkerMessage;

// ---------------------------------------------------------------------------
// Worker handle (daemon side)
// ---------------------------------------------------------------------------

/// Handle to a spawned worker subprocess.
pub struct Worker {
    child: Child,
    stdin: BufWriter<ChildStdin>,
    stdout: BufReader<ChildStdout>,
}

impl Worker {
    /// Spawn a worker subprocess with the given working directory.
    pub fn spawn(cwd: &str) -> crate::Result<Self> {
        let exe = std::env::current_exe().map_err(|e| crate::Error::Io(e.to_string()))?;

        let mut child = Command::new(exe)
            .arg("worker")
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit()) // worker errors go to daemon stderr
            .spawn()
            .map_err(|e| crate::Error::Io(format!("spawn worker: {}", e)))?;

        let stdin = BufWriter::new(
            child
                .stdin
                .take()
                .ok_or_else(|| crate::Error::Io("worker stdin not available".into()))?,
        );
        let stdout = BufReader::new(
            child
                .stdout
                .take()
                .ok_or_else(|| crate::Error::Io("worker stdout not available".into()))?,
        );

        Ok(Self {
            child,
            stdin,
            stdout,
        })
    }

    /// Execute a tool call via the worker subprocess.
    /// Calls `on_output` for each incremental output line (streaming).
    pub fn execute(
        &mut self,
        tool_call: &ToolCall,
        on_output: &mut dyn FnMut(&str),
    ) -> crate::Result<ToolResultMessage> {
        let req = WorkerRequest {
            tool_call_id: tool_call.id.clone(),
            name: tool_call.name.clone(),
            arguments: tool_call.arguments.clone(),
        };

        // Send request
        let mut line =
            serde_json::to_string(&req).map_err(|e| crate::Error::Parse(e.to_string()))?;
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .map_err(|e| crate::Error::Io(format!("write to worker: {}", e)))?;
        self.stdin
            .flush()
            .map_err(|e| crate::Error::Io(format!("flush worker: {}", e)))?;

        // Read messages until Done
        loop {
            let mut resp_line = String::new();
            self.stdout
                .read_line(&mut resp_line)
                .map_err(|e| crate::Error::Io(format!("read from worker: {}", e)))?;

            if resp_line.is_empty() {
                return Err(crate::Error::Io("worker closed unexpectedly".into()));
            }

            let msg: WorkerMessage =
                serde_json::from_str(&resp_line).map_err(|e| crate::Error::Parse(e.to_string()))?;

            match msg {
                WorkerMessage::OutputDelta { text, .. } => {
                    on_output(&text);
                }
                WorkerMessage::Done {
                    tool_call_id,
                    content,
                    is_error,
                } => {
                    return Ok(ToolResultMessage {
                        tool_call_id,
                        tool_name: tool_call.name.clone(),
                        content,
                        details: None,
                        is_error,
                        timestamp: timestamp_ms(),
                    });
                }
            }
        }
    }

    /// Kill the worker process.
    pub fn kill(&mut self) {
        match self.child.try_wait() {
            Ok(Some(_)) => {}
            _ => {
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
        }
    }

    /// Get the child PID.
    pub fn pid(&self) -> u32 {
        self.child.id()
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Trait for tool execution (allows subprocess or in-process).
pub trait ToolExecutor {
    fn execute(
        &mut self,
        tool_call: &ToolCall,
        on_output: &mut dyn FnMut(&str),
    ) -> crate::Result<ToolResultMessage>;
}

impl ToolExecutor for Worker {
    fn execute(
        &mut self,
        tool_call: &ToolCall,
        on_output: &mut dyn FnMut(&str),
    ) -> crate::Result<ToolResultMessage> {
        self.execute(tool_call, on_output)
    }
}

/// In-process worker for testing (no subprocess).
pub struct InProcessWorker {
    tools: Vec<crate::tools::ToolDef>,
    cwd: String,
}

impl InProcessWorker {
    pub fn new(cwd: &str) -> Self {
        Self {
            tools: crate::tools::default_tools(),
            cwd: cwd.to_string(),
        }
    }
}

impl ToolExecutor for InProcessWorker {
    fn execute(
        &mut self,
        tool_call: &ToolCall,
        _on_output: &mut dyn FnMut(&str),
    ) -> crate::Result<ToolResultMessage> {
        let result = crate::tools::execute_tool(&self.tools, tool_call, &self.cwd);
        Ok(result)
    }
}

// ---------------------------------------------------------------------------
// Worker main loop (runs in the subprocess)
// ---------------------------------------------------------------------------

/// Helper to send a worker message to stdout.
fn send_worker_message(writer: &mut impl Write, msg: &WorkerMessage) {
    if let Ok(mut line) = serde_json::to_string(msg) {
        line.push('\n');
        let _ = writer.write_all(line.as_bytes());
        let _ = writer.flush();
    }
}

/// Run the worker loop: read tool calls from stdin, execute, write results to stdout.
/// Called from `tau worker` subcommand.
pub fn run_worker_loop() {
    let tools = crate::tools::default_tools();
    let cwd = std::env::current_dir()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut writer = BufWriter::new(stdout.lock());

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if line.trim().is_empty() {
            continue;
        }

        let req: WorkerRequest = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("worker: bad request: {}", e);
                continue;
            }
        };

        let tool_call = ToolCall {
            id: req.tool_call_id.clone(),
            name: req.name.clone(),
            arguments: req.arguments,
        };

        // For bash, use streaming execution
        if req.name == "bash" {
            let result =
                crate::tools::bash::execute_streaming(&tool_call.arguments, &cwd, |delta| {
                    send_worker_message(
                        &mut writer,
                        &WorkerMessage::OutputDelta {
                            tool_call_id: req.tool_call_id.clone(),
                            text: delta.to_string(),
                        },
                    );
                });

            let content = result.content;
            let is_error = result.is_error;

            send_worker_message(
                &mut writer,
                &WorkerMessage::Done {
                    tool_call_id: req.tool_call_id,
                    content,
                    is_error,
                },
            );
        } else {
            // Non-streaming tools
            let result = crate::tools::execute_tool(&tools, &tool_call, &cwd);

            send_worker_message(
                &mut writer,
                &WorkerMessage::Done {
                    tool_call_id: req.tool_call_id,
                    content: result.content,
                    is_error: result.is_error,
                },
            );
        }
    }
}
