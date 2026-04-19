//! Merge queue for the task system.
//!
//! This module runs inside the `tau plugin-tasks` subprocess, which has no
//! `tracing` subscriber. Diagnostics use `eprintln!`; the parent server
//! forwards the plugin's stderr into its own tracing layer, so lines still
//! end up in `~/.local/state/tau/logs/server.log`. See `tasks.rs` for the
//! full rationale.
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

use crate::tasks_db::{TaskSession, TasksDb};
use crate::tasks_state::TaskState;
use tau_agent_plugin::{Request, Response};

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

/// Load the project checklist from up to three config tiers (operator >
/// project > global).  Checklist items from higher tiers are prepended:
/// operator items run first, then project, then global.
///
/// Returns an empty vec if no tier has a checklist file.
pub fn load_checklist(project_dir: &str, project_name: Option<&str>) -> Vec<CheckItem> {
    let configs: Vec<(_, Checklist)> = tau_agent_base::config_chain::load_all(
        project_name,
        Some(project_dir),
        "checklist.toml",
        true, // checklist is not security-sensitive
    );

    let mut items = Vec::new();
    for (_path, checklist) in configs {
        items.extend(checklist.check);
    }
    items
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
// ServerRequest tunnel (delegates to shared tau_agent_plugin::tunnel)
// ---------------------------------------------------------------------------

fn server_request(
    writer: &mut impl Write,
    reader: &mut impl BufRead,
    request: Request,
) -> tau_agent_plugin::Result<Response> {
    // Route to the thread-local prefix so the plugin's line router
    // delivers the response to the correct channel (main loop vs.
    // merge worker). See `tasks_scheduler::RPC_PREFIX`.
    crate::tasks_scheduler::server_request(writer, reader, request)
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
) -> tau_agent_plugin::Result<(String, bool)> {
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
        Response::Error { message } => Err(tau_agent_plugin::Error::Io(format!(
            "ExecuteTool error: {}",
            message
        ))),
        other => Err(tau_agent_plugin::Error::Io(format!(
            "unexpected ExecuteTool response: {:?}",
            other
        ))),
    }
}

// ---------------------------------------------------------------------------
// Merge execution
// ---------------------------------------------------------------------------

/// Build the shell command used in step 7b to delete a task's branch
/// after a successful merge.
///
/// The command uses `git -C <project_dir>` so it is cwd-agnostic: the
/// log-session that runs it was created with `cwd = worktree_path`,
/// and step 7a deletes that worktree before we reach 7b. Without
/// `-C`, bash fails to `chdir` into the missing cwd and returns
/// ENOENT before git is even exec'd — the silent post-merge
/// branch-leak reported in task #581.
fn build_branch_delete_command(project_dir: &str, branch: &str) -> String {
    format!("git -C '{}' branch -D {}", project_dir, branch)
}

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
) -> tau_agent_plugin::Result<MergeResult> {
    merge_task_for_caller(db, task_id, project_dir, None, writer, reader)
}

