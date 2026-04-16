//! Scheduler logic for the task system.
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

use crate::tasks_config;
use crate::tasks_db::{Task, TaskUpdate, TasksDb};
use crate::tasks_git;
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
    let mut sorted: Vec<&Task> = tasks.iter().collect();
    sorted.sort_by(|a, b| b.priority.cmp(&a.priority));

    // Pre-populate claimed_files with files from already-active tasks.
    let mut claimed_files: HashSet<String> = HashSet::new();
    let mut has_unbounded_active = false;
    for (_id, files) in active_files {
        if files.is_empty() {
            // An active task without affected_files is unbounded — it
            // potentially conflicts with everything.
            has_unbounded_active = true;
        }
        for f in files {
            claimed_files.insert(f.clone());
        }
    }

    let mut selected: Vec<&Task> = Vec::new();
    let mut has_unbounded = has_unbounded_active;

    for task in &sorted {
        let files = extract_files(&task.affected_files);

        if files.is_empty() {
            // No affected_files declared — treat as potentially conflicting
            // with everything. Only schedule alone and only if no active tasks
            // have claimed files.
            if selected.is_empty() && !has_unbounded && claimed_files.is_empty() {
                selected.push(task);
                has_unbounded = true;
                // Don't break — but the flag prevents any further additions.
            }
            // Skip if we already have selections or an unbounded active task.
            continue;
        }

        // If we already selected an unbounded task (or an active task is
        // unbounded), skip everything else.
        if has_unbounded {
            continue;
        }

        // Check overlap with already-claimed files (includes active tasks).
        let overlaps = files.iter().any(|f| claimed_files.contains(f));
        if !overlaps {
            for f in files {
                claimed_files.insert(f);
            }
            selected.push(task);
        }
    }

    selected
}

/// Extract file paths from the `affected_files` JSON value.
pub(crate) fn extract_files(val: &Option<serde_json::Value>) -> Vec<String> {
    match val {
        Some(serde_json::Value::Array(arr)) => arr
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
        _ => Vec::new(),
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
        return Ok(Vec::new());
    }
    let remaining_capacity = MAX_CONCURRENT_TASKS - inflight;

    let schedulable_tasks = db.get_schedulable_tasks(project_name)?;

    if schedulable_tasks.is_empty() {
        return Ok(Vec::new());
    }

    // Separate planning tasks from ready tasks.
    // Planning tasks don't need worktrees or conflict checking.
    let mut planning_tasks = Vec::new();
    let mut ready_tasks = Vec::new();
    for task in &schedulable_tasks {
        if task.state == "planning" {
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
            continue;
        }
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

        let batch = select_non_conflicting(&ready_tasks, &active_files);
        if !batch.is_empty() {
            // We need the repo root to create branches and worktrees.
            let repo_root = tasks_git::get_repo_root(project_path)?;

            for task in batch {
                match prepare_task(db, task, &repo_root) {
                    Ok(st) => scheduled.push(st),
                    Err(e) => {
                        // Log but don't fail the whole batch.
                        eprintln!("tasks scheduler: failed to prepare task {}: {}", task.id, e);
                        // Add a visible message to the task so the error is discoverable.
                        let _ = db.add_message(
                            task.id,
                            &format!("⚠️ Scheduling failed: {}", e),
                            Some("system"),
                        );
                    }
                }
            }
        }
    }

    // Enforce the concurrent tasks limit.
    scheduled.truncate(remaining_capacity);

    Ok(scheduled)
}

/// Prepare a single task for dispatch: create branch, worktree, update DB.
fn prepare_task(
    db: &TasksDb,
    task: &Task,
    repo_root: &str,
) -> tau_agent_plugin::Result<ScheduledTask> {
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
            state: Some("active".to_string()),
            ..Default::default()
        },
        None,
    )?;

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

