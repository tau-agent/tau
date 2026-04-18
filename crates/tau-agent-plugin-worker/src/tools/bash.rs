//! Bash tool — execute shell commands.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use nix::sys::signal::{self, Signal};
use nix::unistd::{Pid, setsid};

use super::{ToolDef, ToolOutput};
use tau_agent_plugin::Tool;

// ---------------------------------------------------------------------------
// Tracked-PGID registry
//
// Every bash child spawned by this tool is run in its own session/process
// group via `setsid()`.  We register the resulting PGID here so that on
// process shutdown (SIGTERM/SIGHUP, panic, etc.) we can SIGKILL every
// orphaned process group instead of leaking them.
//
// The registry is intentionally minimal: callers track on spawn, untrack
// on completion / cancel / timeout, and a single `kill_all_tracked()`
// helper drains it during shutdown.
// ---------------------------------------------------------------------------

static TRACKED_PGIDS: LazyLock<Mutex<HashSet<i32>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

/// Register a PGID (almost always the bash child's own pid, post-`setsid`)
/// so it can be killed on shutdown.
pub fn track_pgid(pgid: i32) {
    if let Ok(mut set) = TRACKED_PGIDS.lock() {
        set.insert(pgid);
    }
}

/// Remove a PGID from the registry once the corresponding bash invocation
/// has finished (normal exit, cancel, or timeout).
pub fn untrack_pgid(pgid: i32) {
    if let Ok(mut set) = TRACKED_PGIDS.lock() {
        set.remove(&pgid);
    }
}

/// SIGKILL every tracked process group and clear the registry.
///
/// Cheap and infallible: errors from `killpg` are ignored (process group
/// may already be gone).  Safe to call multiple times.
///
/// Intended to be called from a signal handler / shutdown path in the
/// worker process.  Does **not** wait for the killed processes.
pub fn kill_all_tracked() {
    let pgids: Vec<i32> = match TRACKED_PGIDS.lock() {
        Ok(mut set) => set.drain().collect(),
        Err(_) => return,
    };
    for pgid in pgids {
        let _ = signal::killpg(Pid::from_raw(pgid), Signal::SIGKILL);
    }
}

/// Test/debug helper: return any one tracked PGID, or None if none.
pub fn first_tracked_pgid() -> Option<i32> {
    TRACKED_PGIDS
        .lock()
        .ok()
        .and_then(|s| s.iter().next().copied())
}

/// Test/debug helper: number of tracked PGIDs.
pub fn tracked_pgid_count() -> usize {
    TRACKED_PGIDS.lock().map(|s| s.len()).unwrap_or(0)
}

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
        prepare_arguments: None,
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
        // The reaper / wait() path will untrack this PGID, but the kill
        // path is the last point at which we *know* the timeout fired,
        // so untrack here too in case the wait races with shutdown.
        untrack_pgid(child_id as i32);
    });
    timed_out
}

