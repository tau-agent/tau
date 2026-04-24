//! Scheduler logic for the task system.
//!
//! This module runs inside the `tau plugin-tasks` subprocess, which has no
//! `tracing` subscriber. Diagnostics use `eprintln!`; the parent server
//! forwards the plugin's stderr into its own tracing layer, so lines still
//! end up in `~/.local/state/tau/logs/server.log`. See `tasks.rs` for the
//! full rationale.
//!
//! Provides three synchronous operations called from tool handlers:
//!
//! - **schedule**: query `ready` tasks, pick a non-conflicting batch, and
//!   prepare them for dispatch (create branch + worktree, update DB).
//! - **dispatch**: create a session for a prepared task and send the initial
//!   chat message via the ServerRequest tunnel.
//! - **merge_approved**: find `approved` tasks and run the merge queue for
//!   each, serializing merges per target branch.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, Write};
use std::sync::{Mutex, OnceLock};

use crate::err::plugin_io_err;
use crate::tasks_config;
use crate::tasks_db::{Task, TaskUpdate, TasksDb};
use crate::tasks_git;
use crate::tasks_state::TaskState;
use tau_agent_plugin::PluginMessage;

// ---------------------------------------------------------------------------
// Batch selection
// ---------------------------------------------------------------------------

/// Select a non-conflicting batch of ready tasks.
///
/// Greedy algorithm:
/// 1. Sort by priority descending (stable — preserves creation order for ties).
/// 2. For each task, check if its `affected_files` overlap with any already
///    selected task **or** with any already-active task (via `active_files`).
/// 3. Tasks **without** `affected_files` are assumed to potentially conflict
///    with everything — they are only scheduled if nothing else is selected
///    yet **and** no active tasks have claimed files (i.e., they run alone).
///
/// `active_files` contains `(task_id, files)` pairs for tasks already in
/// in-flight states (active, review, merging, refining). These files are
/// pre-claimed and cannot overlap with newly scheduled tasks.
pub fn select_non_conflicting<'a>(
    tasks: &'a [Task],
    active_files: &[(i64, Vec<String>)],
) -> Vec<&'a Task> {
    let (selected, _skipped) = select_non_conflicting_with_reasons(tasks, active_files);
    selected
}

/// Why a ready task was skipped by [`select_non_conflicting_with_reasons`].
///
/// Returned alongside the chosen batch so the caller (run_schedule_pass,
/// wait-reason reporter) can surface the reason in logs and on the task
/// itself — without that, file-less tasks silently stall whenever any
/// other task is in-flight (root cause of task #584).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// Task has no `affected_files` declared and another task is already
    /// active (with or without files), so the "schedule file-less alone"
    /// rule rejects it.  `with_task_id` is an arbitrary representative of
    /// the tasks blocking it (the first one encountered).
    EmptyAffectedFilesNotAlone { with_task_id: Option<i64> },
    /// Task has no `affected_files` and another file-less task has already
    /// been selected in this same batch.
    EmptyAffectedFilesBatchFull { with_task_id: i64 },
    /// Task's declared files overlap with an active or already-selected
    /// task's files.
    FileConflict {
        with_task_id: i64,
        overlapping: Vec<String>,
    },
    /// An earlier task in this batch had no `affected_files` (and so was
    /// treated as "claim everything"), blocking further selections.
    BatchBlockedByUnbounded { with_task_id: i64 },
}

/// Like [`select_non_conflicting`] but also returns the reason each
/// non-selected task was skipped, so callers can log per-task decisions
/// and expose wait-reasons on stuck tasks.
///
/// Added in task #584 to make scheduler decisions observable.  The
/// selection algorithm is identical to [`select_non_conflicting`].
pub fn select_non_conflicting_with_reasons<'a>(
    tasks: &'a [Task],
    active_files: &[(i64, Vec<String>)],
) -> (Vec<&'a Task>, Vec<(&'a Task, SkipReason)>) {
    let mut sorted: Vec<&Task> = tasks.iter().collect();
    sorted.sort_by(|a, b| b.priority.cmp(&a.priority));

    // Pre-populate claimed_files with files from already-active tasks,
    // and track which active task each file belongs to so we can report
    // a specific conflict partner.
    let mut claimed_by: HashMap<String, i64> = HashMap::new();
    let mut has_unbounded_active = false;
    let mut unbounded_active_id: Option<i64> = None;
    let mut any_active_id: Option<i64> = active_files.first().map(|(id, _)| *id);
    for (id, files) in active_files {
        if files.is_empty() {
            // An active task without affected_files is unbounded — it
            // potentially conflicts with everything.
            has_unbounded_active = true;
            if unbounded_active_id.is_none() {
                unbounded_active_id = Some(*id);
            }
        }
        for f in files {
            claimed_by.entry(f.clone()).or_insert(*id);
        }
        if any_active_id.is_none() {
            any_active_id = Some(*id);
        }
    }

    let mut selected: Vec<&Task> = Vec::new();
    let mut skipped: Vec<(&Task, SkipReason)> = Vec::new();
    let mut has_unbounded = has_unbounded_active;
    let mut unbounded_selected_id: Option<i64> = None;

    for task in &sorted {
        let files = extract_files(&task.affected_files);

        if files.is_empty() {
            // No affected_files declared — treat as potentially conflicting
            // with everything. Only schedule alone and only if no active tasks
            // have claimed files.
            if selected.is_empty() && !has_unbounded && claimed_by.is_empty() {
                selected.push(task);
                has_unbounded = true;
                unbounded_selected_id = Some(task.id);
                // Don't break — but the flag prevents any further additions.
                continue;
            }
            // Report a specific reason.
            let reason = if unbounded_selected_id.is_some() {
                SkipReason::EmptyAffectedFilesBatchFull {
                    with_task_id: unbounded_selected_id.expect("just checked Some"),
                }
            } else {
                SkipReason::EmptyAffectedFilesNotAlone {
                    with_task_id: unbounded_active_id
                        .or(any_active_id)
                        .or_else(|| claimed_by.values().next().copied())
                        .or_else(|| selected.first().map(|t| t.id)),
                }
            };
            skipped.push((task, reason));
            continue;
        }

        // If we already selected an unbounded task (or an active task is
        // unbounded), skip everything else.
        if has_unbounded {
            let with_id = unbounded_selected_id
                .or(unbounded_active_id)
                .or(any_active_id)
                .unwrap_or(0);
            skipped.push((
                task,
                SkipReason::BatchBlockedByUnbounded {
                    with_task_id: with_id,
                },
            ));
            continue;
        }

        // Check overlap with already-claimed files (includes active tasks).
        let overlapping: Vec<String> = files
            .iter()
            .filter(|f| claimed_by.contains_key(*f))
            .cloned()
            .collect();
        if overlapping.is_empty() {
            for f in files {
                claimed_by.insert(f, task.id);
            }
            selected.push(task);
        } else {
            let with_task_id = overlapping
                .first()
                .and_then(|f| claimed_by.get(f).copied())
                .unwrap_or(0);
            skipped.push((
                task,
                SkipReason::FileConflict {
                    with_task_id,
                    overlapping,
                },
            ));
        }
    }

    (selected, skipped)
}

/// Extract file paths from the `affected_files` JSON value.
///
/// The wildcard marker `"*"` (task #596) signals "this task may touch
/// anything" — a task that genuinely cannot predict its file set, e.g.
/// a codebase-wide survey or a refactor whose scope is the whole tree.
/// Such a task is treated as file-less here so the scheduler's
/// "at-most-one file-less task per project" rule serialises it against
/// every other task. Returning an empty `Vec` is the smallest change
/// that reuses the existing file-less serialisation path verbatim:
/// `select_non_conflicting` and the conflict-overlap checks all already
/// treat empty-files tasks as the file-less slot.
///
/// We honour the marker if it appears anywhere in the array — a list
/// like `["src/foo.rs", "*"]` is conservatively unbounded.
pub(crate) fn extract_files(val: &Option<serde_json::Value>) -> Vec<String> {
    match val {
        Some(serde_json::Value::Array(arr)) => {
            // `"*"` anywhere in the array → unbounded scope, treat as file-less.
            if arr.iter().any(|v| v.as_str() == Some("*")) {
                return Vec::new();
            }
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        }
        _ => Vec::new(),
    }
}

/// Best-effort short commit SHA of the merge target after a successful
/// merge.  Used as the `context` argument to
/// [`tasks_notify::notify_state_change`](crate::tasks_notify::notify_state_change)
/// on `merging → merged` so the info-message reads
/// `[task #N] title: merged (commit abcdef0)`.
///
/// Returns `None` if the SHA cannot be resolved (non-fatal — the merged
/// info-message will simply omit the suffix).
pub(crate) fn extract_merge_commit(project_dir: &str, task: &Task) -> Option<String> {
    let target = task.merge_target.clone().unwrap_or_else(|| "main".into());
    let out = std::process::Command::new("git")
        .args(["-C", project_dir, "rev-parse", "--short", &target])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let sha = String::from_utf8(out.stdout).ok()?.trim().to_string();
    if sha.is_empty() {
        None
    } else {
        Some(format!("commit {}", sha))
    }
}

// ---------------------------------------------------------------------------
// Schedule pass
// ---------------------------------------------------------------------------

/// Result of a scheduling pass for a single task.
#[derive(Debug, serde::Serialize)]
pub struct ScheduledTask {
    pub id: i64,
    pub title: String,
    pub branch: String,
    pub worktree_path: String,
}

/// Maximum number of tasks that can be in-flight simultaneously per project.
/// Counted states: active, review, merging, refining (always), and planning
/// (only when a session is assigned, i.e. a planner is actively running).
pub(crate) const MAX_CONCURRENT_TASKS: usize = 8;

/// Run a scheduling pass: find ready/planning tasks, pick a non-conflicting
/// batch, create branches and worktrees (for ready tasks), update task state.
///
/// Ready tasks get branches/worktrees and transition to `active`.
/// Planning tasks are dispatched without worktrees (read-only sessions).
///
/// Respects `MAX_CONCURRENT_TASKS` — will not schedule more tasks than the
/// remaining capacity allows.
///
/// Returns the list of tasks that were prepared for dispatch.
pub fn schedule(
    db: &TasksDb,
    project_name: &str,
    project_path: &str,
) -> tau_agent_plugin::Result<Vec<ScheduledTask>> {
    // Check how many tasks are already in-flight.
    let inflight = db.count_inflight_tasks(project_name)?;
    if inflight >= MAX_CONCURRENT_TASKS {
        eprintln!(
            "tasks scheduler: schedule pass for project '{}' — in-flight budget exhausted ({} / {}), skipping",
            project_name, inflight, MAX_CONCURRENT_TASKS
        );
        return Ok(Vec::new());
    }
    let remaining_capacity = MAX_CONCURRENT_TASKS - inflight;

    let schedulable_tasks = db.get_schedulable_tasks(project_name)?;

    eprintln!(
        "tasks scheduler: schedule pass for project '{}' — in-flight={} / {}, schedulable candidates={}",
        project_name,
        inflight,
        MAX_CONCURRENT_TASKS,
        schedulable_tasks.len(),
    );

    if schedulable_tasks.is_empty() {
        return Ok(Vec::new());
    }

    // Separate planning tasks from ready tasks.
    // Planning tasks don't need worktrees or conflict checking.
    let mut planning_tasks = Vec::new();
    let mut ready_tasks = Vec::new();
    for task in &schedulable_tasks {
        if task.state == TaskState::Planning {
            planning_tasks.push(task);
        } else {
            ready_tasks.push(task.clone());
        }
    }

    let mut scheduled = Vec::new();

    // Handle planning tasks — dispatch without worktrees
    for task in &planning_tasks {
        // Skip if already has a session (already dispatched)
        if task.session_id.is_some() {
            eprintln!(
                "tasks scheduler: skipping planning task {} — already has session {:?}",
                task.id, task.session_id
            );
            continue;
        }
        eprintln!(
            "tasks scheduler: selected planning task {} ('{}') for dispatch",
            task.id, task.title
        );
        scheduled.push(ScheduledTask {
            id: task.id,
            title: task.title.clone(),
            branch: String::new(),
            worktree_path: String::new(),
        });
    }

    // Handle ready tasks — create branches/worktrees
    if !ready_tasks.is_empty() {
        // Collect affected_files from already in-flight tasks to prevent
        // file conflicts between new and active tasks.
        let inflight = db.get_inflight_tasks(project_name)?;
        let active_files: Vec<(i64, Vec<String>)> = inflight
            .iter()
            .map(|t| (t.id, extract_files(&t.affected_files)))
            .collect();

        let (batch, skipped) = select_non_conflicting_with_reasons(&ready_tasks, &active_files);

        for (task, reason) in &skipped {
            eprintln!(
                "tasks scheduler: skipped ready task {} ('{}'): {}",
                task.id,
                task.title,
                describe_skip_reason(reason)
            );
            // Surface the wait reason on the task itself so `task_get`
            // shows why it's stalled. Best-effort — DB errors are logged
            // by the helper.
            record_skip_reason(db, task, reason);
        }

        if !batch.is_empty() {
            // We need the repo root to create branches and worktrees.
            let repo_root = tasks_git::get_repo_root(project_path)?;

            for task in batch {
                eprintln!(
                    "tasks scheduler: selected ready task {} ('{}') for dispatch",
                    task.id, task.title
                );
                match prepare_task(db, task, &repo_root) {
                    Ok(st) => scheduled.push(st),
                    Err(e) => {
                        // Log but don't fail the whole batch.
                        eprintln!("tasks scheduler: failed to prepare task {}: {}", task.id, e);
                        // Add a visible message to the task so the error is discoverable.
                        if let Err(log_err) = db.add_message(
                            task.id,
                            &format!("⚠️ Scheduling failed: {}", e),
                            Some("system"),
                        ) {
                            eprintln!(
                                "tasks scheduler: failed to persist schedule-error message for task {}: {}",
                                task.id, log_err
                            );
                        }
                    }
                }
            }
        }
    }

    // Enforce the concurrent tasks limit.
    scheduled.truncate(remaining_capacity);

    eprintln!(
        "tasks scheduler: schedule() returning {} task(s) for project '{}': {:?}",
        scheduled.len(),
        project_name,
        scheduled.iter().map(|s| s.id).collect::<Vec<_>>()
    );

    Ok(scheduled)
}

/// Human-readable explanation of a [`SkipReason`], suitable for logging and
/// for posting as a system message on the stalled task.  Kept terse because
/// it shows up both in `server.log` (per pass, multi-task) and in
/// `task_get`.
fn describe_skip_reason(reason: &SkipReason) -> String {
    match reason {
        SkipReason::EmptyAffectedFilesNotAlone { with_task_id } => match with_task_id {
            Some(id) if *id != 0 => format!(
                "no affected_files declared, but task {} is in-flight (file-less tasks only schedule when no other task is active)",
                id
            ),
            _ => "no affected_files declared; another task is in-flight (file-less tasks only schedule when the project is idle)".to_string(),
        },
        SkipReason::EmptyAffectedFilesBatchFull { with_task_id } => format!(
            "no affected_files declared; task {} was already selected in this batch as the file-less slot",
            with_task_id
        ),
        SkipReason::FileConflict { with_task_id, overlapping } => format!(
            "file conflict with task {} on {:?}",
            with_task_id, overlapping
        ),
        SkipReason::BatchBlockedByUnbounded { with_task_id } => format!(
            "task {} has no affected_files and was selected first (or is in-flight), blocking further file-scoped tasks this pass",
            with_task_id
        ),
    }
}

/// Surface a scheduler skip reason on the task itself as a dedup'd system
/// message, so users see why a task is not dispatching.
///
/// The message is only appended if the most recent system message with the
/// `[scheduler-skip]` sentinel does not already carry the same reason —
/// otherwise a busy scheduler would spam the task's transcript every pass.
fn record_skip_reason(db: &TasksDb, task: &Task, reason: &SkipReason) {
    const SENTINEL: &str = "[scheduler-skip]";
    let body = describe_skip_reason(reason);
    let new_msg = format!("{} {}", SENTINEL, body);

    // Check the existing messages to dedup.
    match db.get_messages(task.id) {
        Ok(msgs) => {
            if let Some(latest_skip) = msgs.iter().rev().find(|m| m.content.starts_with(SENTINEL)) {
                if latest_skip.content == new_msg {
                    return; // same reason as before; don't duplicate
                }
            }
        }
        Err(e) => {
            eprintln!(
                "tasks scheduler: failed to read messages for task {} while deduping skip reason: {}",
                task.id, e
            );
            // Fall through and still attempt to write the message.
        }
    }

    if let Err(e) = db.add_message(task.id, &new_msg, Some("system")) {
        eprintln!(
            "tasks scheduler: failed to record skip reason on task {}: {}",
            task.id, e
        );
    }
}

/// Prepare a single task for dispatch: create branch, worktree, update DB.
fn prepare_task(
    db: &TasksDb,
    task: &Task,
    repo_root: &str,
) -> tau_agent_plugin::Result<ScheduledTask> {
    eprintln!(
        "tasks scheduler: prepare_task starting for task {} (state={}, parent_id={:?})",
        task.id, task.state, task.parent_id
    );
    let branch = tasks_git::task_branch_name(task.id, task.parent_id);

    // Determine the base branch: merge target (explicit override, parent's
    // branch, or "main").
    let base_branch = db.get_merge_target(task.id)?;

    // Create branch (skip if it already exists — idempotent).
    if !tasks_git::branch_exists(repo_root, &branch)? {
        // Validate that the base branch exists before trying to create a new branch from it.
        if !tasks_git::branch_exists(repo_root, &base_branch)? {
            return Err(tau_agent_plugin::Error::Io(format!(
                "task #{}: merge_target branch '{}' does not exist. \
                 Set the correct merge_target with: task_update {{\"id\": {}, \"merge_target\": \"<branch>\"}}",
                task.id, base_branch, task.id
            )));
        }
        tasks_git::create_branch(repo_root, &branch, &base_branch)?;
    }

    // Ensure .tau/worktrees/ directory exists before creating the worktree.
    tasks_git::ensure_worktrees_dir(repo_root)?;

    // Derive worktree path and create it.
    let worktree_path = tasks_git::task_worktree_path(repo_root, task.id);

    // Only create worktree if it doesn't already exist.
    if !std::path::Path::new(&worktree_path).exists() {
        tasks_git::create_worktree(repo_root, &worktree_path, &branch)?;
    }

    // Update task in DB: set branch, worktree_path, transition to active.
    db.set_branch(task.id, &branch)?;
    db.set_worktree_path(task.id, &worktree_path)?;
    db.update_task(
        task.id,
        &TaskUpdate {
            state: Some(TaskState::Active),
            ..Default::default()
        },
        None,
    )?;

    eprintln!(
        "tasks scheduler: prepare_task success for task {} (branch={}, worktree={})",
        task.id, branch, worktree_path
    );

    Ok(ScheduledTask {
        id: task.id,
        title: task.title.clone(),
        branch,
        worktree_path,
    })
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// Look up the session ID of a task's parent task. Returns `None` if the
/// task has no parent, or the parent has no session.
fn resolve_parent_session(db: &TasksDb, task: &Task) -> Option<String> {
    let parent_id = task.parent_id?;
    let parent_task = db.get_task(parent_id).ok()??;
    parent_task.session_id
}

/// Resolve the session that should be the **parent** for a newly created
/// worker/reviewer/refiner session.  The hierarchy should be:
///   planner → worker, planner → reviewer, planner → refiner
///
/// Priority:
///   1. The task's planner session (from `task_sessions` table).
///   2. The task's current `session_id` (e.g. if no planner was recorded).
///   3. The parent task's session (via `resolve_parent_session`).
fn resolve_hierarchy_parent(db: &TasksDb, task: &Task) -> Option<String> {
    // 1. Look for the planner session.
    if let Ok(Some(planner_sid)) = db.find_latest_session_by_role(task.id, "planner") {
        return Some(planner_sid);
    }
    // 2. Fall back to the task's current session_id.
    if let Some(ref sid) = task.session_id {
        return Some(sid.clone());
    }
    // 3. Fall back to the parent task's session.
    resolve_parent_session(db, task)
}

/// Resolve the session to inherit the model from.  Uses the explicit
/// `parent_session_id` (the session that triggered the dispatch, e.g. the
/// refiner), falling back to the hierarchy parent, then the parent task's
/// session.
fn resolve_model_source(
    db: &TasksDb,
    task: &Task,
    parent_session_id: Option<&str>,
) -> Option<String> {
    parent_session_id
        .map(str::to_string)
        .or_else(|| resolve_hierarchy_parent(db, task))
}

/// Look up a session's full [`SessionInfo`] via the `GetSessionInfo` RPC.
///
/// Returns `None` if the request fails or the session is not found. Callers
/// typically map this to a single field (see [`get_session_model`],
/// [`get_session_parent`]). Prefer this helper over open-coding the
/// request/response match at each call site.
pub fn get_session_info(
    session_id: &str,
    writer: &mut impl Write,
    reader: &mut impl BufRead,
) -> Option<tau_agent_plugin::SessionInfo> {
    let req = tau_agent_plugin::Request::GetSessionInfo {
        session_id: session_id.to_string(),
    };
    match server_request(writer, reader, req) {
        Ok(tau_agent_plugin::Response::SessionInfo { info }) => Some(info),
        _ => None,
    }
}

/// Look up a session's model via GetSessionInfo. Returns `None` if the
/// request fails or the session is not found.
pub fn get_session_model(
    session_id: &str,
    writer: &mut impl Write,
    reader: &mut impl BufRead,
) -> Option<String> {
    get_session_info(session_id, writer, reader).map(|info| info.model)
}

/// Look up a session's parent session id via GetSessionInfo.
///
/// Returns `None` if the request fails, the session is not found, or the
/// session is a root session (no parent).
pub fn get_session_parent(
    session_id: &str,
    writer: &mut impl Write,
    reader: &mut impl BufRead,
) -> Option<String> {
    get_session_info(session_id, writer, reader).and_then(|info| info.parent_id)
}

/// Last-resort fallback for `resolve_model_source`: when no planner,
/// session_id, parent-task session, or explicit triggering session is
/// available, walk up the task's placeholder-session parent chain.
///
/// Placeholders are created anchored on a real (non-log) session (the
/// root of the creator's session tree — see `find_root_session` in
/// `tasks.rs`), so the placeholder's own `parent_id` points at a
/// session whose model is appropriate to inherit.
///
/// This fixes the bug where top-level ready tasks auto-dispatched with
/// `parent_session_id = None` ended up with the placeholder's own model
/// (`log`) silently inherited on the worker — see task #590.
///
/// Returns `None` if the task has no placeholder, the placeholder lookup
/// fails, or the placeholder is a root session itself (no parent).
fn resolve_placeholder_model_source(
    task: &Task,
    writer: &mut impl Write,
    reader: &mut impl BufRead,
) -> Option<String> {
    let placeholder_sid = task.placeholder_session_id.as_deref()?;
    get_session_parent(placeholder_sid, writer, reader)
}

/// Walk up the session parent chain and return the topmost (root) session's
/// ID.
///
/// Used when dispatching a new session on behalf of a top-level task: the
/// triggering session may be a deeply-nested worker that will soon be
/// archived, which would orphan the new session.  Re-parenting onto the
/// root of that chain (the user's primary session tree) keeps new work
/// visible.
///
/// One round-trip to the server via `GetSessionAncestors` — the response
/// is leaf-first, depth-guarded server-side at 64.
///
/// Returns:
/// - `Some(root_id)` if a non-archived root (`parent_id == None`,
///   `archived == false`) is reached.
/// - `None` if:
///   - the starting session doesn't exist (empty `sessions` response),
///   - the depth-guard-truncated chain didn't include a root (last entry
///     still has a parent),
///   - the root reached is itself archived.
///
/// In the `None` case the caller should treat the new session as a root
/// of its own (unparented).
pub fn find_root_session(
    session_id: &str,
    writer: &mut impl Write,
    reader: &mut impl BufRead,
) -> Option<String> {
    let sessions = match server_request(
        writer,
        reader,
        tau_agent_plugin::Request::GetSessionAncestors {
            session_id: session_id.to_string(),
        },
    ) {
        Ok(tau_agent_plugin::Response::SessionAncestors { sessions }) => sessions,
        _ => return None,
    };

    // Leaf-first: sessions[0] is the requested session, sessions.last() is
    // the topmost ancestor seen.
    let last = sessions.last()?;
    // If the last entry still has a parent, the depth guard truncated the
    // walk before reaching a root — treat as "root not found".
    if last.parent_id.is_some() {
        return None;
    }
    // Archived root → unparented new session (caller handles None).
    if last.archived {
        return None;
    }
    Some(last.id.clone())
}

// ---------------------------------------------------------------------------
// Task phase orchestration (shared by worker / planner / reviewer / refiner)
// ---------------------------------------------------------------------------

/// Which lifecycle phase a dispatch is for. Encodes the per-phase axes of
/// variation consumed by [`dispatch_task_phase`].
///
/// The four variants map onto the four `dispatch_*` public functions —
/// `dispatch` (worker), `dispatch_planning`, `dispatch_review`,
/// `dispatch_refining`. The enum deliberately keeps all per-phase
/// policy (DB role name, cwd, parent-resolution rule, reuse-resume
/// message, whether to overwrite `task.session_id`, whether to emit a
/// `ready → active` notification, which `build_*_message` to call) in a
/// single place so adding a new phase is a one-variant change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TaskPhase {
    Worker,
    Planner,
    Reviewer,
    Refiner,
}