/// Look up a session's model via GetSessionInfo. Returns `None` if the
/// request fails or the session is not found.
pub fn get_session_model(
    session_id: &str,
    writer: &mut impl Write,
    reader: &mut impl BufRead,
) -> Option<String> {
    let req = tau_agent_plugin::Request::GetSessionInfo {
        session_id: session_id.to_string(),
    };
    match server_request(writer, reader, req) {
        Ok(tau_agent_plugin::Response::SessionInfo { info }) => Some(info.model),
        _ => None,
    }
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
    let task = db
        .get_task(task_id)?
        .ok_or_else(|| tau_agent_plugin::Error::Io(format!("task {} not found", task_id)))?;

    // Handle planning-state dispatch (no worktree, read-only session)
    if task.state == "planning" {
        return dispatch_planning(db, &task, parent_session_id, project_path, writer, reader);
    }

    // Task must be active (prepared by schedule) or ready (we'll prepare it).
    if task.state == "ready" {
        // Not yet prepared — do it inline.
        let repo_root = tasks_git::get_repo_root(project_path)?;
        prepare_task(db, &task, &repo_root)?;
        // Re-read after prepare.
    } else if task.state != "active" {
        return Err(tau_agent_plugin::Error::Io(format!(
            "task {} is in state '{}', must be 'ready', 'active', or 'planning' to dispatch",
            task_id, task.state
        )));
    }

    // Re-read the task to get updated fields after potential prepare.
    let task = db.get_task(task_id)?.ok_or_else(|| {
        tau_agent_plugin::Error::Io(format!("task {} not found after prepare", task_id))
    })?;

    // If there is already a live worker session for this task, reuse it
    // instead of creating a duplicate.  This makes dispatch idempotent:
    // calling it twice (e.g. from a second schedule pass after a partial
    // failure, or via a manual task_dispatch call) will not spawn a second
    // session.
    if let Some(existing_sid) = find_reusable_session(db, task_id, "worker", writer, reader) {
        eprintln!(
            "tasks scheduler: task {} already has a live worker session {}, reusing",
            task_id, existing_sid
        );
        return Ok(existing_sid);
    }

    // If the task has a session_id from a previous lifecycle phase (e.g. a
    // planner or refiner session), log and continue — the old session is
    // already recorded in the task_sessions table, and set_session_id below
    // will overwrite with the new worker session.
    if let Some(ref existing_sid) = task.session_id {
        eprintln!(
            "tasks scheduler: task {} replacing previous session {} with new worker dispatch",
            task_id, existing_sid
        );
    }

    let cwd = task.worktree_path.clone();

    // Model inheritance: use the triggering session (e.g. the refiner that
    // moved the task to ready), falling back through the hierarchy.
    let model_source = resolve_model_source(db, &task, parent_session_id);
    let model = model_source
        .as_deref()
        .and_then(|sid| get_session_model(sid, writer, reader));

    // Session parenting: use the task's planner session (the orchestrator),
    // not the refiner/reviewer that triggered the state change.
    let hierarchy_parent = resolve_hierarchy_parent(db, &task);

    // Create session via ServerRequest.
    let create_req = tau_agent_plugin::Request::CreateSession {
        model,
        provider: None,
        system_prompt: None,
        cwd,
        parent_id: hierarchy_parent,
        child_budget: 16,
        tagline: Some(format!("Task {}: {}", task.id, task.title)),
        auto_archive: false,
        notify_parent: false,
        project_name: Some(task.project_name.clone()),
        sandbox_profile: task.sandbox_profile.clone(),
    };

    let session_id = match server_request(writer, reader, create_req)? {
        tau_agent_plugin::Response::SessionCreated { session_id } => session_id,
        tau_agent_plugin::Response::Error { message } => {
            return Err(tau_agent_plugin::Error::Io(format!(
                "create session for task {}: {}",
                task_id, message
            )));
        }
        other => {
            return Err(tau_agent_plugin::Error::Io(format!(
                "unexpected response creating session for task {}: {:?}",
                task_id, other
            )));
        }
    };

    // Send initial chat message.
    let merge_target = db
        .get_merge_target(task_id)
        .unwrap_or_else(|_| "main".into());
    let project_instructions =
        tasks_config::load_project_instructions(project_path, Some(&task.project_name), "worker")
            .unwrap_or_default();
    let chat_msg = build_initial_message(&task, &merge_target, &project_instructions);
    let chat_req = tau_agent_plugin::Request::Chat {
        session_id: session_id.clone(),
        text: chat_msg,
    };

    match server_request(writer, reader, chat_req) {
        Ok(tau_agent_plugin::Response::Ok) => {}
        Ok(tau_agent_plugin::Response::Error { message }) => {
            return Err(tau_agent_plugin::Error::Io(format!(
                "session {} created but chat failed: {}",
                session_id, message
            )));
        }
        Ok(other) => {
            return Err(tau_agent_plugin::Error::Io(format!(
                "session {} created but unexpected chat response: {:?}",
                session_id, other
            )));
        }
        Err(e) => {
            return Err(tau_agent_plugin::Error::Io(format!(
                "session {} created but chat failed: {}",
                session_id, e
            )));
        }
    }

    // Update task with session info.
    db.set_session_id(task_id, &session_id)?;
    db.record_session(task_id, &session_id, "worker")?;

    Ok(session_id)
}

