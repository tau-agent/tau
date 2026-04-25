//! Bash tool — execute shell commands.

use std::collections::HashSet;
use std::os::fd::AsFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use nix::fcntl::{FcntlArg, OFlag, fcntl};
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
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

/// Verify that `cwd` exists and is a directory. Returns `None` if the
/// path is usable, or `Some(message)` describing the problem otherwise.
///
/// We surface this as an explicit pre-spawn check so that callers see
/// a message naming the missing path, instead of the bare `ENOENT`
/// that `Command::spawn` returns when either bash *or* `cwd` is
/// missing — the two are indistinguishable at the syscall level.
pub fn check_cwd(cwd: &str) -> Option<String> {
    let path = std::path::Path::new(cwd);
    match std::fs::metadata(path) {
        Ok(m) if m.is_dir() => None,
        Ok(_) => Some(format!(
            "session cwd is not a directory: {} (use /cd <path> to switch to a valid directory)",
            cwd,
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(format!(
            "session cwd no longer exists: {} (was the worktree removed? use /cd <path> to switch to a valid directory)",
            cwd,
        )),
        Err(e) => Some(format!("cannot access session cwd {}: {}", cwd, e,)),
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
///
/// Sets the shared `killed` flag (so reader threads exit promptly) and the
/// returned `timed_out` flag (so `format_output` can label the result).
/// The `done` flag is shared with `start_cancel_watcher` and lets the
/// caller wake any sleeping watcher early once the child has exited
/// normally.
fn start_watchdog(
    child_id: u32,
    timeout_secs: u64,
    killed: Arc<AtomicBool>,
    done: Arc<AtomicBool>,
) -> Arc<AtomicBool> {
    let timed_out = Arc::new(AtomicBool::new(false));
    let flag = timed_out.clone();
    std::thread::spawn(move || {
        // Poll instead of a single long sleep so that a normal exit
        // (`done = true`) wakes us up and we don't fire after the fact.
        let deadline = Instant::now() + Duration::from_secs(timeout_secs);
        loop {
            if done.load(Ordering::Relaxed) {
                return;
            }
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let remaining = deadline - now;
            std::thread::sleep(remaining.min(Duration::from_millis(100)));
        }
        if done.load(Ordering::Relaxed) {
            return;
        }
        flag.store(true, Ordering::Relaxed);
        killed.store(true, Ordering::Relaxed);
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
/// soon as the [`CancelToken`] becomes cancelled.
///
/// Sets the shared `killed` flag (so reader threads exit promptly) and
/// the returned `cancelled` flag (so `format_output` can distinguish
/// cancelled from timed-out). The caller flips `done` to `true` once the
/// child has exited normally so the watcher thread wakes up and stops
/// polling.
///
/// The watcher polls every 100 ms so that cancellation appears "immediate"
/// from the user's perspective without burning CPU on a tight loop.
fn start_cancel_watcher(
    child_id: u32,
    cancel: tau_agent_plugin::CancelToken,
    killed: Arc<AtomicBool>,
    done: Arc<AtomicBool>,
) -> Arc<AtomicBool> {
    let cancelled = Arc::new(AtomicBool::new(false));
    let cancelled_flag = cancelled.clone();
    std::thread::spawn(move || {
        loop {
            if done.load(Ordering::Relaxed) {
                return;
            }
            if cancel.is_cancelled() {
                cancelled_flag.store(true, Ordering::Relaxed);
                killed.store(true, Ordering::Relaxed);
                let _ = signal::killpg(Pid::from_raw(child_id as i32), Signal::SIGKILL);
                untrack_pgid(child_id as i32);
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    });
    cancelled
}

/// Set a file descriptor to non-blocking mode.
///
/// Used on the bash child's stdout/stderr pipes so that our reader
/// threads can wake up periodically and check the `killed` flag instead
/// of blocking forever in `read()` when a descendant escapes the
/// process group and keeps the pipe write-ends open.
fn set_nonblocking<F: AsFd>(fd: &F) -> nix::Result<()> {
    fcntl(fd.as_fd(), FcntlArg::F_SETFL(OFlag::O_NONBLOCK))?;
    Ok(())
}

/// Drain a non-blocking pipe into `collected` (and optionally invoke
/// `on_line` for each newline-terminated chunk), polling at 100 ms ticks
/// and exiting when:
///   * the pipe reaches EOF (read returns `Ok(0)`), or
///   * the `killed` flag is set, in which case we make one final
///     non-blocking drain pass and then return.
///
/// This is the "forcibly unblock the reader" mechanism: when the
/// watchdog or cancel-watcher fires, it sets `killed = true`; the next
/// tick of this loop sees it and returns within ~100 ms regardless of
/// whether the underlying pipe ever reaches EOF (which it might not, if
/// a descendant process escaped the killed process group).
fn drain_pipe<R: std::io::Read + AsFd>(
    mut reader: R,
    killed: &AtomicBool,
    mut on_line: impl FnMut(&str),
) -> String {
    if let Err(e) = set_nonblocking(&reader) {
        // If we can't switch to non-blocking, fall back to a blocking
        // read — still better than nothing. The kill path may then
        // hang, but that's the pre-fix behaviour and only happens if
        // fcntl itself fails, which is essentially impossible for a
        // valid pipe fd.
        let _ = e;
        let mut collected = String::new();
        let _ = std::io::Read::read_to_string(&mut reader, &mut collected);
        for line in collected.split_inclusive('\n') {
            // Strip trailing newline for the on_line callback.
            let trimmed = line.strip_suffix('\n').unwrap_or(line);
            on_line(trimmed);
        }
        return collected;
    }

    let mut collected = String::new();
    let mut line_buf = String::new();
    let mut buf = [0u8; 4096];
    loop {
        // Wait up to 100 ms for data so we can check `killed` regularly.
        let mut pollfd = [PollFd::new(reader.as_fd(), PollFlags::POLLIN)];
        let pr = poll(&mut pollfd, PollTimeout::from(100u8));
        match pr {
            Ok(0) => {
                // Timeout — no data available yet.
                if killed.load(Ordering::Relaxed) {
                    break;
                }
                continue;
            }
            Ok(_) => {}
            Err(nix::errno::Errno::EINTR) => continue,
            Err(_) => break,
        }

        match reader.read(&mut buf) {
            Ok(0) => break, // EOF.
            Ok(n) => {
                let chunk = String::from_utf8_lossy(&buf[..n]);
                collected.push_str(&chunk);
                // Emit complete lines via on_line.
                line_buf.push_str(&chunk);
                while let Some(idx) = line_buf.find('\n') {
                    let line: String = line_buf.drain(..=idx).collect();
                    let trimmed = line.strip_suffix('\n').unwrap_or(&line);
                    on_line(trimmed);
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if killed.load(Ordering::Relaxed) {
                    break;
                }
                continue;
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }

    // Flush any final partial line that didn't end with '\n'.
    if !line_buf.is_empty() {
        on_line(&line_buf);
    }

    collected
}

/// Join a thread with a small grace period.
///
/// Used after timeout/cancel fires: we've signalled the reader threads
/// via the `killed` flag and they should exit within ~100 ms (one poll
/// tick). If a reader is still stuck after the grace period — e.g. a
/// descendant escaped the pgid AND is actively writing fast enough that
/// `read()` keeps returning data — we give up and detach the thread
/// rather than hang the whole tool call. A leaked reader thread is
/// acceptable; hanging the tool call is not.
fn join_with_grace<T: Send + 'static>(
    handle: std::thread::JoinHandle<T>,
    grace: Duration,
) -> Option<T> {
    // std::thread doesn't have join_timeout, so we poll `is_finished`.
    let deadline = Instant::now() + grace;
    while Instant::now() < deadline {
        if handle.is_finished() {
            return handle.join().ok();
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    if handle.is_finished() {
        handle.join().ok()
    } else {
        // Detach: dropping the JoinHandle leaves the thread running.
        None
    }
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
    run(args, cwd, cancel, Some(&mut on_delta))
}

fn execute(
    args: serde_json::Value,
    cwd: &str,
    cancel: &tau_agent_plugin::CancelToken,
) -> ToolOutput {
    run(&args, cwd, cancel, None)
}

/// Shared implementation backing both [`execute`] and [`execute_streaming`].
///
/// The two paths used to be near-identical copies; merging them ensures
/// the timeout-and-cancel-and-don't-hang-on-escaped-descendants logic
/// stays consistent between them.
fn run(
    args: &serde_json::Value,
    cwd: &str,
    cancel: &tau_agent_plugin::CancelToken,
    mut on_delta: Option<&mut dyn FnMut(&str)>,
) -> ToolOutput {
    let Some(command) = args.get("command").and_then(|c| c.as_str()) else {
        return ToolOutput::error("missing 'command' argument");
    };

    let timeout_secs = args.get("timeout").and_then(|t| t.as_u64()).unwrap_or(120);

    // Disambiguate ENOENT-from-missing-cwd from ENOENT-from-missing-bash
    // by checking the cwd up front. Without this, a deleted session cwd
    // (e.g. a removed worktree) surfaces as a bare
    // "failed to execute command: No such file or directory (os error 2)"
    // which doesn't say *which* path is missing.
    if let Some(err) = check_cwd(cwd) {
        return ToolOutput::error(err);
    }

    let mut child = match spawn_child(command, cwd) {
        Ok(c) => c,
        Err(e) => return ToolOutput::error(format!("failed to execute command: {}", e)),
    };

    let pgid = child.id() as i32;
    track_pgid(pgid);

    // Shared flags coordinating the watchdog, cancel-watcher and reader
    // threads. `killed` is set by whichever watcher fires; reader
    // threads check it on each poll tick and exit promptly even when
    // the underlying pipes never reach EOF (the bug we're fixing).
    let killed = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicBool::new(false));

    let timed_out = start_watchdog(child.id(), timeout_secs, killed.clone(), done.clone());
    let cancelled = start_cancel_watcher(child.id(), cancel.clone(), killed.clone(), done.clone());

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    // Read stderr on a dedicated thread; stdout is read on the calling
    // thread so that the streaming `on_delta` callback can borrow non-
    // `Send` / non-`'static` state (typical for callers that want to
    // accumulate deltas into a local `Vec`).
    //
    // Both reads go through `drain_pipe`, which polls the pipe in
    // 100 ms ticks. When the watchdog or cancel-watcher fires (or once
    // the bash child exits) `killed` is set and the next tick bails
    // out promptly even if the pipe never reaches EOF — the failure
    // mode we're fixing, where a daemonised descendant inherits the
    // pipe write end and stays alive after bash itself is gone.
    let stderr_killed = killed.clone();
    let stderr_handle = std::thread::spawn(move || match stderr {
        Some(stderr) => drain_pipe(stderr, &stderr_killed, |_| {}),
        None => String::new(),
    });

    // Read stdout on a thread *only when not streaming*, so we can use
    // `child.wait()` afterwards without blocking on a possibly-stuck
    // pipe. In the streaming case we're already on the right thread to
    // drain it directly; we just have to keep an eye on `killed`.
    //
    // We start a small companion thread that wakes us up after the
    // child exits (by setting `killed`) so the streaming reader doesn't
    // need to block forever waiting for EOF.
    let waiter_killed = killed.clone();
    let waiter_done = done.clone();
    let child_pid = child.id();
    let waiter_handle = std::thread::spawn(move || {
        // Poll for child exit at 50 ms intervals; we don't actually
        // reap here — the calling thread does that via `child.wait()` —
        // we just observe the exit so the reader knows EOF is coming
        // (or never coming, if a descendant escaped the pgid).
        loop {
            if waiter_done.load(Ordering::Relaxed) {
                return;
            }
            // Use waitid(WNOHANG | WNOWAIT) to peek at the child's
            // status without consuming it; that way `child.wait()` on
            // the calling thread still works.
            let pid = nix::unistd::Pid::from_raw(child_pid as i32);
            match nix::sys::wait::waitpid(
                pid,
                Some(nix::sys::wait::WaitPidFlag::WNOHANG | nix::sys::wait::WaitPidFlag::WNOWAIT),
            ) {
                Ok(nix::sys::wait::WaitStatus::StillAlive) => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                Ok(_) => {
                    // Child has exited (or been killed). Wake the
                    // reader so it stops blocking on a pipe that
                    // may never reach EOF.
                    waiter_killed.store(true, Ordering::Relaxed);
                    return;
                }
                Err(_) => return,
            }
        }
    });

    let collected_stdout = match stdout {
        Some(stdout) => drain_pipe(stdout, &killed, |line| {
            if let Some(cb) = on_delta.as_mut() {
                cb(line);
            }
        }),
        None => String::new(),
    };

    // Wait for the direct bash child to exit. `child.wait()` only waits
    // on the direct child, so it returns as soon as bash exits (or is
    // killed by the watchdog / cancel-watcher); it does *not* wait for
    // escaped descendants. By this point the stdout reader has already
    // returned (either via EOF, the child-exit waiter setting `killed`,
    // or a watchdog/cancel-watcher setting `killed`).
    let status = child.wait();
    let exit_code = status.ok().and_then(|s| s.code()).unwrap_or(-1);
    untrack_pgid(pgid);
    // Tell stderr reader to wrap up.
    killed.store(true, Ordering::Relaxed);
    // Wake the watchdog, cancel-watcher and child-exit waiter so they
    // don't fire after the fact (and so they exit promptly rather than
    // sleeping out their remaining tick).
    done.store(true, Ordering::Relaxed);

    // Give the stderr reader a short grace period to finish draining.
    // If it's still stuck after that (e.g. an escaped descendant is
    // actively writing) we detach it and move on — leaking a reader
    // thread is preferable to hanging the whole tool call.
    let collected_stderr =
        join_with_grace(stderr_handle, Duration::from_millis(500)).unwrap_or_default();
    // The waiter thread checks `done` every 50 ms so it'll exit on
    // the next tick; join with a small grace period to avoid leaking it.
    let _ = join_with_grace(waiter_handle, Duration::from_millis(200));

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
    fn bash_missing_cwd_returns_clear_error() {
        let _guard = lock_registry();
        // A path that almost certainly doesn't exist.
        let bogus = format!("/tmp/tau-bash-bogus-cwd-{}", std::process::id());
        // Make sure it really doesn't exist.
        let _ = std::fs::remove_dir_all(&bogus);
        assert!(!std::path::Path::new(&bogus).exists());

        let output = execute(
            json!({"command": "echo hi"}),
            &bogus,
            &tau_agent_plugin::CancelToken::new(),
        );

        assert!(output.is_error, "missing cwd should be an error");
        let text = output.content[0].text();
        assert!(
            text.contains(&bogus),
            "error message should name the missing cwd, got: {}",
            text,
        );
        assert!(
            text.contains("no longer exists"),
            "error message should mention the cwd is gone, got: {}",
            text,
        );
        assert!(
            !text.contains("os error 2"),
            "raw ENOENT should not leak through, got: {}",
            text,
        );
    }

    #[test]
    fn bash_cwd_is_file_not_directory_returns_clear_error() {
        let _guard = lock_registry();
        // Use any regular file that exists — /etc/hostname is universal
        // on Linux test hosts; fall back to the binary's own path if
        // somehow missing.
        let cwd = if std::path::Path::new("/etc/hostname").is_file() {
            "/etc/hostname".to_string()
        } else {
            std::env::current_exe()
                .expect("current_exe")
                .to_string_lossy()
                .into_owned()
        };

        let output = execute(
            json!({"command": "echo hi"}),
            &cwd,
            &tau_agent_plugin::CancelToken::new(),
        );

        assert!(output.is_error, "non-directory cwd should be an error");
        let text = output.content[0].text();
        assert!(
            text.contains(&cwd),
            "error message should name the bad cwd, got: {}",
            text,
        );
        assert!(
            text.contains("not a directory"),
            "error should mention not-a-directory, got: {}",
            text,
        );
        assert!(
            !text.contains("os error"),
            "raw OS error should not leak, got: {}",
            text,
        );
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

    /// Cleanup helper for the daemonised-child tests.
    ///
    /// `setsid -f sleep 60` deliberately escapes the bash process group
    /// so the tool's `killpg` doesn't reach it. We then kill it by hand
    /// via `pkill` on a unique marker so leaked sleepers don't poison
    /// sibling tests.
    fn pkill_marker(marker: &str) {
        // Best-effort: the process may have already been killed.
        let _ = std::process::Command::new("pkill")
            .args(["-f", marker])
            .status();
    }

    #[test]
    fn bash_timeout_with_daemonized_child_does_not_hang() {
        let _guard = lock_registry();
        // Unique marker so we can kill exactly our sleeper afterwards.
        let marker = format!("tau-bash-test-daemon-timeout-{}", std::process::id());
        let cmd = format!("setsid -f sleep 60 # {}", marker);

        let start = Instant::now();
        let output = execute(
            json!({"command": cmd, "timeout": 2}),
            "/tmp",
            &tau_agent_plugin::CancelToken::new(),
        );
        let elapsed = start.elapsed();
        pkill_marker(&marker);

        assert!(
            output.is_error,
            "escaped-descendant timeout must still be an error"
        );
        assert!(
            output.content[0].text().contains("timed out"),
            "output should indicate timeout: {}",
            output.content[0].text()
        );
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "escaped-descendant timeout must not hang on the pipe; took {:?}",
            elapsed
        );
    }

    #[test]
    fn bash_streaming_timeout_with_daemonized_child_does_not_hang() {
        let _guard = lock_registry();
        let marker = format!("tau-bash-test-daemon-stream-timeout-{}", std::process::id());
        let cmd = format!("setsid -f sleep 60 # {}", marker);

        let start = Instant::now();
        let mut deltas = Vec::new();
        let output = execute_streaming(
            &json!({"command": cmd, "timeout": 2}),
            "/tmp",
            &tau_agent_plugin::CancelToken::new(),
            |line| deltas.push(line.to_string()),
        );
        let elapsed = start.elapsed();
        pkill_marker(&marker);

        assert!(output.is_error);
        assert!(
            output.content[0].text().contains("timed out"),
            "output should indicate timeout: {}",
            output.content[0].text()
        );
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "escaped-descendant streaming timeout must not hang; took {:?}",
            elapsed
        );
    }

    #[test]
    fn bash_cancel_with_daemonized_child_does_not_hang() {
        let _guard = lock_registry();
        let marker = format!("tau-bash-test-daemon-cancel-{}", std::process::id());
        let cmd = format!("setsid -f sleep 60 # {}", marker);

        let cancel = tau_agent_plugin::CancelToken::new();
        let cancel_clone = cancel.clone();
        let flipper = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(150));
            cancel_clone.cancel();
        });

        let start = Instant::now();
        let output = execute(json!({"command": cmd, "timeout": 60}), "/tmp", &cancel);
        let elapsed = start.elapsed();
        flipper.join().expect("flipper thread panicked");
        pkill_marker(&marker);

        assert!(output.is_error);
        assert!(
            output.content[0].text().contains("cancelled"),
            "output should indicate cancellation: {}",
            output.content[0].text()
        );
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "escaped-descendant cancel must not hang; took {:?}",
            elapsed
        );
    }

    #[test]
    fn bash_streaming_cancel_with_daemonized_child_does_not_hang() {
        let _guard = lock_registry();
        let marker = format!("tau-bash-test-daemon-stream-cancel-{}", std::process::id());
        let cmd = format!("setsid -f sleep 60 # {}", marker);

        let cancel = tau_agent_plugin::CancelToken::new();
        let cancel_clone = cancel.clone();
        let flipper = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(150));
            cancel_clone.cancel();
        });

        let start = Instant::now();
        let mut deltas = Vec::new();
        let output = execute_streaming(
            &json!({"command": cmd, "timeout": 60}),
            "/tmp",
            &cancel,
            |line| deltas.push(line.to_string()),
        );
        let elapsed = start.elapsed();
        flipper.join().expect("flipper thread panicked");
        pkill_marker(&marker);

        assert!(output.is_error);
        assert!(
            output.content[0].text().contains("cancelled"),
            "output should indicate cancellation: {}",
            output.content[0].text()
        );
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "escaped-descendant streaming cancel must not hang; took {:?}",
            elapsed
        );
    }
}
