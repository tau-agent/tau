//! Bash tool — execute shell commands.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use nix::sys::signal::{self, Signal};
use nix::unistd::{Pid, setsid};

use super::{ToolDef, ToolOutput};
use crate::types::Tool;

pub fn tool_def() -> ToolDef {
    ToolDef {
        tool: Tool {
            name: "bash".into(),
            description: "Execute a bash command and return its output. Use for running shell commands, scripts, build tools, git operations, etc.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "The bash command to execute"
                    },
                    "timeout": {
                        "type": "integer",
                        "description": "Timeout in seconds (default: 120)"
                    }
                },
                "required": ["command"]
            }),
        },
        execute: Box::new(execute),
    }
}

/// Spawn the bash child process with a new session (setsid) so we can kill
/// the entire process group on timeout.
fn spawn_child(command: &str, cwd: &str) -> Result<std::process::Child, std::io::Error> {
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
                // Create a new session/process group so we can kill all descendants.
                setsid().map_err(std::io::Error::other)?;
                Ok(())
            })
            .spawn()
    }
}

/// Start a watchdog thread that kills the child's process group after `timeout_secs`.
/// Returns a flag that is set to `true` if the timeout fires.
fn start_watchdog(child_id: u32, timeout_secs: u64) -> Arc<AtomicBool> {
    let timed_out = Arc::new(AtomicBool::new(false));
    let flag = timed_out.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(timeout_secs));
        flag.store(true, Ordering::Relaxed);
        // Kill the entire process group (negative PID).
        let _ = signal::killpg(Pid::from_raw(child_id as i32), Signal::SIGKILL);
    });
    timed_out
}

/// Format the final tool output from collected stdout/stderr, exit code, and timeout flag.
fn format_output(stdout: String, stderr: String, exit_code: i32, timed_out: bool) -> ToolOutput {
    let mut text = stdout;
    if !stderr.is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str("STDERR:\n");
        text.push_str(&stderr);
    }

    // Truncate very long output
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

    if timed_out {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str("(timed out)");
        return ToolOutput::error(text.trim_end().to_string());
    }

    let success = exit_code == 0;
    if text.is_empty() {
        text = format!("(exit code: {})", exit_code);
    } else if !success {
        text.push_str(&format!("\n(exit code: {})", exit_code));
    }

    let text = text.trim_end().to_string();
    if success {
        ToolOutput::text(text)
    } else {
        ToolOutput::error(text)
    }
}

/// Streaming bash execution: calls `on_delta` for each output line.
/// Returns the final ToolOutput.
pub fn execute_streaming(
    args: &serde_json::Value,
    cwd: &str,
    mut on_delta: impl FnMut(&str),
) -> ToolOutput {
    let Some(command) = args.get("command").and_then(|c| c.as_str()) else {
        return ToolOutput::error("missing 'command' argument");
    };

    let timeout_secs = args.get("timeout").and_then(|t| t.as_u64()).unwrap_or(120);

    let mut child = match spawn_child(command, cwd) {
        Ok(c) => c,
        Err(e) => return ToolOutput::error(format!("failed to execute command: {}", e)),
    };

    let timed_out = start_watchdog(child.id(), timeout_secs);

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let mut collected_stdout = String::new();

    let stderr_handle = std::thread::spawn(move || {
        let mut collected = String::new();
        if let Some(stderr) = stderr {
            use std::io::BufRead;
            let reader = std::io::BufReader::new(stderr);
            for line in reader.lines().map_while(Result::ok) {
                collected.push_str(&line);
                collected.push('\n');
            }
        }
        collected
    });

    if let Some(stdout) = stdout {
        use std::io::BufRead;
        let reader = std::io::BufReader::new(stdout);
        for line in reader.lines().map_while(Result::ok) {
            on_delta(&line);
            collected_stdout.push_str(&line);
            collected_stdout.push('\n');
        }
    }

    let collected_stderr = stderr_handle.join().unwrap_or_default();
    let status = child.wait();
    let exit_code = status.ok().and_then(|s| s.code()).unwrap_or(-1);

    format_output(
        collected_stdout,
        collected_stderr,
        exit_code,
        timed_out.load(Ordering::Relaxed),
    )
}