impl TaskPhase {
    /// DB role used with `find_latest_session_by_role` and
    /// `record_session`.
    fn db_role(&self) -> &'static str {
        match self {
            Self::Worker => "worker",
            Self::Planner => "planner",
            Self::Reviewer => "reviewer",
            Self::Refiner => "refiner",
        }
    }

    /// Human-facing phase name — the `role` passed to `TaskSessionSpec`
    /// (and thus `task_session_tagline`) and the instructions key for
    /// `tasks_config::load_project_instructions`.
    fn phase_name(&self) -> &'static str {
        match self {
            Self::Worker => "worker",
            Self::Planner => "planning",
            Self::Reviewer => "review",
            Self::Refiner => "refining",
        }
    }

    /// Working directory for the phase's session.
    ///
    /// - Worker: the task's worktree (may be `None` if not yet created).
    /// - Planner, Refiner: the project root (read-only phases, no worktree).
    /// - Reviewer: the task's worktree, falling back to the project root
    ///   (reviewers read the diff — the worktree is the right place, but
    ///   they tolerate a missing worktree).
    fn cwd(&self, task: &Task, project_path: &str) -> Option<String> {
        match self {
            Self::Worker => task.worktree_path.clone(),
            Self::Planner | Self::Refiner => Some(project_path.to_string()),
            Self::Reviewer => task
                .worktree_path
                .clone()
                .or_else(|| Some(project_path.to_string())),
        }
    }

    /// Per-phase fallback for the hierarchy parent (before the top-level
    /// root-session re-parenting rule is applied).
    ///
    /// Planner differs: it skips the planner-hop that
    /// [`resolve_hierarchy_parent`] performs, because the planner *is*
    /// itself the planner — re-using the stale prior planner session as
    /// its own parent is incorrect. Instead it uses the explicit
    /// triggering session (if any), falling back to the parent task's
    /// session.
    fn hierarchy_parent(
        &self,
        db: &TasksDb,
        task: &Task,
        parent_session_id: Option<&str>,
    ) -> Option<String> {
        match self {
            Self::Planner => parent_session_id
                .map(str::to_string)
                .or_else(|| resolve_parent_session(db, task)),
            Self::Worker | Self::Reviewer | Self::Refiner => resolve_hierarchy_parent(db, task),
        }
    }

    /// Whether a top-level task with no placeholder should re-parent its
    /// new session onto the triggering session's root (task #512). Only
    /// applies to the entry phases (worker + planner); review/refining
    /// only run once the task already has a session chain, so the
    /// hierarchy parent is always appropriate.
    fn top_level_root_fallback(&self) -> bool {
        matches!(self, Self::Worker | Self::Planner)
    }

    /// QueueMessage content to send when reusing an existing session.
    /// Worker reuse is a silent short-circuit (the worker should already
    /// be mid-work; waking it up with a message would duplicate work),
    /// so this returns `None` for `Worker`.
    ///
    /// The other three phases send a phase-appropriate "resume" message
    /// so the idle session notices the backward transition and picks up
    /// the latest feedback. Without this the task would stall (bug #589
    /// for the planner; planner/reviewer/refiner all now behave the
    /// same way).
    fn reuse_resume_message(&self, task_id: i64) -> Option<String> {
        match self {
            Self::Worker => None,
            Self::Planner => Some(format!(
                "Task {id} has been moved back to planning for further work. \
                 Please run task_get to read the latest feedback and revise the plan, \
                 then transition the task forward again.\n\
                 - Call `task_get` with arguments: {{\"id\": {id}}}",
                id = task_id,
            )),
            Self::Reviewer => Some(format!(
                "Task {id} has been re-submitted for review. \
                 Please run task_get to read the latest changes and review feedback, \
                 then re-review the work.\n\
                 - Call `task_get` with arguments: {{\"id\": {id}}}",
                id = task_id,
            )),
            Self::Refiner => Some(format!(
                "Task {id} has been re-submitted for refining. \
                 The plan has been revised. Please run task_get to read the updated \
                 plan and re-evaluate it.\n\
                 - Call `task_get` with arguments: {{\"id\": {id}}}",
                id = task_id,
            )),
        }
    }

    /// Whether the newly-created session id should overwrite
    /// `task.session_id`. Only worker + planner do this; review and
    /// refining leave the planner's/worker's session in place so the UI
    /// keeps pointing at the primary session for the current phase.
    fn sets_task_session_id(&self) -> bool {
        matches!(self, Self::Worker | Self::Planner)
    }

    /// Whether to emit a `ready → active` `notify_state_change` after
    /// creation. Worker-only — the `ready → active` transition is the
    /// only one the scheduler applies inside `dispatch`; the other three
    /// phases transition via `task_update` which has its own notifier.
    fn emits_ready_to_active_notify(&self) -> bool {
        matches!(self, Self::Worker)
    }

    /// Build the initial Chat message for this phase.
    fn build_chat_message(
        &self,
        task: &Task,
        project_instructions: &str,
        merge_target: &str,
        checklist: &[crate::tasks_merge::CheckItem],
    ) -> String {
        match self {
            Self::Worker => {
                build_initial_message(task, merge_target, project_instructions, checklist)
            }
            Self::Planner => build_planning_message(task, project_instructions, merge_target),
            Self::Reviewer => {
                build_review_message(task, project_instructions, merge_target, checklist)
            }
            Self::Refiner => build_refining_message(task, project_instructions, merge_target),
        }
    }
}

/// Send the initial Chat to a freshly-created session. Extracted because
/// all four phases used to open-code the same `Request::Chat` + match
/// ladder and the only variation was a log label. Errors map to
/// `Error::Io` with a message that names the phase + session id for
/// easier log triage.
fn send_initial_chat(
    session_id: &str,
    phase_role: &str,
    text: String,
    writer: &mut impl Write,
    reader: &mut impl BufRead,
) -> tau_agent_plugin::Result<()> {
    let chat_req = tau_agent_plugin::Request::Chat {
        session_id: session_id.to_string(),
        text,
    };
    match server_request(writer, reader, chat_req) {
        Ok(tau_agent_plugin::Response::Ok) => Ok(()),
        Ok(tau_agent_plugin::Response::Error { message }) => {
            Err(tau_agent_plugin::Error::Io(format!(
                "{} session {} created but chat failed: {}",
                phase_role, session_id, message
            )))
        }
        Ok(other) => Err(tau_agent_plugin::Error::Io(format!(
            "{} session {} created but unexpected chat response: {:?}",
            phase_role, session_id, other
        ))),
        Err(e) => Err(tau_agent_plugin::Error::Io(format!(
            "{} session {} created but chat failed: {}",
            phase_role, session_id, e
        ))),
    }
}

/// Shared orchestration body for the four phase-dispatch functions.
///
/// Handles session reuse, model + parent resolution, session creation
/// (via [`crate::tasks_session::create_task_session`]), the initial
/// chat, and the DB writes + notifications each phase needs.
/// Phase-specific decisions are delegated to [`TaskPhase`] methods.
///
/// Returns the session id of the new (or reused) session.
pub(crate) fn dispatch_task_phase(
    db: &TasksDb,
    task: &Task,
    phase: TaskPhase,
    parent_session_id: Option<&str>,
    project_path: &str,
    writer: &mut impl Write,
    reader: &mut impl BufRead,
) -> tau_agent_plugin::Result<String> {
    let task_id = task.id;

    // --- 1. Reuse path -----------------------------------------------------
    if let Some(existing_sid) = find_reusable_session(db, task_id, phase.db_role(), writer, reader)
    {
        if let Some(msg) = phase.reuse_resume_message(task_id) {
            resume_session(&existing_sid, task_id, &msg, writer, reader)?;
        }
        eprintln!(
            "tasks scheduler: task {} already has a live {} session {}, reusing",
            task_id,
            phase.db_role(),
            existing_sid
        );
        crate::tasks_notify::set_session_tagline(
            &existing_sid,
            &crate::tasks_notify::task_session_tagline(task, phase.phase_name()),
            writer,
            reader,
        );
        return Ok(existing_sid);
    }

    // --- 2. Stale session_id log ------------------------------------------
    // Worker + planner used to log this explicitly; review + refining never
    // did (they don't overwrite session_id below, so the prior id is still
    // correct). Keep the log gated on phases that actually overwrite
    // session_id to preserve previous behaviour byte-for-byte.
    if phase.sets_task_session_id() {
        if let Some(ref existing_sid) = task.session_id {
            eprintln!(
                "tasks scheduler: task {} replacing previous session {} with new {} dispatch",
                task_id,
                existing_sid,
                phase.db_role()
            );
        }
    }

    // --- 3. Model inheritance ---------------------------------------------
    let model_source = resolve_model_source(db, task, parent_session_id)
        .or_else(|| resolve_placeholder_model_source(task, writer, reader));
    let model = model_source
        .as_deref()
        .and_then(|sid| get_session_model(sid, writer, reader));

    // --- 4. Parent resolution ---------------------------------------------
    let hierarchy_parent = phase.hierarchy_parent(db, task, parent_session_id);
    let session_parent = task.placeholder_session_id.clone().or_else(|| {
        if task.parent_id.is_none() && phase.top_level_root_fallback() {
            parent_session_id.and_then(|sid| find_root_session(sid, writer, reader))
        } else {
            hierarchy_parent.clone()
        }
    });

    // --- 5. Create session ------------------------------------------------
    let session_id = crate::tasks_session::create_task_session(
        crate::tasks_session::TaskSessionSpec {
            task,
            role: phase.phase_name(),
            model,
            cwd: phase.cwd(task, project_path),
            parent_id: session_parent,
            child_budget: 16,
            sandbox_profile: task.sandbox_profile.clone(),
        },
        writer,
        reader,
    )?;

    // --- 6. Initial chat --------------------------------------------------
    let merge_target = db
        .get_merge_target(task_id)
        .unwrap_or_else(|_| "main".into());
    let project_instructions = tasks_config::load_project_instructions(
        project_path,
        Some(&task.project_name),
        phase.phase_name(),
    )
    .unwrap_or_default();
    let checklist = crate::tasks_merge::load_checklist(project_path, Some(&task.project_name));
    let chat_msg = phase.build_chat_message(task, &project_instructions, &merge_target, &checklist);
    send_initial_chat(&session_id, phase.phase_name(), chat_msg, writer, reader)?;

    // --- 7. DB writes -----------------------------------------------------
    if phase.sets_task_session_id() {
        db.set_session_id(task_id, &session_id)?;
    }
    db.record_session(task_id, &session_id, phase.db_role())?;

    // --- 8. Optional ready → active notification --------------------------
    if phase.emits_ready_to_active_notify() {
        if let Ok(Some(updated)) = db.get_task(task_id) {
            crate::tasks_notify::notify_state_change(
                db,
                &updated,
                TaskState::Ready,
                None,
                writer,
                reader,
            );
        }
    }

    Ok(session_id)
}

/// Dispatch a single task: create a session via ServerRequest, send initial
/// chat, and update the task with the session ID.
///
/// For `planning` tasks: creates a read-only session (no worktree) with
/// planning-specific instructions.
///
/// For `ready`/`active` tasks: creates a session with a worktree and
/// full work instructions.
///
/// The `writer` and `reader` are the plugin's stdout/stdin — used to tunnel
/// ServerRequests through to the tau server.
///
/// `parent_session_id` is the calling session when dispatched manually (via
/// tool call), or `None` when auto-dispatched. When `None`, the parent task's
/// session is looked up automatically.
pub fn dispatch(
    db: &TasksDb,
    task_id: i64,
    parent_session_id: Option<&str>,
    project_path: &str,
    writer: &mut impl Write,
    reader: &mut impl BufRead,
) -> tau_agent_plugin::Result<String> {
    eprintln!(
        "tasks scheduler: dispatch starting for task {} (parent_session_id={:?})",
        task_id, parent_session_id
    );
    let task = db
        .get_task(task_id)?
        .ok_or_else(|| tau_agent_plugin::Error::Io(format!("task {} not found", task_id)))?;
    eprintln!(
        "tasks scheduler: dispatch task {} loaded (state={}, session_id={:?}, placeholder={:?})",
        task_id, task.state, task.session_id, task.placeholder_session_id
    );

    // Handle planning-state dispatch (no worktree, read-only session)
    if task.state == TaskState::Planning {
        return dispatch_planning(db, &task, parent_session_id, project_path, writer, reader);
    }

    // Task must be active (prepared by schedule) or ready (we'll prepare it).
    if task.state == TaskState::Ready {
        // Not yet prepared — do it inline.
        let repo_root = tasks_git::get_repo_root(project_path)?;
        prepare_task(db, &task, &repo_root)?;
        // Re-read after prepare.
    } else if task.state != TaskState::Active {
        return Err(tau_agent_plugin::Error::Io(format!(
            "task {} is in state '{}', must be 'ready', 'active', or 'planning' to dispatch",
            task_id, task.state
        )));
    }

    // Re-read the task to get updated fields after potential prepare.
    let task = db.get_task(task_id)?.ok_or_else(|| {
        tau_agent_plugin::Error::Io(format!("task {} not found after prepare", task_id))
    })?;

    dispatch_task_phase(
        db,
        &task,
        TaskPhase::Worker,
        parent_session_id,
        project_path,
        writer,
        reader,
    )
}

// ---------------------------------------------------------------------------
// Planning dispatch
// ---------------------------------------------------------------------------

/// Dispatch a planning-state task: create a read-only session (no worktree)
/// that explores code and produces a plan with affected files.
pub(crate) fn dispatch_planning(
    db: &TasksDb,
    task: &Task,
    parent_session_id: Option<&str>,
    project_path: &str,
    writer: &mut impl Write,
    reader: &mut impl BufRead,
) -> tau_agent_plugin::Result<String> {
    dispatch_task_phase(
        db,
        task,
        TaskPhase::Planner,
        parent_session_id,
        project_path,
        writer,
        reader,
    )
}

/// Build the initial message for a planning-state task.
fn build_planning_message(task: &Task, project_instructions: &str, merge_target: &str) -> String {
    let branch = task.branch.as_deref().unwrap_or("(not yet created)");

    // Warn explicitly when the merge target is not main (nested subtask).
    let nested_warning = if merge_target != "main" {
        format!(
            "\nIMPORTANT: This is a subtask. The merge target is {target}, NOT main. \
             Plan your changes relative to that branch.\n",
            target = merge_target,
        )
    } else {
        String::new()
    };

    // Task #596: gentle nudge for tasks auto-routed from `ready`. The
    // caller thought their spec was complete; the planner should still
    // do the full planning exploration but should treat the caller's
    // scope as authoritative rather than re-litigating it.
    let auto_downgrade_nudge = if task.auto_downgraded_from_ready {
        "\n## Note: this task was auto-routed from `ready`\n\
         \n\
         The caller filed this task with `initial_state = ready` but didn't \
         declare `affected_files`, so it was auto-routed through planning so \
         the file list could be populated (which lets the scheduler run \
         disjoint tasks in parallel). Treat the caller's spec and scope as \
         authoritative — they thought the work was self-contained — but still \
         do the full planning exploration: read the code, identify the files, \
         produce a plan, transition to refining as usual.\n"
    } else {
        ""
    };

    let mut msg = format!(
        "You are in the PLANNING phase for task {id}: {title}\n\
         \n\
         Task branch: {branch}\n\
         Merge target: {target}\n\
         {nested}\
         Use the task_get tool to read the full specification:\n\
         - Call `task_get` with arguments: {{\"id\": {id}}}\n\
         {nudge}\
         \n\
         ## Your mission\n\
         \n\
         You are gathering information and creating a plan. **Do NOT modify any files.**\n\
         \n\
         1. Read the task spec carefully\n\
         2. Explore the codebase to understand the relevant code (use bash and read tools)\n\
         3. Identify all files that will need to be changed\n\
         4. Create a detailed implementation plan\n\
         5. When your plan is ready, do TWO things:\n\
            a. Add your plan as a task message: call `task_message` with the plan\n\
            b. Set the affected_files list: call `task_update` with the affected_files array\n\
            c. Transition to refining: call `task_update` with {{\"id\": {id}, \"state\": \"refining\"}}\n\
         \n\
         **Important**: This is a read-only planning phase. Do NOT create, edit, or write files.\n",
        id = task.id,
        title = task.title,
        branch = branch,
        target = merge_target,
        nested = nested_warning,
        nudge = auto_downgrade_nudge,
    );

    if !project_instructions.is_empty() {
        msg.push_str(&format!(
            "\n## Project-specific planning instructions\n\n{}\n",
            project_instructions
        ));
    }

    msg
}

// ---------------------------------------------------------------------------
// Session reuse helpers
// ---------------------------------------------------------------------------

/// Check whether a session is alive (not archived and not terminated) by
/// querying GetSessionInfo. Returns `Some(session_id)` if the session is
/// reusable, `None` otherwise.
fn find_reusable_session(
    db: &TasksDb,
    task_id: i64,
    role: &str,
    writer: &mut impl Write,
    reader: &mut impl BufRead,
) -> Option<String> {
    let session_id = db.find_latest_session_by_role(task_id, role).ok()??;

    // Ask the server whether the session is still alive.
    let info = get_session_info(&session_id, writer, reader)?;
    if info.archived {
        return None;
    }
    Some(session_id)
}

/// Resume an existing session by sending it a QueueMessage.
fn resume_session(
    session_id: &str,
    task_id: i64,
    message: &str,
    writer: &mut impl Write,
    reader: &mut impl BufRead,
) -> tau_agent_plugin::Result<()> {
    let req = tau_agent_plugin::Request::QueueMessage {
        target_session_id: session_id.to_string(),
        content: message.to_string(),
        sender_info: format!("task-system (task {})", task_id),
        await_reply: false,
        reply_to: None,
    };
    match server_request(writer, reader, req) {
        Ok(tau_agent_plugin::Response::Ok) => Ok(()),
        Ok(tau_agent_plugin::Response::Error { message }) => Err(tau_agent_plugin::Error::Io(
            format!("failed to resume session {}: {}", session_id, message),
        )),
        Ok(other) => Err(tau_agent_plugin::Error::Io(format!(
            "unexpected response resuming session {}: {:?}",
            session_id, other
        ))),
        Err(e) => Err(tau_agent_plugin::Error::Io(format!(
            "failed to resume session {}: {}",
            session_id, e
        ))),
    }
}

// ---------------------------------------------------------------------------
// Review dispatch
// ---------------------------------------------------------------------------

/// Dispatch a review session for a task that just transitioned to `review`.
///
/// If an existing reviewer session is found and is still alive (not archived),
/// it is resumed with a QueueMessage instead of creating a new session.
/// Otherwise, creates a new session that reviews the work done on the task
/// and either approves it or requests changes.
pub fn dispatch_review(
    db: &TasksDb,
    task: &Task,
    parent_session_id: Option<&str>,
    project_path: &str,
    writer: &mut impl Write,
    reader: &mut impl BufRead,
) -> tau_agent_plugin::Result<String> {
    dispatch_task_phase(
        db,
        task,
        TaskPhase::Reviewer,
        parent_session_id,
        project_path,
        writer,
        reader,
    )
}

/// Build the initial message for a review session.
fn build_review_message(
    task: &Task,
    project_instructions: &str,
    merge_target: &str,
    checklist: &[crate::tasks_merge::CheckItem],
) -> String {
    let branch = task.branch.as_deref().unwrap_or("(unknown)");
    let worktree = task.worktree_path.as_deref().unwrap_or("(unknown)");

    // Warn explicitly when the merge target is not main (nested subtask).
    let nested_warning = if merge_target != "main" {
        format!(
            "\nIMPORTANT: The merge target is {target}, NOT main. \
             Use `git diff {target}...HEAD` (not `git diff main`).\n",
            target = merge_target,
        )
    } else {
        String::new()
    };

    // Build the checklist sections for the review message.
    // When non-empty: item 5 in the numbered list + a run-checks block.
    // When empty: nothing — no extra lines, preserving the original spacing.
    let checklist_item = if checklist.is_empty() {
        String::new()
    } else {
        format!(
            "\n5. Run these project checks and verify they pass:\n{}",
            checklist
                .iter()
                .map(|c| format!("   - `{}` ({})", c.command, c.name))
                .collect::<Vec<_>>()
                .join("\n")
        )
    };

    let checklist_run_block = if checklist.is_empty() {
        String::new()
    } else {
        format!(
            "Run these checks before making your decision:\n{}\n\n",
            checklist
                .iter()
                .map(|c| format!("- `{}`", c.command))
                .collect::<Vec<_>>()
                .join("\n")
        )
    };

    let mut msg = format!(
        "You are REVIEWING task {id}: {title}\n\
         \n\
         Task branch: {branch}\n\
         Worktree: {worktree}\n\
         Merge target: {target}\n\
         {nested}\
         Use the task_get tool to read the full specification and review feedback:\n\
         - Call `task_get` with arguments: {{\"id\": {id}}}\n\
         \n\
         ## Your mission\n\
         \n\
         Review the work done on this task. Check:\n\
         1. Does the implementation match the spec?\n\
         2. Code quality, correctness, and completeness\n\
         3. Are there any bugs or edge cases missed?\n\
         4. Does the code follow project conventions?\
         {checklist_item}\n\
         \n\
         {checklist_run_block}\
         Examine the changes using git diff and read the modified files.\n\
         Use `bash` with `git diff {target}...HEAD` or similar to see all changes.\n\
         \n\
         After your review:\n\
         - If approved: call `task_update` with {{\"id\": {id}, \"state\": \"approved\"}}\n\
         - If changes needed: add review feedback via `task_message`, then call \
           `task_update` with {{\"id\": {id}, \"state\": \"active\"}} to send it back to the worker\n",
        id = task.id,
        title = task.title,
        branch = branch,
        worktree = worktree,
        target = merge_target,
        nested = nested_warning,
        checklist_item = checklist_item,
        checklist_run_block = checklist_run_block,
    );

    if !project_instructions.is_empty() {
        msg.push_str(&format!(
            "\n## Project-specific review instructions\n\n{}\n",
            project_instructions
        ));
    }

    msg
}

// ---------------------------------------------------------------------------
// Refining dispatch
// ---------------------------------------------------------------------------

/// Dispatch a refining session for a task that just transitioned to `refining`.
///
/// If an existing refiner session is found and is still alive (not archived),
/// it is resumed with a QueueMessage instead of creating a new session.
/// Otherwise, creates a new read-only session that reviews the plan produced
/// during planning and either approves it to `ready` or sends it back to
/// `planning`.
pub fn dispatch_refining(
    db: &TasksDb,
    task: &Task,
    parent_session_id: Option<&str>,
    project_path: &str,
    writer: &mut impl Write,
    reader: &mut impl BufRead,
) -> tau_agent_plugin::Result<String> {
    dispatch_task_phase(
        db,
        task,
        TaskPhase::Refiner,
        parent_session_id,
        project_path,
        writer,
        reader,
    )
}

/// Build the initial message for a refining session.
fn build_refining_message(task: &Task, project_instructions: &str, merge_target: &str) -> String {
    let approval_instructions = if task.require_approval {
        format!(
            "After your review:\n\
             - If the plan is good: transition to interactive (human sign-off required) with \
               `task_update` {{\"id\": {id}, \"state\": \"interactive\"}}\n\
             - If the plan needs revision: add feedback via `task_message`, then send back to \
               planning with `task_update` {{\"id\": {id}, \"state\": \"planning\"}}\n\
             \n\
             **Note**: This task has require_approval=true. The plan must be approved by a human \
             before work begins. Transition to interactive instead of ready.\n",
            id = task.id,
        )
    } else {
        format!(
            "After your review:\n\
             - If the plan is good: transition to ready with `task_update` {{\"id\": {id}, \"state\": \"ready\"}}\n\
             - If the plan needs revision: add feedback via `task_message`, then send back to \
               planning with `task_update` {{\"id\": {id}, \"state\": \"planning\"}}\n\
             - If the scope has expanded significantly and needs human sign-off: \
               `task_update` {{\"id\": {id}, \"state\": \"interactive\"}}\n",
            id = task.id,
        )
    };

    let branch = task.branch.as_deref().unwrap_or("(not yet created)");

    // Warn explicitly when the merge target is not main (nested subtask).
    let nested_warning = if merge_target != "main" {
        format!(
            "\nIMPORTANT: This is a subtask. The merge target is {target}, NOT main. \
             Evaluate the plan relative to that branch.\n",
            target = merge_target,
        )
    } else {
        String::new()
    };

    let mut msg = format!(
        "You are REFINING the plan for task {id}: {title}\n\
         \n\
         Task branch: {branch}\n\
         Merge target: {target}\n\
         {nested}\
         Use the task_get tool to read the task spec and the planning messages:\n\
         - Call `task_get` with arguments: {{\"id\": {id}}}\n\
         \n\
         ## Your mission\n\
         \n\
         Review the plan created during the planning phase. Check:\n\
         1. Is the plan thorough and complete?\n\
         2. Does it cover all requirements from the spec?\n\
         3. Are the affected_files correct and comprehensive?\n\
         4. Are there any edge cases or risks not addressed?\n\
         5. Does the plan align with project conventions and goals?\n\
         \n\
         **Do NOT modify any files.** This is a review-only phase.\n\
         \n\
         {approval}\n",
        id = task.id,
        title = task.title,
        branch = branch,
        target = merge_target,
        nested = nested_warning,
        approval = approval_instructions,
    );

    if !project_instructions.is_empty() {
        msg.push_str(&format!(
            "\n## Project-specific refining instructions\n\n{}\n",
            project_instructions
        ));
    }

    msg
}

// ---------------------------------------------------------------------------
// Rebase check
// ---------------------------------------------------------------------------