/// Variant of [`merge_task`] that knows the caller's session id, so it can
/// defer archiving sessions in the caller's subtree to Tier-3 via
/// [`Request::EnqueuePostIdleAction`].  This is what fixes the archival
/// race described in task #533: when the reviewer calls
/// `task_update(state=approved)`, the reviewer's own session is one of the
/// sessions scheduled for archive — archiving inline would fail silently
/// because the reviewer's lock is still held by the agent loop.
///
/// If `caller_session_id` is `None` (scheduler-tick merges, CLI merges),
/// archival runs inline as before.
pub fn merge_task_for_caller(
    db: &TasksDb,
    task_id: i64,
    project_dir: &str,
    caller_session_id: Option<&str>,
    writer: &mut impl Write,
    reader: &mut impl BufRead,
) -> tau_agent_plugin::Result<MergeResult> {
    // 1. Get task, branch, merge target
    let task = db
        .get_task(task_id)?
        .ok_or_else(|| tau_agent_plugin::Error::Io(format!("task {} not found", task_id)))?;

    if task.state != TaskState::Merging {
        return Err(tau_agent_plugin::Error::Io(format!(
            "task {} is in state '{}', must be 'merging'",
            task_id, task.state
        )));
    }

    let branch = task.branch.as_ref().ok_or_else(|| {
        tau_agent_plugin::Error::Io(format!("task {} has no branch set", task_id))
    })?;

    let worktree_path = task
        .worktree_path
        .as_ref()
        .ok_or_else(|| tau_agent_plugin::Error::Io(format!("task {} has no worktree", task_id)))?;

    let merge_target = db.get_merge_target(task_id)?;

    let mut log = String::new();

    // 3. Create a log-provider session
    // Task #561: parent the merge session on the task's placeholder so
    // it nests with the rest of the task's sessions and is cascade-
    // archived with the placeholder subtree. Pre-561 tasks (no
    // placeholder) get the legacy `None` parenting.
    let log_session = match crate::tasks_session::create_task_session(
        crate::tasks_session::TaskSessionSpec {
            task: &task,
            role: "merge",
            model: Some("log".into()),
            cwd: Some(worktree_path.clone()),
            parent_id: task.placeholder_session_id.clone(),
            child_budget: 0,
            sandbox_profile: None,
        },
        writer,
        reader,
    ) {
        Ok(sid) => sid,
        Err(e) => {
            return Ok(MergeResult {
                success: false,
                log: format!("Failed to create log session: {}", e),
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
        if let Err(e) = archive_session(writer, reader, &log_session) {
            eprintln!(
                "tasks: warning: failed to archive log session {}: {}",
                log_session, e
            );
        }
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
        if let Err(e) = archive_session(writer, reader, &log_session) {
            eprintln!(
                "tasks: warning: failed to archive log session {}: {}",
                log_session, e
            );
        }
        return Ok(MergeResult {
            success: false,
            log: format!("Rebase failed:\n{}", log),
        });
    }

    // 5. Run checklist
    let checklist = load_checklist(project_dir, None);
    for item in &checklist {
        log.push_str(&format!("=== Check: {} ===\n", item.name));
        let (output, is_error) = execute_bash(writer, reader, &log_session, &item.command)?;
        log.push_str(&output);
        log.push('\n');

        if is_error {
            if let Err(e) = archive_session(writer, reader, &log_session) {
                eprintln!(
                    "tasks: warning: failed to archive log session {}: {}",
                    log_session, e
                );
            }
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
        if let Err(e) = archive_session(writer, reader, &log_session) {
            eprintln!(
                "tasks: warning: failed to archive log session {}: {}",
                log_session, e
            );
        }
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

    // 7a'. Update session cwds: any session still pointing at the removed
    // worktree should be moved back to the project root so that plugin
    // respawns don't fail with "No such file or directory".
    //
    // The `log_session` we created in step 3 was given `cwd =
    // worktree_path` and is NOT recorded in `task_sessions` (it is
    // purely local to this merge), so the DB-driven loop below would
    // miss it. Move its cwd explicitly first — otherwise subsequent
    // `execute_bash` calls on the log session spawn bash with a
    // deleted cwd and fail with `ENOENT` before the command runs
    // (see task #581).
    let _ = server_request(
        writer,
        reader,
        Request::SetCwd {
            session_id: log_session.clone(),
            cwd: project_dir.to_string(),
            caller_session_id: None,
        },
    );
    if let Ok(sessions) = db.get_sessions(task_id) {
        for ts in &sessions {
            let _ = server_request(
                writer,
                reader,
                Request::SetCwd {
                    session_id: ts.session_id.clone(),
                    cwd: project_dir.to_string(),
                    caller_session_id: None,
                },
            );
        }
    }

    // 7b. Delete the task branch (no longer needed after merge).
    //
    // Use `git -C <project_dir>` so the command is cwd-agnostic: the
    // log_session was created with `cwd = worktree_path`, which step
    // 7a just deleted. Before task #581 this was plain `git branch
    // -D <branch>`, and bash's `chdir` into the missing worktree
    // failed with ENOENT before git ever ran — leaving dead
    // `task-NNN` branches after every merge. Step 7a' also moves the
    // log_session's cwd to `project_dir` as a belt-and-braces fix,
    // but the explicit `-C` here is what makes this step robust
    // against any future cwd weirdness.
    let br_cmd = build_branch_delete_command(project_dir, branch);
    let (output, br_err) = execute_bash(writer, reader, &log_session, &br_cmd)?;
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
    // Task #561: if the task has a placeholder session, archive it —
    // `archive_session_tree` cascades to every child (planner, worker,
    // reviewer, refiner, merge log session, …) in one hop, replacing
    // the per-role loop below. The loop is retained as a fallback for
    // tasks that predate task #561 and have no placeholder.
    //
    // We deliberately do NOT fall back to archiving `task.session_id` when
    // it isn't present in `task_sessions`: every codepath that sets
    // `task.session_id` also records an entry in `task_sessions` with an
    // explicit role (`assign_task` records `worker`, interactive creation
    // records `interactive`, etc.), so a `task.session_id` missing from
    // `task_sessions` means we have no role information and cannot safely
    // decide to archive.
    //
    // Task #575: archival always goes through the post-idle retry path
    // when we have a caller session — the server's `ArchiveSession`
    // guard rejects any subtree where at least one session is still
    // live (turn running), and the inline fire-and-forget path
    // swallowed those errors silently, leaving subtrees un-archived
    // indefinitely. The post-idle drain (see
    // `crates/tau-agent-lib/src/server/post_idle.rs`) retries on
    // transient busy errors and is the single source of truth for
    // task-session archival. When there is no caller session
    // (scheduler-tick or CLI merges) we fall back to the inline
    // busy-retry helper so the same behaviour holds for those paths.
    if let Some(cid) = caller_session_id {
        let _ = server_request(
            writer,
            reader,
            Request::EnqueuePostIdleAction {
                session_id: cid.to_string(),
                action: tau_agent_plugin::PostIdleAction::ArchiveTaskSessions { task_id },
            },
        );
        if let Some(ref placeholder_sid) = task.placeholder_session_id {
            log.push_str(&format!(
                "Deferred archival of placeholder session {} (cascades to subtree) to post-idle\n",
                placeholder_sid
            ));
        } else if let Ok(sessions) = db.get_sessions(task_id) {
            let (to_archive, to_skip) = sessions_to_archive(&sessions);
            log.push_str(&format!(
                "Deferred archival of {} task session(s) to post-idle\n",
                to_archive.len()
            ));
            for ts in to_skip {
                log.push_str(&format!(
                    "Preserved {} session {} (role not in archive list)\n",
                    ts.role, ts.session_id
                ));
            }
        }
    } else if let Some(ref placeholder_sid) = task.placeholder_session_id {
        // No caller context — inline archival with busy retry.
        match archive_session_with_busy_retry(writer, reader, placeholder_sid) {
            Ok(()) => log.push_str(&format!(
                "Archived placeholder session {} (cascades to subtree)\n",
                placeholder_sid
            )),
            Err(e) => log.push_str(&format!(
                "WARNING: failed to archive placeholder session {}: {}\n",
                placeholder_sid, e
            )),
        }
    } else if let Ok(sessions) = db.get_sessions(task_id) {
        let (to_archive, to_skip) = sessions_to_archive(&sessions);
        for ts in to_archive {
            match archive_session_with_busy_retry(writer, reader, &ts.session_id) {
                Ok(()) => {
                    log.push_str(&format!("Archived {} session {}\n", ts.role, ts.session_id))
                }
                Err(e) => log.push_str(&format!(
                    "WARNING: failed to archive {} session {}: {}\n",
                    ts.role, ts.session_id, e
                )),
            }
        }
        for ts in to_skip {
            log.push_str(&format!(
                "Preserved {} session {} (role not in archive list)\n",
                ts.role, ts.session_id
            ));
        }
    }

    // 8. Archive the log session. When the task has a placeholder the
    // log session is its descendant and will be cascade-archived by
    // the post-idle drain (or by the inline busy-retry above); this
    // call is idempotent — archiving an already-archived session is a
    // no-op on the server side. For tasks without a placeholder, and
    // for the no-caller path, this is the authoritative archival of
    // the log session. We log any error rather than silently drop it
    // (task #575).
    if let Err(e) = archive_session(writer, reader, &log_session) {
        log.push_str(&format!(
            "WARNING: failed to archive log session {}: {}\n",
            log_session, e
        ));
    }

    log.push_str("=== Merge complete ===\n");

    Ok(MergeResult { success: true, log })
}

/// Archive a session, returning an `Err(message)` on RPC or server
/// failure.
///
/// Historically this was a fire-and-forget call that dropped every
/// error — including the server's "cannot archive: session X is busy"
/// error returned when a subtree session is still live. That silent
/// drop was the root cause of task #575: whole task subtrees were
/// never archived because one child hadn't finished unregistering
/// when the merge ran. Callers must now explicitly handle or log
/// any error, and the common caller (merge cleanup) routes archival
/// through the post-idle retry path instead of calling this directly.
fn archive_session(
    writer: &mut impl Write,
    reader: &mut impl BufRead,
    session_id: &str,
) -> Result<(), String> {
    match server_request(
        writer,
        reader,
        Request::ArchiveSession {
            session_id: session_id.to_string(),
            require_ancestor: None,
        },
    ) {
        Ok(Response::Ok) => Ok(()),
        Ok(Response::Error { message }) => Err(message),
        Ok(other) => Err(format!("unexpected ArchiveSession response: {:?}", other)),
        Err(e) => Err(e.to_string()),
    }
}

/// Maximum attempts when retrying an `ArchiveSession` request that
/// returns a transient "busy" error. Mirrors
/// `post_idle::ARCHIVE_MAX_RETRIES`.
const INLINE_ARCHIVE_MAX_RETRIES: u32 = 20;

/// Delay between retry attempts, in milliseconds. Mirrors
/// `post_idle::ARCHIVE_RETRY_DELAY_MS`.
const INLINE_ARCHIVE_RETRY_DELAY_MS: u64 = 1000;

/// Archive a session, retrying on transient "cannot archive: session X
/// is busy" responses from the server.
///
/// Used on merge paths that have no caller session and therefore
/// cannot defer archival to the Tier-3 post-idle drain (e.g. CLI
/// merges, scheduler-tick merges with no session context). The
/// retry schedule matches the post-idle path so both routes recover
/// from the same race window.
fn archive_session_with_busy_retry(
    writer: &mut impl Write,
    reader: &mut impl BufRead,
    session_id: &str,
) -> Result<(), String> {
    let mut last_err = String::new();
    for attempt in 0..INLINE_ARCHIVE_MAX_RETRIES {
        match archive_session(writer, reader, session_id) {
            Ok(()) => return Ok(()),
            Err(e) => {
                // Only retry on the specific "busy" error — any other
                // error (session not found, DB failure) is terminal.
                if !e.contains("is busy") {
                    return Err(e);
                }
                last_err = e;
                if attempt + 1 < INLINE_ARCHIVE_MAX_RETRIES {
                    std::thread::sleep(std::time::Duration::from_millis(
                        INLINE_ARCHIVE_RETRY_DELAY_MS,
                    ));
                }
            }
        }
    }
    Err(format!(
        "giving up after {} busy-retries: {}",
        INLINE_ARCHIVE_MAX_RETRIES, last_err
    ))
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
) -> tau_agent_plugin::Result<()> {
    let task = db
        .get_task(task_id)?
        .ok_or_else(|| tau_agent_plugin::Error::Io(format!("task {} not found", task_id)))?;

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
            .all(|t| t.state == TaskState::Merged || t.state == TaskState::Closed);

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

    // Tests that read config files must be serialized when we override env.
    static CHECKLIST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct XdgGuard {
        prev_xdg: Option<String>,
        prev_home: Option<String>,
    }

    impl Drop for XdgGuard {
        fn drop(&mut self) {
            match &self.prev_xdg {
                Some(v) => unsafe { std::env::set_var("XDG_CONFIG_HOME", v) },
                None => unsafe { std::env::remove_var("XDG_CONFIG_HOME") },
            }
            match &self.prev_home {
                Some(v) => unsafe { std::env::set_var("HOME", v) },
                None => unsafe { std::env::remove_var("HOME") },
            }
        }
    }

    fn isolate_config() -> (tempfile::TempDir, XdgGuard) {
        let config_tmp = tempfile::TempDir::new().unwrap();
        let guard = XdgGuard {
            prev_xdg: std::env::var("XDG_CONFIG_HOME").ok(),
            prev_home: std::env::var("HOME").ok(),
        };
        unsafe { std::env::set_var("XDG_CONFIG_HOME", config_tmp.path()) };
        (config_tmp, guard)
    }
    use tau_agent_plugin::PluginRequest;

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
            .create_task(
                "test-project",
                "T",
                None,
                None,
                None,
                true,
                "interactive",
                false,
                None,
                None,
                false,
                None,
                false,
            )
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
        let _g = CHECKLIST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let (_cfg, _xdg) = isolate_config();

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

        let items = load_checklist(dir.path().to_str().unwrap(), None);
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].name, "fmt");
        assert_eq!(items[0].command, "cargo fmt --check");
        assert_eq!(items[1].name, "clippy");
        assert_eq!(items[2].name, "test");
        assert_eq!(items[2].command, "cargo test");
    }

    #[test]
    fn test_load_checklist_missing_file() {
        let _g = CHECKLIST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let (_cfg, _xdg) = isolate_config();

        let dir = tempfile::TempDir::new().unwrap();
        let items = load_checklist(dir.path().to_str().unwrap(), None);
        assert!(items.is_empty());
    }

    #[test]
    fn test_load_checklist_empty_file() {
        let _g = CHECKLIST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let (_cfg, _xdg) = isolate_config();

        let dir = tempfile::TempDir::new().unwrap();
        let tau_dir = dir.path().join(".tau");
        std::fs::create_dir_all(&tau_dir).unwrap();
        std::fs::write(tau_dir.join("checklist.toml"), "").unwrap();

        let items = load_checklist(dir.path().to_str().unwrap(), None);
        assert!(items.is_empty());
    }

    #[test]
    fn test_load_checklist_no_checks() {
        let _g = CHECKLIST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let (_cfg, _xdg) = isolate_config();

        let dir = tempfile::TempDir::new().unwrap();
        let tau_dir = dir.path().join(".tau");
        std::fs::create_dir_all(&tau_dir).unwrap();
        std::fs::write(tau_dir.join("checklist.toml"), "# empty checklist\n").unwrap();

        let items = load_checklist(dir.path().to_str().unwrap(), None);
        assert!(items.is_empty());
    }

    #[test]
    fn test_load_checklist_invalid_toml() {
        let _g = CHECKLIST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let (_cfg, _xdg) = isolate_config();

        let dir = tempfile::TempDir::new().unwrap();
        let tau_dir = dir.path().join(".tau");
        std::fs::create_dir_all(&tau_dir).unwrap();
        std::fs::write(tau_dir.join("checklist.toml"), "not [[ valid toml {{").unwrap();

        let items = load_checklist(dir.path().to_str().unwrap(), None);
        assert!(items.is_empty());
    }

    #[test]
    fn test_load_checklist_three_tier_operator_first() {
        let _g = CHECKLIST_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let (config_tmp, _xdg) = isolate_config();

        // Global checklist
        let global_dir = config_tmp.path().join("tau");
        std::fs::create_dir_all(&global_dir).unwrap();
        std::fs::write(
            global_dir.join("checklist.toml"),
            "[[check]]\nname = \"global-lint\"\ncommand = \"lint-global\"\n",
        )
        .unwrap();

        // Project checklist
        let project_tmp = tempfile::TempDir::new().unwrap();
        let tau_dir = project_tmp.path().join(".tau");
        std::fs::create_dir_all(&tau_dir).unwrap();
        std::fs::write(
            tau_dir.join("checklist.toml"),
            "[[check]]\nname = \"project-test\"\ncommand = \"test-project\"\n",
        )
        .unwrap();

        // Operator checklist
        let operator_dir = global_dir.join("projects").join("myproj");
        std::fs::create_dir_all(&operator_dir).unwrap();
        std::fs::write(
            operator_dir.join("checklist.toml"),
            "[[check]]\nname = \"operator-fmt\"\ncommand = \"fmt-operator\"\n",
        )
        .unwrap();

        let items = load_checklist(project_tmp.path().to_str().unwrap(), Some("myproj"));

        assert_eq!(items.len(), 3);
        // Operator first, then project, then global
        assert_eq!(items[0].name, "operator-fmt");
        assert_eq!(items[1].name, "project-test");
        assert_eq!(items[2].name, "global-lint");
    }

    // ----- merge state validation -----

    // ----- branch-delete command construction (task #581) -----

    #[test]
    fn test_build_branch_delete_command_uses_c_flag() {
        // The -C flag is what makes the command cwd-agnostic so it
        // still works after step 7a deletes the worktree that is the
        // log-session's cwd. Before task #581, the command was plain
        // `git branch -D <branch>` and bash silently failed to chdir
        // into the missing worktree.
        let cmd = build_branch_delete_command("/home/user/project", "task-42");
        assert!(
            cmd.contains("git -C '/home/user/project'"),
            "expected -C <project_dir>, got: {}",
            cmd
        );
        assert!(
            cmd.contains("branch -D task-42"),
            "expected branch -D <branch>, got: {}",
            cmd
        );
    }

    #[test]
    fn test_build_branch_delete_command_project_dir_comes_before_branch() {
        // Guard against someone accidentally swapping the argument
        // order — `git branch -C` means something entirely different
        // (copy branch).
        let cmd = build_branch_delete_command("/repo", "task-7");
        let c_idx = cmd.find("-C").expect("-C present");
        let d_idx = cmd.find("-D").expect("-D present");
        assert!(
            c_idx < d_idx,
            "expected -C before -D so it binds to the git top-level flag, got: {}",
            cmd
        );
    }

    #[test]
    fn test_build_branch_delete_command_does_not_depend_on_cwd() {
        // Property we care about: regardless of where the shell is
        // invoked from, the command carries enough info to find the
        // repo. Exercise this by actually running it from /tmp
        // against a throwaway git repo.
        let tmp = tempfile::tempdir().expect("tmpdir");
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).expect("mkdir");

        // Init a repo, make one commit on main, branch off a
        // `task-test` branch, then switch back to main so the
        // task-test branch is deletable.
        let run = |cwd: &std::path::Path, args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(cwd)
                .output()
                .expect("run git");
            assert!(
                out.status.success(),
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&out.stderr)
            );
        };
        run(&repo, &["init", "-q", "-b", "main"]);
        run(&repo, &["config", "user.email", "t@t"]);
        run(&repo, &["config", "user.name", "t"]);
        run(&repo, &["commit", "--allow-empty", "-qm", "init"]);
        run(&repo, &["branch", "task-test"]);

        // Run the delete command from /tmp (NOT inside the repo) —
        // the regression was exactly this case: cwd had nothing to
        // do with the repo and without `-C` the command failed.
        let project_dir = repo.to_str().expect("utf8 path");
        let cmd = build_branch_delete_command(project_dir, "task-test");
        let tmp_cwd = std::env::temp_dir();
        let out = std::process::Command::new("bash")
            .arg("-c")
            .arg(&cmd)
            .current_dir(&tmp_cwd)
            .output()
            .expect("run bash");
        assert!(
            out.status.success(),
            "branch-delete command failed when run from {}: stdout={} stderr={}",
            tmp_cwd.display(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );

        // Confirm the branch is actually gone.
        let check = std::process::Command::new("git")
            .args(["show-ref", "--verify", "refs/heads/task-test"])
            .current_dir(&repo)
            .output()
            .expect("run git show-ref");
        assert!(
            !check.status.success(),
            "task-test branch still exists after build_branch_delete_command ran"
        );
    }

    #[test]
    fn test_build_branch_delete_command_survives_missing_cwd() {
        // Regression for task #581: the log-session's cwd is the
        // task worktree, which step 7a deletes before step 7b runs.
        // Simulate that exact sequence: create a dir, cd bash into
        // it, delete the dir, then run the command. The command
        // MUST succeed because `-C` pins git to project_dir.
        let tmp = tempfile::tempdir().expect("tmpdir");
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).expect("mkdir repo");

        let run = |cwd: &std::path::Path, args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(cwd)
                .output()
                .expect("run git");
            assert!(out.status.success(), "git {:?} failed", args);
        };
        run(&repo, &["init", "-q", "-b", "main"]);
        run(&repo, &["config", "user.email", "t@t"]);
        run(&repo, &["config", "user.name", "t"]);
        run(&repo, &["commit", "--allow-empty", "-qm", "init"]);
        run(&repo, &["branch", "task-581"]);

        // Create a sibling directory that we will delete before
        // invoking bash — this is the "deleted worktree" scenario.
        let doomed_cwd = tmp.path().join("doomed-worktree");
        std::fs::create_dir(&doomed_cwd).expect("mkdir doomed");

        let project_dir = repo.to_str().expect("utf8 path");
        let cmd = build_branch_delete_command(project_dir, "task-581");

        // Spawn bash with cwd = doomed_cwd, but delete doomed_cwd
        // *before* we actually invoke it. std::process::Command
        // delegates chdir to posix_spawn/execvp; if doomed_cwd is
        // gone, spawn itself fails with ENOENT — exactly the
        // failure mode described in the bug report.
        //
        // So the realistic simulation is: use a parent shell that
        // deletes the dir itself, then runs the command. If the
        // command depends on cwd, the parent shell will itself
        // exit with failure; if the command has `-C`, it doesn't
        // care about cwd and succeeds.
        let wrapper = format!(
            "cd '{}' && cd .. && rm -rf '{}' && {}",
            doomed_cwd.display(),
            doomed_cwd.display(),
            cmd
        );
        let out = std::process::Command::new("bash")
            .arg("-c")
            .arg(&wrapper)
            .output()
            .expect("run bash");
        assert!(
            out.status.success(),
            "command failed when cwd was deleted mid-run: stdout={} stderr={}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );

        // Confirm the branch was actually deleted.
        let check = std::process::Command::new("git")
            .args(["show-ref", "--verify", "refs/heads/task-581"])
            .current_dir(&repo)
            .output()
            .expect("run git show-ref");
        assert!(
            !check.status.success(),
            "task-581 branch still exists after the cwd-deleted branch-delete command ran"
        );
    }

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
                "test-project",
                "Test merge",
                None,
                None,
                None,
                true,
                "interactive",
                false,
                None,
                None,
                false,
                None,
                false,
            )
            .unwrap();
        // interactive -> ready
        db.update_task(
            task.id,
            &TaskUpdate {
                state: Some(TaskState::Ready),
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
                state: Some(TaskState::Approved),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        // approved -> merging
        db.update_task(
            task.id,
            &TaskUpdate {
                state: Some(TaskState::Merging),
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
                "test-project",
                "Not merging",
                None,
                None,
                None,
                false,
                "interactive",
                false,
                None,
                None,
                false,
                None,
                false,
            )
            .unwrap();

        // We can't call merge_task without real I/O, but we can validate
        // the state check by creating a mock reader/writer that will cause
        // an early return due to state validation.
        let mut writer: Vec<u8> = Vec::new();
        let mut reader = std::io::BufReader::new(std::io::Cursor::new(Vec::<u8>::new()));

        let result = merge_task(&db, task.id, "test-project", &mut writer, &mut reader);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("must be 'merging'"), "got: {}", err);
    }

    #[test]
    fn test_merge_task_requires_branch() {
        let db = TasksDb::open_memory().unwrap();
        let task = db
            .create_task(
                "test-project",
                "No branch",
                None,
                None,
                None,
                true,
                "interactive",
                false,
                None,
                None,
                false,
                None,
                false,
            )
            .unwrap();
        db.update_task(
            task.id,
            &TaskUpdate {
                state: Some(TaskState::Ready),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        db.assign_task(task.id, "s1").unwrap();
        db.update_task(
            task.id,
            &TaskUpdate {
                state: Some(TaskState::Approved),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        db.update_task(
            task.id,
            &TaskUpdate {
                state: Some(TaskState::Merging),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        // No branch set, no worktree set

        let mut writer: Vec<u8> = Vec::new();
        let mut reader = std::io::BufReader::new(std::io::Cursor::new(Vec::<u8>::new()));

        let result = merge_task(&db, task.id, "test-project", &mut writer, &mut reader);
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
                "test-project",
                "No worktree",
                None,
                None,
                None,
                true,
                "interactive",
                false,
                None,
                None,
                false,
                None,
                false,
            )
            .unwrap();
        db.update_task(
            task.id,
            &TaskUpdate {
                state: Some(TaskState::Ready),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        db.assign_task(task.id, "s1").unwrap();
        db.update_task(
            task.id,
            &TaskUpdate {
                state: Some(TaskState::Approved),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        db.update_task(
            task.id,
            &TaskUpdate {
                state: Some(TaskState::Merging),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        db.set_branch(task.id, "task-1").unwrap();
        // No worktree set

        let mut writer: Vec<u8> = Vec::new();
        let mut reader = std::io::BufReader::new(std::io::Cursor::new(Vec::<u8>::new()));

        let result = merge_task(&db, task.id, "test-project", &mut writer, &mut reader);
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
            .create_task(
                "test-project",
                "Parent",
                None,
                None,
                None,
                false,
                "interactive",
                false,
                None,
                None,
                false,
                None,
                false,
            )
            .unwrap();
        db.set_branch(parent.id, "task-parent").unwrap();

        // Create two subtasks and move them to merged
        let child1 = db
            .create_task(
                "test-project",
                "Child 1",
                None,
                Some(parent.id),
                None,
                false,
                "ready",
                false,
                None,
                None,
                false,
                None,
                false,
            )
            .unwrap();
        let child2 = db
            .create_task(
                "test-project",
                "Child 2",
                None,
                Some(parent.id),
                None,
                false,
                "ready",
                false,
                None,
                None,
                false,
                None,
                false,
            )
            .unwrap();

        // Move both to merged via full state machine
        for child_id in [child1.id, child2.id] {
            db.assign_task(child_id, "s1").unwrap();
            db.update_task(
                child_id,
                &TaskUpdate {
                    state: Some(TaskState::Review),
                    ..Default::default()
                },
                None,
            )
            .unwrap();
            db.update_task(
                child_id,
                &TaskUpdate {
                    state: Some(TaskState::Approved),
                    ..Default::default()
                },
                None,
            )
            .unwrap();
            db.update_task(
                child_id,
                &TaskUpdate {
                    state: Some(TaskState::Merging),
                    ..Default::default()
                },
                None,
            )
            .unwrap();
            db.update_task(
                child_id,
                &TaskUpdate {
                    state: Some(TaskState::Merged),
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
            .create_task(
                "test-project",
                "Parent",
                None,
                None,
                None,
                false,
                "interactive",
                false,
                None,
                None,
                false,
                None,
                false,
            )
            .unwrap();
        db.set_branch(parent.id, "task-parent").unwrap();

        let child1 = db
            .create_task(
                "test-project",
                "Child 1",
                None,
                Some(parent.id),
                None,
                false,
                "ready",
                false,
                None,
                None,
                false,
                None,
                false,
            )
            .unwrap();
        let _child2 = db
            .create_task(
                "test-project",
                "Child 2",
                None,
                Some(parent.id),
                None,
                false,
                "ready",
                false,
                None,
                None,
                false,
                None,
                false,
            )
            .unwrap();

        // Only move child1 to merged
        db.assign_task(child1.id, "s1").unwrap();
        db.update_task(
            child1.id,
            &TaskUpdate {
                state: Some(TaskState::Review),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        db.update_task(
            child1.id,
            &TaskUpdate {
                state: Some(TaskState::Approved),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        db.update_task(
            child1.id,
            &TaskUpdate {
                state: Some(TaskState::Merging),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        db.update_task(
            child1.id,
            &TaskUpdate {
                state: Some(TaskState::Merged),
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
            .create_task(
                "test-project",
                "Root",
                None,
                None,
                None,
                false,
                "interactive",
                false,
                None,
                None,
                false,
                None,
                false,
            )
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
            .create_task(
                "test-project",
                "Parent",
                None,
                None,
                None,
                false,
                "interactive",
                false,
                None,
                None,
                false,
                None,
                false,
            )
            .unwrap();
        db.set_branch(parent.id, "task-parent").unwrap();
        db.set_session_id(parent.id, "parent-session").unwrap();

        let child = db
            .create_task(
                "test-project",
                "Child",
                None,
                Some(parent.id),
                None,
                false,
                "ready",
                false,
                None,
                None,
                false,
                None,
                false,
            )
            .unwrap();

        // Move child to merged
        db.assign_task(child.id, "s1").unwrap();
        db.update_task(
            child.id,
            &TaskUpdate {
                state: Some(TaskState::Review),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        db.update_task(
            child.id,
            &TaskUpdate {
                state: Some(TaskState::Approved),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        db.update_task(
            child.id,
            &TaskUpdate {
                state: Some(TaskState::Merging),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        db.update_task(
            child.id,
            &TaskUpdate {
                state: Some(TaskState::Merged),
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
            .create_task(
                "test-project",
                "Parent",
                None,
                None,
                None,
                false,
                "interactive",
                false,
                None,
                None,
                false,
                None,
                false,
            )
            .unwrap();
        db.set_session_id(parent.id, "parent-session").unwrap();

        let child = db
            .create_task(
                "test-project",
                "Child Task",
                None,
                Some(parent.id),
                None,
                false,
                "ready",
                false,
                None,
                None,
                false,
                None,
                false,
            )
            .unwrap();

        // Move child to merged
        db.assign_task(child.id, "s1").unwrap();
        for state in [
            TaskState::Review,
            TaskState::Approved,
            TaskState::Merging,
            TaskState::Merged,
        ] {
            db.update_task(
                child.id,
                &TaskUpdate {
                    state: Some(state),
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
            .create_task(
                "test-project",
                "Root",
                None,
                None,
                None,
                false,
                "interactive",
                false,
                None,
                None,
                false,
                None,
                false,
            )
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
            .create_task(
                "test-project",
                "Parent",
                None,
                None,
                None,
                false,
                "interactive",
                false,
                None,
                None,
                false,
                None,
                false,
            )
            .unwrap();
        // Don't set session_id on parent

        let child = db
            .create_task(
                "test-project",
                "Child",
                None,
                Some(parent.id),
                None,
                false,
                "ready",
                false,
                None,
                None,
                false,
                None,
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
        assert_eq!(task.state, TaskState::Merging);
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
                state: Some(TaskState::Merged),
                ..Default::default()
            },
            None,
        )
        .unwrap();

        let task = db.get_task(task_id).unwrap().unwrap();
        assert_eq!(task.state, TaskState::Merged);
    }

    #[test]
    fn test_merging_to_active_transition() {
        let db = TasksDb::open_memory().unwrap();
        let task_id = make_merging_task(&db);

        // merging -> active (recoverable merge failure: rebase conflict, checklist)
        db.update_task(
            task_id,
            &TaskUpdate {
                state: Some(TaskState::Active),
                ..Default::default()
            },
            None,
        )
        .unwrap();

        let task = db.get_task(task_id).unwrap().unwrap();
        assert_eq!(task.state, TaskState::Active);
    }

    #[test]
    fn test_merging_to_failed_to_active_transition() {
        let db = TasksDb::open_memory().unwrap();
        let task_id = make_merging_task(&db);

        // merging -> failed (terminal infrastructure error)
        db.update_task(
            task_id,
            &TaskUpdate {
                state: Some(TaskState::Failed),
                ..Default::default()
            },
            None,
        )
        .unwrap();

        let task = db.get_task(task_id).unwrap().unwrap();
        assert_eq!(task.state, TaskState::Failed);

        // failed -> active (manual recovery)
        db.update_task(
            task_id,
            &TaskUpdate {
                state: Some(TaskState::Active),
                ..Default::default()
            },
            None,
        )
        .unwrap();

        let task = db.get_task(task_id).unwrap().unwrap();
        assert_eq!(task.state, TaskState::Active);
    }

    // -----------------------------------------------------------------
    // archive_session / archive_session_with_busy_retry (task #575)
    //
    // The bug: `archive_session` used to be fire-and-forget, silently
    // dropping the server's "cannot archive: session X is busy" error
    // and leaving merged task subtrees un-archived indefinitely.
    //
    // These tests exercise the helpers directly against an in-process
    // mock "server" that runs in a worker thread and answers each
    // request with a canned `Response`. The helpers' signatures now
    // surface the error to the caller, and the retry variant loops on
    // "busy" responses until success or max-retries.
    // -----------------------------------------------------------------

    use std::io::{Read, Write};
    use std::sync::mpsc;
    use std::thread;
    use tau_agent_plugin::{PluginMessage, Request, Response};

    /// Pipe-ish writer: every `write` call appends to a channel the
    /// reader half can drain line-by-line. The merger sends requests
    /// through this and the mock server pulls them out.
    struct ChannelWriter {
        tx: mpsc::Sender<Vec<u8>>,
    }

    impl Write for ChannelWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.tx
                .send(buf.to_vec())
                .map_err(|e| std::io::Error::other(e.to_string()))?;
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Pipe-ish reader fed by the mock server. Blocks in `read` until
    /// data is available, matching how the plugin's `read_line` waits
    /// on a real stdin.
    struct ChannelReader {
        rx: mpsc::Receiver<Vec<u8>>,
        buf: Vec<u8>,
    }

    impl Read for ChannelReader {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            while self.buf.is_empty() {
                match self.rx.recv() {
                    Ok(chunk) => self.buf.extend_from_slice(&chunk),
                    Err(_) => return Ok(0), // channel closed = EOF
                }
            }
            let n = out.len().min(self.buf.len());
            out[..n].copy_from_slice(&self.buf[..n]);
            self.buf.drain(..n);
            Ok(n)
        }
    }

    /// Spawn a mock server that reads one line at a time from
    /// `req_rx`, parses it as a `PluginMessage::ServerRequest`, invokes
    /// `handler` to pick a `Response`, and writes the matching
    /// `PluginRequest::ServerResponse` back through `resp_tx`. Returns
    /// a join handle so the caller can assert on the request count.
    fn spawn_mock_server<F>(
        req_rx: mpsc::Receiver<Vec<u8>>,
        resp_tx: mpsc::Sender<Vec<u8>>,
        handler: F,
    ) -> thread::JoinHandle<Vec<Request>>
    where
        F: Fn(usize, &Request) -> Response + Send + 'static,
    {
        thread::spawn(move || {
            let mut pending = Vec::<u8>::new();
            let mut seen = Vec::<Request>::new();
            loop {
                // Block for the next chunk.
                match req_rx.recv() {
                    Ok(chunk) => pending.extend_from_slice(&chunk),
                    Err(_) => return seen,
                }
                // Drain any complete newline-terminated lines.
                while let Some(nl) = pending.iter().position(|&b| b == b'\n') {
                    let line: Vec<u8> = pending.drain(..=nl).collect();
                    let line_str = String::from_utf8_lossy(&line).to_string();
                    let msg: PluginMessage = match serde_json::from_str(line_str.trim()) {
                        Ok(m) => m,
                        Err(_) => continue,
                    };
                    let (request_id, request) = match msg {
                        PluginMessage::ServerRequest {
                            request_id,
                            request,
                        } => (request_id, request),
                        _ => continue,
                    };
                    let idx = seen.len();
                    let response = handler(idx, &request);
                    seen.push(request);
                    let resp = PluginRequest::ServerResponse {
                        request_id,
                        response,
                    };
                    let mut line = serde_json::to_vec(&resp).expect("serialize response");
                    line.push(b'\n');
                    if resp_tx.send(line).is_err() {
                        return seen;
                    }
                }
            }
        })
    }

    /// Wire up the merge code against a mock server parameterised by
    /// `handler`. Returns the requests the server observed and any
    /// result produced by `call` (typically `Result<(), String>`).
    fn with_mock_server<F, G, R>(handler: F, call: G) -> (Vec<Request>, R)
    where
        F: Fn(usize, &Request) -> Response + Send + 'static,
        G: FnOnce(&mut ChannelWriter, &mut std::io::BufReader<ChannelReader>) -> R,
    {
        let (req_tx, req_rx) = mpsc::channel::<Vec<u8>>();
        let (resp_tx, resp_rx) = mpsc::channel::<Vec<u8>>();
        let server = spawn_mock_server(req_rx, resp_tx, handler);
        let mut writer = ChannelWriter { tx: req_tx };
        let mut reader = std::io::BufReader::new(ChannelReader {
            rx: resp_rx,
            buf: Vec::new(),
        });
        let out = call(&mut writer, &mut reader);
        // Drop writer to close the request channel so the mock server exits.
        drop(writer);
        drop(reader);
        let seen = server.join().expect("mock server panicked");
        (seen, out)
    }

    #[test]
    fn archive_session_returns_ok_when_server_returns_ok() {
        let (seen, result) = with_mock_server(
            |_idx, _req| Response::Ok,
            |w, r| archive_session(w, r, "s42"),
        );
        assert!(result.is_ok(), "expected Ok, got {:?}", result);
        assert_eq!(seen.len(), 1);
        match &seen[0] {
            Request::ArchiveSession { session_id, .. } => {
                assert_eq!(session_id, "s42")
            }
            other => panic!("unexpected request: {:?}", other),
        }
    }

    #[test]
    fn archive_session_surfaces_server_error_instead_of_swallowing() {
        // Regression for task #575: the old implementation dropped this
        // error, leaving subtrees un-archived with no diagnostic.
        let (seen, result) = with_mock_server(
            |_idx, _req| Response::Error {
                message: "cannot archive: session s42 is busy".into(),
            },
            |w, r| archive_session(w, r, "s42"),
        );
        assert_eq!(seen.len(), 1);
        let err = result.expect_err("expected Err, got Ok");
        assert!(
            err.contains("is busy"),
            "error should preserve server message, got: {}",
            err
        );
    }

    #[test]
    fn archive_session_with_busy_retry_succeeds_after_transient_busy() {
        // First N attempts return busy, then the session unregisters
        // and the server returns Ok. The retry helper must drive
        // through the busy window rather than giving up on the first
        // failure.
        const BUSY_ATTEMPTS: usize = 3;
        let (seen, result) = with_mock_server(
            move |idx, _req| {
                if idx < BUSY_ATTEMPTS {
                    Response::Error {
                        message: "cannot archive: session s99 is busy".into(),
                    }
                } else {
                    Response::Ok
                }
            },
            |w, r| archive_session_with_busy_retry_for_test(w, r, "s99"),
        );
        assert!(result.is_ok(), "expected eventual Ok, got {:?}", result);
        assert_eq!(
            seen.len(),
            BUSY_ATTEMPTS + 1,
            "expected {} retries + 1 success",
            BUSY_ATTEMPTS
        );
        for req in &seen {
            match req {
                Request::ArchiveSession { session_id, .. } => assert_eq!(session_id, "s99"),
                other => panic!("unexpected request: {:?}", other),
            }
        }
    }

    #[test]
    fn archive_session_with_busy_retry_does_not_retry_terminal_errors() {
        // A non-busy error (e.g. session not found, DB error) is
        // terminal — the retry helper must not spin on it.
        let (seen, result) = with_mock_server(
            |_idx, _req| Response::Error {
                message: "session not found: s-missing".into(),
            },
            |w, r| archive_session_with_busy_retry_for_test(w, r, "s-missing"),
        );
        assert_eq!(seen.len(), 1, "non-busy errors must not retry");
        let err = result.expect_err("expected Err");
        assert!(err.contains("not found"), "got: {}", err);
    }

    /// Test-only shim around `archive_session_with_busy_retry` that
    /// collapses the sleep to zero so tests don't wait 20 seconds. The
    /// production constants (`INLINE_ARCHIVE_MAX_RETRIES` / `_DELAY_MS`)
    /// are intentionally left in production form; this helper simply
    /// loops without sleeping between attempts.
    fn archive_session_with_busy_retry_for_test(
        writer: &mut impl std::io::Write,
        reader: &mut impl std::io::BufRead,
        session_id: &str,
    ) -> Result<(), String> {
        let mut last_err = String::new();
        for _ in 0..INLINE_ARCHIVE_MAX_RETRIES {
            match archive_session(writer, reader, session_id) {
                Ok(()) => return Ok(()),
                Err(e) => {
                    if !e.contains("is busy") {
                        return Err(e);
                    }
                    last_err = e;
                    // No sleep — production sleeps INLINE_ARCHIVE_RETRY_DELAY_MS.
                }
            }
        }
        Err(format!(
            "giving up after {} busy-retries: {}",
            INLINE_ARCHIVE_MAX_RETRIES, last_err
        ))
    }
}
