//! Scheduler logic for the task system.
//!
//! Provides two synchronous operations called from tool handlers:
//!
//! - **schedule**: query `ready` tasks, pick a non-conflicting batch, and
//!   prepare them for dispatch (create branch + worktree, update DB).
//! - **dispatch**: create a session for a prepared task and send the initial
//!   chat message via the ServerRequest tunnel.

use std::collections::HashSet;
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

/// Run a scheduling pass: find ready tasks, pick a non-conflicting batch,
/// create branches and worktrees, update task state to `active`.
///
/// Returns the list of tasks that were prepared for dispatch.
pub fn schedule(db: &TasksDb, project: &str) -> crate::Result<Vec<ScheduledTask>> {
    let ready_tasks = db.list_tasks(project, Some("ready"), None, None, None)?;

    if ready_tasks.is_empty() {
        return Ok(Vec::new());
    }

    let batch = select_non_conflicting(&ready_tasks);
    if batch.is_empty() {
        return Ok(Vec::new());
    }

    // We need the repo root to create branches and worktrees.
    let repo_root = tasks_git::get_repo_root(project)?;

    let mut scheduled = Vec::new();

    for task in batch {
        match prepare_task(db, task, &repo_root) {
            Ok(st) => scheduled.push(st),
            Err(e) => {
                // Log but don't fail the whole batch.
                eprintln!("tasks scheduler: failed to prepare task {}: {}", task.id, e);
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

/// Dispatch a single task: create a session via ServerRequest, send initial
/// chat, and update the task with the session ID.
///
/// The `writer` and `reader` are the plugin's stdout/stdin — used to tunnel
/// ServerRequests through to the tau server.
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

    // Task must be active (prepared by schedule) or ready (we'll prepare it).
    if task.state == "ready" {
        // Not yet prepared — do it inline.
        let repo_root = tasks_git::get_repo_root(&task.project)?;
        prepare_task(db, &task, &repo_root)?;
        // Re-read after prepare.
    } else if task.state != "active" {
        return Err(crate::Error::Io(format!(
            "task {} is in state '{}', must be 'ready' or 'active' to dispatch",
            task_id, task.state
        )));
    }

    // Re-read the task to get updated fields after potential prepare.
    let task = db
        .get_task(task_id)?
        .ok_or_else(|| crate::Error::Io(format!("task {} not found after prepare", task_id)))?;

    let cwd = task.worktree_path.clone();

    // Create session via ServerRequest.
    let create_req = crate::protocol::Request::CreateSession {
        model: None,
        provider: None,
        system_prompt: None,
        cwd,
        parent_id: parent_session_id.map(String::from),
        child_budget: 4,
        tagline: Some(format!("Task {}: {}", task.id, task.title)),
        auto_archive: false,
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

    // Also set assigned_session if not already set.
    if task.assigned_session.is_none() {
        // Use update_task but only to trigger the assigned_session history;
        // we already transitioned state in prepare_task.
        let now = crate::types::timestamp_ms() as i64;
        // Direct SQL to set assigned_session without state validation.
        // (assign_task requires "ready" state, but we're already "active".)
        db.set_assigned_session(task_id, &session_id)?;
        let _ = now; // suppress warning
    }

    Ok(session_id)
}

/// Build the initial chat message sent to a dispatched task's session.
fn build_initial_message(task: &Task) -> String {
    let review_instruction = if task.skip_review {
        format!(
            "- task_update {} state=approved (skip_review is true for this task)",
            task.id
        )
    } else {
        format!(
            "- task_update {} state=review (skip_review is false — needs review)",
            task.id
        )
    };

    format!(
        "You are working on task {id}: {title}\n\
         \n\
         Read the full task specification with task_get {id}.\n\
         Assign yourself with task_assign {id}.\n\
         Do the work in this worktree.\n\
         When done, run the project checklist, then mark the task:\n\
         {review}",
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
fn server_request(
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
        if let PluginRequest::ServerResponse {
            request_id: rid,
            response,
        } = req
            && rid == request_id
        {
            return Ok(response);
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
            assigned_session: None,
            branch: None,
            worktree_path: None,
            session_id: None,
            skip_review: false,
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
        assert!(msg.contains("task_get 5"));
        assert!(msg.contains("task_assign 5"));
        assert!(msg.contains("state=review"));
        assert!(msg.contains("skip_review is false"));
    }

    #[test]
    fn test_build_initial_message_skip_review() {
        let mut task = make_task(7, 0, None);
        task.skip_review = true;
        let msg = build_initial_message(&task);
        assert!(msg.contains("state=approved"));
        assert!(msg.contains("skip_review is true"));
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
}