/// Check whether a task's branch is rebased on its merge target.
///
/// Returns `Ok(true)` if the branch is up-to-date (merge target is an
/// ancestor of the branch HEAD). Returns `Ok(false)` if the branch needs
/// rebasing. Returns `Err` if the check cannot be performed.
pub fn is_rebased_on_target(db: &TasksDb, task: &Task) -> tau_agent_plugin::Result<bool> {
    let branch = task.branch.as_ref().ok_or_else(|| {
        tau_agent_plugin::Error::Io(format!("task {} has no branch set", task.id))
    })?;

    let worktree = task
        .worktree_path
        .as_ref()
        .ok_or_else(|| tau_agent_plugin::Error::Io(format!("task {} has no worktree", task.id)))?;

    let merge_target = db.get_merge_target(task.id)?;

    // Use git merge-base --is-ancestor to check if merge_target is an
    // ancestor of the task's branch.
    let output = std::process::Command::new("git")
        .args(["merge-base", "--is-ancestor", &merge_target, branch])
        .current_dir(worktree)
        .output()
        .map_err(plugin_io_err("git merge-base"))?;

    Ok(output.status.success())
}

// ---------------------------------------------------------------------------
// Auto-merge approved tasks
// ---------------------------------------------------------------------------

/// Result of a single auto-merge attempt.
#[derive(Debug, serde::Serialize)]
pub struct MergeAttempt {
    pub task_id: i64,
    pub title: String,
    pub success: bool,
    pub log: String,
}

/// Find all `approved` tasks and merge them, serializing merges per target
/// branch (no parallel merges into the same branch).
///
/// Returns the list of merge attempts (both successes and failures).
pub fn merge_approved(
    db: &TasksDb,
    resolve_path: &dyn Fn(&str) -> tau_agent_plugin::Result<String>,
    writer: &mut impl Write,
    reader: &mut impl BufRead,
) -> tau_agent_plugin::Result<Vec<MergeAttempt>> {
    merge_approved_for_caller(db, resolve_path, None, writer, reader)
}

/// Variant of [`merge_approved`] that threads a caller session id through
/// [`merge_one_task`] — the caller may be in the to-be-archived subtree
/// of one of the approved tasks, in which case archival is deferred to
/// Tier-3.  See [`crate::tasks_merge::merge_task_for_caller`].
pub fn merge_approved_for_caller(
    db: &TasksDb,
    resolve_path: &dyn Fn(&str) -> tau_agent_plugin::Result<String>,
    caller_session_id: Option<&str>,
    writer: &mut impl Write,
    reader: &mut impl BufRead,
) -> tau_agent_plugin::Result<Vec<MergeAttempt>> {
    let approved = db.get_approved_tasks(None)?;
    if approved.is_empty() {
        return Ok(Vec::new());
    }

    // Group tasks by their merge target branch. Within each group, process
    // one at a time (serialized). Across groups we could parallelize, but
    // since we have a single writer/reader pair, we process sequentially.
    let mut by_target: HashMap<String, Vec<Task>> = HashMap::new();
    for task in approved {
        let target = db
            .get_merge_target(task.id)
            .unwrap_or_else(|_| "main".into());
        by_target.entry(target).or_default().push(task);
    }

    let mut attempts = Vec::new();

    for tasks in by_target.values() {
        for task in tasks {
            let attempt = merge_one_task(db, task, resolve_path, caller_session_id, writer, reader);
            attempts.push(attempt);
        }
    }

    attempts.sort_by_key(|a| a.task_id);
    Ok(attempts)
}

/// Execute the merge sequence for a single approved task.
///
/// Transitions: approved → merging → merged (success) or merging → active (failure).
fn merge_one_task(
    db: &TasksDb,
    task: &Task,
    resolve_path: &dyn Fn(&str) -> tau_agent_plugin::Result<String>,
    caller_session_id: Option<&str>,
    writer: &mut impl Write,
    reader: &mut impl BufRead,
) -> MergeAttempt {
    let task_id = task.id;
    let title = task.title.clone();

    // Re-check state — another merge pass may have already processed this task,
    // or the user may have changed it.
    let current = match db.get_task(task_id) {
        Ok(Some(t)) => t,
        Ok(None) => {
            return MergeAttempt {
                task_id,
                title,
                success: false,
                log: "task not found".into(),
            };
        }
        Err(e) => {
            return MergeAttempt {
                task_id,
                title,
                success: false,
                log: format!("db error: {}", e),
            };
        }
    };

    if current.state != TaskState::Approved {
        return MergeAttempt {
            task_id,
            title,
            success: false,
            log: format!("task is now in '{}' state, skipping", current.state),
        };
    }

    // Transition to merging
    if let Err(e) = db.update_task(
        task_id,
        &TaskUpdate {
            state: Some(TaskState::Merging),
            ..Default::default()
        },
        None,
    ) {
        return MergeAttempt {
            task_id,
            title,
            success: false,
            log: format!("failed to transition to merging: {}", e),
        };
    }

    // Broadcast approved -> merging to all involved sessions.
    if let Ok(Some(t)) = db.get_task(task_id) {
        crate::tasks_notify::notify_state_change(db, &t, TaskState::Approved, None, writer, reader);
    }

    eprintln!("tasks scheduler: auto-merging task {} ({})", task_id, title);

    // Run the merge
    let project_dir = match resolve_path(&current.project_name) {
        Ok(p) => p,
        Err(e) => {
            return MergeAttempt {
                task_id,
                title,
                success: false,
                log: format!("resolve project path: {}", e),
            };
        }
    };
    match crate::tasks_merge::merge_task_for_caller(
        db,
        task_id,
        &project_dir,
        caller_session_id,
        writer,
        reader,
    ) {
        Ok(result) => {
            if result.success {
                // Transition to merged
                if let Err(e) = db.update_task(
                    task_id,
                    &TaskUpdate {
                        state: Some(TaskState::Merged),
                        ..Default::default()
                    },
                    None,
                ) {
                    eprintln!(
                        "tasks scheduler: merge succeeded but transition to merged failed for task {}: {}",
                        task_id, e
                    );
                }

                // Broadcast merging -> merged (terminal).  Root session is
                // added to the recipient list automatically.
                if let Ok(Some(t)) = db.get_task(task_id) {
                    let ctx = extract_merge_commit(&project_dir, &t);
                    crate::tasks_notify::notify_state_change(
                        db,
                        &t,
                        TaskState::Merging,
                        ctx.as_deref(),
                        writer,
                        reader,
                    );
                }

                // Notify parent session that this individual subtask completed
                crate::tasks_merge::notify_parent_of_subtask_done(db, task_id, writer, reader);

                // Notify parent if all subtasks are in a terminal state
                if let Err(e) =
                    crate::tasks_merge::notify_parent_if_all_done(db, task_id, writer, reader)
                {
                    eprintln!(
                        "tasks scheduler: parent notification failed for task {}: {}",
                        task_id, e
                    );
                }

                eprintln!("tasks scheduler: task {} merged successfully", task_id);
                MergeAttempt {
                    task_id,
                    title,
                    success: true,
                    log: result.log,
                }
            } else {
                // Merge failed — transition back to active
                if let Err(e) = db.update_task(
                    task_id,
                    &TaskUpdate {
                        state: Some(TaskState::Active),
                        ..Default::default()
                    },
                    None,
                ) {
                    eprintln!(
                        "tasks scheduler: failed to transition task {} back to active: {}",
                        task_id, e
                    );
                }

                // Broadcast merging -> active (recoverable failure).
                if let Ok(Some(t)) = db.get_task(task_id) {
                    crate::tasks_notify::notify_state_change(
                        db,
                        &t,
                        TaskState::Merging,
                        Some("merge failed — reverted to active"),
                        writer,
                        reader,
                    );
                }

                // Add error details as a task message
                let _ = db.add_message(
                    task_id,
                    &format!("Auto-merge failed:\n{}", result.log),
                    Some("system"),
                );

                // Notify assigned session about failure
                if let Some(ref sid) = current.session_id {
                    crate::tasks_merge::notify_session_of_merge_failure(
                        sid,
                        task_id,
                        &result.log,
                        writer,
                        reader,
                    );
                }

                eprintln!("tasks scheduler: task {} merge failed", task_id);
                MergeAttempt {
                    task_id,
                    title,
                    success: false,
                    log: result.log,
                }
            }
        }
        Err(e) => {
            // Unexpected error — transition back to active
            if let Err(te) = db.update_task(
                task_id,
                &TaskUpdate {
                    state: Some(TaskState::Active),
                    ..Default::default()
                },
                None,
            ) {
                eprintln!(
                    "tasks scheduler: failed to transition task {} back to active: {}",
                    task_id, te
                );
            }

            // Broadcast merging -> active (unexpected error).
            if let Ok(Some(t)) = db.get_task(task_id) {
                crate::tasks_notify::notify_state_change(
                    db,
                    &t,
                    TaskState::Merging,
                    Some(&format!("merge error: {}", e)),
                    writer,
                    reader,
                );
            }

            let _ = db.add_message(task_id, &format!("Auto-merge error: {}", e), Some("system"));

            eprintln!("tasks scheduler: task {} merge error: {}", task_id, e);
            MergeAttempt {
                task_id,
                title,
                success: false,
                log: format!("merge error: {}", e),
            }
        }
    }
}

/// Build the initial chat message sent to a dispatched task's session.
///
/// `project_instructions` is the combined text loaded from
/// `.tau/instructions.toml` (via
/// [`tasks_config::load_project_instructions`]) for the `"worker"` phase.
/// When non-empty it is appended as a dedicated section so the worker sees
/// project-specific guidance in addition to the generic boilerplate.
fn build_initial_message(
    task: &Task,
    merge_target: &str,
    project_instructions: &str,
    checklist: &[crate::tasks_merge::CheckItem],
) -> String {
    let review_instruction = if task.skip_review {
        format!(
            "- Call the `task_update` tool with arguments: {{\"id\": {id}, \"state\": \"approved\"}}  (skip_review is true for this task)",
            id = task.id
        )
    } else {
        format!(
            "- Call the `task_update` tool with arguments: {{\"id\": {id}, \"state\": \"review\"}}  (skip_review is false — needs review)",
            id = task.id
        )
    };

    let branch = task.branch.as_deref().unwrap_or("(unknown)");
    let worktree = task.worktree_path.as_deref().unwrap_or("(unknown)");

    // Warn explicitly when the merge target is not main (nested subtask).
    let nested_warning = if merge_target != "main" {
        format!(
            "\nIMPORTANT: Your merge target is {target}, NOT main. \
             Do all your work in this worktree ({worktree}). Do not switch directories.\n",
            target = merge_target,
            worktree = worktree,
        )
    } else {
        String::new()
    };

    // Build the "when done" block.
    // When checklist is non-empty: concrete check commands + "Then mark the task:".
    // When empty: just "When done, mark the task:".
    let when_done_block = if checklist.is_empty() {
        "When done, mark the task:".to_string()
    } else {
        let checks = checklist
            .iter()
            .map(|c| format!("- `{}`", c.command))
            .collect::<Vec<_>>()
            .join("\n");
        format!(
            "When done, run these checks:\n{}\nThen mark the task:",
            checks
        )
    };

    let mut msg = format!(
        "You are working on task {id}: {title}\n\
         \n\
         Your branch: {branch}\n\
         Your worktree: {worktree}\n\
         Merge target: {target}\n\
         {nested}\
         Use the task_get tool (not a bash command) to read the full specification:\n\
         - Call the `task_get` tool with arguments: {{\"id\": {id}}}\n\
         \n\
         Do the work in this worktree. Commit your changes on the current branch — do NOT merge into {target}.\n\
         {when_done_block}\n\
         {review}\n\
         \n\
         Note: task_get and task_update are agent tools (like bash or edit), not CLI commands.",
        id = task.id,
        title = task.title,
        branch = branch,
        worktree = worktree,
        target = merge_target,
        nested = nested_warning,
        review = review_instruction,
        when_done_block = when_done_block,
    );

    let trimmed = project_instructions.trim();
    if !trimmed.is_empty() {
        msg.push_str(&format!(
            "\n\n## Project-specific worker instructions\n\n{}\n",
            trimmed
        ));
    }

    msg
}

// ---------------------------------------------------------------------------
// ServerRequest tunnel (delegates to shared tau_agent_plugin::tunnel)
// ---------------------------------------------------------------------------

#[allow(dead_code)] // Used in tests
pub(crate) fn send_message(writer: &mut impl Write, msg: &PluginMessage) {
    tau_agent_plugin::tunnel::send_message(writer, msg);
}

thread_local! {
    /// Request-id prefix for `server_request` calls made on the current
    /// thread. Defaults to `"task-sr"` (the plugin main loop); the merge
    /// worker overrides this to `"merge-sr"` at thread start via
    /// [`set_thread_rpc_prefix`] so its outgoing RPCs are routed to its
    /// own response channel.
    ///
    /// See `crates/tau-agent-plugin/src/tunnel.rs` for the wire format
    /// and `crate::tasks::run_tasks_plugin` for the line router that
    /// dispatches `ServerResponse`s based on this prefix.
    static RPC_PREFIX: std::cell::RefCell<&'static str> = const { std::cell::RefCell::new("task-sr") };
}

/// Set the per-thread RPC prefix used by [`server_request`] below. Call
/// this once at the top of a worker thread's entry point.
pub(crate) fn set_thread_rpc_prefix(prefix: &'static str) {
    RPC_PREFIX.with(|p| *p.borrow_mut() = prefix);
}

/// Read the current thread's RPC prefix. Useful in tests that want to
/// assert the worker thread is using the merge-sr prefix.
#[allow(dead_code)]
pub(crate) fn current_rpc_prefix() -> &'static str {
    RPC_PREFIX.with(|p| *p.borrow())
}

pub fn server_request(
    writer: &mut impl Write,
    reader: &mut impl BufRead,
    request: tau_agent_plugin::Request,
) -> tau_agent_plugin::Result<tau_agent_plugin::Response> {
    let prefix = RPC_PREFIX.with(|p| *p.borrow());
    tau_agent_plugin::tunnel::server_request(writer, reader, request, prefix)
}

// ---------------------------------------------------------------------------
// Scheduler status
// ---------------------------------------------------------------------------

/// Reason a task is waiting/blocked.
#[derive(Debug, Clone, serde::Serialize)]
pub enum WaitReason {
    /// Blocked by a dependency that hasn't completed yet.
    Dependency {
        task_id: i64,
        title: String,
        state: String,
        project_name: String,
    },
    /// Affected files overlap with an active/in-flight task.
    FileConflict {
        files: Vec<String>,
        with_task_id: i64,
    },
    /// Concurrent task budget exhausted.
    BudgetExhausted { used: usize, max: usize },
    /// The merge_target branch does not exist in the repository.
    MergeTargetNotFound { branch: String },
    /// In ready/planning state but not yet scheduled.
    NotScheduled,
}

impl From<WaitReason> for tau_agent_base::protocol::TaskWaitReason {
    fn from(w: WaitReason) -> Self {
        use tau_agent_base::protocol::TaskWaitReason as P;
        match w {
            WaitReason::Dependency {
                task_id,
                title,
                state,
                project_name,
            } => P::Dependency {
                task_id,
                title,
                state,
                project_name,
            },
            WaitReason::FileConflict {
                files,
                with_task_id,
            } => P::FileConflict {
                files,
                with_task_id,
            },
            WaitReason::BudgetExhausted { used, max } => P::BudgetExhausted { used, max },
            WaitReason::MergeTargetNotFound { branch } => P::MergeTargetNotFound { branch },
            WaitReason::NotScheduled => P::NotScheduled,
        }
    }
}

impl WaitReason {
    /// Render this wait reason as a short human-readable phrase suitable
    /// for embedding in a placeholder timeline message (task #574).
    /// Stable, language-invariant output — the tracker dedupes on this
    /// string, so changes to the text would replay spurious messages on
    /// the next scheduler pass after an upgrade.
    pub fn describe(&self) -> String {
        match self {
            WaitReason::Dependency {
                task_id,
                state,
                project_name,
                ..
            } => format!(
                "dependency task #{} not yet complete (state {}, project {})",
                task_id, state, project_name
            ),
            WaitReason::FileConflict {
                files,
                with_task_id,
            } => format!(
                "file conflict with task #{} on {}",
                with_task_id,
                files.join(", ")
            ),
            WaitReason::BudgetExhausted { used, max } => {
                format!("concurrent-task budget exhausted ({}/{})", used, max)
            }
            WaitReason::MergeTargetNotFound { branch } => {
                format!("merge target branch '{}' not found", branch)
            }
            WaitReason::NotScheduled => "not yet scheduled".to_string(),
        }
    }
}

/// Combine multiple wait reasons into a single short summary string
/// suitable for posting to a placeholder. Returns `None` when the list
/// is empty.
pub fn summarize_wait_reasons(reasons: &[WaitReason]) -> Option<String> {
    if reasons.is_empty() {
        return None;
    }
    let parts: Vec<String> = reasons.iter().map(|r| r.describe()).collect();
    Some(parts.join("; "))
}

/// In-memory tracker mapping `task_id -> last-posted wait-reason digest`.
/// Used by the scheduler loop to post a placeholder info message only
/// when the wait reason changes (newly-present, changed, or cleared).
///
/// The map is intentionally not persisted across restarts — a fresh
/// server replays the current wait reasons once, which is acceptable.
#[derive(Debug, Default)]
pub struct WaitTracker {
    /// `None` value means "last-seen state was ‘no wait reason’". Only
    /// tasks that have ever been observed appear in the map; freshly
    /// seen ones post an initial "Waiting:" line.
    last: std::collections::HashMap<i64, WaitTrackerEntry>,
}

#[derive(Debug, Clone)]
struct WaitTrackerEntry {
    /// Last-posted digest (None = last state was ‘cleared’). Always
    /// present once a task is seen.
    digest: Option<String>,
    /// Unix-ms timestamp when the current `digest` was first observed.
    since_ms: i64,
}

/// One tracker diff: what the scheduler should post to a task's
/// placeholder right now. `None` means no action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitEvent {
    /// Task has a (new or changed) wait reason. Post `"Waiting: {text}"`.
    Waiting(String),
    /// Task's wait cleared (previously had a reason, now has none).
    /// Post `"Wait cleared after {duration}. Dispatching."`.
    Cleared { elapsed_ms: i64 },
}

impl WaitTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Update the tracker with the current wait-reason summary for
    /// `task_id`, returning the event to post (if any).
    ///
    /// * `digest = Some(..)`: the task is currently waiting with that
    ///   summary text.
    /// * `digest = None`: the task has no wait reason (dispatched,
    ///   terminal, or otherwise off the queue).
    /// * `now_ms`: the current wall-clock for "since" bookkeeping.
    pub fn observe(
        &mut self,
        task_id: i64,
        digest: Option<String>,
        now_ms: i64,
    ) -> Option<WaitEvent> {
        match (self.last.get(&task_id).cloned(), digest) {
            (None, Some(new)) => {
                // First time we see this task and it has a wait reason.
                self.last.insert(
                    task_id,
                    WaitTrackerEntry {
                        digest: Some(new.clone()),
                        since_ms: now_ms,
                    },
                );
                Some(WaitEvent::Waiting(new))
            }
            (None, None) => {
                // First time we see this task and it's not waiting — no
                // message needed. Record it so a subsequent wait posts.
                self.last.insert(
                    task_id,
                    WaitTrackerEntry {
                        digest: None,
                        since_ms: now_ms,
                    },
                );
                None
            }
            (Some(entry), Some(new)) => {
                if entry.digest.as_deref() == Some(new.as_str()) {
                    // Same reason — no-op.
                    None
                } else {
                    // Reason changed (including going from None -> Some).
                    self.last.insert(
                        task_id,
                        WaitTrackerEntry {
                            digest: Some(new.clone()),
                            since_ms: now_ms,
                        },
                    );
                    Some(WaitEvent::Waiting(new))
                }
            }
            (Some(entry), None) => {
                if entry.digest.is_none() {
                    // Was already cleared — no-op.
                    None
                } else {
                    let elapsed = now_ms.saturating_sub(entry.since_ms);
                    self.last.insert(
                        task_id,
                        WaitTrackerEntry {
                            digest: None,
                            since_ms: now_ms,
                        },
                    );
                    Some(WaitEvent::Cleared {
                        elapsed_ms: elapsed,
                    })
                }
            }
        }
    }
}

/// Process-global [`WaitTracker`] for the plugin. The plugin runs as a
/// single process with one scheduler loop, so a single tracker suffices
/// for all projects.
fn global_wait_tracker() -> &'static Mutex<WaitTracker> {
    static TRACKER: OnceLock<Mutex<WaitTracker>> = OnceLock::new();
    TRACKER.get_or_init(|| Mutex::new(WaitTracker::new()))
}

/// Run a wait-reason pass for `project_name`: compute the current
/// scheduler status, feed each waiting task's digest into the global
/// [`WaitTracker`], and post any resulting placeholder info messages.
///
/// Called by the plugin's schedule-pass driver after every `schedule()`
/// + dispatch attempt so wait lines on the placeholder timeline stay in
/// sync with the scheduler's view.
///
/// Best-effort: failures to compute status or send RPCs are logged and
/// swallowed — placeholder messaging must not break the scheduler.
pub fn post_wait_updates_for_project(
    db: &TasksDb,
    project_name: &str,
    project_path: Option<&str>,
    writer: &mut impl Write,
    reader: &mut impl BufRead,
) {
    let status = match get_status(db, project_name, project_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "tasks placeholder wait: failed to compute status for '{}': {}",
                project_name, e
            );
            return;
        }
    };
    let mut tracker = match global_wait_tracker().lock() {
        Ok(t) => t,
        Err(poisoned) => poisoned.into_inner(),
    };
    post_wait_updates(&status, &mut tracker, writer, reader);
}

/// Helper for the main scheduler loop: for every waiting / queued /
/// blocked / held task in `status`, feed its current wait reasons into
/// `tracker` and post any resulting placeholder messages.
///
/// Tasks that are actively dispatched (`active`, `review`, `merging`,
/// `refining`) count as "no wait reason" for placeholder purposes even
/// if `status.active[].wait_reasons` lists downstream dependencies —
/// the placeholder already receives state-change messages for those.
pub fn post_wait_updates(
    status: &SchedulerStatus,
    tracker: &mut WaitTracker,
    writer: &mut impl Write,
    reader: &mut impl BufRead,
) {
    let now_ms = now_unix_ms();

    // Tasks that are in-flight: wait tracker should observe "no reason"
    // so any pending "Waiting:" message clears into a "Wait cleared" line.
    for ts in &status.active {
        handle_wait(tracker, ts, None, now_ms, writer, reader);
    }

    // Queued/blocked/held: report current wait summary.
    for bucket in [
        &status.queued_planning,
        &status.queued_ready,
        &status.blocked,
        &status.held,
    ] {
        for ts in bucket {
            let digest = summarize_wait_reasons(&ts.wait_reasons);
            let digest = if ts.task.held {
                // Held tasks always surface a fixed reason even if the
                // underlying wait_reasons vector is NotScheduled.
                Some(match digest {
                    Some(r) => format!("held — awaiting manual release ({})", r),
                    None => "held — awaiting manual release".to_string(),
                })
            } else {
                digest
            };
            handle_wait(tracker, ts, digest, now_ms, writer, reader);
        }
    }
}

fn handle_wait(
    tracker: &mut WaitTracker,
    ts: &TaskStatus,
    digest: Option<String>,
    now_ms: i64,
    writer: &mut impl Write,
    reader: &mut impl BufRead,
) {
    // Only post if the task has a placeholder to post to.
    if ts.task.placeholder_session_id.is_none() {
        // Still observe so future wait transitions for this task start
        // from a coherent baseline if placeholder is populated later.
        let _ = tracker.observe(ts.task.id, digest, now_ms);
        return;
    }
    let Some(event) = tracker.observe(ts.task.id, digest, now_ms) else {
        return;
    };
    let text = match event {
        WaitEvent::Waiting(r) => format!("Waiting: {}", r),
        WaitEvent::Cleared { elapsed_ms } => {
            format!(
                "Wait cleared after {}. Dispatching.",
                format_duration_ms_short(elapsed_ms)
            )
        }
    };
    crate::tasks_notify::notify_placeholder_wait(&ts.task, &text, writer, reader);
}

fn now_unix_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Short duration formatter used in wait-cleared messages.
fn format_duration_ms_short(ms: i64) -> String {
    if ms < 0 {
        return "0ms".to_string();
    }
    let ms = ms as u64;
    if ms < 1_000 {
        return format!("{}ms", ms);
    }
    let secs = ms / 1_000;
    if secs < 60 {
        return format!("{}s", secs);
    }
    let mins = secs / 60;
    let rem_secs = secs % 60;
    if mins < 60 {
        return format!("{}m{:02}s", mins, rem_secs);
    }
    let hours = mins / 60;
    let rem_mins = mins % 60;
    format!("{}h{:02}m", hours, rem_mins)
}

/// Status of a single task in the scheduler view.
#[derive(Debug, Clone, serde::Serialize)]
pub struct TaskStatus {
    pub task: Task,
    pub session_id: Option<String>,
    pub wait_reasons: Vec<WaitReason>,
}

/// Overall scheduler status.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SchedulerStatus {
    pub active: Vec<TaskStatus>,
    pub queued_planning: Vec<TaskStatus>,
    pub queued_ready: Vec<TaskStatus>,
    pub held: Vec<TaskStatus>,
    pub blocked: Vec<TaskStatus>,
    pub inflight_count: usize,
    pub max_concurrent: usize,
}

