//! Merge queue for the task system.
//!
//! Processes `merging` tasks: rebases onto the merge target, runs the project
//! checklist, and performs a fast-forward merge.
//!
//! # Session roles and post-merge cleanup
//!
//! Sessions are tracked against a task in the `task_sessions` table with a
//! `role` column. The roles currently in use (grep for `record_session(` to
//! audit) are:
//!
//! | Role          | Spawned by task system? | Archive on merge? |
//! |---------------|-------------------------|-------------------|
//! | `worker`      | yes (dispatch)          | yes               |
//! | `planner`     | yes (dispatch_planning) | yes               |
//! | `reviewer`    | yes (dispatch_review)   | yes               |
//! | `refiner`     | yes (dispatch_refining) | yes               |
//! | `log`         | yes (merge itself)      | yes               |
//! | `interactive` | yes, but user-driven    | no (user may still be using it) |
//! | `creator`     | no — orchestrator ref   | no                |
//! | `contributor` | no — orchestrator ref   | no                |
//!
//! Only roles listed in [`ARCHIVABLE_ROLES`] are archived when a task merges.
//! This prevents the long-lived orchestrator/user sessions that merely
//! *created* or *commented on* a task from being archived as a side-effect
//! of its merge. See [`sessions_to_archive`] for the filter and the unit
//! tests below.

use std::io::{BufRead, Write};

use serde::Deserialize;
use serde_json::json;

use crate::plugin::{PluginMessage, PluginRequest};
use crate::protocol::{Request, Response};
use crate::tasks_db::{TaskSession, TasksDb};

/// Session roles that should be archived when a task merges.
///
/// Only sessions whose role is in this list are archived by [`merge_task`]'s
/// cleanup step. Roles outside this list (`creator`, `contributor`,
/// `interactive`, and anything unknown) are preserved so that orchestrator
/// and user sessions are not clobbered by a subtask merge.
///
/// If a new task-spawned role is introduced, add it to this list explicitly
/// rather than relying on a deny-list — unknown roles are preserved by
/// default.
pub const ARCHIVABLE_ROLES: &[&str] = &["worker", "planner", "reviewer", "refiner", "log"];

/// Partition a list of task sessions into (to-archive, to-skip) based on
/// [`ARCHIVABLE_ROLES`]. Pure function, trivially testable.
///
/// The first tuple element contains sessions whose role is archivable; the
/// second contains sessions that should be left alone (orchestrator,
/// interactive, contributor, creator, etc.).
pub fn sessions_to_archive(sessions: &[TaskSession]) -> (Vec<&TaskSession>, Vec<&TaskSession>) {
    let mut archive = Vec::new();
    let mut skip = Vec::new();
    for ts in sessions {
        if ARCHIVABLE_ROLES.contains(&ts.role.as_str()) {
            archive.push(ts);
        } else {
            skip.push(ts);
        }
    }
    (archive, skip)
}

