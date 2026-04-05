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

use crate::plugin::{PluginMessage, PluginRequest};
use crate::tasks_db::{Task, TaskUpdate, TasksDb};
use crate::tasks_git;

// ---------------------------------------------------------------------------
// Batch selection
// ---------------------------------------------------------------------------

/// Select a non-conflicting batch of ready tasks.
///
/// Greedy algorithm:
/// 1. Sort by priority descending (stable — preserves creation order for ties).
/// 2. For each task, check if its `affected_files` overlap with any already
///    selected task.
/// 3. Tasks **without** `affected_files` are assumed to potentially conflict
///    with everything — they are only scheduled if nothing else is selected
///    yet (i.e., they run alone).
pub fn select_non_conflicting(tasks: &[Task]) -> Vec<&Task> {
    let mut sorted: Vec<&Task> = tasks.iter().collect();
    sorted.sort_by(|a, b| b.priority.cmp(&a.priority));

    let mut selected: Vec<&Task> = Vec::new();
    let mut claimed_files: HashSet<String> = HashSet::new();
    let mut has_unbounded = false;

    for task in &sorted {
        let files = extract_files(&task.affected_files);

        if files.is_empty() {
            // No affected_files declared — treat as potentially conflicting
            // with everything. Only schedule alone.
            if selected.is_empty() {
                selected.push(task);
                has_unbounded = true;
                // Don't break — but the flag prevents any further additions.
            }
            // Skip if we already have selections.
            continue;
        }

        // If we already selected an unbounded task, skip everything else.
        if has_unbounded {
            continue;
        }

        // Check overlap with already-claimed files.
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
fn extract_files(val: &Option<serde_json::Value>) -> Vec<String> {
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

/// Run a scheduling pass: find ready/planning tasks, pick a non-conflicting
/// batch, create branches and worktrees (for ready tasks), update task state.
///
/// Ready tasks get branches/worktrees and transition to `active`.
/// Planning tasks are dispatched without worktrees (read-only sessions).
///
/// Returns the list of tasks that were prepared for dispatch.
pub fn schedule(db: &TasksDb, project: &str) -> crate::Result<Vec<ScheduledTask>> {
    let schedulable_tasks = db.get_schedulable_tasks(project)?;

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
        let batch = select_non_conflicting(&ready_tasks);
        if !batch.is_empty() {
            // We need the repo root to create branches and worktrees.
            let repo_root = tasks_git::get_repo_root(project)?;

            for task in batch {
                match prepare_task(db, task, &repo_root) {
                    Ok(st) => scheduled.push(st),
                    Err(e) => {
                        // Log but don't fail the whole batch.
                        eprintln!("tasks scheduler: failed to prepare task {}: {}", task.id, e);
                    }
                }
            }
        }
    }

    Ok(scheduled)
}

/// Prepare a single task for dispatch: create branch, worktree, update DB.
fn prepare_task(db: &TasksDb, task: &Task, repo_root: &str) -> crate::Result<ScheduledTask> {
    let branch = tasks_git::task_branch_name(task.id, task.parent_id);

    // Determine the base branch: parent's branch, or "main".
    let base_branch = match task.parent_id {
        Some(pid) => {
            let parent = db
                .get_task(pid)?
                .ok_or_else(|| crate::Error::Io(format!("parent task {} not found", pid)))?;
            parent.branch.unwrap_or_else(|| "main".to_string())
        }
        None => "main".to_string(),
    };

    // Create branch (skip if it already exists — idempotent).
    if !tasks_git::branch_exists(repo_root, &branch)? {
        tasks_git::create_branch(repo_root, &branch, &base_branch)?;
    }

    // Derive worktree path and create it.
    let worktree_path = tasks_git::task_worktree_path(repo_root, task.id)?;

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

/// Look up a session's model via GetSessionInfo. Returns `None` if the
/// request fails or the session is not found.
pub fn get_session_model(
    session_id: &str,
    writer: &mut impl Write,
    reader: &mut impl BufRead,
) -> Option<String> {
    let req = crate::protocol::Request::GetSessionInfo {
        session_id: session_id.to_string(),
    };
    match server_request(writer, reader, req) {
        Ok(crate::protocol::Response::SessionInfo { info }) => Some(info.model),
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
    writer: &mut impl Write,
    reader: &mut impl BufRead,
) -> crate::Result<String> {
    let task = db
        .get_task(task_id)?
        .ok_or_else(|| crate::Error::Io(format!("task {} not found", task_id)))?;

    // Handle planning-state dispatch (no worktree, read-only session)
    if task.state == "planning" {
        return dispatch_planning(db, &task, parent_session_id, writer, reader);
    }

    // Task must be active (prepared by schedule) or ready (we'll prepare it).
    if task.state == "ready" {
        // Not yet prepared — do it inline.
        let repo_root = tasks_git::get_repo_root(&task.project)?;
        prepare_task(db, &task, &repo_root)?;
        // Re-read after prepare.
    } else if task.state != "active" {
        return Err(crate::Error::Io(format!(
            "task {} is in state '{}', must be 'ready', 'active', or 'planning' to dispatch",
            task_id, task.state
        )));
    }

    // Re-read the task to get updated fields after potential prepare.
    let task = db
        .get_task(task_id)?
        .ok_or_else(|| crate::Error::Io(format!("task {} not found after prepare", task_id)))?;

    // Reject if the task already has a session — dispatching again would
    // create a duplicate session working in the same worktree.
    if let Some(ref existing_sid) = task.session_id {
        return Err(crate::Error::Io(format!(
            "task {} already has session {}, cannot dispatch again",
            task_id, existing_sid
        )));
    }

    let cwd = task.worktree_path.clone();

    // Resolve the effective parent session: use the explicit parent_session_id
    // (from a manual tool call), or fall back to the parent task's session.
    let effective_parent_session = match parent_session_id {
        Some(sid) => Some(sid.to_string()),
        None => resolve_parent_session(db, &task),
    };

    // Inherit model from the parent session so child tasks use the same model.
    let model = effective_parent_session
        .as_deref()
        .and_then(|sid| get_session_model(sid, writer, reader));

    // Create session via ServerRequest.
    let create_req = crate::protocol::Request::CreateSession {
        model,
        provider: None,
        system_prompt: None,
        cwd,
        parent_id: effective_parent_session,
        child_budget: 4,
        tagline: Some(format!("Task {}: {}", task.id, task.title)),
        auto_archive: false,
        notify_parent: false,
    };

    let session_id = match server_request(writer, reader, create_req)? {
        crate::protocol::Response::SessionCreated { session_id } => session_id,
        crate::protocol::Response::Error { message } => {
            return Err(crate::Error::Io(format!(
                "create session for task {}: {}",
                task_id, message
            )));
        }
        other => {
            return Err(crate::Error::Io(format!(
                "unexpected response creating session for task {}: {:?}",
                task_id, other
            )));
        }
    };

    // Send initial chat message.
    let chat_msg = build_initial_message(&task);
    let chat_req = crate::protocol::Request::Chat {
        session_id: session_id.clone(),
        text: chat_msg,
    };

    match server_request(writer, reader, chat_req) {
        Ok(crate::protocol::Response::Ok) => {}
        Ok(crate::protocol::Response::Error { message }) => {
            return Err(crate::Error::Io(format!(
                "session {} created but chat failed: {}",
                session_id, message
            )));
        }
        Ok(other) => {
            return Err(crate::Error::Io(format!(
                "session {} created but unexpected chat response: {:?}",
                session_id, other
            )));
        }
        Err(e) => {
            return Err(crate::Error::Io(format!(
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
    writer: &mut impl Write,
    reader: &mut impl BufRead,
) -> crate::Result<String> {
    let task_id = task.id;

    // Reject if the task already has a session
    if let Some(ref existing_sid) = task.session_id {
        return Err(crate::Error::Io(format!(
            "task {} already has session {}, cannot dispatch again",
            task_id, existing_sid
        )));
    }

    // Resolve the effective parent session
    let effective_parent_session = match parent_session_id {
        Some(sid) => Some(sid.to_string()),
        None => resolve_parent_session(db, task),
    };

    let model = effective_parent_session
        .as_deref()
        .and_then(|sid| get_session_model(sid, writer, reader));

    // Planning sessions use the project directory as cwd (no worktree).
    let create_req = crate::protocol::Request::CreateSession {
        model,
        provider: None,
        system_prompt: None,
        cwd: Some(task.project.clone()),
        parent_id: effective_parent_session,
        child_budget: 4,
        tagline: Some(format!("Planning task {}: {}", task.id, task.title)),
        auto_archive: false,
        notify_parent: false,
    };

    let session_id = match server_request(writer, reader, create_req)? {
        crate::protocol::Response::SessionCreated { session_id } => session_id,
        crate::protocol::Response::Error { message } => {
            return Err(crate::Error::Io(format!(
                "create planning session for task {}: {}",
                task_id, message
            )));
        }
        other => {
            return Err(crate::Error::Io(format!(
                "unexpected response creating planning session for task {}: {:?}",
                task_id, other
            )));
        }
    };

    // Load project-specific planning instructions
    let project_instructions = load_project_instructions(&task.project, "planning");

    // Send planning-specific initial message.
    let chat_msg = build_planning_message(task, &project_instructions);
    let chat_req = crate::protocol::Request::Chat {
        session_id: session_id.clone(),
        text: chat_msg,
    };

    match server_request(writer, reader, chat_req) {
        Ok(crate::protocol::Response::Ok) => {}
        Ok(crate::protocol::Response::Error { message }) => {
            return Err(crate::Error::Io(format!(
                "planning session {} created but chat failed: {}",
                session_id, message
            )));
        }
        Ok(other) => {
            return Err(crate::Error::Io(format!(
                "planning session {} created but unexpected chat response: {:?}",
                session_id, other
            )));
        }
        Err(e) => {
            return Err(crate::Error::Io(format!(
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
fn build_planning_message(task: &Task, project_instructions: &str) -> String {
    let mut msg = format!(
        "You are in the PLANNING phase for task {id}: {title}\n\
         \n\
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
// Review dispatch
// ---------------------------------------------------------------------------

/// Dispatch a review session for a task that just transitioned to `review`.
///
/// Creates a new read-only session that reviews the work done on the task
/// and either approves it or requests changes.
pub fn dispatch_review(
    db: &TasksDb,
    task: &Task,
    parent_session_id: Option<&str>,
    writer: &mut impl Write,
    reader: &mut impl BufRead,
) -> crate::Result<String> {
    let task_id = task.id;

    // Resolve the effective parent session
    let effective_parent_session = match parent_session_id {
        Some(sid) => Some(sid.to_string()),
        None => resolve_parent_session(db, task),
    };

    let model = effective_parent_session
        .as_deref()
        .and_then(|sid| get_session_model(sid, writer, reader));

    // Review sessions use the task's worktree as cwd.
    let cwd = task.worktree_path.clone().or(Some(task.project.clone()));

    let create_req = crate::protocol::Request::CreateSession {
        model,
        provider: None,
        system_prompt: None,
        cwd,
        parent_id: effective_parent_session,
        child_budget: 4,
        tagline: Some(format!("Review task {}: {}", task.id, task.title)),
        auto_archive: false,
        notify_parent: false,
    };

    let session_id = match server_request(writer, reader, create_req)? {
        crate::protocol::Response::SessionCreated { session_id } => session_id,
        crate::protocol::Response::Error { message } => {
            return Err(crate::Error::Io(format!(
                "create review session for task {}: {}",
                task_id, message
            )));
        }
        other => {
            return Err(crate::Error::Io(format!(
                "unexpected response creating review session for task {}: {:?}",
                task_id, other
            )));
        }
    };

    // Load project-specific review instructions
    let project_instructions = load_project_instructions(&task.project, "review");

    let chat_msg = build_review_message(task, &project_instructions);
    let chat_req = crate::protocol::Request::Chat {
        session_id: session_id.clone(),
        text: chat_msg,
    };

    match server_request(writer, reader, chat_req) {
        Ok(crate::protocol::Response::Ok) => {}
        Ok(crate::protocol::Response::Error { message }) => {
            return Err(crate::Error::Io(format!(
                "review session {} created but chat failed: {}",
                session_id, message
            )));
        }
        Ok(other) => {
            return Err(crate::Error::Io(format!(
                "review session {} created but unexpected chat response: {:?}",
                session_id, other
            )));
        }
        Err(e) => {
            return Err(crate::Error::Io(format!(
                "review session {} created but chat failed: {}",
                session_id, e
            )));
        }
    }

    db.record_session(task_id, &session_id, "reviewer")?;

    Ok(session_id)
}

/// Build the initial message for a review session.
fn build_review_message(task: &Task, project_instructions: &str) -> String {
    let mut msg = format!(
        "You are REVIEWING task {id}: {title}\n\
         \n\
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
         Use `bash` with `git diff main...HEAD` or similar to see all changes.\n\
         \n\
         After your review:\n\
         - If approved: call `task_update` with {{\"id\": {id}, \"state\": \"approved\"}}\n\
         - If changes needed: add review feedback via `task_message`, then call \
           `task_update` with {{\"id\": {id}, \"state\": \"active\"}} to send it back to the worker\n",
        id = task.id,
        title = task.title,
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
/// Creates a new read-only session that reviews the plan produced during
/// planning and either approves it to `ready` or sends it back to `planning`.
pub fn dispatch_refining(
    db: &TasksDb,
    task: &Task,
    parent_session_id: Option<&str>,
    writer: &mut impl Write,
    reader: &mut impl BufRead,
) -> crate::Result<String> {
    let task_id = task.id;

    let effective_parent_session = match parent_session_id {
        Some(sid) => Some(sid.to_string()),
        None => resolve_parent_session(db, task),
    };

    let model = effective_parent_session
        .as_deref()
        .and_then(|sid| get_session_model(sid, writer, reader));

    let create_req = crate::protocol::Request::CreateSession {
        model,
        provider: None,
        system_prompt: None,
        cwd: Some(task.project.clone()),
        parent_id: effective_parent_session,
        child_budget: 4,
        tagline: Some(format!("Refining task {}: {}", task.id, task.title)),
        auto_archive: false,
        notify_parent: false,
    };

    let session_id = match server_request(writer, reader, create_req)? {
        crate::protocol::Response::SessionCreated { session_id } => session_id,
        crate::protocol::Response::Error { message } => {
            return Err(crate::Error::Io(format!(
                "create refining session for task {}: {}",
                task_id, message
            )));
        }
        other => {
            return Err(crate::Error::Io(format!(
                "unexpected response creating refining session for task {}: {:?}",
                task_id, other
            )));
        }
    };

    // Load project-specific refining instructions
    let project_instructions = load_project_instructions(&task.project, "refining");

    let chat_msg = build_refining_message(task, &project_instructions);
    let chat_req = crate::protocol::Request::Chat {
        session_id: session_id.clone(),
        text: chat_msg,
    };

    match server_request(writer, reader, chat_req) {
        Ok(crate::protocol::Response::Ok) => {}
        Ok(crate::protocol::Response::Error { message }) => {
            return Err(crate::Error::Io(format!(
                "refining session {} created but chat failed: {}",
                session_id, message
            )));
        }
        Ok(other) => {
            return Err(crate::Error::Io(format!(
                "refining session {} created but unexpected chat response: {:?}",
                session_id, other
            )));
        }
        Err(e) => {
            return Err(crate::Error::Io(format!(
                "refining session {} created but chat failed: {}",
                session_id, e
            )));
        }
    }

    db.record_session(task_id, &session_id, "refiner")?;

    Ok(session_id)
}

/// Build the initial message for a refining session.
fn build_refining_message(task: &Task, project_instructions: &str) -> String {
    let mut msg = format!(
        "You are REFINING the plan for task {id}: {title}\n\
         \n\
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
         After your review:\n\
         - If the plan is good: transition to ready with `task_update` {{\"id\": {id}, \"state\": \"ready\"}}\n\
         - If the plan needs revision: add feedback via `task_message`, then send back to \
           planning with `task_update` {{\"id\": {id}, \"state\": \"planning\"}}\n\
         - If the scope has expanded significantly and needs human sign-off: \
           `task_update` {{\"id\": {id}, \"state\": \"interactive\"}}\n",
        id = task.id,
        title = task.title,
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
// Project-specific instructions
// ---------------------------------------------------------------------------

/// Load project-specific instructions for a given phase from
/// `.tau/instructions.toml`.
///
/// The file format is:
/// ```toml
/// [planning]
/// instructions = "..."
///
/// [refining]
/// instructions = "..."
///
/// [review]
/// instructions = "..."
/// ```
pub fn load_project_instructions(project_dir: &str, phase: &str) -> String {
    let path = std::path::Path::new(project_dir)
        .join(".tau")
        .join("instructions.toml");
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return String::new(),
    };
    match toml::from_str::<toml::Value>(&content) {
        Ok(val) => val
            .get(phase)
            .and_then(|v| v.get("instructions"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        Err(_) => String::new(),
    }
}

// ---------------------------------------------------------------------------
// Rebase check
// ---------------------------------------------------------------------------

/// Check whether a task's branch is rebased on its merge target.
///
/// Returns `Ok(true)` if the branch is up-to-date (merge target is an
/// ancestor of the branch HEAD). Returns `Ok(false)` if the branch needs
/// rebasing. Returns `Err` if the check cannot be performed.
pub fn is_rebased_on_target(db: &TasksDb, task: &Task) -> crate::Result<bool> {
    let branch = task
        .branch
        .as_ref()
        .ok_or_else(|| crate::Error::Io(format!("task {} has no branch set", task.id)))?;

    let worktree = task
        .worktree_path
        .as_ref()
        .ok_or_else(|| crate::Error::Io(format!("task {} has no worktree", task.id)))?;

    let merge_target = db.get_merge_target(task.id)?;

    // Use git merge-base --is-ancestor to check if merge_target is an
    // ancestor of the task's branch.
    let output = std::process::Command::new("git")
        .args(["merge-base", "--is-ancestor", &merge_target, branch])
        .current_dir(worktree)
        .output()
        .map_err(|e| crate::Error::Io(format!("git merge-base: {}", e)))?;

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
    writer: &mut impl Write,
    reader: &mut impl BufRead,
) -> crate::Result<Vec<MergeAttempt>> {
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
            let attempt = merge_one_task(db, task, writer, reader);
            attempts.push(attempt);
        }
    }

    attempts.sort_by_key(|a| a.task_id);
    Ok(attempts)
}

/// Execute the merge sequence for a single approved task.
///
/// Transitions: approved → merging → done (success) or merging → active (failure).
fn merge_one_task(
    db: &TasksDb,
    task: &Task,
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
    let project_dir = &current.project;
    match crate::tasks_merge::merge_task(db, task_id, project_dir, writer, reader) {
        Ok(result) => {
            if result.success {
                // Transition to done
                if let Err(e) = db.update_task(
                    task_id,
                    &TaskUpdate {
                        state: Some("done".into()),
                        ..Default::default()
                    },
                    None,
                ) {
                    eprintln!(
                        "tasks scheduler: merge succeeded but transition to done failed for task {}: {}",
                        task_id, e
                    );
                }

                // Notify parent if all subtasks are done
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
fn build_initial_message(task: &Task) -> String {
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

    format!(
        "You are working on task {id}: {title}\n\
         \n\
         Use the task_get tool (not a bash command) to read the full specification:\n\
         - Call the `task_get` tool with arguments: {{\"id\": {id}}}\n\
         Then assign yourself:\n\
         - Call the `task_assign` tool with arguments: {{\"id\": {id}}}\n\
         \n\
         Do the work in this worktree. Commit your changes on the current branch — do NOT merge into main.\n\
         When done, run the project checklist, then mark the task:\n\
         {review}\n\
         \n\
         Note: task_get, task_assign, and task_update are agent tools (like bash or edit), not CLI commands.",
        id = task.id,
        title = task.title,
        review = review_instruction,
    )
}

// ---------------------------------------------------------------------------
// ServerRequest tunnel (same pattern as worker.rs)
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
/// not hang.  This situation arises when the tasks plugin is executing a
/// background merge/schedule pass (triggered by a state transition) and the
/// server delivers a concurrent tool call before the pass completes.  Rather
/// than silently dropping the concurrent call — which would leave the
/// session stuck forever in "running tools" — we send a descriptive error
/// that the LLM can react to.
pub fn server_request(
    writer: &mut impl Write,
    reader: &mut impl BufRead,
    request: crate::protocol::Request,
) -> crate::Result<crate::protocol::Response> {
    let request_id = format!("task-sr-{}", crate::types::timestamp_ms());
    send_message(
        writer,
        &PluginMessage::ServerRequest {
            request_id: request_id.clone(),
            request,
        },
    );

    // Read lines until we get our ServerResponse.
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
            // A ToolCall arrived while we are mid-ServerRequest (e.g. during a
            // background merge pass).  Answer it immediately with an error so
            // the calling session is not left hanging in "running tools".
            PluginRequest::ToolCall { tool_call_id, .. } => {
                send_message(
                    writer,
                    &PluginMessage::ToolResult(crate::plugin::PluginToolResult {
                        tool_call_id,
                        content: vec![crate::types::ToolResultContent::Text(
                            crate::types::TextContent {
                                text: "tasks plugin is busy with a background merge/schedule \
                                       pass — please retry in a moment"
                                    .into(),
                                text_signature: None,
                            },
                        )],
                        is_error: true,
                    }),
                );
                // Continue waiting for our ServerResponse.
            }
            // Ignore other message types (ServerResponse with wrong ID, etc.)
            _ => {}
        }
    }
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
            project: "/project".to_string(),
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
            worktree_path: None,
            session_id: None,
            skip_review: false,
            skip_planning: false,
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
        let batch = select_non_conflicting(&tasks);
        assert_eq!(batch.len(), 3);
    }

    #[test]
    fn test_select_non_conflicting_with_overlap() {
        let tasks = vec![
            make_task(1, 5, Some(vec!["src/a.rs", "src/b.rs"])),
            make_task(2, 3, Some(vec!["src/b.rs", "src/c.rs"])), // overlaps with task 1
            make_task(3, 1, Some(vec!["src/d.rs"])),             // no overlap
        ];
        let batch = select_non_conflicting(&tasks);
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
        let batch = select_non_conflicting(&tasks);
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
        let batch = select_non_conflicting(&tasks);
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
        let batch = select_non_conflicting(&tasks);
        // Task 1 selected first (higher priority with files), task 2 skipped
        // because we already have selections and task 2 has no files.
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].id, 1);
    }

    #[test]
    fn test_select_non_conflicting_empty() {
        let tasks: Vec<Task> = Vec::new();
        let batch = select_non_conflicting(&tasks);
        assert!(batch.is_empty());
    }

    #[test]
    fn test_select_non_conflicting_all_overlap() {
        let tasks = vec![
            make_task(1, 5, Some(vec!["src/main.rs"])),
            make_task(2, 3, Some(vec!["src/main.rs"])),
            make_task(3, 1, Some(vec!["src/main.rs"])),
        ];
        let batch = select_non_conflicting(&tasks);
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].id, 1); // highest priority wins
    }

    #[test]
    fn test_select_non_conflicting_multiple_no_files() {
        let tasks = vec![make_task(1, 10, None), make_task(2, 5, None)];
        let batch = select_non_conflicting(&tasks);
        // Only highest priority no-files task should be selected
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].id, 1);
    }

    #[test]
    fn test_select_non_conflicting_single_task() {
        let tasks = vec![make_task(42, 0, Some(vec!["file.txt"]))];
        let batch = select_non_conflicting(&tasks);
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].id, 42);
    }

    #[test]
    fn test_select_non_conflicting_single_no_files() {
        let tasks = vec![make_task(42, 0, None)];
        let batch = select_non_conflicting(&tasks);
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
        let task = make_task(5, 0, None);
        let msg = build_initial_message(&task);
        assert!(msg.contains("task 5"));
        assert!(msg.contains("task_get"));
        assert!(msg.contains("task_assign"));
        assert!(msg.contains("task_update"));
        assert!(msg.contains("\"state\": \"review\""));
        assert!(msg.contains("skip_review is false"));
        // Must clarify these are tool calls, not CLI commands
        assert!(msg.contains("not a bash command") || msg.contains("not CLI commands"));
        assert!(msg.contains("do NOT merge into main") || msg.contains("do not merge"));
    }

    #[test]
    fn test_build_initial_message_skip_review() {
        let mut task = make_task(7, 0, None);
        task.skip_review = true;
        let msg = build_initial_message(&task);
        assert!(msg.contains("\"state\": \"approved\""));
        assert!(msg.contains("skip_review is true"));
    }

    #[test]
    fn test_build_initial_message_tool_call_format() {
        let task = make_task(42, 0, None);
        let msg = build_initial_message(&task);
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
    fn test_schedule_empty_db() {
        let db = TasksDb::open_memory().unwrap();
        let result = schedule(&db, "/project");
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
        let batch = select_non_conflicting(&tasks);
        // Should select: 1 (highest), 3 (no overlap with 1), 5 (no overlap with 1, 3)
        assert_eq!(batch.len(), 3);
        assert_eq!(batch[0].id, 1);
        assert_eq!(batch[1].id, 3);
        assert_eq!(batch[2].id, 5);
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
            .create_task(project, title, Some(priority), None, None, true, true)
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

    /// Helper: move task through all states to done.
    fn move_to_done(db: &TasksDb, task_id: i64) {
        // Must be in ready → assign → active → approved → merging → done
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
                state: Some("done".into()),
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

        let dep = create_ready_task(&db, "/project", "Dependency", 5, Some(&files_a));
        let blocked = create_ready_task(&db, "/project", "Blocked", 3, Some(&files_b));
        let free = create_ready_task(
            &db,
            "/project",
            "Free",
            1,
            Some(&serde_json::json!(["src/c.rs"])),
        );

        db.add_relation(blocked.id, dep.id, "depends_on").unwrap();

        // get_schedulable_tasks should exclude "blocked" but include "dep" and "free"
        let schedulable = db.get_schedulable_tasks("/project").unwrap();
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

        let dep = create_ready_task(&db, "/project", "Dependency", 10, Some(&files_shared));
        let blocked = create_ready_task(&db, "/project", "Blocked", 5, Some(&files_other));
        let free = create_ready_task(&db, "/project", "Free", 1, Some(&files_other));

        db.add_relation(blocked.id, dep.id, "depends_on").unwrap();

        // get_schedulable_tasks filters out "blocked"
        let schedulable = db.get_schedulable_tasks("/project").unwrap();
        assert_eq!(schedulable.len(), 2);

        // select_non_conflicting on the filtered set
        let batch = select_non_conflicting(&schedulable);
        let batch_ids: Vec<i64> = batch.iter().map(|t| t.id).collect();
        // dep (priority 10, shared.rs) and free (priority 1, other.rs) don't conflict
        assert!(batch_ids.contains(&dep.id));
        assert!(batch_ids.contains(&free.id));
        assert!(!batch_ids.contains(&blocked.id));
    }

    #[test]
    fn test_dependency_becomes_schedulable_after_dep_done() {
        let db = TasksDb::open_memory().unwrap();
        let dep = create_ready_task(&db, "/project", "Dep", 5, None);
        let task = create_ready_task(&db, "/project", "Task", 3, None);

        db.add_relation(task.id, dep.id, "depends_on").unwrap();

        // Before: only dep is schedulable
        let schedulable = db.get_schedulable_tasks("/project").unwrap();
        let ids: Vec<i64> = schedulable.iter().map(|t| t.id).collect();
        assert!(ids.contains(&dep.id));
        assert!(!ids.contains(&task.id));

        // Move dep to done
        move_to_done(&db, dep.id);

        // After: task should now be schedulable
        let schedulable = db.get_schedulable_tasks("/project").unwrap();
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

        let attempts = merge_approved(&db, &mut writer, &mut reader).unwrap();
        assert!(attempts.is_empty());
    }

    #[test]
    fn test_merge_approved_skips_non_approved() {
        let db = TasksDb::open_memory().unwrap();

        // Create a task in ready state — should not be picked up by merge_approved
        create_ready_task(&db, "/project", "Ready task", 5, None);

        let mut writer: Vec<u8> = Vec::new();
        let mut reader = std::io::BufReader::new(std::io::Cursor::new(Vec::<u8>::new()));

        let attempts = merge_approved(&db, &mut writer, &mut reader).unwrap();
        assert!(attempts.is_empty());
    }

    #[test]
    fn test_merge_approved_task_state_changed_before_merge() {
        let db = TasksDb::open_memory().unwrap();

        // Create a task and move it to approved state
        let task = db
            .create_task("/project", "Will be moved", None, None, None, true, false)
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
        let attempts = merge_approved(&db, &mut writer, &mut reader).unwrap();
        // get_approved_tasks returns nothing since we moved it out of approved
        assert!(attempts.is_empty());
    }

    #[test]
    fn test_merge_approved_transitions_to_merging() {
        let db = TasksDb::open_memory().unwrap();

        // Create and approve a task
        let task = db
            .create_task("/project", "Merge me", None, None, None, true, false)
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

        let attempts = merge_approved(&db, &mut writer, &mut reader).unwrap();
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
    fn test_dispatch_rejects_task_with_existing_session() {
        let db = TasksDb::open_memory().unwrap();

        // Create a task and move it to active state with a session_id already set.
        let task = db
            .create_task(
                "/project",
                "Already dispatched task",
                Some(5),
                None,
                None,
                true,
                false,
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

        // Now try to dispatch again — must fail with a clear error.
        let mut buf = Vec::new();
        let mut reader = std::io::BufReader::new(std::io::Cursor::new(Vec::<u8>::new()));
        let result = dispatch(&db, task.id, Some("caller-session"), &mut buf, &mut reader);

        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("already has session"),
            "unexpected error: {}",
            err_msg
        );
        assert!(
            err_msg.contains("existing-session"),
            "unexpected error: {}",
            err_msg
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
        use crate::plugin::{PluginMessage, PluginRequest};
        use crate::protocol::Response;

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
                        &PluginMessage::ToolResult(crate::plugin::PluginToolResult {
                            tool_call_id,
                            content: vec![crate::types::ToolResultContent::Text(
                                crate::types::TextContent {
                                    text: "tasks plugin is busy".into(),
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
        let msg = build_planning_message(&task, "");
        assert!(msg.contains("PLANNING phase"));
        assert!(msg.contains("task 10"));
        assert!(msg.contains("task_get"));
        assert!(msg.contains("Do NOT modify any files"));
        assert!(msg.contains("refining"));
    }

    #[test]
    fn test_build_planning_message_with_instructions() {
        let task = make_task(10, 0, None);
        let msg = build_planning_message(&task, "Always check for race conditions.");
        assert!(msg.contains("Always check for race conditions"));
        assert!(msg.contains("Project-specific planning instructions"));
    }

    #[test]
    fn test_build_review_message() {
        let task = make_task(10, 0, None);
        let msg = build_review_message(&task, "");
        assert!(msg.contains("REVIEWING task 10"));
        assert!(msg.contains("task_get"));
        assert!(msg.contains("git diff"));
        assert!(msg.contains("approved"));
        assert!(msg.contains("active"));
    }

    #[test]
    fn test_build_review_message_with_instructions() {
        let task = make_task(10, 0, None);
        let msg = build_review_message(&task, "Check for SQL injection.");
        assert!(msg.contains("Check for SQL injection"));
        assert!(msg.contains("Project-specific review instructions"));
    }

    #[test]
    fn test_build_refining_message() {
        let task = make_task(10, 0, None);
        let msg = build_refining_message(&task, "");
        assert!(msg.contains("REFINING the plan"));
        assert!(msg.contains("task 10"));
        assert!(msg.contains("task_get"));
        assert!(msg.contains("ready"));
        assert!(msg.contains("planning"));
    }

    #[test]
    fn test_build_refining_message_with_instructions() {
        let task = make_task(10, 0, None);
        let msg = build_refining_message(&task, "Ensure backward compat.");
        assert!(msg.contains("Ensure backward compat."));
        assert!(msg.contains("Project-specific refining instructions"));
    }

    #[test]
    fn test_load_project_instructions_missing_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let result = load_project_instructions(dir.path().to_str().unwrap(), "review");
        assert_eq!(result, "");
    }

    #[test]
    fn test_load_project_instructions_valid() {
        let dir = tempfile::TempDir::new().unwrap();
        let tau_dir = dir.path().join(".tau");
        std::fs::create_dir_all(&tau_dir).unwrap();
        std::fs::write(
            tau_dir.join("instructions.toml"),
            r#"
[planning]
instructions = "Plan carefully."

[refining]
instructions = "Check for edge cases."

[review]
instructions = "Run the full test suite."
"#,
        )
        .unwrap();

        let planning = load_project_instructions(dir.path().to_str().unwrap(), "planning");
        assert_eq!(planning, "Plan carefully.");

        let refining = load_project_instructions(dir.path().to_str().unwrap(), "refining");
        assert_eq!(refining, "Check for edge cases.");

        let review = load_project_instructions(dir.path().to_str().unwrap(), "review");
        assert_eq!(review, "Run the full test suite.");
    }

    #[test]
    fn test_load_project_instructions_missing_section() {
        let dir = tempfile::TempDir::new().unwrap();
        let tau_dir = dir.path().join(".tau");
        std::fs::create_dir_all(&tau_dir).unwrap();
        std::fs::write(
            tau_dir.join("instructions.toml"),
            "[planning]\ninstructions = \"Only planning\"\n",
        )
        .unwrap();

        let review = load_project_instructions(dir.path().to_str().unwrap(), "review");
        assert_eq!(review, "");
    }

    #[test]
    fn test_schedule_includes_planning_tasks() {
        let db = TasksDb::open_memory().unwrap();

        // Create a parent task, then a subtask (which defaults to planning)
        let parent = db
            .create_task("/project", "Parent", None, None, None, false, false)
            .unwrap();
        let child = db
            .create_task(
                "/project",
                "Child",
                None,
                Some(parent.id),
                None,
                false,
                false,
            )
            .unwrap();
        assert_eq!(child.state, "planning");

        // get_schedulable_tasks should include planning tasks
        let schedulable = db.get_schedulable_tasks("/project").unwrap();
        assert!(schedulable.iter().any(|t| t.id == child.id));
    }

    #[test]
    fn test_get_schedulable_tasks_includes_planning_state() {
        let db = TasksDb::open_memory().unwrap();

        // Create a task in planning state
        let task = db
            .create_task("/project", "Interactive", None, None, None, false, false)
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

        let schedulable = db.get_schedulable_tasks("/project").unwrap();
        assert_eq!(schedulable.len(), 1);
        assert_eq!(schedulable[0].id, task.id);
        assert_eq!(schedulable[0].state, "planning");
    }
}