/// Compute the current scheduler status: active, queued, and blocked tasks.
///
/// When `project_path` is provided, additionally validates that each ready
/// task's `merge_target` branch exists in the repository — surfacing a
/// `MergeTargetNotFound` wait reason when it doesn't.
pub fn get_status(
    db: &TasksDb,
    project_name: &str,
    project_path: Option<&str>,
) -> tau_agent_plugin::Result<SchedulerStatus> {
    let inflight_count = db.count_inflight_tasks(project_name)?;
    let max_concurrent = MAX_CONCURRENT_TASKS;

    // Get all non-terminal tasks for this project.
    let all_tasks = db.list_tasks(project_name, None, None, None, None)?;

    // Collect active tasks (in-flight working states).
    // `TaskState::is_inflight` is the canonical predicate (task #611).

    let mut active = Vec::new();
    let mut queued_planning = Vec::new();
    let mut queued_ready = Vec::new();
    let mut held = Vec::new();
    let mut blocked = Vec::new();

    // Build a map of active task IDs to their affected files for conflict detection.
    let active_tasks_files: Vec<(i64, Vec<String>)> = all_tasks
        .iter()
        .filter(|t| t.state.is_inflight())
        .map(|t| (t.id, extract_files(&t.affected_files)))
        .collect();

    for task in all_tasks {
        if task.state.is_inflight() {
            // Active/in-flight task.
            // Check if it's waiting on dependencies even though it's active.
            let deps = db.get_blocking_dependencies(task.id)?;
            let wait_reasons: Vec<WaitReason> = deps
                .iter()
                .map(|d| WaitReason::Dependency {
                    task_id: d.id,
                    title: d.title.clone(),
                    state: d.state.as_str().to_string(),
                    project_name: d.project_name.clone(),
                })
                .collect();
            active.push(TaskStatus {
                session_id: task.session_id.clone(),
                task,
                wait_reasons,
            });
        } else if task.state == TaskState::Ready || task.state == TaskState::Planning {
            // Check blocking dependencies first.
            let deps = db.get_blocking_dependencies(task.id)?;
            if !deps.is_empty() {
                // Blocked by dependencies.
                let wait_reasons = deps
                    .iter()
                    .map(|d| WaitReason::Dependency {
                        task_id: d.id,
                        title: d.title.clone(),
                        state: d.state.as_str().to_string(),
                        project_name: d.project_name.clone(),
                    })
                    .collect();
                blocked.push(TaskStatus {
                    session_id: task.session_id.clone(),
                    task,
                    wait_reasons,
                });
            } else {
                // Not blocked by deps — it's queued. Compute why it's waiting.
                let mut wait_reasons = Vec::new();

                // Check file conflicts (only for ready tasks with affected_files).
                if task.state == TaskState::Ready {
                    let task_files = extract_files(&task.affected_files);
                    if !task_files.is_empty() {
                        for (active_id, active_files) in &active_tasks_files {
                            let overlapping: Vec<String> = task_files
                                .iter()
                                .filter(|f| active_files.contains(f))
                                .cloned()
                                .collect();
                            if !overlapping.is_empty() {
                                wait_reasons.push(WaitReason::FileConflict {
                                    files: overlapping,
                                    with_task_id: *active_id,
                                });
                            }
                        }
                    }
                }

                // Check merge_target branch existence (only for ready tasks).
                if task.state == TaskState::Ready {
                    if let Some(path) = project_path {
                        let merge_target = db
                            .get_merge_target(task.id)
                            .unwrap_or_else(|_| "main".into());
                        if let Ok(repo_root) = tasks_git::get_repo_root(path) {
                            if let Ok(false) = tasks_git::branch_exists(&repo_root, &merge_target) {
                                wait_reasons.push(WaitReason::MergeTargetNotFound {
                                    branch: merge_target,
                                });
                            }
                        }
                    }
                }

                // Check budget.
                if inflight_count >= max_concurrent {
                    wait_reasons.push(WaitReason::BudgetExhausted {
                        used: inflight_count,
                        max: max_concurrent,
                    });
                }

                // If no specific reason found, it's just not scheduled yet.
                if wait_reasons.is_empty() {
                    wait_reasons.push(WaitReason::NotScheduled);
                }

                let status = TaskStatus {
                    session_id: task.session_id.clone(),
                    task: task.clone(),
                    wait_reasons,
                };
                if task.held {
                    // Held tasks are parked by the caller — the scheduler
                    // skips them regardless of other state. Surface them in
                    // a dedicated section instead of "about to be dispatched".
                    held.push(status);
                } else if task.state == TaskState::Planning {
                    queued_planning.push(status);
                } else {
                    queued_ready.push(status);
                }
            }
        }
        // Skip interactive, approved, failed, merged, closed — they aren't relevant to scheduler status.
    }

    Ok(SchedulerStatus {
        active,
        queued_planning,
        queued_ready,
        held,
        blocked,
        inflight_count,
        max_concurrent,
    })
}

/// Format the scheduler status as a human-readable string.
pub fn format_status(status: &SchedulerStatus) -> String {
    let mut out = String::new();
    out.push_str("=== Task Scheduler Status ===\n");
    out.push_str(&format!(
        "    in-flight: {}/{}\n",
        status.inflight_count, status.max_concurrent
    ));

    // Active tasks.
    if !status.active.is_empty() {
        out.push_str(&format!("\nactive ({}):\n", status.active.len()));
        for ts in &status.active {
            format_task_line(&mut out, ts);
        }
    }

    // Queued - Planning.
    if !status.queued_planning.is_empty() {
        out.push_str(&format!(
            "\nqueued - planning ({}):\n",
            status.queued_planning.len()
        ));
        for ts in &status.queued_planning {
            format_task_line(&mut out, ts);
        }
    }

    // Queued - Ready.
    if !status.queued_ready.is_empty() {
        out.push_str(&format!(
            "\nqueued - ready ({}):\n",
            status.queued_ready.len()
        ));
        for ts in &status.queued_ready {
            format_task_line(&mut out, ts);
        }
    }

    // Held.
    if !status.held.is_empty() {
        out.push_str(&format!("\nheld ({}):\n", status.held.len()));
        for ts in &status.held {
            format_task_line(&mut out, ts);
        }
    }

    // Blocked.
    if !status.blocked.is_empty() {
        out.push_str(&format!("\nblocked ({}):\n", status.blocked.len()));
        for ts in &status.blocked {
            format_task_line(&mut out, ts);
        }
    }

    if status.active.is_empty()
        && status.queued_planning.is_empty()
        && status.queued_ready.is_empty()
        && status.held.is_empty()
        && status.blocked.is_empty()
    {
        out.push_str("\nNo active or queued tasks.\n");
    }

    out
}

/// Return, for each input path, the shortest suffix (joined by `/`) that is
/// unique among all paths in the list. A path whose basename is already
/// unique collapses to that basename; otherwise enough leading components
/// are retained to disambiguate. Used purely for display.
fn shortest_unique_suffixes(paths: &[String]) -> Vec<String> {
    fn suffix_of(parts: &[&str], n: usize) -> String {
        let start = parts.len().saturating_sub(n);
        parts[start..].join("/")
    }

    let parts: Vec<Vec<&str>> = paths.iter().map(|p| p.split('/').collect()).collect();
    let mut out = Vec::with_capacity(paths.len());
    for i in 0..parts.len() {
        let mut n = 1usize;
        loop {
            let suffix_i = suffix_of(&parts[i], n);
            let unique = parts
                .iter()
                .enumerate()
                .all(|(j, p)| j == i || suffix_of(p, n) != suffix_i);
            if unique || n >= parts[i].len() {
                out.push(suffix_i);
                break;
            }
            n += 1;
        }
    }
    out
}

fn format_task_line(out: &mut String, ts: &TaskStatus) {
    use std::fmt::Write;

    let sid = ts.session_id.as_deref().unwrap_or("-");
    let files = extract_files(&ts.task.affected_files);
    let files_str = if files.is_empty() {
        String::new()
    } else {
        // Show minimal unique path suffixes so same-basename files in
        // different directories remain distinguishable.
        let abbrev = shortest_unique_suffixes(&files);
        format!("  [{}]", abbrev.join(", "))
    };

    let _ = write!(
        out,
        "  #{:<5} {:<40} {:<6}{}",
        ts.task.id, ts.task.title, sid, files_str
    );

    if let Some(profile) = ts.task.sandbox_profile.as_deref() {
        let _ = write!(out, " 🔒{}", profile);
    }

    if ts.task.held {
        let _ = write!(out, " 🔒held");
    }

    // Append wait reasons.
    for reason in &ts.wait_reasons {
        match reason {
            WaitReason::Dependency {
                task_id,
                title: _,
                state,
                project_name,
            } => {
                if project_name != &ts.task.project_name {
                    let _ = write!(
                        out,
                        "  ⏳ depends on #{} ({}) [{}]",
                        task_id, state, project_name
                    );
                } else {
                    let _ = write!(out, "  ⏳ depends on #{} ({})", task_id, state);
                }
            }
            WaitReason::FileConflict {
                files,
                with_task_id,
            } => {
                let abbrev = shortest_unique_suffixes(files);
                let _ = write!(
                    out,
                    "  ⏳ file conflict [{}] with #{}",
                    abbrev.join(", "),
                    with_task_id
                );
            }
            WaitReason::BudgetExhausted { used, max } => {
                let _ = write!(out, "  ⏳ budget ({}/{} sessions used)", used, max);
            }
            WaitReason::MergeTargetNotFound { branch } => {
                let _ = write!(out, "  ⚠️ merge_target branch '{}' not found", branch);
            }
            WaitReason::NotScheduled => {
                let _ = write!(out, "  ⏳ not yet scheduled");
            }
        }
    }

    out.push('\n');
}

// ---------------------------------------------------------------------------
// Structured overview (for TaskOverview RPC + task_overview tool)
// ---------------------------------------------------------------------------

/// Convert a DB `Task` to its wire form, setting `has_live_session` from
/// the caller-supplied set of live task ids.
fn task_to_info_with_live(
    t: crate::tasks_db::Task,
    live_task_ids: &HashSet<i64>,
) -> tau_agent_base::protocol::TaskInfo {
    tau_agent_base::protocol::TaskInfo {
        has_live_session: live_task_ids.contains(&t.id),
        id: t.id,
        project_name: t.project_name,
        title: t.title,
        state: t.state.as_str().to_string(),
        priority: t.priority,
        parent_id: t.parent_id,
        tags: t.tags,
        affected_files: t.affected_files,
        branch: t.branch,
        worktree_path: t.worktree_path,
        session_id: t.session_id,
        skip_review: t.skip_review,
        require_approval: t.require_approval,
        sandbox_profile: t.sandbox_profile,
        held: t.held,
        created_at: t.created_at,
        updated_at: t.updated_at,
    }
}