// ---------------------------------------------------------------------------
// Checklist
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct Checklist {
    #[serde(default)]
    pub check: Vec<CheckItem>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CheckItem {
    pub name: String,
    pub command: String,
}

/// Load the project checklist from `{project_dir}/.tau/checklist.toml`.
/// Returns an empty vec if the file doesn't exist or can't be parsed.
pub fn load_checklist(project_dir: &str) -> Vec<CheckItem> {
    let path = std::path::Path::new(project_dir)
        .join(".tau")
        .join("checklist.toml");
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    match toml::from_str::<Checklist>(&content) {
        Ok(cl) => cl.check,
        Err(e) => {
            eprintln!("tasks merge: failed to parse checklist: {}", e);
            Vec::new()
        }
    }
}

// ---------------------------------------------------------------------------
// Merge result
// ---------------------------------------------------------------------------

#[derive(Debug, serde::Serialize)]
pub struct MergeResult {
    pub success: bool,
    pub log: String,
}

// ---------------------------------------------------------------------------
// ServerRequest tunnel helpers (same pattern as tasks_scheduler)
// ---------------------------------------------------------------------------

fn send_message(writer: &mut impl Write, msg: &PluginMessage) {
    if let Ok(mut line) = serde_json::to_string(msg) {
        line.push('\n');
        let _ = writer.write_all(line.as_bytes());
        let _ = writer.flush();
    }
}

/// Send a ServerRequest via plugin protocol and wait for the ServerResponse.
///
/// While waiting, any `ToolCall` messages that arrive on stdin are
/// **immediately answered with an error** so that the calling session does
/// not hang.  See the identical comment in `tasks_scheduler::server_request`
/// for the full rationale.
fn server_request(
    writer: &mut impl Write,
    reader: &mut impl BufRead,
    request: Request,
) -> crate::Result<Response> {
    let request_id = format!("merge-sr-{}", crate::types::timestamp_ms());
    send_message(
        writer,
        &PluginMessage::ServerRequest {
            request_id: request_id.clone(),
            request,
        },
    );

    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => {
                return Err(crate::Error::Io(
                    "stdin closed while waiting for server response".into(),
                ));
            }
            Ok(_) => {}
            Err(e) => {
                return Err(crate::Error::Io(format!("read error: {}", e)));
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
            // A concurrent ToolCall arrived while we are mid-ServerRequest.
            // Answer immediately with an error so the session is not left
            // hanging in "running tools".
            PluginRequest::ToolCall { tool_call_id, .. } => {
                send_message(
                    writer,
                    &PluginMessage::ToolResult(crate::plugin::PluginToolResult {
                        tool_call_id,
                        content: vec![crate::types::ToolResultContent::Text(
                            crate::types::TextContent {
                                text: "tasks plugin is busy with a background merge pass \
                                       — please retry in a moment"
                                    .into(),
                                text_signature: None,
                            },
                        )],
                        is_error: true,
                    }),
                );
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Execute a bash command via the log session
// ---------------------------------------------------------------------------

/// Run a bash command via ExecuteTool on the given session.
/// Returns (stdout text, is_error).
fn execute_bash(
    writer: &mut impl Write,
    reader: &mut impl BufRead,
    session_id: &str,
    command: &str,
) -> crate::Result<(String, bool)> {
    let resp = server_request(
        writer,
        reader,
        Request::ExecuteTool {
            session_id: session_id.to_string(),
            tool_name: "bash".into(),
            arguments: json!({ "command": command }),
        },
    )?;

    match resp {
        Response::ToolExecuted { content, is_error } => Ok((content, is_error)),
        Response::Error { message } => {
            Err(crate::Error::Io(format!("ExecuteTool error: {}", message)))
        }
        other => Err(crate::Error::Io(format!(
            "unexpected ExecuteTool response: {:?}",
            other
        ))),
    }
}

// ---------------------------------------------------------------------------
// Merge execution
// ---------------------------------------------------------------------------

/// Execute the merge sequence for a task.
///
/// The task must already be in `merging` state with a worktree.
/// Creates a log session via ServerRequest, rebases, runs the checklist,
/// and fast-forward merges into the merge target.
pub fn merge_task(
    db: &TasksDb,
    task_id: i64,
    project_dir: &str,
    writer: &mut impl Write,
    reader: &mut impl BufRead,
) -> crate::Result<MergeResult> {
    // 1. Get task, branch, merge target
    let task = db
        .get_task(task_id)?
        .ok_or_else(|| crate::Error::Io(format!("task {} not found", task_id)))?;

    if task.state != "merging" {
        return Err(crate::Error::Io(format!(
            "task {} is in state '{}', must be 'merging'",
            task_id, task.state
        )));
    }

    let branch = task
        .branch
        .as_ref()
        .ok_or_else(|| crate::Error::Io(format!("task {} has no branch set", task_id)))?;

    let worktree_path = task
        .worktree_path
        .as_ref()
        .ok_or_else(|| crate::Error::Io(format!("task {} has no worktree", task_id)))?;

    let merge_target = db.get_merge_target(task_id)?;

    let mut log = String::new();

    // 3. Create a log-provider session
    let log_session = match server_request(
        writer,
        reader,
        Request::CreateSession {
            model: Some("log".into()),
            provider: None,
            system_prompt: None,
            cwd: Some(worktree_path.clone()),
            parent_id: None,
            child_budget: 0,
            tagline: Some(format!("Merge task {}", task_id)),
            auto_archive: false,
            notify_parent: false,
        },
    )? {
        Response::SessionCreated { session_id } => session_id,
        Response::Error { message } => {
            return Ok(MergeResult {
                success: false,
                log: format!("Failed to create log session: {}", message),
            });
        }
        other => {
            return Ok(MergeResult {
                success: false,
                log: format!("Unexpected response creating log session: {:?}", other),
            });
        }
    };

    // 3b. Check that the main worktree is clean before merging.
    //
    // If there are uncommitted changes, the post-merge `git reset --hard HEAD`
    // would clobber them — fail early with a clear error instead.
    let (wt_status, is_error) = execute_bash(
        writer,
        reader,
        &log_session,
        &format!(
            "git -C '{}' diff --quiet HEAD && git -C '{}' diff --cached --quiet HEAD",
            project_dir, project_dir
        ),
    )?;
    if is_error {
        archive_session(writer, reader, &log_session);
        return Ok(MergeResult {
            success: false,
            log: format!(
                "Main worktree has uncommitted changes — refusing to merge:\n{}",
                wt_status
            ),
        });
    }

    // 4. Rebase onto merge target
    //
    // Set GIT_EDITOR and GIT_SEQUENCE_EDITOR to `true` so git never opens an
    // interactive editor (which would hang indefinitely in this headless context).
    log.push_str(&format!("=== Rebase onto {} ===\n", merge_target));
    let rebase_cmd = format!(
        "GIT_EDITOR=true GIT_SEQUENCE_EDITOR=true \
         git -c advice.resolveConflict=false rebase {}",
        merge_target,
    );
    let (output, is_error) = execute_bash(writer, reader, &log_session, &rebase_cmd)?;
    log.push_str(&output);
    log.push('\n');

    if is_error {
        // Abort the rebase so we leave a clean state
        let _ = execute_bash(writer, reader, &log_session, "git rebase --abort");
        archive_session(writer, reader, &log_session);
        return Ok(MergeResult {
            success: false,
            log: format!("Rebase failed:\n{}", log),
        });
    }

    // 5. Run checklist
    let checklist = load_checklist(project_dir);
    for item in &checklist {
        log.push_str(&format!("=== Check: {} ===\n", item.name));
        let (output, is_error) = execute_bash(writer, reader, &log_session, &item.command)?;
        log.push_str(&output);
        log.push('\n');

        if is_error {
            archive_session(writer, reader, &log_session);
            return Ok(MergeResult {
                success: false,
                log: format!("Checklist '{}' failed:\n{}", item.name, log),
            });
        }
    }

    // 6. Fast-forward merge using update-ref (worktree-safe)
    //
    // We can't `git checkout <target> && git merge` because the target
    // branch may be checked out in another worktree. Instead, verify the
    // fast-forward condition and update the ref directly.
    log.push_str(&format!("=== Merge {} into {} ===\n", branch, merge_target));

    // Verify fast-forward: merge_target must be an ancestor of branch
    let (output, is_error) = execute_bash(
        writer,
        reader,
        &log_session,
        &format!(
            "git merge-base --is-ancestor {} {} && git update-ref refs/heads/{} $(git rev-parse {})",
            merge_target, branch, merge_target, branch
        ),
    )?;
    log.push_str(&output);
    log.push('\n');

    if is_error {
        archive_session(writer, reader, &log_session);
        return Ok(MergeResult {
            success: false,
            log: format!("Merge failed:\n{}", log),
        });
    }

    // 6b. Sync the main worktree's index + working tree after update-ref.
    //
    // `git update-ref` only moves the ref — it does NOT touch the index or
    // working tree of any worktree that has that branch checked out. When
    // the merge target (typically `main`) is checked out in the main worktree,
    // the index becomes stale and `git status` shows a phantom staged diff
    // that reverts the merge. Fix this by running `git reset --hard HEAD` in
    // the main worktree, but only when its HEAD branch matches the merge target.
    let (main_head, _) = execute_bash(
        writer,
        reader,
        &log_session,
        &format!("git -C '{}' rev-parse --abbrev-ref HEAD", project_dir),
    )?;
    if main_head.trim() == merge_target {
        log.push_str("=== Sync main worktree index ===\n");
        let (output, _) = execute_bash(
            writer,
            reader,
            &log_session,
            &format!("git -C '{}' reset --hard HEAD", project_dir),
        )?;
        log.push_str(&output);
        log.push('\n');
    }

    // 7. Clean up: remove worktree, delete branch, archive session, clear DB
    log.push_str("=== Cleanup ===\n");

    // 7a. Remove the git worktree (but never the main worktree)
    let wt_is_main = std::fs::canonicalize(worktree_path)
        .and_then(|wt| std::fs::canonicalize(project_dir).map(|pd| wt == pd))
        .unwrap_or(false);
    if wt_is_main {
        let msg = format!(
            "refusing to remove main worktree for task {}: {} is the repo root\n",
            task_id, worktree_path
        );
        eprintln!("tasks: warning: {}", msg.trim());
        log.push_str(&msg);
    } else {
        let (output, wt_err) = execute_bash(
            writer,
            reader,
            &log_session,
            &format!(
                "cd $(git rev-parse --show-toplevel) && git worktree remove --force {}",
                worktree_path
            ),
        )?;
        log.push_str(&output);
        if wt_err {
            eprintln!(
                "tasks: warning: failed to remove worktree for task {}: {}",
                task_id,
                output.trim()
            );
        }
    }
    let _ = db.clear_worktree(task_id);

    // 7b. Delete the task branch (no longer needed after merge)
    let (output, br_err) = execute_bash(
        writer,
        reader,
        &log_session,
        &format!("git branch -D {}", branch),
    )?;
    log.push_str(&output);
    if br_err {
        eprintln!(
            "tasks: warning: failed to delete branch for task {}: {}",
            task_id,
            output.trim()
        );
    }

    // 7c. Archive task-spawned sessions (worker, planner, reviewer, refiner,
    // log). Sessions with roles like `contributor`, `creator`, or
    // `interactive` are the orchestrator/user sessions that merely referenced
    // the task — archiving them as a side-effect of a merge would rip them
    // out from under the user. See `ARCHIVABLE_ROLES` and
    // `sessions_to_archive` at the top of this module.
    //
    // We deliberately do NOT fall back to archiving `task.session_id` when
    // it isn't present in `task_sessions`: every codepath that sets
    // `task.session_id` also records an entry in `task_sessions` with an
    // explicit role (`assign_task` records `worker`, interactive creation
    // records `interactive`, etc.), so a `task.session_id` missing from
    // `task_sessions` means we have no role information and cannot safely
    // decide to archive.
    if let Ok(sessions) = db.get_sessions(task_id) {
        let (to_archive, to_skip) = sessions_to_archive(&sessions);
        for ts in to_archive {
            archive_session(writer, reader, &ts.session_id);
            log.push_str(&format!("Archived {} session {}\n", ts.role, ts.session_id));
        }
        for ts in to_skip {
            log.push_str(&format!(
                "Preserved {} session {} (role not in archive list)\n",
                ts.role, ts.session_id
            ));
        }
    }

    // 8. Archive the log session
    archive_session(writer, reader, &log_session);

    log.push_str("=== Merge complete ===\n");

    Ok(MergeResult { success: true, log })
}

/// Archive a session (best-effort, errors are ignored).
fn archive_session(writer: &mut impl Write, reader: &mut impl BufRead, session_id: &str) {
    let _ = server_request(
        writer,
        reader,
        Request::ArchiveSession {
            session_id: session_id.to_string(),
            require_ancestor: None,
        },
    );
}

// ---------------------------------------------------------------------------
// Parent notification
// ---------------------------------------------------------------------------

/// After a subtask merges successfully, check if all sibling subtasks under
/// the same parent are in a terminal state (`merged` or `closed`). If so, add a message to the parent and
/// optionally notify its session.
pub fn notify_parent_if_all_done(
    db: &TasksDb,
    task_id: i64,
    writer: &mut impl Write,
    reader: &mut impl BufRead,
) -> crate::Result<()> {
    let task = db
        .get_task(task_id)?
        .ok_or_else(|| crate::Error::Io(format!("task {} not found", task_id)))?;

    let parent_id = match task.parent_id {
        Some(pid) => pid,
        None => return Ok(()), // root task, nothing to notify
    };

    let parent = match db.get_task(parent_id)? {
        Some(p) => p,
        None => return Ok(()),
    };

    // Check if all sibling subtasks are in a terminal state
    let subtasks = db.get_subtasks(parent_id)?;
    let all_done = !subtasks.is_empty()
        && subtasks
            .iter()
            .all(|t| t.state == "merged" || t.state == "closed");

    if !all_done {
        return Ok(());
    }

    // All subtasks in terminal state — notify parent
    let parent_branch = parent.branch.as_deref().unwrap_or("main");
    let msg = format!(
        "All subtasks completed and merged into branch {}.",
        parent_branch
    );
    let _ = db.add_message(parent_id, &msg, Some("system"));

    // If parent has a session_id, send QueueMessage to notify the agent
    if let Some(ref session_id) = parent.session_id {
        let _ = server_request(
            writer,
            reader,
            Request::QueueMessage {
                target_session_id: session_id.clone(),
                content: msg,
                sender_info: format!("task-system (task {})", task_id),
                await_reply: false,
                reply_to: None,
            },
        );
    }

    Ok(())
}

/// Notify the parent task's session that an individual subtask has completed.
/// This fires for each subtask completion (unlike `notify_parent_if_all_done`
/// which only fires when ALL subtasks reach a terminal state). Best-effort — errors are
/// logged but don't affect the caller.
pub fn notify_parent_of_subtask_done(
    db: &TasksDb,
    task_id: i64,
    writer: &mut impl Write,
    reader: &mut impl BufRead,
) {
    let task = match db.get_task(task_id) {
        Ok(Some(t)) => t,
        _ => return,
    };

    let parent_id = match task.parent_id {
        Some(pid) => pid,
        None => return, // root task, nothing to notify
    };

    let parent = match db.get_task(parent_id) {
        Ok(Some(p)) => p,
        _ => return,
    };

    // Only notify if the parent has an active session
    if let Some(ref session_id) = parent.session_id {
        let content = format!("✓ Subtask #{} {}: {}", task_id, task.state, task.title);
        let _ = server_request(
            writer,
            reader,
            Request::QueueMessage {
                target_session_id: session_id.clone(),
                content,
                sender_info: format!("task-system (task {})", task_id),
                await_reply: false,
                reply_to: None,
            },
        );
    }
}

/// Send a QueueMessage to notify a session about merge failure.
/// Best-effort — errors are ignored.
pub fn notify_session_of_merge_failure(
    session_id: &str,
    task_id: i64,
    log: &str,
    writer: &mut impl Write,
    reader: &mut impl BufRead,
) {
    let content = format!(
        "Merge for task {} failed. The task has been moved back to active state so you can fix the issue and retry.\n\n{}",
        task_id, log
    );
    let _ = server_request(
        writer,
        reader,
        Request::QueueMessage {
            target_session_id: session_id.to_string(),
            content,
            sender_info: format!("task-system (merge task {})", task_id),
            await_reply: false,
            reply_to: None,
        },
    );
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks_db::{TaskSession, TaskUpdate, TasksDb};

    // ----- archive role filter -----

    fn ts(session_id: &str, role: &str) -> TaskSession {
        TaskSession {
            task_id: 1,
            session_id: session_id.into(),
            role: role.into(),
            created_at: 0,
        }
    }

    #[test]
    fn test_sessions_to_archive_allows_task_spawned_roles() {
        let sessions = vec![
            ts("s-worker", "worker"),
            ts("s-planner", "planner"),
            ts("s-reviewer", "reviewer"),
            ts("s-refiner", "refiner"),
            ts("s-log", "log"),
        ];
        let (to_archive, to_skip) = sessions_to_archive(&sessions);
        assert_eq!(to_archive.len(), 5);
        assert!(to_skip.is_empty());
    }

    #[test]
    fn test_sessions_to_archive_skips_orchestrator_and_user_roles() {
        let sessions = vec![
            ts("s-creator", "creator"),
            ts("s-contributor", "contributor"),
            ts("s-interactive", "interactive"),
        ];
        let (to_archive, to_skip) = sessions_to_archive(&sessions);
        assert!(
            to_archive.is_empty(),
            "creator/contributor/interactive must never be archived on merge"
        );
        assert_eq!(to_skip.len(), 3);
    }

    #[test]
    fn test_sessions_to_archive_mixed() {
        // Reproduces the s560 bug scenario: the orchestrator session is
        // recorded as `contributor` against the task because it created it,
        // and must survive the merge. The task's worker session should be
        // archived as usual.
        let sessions = vec![
            ts("s-worker", "worker"),
            ts("s560", "contributor"),
            ts("s-reviewer", "reviewer"),
            ts("s-human", "creator"),
        ];
        let (to_archive, to_skip) = sessions_to_archive(&sessions);

        let archived_ids: Vec<&str> = to_archive.iter().map(|t| t.session_id.as_str()).collect();
        assert_eq!(archived_ids, vec!["s-worker", "s-reviewer"]);

        let skipped_ids: Vec<&str> = to_skip.iter().map(|t| t.session_id.as_str()).collect();
        assert_eq!(skipped_ids, vec!["s560", "s-human"]);
    }

    #[test]
    fn test_sessions_to_archive_unknown_roles_are_preserved() {
        // New roles default to preserved — forces an explicit ARCHIVABLE_ROLES
        // update whenever a task-spawned role is introduced.
        let sessions = vec![ts("s-new", "some-future-role")];
        let (to_archive, to_skip) = sessions_to_archive(&sessions);
        assert!(to_archive.is_empty());
        assert_eq!(to_skip.len(), 1);
        assert_eq!(to_skip[0].role, "some-future-role");
    }

    #[test]
    fn test_archivable_roles_constant() {
        // Guard against accidental reordering/removal. If this test needs to
        // change, audit `record_session(` call sites first.
        assert_eq!(
            ARCHIVABLE_ROLES,
            &["worker", "planner", "reviewer", "refiner", "log"]
        );
    }

    /// End-to-end DB-level check: record sessions with different roles and
    /// confirm `sessions_to_archive` keeps the orchestrator sessions.
    #[test]
    fn test_sessions_to_archive_via_db() {
        let db = TasksDb::open_memory().unwrap();
        let task = db
            .create_task("/project", "T", None, None, None, true, false, false)
            .unwrap();
        db.record_session(task.id, "s-worker", "worker").unwrap();
        db.record_session(task.id, "s-reviewer", "reviewer")
            .unwrap();
        db.record_session(task.id, "s-orchestrator", "contributor")
            .unwrap();
        db.record_session(task.id, "s-human", "creator").unwrap();

        let sessions = db.get_sessions(task.id).unwrap();
        let (to_archive, to_skip) = sessions_to_archive(&sessions);

        let archived: Vec<&str> = to_archive.iter().map(|t| t.session_id.as_str()).collect();
        assert!(archived.contains(&"s-worker"));
        assert!(archived.contains(&"s-reviewer"));
        assert!(!archived.contains(&"s-orchestrator"));
        assert!(!archived.contains(&"s-human"));

        let skipped: Vec<&str> = to_skip.iter().map(|t| t.session_id.as_str()).collect();
        assert!(skipped.contains(&"s-orchestrator"));
        assert!(skipped.contains(&"s-human"));
    }

    // ----- checklist parsing -----

    #[test]
    fn test_load_checklist_valid_toml() {
        let dir = tempfile::TempDir::new().unwrap();
        let tau_dir = dir.path().join(".tau");
        std::fs::create_dir_all(&tau_dir).unwrap();
        std::fs::write(
            tau_dir.join("checklist.toml"),
            r#"
[[check]]
name = "fmt"
command = "cargo fmt --check"

[[check]]
name = "clippy"
command = "cargo clippy -- -D warnings"

[[check]]
name = "test"
command = "cargo test"
"#,
        )
        .unwrap();

        let items = load_checklist(dir.path().to_str().unwrap());
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].name, "fmt");
        assert_eq!(items[0].command, "cargo fmt --check");
        assert_eq!(items[1].name, "clippy");
        assert_eq!(items[2].name, "test");
        assert_eq!(items[2].command, "cargo test");
    }

    #[test]
    fn test_load_checklist_missing_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let items = load_checklist(dir.path().to_str().unwrap());
        assert!(items.is_empty());
    }

    #[test]
    fn test_load_checklist_empty_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let tau_dir = dir.path().join(".tau");
        std::fs::create_dir_all(&tau_dir).unwrap();
        std::fs::write(tau_dir.join("checklist.toml"), "").unwrap();

        let items = load_checklist(dir.path().to_str().unwrap());
        assert!(items.is_empty());
    }

    #[test]
    fn test_load_checklist_no_checks() {
        let dir = tempfile::TempDir::new().unwrap();
        let tau_dir = dir.path().join(".tau");
        std::fs::create_dir_all(&tau_dir).unwrap();
        std::fs::write(tau_dir.join("checklist.toml"), "# empty checklist\n").unwrap();

        let items = load_checklist(dir.path().to_str().unwrap());
        assert!(items.is_empty());
    }

    #[test]
    fn test_load_checklist_invalid_toml() {
        let dir = tempfile::TempDir::new().unwrap();
        let tau_dir = dir.path().join(".tau");
        std::fs::create_dir_all(&tau_dir).unwrap();
        std::fs::write(tau_dir.join("checklist.toml"), "not [[ valid toml {{").unwrap();

        let items = load_checklist(dir.path().to_str().unwrap());
        assert!(items.is_empty());
    }

    // ----- merge state validation -----

    #[test]
    fn test_merge_result_serialization() {
        let result = MergeResult {
            success: true,
            log: "all good".into(),
        };
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("\"success\":true"));
        assert!(json.contains("all good"));
    }

    // ----- helper to create a task in merging state -----

    fn make_merging_task(db: &TasksDb) -> i64 {
        let task = db
            .create_task(
                "/project",
                "Test merge",
                None,
                None,
                None,
                true,
                false,
                false,
            )
            .unwrap();
        // interactive -> ready
        db.update_task(
            task.id,
            &TaskUpdate {
                state: Some("ready".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        // ready -> active (via assign)
        db.assign_task(task.id, "s1").unwrap();
        // active -> approved (skip_review=true)
        db.update_task(
            task.id,
            &TaskUpdate {
                state: Some("approved".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        // approved -> merging
        db.update_task(
            task.id,
            &TaskUpdate {
                state: Some("merging".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        db.set_branch(task.id, "task-1").unwrap();
        db.set_worktree_path(task.id, "/tmp/wt-1").unwrap();
        task.id
    }

    #[test]
    fn test_merge_task_requires_merging_state() {
        let db = TasksDb::open_memory().unwrap();
        let task = db
            .create_task(
                "/project",
                "Not merging",
                None,
                None,
                None,
                false,
                false,
                false,
            )
            .unwrap();

        // We can't call merge_task without real I/O, but we can validate
        // the state check by creating a mock reader/writer that will cause
        // an early return due to state validation.
        let mut writer: Vec<u8> = Vec::new();
        let mut reader = std::io::BufReader::new(std::io::Cursor::new(Vec::<u8>::new()));

        let result = merge_task(&db, task.id, "/project", &mut writer, &mut reader);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("must be 'merging'"), "got: {}", err);
    }

    #[test]
    fn test_merge_task_requires_branch() {
        let db = TasksDb::open_memory().unwrap();
        let task = db
            .create_task(
                "/project",
                "No branch",
                None,
                None,
                None,
                true,
                false,
                false,
            )
            .unwrap();
        db.update_task(
            task.id,
            &TaskUpdate {
                state: Some("ready".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        db.assign_task(task.id, "s1").unwrap();
        db.update_task(
            task.id,
            &TaskUpdate {
                state: Some("approved".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        db.update_task(
            task.id,
            &TaskUpdate {
                state: Some("merging".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        // No branch set, no worktree set

        let mut writer: Vec<u8> = Vec::new();
        let mut reader = std::io::BufReader::new(std::io::Cursor::new(Vec::<u8>::new()));

        let result = merge_task(&db, task.id, "/project", &mut writer, &mut reader);
        assert!(result.is_err());
        assert!(
            result.unwrap_err().to_string().contains("no branch"),
            "expected 'no branch' error"
        );
    }

    #[test]
    fn test_merge_task_requires_worktree() {
        let db = TasksDb::open_memory().unwrap();
        let task = db
            .create_task(
                "/project",
                "No worktree",
                None,
                None,
                None,
                true,
                false,
                false,
            )
            .unwrap();
        db.update_task(
            task.id,
            &TaskUpdate {
                state: Some("ready".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        db.assign_task(task.id, "s1").unwrap();
        db.update_task(
            task.id,
            &TaskUpdate {
                state: Some("approved".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        db.update_task(
            task.id,
            &TaskUpdate {
                state: Some("merging".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        db.set_branch(task.id, "task-1").unwrap();
        // No worktree set

        let mut writer: Vec<u8> = Vec::new();
        let mut reader = std::io::BufReader::new(std::io::Cursor::new(Vec::<u8>::new()));

        let result = merge_task(&db, task.id, "/project", &mut writer, &mut reader);
        assert!(result.is_err());
        assert!(
            result.unwrap_err().to_string().contains("no worktree"),
            "expected 'no worktree' error"
        );
    }

    // ----- parent notification -----

    #[test]
    fn test_notify_parent_all_subtasks_done() {
        let db = TasksDb::open_memory().unwrap();

        // Create parent
        let parent = db
            .create_task("/project", "Parent", None, None, None, false, false, false)
            .unwrap();
        db.set_branch(parent.id, "task-parent").unwrap();

        // Create two subtasks and move them to merged
        let child1 = db
            .create_task(
                "/project",
                "Child 1",
                None,
                Some(parent.id),
                None,
                false,
                true,
                false,
            )
            .unwrap();
        let child2 = db
            .create_task(
                "/project",
                "Child 2",
                None,
                Some(parent.id),
                None,
                false,
                true,
                false,
            )
            .unwrap();

        // Move both to merged via full state machine
        for child_id in [child1.id, child2.id] {
            db.assign_task(child_id, "s1").unwrap();
            db.update_task(
                child_id,
                &TaskUpdate {
                    state: Some("review".into()),
                    ..Default::default()
                },
                None,
            )
            .unwrap();
            db.update_task(
                child_id,
                &TaskUpdate {
                    state: Some("approved".into()),
                    ..Default::default()
                },
                None,
            )
            .unwrap();
            db.update_task(
                child_id,
                &TaskUpdate {
                    state: Some("merging".into()),
                    ..Default::default()
                },
                None,
            )
            .unwrap();
            db.update_task(
                child_id,
                &TaskUpdate {
                    state: Some("merged".into()),
                    ..Default::default()
                },
                None,
            )
            .unwrap();
        }

        // No real server — writer/reader that won't be used (no session_id on parent)
        let mut writer: Vec<u8> = Vec::new();
        let mut reader = std::io::BufReader::new(std::io::Cursor::new(Vec::<u8>::new()));

        notify_parent_if_all_done(&db, child1.id, &mut writer, &mut reader).unwrap();

        // Parent should have a message about all subtasks completed
        let messages = db.get_messages(parent.id).unwrap();
        assert_eq!(messages.len(), 1);
        assert!(messages[0].content.contains("All subtasks completed"));
        assert!(messages[0].content.contains("task-parent"));
    }

    #[test]
    fn test_notify_parent_not_all_done() {
        let db = TasksDb::open_memory().unwrap();

        let parent = db
            .create_task("/project", "Parent", None, None, None, false, false, false)
            .unwrap();
        db.set_branch(parent.id, "task-parent").unwrap();

        let child1 = db
            .create_task(
                "/project",
                "Child 1",
                None,
                Some(parent.id),
                None,
                false,
                true,
                false,
            )
            .unwrap();
        let _child2 = db
            .create_task(
                "/project",
                "Child 2",
                None,
                Some(parent.id),
                None,
                false,
                true,
                false,
            )
            .unwrap();

        // Only move child1 to merged
        db.assign_task(child1.id, "s1").unwrap();
        db.update_task(
            child1.id,
            &TaskUpdate {
                state: Some("review".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        db.update_task(
            child1.id,
            &TaskUpdate {
                state: Some("approved".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        db.update_task(
            child1.id,
            &TaskUpdate {
                state: Some("merging".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        db.update_task(
            child1.id,
            &TaskUpdate {
                state: Some("merged".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();

        let mut writer: Vec<u8> = Vec::new();
        let mut reader = std::io::BufReader::new(std::io::Cursor::new(Vec::<u8>::new()));

        notify_parent_if_all_done(&db, child1.id, &mut writer, &mut reader).unwrap();

        // No message should be added — child2 is still in 'ready' state
        let messages = db.get_messages(parent.id).unwrap();
        assert!(messages.is_empty());
    }

    #[test]
    fn test_notify_parent_root_task_noop() {
        let db = TasksDb::open_memory().unwrap();

        let task = db
            .create_task("/project", "Root", None, None, None, false, false, false)
            .unwrap();

        let mut writer: Vec<u8> = Vec::new();
        let mut reader = std::io::BufReader::new(std::io::Cursor::new(Vec::<u8>::new()));

        // Should be a no-op for root tasks
        notify_parent_if_all_done(&db, task.id, &mut writer, &mut reader).unwrap();
    }

    #[test]
    fn test_notify_parent_sends_queue_message() {
        let db = TasksDb::open_memory().unwrap();

        let parent = db
            .create_task("/project", "Parent", None, None, None, false, false, false)
            .unwrap();
        db.set_branch(parent.id, "task-parent").unwrap();
        db.set_session_id(parent.id, "parent-session").unwrap();

        let child = db
            .create_task(
                "/project",
                "Child",
                None,
                Some(parent.id),
                None,
                false,
                true,
                false,
            )
            .unwrap();

        // Move child to merged
        db.assign_task(child.id, "s1").unwrap();
        db.update_task(
            child.id,
            &TaskUpdate {
                state: Some("review".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        db.update_task(
            child.id,
            &TaskUpdate {
                state: Some("approved".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        db.update_task(
            child.id,
            &TaskUpdate {
                state: Some("merging".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        db.update_task(
            child.id,
            &TaskUpdate {
                state: Some("merged".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();

        // Provide a fake server response for the QueueMessage
        let _fake_response = serde_json::to_string(&PluginRequest::ServerResponse {
            request_id: "placeholder".into(),
            response: Response::Ok,
        })
        .unwrap();

        // We need the reader to provide a response. However, the request_id
        // is generated dynamically, so we simulate by writing a response that
        // matches. For this test, we verify the writer output instead.
        // Use an empty reader — the QueueMessage send will fail/block but
        // since it's best-effort (uses `let _ =`), the function should still
        // return Ok. Actually, it will block on read... Let's use a reader
        // that provides a response. We need to predict the request_id prefix.

        // Actually, notify_parent_if_all_done catches the QueueMessage error
        // with `let _ =`. But server_request will block on stdin. For testing,
        // we can't easily mock this. Instead let's verify the DB side effects
        // and that the function doesn't panic when stdin is empty (it will error,
        // but the `let _ =` catches it).

        // Provide a reader that immediately returns EOF, which will cause
        // server_request to return Err, which is caught by `let _ =`.
        let mut writer: Vec<u8> = Vec::new();
        let mut reader = std::io::BufReader::new(std::io::Cursor::new(Vec::<u8>::new()));

        // The QueueMessage will fail (empty reader), but that's ok — it's
        // handled gracefully. The function should still succeed.
        notify_parent_if_all_done(&db, child.id, &mut writer, &mut reader).unwrap();

        // Verify DB side effects
        let messages = db.get_messages(parent.id).unwrap();
        assert_eq!(messages.len(), 1);
        assert!(messages[0].content.contains("All subtasks completed"));

        // Verify that a QueueMessage was attempted (check writer output)
        let output = String::from_utf8(writer).unwrap();
        assert!(output.contains("queue_message"), "output: {}", output);
        assert!(output.contains("parent-session"), "output: {}", output);
    }

    // ----- notify_parent_of_subtask_done tests -----

    #[test]
    fn test_notify_subtask_done_sends_queue_message() {
        let db = TasksDb::open_memory().unwrap();

        let parent = db
            .create_task("/project", "Parent", None, None, None, false, false, false)
            .unwrap();
        db.set_session_id(parent.id, "parent-session").unwrap();

        let child = db
            .create_task(
                "/project",
                "Child Task",
                None,
                Some(parent.id),
                None,
                false,
                true,
                false,
            )
            .unwrap();

        // Move child to merged
        db.assign_task(child.id, "s1").unwrap();
        for state in ["review", "approved", "merging", "merged"] {
            db.update_task(
                child.id,
                &TaskUpdate {
                    state: Some(state.into()),
                    ..Default::default()
                },
                None,
            )
            .unwrap();
        }

        // Reader returns EOF → server_request fails, but the function is
        // best-effort and won't panic.
        let mut writer: Vec<u8> = Vec::new();
        let mut reader = std::io::BufReader::new(std::io::Cursor::new(Vec::<u8>::new()));

        notify_parent_of_subtask_done(&db, child.id, &mut writer, &mut reader);

        // Verify that a QueueMessage was attempted with the right content
        let output = String::from_utf8(writer).unwrap();
        assert!(output.contains("queue_message"), "output: {}", output);
        assert!(output.contains("parent-session"), "output: {}", output);
        assert!(
            output.contains("Child Task"),
            "should contain task title: {}",
            output
        );
        assert!(
            output.contains(&format!("#{}", child.id)),
            "should contain task id: {}",
            output
        );
    }

    #[test]
    fn test_notify_subtask_done_root_task_noop() {
        let db = TasksDb::open_memory().unwrap();

        let task = db
            .create_task("/project", "Root", None, None, None, false, false, false)
            .unwrap();

        let mut writer: Vec<u8> = Vec::new();
        let mut reader = std::io::BufReader::new(std::io::Cursor::new(Vec::<u8>::new()));

        // Should be a no-op for root tasks — no writes
        notify_parent_of_subtask_done(&db, task.id, &mut writer, &mut reader);
        assert!(writer.is_empty());
    }

    #[test]
    fn test_notify_subtask_done_parent_without_session() {
        let db = TasksDb::open_memory().unwrap();

        let parent = db
            .create_task("/project", "Parent", None, None, None, false, false, false)
            .unwrap();
        // Don't set session_id on parent

        let child = db
            .create_task(
                "/project",
                "Child",
                None,
                Some(parent.id),
                None,
                false,
                true,
                false,
            )
            .unwrap();

        let mut writer: Vec<u8> = Vec::new();
        let mut reader = std::io::BufReader::new(std::io::Cursor::new(Vec::<u8>::new()));

        // Should be a no-op — no session to notify
        notify_parent_of_subtask_done(&db, child.id, &mut writer, &mut reader);
        assert!(writer.is_empty());
    }

    // ----- merging state transition helpers -----

    #[test]
    fn test_full_state_machine_to_merging() {
        let db = TasksDb::open_memory().unwrap();
        let task_id = make_merging_task(&db);

        let task = db.get_task(task_id).unwrap().unwrap();
        assert_eq!(task.state, "merging");
        assert_eq!(task.branch.as_deref(), Some("task-1"));
        assert_eq!(task.worktree_path.as_deref(), Some("/tmp/wt-1"));
    }

    #[test]
    fn test_merging_to_merged_transition() {
        let db = TasksDb::open_memory().unwrap();
        let task_id = make_merging_task(&db);

        // merging -> merged
        db.update_task(
            task_id,
            &TaskUpdate {
                state: Some("merged".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();

        let task = db.get_task(task_id).unwrap().unwrap();
        assert_eq!(task.state, "merged");
    }

    #[test]
    fn test_merging_to_active_transition() {
        let db = TasksDb::open_memory().unwrap();
        let task_id = make_merging_task(&db);

        // merging -> active (recoverable merge failure: rebase conflict, checklist)
        db.update_task(
            task_id,
            &TaskUpdate {
                state: Some("active".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();

        let task = db.get_task(task_id).unwrap().unwrap();
        assert_eq!(task.state, "active");
    }

    #[test]
    fn test_merging_to_failed_to_active_transition() {
        let db = TasksDb::open_memory().unwrap();
        let task_id = make_merging_task(&db);

        // merging -> failed (terminal infrastructure error)
        db.update_task(
            task_id,
            &TaskUpdate {
                state: Some("failed".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();

        let task = db.get_task(task_id).unwrap().unwrap();
        assert_eq!(task.state, "failed");

        // failed -> active (manual recovery)
        db.update_task(
            task_id,
            &TaskUpdate {
                state: Some("active".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();

        let task = db.get_task(task_id).unwrap().unwrap();
        assert_eq!(task.state, "active");
    }
}