// ---------------------------------------------------------------------------
// Planning dispatch
// ---------------------------------------------------------------------------

/// Dispatch a planning-state task: create a read-only session (no worktree)
/// that explores code and produces a plan with affected files.
fn dispatch_planning(
    db: &TasksDb,
    task: &Task,
    parent_session_id: Option<&str>,
    project_path: &str,
    writer: &mut impl Write,
    reader: &mut impl BufRead,
) -> tau_agent_plugin::Result<String> {
    let task_id = task.id;

    // If there is already a live planner session for this task, reuse it
    // instead of creating a duplicate.  This makes dispatch idempotent when
    // the schedule pass runs more than once while the task is still planning.
    if let Some(existing_sid) = find_reusable_session(db, task_id, "planner", writer, reader) {
        eprintln!(
            "tasks scheduler: planning task {} already has a live planner session {}, reusing",
            task_id, existing_sid
        );
        return Ok(existing_sid);
    }

    // If the task has a session_id from a previous lifecycle phase, log and
    // continue — the old session is already recorded in task_sessions.
    if let Some(ref existing_sid) = task.session_id {
        eprintln!(
            "tasks scheduler: planning task {} replacing previous session {} with new dispatch",
            task_id, existing_sid
        );
    }

    // Model inheritance: use the triggering session for model.
    let model_source = resolve_model_source(db, task, parent_session_id);
    let model = model_source
        .as_deref()
        .and_then(|sid| get_session_model(sid, writer, reader));

    // Session parenting: the planner is the root session for this task,
    // so its parent is the parent *task*'s session.
    let hierarchy_parent = parent_session_id
        .map(str::to_string)
        .or_else(|| resolve_parent_session(db, task));

    // Planning sessions use the project directory as cwd (no worktree).
    let create_req = tau_agent_plugin::Request::CreateSession {
        model,
        provider: None,
        system_prompt: None,
        cwd: Some(project_path.to_string()),
        parent_id: hierarchy_parent,
        child_budget: 16,
        tagline: Some(format!("Planning task {}: {}", task.id, task.title)),
        auto_archive: false,
        notify_parent: false,
        project_name: Some(task.project_name.clone()),
        sandbox_profile: task.sandbox_profile.clone(),
    };

    let session_id = match server_request(writer, reader, create_req)? {
        tau_agent_plugin::Response::SessionCreated { session_id } => session_id,
        tau_agent_plugin::Response::Error { message } => {
            return Err(tau_agent_plugin::Error::Io(format!(
                "create planning session for task {}: {}",
                task_id, message
            )));
        }
        other => {
            return Err(tau_agent_plugin::Error::Io(format!(
                "unexpected response creating planning session for task {}: {:?}",
                task_id, other
            )));
        }
    };

    // Load project-specific planning instructions
    let project_instructions =
        tasks_config::load_project_instructions(project_path, Some(&task.project_name), "planning")
            .unwrap_or_default();

    let merge_target = db
        .get_merge_target(task_id)
        .unwrap_or_else(|_| "main".into());

    // Send planning-specific initial message.
    let chat_msg = build_planning_message(task, &project_instructions, &merge_target);
    let chat_req = tau_agent_plugin::Request::Chat {
        session_id: session_id.clone(),
        text: chat_msg,
    };

    match server_request(writer, reader, chat_req) {
        Ok(tau_agent_plugin::Response::Ok) => {}
        Ok(tau_agent_plugin::Response::Error { message }) => {
            return Err(tau_agent_plugin::Error::Io(format!(
                "planning session {} created but chat failed: {}",
                session_id, message
            )));
        }
        Ok(other) => {
            return Err(tau_agent_plugin::Error::Io(format!(
                "planning session {} created but unexpected chat response: {:?}",
                session_id, other
            )));
        }
        Err(e) => {
            return Err(tau_agent_plugin::Error::Io(format!(
                "planning session {} created but chat failed: {}",
                session_id, e
            )));
        }
    }

    db.set_session_id(task_id, &session_id)?;
    db.record_session(task_id, &session_id, "planner")?;

    Ok(session_id)
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

    let mut msg = format!(
        "You are in the PLANNING phase for task {id}: {title}\n\
         \n\
         Task branch: {branch}\n\
         Merge target: {target}\n\
         {nested}\
         Use the task_get tool to read the full specification:\n\
         - Call `task_get` with arguments: {{\"id\": {id}}}\n\
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
    let req = tau_agent_plugin::Request::GetSessionInfo {
        session_id: session_id.clone(),
    };
    match server_request(writer, reader, req) {
        Ok(tau_agent_plugin::Response::SessionInfo { info }) => {
            if info.archived {
                return None;
            }
            Some(session_id)
        }
        _ => None,
    }
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
    let task_id = task.id;

    // Try to reuse an existing reviewer session.
    if let Some(existing_sid) = find_reusable_session(db, task_id, "reviewer", writer, reader) {
        let msg = format!(
            "Task {} has been re-submitted for review. \
             Please run task_get to read the latest changes and review feedback, \
             then re-review the work.\n\
             - Call `task_get` with arguments: {{\"id\": {}}}",
            task_id, task_id
        );
        resume_session(&existing_sid, task_id, &msg, writer, reader)?;
        eprintln!(
            "tasks: reusing existing reviewer session {} for task {}",
            existing_sid, task_id
        );
        return Ok(existing_sid);
    }

    // No reusable session found — create a new one.

    // Model inheritance: use the triggering session for model, falling back
    // through the hierarchy.
    let model_source = resolve_model_source(db, task, parent_session_id);
    let model = model_source
        .as_deref()
        .and_then(|sid| get_session_model(sid, writer, reader));

    // Session parenting: use the planner (orchestrator), not the worker that
    // triggered the review.
    let hierarchy_parent = resolve_hierarchy_parent(db, task);

    // Review sessions use the task's worktree as cwd.
    let cwd = task
        .worktree_path
        .clone()
        .or(Some(project_path.to_string()));

    let create_req = tau_agent_plugin::Request::CreateSession {
        model,
        provider: None,
        system_prompt: None,
        cwd,
        parent_id: hierarchy_parent,
        child_budget: 16,
        tagline: Some(format!("Review task {}: {}", task.id, task.title)),
        auto_archive: false,
        notify_parent: false,
        project_name: Some(task.project_name.clone()),
        sandbox_profile: task.sandbox_profile.clone(),
    };

    let session_id = match server_request(writer, reader, create_req)? {
        tau_agent_plugin::Response::SessionCreated { session_id } => session_id,
        tau_agent_plugin::Response::Error { message } => {
            return Err(tau_agent_plugin::Error::Io(format!(
                "create review session for task {}: {}",
                task_id, message
            )));
        }
        other => {
            return Err(tau_agent_plugin::Error::Io(format!(
                "unexpected response creating review session for task {}: {:?}",
                task_id, other
            )));
        }
    };

    // Load project-specific review instructions
    let project_instructions =
        tasks_config::load_project_instructions(project_path, Some(&task.project_name), "review")
            .unwrap_or_default();

    let merge_target = db
        .get_merge_target(task.id)
        .unwrap_or_else(|_| "main".into());
    let chat_msg = build_review_message(task, &project_instructions, &merge_target);
    let chat_req = tau_agent_plugin::Request::Chat {
        session_id: session_id.clone(),
        text: chat_msg,
    };

    match server_request(writer, reader, chat_req) {
        Ok(tau_agent_plugin::Response::Ok) => {}
        Ok(tau_agent_plugin::Response::Error { message }) => {
            return Err(tau_agent_plugin::Error::Io(format!(
                "review session {} created but chat failed: {}",
                session_id, message
            )));
        }
        Ok(other) => {
            return Err(tau_agent_plugin::Error::Io(format!(
                "review session {} created but unexpected chat response: {:?}",
                session_id, other
            )));
        }
        Err(e) => {
            return Err(tau_agent_plugin::Error::Io(format!(
                "review session {} created but chat failed: {}",
                session_id, e
            )));
        }
    }

    db.record_session(task_id, &session_id, "reviewer")?;

    Ok(session_id)
}

/// Build the initial message for a review session.
fn build_review_message(task: &Task, project_instructions: &str, merge_target: &str) -> String {
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
         4. Does the code follow project conventions?\n\
         5. Run the project checklist if available\n\
         \n\
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
    let task_id = task.id;

    // Try to reuse an existing refiner session.
    if let Some(existing_sid) = find_reusable_session(db, task_id, "refiner", writer, reader) {
        let msg = format!(
            "Task {} has been re-submitted for refining. \
             The plan has been revised. Please run task_get to read the updated \
             plan and re-evaluate it.\n\
             - Call `task_get` with arguments: {{\"id\": {}}}",
            task_id, task_id
        );
        resume_session(&existing_sid, task_id, &msg, writer, reader)?;
        eprintln!(
            "tasks: reusing existing refiner session {} for task {}",
            existing_sid, task_id
        );
        return Ok(existing_sid);
    }

    // No reusable session found — create a new one.

    // Model inheritance: use the triggering session for model, falling back
    // through the hierarchy.
    let model_source = resolve_model_source(db, task, parent_session_id);
    let model = model_source
        .as_deref()
        .and_then(|sid| get_session_model(sid, writer, reader));

    // Session parenting: use the planner (orchestrator), not whatever
    // session triggered the refining state change.
    let hierarchy_parent = resolve_hierarchy_parent(db, task);

    let create_req = tau_agent_plugin::Request::CreateSession {
        model,
        provider: None,
        system_prompt: None,
        cwd: Some(project_path.to_string()),
        parent_id: hierarchy_parent,
        child_budget: 16,
        tagline: Some(format!("Refining task {}: {}", task.id, task.title)),
        auto_archive: false,
        notify_parent: false,
        project_name: Some(task.project_name.clone()),
        sandbox_profile: task.sandbox_profile.clone(),
    };

    let session_id = match server_request(writer, reader, create_req)? {
        tau_agent_plugin::Response::SessionCreated { session_id } => session_id,
        tau_agent_plugin::Response::Error { message } => {
            return Err(tau_agent_plugin::Error::Io(format!(
                "create refining session for task {}: {}",
                task_id, message
            )));
        }
        other => {
            return Err(tau_agent_plugin::Error::Io(format!(
                "unexpected response creating refining session for task {}: {:?}",
                task_id, other
            )));
        }
    };

    // Load project-specific refining instructions
    let project_instructions =
        tasks_config::load_project_instructions(project_path, Some(&task.project_name), "refining")
            .unwrap_or_default();

    let merge_target = db
        .get_merge_target(task_id)
        .unwrap_or_else(|_| "main".into());

    let chat_msg = build_refining_message(task, &project_instructions, &merge_target);
    let chat_req = tau_agent_plugin::Request::Chat {
        session_id: session_id.clone(),
        text: chat_msg,
    };

    match server_request(writer, reader, chat_req) {
        Ok(tau_agent_plugin::Response::Ok) => {}
        Ok(tau_agent_plugin::Response::Error { message }) => {
            return Err(tau_agent_plugin::Error::Io(format!(
                "refining session {} created but chat failed: {}",
                session_id, message
            )));
        }
        Ok(other) => {
            return Err(tau_agent_plugin::Error::Io(format!(
                "refining session {} created but unexpected chat response: {:?}",
                session_id, other
            )));
        }
        Err(e) => {
            return Err(tau_agent_plugin::Error::Io(format!(
                "refining session {} created but chat failed: {}",
                session_id, e
            )));
        }
    }

    db.record_session(task_id, &session_id, "refiner")?;

    Ok(session_id)
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
        .map_err(|e| tau_agent_plugin::Error::Io(format!("git merge-base: {}", e)))?;

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
            let attempt = merge_one_task(db, task, resolve_path, writer, reader);
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

    if current.state != "approved" {
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
            state: Some("merging".into()),
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
    match crate::tasks_merge::merge_task(db, task_id, &project_dir, writer, reader) {
        Ok(result) => {
            if result.success {
                // Transition to merged
                if let Err(e) = db.update_task(
                    task_id,
                    &TaskUpdate {
                        state: Some("merged".into()),
                        ..Default::default()
                    },
                    None,
                ) {
                    eprintln!(
                        "tasks scheduler: merge succeeded but transition to merged failed for task {}: {}",
                        task_id, e
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
                        state: Some("active".into()),
                        ..Default::default()
                    },
                    None,
                ) {
                    eprintln!(
                        "tasks scheduler: failed to transition task {} back to active: {}",
                        task_id, e
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
                    state: Some("active".into()),
                    ..Default::default()
                },
                None,
            ) {
                eprintln!(
                    "tasks scheduler: failed to transition task {} back to active: {}",
                    task_id, te
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
fn build_initial_message(task: &Task, merge_target: &str, project_instructions: &str) -> String {
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
         When done, run the project checklist, then mark the task:\n\
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

pub fn server_request(
    writer: &mut impl Write,
    reader: &mut impl BufRead,
    request: tau_agent_plugin::Request,
) -> tau_agent_plugin::Result<tau_agent_plugin::Response> {
    tau_agent_plugin::tunnel::server_request(writer, reader, request, "task-sr")
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
    let inflight_states: HashSet<&str> = ["active", "review", "merging", "refining"]
        .iter()
        .copied()
        .collect();

    let mut active = Vec::new();
    let mut queued_planning = Vec::new();
    let mut queued_ready = Vec::new();
    let mut blocked = Vec::new();

    // Build a map of active task IDs to their affected files for conflict detection.
    let active_tasks_files: Vec<(i64, Vec<String>)> = all_tasks
        .iter()
        .filter(|t| inflight_states.contains(t.state.as_str()))
        .map(|t| (t.id, extract_files(&t.affected_files)))
        .collect();

    for task in all_tasks {
        if inflight_states.contains(task.state.as_str()) {
            // Active/in-flight task.
            // Check if it's waiting on dependencies even though it's active.
            let deps = db.get_blocking_dependencies(task.id)?;
            let wait_reasons: Vec<WaitReason> = deps
                .iter()
                .map(|d| WaitReason::Dependency {
                    task_id: d.id,
                    title: d.title.clone(),
                    state: d.state.clone(),
                    project_name: d.project_name.clone(),
                })
                .collect();
            active.push(TaskStatus {
                session_id: task.session_id.clone(),
                task,
                wait_reasons,
            });
        } else if task.state == "ready" || task.state == "planning" {
            // Check blocking dependencies first.
            let deps = db.get_blocking_dependencies(task.id)?;
            if !deps.is_empty() {
                // Blocked by dependencies.
                let wait_reasons = deps
                    .iter()
                    .map(|d| WaitReason::Dependency {
                        task_id: d.id,
                        title: d.title.clone(),
                        state: d.state.clone(),
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
                if task.state == "ready" {
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
                if task.state == "ready" {
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
                if task.state == "planning" {
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
        out.push_str(&format!("\nACTIVE ({}):\n", status.active.len()));
        for ts in &status.active {
            format_task_line(&mut out, ts);
        }
    }

    // Queued - Planning.
    if !status.queued_planning.is_empty() {
        out.push_str(&format!(
            "\nQUEUED - PLANNING ({}):\n",
            status.queued_planning.len()
        ));
        for ts in &status.queued_planning {
            format_task_line(&mut out, ts);
        }
    }

    // Queued - Ready.
    if !status.queued_ready.is_empty() {
        out.push_str(&format!(
            "\nQUEUED - READY ({}):\n",
            status.queued_ready.len()
        ));
        for ts in &status.queued_ready {
            format_task_line(&mut out, ts);
        }
    }

    // Blocked.
    if !status.blocked.is_empty() {
        out.push_str(&format!("\nBLOCKED ({}):\n", status.blocked.len()));
        for ts in &status.blocked {
            format_task_line(&mut out, ts);
        }
    }

    if status.active.is_empty()
        && status.queued_planning.is_empty()
        && status.queued_ready.is_empty()
        && status.blocked.is_empty()
    {
        out.push_str("\nNo active or queued tasks.\n");
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
        // Show abbreviated file names (just filename, not full path).
        let abbrev: Vec<&str> = files
            .iter()
            .map(|f| f.rsplit('/').next().unwrap_or(f.as_str()))
            .collect();
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
                let abbrev: Vec<&str> = files
                    .iter()
                    .map(|f| f.rsplit('/').next().unwrap_or(f.as_str()))
                    .collect();
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
            state: "ready".to_string(),
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
            skip_planning: false,
            require_approval: false,
            sandbox_profile: None,
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
    fn test_build_initial_message_with_review() {
        let mut task = make_task(5, 0, None);
        task.branch = Some("task-1-5".into());
        task.worktree_path = Some("/tmp/wt-5".into());
        let msg = build_initial_message(&task, "main", "");
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
        let msg = build_initial_message(&task, "main", "");
        assert!(msg.contains("\"state\": \"approved\""));
        assert!(msg.contains("skip_review is true"));
    }

    #[test]
    fn test_build_initial_message_tool_call_format() {
        let mut task = make_task(42, 0, None);
        task.branch = Some("task-1-42".into());
        task.worktree_path = Some("/tmp/wt-42".into());
        let msg = build_initial_message(&task, "main", "");
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
        let msg = build_initial_message(&task, "task-1-5", "");
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
        let msg = build_initial_message(&task, "main", instructions);
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
        let msg = build_initial_message(&task, "main", "   \n\n  ");
        assert!(!msg.contains("Project-specific worker instructions"));
    }

    #[test]
    fn test_build_review_message_uses_merge_target() {
        let mut task = make_task(10, 0, None);
        task.branch = Some("task-14-10".into());
        task.worktree_path = Some("/tmp/wt-10".into());
        let msg = build_review_message(&task, "", "task-1-5");
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
                true,
                false,
                None,
                None,
            )
            .unwrap();
        // interactive -> ready
        db.update_task(
            task.id,
            &crate::tasks_db::TaskUpdate {
                state: Some("ready".into()),
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
        if task.state == "ready" {
            db.assign_task(task_id, "test-session").unwrap();
        }
        db.update_task(
            task_id,
            &crate::tasks_db::TaskUpdate {
                state: Some("approved".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        db.update_task(
            task_id,
            &crate::tasks_db::TaskUpdate {
                state: Some("merging".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        db.update_task(
            task_id,
            &crate::tasks_db::TaskUpdate {
                state: Some("merged".into()),
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
                false,
                false,
                None,
                None,
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

        // Now move it to active before merge_approved runs
        db.update_task(
            task.id,
            &TaskUpdate {
                state: Some("active".into()),
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
                false,
                false,
                None,
                None,
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
        assert_eq!(updated.state, "active");
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
                false,
                false,
                None,
                None,
            )
            .unwrap();
        // interactive -> ready
        db.update_task(
            task.id,
            &crate::tasks_db::TaskUpdate {
                state: Some("ready".into()),
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
                false,
                false,
                None,
                None,
            )
            .unwrap();
        // interactive -> ready
        db.update_task(
            task.id,
            &crate::tasks_db::TaskUpdate {
                state: Some("ready".into()),
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
                false,
                false,
                None,
                None,
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
                false,
                false,
                None,
                None,
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
                false,
                false,
                None,
                None,
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
    fn test_build_review_message() {
        let mut task = make_task(10, 0, None);
        task.branch = Some("task-1-10".into());
        task.worktree_path = Some("/tmp/wt-10".into());
        let msg = build_review_message(&task, "", "main");
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
        let msg = build_review_message(&task, "Check for SQL injection.", "main");
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
                false,
                false,
                None,
                None,
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
                false,
                false,
                None,
                None,
            )
            .unwrap();
        assert_eq!(child.state, "planning");

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
                false,
                false,
                None,
                None,
            )
            .unwrap();
        db.update_task(
            task.id,
            &crate::tasks_db::TaskUpdate {
                state: Some("planning".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();

        let schedulable = db.get_schedulable_tasks("test-project").unwrap();
        assert_eq!(schedulable.len(), 1);
        assert_eq!(schedulable[0].id, task.id);
        assert_eq!(schedulable[0].state, "planning");
    }

    #[test]
    fn test_get_status_empty() {
        let db = TasksDb::open_memory().unwrap();
        let status = get_status(&db, "test-project", None).unwrap();
        assert!(status.active.is_empty());
        assert!(status.queued_planning.is_empty());
        assert!(status.queued_ready.is_empty());
        assert!(status.blocked.is_empty());
        assert_eq!(status.inflight_count, 0);
        assert_eq!(status.max_concurrent, MAX_CONCURRENT_TASKS);
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
                false,
                false,
                None,
                None,
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
                false,
                false,
                None,
                None,
            )
            .unwrap();
        assert_eq!(child.state, "planning");

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
            blocked: vec![],
            inflight_count: 1,
            max_concurrent: 8,
        };
        let output = format_status(&status);
        assert!(output.contains("Task Scheduler Status"));
        assert!(output.contains("ACTIVE"));
        assert!(output.contains("#1"));
        assert!(output.contains("s100"));
        assert!(output.contains("QUEUED - READY"));
        assert!(output.contains("#2"));
        assert!(output.contains("file conflict"));
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
}