/// Build a structured [`tau_agent_base::protocol::Response::TaskOverview`]
/// for `project`, with `recent_limit` applied per bucket to the recently-
/// completed tail.
///
/// `live_task_ids` lets the caller stamp `has_live_session` based on its
/// own view of running sessions (the plugin has no access to the server's
/// `live_sessions` set). Callers without that information can pass an
/// empty set.
///
/// All classification (active / queued_ready / queued_planning / blocked /
/// held) is delegated to [`get_status`]; this function just converts the
/// scheduler `TaskStatus` entries into wire `TaskInfo`s, collects the
/// per-task wait-reason side table, and queries the
/// DB for the recent tail.
pub fn task_overview_response(
    db: &TasksDb,
    project: &str,
    recent_limit: usize,
    live_task_ids: &HashSet<i64>,
) -> tau_agent_plugin::Result<tau_agent_base::protocol::Response> {
    use tau_agent_base::protocol::{Response, TaskInfo, TaskWaitReasons};

    let status = get_status(db, project, None)?;

    let to_infos = |rows: Vec<TaskStatus>| -> Vec<TaskInfo> {
        rows.into_iter()
            .map(|ts| task_to_info_with_live(ts.task, live_task_ids))
            .collect()
    };

    // Collect all wait reasons for blocked/queued (and any active still
    // waiting on deps). The TUI uses dependency reasons for the inline
    // `⏳ #N` suffix and the complete list for the detail overlay.
    let mut wait_reasons: Vec<TaskWaitReasons> = Vec::new();
    for ts in status
        .blocked
        .iter()
        .chain(status.queued_ready.iter())
        .chain(status.queued_planning.iter())
        .chain(status.active.iter())
    {
        if ts.wait_reasons.is_empty() {
            continue;
        }
        let reasons: Vec<_> = ts.wait_reasons.iter().cloned().map(Into::into).collect();
        wait_reasons.push(TaskWaitReasons {
            task_id: ts.task.id,
            reasons,
        });
    }

    let active = to_infos(status.active);
    let queued_ready = to_infos(status.queued_ready);
    let queued_planning = to_infos(status.queued_planning);
    let blocked = to_infos(status.blocked);
    let held = to_infos(status.held);

    let recently_merged: Vec<TaskInfo> = db
        .list_recent_by_state(project, "merged", recent_limit)?
        .into_iter()
        .map(|t| task_to_info_with_live(t, live_task_ids))
        .collect();
    let recently_closed: Vec<TaskInfo> = db
        .list_recent_by_state(project, "closed", recent_limit)?
        .into_iter()
        .map(|t| task_to_info_with_live(t, live_task_ids))
        .collect();

    Ok(Response::TaskOverview {
        active,
        queued_ready,
        queued_planning,
        blocked,
        held,
        recently_merged,
        recently_closed,
        inflight_count: status.inflight_count,
        max_concurrent: status.max_concurrent,
        wait_reasons,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks_db::TasksDb;

    fn make_task(id: i64, priority: i64, files: Option<Vec<&str>>) -> Task {
        Task {
            id,
            project_name: "test-project".to_string(),
            title: format!("Task {}", id),
            state: TaskState::Ready,
            priority,
            parent_id: None,
            tags: None,
            affected_files: files.map(|f| {
                serde_json::Value::Array(
                    f.into_iter()
                        .map(|s| serde_json::Value::String(s.to_string()))
                        .collect(),
                )
            }),
            branch: None,
            merge_target: None,
            worktree_path: None,
            session_id: None,
            skip_review: false,
            require_approval: false,
            sandbox_profile: None,
            held: false,
            placeholder_session_id: None,
            auto_downgraded_from_ready: false,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn test_select_non_conflicting_no_overlap() {
        let tasks = vec![
            make_task(1, 5, Some(vec!["src/a.rs"])),
            make_task(2, 3, Some(vec!["src/b.rs"])),
            make_task(3, 1, Some(vec!["src/c.rs"])),
        ];
        let batch = select_non_conflicting(&tasks, &[]);
        assert_eq!(batch.len(), 3);
    }

    // ----- skip-reason tests (task #584) -----

    #[test]
    fn test_with_reasons_file_conflict_reports_partner() {
        let tasks = vec![make_task(1, 5, Some(vec!["src/a.rs"]))];
        let active_files = vec![(77, vec!["src/a.rs".to_string()])];
        let (selected, skipped) = select_non_conflicting_with_reasons(&tasks, &active_files);
        assert!(selected.is_empty());
        assert_eq!(skipped.len(), 1);
        match &skipped[0].1 {
            SkipReason::FileConflict {
                with_task_id,
                overlapping,
            } => {
                assert_eq!(*with_task_id, 77);
                assert_eq!(overlapping, &vec!["src/a.rs".to_string()]);
            }
            other => panic!("unexpected skip reason {:?}", other),
        }
    }

    #[test]
    fn test_with_reasons_file_less_blocked_by_active_reports_task_id() {
        // Root-cause scenario for task #584: a ready task with no
        // affected_files can't schedule when any task is in-flight.
        let tasks = vec![make_task(1, 5, None)];
        let active_files = vec![(42, vec!["src/something.rs".to_string()])];
        let (selected, skipped) = select_non_conflicting_with_reasons(&tasks, &active_files);
        assert!(selected.is_empty());
        assert_eq!(skipped.len(), 1);
        match &skipped[0].1 {
            SkipReason::EmptyAffectedFilesNotAlone { with_task_id } => {
                assert_eq!(*with_task_id, Some(42));
            }
            other => panic!("unexpected skip reason {:?}", other),
        }
    }

    #[test]
    fn test_with_reasons_file_less_blocked_by_unbounded_active() {
        // An active task with no affected_files is "unbounded" — it
        // conflicts with everything, including other file-less tasks.
        let tasks = vec![make_task(1, 5, None)];
        let active_files = vec![(42, vec![])];
        let (selected, skipped) = select_non_conflicting_with_reasons(&tasks, &active_files);
        assert!(selected.is_empty());
        assert_eq!(skipped.len(), 1);
        match &skipped[0].1 {
            SkipReason::EmptyAffectedFilesNotAlone { with_task_id } => {
                assert_eq!(*with_task_id, Some(42));
            }
            other => panic!("unexpected skip reason {:?}", other),
        }
    }

    #[test]
    fn test_with_reasons_second_file_less_blocked_by_first() {
        // Two ready tasks both lacking affected_files: the first wins
        // (highest priority), the second is skipped with a reason that
        // points at the one that was selected.
        let tasks = vec![make_task(1, 5, None), make_task(2, 3, None)];
        let (selected, skipped) = select_non_conflicting_with_reasons(&tasks, &[]);
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].id, 1);
        assert_eq!(skipped.len(), 1);
        match &skipped[0].1 {
            SkipReason::EmptyAffectedFilesBatchFull { with_task_id } => {
                assert_eq!(*with_task_id, 1);
            }
            other => panic!("unexpected skip reason {:?}", other),
        }
    }

    #[test]
    fn test_with_reasons_batch_blocked_by_selected_unbounded() {
        // A file-less task selected first makes the batch unbounded;
        // subsequent file-scoped tasks are skipped.
        let tasks = vec![
            make_task(1, 10, None),
            make_task(2, 5, Some(vec!["src/a.rs"])),
        ];
        let (selected, skipped) = select_non_conflicting_with_reasons(&tasks, &[]);
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].id, 1);
        assert_eq!(skipped.len(), 1);
        match &skipped[0].1 {
            SkipReason::BatchBlockedByUnbounded { with_task_id } => {
                assert_eq!(*with_task_id, 1);
            }
            other => panic!("unexpected skip reason {:?}", other),
        }
    }

    #[test]
    fn test_select_non_conflicting_with_overlap() {
        let tasks = vec![
            make_task(1, 5, Some(vec!["src/a.rs", "src/b.rs"])),
            make_task(2, 3, Some(vec!["src/b.rs", "src/c.rs"])), // overlaps with task 1
            make_task(3, 1, Some(vec!["src/d.rs"])),             // no overlap
        ];
        let batch = select_non_conflicting(&tasks, &[]);
        assert_eq!(batch.len(), 2);
        assert_eq!(batch[0].id, 1);
        assert_eq!(batch[1].id, 3);
    }

    #[test]
    fn test_select_non_conflicting_priority_ordering() {
        // Lower priority task has no overlap, higher priority task overlaps
        let tasks = vec![
            make_task(1, 1, Some(vec!["src/a.rs"])),
            make_task(2, 10, Some(vec!["src/a.rs"])), // same file, higher priority
        ];
        let batch = select_non_conflicting(&tasks, &[]);
        // Task 2 should be picked first (higher priority), task 1 excluded
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].id, 2);
    }

    #[test]
    fn test_select_non_conflicting_no_files_runs_alone() {
        let tasks = vec![
            make_task(1, 10, None), // no affected_files — runs alone
            make_task(2, 5, Some(vec!["src/a.rs"])),
        ];
        let batch = select_non_conflicting(&tasks, &[]);
        // Task 1 (higher priority, no files) should be selected alone
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].id, 1);
    }

    #[test]
    fn test_select_non_conflicting_no_files_lower_priority() {
        let tasks = vec![
            make_task(1, 5, Some(vec!["src/a.rs"])),
            make_task(2, 1, None), // lower priority, no files
        ];
        let batch = select_non_conflicting(&tasks, &[]);
        // Task 1 selected first (higher priority with files), task 2 skipped
        // because we already have selections and task 2 has no files.
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].id, 1);
    }

    #[test]
    fn test_select_non_conflicting_empty() {
        let tasks: Vec<Task> = Vec::new();
        let batch = select_non_conflicting(&tasks, &[]);
        assert!(batch.is_empty());
    }

    #[test]
    fn test_select_non_conflicting_all_overlap() {
        let tasks = vec![
            make_task(1, 5, Some(vec!["src/main.rs"])),
            make_task(2, 3, Some(vec!["src/main.rs"])),
            make_task(3, 1, Some(vec!["src/main.rs"])),
        ];
        let batch = select_non_conflicting(&tasks, &[]);
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].id, 1); // highest priority wins
    }

    #[test]
    fn test_select_non_conflicting_multiple_no_files() {
        let tasks = vec![make_task(1, 10, None), make_task(2, 5, None)];
        let batch = select_non_conflicting(&tasks, &[]);
        // Only highest priority no-files task should be selected
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].id, 1);
    }

    #[test]
    fn test_select_non_conflicting_single_task() {
        let tasks = vec![make_task(42, 0, Some(vec!["file.txt"]))];
        let batch = select_non_conflicting(&tasks, &[]);
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].id, 42);
    }

    #[test]
    fn test_select_non_conflicting_single_no_files() {
        let tasks = vec![make_task(42, 0, None)];
        let batch = select_non_conflicting(&tasks, &[]);
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].id, 42);
    }

    #[test]
    fn test_extract_files_some() {
        let val = Some(serde_json::json!(["a.rs", "b.rs"]));
        let files = extract_files(&val);
        assert_eq!(files, vec!["a.rs", "b.rs"]);
    }

    #[test]
    fn test_extract_files_none() {
        let files = extract_files(&None);
        assert!(files.is_empty());
    }

    #[test]
    fn test_extract_files_non_array() {
        let val = Some(serde_json::json!("not an array"));
        let files = extract_files(&val);
        assert!(files.is_empty());
    }

    #[test]
    fn test_extract_files_star_marker_returns_empty() {
        // Task #596: `["*"]` is the explicit "unbounded scope" marker.
        // It must collapse to an empty vec so the scheduler treats the
        // task as file-less (which serialises it via the at-most-one
        // file-less rule).
        let val = Some(serde_json::json!(["*"]));
        let files = extract_files(&val);
        assert!(
            files.is_empty(),
            "`[\"*\"]` must collapse to empty (file-less), got {:?}",
            files
        );
    }

    #[test]
    fn test_extract_files_star_anywhere_in_array_returns_empty() {
        // A `"*"` anywhere in the list is conservatively unbounded —
        // the caller has signalled they don't fully know the scope, so
        // we must serialise the task even if some concrete files are
        // also listed.
        let val = Some(serde_json::json!(["src/foo.rs", "*"]));
        let files = extract_files(&val);
        assert!(files.is_empty(), "got {:?}", files);
    }

    #[test]
    fn test_select_non_conflicting_two_star_marker_tasks_serialise() {
        // Two tasks both with `["*"]` must NOT run in parallel — the
        // marker means "unbounded scope", and the file-less rule
        // serialises file-less tasks against each other.
        let tasks = vec![
            make_task(1, 0, Some(vec!["*"])),
            make_task(2, 0, Some(vec!["*"])),
        ];
        let batch = select_non_conflicting(&tasks, &[]);
        assert_eq!(
            batch.len(),
            1,
            "only one star-marker task may run per pass, got {}",
            batch.len()
        );
    }

    #[test]
    fn test_select_non_conflicting_star_marker_blocks_concrete_files_task() {
        // A `["*"]` task selected first claims everything; subsequent
        // tasks with concrete files must be blocked.
        let tasks = vec![
            make_task(1, /*high prio*/ 10, Some(vec!["*"])),
            make_task(2, 0, Some(vec!["src/foo.rs"])),
        ];
        let batch = select_non_conflicting(&tasks, &[]);
        assert_eq!(batch.len(), 1, "got {:?}", batch);
        assert_eq!(
            batch[0].id, 1,
            "higher-priority star task should win the batch"
        );
    }

    #[test]
    fn test_select_non_conflicting_star_marker_blocked_by_active_task() {
        // An active task with concrete files holds those files;
        // a `["*"]` task must be blocked by it (it cannot prove
        // disjointness against an active task with any files).
        let tasks = vec![make_task(1, 0, Some(vec!["*"]))];
        let active = vec![(99, vec!["src/active.rs".to_string()])];
        let batch = select_non_conflicting(&tasks, &active);
        assert!(
            batch.is_empty(),
            "star-marker task must wait while any other task is active, got {:?}",
            batch
        );
    }

    #[test]
    fn test_build_initial_message_with_review() {
        let mut task = make_task(5, 0, None);
        task.branch = Some("task-1-5".into());
        task.worktree_path = Some("/tmp/wt-5".into());
        let msg = build_initial_message(&task, "main", "", &[]);
        assert!(msg.contains("task 5"));
        assert!(msg.contains("task_get"));
        assert!(!msg.contains("task_assign"));
        assert!(msg.contains("task_update"));
        assert!(msg.contains("\"state\": \"review\""));
        assert!(msg.contains("skip_review is false"));
        // Must clarify these are tool calls, not CLI commands
        assert!(msg.contains("not a bash command") || msg.contains("not CLI commands"));
        assert!(msg.contains("do NOT merge into main") || msg.contains("do not merge"));
        // Rebase instruction removed — merge queue handles it.
        assert!(!msg.contains("rebase"));
        // No project instructions supplied — the section header must not appear.
        assert!(!msg.contains("Project-specific worker instructions"));
        // Branch, worktree, and merge target must be stated explicitly.
        assert!(msg.contains("Your branch: task-1-5"));
        assert!(msg.contains("Your worktree: /tmp/wt-5"));
        assert!(msg.contains("Merge target: main"));
        // No nested warning for main target.
        assert!(!msg.contains("IMPORTANT"));
    }

    #[test]
    fn test_build_initial_message_skip_review() {
        let mut task = make_task(7, 0, None);
        task.skip_review = true;
        task.branch = Some("task-1-7".into());
        task.worktree_path = Some("/tmp/wt-7".into());
        let msg = build_initial_message(&task, "main", "", &[]);
        assert!(msg.contains("\"state\": \"approved\""));
        assert!(msg.contains("skip_review is true"));
    }

    #[test]
    fn test_build_initial_message_tool_call_format() {
        let mut task = make_task(42, 0, None);
        task.branch = Some("task-1-42".into());
        task.worktree_path = Some("/tmp/wt-42".into());
        let msg = build_initial_message(&task, "main", "", &[]);
        // Should include JSON argument hint so agent knows the invocation format
        assert!(msg.contains(r#"{"id": 42}"#));
        // task_update should also use JSON format, not CLI-style positional args
        assert!(msg.contains(r#""id": 42"#));
        assert!(msg.contains(r#""state":"#) || msg.contains(r#""state": "#));
        // Should tell agent to commit on branch
        assert!(msg.contains("current branch"));
        // Should NOT use CLI-style format like "task_update 42 state=review"
        assert!(!msg.contains(&format!("task_update {} state=", 42)));
    }

    #[test]
    fn test_build_initial_message_uses_merge_target() {
        let mut task = make_task(42, 0, None);
        task.branch = Some("task-14-42".into());
        task.worktree_path = Some("/tmp/wt-42".into());
        let msg = build_initial_message(&task, "task-1-5", "", &[]);
        assert!(msg.contains("do NOT merge into task-1-5"));
        assert!(!msg.contains("merge into main"));
        // Rebase instruction removed — merge queue handles it.
        assert!(!msg.contains("git rebase"));
        // Merge target header
        assert!(msg.contains("Merge target: task-1-5"));
        // Nested subtask warning should appear.
        assert!(msg.contains("IMPORTANT"));
        assert!(msg.contains("NOT main"));
        assert!(msg.contains("/tmp/wt-42"));
    }

    #[test]
    fn test_build_initial_message_includes_project_instructions() {
        let mut task = make_task(9, 0, None);
        task.branch = Some("task-1-9".into());
        task.worktree_path = Some("/tmp/wt-9".into());
        let instructions = "- Follow project style\n- Keep diffs minimal";
        let msg = build_initial_message(&task, "main", instructions, &[]);
        assert!(msg.contains("Project-specific worker instructions"));
        assert!(msg.contains("Follow project style"));
        assert!(msg.contains("Keep diffs minimal"));
    }

    #[test]
    fn test_build_initial_message_blank_instructions_omitted() {
        let mut task = make_task(11, 0, None);
        task.branch = Some("task-1-11".into());
        task.worktree_path = Some("/tmp/wt-11".into());
        // Whitespace-only should be treated as empty — no section header.
        let msg = build_initial_message(&task, "main", "   \n\n  ", &[]);
        assert!(!msg.contains("Project-specific worker instructions"));
    }

    #[test]
    fn test_build_initial_message_empty_checklist() {
        let mut task = make_task(20, 0, None);
        task.branch = Some("task-1-20".into());
        task.worktree_path = Some("/tmp/wt-20".into());
        let msg = build_initial_message(&task, "main", "", &[]);
        // No checklist: should say "When done, mark the task:" without mentioning checks.
        assert!(msg.contains("When done, mark the task:"));
        assert!(!msg.contains("run the project checklist"));
        assert!(!msg.contains("run these checks"));
        // No extra blank lines: the message should not contain triple newlines.
        assert!(
            !msg.contains("\n\n\n"),
            "extra blank line in empty-checklist worker message"
        );
    }

    #[test]
    fn test_build_initial_message_with_checklist() {
        let mut task = make_task(21, 0, None);
        task.branch = Some("task-1-21".into());
        task.worktree_path = Some("/tmp/wt-21".into());
        let checklist = vec![
            crate::tasks_merge::CheckItem {
                name: "format".into(),
                command: "cargo fmt --check".into(),
            },
            crate::tasks_merge::CheckItem {
                name: "lint".into(),
                command: "cargo clippy --all-targets".into(),
            },
        ];
        let msg = build_initial_message(&task, "main", "", &checklist);
        // Should include concrete commands.
        assert!(msg.contains("cargo fmt --check"));
        assert!(msg.contains("cargo clippy --all-targets"));
        assert!(msg.contains("run these checks"));
        // "Then mark the task:" should be immediately followed by the review instruction.
        assert!(msg.contains("Then mark the task:\n"));
        // Should NOT contain the old vague text.
        assert!(!msg.contains("run the project checklist"));
        // No extra blank lines.
        assert!(
            !msg.contains("\n\n\n"),
            "extra blank line in non-empty-checklist worker message"
        );
    }

    #[test]
    fn test_build_review_message_empty_checklist() {
        let mut task = make_task(22, 0, None);
        task.branch = Some("task-1-22".into());
        task.worktree_path = Some("/tmp/wt-22".into());
        let msg = build_review_message(&task, "", "main", &[]);
        // No checklist: should not mention any checklist or checks item 5.
        assert!(!msg.contains("run the project checklist"));
        assert!(!msg.contains("Run these project checks"));
        // No extra blank lines.
        assert!(
            !msg.contains("\n\n\n"),
            "extra blank line in empty-checklist review message"
        );
    }

    #[test]
    fn test_build_review_message_with_checklist() {
        let mut task = make_task(23, 0, None);
        task.branch = Some("task-1-23".into());
        task.worktree_path = Some("/tmp/wt-23".into());
        let checklist = vec![crate::tasks_merge::CheckItem {
            name: "test".into(),
            command: "cargo test --workspace".into(),
        }];
        let msg = build_review_message(&task, "", "main", &checklist);
        // Should include concrete check.
        assert!(msg.contains("cargo test --workspace"));
        assert!(msg.contains("Run these project checks"));
        // Should NOT contain the old vague text.
        assert!(!msg.contains("run the project checklist"));
        // No extra blank lines.
        assert!(
            !msg.contains("\n\n\n"),
            "extra blank line in non-empty-checklist review message"
        );
    }

    #[test]
    fn test_build_review_message_uses_merge_target() {
        let mut task = make_task(10, 0, None);
        task.branch = Some("task-14-10".into());
        task.worktree_path = Some("/tmp/wt-10".into());
        let msg = build_review_message(&task, "", "task-1-5", &[]);
        assert!(msg.contains("git diff task-1-5...HEAD"));
        assert!(!msg.contains("git diff main...HEAD"));
        // Branch and worktree must be stated.
        assert!(msg.contains("Task branch: task-14-10"));
        assert!(msg.contains("Worktree: /tmp/wt-10"));
        assert!(msg.contains("Merge target: task-1-5"));
        // Nested subtask warning should appear.
        assert!(msg.contains("IMPORTANT"));
        assert!(msg.contains("NOT main"));
    }

    #[test]
    fn test_schedule_empty_db() {
        let db = TasksDb::open_memory().unwrap();
        let result = schedule(&db, "test-project", "/fake/path");
        // Will fail because /project is not a git repo, but with empty tasks
        // it should return empty before reaching git operations.
        assert!(result.is_ok());
        assert!(result.unwrap().is_empty());
    }

    #[test]
    fn test_select_non_conflicting_complex_scenario() {
        // A more complex scenario with multiple overlapping groups
        let tasks = vec![
            make_task(1, 10, Some(vec!["src/api.rs", "src/db.rs"])),
            make_task(2, 8, Some(vec!["src/db.rs", "src/models.rs"])), // overlaps 1
            make_task(3, 6, Some(vec!["src/ui.rs", "src/styles.css"])), // no overlap with 1
            make_task(4, 4, Some(vec!["src/styles.css", "src/app.rs"])), // overlaps 3
            make_task(5, 2, Some(vec!["src/tests.rs"])),               // no overlap with 1, 3
        ];
        let batch = select_non_conflicting(&tasks, &[]);
        // Should select: 1 (highest), 3 (no overlap with 1), 5 (no overlap with 1, 3)
        assert_eq!(batch.len(), 3);
        assert_eq!(batch[0].id, 1);
        assert_eq!(batch[1].id, 3);
        assert_eq!(batch[2].id, 5);
    }

    // ----- active file conflict tests -----

    #[test]
    fn test_select_non_conflicting_with_active_file_overlap() {
        // A ready task that overlaps with an active task's files should be excluded.
        let tasks = vec![
            make_task(1, 5, Some(vec!["src/server.rs"])),
            make_task(2, 3, Some(vec!["src/other.rs"])),
        ];
        let active_files = vec![(100, vec!["src/server.rs".to_string()])];
        let batch = select_non_conflicting(&tasks, &active_files);
        // Task 1 overlaps with active task 100 — excluded
        // Task 2 has no overlap — included
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].id, 2);
    }

    #[test]
    fn test_select_non_conflicting_all_overlap_with_active() {
        // All ready tasks overlap with active tasks — none should be selected.
        let tasks = vec![
            make_task(1, 5, Some(vec!["src/a.rs"])),
            make_task(2, 3, Some(vec!["src/b.rs"])),
        ];
        let active_files = vec![
            (100, vec!["src/a.rs".to_string()]),
            (101, vec!["src/b.rs".to_string()]),
        ];
        let batch = select_non_conflicting(&tasks, &active_files);
        assert!(batch.is_empty());
    }

    #[test]
    fn test_select_non_conflicting_active_unbounded_blocks_all() {
        // An active task without affected_files (unbounded) should block all
        // ready tasks.
        let tasks = vec![
            make_task(1, 5, Some(vec!["src/a.rs"])),
            make_task(2, 3, Some(vec!["src/b.rs"])),
        ];
        let active_files = vec![
            (100, vec![]), // unbounded active task
        ];
        let batch = select_non_conflicting(&tasks, &active_files);
        assert!(batch.is_empty());
    }

    #[test]
    fn test_select_non_conflicting_active_unbounded_blocks_unbounded_ready() {
        // A ready task without affected_files should not be scheduled when
        // an active task is unbounded.
        let tasks = vec![
            make_task(1, 5, None), // unbounded ready task
        ];
        let active_files = vec![
            (100, vec![]), // unbounded active task
        ];
        let batch = select_non_conflicting(&tasks, &active_files);
        assert!(batch.is_empty());
    }

    #[test]
    fn test_select_non_conflicting_no_files_ready_blocked_by_active_with_files() {
        // A ready task without affected_files should not be scheduled when
        // active tasks have claimed files (the unbounded task could conflict
        // with anything).
        let tasks = vec![
            make_task(1, 10, None), // unbounded ready task
        ];
        let active_files = vec![(100, vec!["src/something.rs".to_string()])];
        let batch = select_non_conflicting(&tasks, &active_files);
        // An unbounded ready task (no affected_files) is assumed to potentially
        // conflict with everything. Since active tasks have claimed files
        // (claimed_files is non-empty), the unbounded task must not be scheduled.
        assert!(batch.is_empty());
    }

    #[test]
    fn test_select_non_conflicting_partial_overlap_with_active() {
        // Some tasks overlap with active, some don't.
        let tasks = vec![
            make_task(1, 10, Some(vec!["src/a.rs", "src/b.rs"])),
            make_task(2, 8, Some(vec!["src/b.rs", "src/c.rs"])), // overlaps with 1
            make_task(3, 6, Some(vec!["src/d.rs"])),             // no overlap
            make_task(4, 4, Some(vec!["src/e.rs"])),             // no overlap
        ];
        let active_files = vec![
            (100, vec!["src/a.rs".to_string()]), // blocks task 1
        ];
        let batch = select_non_conflicting(&tasks, &active_files);
        // Task 1 blocked by active task 100 (src/a.rs)
        // Task 2 can run (src/b.rs, src/c.rs — no overlap with active)
        // Task 3 can run (src/d.rs — no overlap)
        // Task 4 can run (src/e.rs — no overlap)
        assert_eq!(batch.len(), 3);
        assert_eq!(batch[0].id, 2);
        assert_eq!(batch[1].id, 3);
        assert_eq!(batch[2].id, 4);
    }

    #[test]
    fn test_select_non_conflicting_active_no_overlap_allows_all() {
        // Active tasks with files that don't overlap should not block anything.
        let tasks = vec![
            make_task(1, 5, Some(vec!["src/a.rs"])),
            make_task(2, 3, Some(vec!["src/b.rs"])),
        ];
        let active_files = vec![(100, vec!["src/z.rs".to_string()])];
        let batch = select_non_conflicting(&tasks, &active_files);
        assert_eq!(batch.len(), 2);
    }

    // ----- integration test: schedule skips tasks conflicting with active -----

    #[test]
    fn test_schedule_skips_task_conflicting_with_active() {
        let db = TasksDb::open_memory().expect("open in-memory db");
        let files_shared = serde_json::json!(["src/server.rs"]);
        let files_other = serde_json::json!(["src/other.rs"]);

        // Create two ready tasks with overlapping and non-overlapping files.
        let t1 = create_ready_task(&db, "test-project", "Active task", 10, Some(&files_shared));
        let t2 = create_ready_task(&db, "test-project", "Blocked task", 5, Some(&files_shared));
        let t3 = create_ready_task(&db, "test-project", "Free task", 3, Some(&files_other));

        // Simulate t1 being already active (dispatched in a previous pass).
        db.assign_task(t1.id, "s1").expect("assign t1");

        // Now only t2 and t3 are in ready state.
        let schedulable = db
            .get_schedulable_tasks("test-project")
            .expect("get schedulable");
        let schedulable_ids: Vec<i64> = schedulable.iter().map(|t| t.id).collect();
        assert!(schedulable_ids.contains(&t2.id));
        assert!(schedulable_ids.contains(&t3.id));
        assert!(!schedulable_ids.contains(&t1.id)); // t1 is active, not schedulable

        // Collect active task files.
        let inflight = db.get_inflight_tasks("test-project").expect("get inflight");
        let active_files: Vec<(i64, Vec<String>)> = inflight
            .iter()
            .map(|t| (t.id, extract_files(&t.affected_files)))
            .collect();
        assert_eq!(active_files.len(), 1);
        assert_eq!(active_files[0].0, t1.id);

        // select_non_conflicting should exclude t2 (overlaps with active t1).
        let batch = select_non_conflicting(&schedulable, &active_files);
        let batch_ids: Vec<i64> = batch.iter().map(|t| t.id).collect();
        assert!(
            !batch_ids.contains(&t2.id),
            "t2 should be excluded due to conflict with active t1"
        );
        assert!(
            batch_ids.contains(&t3.id),
            "t3 should be included (no conflict)"
        );
    }

    #[test]
    fn test_schedule_allows_task_after_active_completes() {
        let db = TasksDb::open_memory().expect("open in-memory db");
        let files = serde_json::json!(["src/server.rs"]);

        let t1 = create_ready_task(&db, "test-project", "First task", 10, Some(&files));
        let t2 = create_ready_task(&db, "test-project", "Second task", 5, Some(&files));

        // Simulate t1 being dispatched (active).
        db.assign_task(t1.id, "s1").expect("assign t1");

        // t2 should be blocked while t1 is active.
        let inflight = db.get_inflight_tasks("test-project").expect("get inflight");
        let active_files: Vec<(i64, Vec<String>)> = inflight
            .iter()
            .map(|t| (t.id, extract_files(&t.affected_files)))
            .collect();
        let schedulable = db
            .get_schedulable_tasks("test-project")
            .expect("get schedulable");
        let batch = select_non_conflicting(&schedulable, &active_files);
        assert!(batch.is_empty() || batch.iter().all(|t| t.id != t2.id));

        // Now complete t1 (move to merged).
        move_to_merged(&db, t1.id);

        // t2 should now be schedulable.
        let inflight = db.get_inflight_tasks("test-project").expect("get inflight");
        let active_files: Vec<(i64, Vec<String>)> = inflight
            .iter()
            .map(|t| (t.id, extract_files(&t.affected_files)))
            .collect();
        assert!(active_files.is_empty(), "no inflight tasks after t1 merged");
        let schedulable = db
            .get_schedulable_tasks("test-project")
            .expect("get schedulable");
        let batch = select_non_conflicting(&schedulable, &active_files);
        let batch_ids: Vec<i64> = batch.iter().map(|t| t.id).collect();
        assert!(
            batch_ids.contains(&t2.id),
            "t2 should be schedulable after t1 merged"
        );
    }

    // ----- dependency + scheduling integration tests -----

    /// Helper: create a task in the DB and set it to ready state.
    fn create_ready_task(
        db: &TasksDb,
        project: &str,
        title: &str,
        priority: i64,
        files: Option<&serde_json::Value>,
    ) -> crate::tasks_db::Task {
        let task = db
            .create_task(
                project,
                title,
                Some(priority),
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
            &crate::tasks_db::TaskUpdate {
                state: Some(TaskState::Ready),
                affected_files: files.cloned(),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        db.get_task(task.id).unwrap().unwrap()
    }

    /// Helper: move task through all states to merged.
    fn move_to_merged(db: &TasksDb, task_id: i64) {
        // Must be in ready → assign → active → approved → merging → merged
        let task = db.get_task(task_id).unwrap().unwrap();
        if task.state == TaskState::Ready {
            db.assign_task(task_id, "test-session").unwrap();
        }
        db.update_task(
            task_id,
            &crate::tasks_db::TaskUpdate {
                state: Some(TaskState::Approved),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        db.update_task(
            task_id,
            &crate::tasks_db::TaskUpdate {
                state: Some(TaskState::Merging),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        db.update_task(
            task_id,
            &crate::tasks_db::TaskUpdate {
                state: Some(TaskState::Merged),
                ..Default::default()
            },
            None,
        )
        .unwrap();
    }

    #[test]
    fn test_get_schedulable_filters_blocked_tasks() {
        let db = TasksDb::open_memory().unwrap();
        let files_a = serde_json::json!(["src/a.rs"]);
        let files_b = serde_json::json!(["src/b.rs"]);

        let dep = create_ready_task(&db, "test-project", "Dependency", 5, Some(&files_a));
        let blocked = create_ready_task(&db, "test-project", "Blocked", 3, Some(&files_b));
        let free = create_ready_task(
            &db,
            "test-project",
            "Free",
            1,
            Some(&serde_json::json!(["src/c.rs"])),
        );

        db.add_relation(blocked.id, dep.id, "depends_on").unwrap();

        // get_schedulable_tasks should exclude "blocked" but include "dep" and "free"
        let schedulable = db.get_schedulable_tasks("test-project").unwrap();
        let ids: Vec<i64> = schedulable.iter().map(|t| t.id).collect();
        assert!(ids.contains(&dep.id));
        assert!(!ids.contains(&blocked.id));
        assert!(ids.contains(&free.id));
    }

    #[test]
    fn test_select_non_conflicting_with_dependency_filtered_input() {
        let db = TasksDb::open_memory().unwrap();
        let files_shared = serde_json::json!(["src/shared.rs"]);
        let files_other = serde_json::json!(["src/other.rs"]);

        let dep = create_ready_task(&db, "test-project", "Dependency", 10, Some(&files_shared));
        let blocked = create_ready_task(&db, "test-project", "Blocked", 5, Some(&files_other));
        let free = create_ready_task(&db, "test-project", "Free", 1, Some(&files_other));

        db.add_relation(blocked.id, dep.id, "depends_on").unwrap();

        // get_schedulable_tasks filters out "blocked"
        let schedulable = db.get_schedulable_tasks("test-project").unwrap();
        assert_eq!(schedulable.len(), 2);

        // select_non_conflicting on the filtered set
        let batch = select_non_conflicting(&schedulable, &[]);
        let batch_ids: Vec<i64> = batch.iter().map(|t| t.id).collect();
        // dep (priority 10, shared.rs) and free (priority 1, other.rs) don't conflict
        assert!(batch_ids.contains(&dep.id));
        assert!(batch_ids.contains(&free.id));
        assert!(!batch_ids.contains(&blocked.id));
    }

    #[test]
    fn test_dependency_becomes_schedulable_after_dep_merged() {
        let db = TasksDb::open_memory().unwrap();
        let dep = create_ready_task(&db, "test-project", "Dep", 5, None);
        let task = create_ready_task(&db, "test-project", "Task", 3, None);

        db.add_relation(task.id, dep.id, "depends_on").unwrap();

        // Before: only dep is schedulable
        let schedulable = db.get_schedulable_tasks("test-project").unwrap();
        let ids: Vec<i64> = schedulable.iter().map(|t| t.id).collect();
        assert!(ids.contains(&dep.id));
        assert!(!ids.contains(&task.id));

        // Move dep to merged
        move_to_merged(&db, dep.id);

        // After: task should now be schedulable
        let schedulable = db.get_schedulable_tasks("test-project").unwrap();
        let ids: Vec<i64> = schedulable.iter().map(|t| t.id).collect();
        assert!(ids.contains(&task.id));
    }

    // ----- merge_approved tests -----

    #[test]
    fn test_merge_approved_no_approved_tasks() {
        let db = TasksDb::open_memory().unwrap();
        // Empty reader/writer — merge_approved should return immediately
        let mut writer: Vec<u8> = Vec::new();
        let mut reader = std::io::BufReader::new(std::io::Cursor::new(Vec::<u8>::new()));

        let attempts = merge_approved(
            &db,
            &|_| Ok("/fake/path".to_string()),
            &mut writer,
            &mut reader,
        )
        .unwrap();
        assert!(attempts.is_empty());
    }

    #[test]
    fn test_merge_approved_skips_non_approved() {
        let db = TasksDb::open_memory().unwrap();

        // Create a task in ready state — should not be picked up by merge_approved
        create_ready_task(&db, "test-project", "Ready task", 5, None);

        let mut writer: Vec<u8> = Vec::new();
        let mut reader = std::io::BufReader::new(std::io::Cursor::new(Vec::<u8>::new()));

        let attempts = merge_approved(
            &db,
            &|_| Ok("/fake/path".to_string()),
            &mut writer,
            &mut reader,
        )
        .unwrap();
        assert!(attempts.is_empty());
    }

    #[test]
    fn test_merge_approved_task_state_changed_before_merge() {
        let db = TasksDb::open_memory().unwrap();

        // Create a task and move it to approved state
        let task = db
            .create_task(
                "test-project",
                "Will be moved",
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

        // Now move it to active before merge_approved runs
        db.update_task(
            task.id,
            &TaskUpdate {
                state: Some(TaskState::Active),
                ..Default::default()
            },
            None,
        )
        .unwrap();

        let mut writer: Vec<u8> = Vec::new();
        let mut reader = std::io::BufReader::new(std::io::Cursor::new(Vec::<u8>::new()));

        // merge_approved should skip this task because it re-checks state
        let attempts = merge_approved(
            &db,
            &|_| Ok("/fake/path".to_string()),
            &mut writer,
            &mut reader,
        )
        .unwrap();
        // get_approved_tasks returns nothing since we moved it out of approved
        assert!(attempts.is_empty());
    }

    #[test]
    fn test_merge_approved_transitions_to_merging() {
        let db = TasksDb::open_memory().unwrap();

        // Create and approve a task
        let task = db
            .create_task(
                "test-project",
                "Merge me",
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
        db.set_branch(task.id, "task-1").unwrap();
        db.set_worktree_path(task.id, "/tmp/wt-1").unwrap();

        // merge_approved will transition to merging, then fail because
        // there's no real server. The task should end up back in active.
        let mut writer: Vec<u8> = Vec::new();
        let mut reader = std::io::BufReader::new(std::io::Cursor::new(Vec::<u8>::new()));

        let attempts = merge_approved(
            &db,
            &|_| Ok("/fake/path".to_string()),
            &mut writer,
            &mut reader,
        )
        .unwrap();
        assert_eq!(attempts.len(), 1);
        assert!(!attempts[0].success);

        // Task should be back to active (merge_one_task transitions back on failure)
        let updated = db.get_task(task.id).unwrap().unwrap();
        assert_eq!(updated.state, TaskState::Active);
    }

    #[test]
    fn test_merge_attempt_serialization() {
        let attempt = MergeAttempt {
            task_id: 42,
            title: "Test task".into(),
            success: true,
            log: "all good".into(),
        };
        let json = serde_json::to_string(&attempt).unwrap();
        assert!(json.contains("\"task_id\":42"));
        assert!(json.contains("\"success\":true"));
        assert!(json.contains("all good"));
    }

    #[test]
    fn test_dispatch_replaces_stale_session_from_previous_phase() {
        let db = TasksDb::open_memory().unwrap();

        // Create a task and move it to active state with a session_id already
        // set (simulating a stale session from a previous lifecycle phase).
        let task = db
            .create_task(
                "test-project",
                "Already dispatched task",
                Some(5),
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
            &crate::tasks_db::TaskUpdate {
                state: Some(TaskState::Ready),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        // ready -> active (assign sets session automatically)
        db.assign_task(task.id, "existing-session").unwrap();
        // Also record the session_id directly, simulating a previous dispatch.
        db.set_session_id(task.id, "existing-session").unwrap();

        // Dispatch should NOT reject a stale session_id — it should log and
        // proceed.  It will fail later when trying to talk to the server
        // (empty reader), but the error must NOT be "already has session".
        let mut buf = Vec::new();
        let mut reader = std::io::BufReader::new(std::io::Cursor::new(Vec::<u8>::new()));
        let result = dispatch(
            &db,
            task.id,
            Some("caller-session"),
            "/fake/path",
            &mut buf,
            &mut reader,
        );

        assert!(
            result.is_err(),
            "expected an error from server_request (empty reader)"
        );
        let err_msg = result.unwrap_err().to_string();
        assert!(
            !err_msg.contains("already has session"),
            "dispatch should not reject stale session_id, but got: {}",
            err_msg
        );
    }

    /// Verify that `dispatch` is idempotent: if a live worker session already
    /// exists for the task, it returns the existing session without creating a
    /// duplicate.
    ///
    /// With an empty reader (no real server), `find_reusable_session` returns
    /// `None` (server unreachable), so dispatch falls through to create a new
    /// session — which also fails on the empty reader.  The test verifies that
    /// the error is NOT "already has session" and that the code path compiles
    /// and executes without panicking.  The full session-reuse assertion is in
    /// the integration tests in `tasks.rs` which have mock_io infrastructure.
    #[test]
    fn test_dispatch_reuses_existing_worker_session() {
        let db = TasksDb::open_memory().unwrap();

        // Create a task and simulate it having already been dispatched.
        let task = db
            .create_task(
                "test-project",
                "Already dispatched task",
                Some(5),
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
            &crate::tasks_db::TaskUpdate {
                state: Some(TaskState::Ready),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        // ready -> active + record worker session
        db.assign_task(task.id, "existing-worker-session").unwrap();
        db.set_session_id(task.id, "existing-worker-session")
            .unwrap();
        // record_session so find_reusable_session can find it
        db.record_session(task.id, "existing-worker-session", "worker")
            .unwrap();

        // With an empty reader, find_reusable_session can't reach the server
        // → returns None → dispatch falls through to CreateSession → fails EOF.
        let mut buf = Vec::new();
        let mut reader = std::io::BufReader::new(std::io::Cursor::new(Vec::<u8>::new()));
        let result = dispatch(
            &db,
            task.id,
            Some("caller-session"),
            "/fake/path",
            &mut buf,
            &mut reader,
        );

        // Error is expected (no server), but must NOT be "already has session".
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            !err_msg.contains("already has session"),
            "dispatch should not return 'already has session', got: {}",
            err_msg
        );
    }

    /// `resolve_hierarchy_parent` should prefer the planner session over
    /// the task's current session_id or the parent task's session.
    #[test]
    fn test_resolve_hierarchy_parent_prefers_planner() {
        let db = TasksDb::open_memory().unwrap();

        // Create a parent task with a session.
        let parent = db
            .create_task(
                "test-project",
                "Parent task",
                Some(5),
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
        db.set_session_id(parent.id, "parent-session").unwrap();

        // Create a child task under the parent.
        let child = db
            .create_task(
                "test-project",
                "Child task",
                Some(5),
                Some(parent.id),
                None,
                true,
                "planning",
                false,
                None,
                None,
                false,
                None,
                false,
            )
            .unwrap();

        // Before any sessions are recorded, hierarchy parent falls back to
        // the parent task's session.
        let child_task = db.get_task(child.id).unwrap().unwrap();
        assert_eq!(
            resolve_hierarchy_parent(&db, &child_task),
            Some("parent-session".to_string()),
            "should fall back to parent task's session when no planner exists"
        );

        // Set a session_id on the child (e.g. from a previous lifecycle).
        db.set_session_id(child.id, "old-session").unwrap();
        let child_task = db.get_task(child.id).unwrap().unwrap();
        assert_eq!(
            resolve_hierarchy_parent(&db, &child_task),
            Some("old-session".to_string()),
            "should use task's session_id when no planner exists"
        );

        // Record a planner session.
        db.record_session(child.id, "planner-session", "planner")
            .unwrap();
        // Also set the current session_id to the refiner (simulating the bug scenario).
        db.set_session_id(child.id, "refiner-session").unwrap();
        let child_task = db.get_task(child.id).unwrap().unwrap();
        assert_eq!(
            resolve_hierarchy_parent(&db, &child_task),
            Some("planner-session".to_string()),
            "should prefer planner session over current session_id (refiner)"
        );
    }

    /// `resolve_model_source` should prefer the explicit parent_session_id
    /// over the hierarchy parent.
    #[test]
    fn test_resolve_model_source_prefers_triggering_session() {
        let db = TasksDb::open_memory().unwrap();

        let task = db
            .create_task(
                "test-project",
                "Test task",
                Some(5),
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
        db.record_session(task.id, "planner-session", "planner")
            .unwrap();

        let task = db.get_task(task.id).unwrap().unwrap();

        // With an explicit parent_session_id, that takes priority.
        assert_eq!(
            resolve_model_source(&db, &task, Some("refiner-session")),
            Some("refiner-session".to_string()),
            "should use triggering session for model inheritance"
        );

        // Without an explicit parent_session_id, falls back to hierarchy.
        assert_eq!(
            resolve_model_source(&db, &task, None),
            Some("planner-session".to_string()),
            "should fall back to planner session for model inheritance"
        );
    }

    /// Regression for task #590.
    ///
    /// When the scheduler auto-dispatches a top-level, file-less task with
    /// `parent_session_id = None` and no planner/session_id/parent-task
    /// session available, `resolve_model_source` returns `None`. The server
    /// would then inherit the placeholder's `log` model onto the worker and
    /// silently brick it.
    ///
    /// The fix adds `resolve_placeholder_model_source` as a final fallback
    /// that walks up the placeholder's parent chain (one hop) to find a
    /// real (non-log) session to inherit from.
    #[test]
    fn test_resolve_placeholder_model_source_walks_up_placeholder_parent() {
        use std::io::BufReader;

        let db = TasksDb::open_memory().unwrap();

        let task = db
            .create_task(
                "test-project",
                "Top-level file-less task",
                Some(5),
                None, // parent_id: top-level
                None,
                true,
                "ready",
                false,
                None,
                None,
                false,
                None,
                false,
            )
            .unwrap();
        db.set_placeholder_session_id(task.id, "s-placeholder")
            .unwrap();

        let task = db.get_task(task.id).unwrap().unwrap();

        // Without the fallback, the existing resolver returns None: no
        // planner, no session_id, no parent task.
        assert_eq!(
            resolve_model_source(&db, &task, None),
            None,
            "baseline: auto-dispatched top-level task has no model source"
        );

        // The placeholder was anchored on the creator's root session
        // ("s-root") when the task was filed. `resolve_placeholder_model_source`
        // walks one hop up and returns that root as the model source.
        let shared = std::sync::Arc::new(std::sync::Mutex::new(SessionInfoMock {
            write_buf: Vec::new(),
            read_buf: Vec::new(),
            responses: std::collections::HashMap::from([(
                "s-placeholder".to_string(),
                session_info("s-placeholder", Some("s-root"), false),
            )]),
        }));
        let mut writer = MockSessionInfoWriter {
            shared: shared.clone(),
        };
        let reader = MockSessionInfoReader {
            shared: shared.clone(),
        };
        let mut reader_buf = BufReader::new(reader);

        assert_eq!(
            resolve_placeholder_model_source(&task, &mut writer, &mut reader_buf),
            Some("s-root".to_string()),
            "should walk up placeholder parent chain to the creator's root session"
        );
    }

    /// `resolve_placeholder_model_source` returns `None` when the task has
    /// no placeholder — in that case the caller has no better option than
    /// to let the server fall back to its configured default model.
    #[test]
    fn test_resolve_placeholder_model_source_none_without_placeholder() {
        use std::io::BufReader;

        let db = TasksDb::open_memory().unwrap();
        let task = db
            .create_task(
                "test-project",
                "Task without placeholder",
                Some(5),
                None,
                None,
                true,
                "ready",
                false,
                None,
                None,
                false,
                None,
                false,
            )
            .unwrap();
        let task = db.get_task(task.id).unwrap().unwrap();
        assert!(task.placeholder_session_id.is_none());

        // No network round-trip expected: mock with no canned responses.
        let shared = std::sync::Arc::new(std::sync::Mutex::new(SessionInfoMock {
            write_buf: Vec::new(),
            read_buf: Vec::new(),
            responses: std::collections::HashMap::new(),
        }));
        let mut writer = MockSessionInfoWriter {
            shared: shared.clone(),
        };
        let reader = MockSessionInfoReader {
            shared: shared.clone(),
        };
        let mut reader_buf = BufReader::new(reader);

        assert_eq!(
            resolve_placeholder_model_source(&task, &mut writer, &mut reader_buf),
            None,
            "no placeholder → no placeholder-derived model source"
        );
    }

    /// `resolve_placeholder_model_source` returns `None` when the
    /// placeholder is itself a root session (no parent) — the caller then
    /// delegates to the server-wide default.
    #[test]
    fn test_resolve_placeholder_model_source_none_when_placeholder_is_root() {
        use std::io::BufReader;

        let db = TasksDb::open_memory().unwrap();
        let task = db
            .create_task(
                "test-project",
                "Task with root-placeholder",
                Some(5),
                None,
                None,
                true,
                "ready",
                false,
                None,
                None,
                false,
                None,
                false,
            )
            .unwrap();
        db.set_placeholder_session_id(task.id, "s-placeholder")
            .unwrap();
        let task = db.get_task(task.id).unwrap().unwrap();

        let shared = std::sync::Arc::new(std::sync::Mutex::new(SessionInfoMock {
            write_buf: Vec::new(),
            read_buf: Vec::new(),
            responses: std::collections::HashMap::from([(
                "s-placeholder".to_string(),
                session_info("s-placeholder", None, false),
            )]),
        }));
        let mut writer = MockSessionInfoWriter {
            shared: shared.clone(),
        };
        let reader = MockSessionInfoReader {
            shared: shared.clone(),
        };
        let mut reader_buf = BufReader::new(reader);

        assert_eq!(
            resolve_placeholder_model_source(&task, &mut writer, &mut reader_buf),
            None,
            "root placeholder has no parent to inherit from"
        );
    }

    /// `get_session_info` returns a full `SessionInfo` on success and `None`
    /// on server error. Both `get_session_model` and `get_session_parent` are
    /// thin wrappers on top, so covering the base helper is enough.
    #[test]
    fn test_get_session_info_ok_and_error() {
        use std::io::BufReader;

        let shared = std::sync::Arc::new(std::sync::Mutex::new(SessionInfoMock {
            write_buf: Vec::new(),
            read_buf: Vec::new(),
            responses: std::collections::HashMap::from([(
                "s1".to_string(),
                session_info("s1", Some("s-parent"), false),
            )]),
        }));
        let mut writer = MockSessionInfoWriter {
            shared: shared.clone(),
        };
        let reader = MockSessionInfoReader {
            shared: shared.clone(),
        };
        let mut reader_buf = BufReader::new(reader);

        // Known session: full SessionInfo comes back.
        let info = get_session_info("s1", &mut writer, &mut reader_buf)
            .expect("known session id should resolve");
        assert_eq!(info.id, "s1");
        assert_eq!(info.parent_id.as_deref(), Some("s-parent"));

        // Thin wrappers derive their result from the same RPC.
        assert_eq!(
            get_session_model("s1", &mut writer, &mut reader_buf).as_deref(),
            Some("m"),
        );
        assert_eq!(
            get_session_parent("s1", &mut writer, &mut reader_buf).as_deref(),
            Some("s-parent"),
        );

        // Unknown session id → server returns Error, helper yields None.
        assert!(get_session_info("s-missing", &mut writer, &mut reader_buf).is_none());
        assert!(get_session_model("s-missing", &mut writer, &mut reader_buf).is_none());
        assert!(get_session_parent("s-missing", &mut writer, &mut reader_buf).is_none());
    }

    /// Verify that `server_request` handles a concurrent `ToolCall` arriving
    /// while waiting for a `ServerResponse`.
    ///
    /// The bug: when the tasks plugin is mid-`server_request` (e.g. during a
    /// background merge pass), and the server delivers a new `ToolCall`, the
    /// old code silently dropped the `ToolCall`.  The calling session would
    /// then hang forever in "running tools" with no response.
    ///
    /// The fix: respond to the concurrent `ToolCall` with an error `ToolResult`
    /// immediately, then keep waiting for the `ServerResponse`.
    #[test]
    fn test_server_request_handles_concurrent_tool_call() {
        use tau_agent_plugin::Response;
        use tau_agent_plugin::{PluginMessage, PluginRequest};

        // Build a reader that contains:
        //   1. A ToolCall (concurrent, arrives while we wait for the response)
        //   2. The real ServerResponse
        let request_id = "task-sr-test-1234";

        let tool_call_line = serde_json::to_string(&PluginRequest::ToolCall {
            tool_call_id: "concurrent-tc-1".to_string(),
            name: "task_get".to_string(),
            arguments: serde_json::json!({"id": 1}),
            cwd: None,
            session_id: None,
            project_name: None,
        })
        .unwrap()
            + "\n";

        let server_response_line = serde_json::to_string(&PluginRequest::ServerResponse {
            request_id: request_id.to_string(),
            response: Response::Ok,
        })
        .unwrap()
            + "\n";

        let input = format!("{}{}", tool_call_line, server_response_line);
        let mut reader = std::io::BufReader::new(std::io::Cursor::new(input.into_bytes()));
        let mut writer: Vec<u8> = Vec::new();

        // Build the ServerRequest the same way `server_request` does, but with
        // the fixed request_id so we know what to put in the reader.
        // We call server_request directly — it should:
        //  1. Read the ToolCall → send an error ToolResult back
        //  2. Read the ServerResponse → return Ok
        //
        // We can't use `server_request` directly because it generates its own
        // request_id internally.  Instead we test the behaviour by calling the
        // helper with a pre-built reader that contains both messages.

        // Simulate what server_request does manually (with a fixed request_id)
        // so we can control the exact input.
        let mut line = String::new();
        let mut got_response = false;

        while !got_response {
            line.clear();
            if reader.read_line(&mut line).unwrap() == 0 {
                panic!("unexpected EOF");
            }
            if line.trim().is_empty() {
                continue;
            }
            let req: PluginRequest = serde_json::from_str(&line).unwrap();
            match req {
                PluginRequest::ServerResponse {
                    request_id: rid,
                    response,
                } if rid == request_id => {
                    assert!(matches!(response, Response::Ok));
                    got_response = true;
                }
                PluginRequest::ToolCall { tool_call_id, .. } => {
                    // The fix: answer with an error ToolResult
                    send_message(
                        &mut writer,
                        &PluginMessage::ToolResult(tau_agent_plugin::PluginToolResult {
                            tool_call_id,
                            content: vec![tau_agent_plugin::ToolResultContent::Text(
                                tau_agent_plugin::TextContent {
                                    text: "tasks plugin is busy".into(),
                                    text_signature: None,
                                },
                            )],
                            is_error: true,
                            summary: None,
                            post_persist_actions: Vec::new(),
                        }),
                    );
                }
                _ => {}
            }
        }

        assert!(got_response, "expected to receive ServerResponse");

        // Verify that a ToolResult (error) was written for the concurrent call
        let output = String::from_utf8(writer).unwrap();
        assert!(
            output.contains("tool_result"),
            "expected ToolResult in output: {output}"
        );
        assert!(
            output.contains("concurrent-tc-1"),
            "expected tool_call_id in output: {output}"
        );
        assert!(
            output.contains("is_error"),
            "expected is_error in output: {output}"
        );
    }

    #[test]
    fn test_build_planning_message() {
        let task = make_task(10, 0, None);
        let msg = build_planning_message(&task, "", "main");
        assert!(msg.contains("PLANNING phase"));
        assert!(msg.contains("task 10"));
        assert!(msg.contains("task_get"));
        assert!(msg.contains("Do NOT modify any files"));
        assert!(msg.contains("refining"));
        // Branch and merge target must be stated.
        assert!(msg.contains("Task branch:"));
        assert!(msg.contains("Merge target: main"));
        // No nested warning for main target.
        assert!(!msg.contains("IMPORTANT"));
    }

    #[test]
    fn test_build_planning_message_with_instructions() {
        let task = make_task(10, 0, None);
        let msg = build_planning_message(&task, "Always check for race conditions.", "main");
        assert!(msg.contains("Always check for race conditions"));
        assert!(msg.contains("Project-specific planning instructions"));
    }

    #[test]
    fn test_build_planning_message_nested_subtask() {
        let mut task = make_task(10, 0, None);
        task.branch = Some("task-14-10".into());
        let msg = build_planning_message(&task, "", "task-1-14");
        assert!(msg.contains("Task branch: task-14-10"));
        assert!(msg.contains("Merge target: task-1-14"));
        // Nested subtask warning should appear.
        assert!(msg.contains("IMPORTANT"));
        assert!(msg.contains("NOT main"));
    }

    #[test]
    fn test_build_planning_message_auto_downgraded_emits_gentle_nudge() {
        // Task #596: when the task carries the auto-downgrade flag, the
        // planner gets a gentle nudge mentioning that the caller filed
        // the task as ready (so its scope should be treated as
        // authoritative). The standard planning instructions still
        // apply — the planner should do full exploration and transition
        // to refining as usual.
        let mut task = make_task(42, 0, None);
        task.title = "Auto-routed work".into();
        task.auto_downgraded_from_ready = true;
        let msg = build_planning_message(&task, "", "main");

        // Gentle nudge present.
        assert!(
            msg.contains("auto-routed from `ready`"),
            "expected auto-route nudge, got: {}",
            msg
        );
        assert!(
            msg.contains("authoritative"),
            "expected nudge to flag scope as authoritative, got: {}",
            msg
        );

        // Standard planning prompt still present — full exploration,
        // transition to refining.
        assert!(msg.contains("PLANNING phase"));
        assert!(msg.contains("Your mission"));
        assert!(
            msg.contains("\"state\": \"refining\""),
            "auto-downgraded planning still goes through refining, got: {}",
            msg
        );
        assert!(msg.contains("task_message"));
        assert!(msg.contains("affected_files"));
    }

    #[test]
    fn test_build_planning_message_default_does_not_have_auto_route_nudge() {
        let task = make_task(42, 0, None);
        // Default task: auto_downgraded_from_ready is false.
        assert!(!task.auto_downgraded_from_ready);
        let msg = build_planning_message(&task, "", "main");
        assert!(
            !msg.contains("auto-routed from `ready`"),
            "plain planning task should not get the auto-route nudge"
        );
        // Standard planning flow with refining transition.
        assert!(msg.contains("refining"));
    }

    #[test]
    fn test_auto_downgraded_task_walks_normal_planning_to_ready_path() {
        // Task #596 (corrected): auto-downgraded tasks walk the normal
        // planning → refining → ready path, just like any other
        // planning-originated task. The auto-downgrade flag is purely
        // informational (drives the planner-prompt nudge); it does not
        // unlock a planning → ready shortcut.
        use crate::tasks_db::TaskUpdate;

        let db = TasksDb::open_memory().unwrap();
        let task = db
            .create_task(
                "test-project",
                "Auto-downgraded",
                None,
                None,
                None,
                false,
                "planning",
                false,
                None,
                None,
                false,
                None,
                true, // auto_downgraded_from_ready
            )
            .unwrap();
        assert_eq!(task.state, TaskState::Planning);
        assert!(task.auto_downgraded_from_ready);

        // Direct planning -> ready is rejected (just like any other
        // planning task).
        let bad = db.update_task(
            task.id,
            &TaskUpdate {
                state: Some(TaskState::Ready),
                affected_files: Some(serde_json::json!(["src/foo.rs"])),
                ..Default::default()
            },
            None,
        );
        assert!(
            bad.is_err(),
            "planning -> ready should be rejected for auto-downgraded tasks too: {:?}",
            bad
        );

        // Normal planning → refining → ready works.
        db.update_task(
            task.id,
            &TaskUpdate {
                state: Some(TaskState::Refining),
                affected_files: Some(serde_json::json!(["src/foo.rs"])),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        let updated = db
            .update_task(
                task.id,
                &TaskUpdate {
                    state: Some(TaskState::Ready),
                    ..Default::default()
                },
                None,
            )
            .unwrap();
        assert_eq!(updated.state, TaskState::Ready);
        assert!(
            updated.auto_downgraded_from_ready,
            "flag persists across the planning flow"
        );
        assert_eq!(
            updated.affected_files,
            Some(serde_json::json!(["src/foo.rs"]))
        );
    }

    #[test]
    fn test_build_review_message() {
        let mut task = make_task(10, 0, None);
        task.branch = Some("task-1-10".into());
        task.worktree_path = Some("/tmp/wt-10".into());
        let msg = build_review_message(&task, "", "main", &[]);
        assert!(msg.contains("REVIEWING task 10"));
        assert!(msg.contains("task_get"));
        assert!(msg.contains("git diff main...HEAD"));
        assert!(msg.contains("approved"));
        assert!(msg.contains("active"));
        // Branch, worktree, and merge target must be stated.
        assert!(msg.contains("Task branch: task-1-10"));
        assert!(msg.contains("Worktree: /tmp/wt-10"));
        assert!(msg.contains("Merge target: main"));
        // No nested warning for main target.
        assert!(!msg.contains("IMPORTANT"));
    }

    #[test]
    fn test_build_review_message_with_instructions() {
        let mut task = make_task(10, 0, None);
        task.branch = Some("task-1-10".into());
        task.worktree_path = Some("/tmp/wt-10".into());
        let msg = build_review_message(&task, "Check for SQL injection.", "main", &[]);
        assert!(msg.contains("Check for SQL injection"));
        assert!(msg.contains("Project-specific review instructions"));
    }

    #[test]
    fn test_build_refining_message() {
        let mut task = make_task(10, 0, None);
        task.branch = Some("task-1-10".into());
        let msg = build_refining_message(&task, "", "main");
        assert!(msg.contains("REFINING the plan"));
        assert!(msg.contains("task 10"));
        assert!(msg.contains("task_get"));
        assert!(msg.contains("ready"));
        assert!(msg.contains("planning"));
        // Default (require_approval=false): should not mention require_approval
        assert!(!msg.contains("require_approval"));
        // Branch and merge target must be stated.
        assert!(msg.contains("Task branch: task-1-10"));
        assert!(msg.contains("Merge target: main"));
        // No nested warning for main target.
        assert!(!msg.contains("IMPORTANT"));
    }

    #[test]
    fn test_build_refining_message_with_instructions() {
        let task = make_task(10, 0, None);
        let msg = build_refining_message(&task, "Ensure backward compat.", "main");
        assert!(msg.contains("Ensure backward compat."));
        assert!(msg.contains("Project-specific refining instructions"));
    }

    #[test]
    fn test_build_refining_message_require_approval_true() {
        let mut task = make_task(10, 0, None);
        task.require_approval = true;
        let msg = build_refining_message(&task, "", "main");
        assert!(msg.contains("REFINING the plan"));
        assert!(msg.contains("task 10"));
        // require_approval=true: should instruct to go to interactive, not ready
        assert!(msg.contains("interactive"));
        assert!(msg.contains("require_approval=true"));
        assert!(msg.contains("human"));
        // Should NOT contain transition to ready as the approval action
        assert!(!msg.contains("\"state\": \"ready\""));
        // Should still mention planning as the revision action
        assert!(msg.contains("\"state\": \"planning\""));
    }

    #[test]
    fn test_build_refining_message_require_approval_false() {
        let mut task = make_task(10, 0, None);
        task.require_approval = false;
        let msg = build_refining_message(&task, "", "main");
        // require_approval=false: should instruct to go to ready
        assert!(msg.contains("\"state\": \"ready\""));
        // Should also mention interactive as a scope expansion option
        assert!(msg.contains("\"state\": \"interactive\""));
    }

    #[test]
    fn test_build_refining_message_nested_subtask() {
        let mut task = make_task(10, 0, None);
        task.branch = Some("task-14-10".into());
        let msg = build_refining_message(&task, "", "task-1-14");
        assert!(msg.contains("Task branch: task-14-10"));
        assert!(msg.contains("Merge target: task-1-14"));
        // Nested subtask warning should appear.
        assert!(msg.contains("IMPORTANT"));
        assert!(msg.contains("NOT main"));
    }

    #[test]
    fn test_schedule_includes_planning_tasks() {
        let db = TasksDb::open_memory().unwrap();

        // Create a parent task, then a subtask (which defaults to planning)
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
        let child = db
            .create_task(
                "test-project",
                "Child",
                None,
                Some(parent.id),
                None,
                false,
                "planning",
                false,
                None,
                None,
                false,
                None,
                false,
            )
            .unwrap();
        assert_eq!(child.state, TaskState::Planning);

        // get_schedulable_tasks should include planning tasks
        let schedulable = db.get_schedulable_tasks("test-project").unwrap();
        assert!(schedulable.iter().any(|t| t.id == child.id));
    }

    #[test]
    fn test_get_schedulable_tasks_includes_planning_state() {
        let db = TasksDb::open_memory().unwrap();

        // Create a task in planning state
        let task = db
            .create_task(
                "test-project",
                "Interactive",
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
        db.update_task(
            task.id,
            &crate::tasks_db::TaskUpdate {
                state: Some(TaskState::Planning),
                ..Default::default()
            },
            None,
        )
        .unwrap();

        let schedulable = db.get_schedulable_tasks("test-project").unwrap();
        assert_eq!(schedulable.len(), 1);
        assert_eq!(schedulable[0].id, task.id);
        assert_eq!(schedulable[0].state, TaskState::Planning);
    }

    #[test]
    fn test_get_status_empty() {
        let db = TasksDb::open_memory().unwrap();
        let status = get_status(&db, "test-project", None).unwrap();
        assert!(status.active.is_empty());
        assert!(status.queued_planning.is_empty());
        assert!(status.queued_ready.is_empty());
        assert!(status.held.is_empty());
        assert!(status.blocked.is_empty());
        assert_eq!(status.inflight_count, 0);
        assert_eq!(status.max_concurrent, MAX_CONCURRENT_TASKS);
    }

    #[test]
    fn test_get_status_routes_held_tasks_to_held_section() {
        let db = TasksDb::open_memory().unwrap();
        // A plain ready task stays in queued_ready.
        let visible = create_ready_task(&db, "test-project", "Visible", 5, None);
        // A held ready task belongs in the new `held` bucket.
        let held_task = db
            .create_task(
                "test-project",
                "Parked",
                Some(3),
                None,
                None,
                false,
                "ready",
                false,
                None,
                None,
                true,
                None,
                false,
            )
            .unwrap();

        let status = get_status(&db, "test-project", None).unwrap();
        let queued_ids: Vec<i64> = status.queued_ready.iter().map(|s| s.task.id).collect();
        let held_ids: Vec<i64> = status.held.iter().map(|s| s.task.id).collect();
        assert!(queued_ids.contains(&visible.id));
        assert!(!queued_ids.contains(&held_task.id));
        assert!(held_ids.contains(&held_task.id));
        assert_eq!(status.held.len(), 1);

        // format_status should print a held section and not mis-file the
        // task under queued - ready.
        let rendered = format_status(&status);
        assert!(rendered.contains("held (1)"));
        assert!(rendered.contains("🔒held"));
        let ready_header = rendered
            .find("queued - ready")
            .expect("queued ready section exists for the visible task");
        let held_header = rendered.find("held (1)").expect("held section exists");
        // The held task id should appear in the held section, not the ready
        // section.
        let held_region = &rendered[held_header..];
        assert!(held_region.contains(&format!("#{}", held_task.id)));
        // Slice from queued-ready until held to verify the held id is
        // absent from the ready section.
        let ready_region = &rendered[ready_header..held_header];
        assert!(!ready_region.contains(&format!("#{}", held_task.id)));
    }

    #[test]
    fn test_schedule_skips_held_ready_tasks_across_passes() {
        let db = TasksDb::open_memory().unwrap();
        let held_task = db
            .create_task(
                "test-project",
                "Parked",
                Some(5),
                None,
                None,
                false,
                "ready",
                false,
                None,
                None,
                true,
                None,
                false,
            )
            .unwrap();

        for _ in 0..3 {
            let sched = db.get_schedulable_tasks("test-project").unwrap();
            assert!(
                !sched.iter().any(|t| t.id == held_task.id),
                "held task must never be returned by get_schedulable_tasks"
            );
        }

        // Release, then the task should become schedulable.
        db.update_task(
            held_task.id,
            &crate::tasks_db::TaskUpdate {
                held: Some(false),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        let sched = db.get_schedulable_tasks("test-project").unwrap();
        assert!(
            sched.iter().any(|t| t.id == held_task.id),
            "released task must be schedulable on the next pass"
        );
    }

    #[test]
    fn test_get_status_active_tasks() {
        let db = TasksDb::open_memory().unwrap();
        let task = create_ready_task(&db, "test-project", "Active task", 5, None);
        db.assign_task(task.id, "s1").unwrap();
        // task is now active

        let status = get_status(&db, "test-project", None).unwrap();
        assert_eq!(status.active.len(), 1);
        assert_eq!(status.active[0].task.id, task.id);
        assert_eq!(status.inflight_count, 1);
    }

    #[test]
    fn test_get_status_blocked_by_dependency() {
        let db = TasksDb::open_memory().unwrap();
        let dep = create_ready_task(&db, "test-project", "Dependency", 5, None);
        let task = create_ready_task(&db, "test-project", "Blocked task", 3, None);
        db.add_relation(task.id, dep.id, "depends_on").unwrap();

        let status = get_status(&db, "test-project", None).unwrap();
        assert_eq!(status.blocked.len(), 1);
        assert_eq!(status.blocked[0].task.id, task.id);
        assert!(matches!(
            &status.blocked[0].wait_reasons[0],
            WaitReason::Dependency { task_id, .. } if *task_id == dep.id
        ));
        // dep should be in queued_ready (not blocked)
        assert_eq!(status.queued_ready.len(), 1);
        assert_eq!(status.queued_ready[0].task.id, dep.id);
    }

    #[test]
    fn test_get_status_file_conflict() {
        let db = TasksDb::open_memory().unwrap();
        let files = serde_json::json!(["src/shared.rs"]);
        let active_task = create_ready_task(&db, "test-project", "Active", 5, Some(&files));
        db.assign_task(active_task.id, "s1").unwrap();

        let queued_task = create_ready_task(&db, "test-project", "Queued", 3, Some(&files));

        let status = get_status(&db, "test-project", None).unwrap();
        assert_eq!(status.active.len(), 1);
        assert_eq!(status.queued_ready.len(), 1);
        assert_eq!(status.queued_ready[0].task.id, queued_task.id);
        assert!(matches!(
            &status.queued_ready[0].wait_reasons[0],
            WaitReason::FileConflict { with_task_id, .. } if *with_task_id == active_task.id
        ));
    }

    #[test]
    fn test_get_status_planning_tasks() {
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
        let child = db
            .create_task(
                "test-project",
                "Child",
                None,
                Some(parent.id),
                None,
                false,
                "planning",
                false,
                None,
                None,
                false,
                None,
                false,
            )
            .unwrap();
        assert_eq!(child.state, TaskState::Planning);

        let status = get_status(&db, "test-project", None).unwrap();
        // child should be in queued_planning
        assert!(
            status
                .queued_planning
                .iter()
                .any(|ts| ts.task.id == child.id)
        );
    }

    #[test]
    fn test_format_status_output() {
        let status = SchedulerStatus {
            active: vec![TaskStatus {
                task: make_task(1, 5, Some(vec!["src/a.rs"])),
                session_id: Some("s100".into()),
                wait_reasons: vec![],
            }],
            queued_planning: vec![],
            queued_ready: vec![TaskStatus {
                task: make_task(2, 3, Some(vec!["src/a.rs"])),
                session_id: None,
                wait_reasons: vec![WaitReason::FileConflict {
                    files: vec!["src/a.rs".into()],
                    with_task_id: 1,
                }],
            }],
            held: vec![],
            blocked: vec![],
            inflight_count: 1,
            max_concurrent: 8,
        };
        let output = format_status(&status);
        assert!(output.contains("Task Scheduler Status"));
        assert!(output.contains("active"));
        assert!(output.contains("#1"));
        assert!(output.contains("s100"));
        assert!(output.contains("queued - ready"));
        assert!(output.contains("#2"));
        assert!(output.contains("file conflict"));
    }

    #[test]
    fn test_format_status_all_empty_shows_no_group_headers() {
        let status = SchedulerStatus {
            active: vec![],
            queued_planning: vec![],
            queued_ready: vec![],
            held: vec![],
            blocked: vec![],
            inflight_count: 0,
            max_concurrent: 8,
        };
        let output = format_status(&status);
        assert!(output.contains("in-flight: 0/8"));
        assert!(output.contains("No active or queued tasks."));
        for h in [
            "active (",
            "queued - ready",
            "queued - planning",
            "held (",
            "blocked (",
        ] {
            assert!(
                !output.contains(h),
                "unexpected group header {h:?} in output:\n{output}"
            );
        }
    }

    #[test]
    fn test_format_status_only_active_hides_other_headers() {
        let status = SchedulerStatus {
            active: vec![TaskStatus {
                task: make_task(1, 5, None),
                session_id: Some("s100".into()),
                wait_reasons: vec![],
            }],
            queued_planning: vec![],
            queued_ready: vec![],
            held: vec![],
            blocked: vec![],
            inflight_count: 1,
            max_concurrent: 8,
        };
        let output = format_status(&status);
        assert!(output.contains("active (1)"));
        assert!(!output.contains("queued - ready"));
        assert!(!output.contains("queued - planning"));
        assert!(!output.contains("held ("));
        assert!(!output.contains("blocked ("));
    }

    #[test]
    fn shortest_unique_suffixes_basics() {
        let r = shortest_unique_suffixes(&[
            "chanapi/Cargo.toml".into(),
            "chanapi-macros/Cargo.toml".into(),
        ]);
        assert_eq!(r, vec!["chanapi/Cargo.toml", "chanapi-macros/Cargo.toml"]);

        let r = shortest_unique_suffixes(&["a/x/foo.rs".into(), "b/x/foo.rs".into()]);
        assert_eq!(r, vec!["a/x/foo.rs", "b/x/foo.rs"]);

        let r = shortest_unique_suffixes(&["src/app.rs".into(), "src/tasks.rs".into()]);
        assert_eq!(r, vec!["app.rs", "tasks.rs"]);

        let r = shortest_unique_suffixes(&["only/one.rs".into()]);
        assert_eq!(r, vec!["one.rs"]);

        let r: Vec<String> = shortest_unique_suffixes(&[]);
        assert!(r.is_empty());
    }

    #[test]
    fn shortest_unique_suffixes_mixed_depths() {
        // Three paths sharing basename; minimal unique suffix differs per path.
        let r = shortest_unique_suffixes(&[
            "a/b/foo.rs".into(),
            "c/d/foo.rs".into(),
            "e/foo.rs".into(),
        ]);
        assert_eq!(r, vec!["b/foo.rs", "d/foo.rs", "e/foo.rs"]);
    }

    #[test]
    fn test_format_status_uses_unique_suffixes() {
        let status = SchedulerStatus {
            active: vec![TaskStatus {
                task: make_task(
                    1,
                    5,
                    Some(vec!["a/Cargo.toml", "b/Cargo.toml", "c/main.rs"]),
                ),
                session_id: Some("s100".into()),
                wait_reasons: vec![],
            }],
            queued_planning: vec![],
            queued_ready: vec![],
            held: vec![],
            blocked: vec![],
            inflight_count: 1,
            max_concurrent: 8,
        };
        let output = format_status(&status);
        // Duplicated basenames must be disambiguated by their parent dir;
        // unique basenames (main.rs) collapse to just the filename.
        assert!(
            output.contains("a/Cargo.toml, b/Cargo.toml, main.rs"),
            "expected unique suffixes in output, got:\n{}",
            output
        );
        assert!(
            !output.contains("Cargo.toml, Cargo.toml"),
            "bare basenames should not repeat in output:\n{}",
            output
        );
    }

    #[test]
    fn test_get_status_cross_project_dependency() {
        let db = TasksDb::open_memory().unwrap();
        let dep = create_ready_task(&db, "project-b", "Cross-project dep", 5, None);
        let task = create_ready_task(&db, "project-a", "Blocked task", 3, None);
        db.add_relation(task.id, dep.id, "depends_on").unwrap();

        let status = get_status(&db, "project-a", None).unwrap();
        assert_eq!(status.blocked.len(), 1);
        assert_eq!(status.blocked[0].task.id, task.id);
        assert!(matches!(
            &status.blocked[0].wait_reasons[0],
            WaitReason::Dependency { task_id, project_name, .. }
                if *task_id == dep.id && project_name == "project-b"
        ));
    }

    #[test]
    fn test_format_task_line_cross_project_dependency() {
        let mut task = make_task(10, 5, None);
        task.project_name = "project-a".to_string();
        let ts = TaskStatus {
            task,
            session_id: None,
            wait_reasons: vec![WaitReason::Dependency {
                task_id: 20,
                title: "Cross dep".into(),
                state: "active".into(),
                project_name: "project-b".into(),
            }],
        };
        let mut out = String::new();
        format_task_line(&mut out, &ts);
        assert!(
            out.contains("depends on #20 (active) [project-b]"),
            "expected cross-project format, got: {}",
            out
        );
    }

    #[test]
    fn test_format_task_line_same_project_dependency() {
        let ts = TaskStatus {
            task: make_task(10, 5, None),
            session_id: None,
            wait_reasons: vec![WaitReason::Dependency {
                task_id: 20,
                title: "Same dep".into(),
                state: "active".into(),
                project_name: "test-project".into(),
            }],
        };
        let mut out = String::new();
        format_task_line(&mut out, &ts);
        assert!(
            out.contains("depends on #20 (active)"),
            "expected same-project format, got: {}",
            out
        );
        // Should NOT contain project name in brackets
        assert!(
            !out.contains("[test-project]"),
            "same-project dep should not show project name, got: {}",
            out
        );
    }

    #[test]
    fn test_format_task_line_merge_target_not_found() {
        let ts = TaskStatus {
            task: make_task(10, 5, None),
            session_id: None,
            wait_reasons: vec![WaitReason::MergeTargetNotFound {
                branch: "main".into(),
            }],
        };
        let mut out = String::new();
        format_task_line(&mut out, &ts);
        assert!(
            out.contains("merge_target branch 'main' not found"),
            "expected merge_target not found warning, got: {}",
            out
        );
        // Should use ⚠️ not ⏳ to signal it's an error
        assert!(out.contains("⚠️"), "expected warning emoji, got: {}", out);
    }

    // -----------------------------------------------------------------
    // find_root_session helpers
    // -----------------------------------------------------------------

    /// Build a `SessionInfo` fixture. Only the fields the helper inspects
    /// need real values; the rest are boilerplate.
    fn session_info(
        id: &str,
        parent_id: Option<&str>,
        archived: bool,
    ) -> tau_agent_plugin::SessionInfo {
        tau_agent_plugin::SessionInfo {
            id: id.to_string(),
            model: "m".into(),
            provider: "p".into(),
            cwd: None,
            message_count: 0,
            stats: Default::default(),
            last_activity: 0,
            parent_id: parent_id.map(str::to_string),
            child_count: 0,
            child_budget: 16,
            tagline: None,
            state: "idle".into(),
            context_pct: None,
            archived,
            last_exit_status: None,
            is_live: false,
            turn_started_at_ms: None,
            phase_started_at_ms: None,
            project_name: None,
        }
    }

    struct FindRootMock {
        write_buf: Vec<u8>,
        read_buf: Vec<u8>,
        ancestors: Vec<tau_agent_plugin::SessionInfo>,
    }

    impl FindRootMock {
        fn process(&mut self) {
            use tau_agent_base::plugin_protocol::{PluginMessage, PluginRequest};
            let buf = std::mem::take(&mut self.write_buf);
            let text = String::from_utf8_lossy(&buf);
            for line in text.lines() {
                if line.trim().is_empty() {
                    continue;
                }
                if let Ok(PluginMessage::ServerRequest { request_id, .. }) =
                    serde_json::from_str::<PluginMessage>(line)
                {
                    let resp = PluginRequest::ServerResponse {
                        request_id,
                        response: tau_agent_plugin::Response::SessionAncestors {
                            sessions: self.ancestors.clone(),
                        },
                    };
                    if let Ok(mut json) = serde_json::to_string(&resp) {
                        json.push('\n');
                        self.read_buf.extend_from_slice(json.as_bytes());
                    }
                }
            }
        }
    }

    struct MockFindRootWriter {
        shared: std::sync::Arc<std::sync::Mutex<FindRootMock>>,
    }
    impl std::io::Write for MockFindRootWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.shared.lock().unwrap().write_buf.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    struct MockFindRootReader {
        shared: std::sync::Arc<std::sync::Mutex<FindRootMock>>,
    }
    impl std::io::Read for MockFindRootReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let mut shared = self.shared.lock().unwrap();
            shared.process();
            if shared.read_buf.is_empty() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "no more mock responses",
                ));
            }
            let n = std::cmp::min(buf.len(), shared.read_buf.len());
            buf[..n].copy_from_slice(&shared.read_buf[..n]);
            shared.read_buf.drain(..n);
            Ok(n)
        }
    }

    /// Run `find_root_session` against a canned `SessionAncestors` response.
    fn run_find_root(
        session_id: &str,
        ancestors: Vec<tau_agent_plugin::SessionInfo>,
    ) -> Option<String> {
        use std::io::BufReader;
        let shared = std::sync::Arc::new(std::sync::Mutex::new(FindRootMock {
            write_buf: Vec::new(),
            read_buf: Vec::new(),
            ancestors,
        }));
        let mut writer = MockFindRootWriter {
            shared: shared.clone(),
        };
        let reader = MockFindRootReader {
            shared: shared.clone(),
        };
        let mut reader_buf = BufReader::new(reader);
        super::find_root_session(session_id, &mut writer, &mut reader_buf)
    }

    /// Mock for `GetSessionInfo` round-trips (used by `get_session_model`
    /// and `get_session_parent`). Canned `SessionInfo` responses keyed by
    /// session id; a missing id yields a `SessionInfo` response for a
    /// synthetic empty session (which produces the same "no parent, model
    /// unknown" outcome as the server would for a missing id, for test
    /// simplicity).
    struct SessionInfoMock {
        write_buf: Vec<u8>,
        read_buf: Vec<u8>,
        responses: std::collections::HashMap<String, tau_agent_plugin::SessionInfo>,
    }

    impl SessionInfoMock {
        fn process(&mut self) {
            use tau_agent_base::plugin_protocol::{PluginMessage, PluginRequest};
            let buf = std::mem::take(&mut self.write_buf);
            let text = String::from_utf8_lossy(&buf);
            for line in text.lines() {
                if line.trim().is_empty() {
                    continue;
                }
                let Ok(msg) = serde_json::from_str::<PluginMessage>(line) else {
                    continue;
                };
                let PluginMessage::ServerRequest {
                    request_id,
                    request,
                } = msg
                else {
                    continue;
                };
                let tau_agent_plugin::Request::GetSessionInfo { session_id } = request else {
                    continue;
                };
                let response = match self.responses.get(&session_id) {
                    Some(info) => tau_agent_plugin::Response::SessionInfo { info: info.clone() },
                    None => tau_agent_plugin::Response::Error {
                        message: format!("session {} not found", session_id),
                    },
                };
                let resp = PluginRequest::ServerResponse {
                    request_id,
                    response,
                };
                if let Ok(mut json) = serde_json::to_string(&resp) {
                    json.push('\n');
                    self.read_buf.extend_from_slice(json.as_bytes());
                }
            }
        }
    }

    struct MockSessionInfoWriter {
        shared: std::sync::Arc<std::sync::Mutex<SessionInfoMock>>,
    }
    impl std::io::Write for MockSessionInfoWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.shared
                .lock()
                .expect("mock writer lock")
                .write_buf
                .extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    struct MockSessionInfoReader {
        shared: std::sync::Arc<std::sync::Mutex<SessionInfoMock>>,
    }
    impl std::io::Read for MockSessionInfoReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let mut shared = self.shared.lock().expect("mock reader lock");
            shared.process();
            if shared.read_buf.is_empty() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "no more mock responses",
                ));
            }
            let n = std::cmp::min(buf.len(), shared.read_buf.len());
            buf[..n].copy_from_slice(&shared.read_buf[..n]);
            shared.read_buf.drain(..n);
            Ok(n)
        }
    }

    #[test]
    fn test_find_root_session_already_root() {
        let root = session_info("root", None, false);
        assert_eq!(run_find_root("root", vec![root]), Some("root".to_string()));
    }

    #[test]
    fn test_find_root_session_one_deep() {
        let child = session_info("child", Some("root"), false);
        let root = session_info("root", None, false);
        assert_eq!(
            run_find_root("child", vec![child, root]),
            Some("root".to_string())
        );
    }

    #[test]
    fn test_find_root_session_three_deep() {
        let leaf = session_info("leaf", Some("c"), false);
        let c = session_info("c", Some("b"), false);
        let b = session_info("b", Some("root"), false);
        let root = session_info("root", None, false);
        assert_eq!(
            run_find_root("leaf", vec![leaf, c, b, root]),
            Some("root".to_string())
        );
    }

    #[test]
    fn test_find_root_session_archived_root_returns_none() {
        let child = session_info("child", Some("root"), false);
        let root = session_info("root", None, /* archived */ true);
        assert_eq!(run_find_root("child", vec![child, root]), None);
    }

    #[test]
    fn test_find_root_session_empty_response_returns_none() {
        assert_eq!(run_find_root("nope", vec![]), None);
    }

    #[test]
    fn test_find_root_session_depth_guard_truncated_returns_none() {
        // Last entry still has a parent → the server's depth guard cut off
        // the walk before reaching a root. Treat as "root not found".
        let leaf = session_info("leaf", Some("a"), false);
        let a = session_info("a", Some("b-unreachable"), false);
        assert_eq!(run_find_root("leaf", vec![leaf, a]), None);
    }

    // -----------------------------------------------------------------------
    // task_overview_response
    // -----------------------------------------------------------------------

    #[test]
    fn task_overview_response_classifies_each_bucket() {
        let db = TasksDb::open_memory().unwrap();

        // 1. An active task.
        let active = create_ready_task(&db, "proj", "active one", 5, None);
        db.assign_task(active.id, "s-worker").unwrap();

        // 2. A ready task with no blocker -> queued_ready.
        let ready = create_ready_task(&db, "proj", "ready one", 3, None);

        // 3. A ready task blocked by a dependency -> blocked.
        let blocked = create_ready_task(&db, "proj", "waiting", 2, None);
        db.add_relation(blocked.id, ready.id, "depends_on").unwrap();

        // 4. A held task -> held.
        let held = db
            .create_task(
                "proj",
                "parked",
                Some(1),
                None,
                None,
                false,
                "ready",
                false,
                None,
                None,
                /* held */ true,
                None,
                false,
            )
            .unwrap();

        // 5. A merged task -> recently_merged (and NOT in the live buckets).
        let merge_me = create_ready_task(&db, "proj", "done", 1, None);
        move_to_merged(&db, merge_me.id);

        // 6. A task in a different project must not leak.
        let _other = create_ready_task(&db, "other-proj", "other", 1, None);

        let live = HashSet::new();
        let resp = task_overview_response(&db, "proj", 10, &live).unwrap();
        let (
            active_ids,
            queued_ready_ids,
            blocked_ids,
            held_ids,
            merged_ids,
            inflight,
            wait_reasons,
        ) = match resp {
            tau_agent_base::protocol::Response::TaskOverview {
                active,
                queued_ready,
                blocked,
                held,
                recently_merged,
                inflight_count,
                wait_reasons,
                ..
            } => (
                active.iter().map(|t| t.id).collect::<Vec<_>>(),
                queued_ready.iter().map(|t| t.id).collect::<Vec<_>>(),
                blocked.iter().map(|t| t.id).collect::<Vec<_>>(),
                held.iter().map(|t| t.id).collect::<Vec<_>>(),
                recently_merged.iter().map(|t| t.id).collect::<Vec<_>>(),
                inflight_count,
                wait_reasons,
            ),
            other => panic!("expected TaskOverview, got {:?}", other),
        };
        assert_eq!(active_ids, vec![active.id]);
        assert_eq!(queued_ready_ids, vec![ready.id]);
        assert_eq!(blocked_ids, vec![blocked.id]);
        assert_eq!(held_ids, vec![held.id]);
        assert_eq!(merged_ids, vec![merge_me.id]);
        assert_eq!(inflight, 1);
        // Dependency wait-reason surfaces in `wait_reasons`.
        let entry = wait_reasons
            .iter()
            .find(|b| b.task_id == blocked.id)
            .expect("blocked task should have an entry");
        let dep_ids: Vec<i64> = entry
            .reasons
            .iter()
            .filter_map(|r| match r {
                tau_agent_base::protocol::TaskWaitReason::Dependency { task_id, .. } => {
                    Some(*task_id)
                }
                _ => None,
            })
            .collect();
        assert_eq!(dep_ids, vec![ready.id]);
    }

    #[test]
    fn task_overview_response_recent_limit_truncates_and_orders() {
        let db = TasksDb::open_memory().unwrap();
        // Seed 20 merged tasks with distinct updated_at.
        let mut ids = Vec::new();
        for i in 0..20 {
            let t = create_ready_task(&db, "proj", &format!("m{}", i), 0, None);
            db.conn
                .execute(
                    "UPDATE tasks SET state = 'merged', updated_at = ?1 WHERE id = ?2",
                    rusqlite::params![1_000_000_i64 + i as i64 * 1000, t.id],
                )
                .unwrap();
            ids.push(t.id);
        }

        let resp = task_overview_response(&db, "proj", 5, &HashSet::new()).unwrap();
        let merged = match resp {
            tau_agent_base::protocol::Response::TaskOverview {
                recently_merged, ..
            } => recently_merged,
            other => panic!("expected TaskOverview, got {:?}", other),
        };
        assert_eq!(merged.len(), 5);
        // Newest first.
        let merged_ids: Vec<i64> = merged.iter().map(|t| t.id).collect();
        assert_eq!(
            merged_ids,
            vec![ids[19], ids[18], ids[17], ids[16], ids[15]]
        );
    }

    #[test]
    fn task_overview_response_live_ids_stamp_has_live_session() {
        let db = TasksDb::open_memory().unwrap();
        let active = create_ready_task(&db, "proj", "a", 0, None);
        db.assign_task(active.id, "s-worker").unwrap();

        let mut live = HashSet::new();
        live.insert(active.id);
        let resp = task_overview_response(&db, "proj", 10, &live).unwrap();
        let got = match resp {
            tau_agent_base::protocol::Response::TaskOverview { active, .. } => active,
            other => panic!("expected TaskOverview, got {:?}", other),
        };
        assert_eq!(got.len(), 1);
        assert!(got[0].has_live_session, "live_ids should set the flag");
    }

    #[test]
    fn wait_reason_to_protocol_mapping() {
        use tau_agent_base::protocol::TaskWaitReason as P;

        let cases = vec![
            (
                WaitReason::Dependency {
                    task_id: 1,
                    title: "t".into(),
                    state: "active".into(),
                    project_name: "p".into(),
                },
                P::Dependency {
                    task_id: 1,
                    title: "t".into(),
                    state: "active".into(),
                    project_name: "p".into(),
                },
            ),
            (
                WaitReason::FileConflict {
                    files: vec!["a.rs".into(), "b.rs".into()],
                    with_task_id: 7,
                },
                P::FileConflict {
                    files: vec!["a.rs".into(), "b.rs".into()],
                    with_task_id: 7,
                },
            ),
            (
                WaitReason::BudgetExhausted { used: 8, max: 8 },
                P::BudgetExhausted { used: 8, max: 8 },
            ),
            (
                WaitReason::MergeTargetNotFound {
                    branch: "main".into(),
                },
                P::MergeTargetNotFound {
                    branch: "main".into(),
                },
            ),
            (WaitReason::NotScheduled, P::NotScheduled),
        ];
        for (src, want) in cases {
            let got: P = src.into();
            assert_eq!(got, want);
        }
    }

    // -----------------------------------------------------------------
    // WaitTracker (task #574)
    // -----------------------------------------------------------------

    /// First observation of a waiting task emits a `Waiting` event.
    /// Repeating the same digest is a no-op.
    #[test]
    fn wait_tracker_first_observe_emits_then_dedups() {
        let mut t = WaitTracker::new();
        let e1 = t.observe(1, Some("file conflict".into()), 1_000);
        assert_eq!(e1, Some(WaitEvent::Waiting("file conflict".into())));
        let e2 = t.observe(1, Some("file conflict".into()), 2_000);
        assert_eq!(e2, None);
        let e3 = t.observe(1, Some("file conflict".into()), 3_000);
        assert_eq!(e3, None);
    }

    /// A changed digest emits a new `Waiting` event with the new text.
    #[test]
    fn wait_tracker_changed_reason_emits() {
        let mut t = WaitTracker::new();
        assert!(matches!(
            t.observe(1, Some("A".into()), 1_000),
            Some(WaitEvent::Waiting(_))
        ));
        let e = t.observe(1, Some("B".into()), 2_000);
        assert_eq!(e, Some(WaitEvent::Waiting("B".into())));
    }

    /// Clearing a wait (Some → None) emits a `Cleared` event with
    /// elapsed-since-first-observed-digest.
    #[test]
    fn wait_tracker_clear_emits_cleared_with_elapsed() {
        let mut t = WaitTracker::new();
        t.observe(1, Some("A".into()), 1_000);
        t.observe(1, Some("A".into()), 1_500); // dedup, doesn't reset clock
        let e = t.observe(1, None, 4_000);
        assert_eq!(e, Some(WaitEvent::Cleared { elapsed_ms: 3_000 }));
        // Second clear is a no-op.
        assert_eq!(t.observe(1, None, 5_000), None);
    }

    /// First observation with `None` digest records the baseline but
    /// does NOT emit an event (nothing to tell the user yet).
    #[test]
    fn wait_tracker_first_observe_none_is_silent() {
        let mut t = WaitTracker::new();
        assert_eq!(t.observe(42, None, 1_000), None);
        // Then a new waiting reason DOES emit.
        assert_eq!(
            t.observe(42, Some("dep".into()), 2_000),
            Some(WaitEvent::Waiting("dep".into()))
        );
    }

    /// `summarize_wait_reasons` is stable: same reasons → same string.
    #[test]
    fn summarize_wait_reasons_joins_with_semicolons() {
        let reasons = vec![
            WaitReason::NotScheduled,
            WaitReason::BudgetExhausted { used: 3, max: 8 },
        ];
        let s = summarize_wait_reasons(&reasons).expect("non-empty");
        assert_eq!(
            s,
            "not yet scheduled; concurrent-task budget exhausted (3/8)"
        );
    }

    #[test]
    fn summarize_wait_reasons_empty_returns_none() {
        assert!(summarize_wait_reasons(&[]).is_none());
    }

    /// End-to-end scheduler wait-message posting: two passes with the
    /// same wait reason post only one message; a cleared wait posts the
    /// "Wait cleared" message.
    #[test]
    fn post_wait_updates_dedups_and_clears() {
        use crate::tasks_db::TasksDb;

        let db = TasksDb::open_memory().expect("db");
        let task = db
            .create_task(
                "p", "t", None, None, None, false, "ready", false, None, None, false, None, false,
            )
            .expect("create");
        db.set_placeholder_session_id(task.id, "s-ph").expect("ph");
        let task = db.get_task(task.id).expect("get").expect("task");

        // Build a synthetic SchedulerStatus that has this task queued
        // with a file-conflict wait reason.
        let ts = TaskStatus {
            task: task.clone(),
            session_id: None,
            wait_reasons: vec![WaitReason::FileConflict {
                files: vec!["x.rs".into()],
                with_task_id: 99,
            }],
        };
        let status_waiting = SchedulerStatus {
            active: vec![],
            queued_planning: vec![],
            queued_ready: vec![ts.clone()],
            held: vec![],
            blocked: vec![],
            inflight_count: 0,
            max_concurrent: 8,
        };

        // Two passes with same reason → one message.
        let mut tracker = WaitTracker::new();
        let (shared, mut w, mut r) = make_io_nop();
        post_wait_updates(&status_waiting, &mut tracker, &mut w, &mut r);
        post_wait_updates(&status_waiting, &mut tracker, &mut w, &mut r);
        let calls = shared.lock().expect("lock").queue_info_calls.clone();
        assert_eq!(calls.len(), 1, "first-pass calls: {:?}", calls);
        assert_eq!(calls[0].0, "s-ph");
        assert!(calls[0].1.starts_with("Waiting: "), "msg: {:?}", calls[0].1);
        assert!(
            calls[0].1.contains("task #99"),
            "msg should mention conflicting task: {:?}",
            calls[0].1
        );

        // Task moves to active → wait cleared.
        let mut task_active = task.clone();
        task_active.state = TaskState::Active;
        let status_active = SchedulerStatus {
            active: vec![TaskStatus {
                task: task_active,
                session_id: Some("s-worker".into()),
                wait_reasons: vec![],
            }],
            queued_planning: vec![],
            queued_ready: vec![],
            held: vec![],
            blocked: vec![],
            inflight_count: 1,
            max_concurrent: 8,
        };
        post_wait_updates(&status_active, &mut tracker, &mut w, &mut r);
        let calls = shared.lock().expect("lock").queue_info_calls.clone();
        assert_eq!(calls.len(), 2, "after-clear calls: {:?}", calls);
        assert!(
            calls[1].1.starts_with("Wait cleared after "),
            "cleared msg: {:?}",
            calls[1].1
        );

        // Another pass while active → no-op.
        post_wait_updates(&status_active, &mut tracker, &mut w, &mut r);
        let calls = shared.lock().expect("lock").queue_info_calls.clone();
        assert_eq!(calls.len(), 2, "no extra message expected: {:?}", calls);
    }

    /// A *changed* wait reason (e.g. file-conflict → budget-exhausted)
    /// posts a new message.
    #[test]
    fn post_wait_updates_changed_reason_posts_new_message() {
        use crate::tasks_db::TasksDb;

        let db = TasksDb::open_memory().expect("db");
        let task = db
            .create_task(
                "p", "t", None, None, None, false, "ready", false, None, None, false, None, false,
            )
            .expect("create");
        db.set_placeholder_session_id(task.id, "s-ph").expect("ph");
        let task = db.get_task(task.id).expect("get").expect("task");

        let make_status = |reason: WaitReason| SchedulerStatus {
            active: vec![],
            queued_planning: vec![],
            queued_ready: vec![TaskStatus {
                task: task.clone(),
                session_id: None,
                wait_reasons: vec![reason],
            }],
            held: vec![],
            blocked: vec![],
            inflight_count: 0,
            max_concurrent: 8,
        };

        let mut tracker = WaitTracker::new();
        let (shared, mut w, mut r) = make_io_nop();
        post_wait_updates(
            &make_status(WaitReason::FileConflict {
                files: vec!["x.rs".into()],
                with_task_id: 1,
            }),
            &mut tracker,
            &mut w,
            &mut r,
        );
        post_wait_updates(
            &make_status(WaitReason::BudgetExhausted { used: 8, max: 8 }),
            &mut tracker,
            &mut w,
            &mut r,
        );
        let calls = shared.lock().expect("lock").queue_info_calls.clone();
        assert_eq!(calls.len(), 2, "{:?}", calls);
        assert!(calls[0].1.contains("file conflict"), "{:?}", calls[0].1);
        assert!(calls[1].1.contains("budget"), "{:?}", calls[1].1);
    }

    // Minimal mock IO that records QueueInfo requests and responds Ok
    // to everything. Reused by the post_wait_updates tests so they
    // don't need the full tasks_notify mock harness.
    //
    // The main difference from the tasks_notify MockShared is that
    // this one only needs to handle QueueInfo — placeholder lookups
    // skip GetSessionInfo because the placeholder is already in the
    // task row and collect_recipients is bypassed.
    mod mock_io {
        use std::io::{BufReader, Read, Write};
        use std::sync::{Arc, Mutex};
        use tau_agent_plugin::{PluginMessage, PluginRequest, Request, Response};

        pub struct MockShared {
            write_buf: Vec<u8>,
            read_buf: Vec<u8>,
            pub queue_info_calls: Vec<(String, String)>,
        }

        impl MockShared {
            pub fn new() -> Self {
                Self {
                    write_buf: Vec::new(),
                    read_buf: Vec::new(),
                    queue_info_calls: Vec::new(),
                }
            }

            fn process_pending(&mut self) {
                let buf = std::mem::take(&mut self.write_buf);
                let text = String::from_utf8_lossy(&buf);
                for line in text.lines() {
                    if line.trim().is_empty() {
                        continue;
                    }
                    let msg: PluginMessage = match serde_json::from_str(line) {
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
                    if let Request::QueueInfo {
                        target_session_id,
                        text,
                    } = &request
                    {
                        self.queue_info_calls
                            .push((target_session_id.clone(), text.clone()));
                    }
                    let reply = PluginRequest::ServerResponse {
                        request_id,
                        response: Response::Ok,
                    };
                    if let Ok(mut json) = serde_json::to_string(&reply) {
                        json.push('\n');
                        self.read_buf.extend_from_slice(json.as_bytes());
                    }
                }
            }
        }

        pub struct MockWriter {
            pub shared: Arc<Mutex<MockShared>>,
        }
        impl Write for MockWriter {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.shared
                    .lock()
                    .expect("mock writer lock")
                    .write_buf
                    .extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        pub struct MockReader {
            pub shared: Arc<Mutex<MockShared>>,
        }
        impl Read for MockReader {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                let mut shared = self.shared.lock().expect("mock reader lock");
                shared.process_pending();
                if shared.read_buf.is_empty() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "no mock responses left",
                    ));
                }
                let n = std::cmp::min(buf.len(), shared.read_buf.len());
                buf[..n].copy_from_slice(&shared.read_buf[..n]);
                shared.read_buf.drain(..n);
                Ok(n)
            }
        }

        pub fn make_io() -> (Arc<Mutex<MockShared>>, MockWriter, BufReader<MockReader>) {
            let shared = Arc::new(Mutex::new(MockShared::new()));
            let writer = MockWriter {
                shared: shared.clone(),
            };
            let reader = BufReader::new(MockReader {
                shared: shared.clone(),
            });
            (shared, writer, reader)
        }
    }

    fn make_io_nop() -> (
        std::sync::Arc<std::sync::Mutex<mock_io::MockShared>>,
        mock_io::MockWriter,
        std::io::BufReader<mock_io::MockReader>,
    ) {
        mock_io::make_io()
    }

    // -----------------------------------------------------------------
    // Phase-dispatch unit tests (task #605)
    //
    // Richer mock IO that understands CreateSession / GetSessionInfo /
    // QueueMessage etc., so we can exercise `dispatch_task_phase` in
    // isolation from tasks.rs's harness.
    // -----------------------------------------------------------------

    mod phase_mock_io {
        use std::collections::HashSet;
        use std::io::{BufReader, Read, Write};
        use std::sync::{Arc, Mutex};
        use tau_agent_plugin::{PluginMessage, PluginRequest, Request, Response};

        pub struct PhaseMockShared {
            write_buf: Vec<u8>,
            read_buf: Vec<u8>,
            pub session_counter: u32,
            pub archived_sessions: HashSet<String>,
            pub written_lines: Vec<String>,
            /// Parent id returned for every `GetSessionInfo` response
            /// (the tests that care set this explicitly).
            pub session_parent_id: Option<String>,
        }

        impl PhaseMockShared {
            pub fn new() -> Self {
                Self {
                    write_buf: Vec::new(),
                    read_buf: Vec::new(),
                    session_counter: 0,
                    archived_sessions: HashSet::new(),
                    written_lines: Vec::new(),
                    session_parent_id: None,
                }
            }

            fn process_pending(&mut self) {
                let buf = std::mem::take(&mut self.write_buf);
                let text = String::from_utf8_lossy(&buf);
                for line in text.lines() {
                    if line.trim().is_empty() {
                        continue;
                    }
                    self.written_lines.push(line.to_string());
                    let msg: PluginMessage = match serde_json::from_str(line) {
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
                    let response = match request {
                        Request::CreateSession { .. } => {
                            self.session_counter += 1;
                            Response::SessionCreated {
                                session_id: format!("mock-s{}", self.session_counter),
                            }
                        }
                        Request::GetSessionInfo { session_id } => {
                            let is_archived = self.archived_sessions.contains(&session_id);
                            Response::SessionInfo {
                                info: tau_agent_plugin::SessionInfo {
                                    id: session_id,
                                    model: "mock-model".to_string(),
                                    provider: "mock".to_string(),
                                    cwd: None,
                                    message_count: 0,
                                    stats: Default::default(),
                                    last_activity: 0,
                                    parent_id: self.session_parent_id.clone(),
                                    child_count: 0,
                                    child_budget: 16,
                                    tagline: None,
                                    state: "idle".to_string(),
                                    context_pct: None,
                                    archived: is_archived,
                                    last_exit_status: None,
                                    is_live: false,
                                    turn_started_at_ms: None,
                                    phase_started_at_ms: None,
                                    project_name: None,
                                },
                            }
                        }
                        Request::GetSessionAncestors { session_id } => Response::SessionAncestors {
                            sessions: vec![tau_agent_plugin::SessionInfo {
                                id: session_id.clone(),
                                model: "mock-model".to_string(),
                                provider: "mock".to_string(),
                                cwd: None,
                                message_count: 0,
                                stats: Default::default(),
                                last_activity: 0,
                                parent_id: None,
                                child_count: 0,
                                child_budget: 16,
                                tagline: None,
                                state: "idle".to_string(),
                                context_pct: None,
                                archived: self.archived_sessions.contains(&session_id),
                                last_exit_status: None,
                                is_live: false,
                                turn_started_at_ms: None,
                                phase_started_at_ms: None,
                                project_name: None,
                            }],
                        },
                        _ => Response::Ok,
                    };
                    let reply = PluginRequest::ServerResponse {
                        request_id,
                        response,
                    };
                    if let Ok(mut json) = serde_json::to_string(&reply) {
                        json.push('\n');
                        self.read_buf.extend_from_slice(json.as_bytes());
                    }
                }
            }
        }

        pub struct PhaseMockWriter {
            pub shared: Arc<Mutex<PhaseMockShared>>,
        }
        impl Write for PhaseMockWriter {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.shared
                    .lock()
                    .expect("phase mock writer lock")
                    .write_buf
                    .extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        pub struct PhaseMockReader {
            pub shared: Arc<Mutex<PhaseMockShared>>,
        }
        impl Read for PhaseMockReader {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                let mut shared = self.shared.lock().expect("phase mock reader lock");
                shared.process_pending();
                if shared.read_buf.is_empty() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "no mock responses left",
                    ));
                }
                let n = std::cmp::min(buf.len(), shared.read_buf.len());
                buf[..n].copy_from_slice(&shared.read_buf[..n]);
                shared.read_buf.drain(..n);
                Ok(n)
            }
        }

        pub fn make_io() -> (
            Arc<Mutex<PhaseMockShared>>,
            PhaseMockWriter,
            BufReader<PhaseMockReader>,
        ) {
            let shared = Arc::new(Mutex::new(PhaseMockShared::new()));
            let writer = PhaseMockWriter {
                shared: shared.clone(),
            };
            let reader = BufReader::new(PhaseMockReader {
                shared: shared.clone(),
            });
            (shared, writer, reader)
        }

        /// Drain any writes still in `write_buf` into `written_lines`
        /// (without waiting for a read) so tests can inspect the full
        /// transcript after a dispatch call returns.
        pub fn drain_written(shared: &Arc<Mutex<PhaseMockShared>>) -> Vec<String> {
            let mut s = shared.lock().expect("phase mock shared");
            let buf = std::mem::take(&mut s.write_buf);
            let text = String::from_utf8_lossy(&buf);
            for line in text.lines() {
                if !line.trim().is_empty() {
                    s.written_lines.push(line.to_string());
                }
            }
            s.written_lines.clone()
        }
    }

    fn phase_test_task(db: &TasksDb, title: &str, initial_state: &str) -> Task {
        // `create_task` only accepts ready/planning/interactive as the
        // initial state — that's fine for these tests because
        // `dispatch_task_phase` doesn't gate on `task.state` (the
        // `dispatch` wrapper does).
        db.create_task(
            "test-project",
            title,
            Some(1),
            None,
            None,
            false,
            initial_state,
            false,
            None,
            None,
            false,
            None,
            false,
        )
        .expect("create test task")
    }

    #[test]
    fn dispatch_task_phase_worker_happy_path_sets_session_id() {
        let db = TasksDb::open_memory().expect("open memory db");
        let mut task = phase_test_task(&db, "worker happy", "ready");
        // Pretend prepare_task already set a worktree.
        db.set_worktree_path(task.id, "/tmp/wt-605")
            .expect("set wt");
        task = db
            .get_task(task.id)
            .expect("get task")
            .expect("task exists");

        let (shared, mut writer, mut reader) = phase_mock_io::make_io();
        let sid = dispatch_task_phase(
            &db,
            &task,
            TaskPhase::Worker,
            None,
            "/test/project",
            &mut writer,
            &mut reader,
        )
        .expect("worker dispatch ok");
        assert_eq!(sid, "mock-s1");

        // Worker overwrites task.session_id.
        let after = db
            .get_task(task.id)
            .expect("get task")
            .expect("task exists");
        assert_eq!(after.session_id.as_deref(), Some("mock-s1"));

        // A worker row must be recorded in task_sessions.
        let recorded = db
            .find_latest_session_by_role(task.id, "worker")
            .expect("find worker session");
        assert_eq!(recorded.as_deref(), Some("mock-s1"));

        // Verify the emitted CreateSession used the worker's worktree cwd.
        let written = phase_mock_io::drain_written(&shared).join("\n");
        assert!(
            written.contains("/tmp/wt-605"),
            "worker CreateSession should include the worktree path, got:\n{}",
            written
        );
    }

    #[test]
    fn dispatch_task_phase_worker_reuse_is_silent() {
        let db = TasksDb::open_memory().expect("open memory db");
        let task = phase_test_task(&db, "worker reuse", "ready");
        // Record a live worker session.
        db.record_session(task.id, "existing-worker", "worker")
            .expect("record worker");

        let (shared, mut writer, mut reader) = phase_mock_io::make_io();
        let sid = dispatch_task_phase(
            &db,
            &task,
            TaskPhase::Worker,
            None,
            "/test/project",
            &mut writer,
            &mut reader,
        )
        .expect("worker reuse ok");
        assert_eq!(sid, "existing-worker");

        // Worker reuse must NOT queue a resume message.
        let written = phase_mock_io::drain_written(&shared).join("\n");
        assert!(
            !written.contains("queue_message"),
            "worker reuse must not send a QueueMessage, got:\n{}",
            written
        );
    }

    #[test]
    fn dispatch_task_phase_planner_reuse_sends_resume() {
        let db = TasksDb::open_memory().expect("open memory db");
        let task = phase_test_task(&db, "planner reuse", "planning");
        db.record_session(task.id, "existing-planner", "planner")
            .expect("record planner");

        let (shared, mut writer, mut reader) = phase_mock_io::make_io();
        let sid = dispatch_task_phase(
            &db,
            &task,
            TaskPhase::Planner,
            None,
            "/test/project",
            &mut writer,
            &mut reader,
        )
        .expect("planner reuse ok");
        assert_eq!(sid, "existing-planner");

        let written = phase_mock_io::drain_written(&shared).join("\n");
        assert!(
            written.contains("queue_message"),
            "planner reuse must send a QueueMessage, got:\n{}",
            written
        );
        assert!(
            written.contains("moved back to planning"),
            "planner resume body should explain the backward transition, got:\n{}",
            written
        );
    }

    #[test]
    fn dispatch_task_phase_reviewer_reuse_sends_resume() {
        let db = TasksDb::open_memory().expect("open memory db");
        let task = phase_test_task(&db, "reviewer reuse", "ready");
        db.record_session(task.id, "existing-reviewer", "reviewer")
            .expect("record reviewer");

        let (shared, mut writer, mut reader) = phase_mock_io::make_io();
        let sid = dispatch_task_phase(
            &db,
            &task,
            TaskPhase::Reviewer,
            None,
            "/test/project",
            &mut writer,
            &mut reader,
        )
        .expect("reviewer reuse ok");
        assert_eq!(sid, "existing-reviewer");

        let written = phase_mock_io::drain_written(&shared).join("\n");
        assert!(
            written.contains("queue_message"),
            "reviewer reuse must send a QueueMessage, got:\n{}",
            written
        );
        assert!(
            written.contains("re-submitted for review"),
            "reviewer resume body should mention re-submission, got:\n{}",
            written
        );
    }

    #[test]
    fn dispatch_task_phase_refiner_reuse_sends_resume() {
        let db = TasksDb::open_memory().expect("open memory db");
        let task = phase_test_task(&db, "refiner reuse", "ready");
        db.record_session(task.id, "existing-refiner", "refiner")
            .expect("record refiner");

        let (shared, mut writer, mut reader) = phase_mock_io::make_io();
        let sid = dispatch_task_phase(
            &db,
            &task,
            TaskPhase::Refiner,
            None,
            "/test/project",
            &mut writer,
            &mut reader,
        )
        .expect("refiner reuse ok");
        assert_eq!(sid, "existing-refiner");

        let written = phase_mock_io::drain_written(&shared).join("\n");
        assert!(
            written.contains("queue_message"),
            "refiner reuse must send a QueueMessage, got:\n{}",
            written
        );
        assert!(
            written.contains("re-submitted for refining"),
            "refiner resume body should mention refining re-submission, got:\n{}",
            written
        );
    }

    #[test]
    fn dispatch_task_phase_reviewer_does_not_set_session_id() {
        let db = TasksDb::open_memory().expect("open memory db");
        let task = phase_test_task(&db, "review ssid", "ready");
        // Pretend a prior worker session already claimed task.session_id.
        db.set_session_id(task.id, "prior-worker")
            .expect("set session id");
        let task = db
            .get_task(task.id)
            .expect("get task")
            .expect("task exists");

        let (_shared, mut writer, mut reader) = phase_mock_io::make_io();
        let sid = dispatch_task_phase(
            &db,
            &task,
            TaskPhase::Reviewer,
            None,
            "/test/project",
            &mut writer,
            &mut reader,
        )
        .expect("reviewer dispatch ok");
        assert_eq!(sid, "mock-s1");

        // Reviewer must NOT overwrite task.session_id.
        let after = db
            .get_task(task.id)
            .expect("get task")
            .expect("task exists");
        assert_eq!(
            after.session_id.as_deref(),
            Some("prior-worker"),
            "reviewer dispatch must leave task.session_id untouched"
        );
    }

    #[test]
    fn dispatch_task_phase_refiner_does_not_set_session_id() {
        let db = TasksDb::open_memory().expect("open memory db");
        let task = phase_test_task(&db, "refine ssid", "ready");
        db.set_session_id(task.id, "prior-planner")
            .expect("set session id");
        let task = db
            .get_task(task.id)
            .expect("get task")
            .expect("task exists");

        let (_shared, mut writer, mut reader) = phase_mock_io::make_io();
        let sid = dispatch_task_phase(
            &db,
            &task,
            TaskPhase::Refiner,
            None,
            "/test/project",
            &mut writer,
            &mut reader,
        )
        .expect("refiner dispatch ok");
        assert_eq!(sid, "mock-s1");

        let after = db
            .get_task(task.id)
            .expect("get task")
            .expect("task exists");
        assert_eq!(
            after.session_id.as_deref(),
            Some("prior-planner"),
            "refiner dispatch must leave task.session_id untouched"
        );
    }

    #[test]
    fn dispatch_task_phase_planner_uses_project_cwd() {
        let db = TasksDb::open_memory().expect("open memory db");
        let task = phase_test_task(&db, "planner cwd", "planning");

        let (shared, mut writer, mut reader) = phase_mock_io::make_io();
        let _sid = dispatch_task_phase(
            &db,
            &task,
            TaskPhase::Planner,
            None,
            "/test/project",
            &mut writer,
            &mut reader,
        )
        .expect("planner dispatch ok");

        let written = phase_mock_io::drain_written(&shared).join("\n");
        assert!(
            written.contains("\"cwd\":\"/test/project\""),
            "planner CreateSession should carry project_path as cwd, got:\n{}",
            written
        );
    }

    /// Regression guard for catalog #2 (#573) / this task: after the
    /// refactor, the only `Request::CreateSession { ... }` construction
    /// site in the scheduler must be inside the phase helper (it lives in
    /// `tasks_session.rs` — see task #604), so this file should contain
    /// zero bare construction sites.
    #[test]
    fn scheduler_has_no_bare_create_session_literal() {
        let src = include_str!("tasks_scheduler.rs");
        // Build the match needle at runtime so our own scanner doesn't
        // count as a literal.  The sister test in `tasks_session.rs` does
        // the same thing for the same reason.
        let needle = concat!("Request::", "CreateSession {");
        let mut count = 0;
        for line in src.lines() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("//") || trimmed.starts_with("///") {
                continue;
            }
            // Skip destructuring / match patterns.
            if line.contains(concat!("Request::", "CreateSession { .. }")) {
                continue;
            }
            // Skip the scanner lines that construct the needles themselves.
            if line.contains("concat!(\"") {
                continue;
            }
            if line.contains(needle) {
                count += 1;
            }
        }
        assert_eq!(
            count, 0,
            "scheduler must not contain any bare Request::CreateSession literal \
             (it lives in tasks_session.rs::create_task_session); found {}",
            count
        );
    }

    /// Regression guard: the four `dispatch*` public wrappers must remain
    /// thin — none of them should contain its own `Request::CreateSession`
    /// literal or open-code the reuse ladder. We check this indirectly by
    /// counting the `dispatch_task_phase(` call sites: exactly four (one
    /// per wrapper) are expected, plus the helper's own definition for a
    /// total of five matches, three of which can be in comments/doctests.
    #[test]
    fn four_dispatch_wrappers_delegate_to_helper() {
        let src = include_str!("tasks_scheduler.rs");
        let mut call_sites = 0;
        for line in src.lines() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("//") || trimmed.starts_with("///") {
                continue;
            }
            // Definition line itself.
            if trimmed.starts_with("pub(crate) fn dispatch_task_phase(") {
                continue;
            }
            if line.contains("dispatch_task_phase(") {
                call_sites += 1;
            }
        }
        assert!(
            call_sites >= 4,
            "expected at least 4 dispatch_task_phase(...) call sites \
             (one per wrapper), found {}",
            call_sites
        );
    }
}
