//! Bash tool — execute shell commands.

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

fn execute(args: serde_json::Value) -> ToolOutput {
    let Some(command) = args.get("command").and_then(|c| c.as_str()) else {
        return ToolOutput::error("missing 'command' argument");
    };

    let timeout_secs = args.get("timeout").and_then(|t| t.as_u64()).unwrap_or(120);

    let result = std::process::Command::new("bash")
        .arg("-c")
        .arg(command)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        // Prevent child processes from accessing the TTY (blocks sudo, passwd, etc.)
        .env("SUDO_ASKPASS", "/bin/false")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output();

    // TODO: actual timeout handling with a thread/signal
    let _ = timeout_secs;

    match result {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);

            let mut text = String::new();
            if !stdout.is_empty() {
                text.push_str(&stdout);
            }
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

            if text.is_empty() {
                text = format!("(exit code: {})", output.status.code().unwrap_or(-1));
            } else if !output.status.success() {
                text.push_str(&format!(
                    "\n(exit code: {})",
                    output.status.code().unwrap_or(-1)
                ));
            }

            if output.status.success() {
                ToolOutput::text(text)
            } else {
                ToolOutput::error(text)
            }
        }
        Err(e) => ToolOutput::error(format!("failed to execute command: {}", e)),
    }
}