fn execute(args: serde_json::Value, cwd: &str) -> ToolOutput {
    let Some(command) = args.get("command").and_then(|c| c.as_str()) else {
        return ToolOutput::error("missing 'command' argument");
    };

    let timeout_secs = args.get("timeout").and_then(|t| t.as_u64()).unwrap_or(120);

    let mut child = match spawn_child(command, cwd) {
        Ok(c) => c,
        Err(e) => return ToolOutput::error(format!("failed to execute command: {}", e)),
    };

    let timed_out = start_watchdog(child.id(), timeout_secs);

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let stderr_handle = std::thread::spawn(move || {
        let mut collected = String::new();
        if let Some(stderr) = stderr {
            use std::io::BufRead;
            let reader = std::io::BufReader::new(stderr);
            for line in reader.lines().map_while(Result::ok) {
                collected.push_str(&line);
                collected.push('\n');
            }
        }
        collected
    });

    let mut collected_stdout = String::new();
    if let Some(stdout) = stdout {
        use std::io::BufRead;
        let reader = std::io::BufReader::new(stdout);
        for line in reader.lines().map_while(Result::ok) {
            collected_stdout.push_str(&line);
            collected_stdout.push('\n');
        }
    }

    let collected_stderr = stderr_handle.join().unwrap_or_default();
    let status = child.wait();
    let exit_code = status.ok().and_then(|s| s.code()).unwrap_or(-1);

    format_output(
        collected_stdout,
        collected_stderr,
        exit_code,
        timed_out.load(Ordering::Relaxed),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::Instant;

    #[test]
    fn bash_simple_command() {
        let output = execute(json!({"command": "echo hello"}), "/tmp");
        assert!(!output.is_error);
        assert!(output.content[0].text().contains("hello"));
    }

    #[test]
    fn bash_timeout_kills_process() {
        let start = Instant::now();
        let output = execute(json!({"command": "sleep 60", "timeout": 2}), "/tmp");
        let elapsed = start.elapsed();

        assert!(output.is_error, "should be error on timeout");
        assert!(
            output.content[0].text().contains("timed out"),
            "output should indicate timeout: {}",
            output.content[0].text()
        );
        assert!(
            elapsed.as_secs() < 5,
            "should complete in ~2s, took {}s",
            elapsed.as_secs()
        );
    }

    #[test]
    fn bash_timeout_not_triggered() {
        let start = Instant::now();
        let output = execute(json!({"command": "echo fast", "timeout": 10}), "/tmp");
        let elapsed = start.elapsed();

        assert!(!output.is_error);
        assert!(output.content[0].text().contains("fast"));
        assert!(elapsed.as_secs() < 3);
    }

    #[test]
    fn bash_streaming_timeout() {
        let start = Instant::now();
        let mut deltas = Vec::new();
        let output = execute_streaming(
            &json!({"command": "echo before; sleep 60", "timeout": 2}),
            "/tmp",
            |line| deltas.push(line.to_string()),
        );
        let elapsed = start.elapsed();

        assert!(output.is_error);
        assert!(output.content[0].text().contains("timed out"));
        assert!(elapsed.as_secs() < 5);
        // Should have captured "before" line before timeout
        assert!(
            deltas.iter().any(|d| d.contains("before")),
            "should stream output before timeout: {:?}",
            deltas
        );
    }

    #[test]
    fn bash_timeout_kills_child_processes() {
        // Verify that child processes spawned by bash are also killed (process group kill).
        let start = Instant::now();
        let output = execute(
            json!({"command": "bash -c 'sleep 60 & sleep 60 & wait'", "timeout": 2}),
            "/tmp",
        );
        let elapsed = start.elapsed();

        assert!(output.is_error);
        assert!(output.content[0].text().contains("timed out"));
        assert!(elapsed.as_secs() < 5);
    }
}