/// Start a cancel-watcher thread that kills the child's process group as
/// soon as the [`CancelToken`] becomes cancelled. Returns:
/// - a `cancelled` flag that is set to `true` if the kill fired from this
///   watcher (so `format_output` can distinguish cancelled from timed-out),
/// - a `done` flag the caller flips to `true` once the child has exited
///   normally, so the watcher thread wakes up and stops polling.
///
/// The watcher polls every 100 ms so that cancellation appears "immediate"
/// from the user's perspective (well under the old 120 s watchdog ceiling)
/// without burning CPU on a tight loop.
fn start_cancel_watcher(
    child_id: u32,
    cancel: tau_agent_plugin::CancelToken,
) -> (Arc<AtomicBool>, Arc<AtomicBool>) {
    let cancelled = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicBool::new(false));
    let cancelled_flag = cancelled.clone();
    let done_flag = done.clone();
    std::thread::spawn(move || {
        loop {
            if done_flag.load(Ordering::Relaxed) {
                return;
            }
            if cancel.is_cancelled() {
                cancelled_flag.store(true, Ordering::Relaxed);
                let _ = signal::killpg(Pid::from_raw(child_id as i32), Signal::SIGKILL);
                untrack_pgid(child_id as i32);
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    });
    (cancelled, done)
}

/// Format the final tool output from collected stdout/stderr, exit code, timeout flag, and cancelled flag.
fn format_output(
    stdout: String,
    stderr: String,
    exit_code: i32,
    timed_out: bool,
    cancelled: bool,
) -> ToolOutput {
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

    // Cancellation takes priority over timeout in the output marker: if a
    // user Ctrl-C'd during a command that happened to also hit its timeout
    // window, "(cancelled)" is the more useful signal.
    if cancelled {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str("(cancelled)");
        return ToolOutput::error(text.trim_end().to_string());
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

/// Generate a summary for long bash output.
fn maybe_add_summary(output: &mut ToolOutput, command: &str, exit_code: i32) {
    let text_content = output.content.first().map(|c| c.text()).unwrap_or("");
    let line_count = text_content.lines().count();
    if line_count > 20 {
        let cmd_preview = if command.chars().count() > 60 {
            let truncated: String = command.chars().take(57).collect();
            format!("{}...", truncated)
        } else {
            command.to_string()
        };
        let exit_suffix = if exit_code != 0 {
            format!(", exit {}", exit_code)
        } else {
            String::new()
        };
        output.summary = Some(format!(
            "bash: $ {} → {} lines{}",
            cmd_preview, line_count, exit_suffix
        ));
    }
}

/// Streaming bash execution: calls `on_delta` for each output line.
/// Returns the final ToolOutput.
pub fn execute_streaming(
    args: &serde_json::Value,
    cwd: &str,
    cancel: &tau_agent_plugin::CancelToken,
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

    let pgid = child.id() as i32;
    track_pgid(pgid);

    let timed_out = start_watchdog(child.id(), timeout_secs);
    let (cancelled, cancel_done) = start_cancel_watcher(child.id(), cancel.clone());

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
    untrack_pgid(pgid);
    cancel_done.store(true, Ordering::Relaxed);

    let mut output = format_output(
        collected_stdout,
        collected_stderr,
        exit_code,
        timed_out.load(Ordering::Relaxed),
        cancelled.load(Ordering::Relaxed),
    );
    maybe_add_summary(&mut output, command, exit_code);
    output
}

fn execute(
    args: serde_json::Value,
    cwd: &str,
    cancel: &tau_agent_plugin::CancelToken,
) -> ToolOutput {
    let Some(command) = args.get("command").and_then(|c| c.as_str()) else {
        return ToolOutput::error("missing 'command' argument");
    };

    let timeout_secs = args.get("timeout").and_then(|t| t.as_u64()).unwrap_or(120);

    let mut child = match spawn_child(command, cwd) {
        Ok(c) => c,
        Err(e) => return ToolOutput::error(format!("failed to execute command: {}", e)),
    };

    let pgid = child.id() as i32;
    track_pgid(pgid);

    let timed_out = start_watchdog(child.id(), timeout_secs);
    let (cancelled, cancel_done) = start_cancel_watcher(child.id(), cancel.clone());

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
    untrack_pgid(pgid);
    cancel_done.store(true, Ordering::Relaxed);

    let mut output = format_output(
        collected_stdout,
        collected_stderr,
        exit_code,
        timed_out.load(Ordering::Relaxed),
        cancelled.load(Ordering::Relaxed),
    );
    maybe_add_summary(&mut output, command, exit_code);
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Mutex;
    use std::time::Instant;

    /// Serialise tests that exercise the global TRACKED_PGIDS registry.
    /// Without this, `kill_all_tracked_kills_running_bash_children`
    /// drains PGIDs from concurrent timeout tests and kills their bash
    /// children early, breaking their assertions.
    static REGISTRY_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn lock_registry() -> std::sync::MutexGuard<'static, ()> {
        REGISTRY_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn bash_simple_command() {
        let _guard = lock_registry();
        let output = execute(
            json!({"command": "echo hello"}),
            "/tmp",
            &tau_agent_plugin::CancelToken::new(),
        );
        assert!(!output.is_error);
        assert!(output.content[0].text().contains("hello"));
    }

    #[test]
    fn bash_timeout_kills_process() {
        let _guard = lock_registry();
        let start = Instant::now();
        let output = execute(
            json!({"command": "sleep 60", "timeout": 2}),
            "/tmp",
            &tau_agent_plugin::CancelToken::new(),
        );
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
        let _guard = lock_registry();
        let start = Instant::now();
        let output = execute(
            json!({"command": "echo fast", "timeout": 10}),
            "/tmp",
            &tau_agent_plugin::CancelToken::new(),
        );
        let elapsed = start.elapsed();

        assert!(!output.is_error);
        assert!(output.content[0].text().contains("fast"));
        assert!(elapsed.as_secs() < 3);
    }

    #[test]
    fn bash_streaming_timeout() {
        let _guard = lock_registry();
        let start = Instant::now();
        let mut deltas = Vec::new();
        let output = execute_streaming(
            &json!({"command": "echo before; sleep 60", "timeout": 2}),
            "/tmp",
            &tau_agent_plugin::CancelToken::new(),
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
        let _guard = lock_registry();
        // Verify that child processes spawned by bash are also killed (process group kill).
        let start = Instant::now();
        let output = execute(
            json!({"command": "bash -c 'sleep 60 & sleep 60 & wait'", "timeout": 2}),
            "/tmp",
            &tau_agent_plugin::CancelToken::new(),
        );
        let elapsed = start.elapsed();

        assert!(output.is_error);
        assert!(output.content[0].text().contains("timed out"));
        assert!(elapsed.as_secs() < 5);
    }

    #[test]
    fn kill_all_tracked_kills_running_bash_children() {
        let _guard = lock_registry();
        // Spawn a bash command with a long sleep on a background thread,
        // wait for it to register itself in TRACKED_PGIDS, then call
        // kill_all_tracked() and verify the process group is gone.
        use std::time::Duration;

        // Use a unique marker so we can find our specific process via ps.
        let marker = format!("tau-bash-kill-test-{}", std::process::id());
        let marker_for_thread = marker.clone();

        let handle = std::thread::spawn(move || {
            // sleep 30; the marker is encoded in the command so we can grep for it.
            execute(
                json!({
                    "command": format!("# {}\nsleep 30", marker_for_thread),
                    "timeout": 60,
                }),
                "/tmp",
                &tau_agent_plugin::CancelToken::new(),
            )
        });

        // Wait until the bash invocation registers a PGID.
        let pgid = {
            let mut found = None;
            for _ in 0..50 {
                std::thread::sleep(Duration::from_millis(100));
                if let Ok(set) = TRACKED_PGIDS.lock()
                    && let Some(p) = set.iter().next().copied()
                {
                    found = Some(p);
                    break;
                }
            }
            found.expect("bash child failed to register PGID within 5s")
        };

        // Sanity: the process group is alive.
        assert!(
            signal::killpg(Pid::from_raw(pgid), None).is_ok(),
            "expected pgid {} to be alive before kill_all_tracked()",
            pgid,
        );

        // Drain and kill.
        kill_all_tracked();

        // Registry must be empty.
        assert!(
            TRACKED_PGIDS
                .lock()
                .expect("registry mutex poisoned")
                .is_empty(),
            "TRACKED_PGIDS not empty after kill_all_tracked()",
        );

        // Process group should be gone within a short timeout.
        let mut still_alive = true;
        for _ in 0..50 {
            std::thread::sleep(Duration::from_millis(100));
            if signal::killpg(Pid::from_raw(pgid), None).is_err() {
                still_alive = false;
                break;
            }
        }
        assert!(
            !still_alive,
            "pgid {} still alive 5s after kill_all_tracked()",
            pgid,
        );

        // The execute() call should now return (timed-out / killed).
        let output = handle.join().expect("bash thread panicked");
        // It either reports the killed sub-process exit or treats it as
        // an error — we don't care which, just that it returned.
        let _ = output;
    }

    #[test]
    fn bash_cancel_flag_kills_subprocess_quickly() {
        // Setting the cancel token while a bash subprocess is running must
        // kill it within ~200 ms — the watcher polls at 100 ms intervals —
        // rather than waiting for the timeout watchdog.
        let _guard = lock_registry();
        use std::time::Duration;

        let cancel = tau_agent_plugin::CancelToken::new();
        let cancel_clone = cancel.clone();

        // Flip the cancel flag after 150 ms.
        let flipper = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            cancel_clone.cancel();
        });

        let start = Instant::now();
        // Long sleep with a long timeout — only cancellation can end it
        // inside the test budget.
        let output = execute(
            json!({"command": "sleep 30", "timeout": 60}),
            "/tmp",
            &cancel,
        );
        let elapsed = start.elapsed();

        flipper.join().expect("flipper thread panicked");

        assert!(output.is_error, "cancelled bash should be an error result");
        assert!(
            output.content[0].text().contains("cancelled"),
            "output should mention cancellation, got: {}",
            output.content[0].text()
        );
        // Must return well before the 30 s sleep or 60 s timeout.
        assert!(
            elapsed < Duration::from_secs(2),
            "cancel should kill subprocess promptly, took {:?}",
            elapsed
        );
    }

    #[test]
    fn bash_prefire_cancel_returns_immediately() {
        // A token that is already cancelled before execute() is called
        // should still result in a cancelled-output termination without
        // burning the full timeout.
        let _guard = lock_registry();
        use std::time::Duration;

        let cancel = tau_agent_plugin::CancelToken::new();
        cancel.cancel();

        let start = Instant::now();
        let output = execute(
            json!({"command": "sleep 30", "timeout": 60}),
            "/tmp",
            &cancel,
        );
        let elapsed = start.elapsed();

        assert!(output.is_error);
        assert!(
            output.content[0].text().contains("cancelled"),
            "output should mention cancellation, got: {}",
            output.content[0].text()
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "pre-cancelled execute should return quickly, took {:?}",
            elapsed
        );
    }

    #[test]
    fn format_output_distinguishes_cancelled_from_timeout() {
        // cancelled=true wins over timed_out=true (user intent signal).
        let out = format_output(String::new(), String::new(), -1, true, true);
        assert!(out.is_error);
        assert!(out.content[0].text().contains("(cancelled)"));
        assert!(!out.content[0].text().contains("(timed out)"));

        // Only timed_out: message says timed out.
        let out = format_output(String::new(), String::new(), -1, true, false);
        assert!(out.is_error);
        assert!(out.content[0].text().contains("(timed out)"));
        assert!(!out.content[0].text().contains("(cancelled)"));

        // Only cancelled: message says cancelled.
        let out = format_output(String::new(), String::new(), -1, false, true);
        assert!(out.is_error);
        assert!(out.content[0].text().contains("(cancelled)"));
    }

    #[test]
    fn execute_tool_dispatch_short_circuits_on_prefire_cancel() {
        // super::super::execute_tool is the entry point used by the worker
        // plugin; if the cancel token is already set before dispatch, it
        // must short-circuit without even invoking the tool.
        use super::super::execute_tool;
        use tau_agent_plugin::ToolCall;

        let tools = super::super::default_tools();
        let cancel = tau_agent_plugin::CancelToken::new();
        cancel.cancel();

        let tc = ToolCall {
            id: "tc-cancelled".into(),
            name: "bash".into(),
            arguments: json!({"command": "echo should-not-run"}),
        };
        let result = execute_tool(&tools, &tc, "/tmp", &cancel);
        assert!(result.is_error);
        let text: String = result
            .content
            .iter()
            .map(|c| c.text().to_string())
            .collect::<Vec<_>>()
            .join("");
        assert!(
            text.contains("cancelled before execution"),
            "expected cancelled-before-execution, got: {}",
            text
        );
    }

    #[test]
    fn read_tool_with_prefire_cancel_returns_error() {
        use super::super::execute_tool;
        use tau_agent_plugin::ToolCall;

        let tools = super::super::default_tools();
        let cancel = tau_agent_plugin::CancelToken::new();
        cancel.cancel();

        let tc = ToolCall {
            id: "tc-read".into(),
            name: "read".into(),
            arguments: json!({"path": "/dev/null"}),
        };
        let result = execute_tool(&tools, &tc, "/tmp", &cancel);
        assert!(result.is_error);
    }
}
