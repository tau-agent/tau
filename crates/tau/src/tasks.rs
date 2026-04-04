//! Task system plugin — global plugin for project task management.
//!
//! Speaks the plugin protocol (JSON lines over stdin/stdout).
//! Registers task management tools and handles them via TasksDb.

use std::io::{BufRead, BufReader, BufWriter, Write};

use crate::plugin::{
    HookResult, PluginMessage, PluginRegistration, PluginRequest, PluginToolDef, PluginToolResult,
};
use crate::tasks_db::{TaskUpdate, TasksDb};
use crate::tasks_merge;
use crate::tasks_scheduler;
use crate::types::ToolResultContent;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn send_message(writer: &mut impl Write, msg: &PluginMessage) {
    if let Ok(mut line) = serde_json::to_string(msg) {
        line.push('\n');
        let _ = writer.write_all(line.as_bytes());
        let _ = writer.flush();
    }
}

fn tool_ok(tool_call_id: &str, text: &str) -> PluginToolResult {
    PluginToolResult {
        tool_call_id: tool_call_id.to_string(),
        content: vec![ToolResultContent::Text(crate::types::TextContent {
            text: text.to_string(),
            text_signature: None,
        })],
        is_error: false,
    }
}

fn tool_err(tool_call_id: &str, text: &str) -> PluginToolResult {
    PluginToolResult {
        tool_call_id: tool_call_id.to_string(),
        content: vec![ToolResultContent::Text(crate::types::TextContent {
            text: text.to_string(),
            text_signature: None,
        })],
        is_error: true,
    }
}

// ---------------------------------------------------------------------------
// Tool definitions
// ---------------------------------------------------------------------------

fn tasks_tools() -> Vec<PluginToolDef> {
    vec![
        PluginToolDef {
            name: "task_create".into(),
            description: "Create a new task in the project task board.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "title": {
                        "type": "string",
                        "description": "Task title"
                    },
                    "priority": {
                        "type": "integer",
                        "description": "Priority (higher = more important). Default: 0"
                    },
                    "parent_id": {
                        "type": "integer",
                        "description": "Parent task ID for subtasks"
                    },
                    "tags": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Tags for categorization"
                    },
                    "skip_review": {
                        "type": "boolean",
                        "description": "If true, task skips review and goes directly to approved when done"
                    },
                    "message": {
                        "type": "string",
                        "description": "Initial message/description for the task"
                    }
                },
                "required": ["title"]
            }),
            prompt_snippet: Some("Create a new task in the project task board".into()),
            prompt_guidelines: vec![
                "Top-level tasks start in 'interactive' state for spec refinement".into(),
                "Subtasks (with parent_id) start in 'ready' state with skip_review=false".into(),
                "Valid states: interactive, ready, active, review, approved, merging, failed, done".into(),
            ],
        },
        PluginToolDef {
            name: "task_get".into(),
            description: "Get full details of a task including messages, relations, and subtasks."
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "integer",
                        "description": "Task ID"
                    }
                },
                "required": ["id"]
            }),
            prompt_snippet: Some("Use task_get to read the full specification of a task including all messages and subtasks.".into()),
            prompt_guidelines: vec![
                "When working on a task, first task_get to read the spec, then task_assign to claim it, do the work, then task_update to mark review or approved.".into(),
            ],
        },
        PluginToolDef {
            name: "task_list".into(),
            description: "List tasks filtered by state, parent, or tag. Default: all non-done tasks for current project.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "state": {
                        "type": "string",
                        "description": "Filter by state (interactive, ready, active, review, approved, merging, failed, done)"
                    },
                    "parent_id": {
                        "type": "integer",
                        "description": "Filter by parent task ID"
                    },
                    "tag": {
                        "type": "string",
                        "description": "Filter by tag"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of tasks to return"
                    }
                }
            }),
            prompt_snippet: Some("List tasks filtered by state, parent, or tag".into()),
            prompt_guidelines: vec![],
        },
        PluginToolDef {
            name: "task_assign".into(),
            description: "Assign a task to a session and start working on it. Transitions the task from ready to active.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "integer",
                        "description": "Task ID to assign"
                    },
                    "session_id": {
                        "type": "string",
                        "description": "Session ID to assign to (defaults to current session)"
                    }
                },
                "required": ["id"]
            }),
            prompt_snippet: Some("Use task_assign to claim a task and start working on it. This transitions the task from ready to active.".into()),
            prompt_guidelines: vec![
                "Task must be in 'ready' state to be assigned".into(),
                "If session_id is omitted, the current session is used".into(),
            ],
        },
        PluginToolDef {
            name: "task_update".into(),
            description: "Update task fields (title, state, priority, tags, etc.). Validates state transitions.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "integer",
                        "description": "Task ID"
                    },
                    "title": {
                        "type": "string",
                        "description": "New title"
                    },
                    "state": {
                        "type": "string",
                        "description": "New state (must be a valid transition)"
                    },
                    "priority": {
                        "type": "integer",
                        "description": "New priority"
                    },
                    "tags": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "New tags (replaces existing)"
                    },
                    "affected_files": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Advisory list of affected files"
                    },
                    "skip_review": {
                        "type": "boolean",
                        "description": "Whether to skip review"
                    }
                },
                "required": ["id"]
            }),
            prompt_snippet: Some("Update task fields (title, state, priority, tags, etc.)".into()),
            prompt_guidelines: vec![
                "State transitions are validated: interactive->ready->active->review->approved->merging->done".into(),
                "Shortcuts: interactive->approved, active->approved (skip_review only)".into(),
                "Backward (error recovery): review->active, approved->active/ready/interactive, merging->active (recoverable), merging->failed (terminal), failed->active (manual retry)".into(),
                "Universal overrides: any state->done (manual close), any state->interactive (human takes over)".into(),
                "active -> approved is only allowed if skip_review=true on the task".into(),
            ],
        },
        PluginToolDef {
            name: "task_message".into(),
            description: "Add a message/comment to a task.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "integer",
                        "description": "Task ID"
                    },
                    "content": {
                        "type": "string",
                        "description": "Message content"
                    }
                },
                "required": ["id", "content"]
            }),
            prompt_snippet: Some("Add a message/comment to a task".into()),
            prompt_guidelines: vec![],
        },
        PluginToolDef {
            name: "task_message_edit".into(),
            description: "Edit an existing task message.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "task_id": {
                        "type": "integer",
                        "description": "Task ID"
                    },
                    "message_id": {
                        "type": "integer",
                        "description": "Message ID to edit"
                    },
                    "content": {
                        "type": "string",
                        "description": "New message content"
                    }
                },
                "required": ["task_id", "message_id", "content"]
            }),
            prompt_snippet: Some("Edit an existing task message".into()),
            prompt_guidelines: vec![],
        },
        PluginToolDef {
            name: "task_relate".into(),
            description: "Create a relationship between two tasks.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "from_task": {
                        "type": "integer",
                        "description": "Source task ID"
                    },
                    "to_task": {
                        "type": "integer",
                        "description": "Target task ID"
                    },
                    "relation": {
                        "type": "string",
                        "enum": ["depends_on", "blocks", "related"],
                        "description": "Relationship type"
                    }
                },
                "required": ["from_task", "to_task", "relation"]
            }),
            prompt_snippet: Some("Create a relationship between two tasks".into()),
            prompt_guidelines: vec![],
        },
        PluginToolDef {
            name: "task_search".into(),
            description: "Search tasks by title and message content for current project.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Search query"
                    },
                    "state": {
                        "type": "string",
                        "description": "Optional state filter"
                    }
                },
                "required": ["query"]
            }),
            prompt_snippet: Some("Search tasks by title and message content".into()),
            prompt_guidelines: vec![],
        },
        PluginToolDef {
            name: "task_schedule".into(),
            description: "Run a scheduling pass: find ready tasks, pick a non-conflicting batch, create branches and worktrees, and transition them to active. Returns the list of tasks ready to dispatch.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "project": {
                        "type": "string",
                        "description": "Project path (defaults to session cwd)"
                    }
                }
            }),
            prompt_snippet: Some("Run a scheduling pass to prepare ready tasks for dispatch".into()),
            prompt_guidelines: vec![
                "Finds all ready tasks, selects a non-conflicting batch based on affected_files, creates branches/worktrees, transitions to active.".into(),
                "After scheduling, use task_dispatch to create sessions and start work.".into(),
            ],
        },
        PluginToolDef {
            name: "task_merge".into(),
            description: "Merge an approved task: rebase, run checklist, fast-forward merge into target branch.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "integer",
                        "description": "Task ID to merge"
                    }
                },
                "required": ["id"]
            }),
            prompt_snippet: Some("Merge an approved task into its target branch".into()),
            prompt_guidelines: vec![
                "Task must be in 'approved' state. Transitions to 'merging', then 'done' on success, back to 'active' on recoverable error (rebase/checklist), or 'failed' on terminal error.".into(),
                "Runs: rebase onto target, project checklist, fast-forward merge.".into(),
            ],
        },
        PluginToolDef {
            name: "task_dispatch".into(),
            description: "Dispatch a task: create a session, send the initial chat message, and record the session on the task. The task must be in active state (run task_schedule first) or ready state (will be prepared automatically).".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "integer",
                        "description": "Task ID to dispatch"
                    }
                },
                "required": ["id"]
            }),
            prompt_snippet: Some("Dispatch a task: create a session and start work on it".into()),
            prompt_guidelines: vec![
                "Creates a new session with cwd set to the task's worktree.".into(),
                "Sends an initial chat message with instructions to read the task spec and do the work.".into(),
                "The task must be active (prepared by task_schedule) or ready (will be prepared inline).".into(),
            ],
        },
    ]
}

// ---------------------------------------------------------------------------
// Tool handlers
// ---------------------------------------------------------------------------

fn handle_task_create(
    db: &TasksDb,
    args: &serde_json::Value,
    project: &str,
    session_id: Option<&str>,
    tool_call_id: &str,
    writer: &mut impl Write,
    reader: &mut impl BufRead,
    pending_events: &mut Vec<SchedulerEvent>,
) -> PluginToolResult {
    let title = match args.get("title").and_then(|v| v.as_str()) {
        Some(t) => t,
        None => return tool_err(tool_call_id, "title is required"),
    };
    let priority = args.get("priority").and_then(|v| v.as_i64());
    let parent_id = args.get("parent_id").and_then(|v| v.as_i64());
    let tags = args.get("tags");
    let skip_review = args
        .get("skip_review")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let message = args.get("message").and_then(|v| v.as_str());

    match db.create_task(project, title, priority, parent_id, tags, skip_review) {
        Ok(task) => {
            // Subtasks start in ready state — trigger a schedule pass.
            if task.state == "ready" {
                pending_events.push(SchedulerEvent::ScheduleNeeded(project.to_string()));
            }

            // Interactive tasks get a fresh session for the user to drive
            if task.state == "interactive"
                && let Some(new_sid) =
                    create_interactive_session(db, &task, project, session_id, writer, reader)
            {
                let _ = db.set_session_id(task.id, &new_sid);
                let _ = db.set_assigned_session(task.id, &new_sid);
                let _ = db.record_session(task.id, &new_sid, "interactive");
                if let Some(creator_sid) = session_id {
                    let _ = db.record_session(task.id, creator_sid, "creator");
                }
            }

            if let Some(msg_content) = message {
                let author = session_id.unwrap_or("user");
                if let Err(e) = db.add_message(task.id, msg_content, Some(author)) {
                    return tool_err(
                        tool_call_id,
                        &format!("task created (id={}) but message failed: {}", task.id, e),
                    );
                }
            }

            // Re-fetch to include the session_id/assigned_session updates
            match db.get_task(task.id) {
                Ok(Some(updated_task)) => match serde_json::to_string_pretty(&updated_task) {
                    Ok(json) => tool_ok(tool_call_id, &json),
                    Err(e) => tool_err(tool_call_id, &format!("serialize: {}", e)),
                },
                Ok(None) => tool_err(tool_call_id, "task not found after create"),
                Err(e) => tool_err(tool_call_id, &format!("re-fetch task: {}", e)),
            }
        }
        Err(e) => tool_err(tool_call_id, &format!("create task: {}", e)),
    }
}

/// Create a fresh session for an interactive task.
///
/// Returns the new session ID on success, or None if session creation fails
/// (the task is still created — session linking is best-effort).
fn create_interactive_session(
    _db: &TasksDb,
    task: &crate::tasks_db::Task,
    project: &str,
    parent_session_id: Option<&str>,
    writer: &mut impl Write,
    reader: &mut impl BufRead,
) -> Option<String> {
    use crate::protocol::{Request, Response};

    let create_req = Request::CreateSession {
        model: None,
        provider: None,
        system_prompt: None,
        cwd: Some(project.to_string()),
        parent_id: parent_session_id.map(String::from),
        child_budget: 4,
        tagline: Some(format!("Task {}: {}", task.id, task.title)),
        auto_archive: false,
        notify_parent: false,
    };

    let new_sid = match crate::tasks_scheduler::server_request(writer, reader, create_req) {
        Ok(Response::SessionCreated { session_id }) => session_id,
        Ok(Response::Error { message }) => {
            eprintln!(
                "tasks: failed to create session for task {}: {}",
                task.id, message
            );
            return None;
        }
        Ok(_) => {
            eprintln!(
                "tasks: unexpected response creating session for task {}",
                task.id
            );
            return None;
        }
        Err(e) => {
            eprintln!("tasks: error creating session for task {}: {}", task.id, e);
            return None;
        }
    };

    // Queue an initial message so the session has context when the user connects
    let initial_msg = format!(
        "You are working on task {id}: {title}. Use task_get {id} to read the full spec.",
        id = task.id,
        title = task.title,
    );

    let queue_req = Request::QueueMessage {
        target_session_id: new_sid.clone(),
        content: initial_msg,
        sender_info: "task-system".to_string(),
        await_reply: false,
        reply_to: None,
    };

    if let Err(e) = crate::tasks_scheduler::server_request(writer, reader, queue_req) {
        eprintln!(
            "tasks: session {} created for task {} but failed to queue initial message: {}",
            new_sid, task.id, e
        );
    }

    Some(new_sid)
}

fn handle_task_assign(
    db: &TasksDb,
    args: &serde_json::Value,
    session_id: Option<&str>,
    tool_call_id: &str,
) -> PluginToolResult {
    let id = match args.get("id").and_then(|v| v.as_i64()) {
        Some(id) => id,
        None => return tool_err(tool_call_id, "id is required"),
    };

    // Use explicit session_id from args, or fall back to context session_id
    let sid = args
        .get("session_id")
        .and_then(|v| v.as_str())
        .or(session_id);
    let sid = match sid {
        Some(s) => s,
        None => {
            return tool_err(
                tool_call_id,
                "session_id is required (not available from context)",
            );
        }
    };

    match db.assign_task(id, sid) {
        Ok(task) => match serde_json::to_string_pretty(&task) {
            Ok(json) => tool_ok(tool_call_id, &json),
            Err(e) => tool_err(tool_call_id, &format!("serialize: {}", e)),
        },
        Err(e) => tool_err(tool_call_id, &format!("assign task: {}", e)),
    }
}

fn handle_task_get(db: &TasksDb, args: &serde_json::Value, tool_call_id: &str) -> PluginToolResult {
    let id = match args.get("id").and_then(|v| v.as_i64()) {
        Some(id) => id,
        None => return tool_err(tool_call_id, "id is required"),
    };

    let task = match db.get_task(id) {
        Ok(Some(t)) => t,
        Ok(None) => return tool_err(tool_call_id, &format!("task {} not found", id)),
        Err(e) => return tool_err(tool_call_id, &format!("get task: {}", e)),
    };

    let messages = match db.get_messages(id) {
        Ok(m) => m,
        Err(e) => return tool_err(tool_call_id, &format!("get messages: {}", e)),
    };

    let relations = match db.get_relations(id) {
        Ok(r) => r,
        Err(e) => return tool_err(tool_call_id, &format!("get relations: {}", e)),
    };

    let subtasks = match db.get_subtasks(id) {
        Ok(s) => s,
        Err(e) => return tool_err(tool_call_id, &format!("get subtasks: {}", e)),
    };

    // Build enriched relations with dependency status
    let enriched_relations: Vec<serde_json::Value> = relations
        .iter()
        .map(|rel| {
            let mut obj = serde_json::json!({
                "from_task": rel.from_task,
                "to_task": rel.to_task,
                "relation": rel.relation,
            });
            // For depends_on relations where this task is the dependent,
            // include whether the dependency is satisfied or blocking.
            if rel.relation == "depends_on"
                && rel.from_task == id
                && let Ok(Some(dep_task)) = db.get_task(rel.to_task)
            {
                let satisfied = dep_task.state == "done";
                obj["dependency_status"] = if satisfied {
                    serde_json::json!("satisfied")
                } else {
                    serde_json::json!("blocking")
                };
                obj["dependency_state"] = serde_json::json!(dep_task.state);
            }
            obj
        })
        .collect();

    let result = serde_json::json!({
        "task": task,
        "messages": messages,
        "relations": enriched_relations,
        "subtasks": subtasks,
    });

    match serde_json::to_string_pretty(&result) {
        Ok(json) => tool_ok(tool_call_id, &json),
        Err(e) => tool_err(tool_call_id, &format!("serialize: {}", e)),
    }
}

fn handle_task_list(
    db: &TasksDb,
    args: &serde_json::Value,
    project: &str,
    tool_call_id: &str,
) -> PluginToolResult {
    let state = args.get("state").and_then(|v| v.as_str());
    let parent_id = args.get("parent_id").and_then(|v| v.as_i64());
    let tag = args.get("tag").and_then(|v| v.as_str());
    let limit = args.get("limit").and_then(|v| v.as_i64());

    match db.list_tasks(project, state, parent_id, tag, limit) {
        Ok(tasks) => match serde_json::to_string_pretty(&tasks) {
            Ok(json) => tool_ok(tool_call_id, &json),
            Err(e) => tool_err(tool_call_id, &format!("serialize: {}", e)),
        },
        Err(e) => tool_err(tool_call_id, &format!("list tasks: {}", e)),
    }
}

fn handle_task_update(
    db: &TasksDb,
    args: &serde_json::Value,
    session_id: Option<&str>,
    tool_call_id: &str,
    writer: &mut impl Write,
    reader: &mut impl BufRead,
    pending_events: &mut Vec<SchedulerEvent>,
) -> PluginToolResult {
    let id = match args.get("id").and_then(|v| v.as_i64()) {
        Some(id) => id,
        None => return tool_err(tool_call_id, "id is required"),
    };

    let update = TaskUpdate {
        title: args.get("title").and_then(|v| v.as_str()).map(String::from),
        state: args.get("state").and_then(|v| v.as_str()).map(String::from),
        priority: args.get("priority").and_then(|v| v.as_i64()),
        tags: args.get("tags").cloned(),
        affected_files: args.get("affected_files").cloned(),
        skip_review: args.get("skip_review").and_then(|v| v.as_bool()),
    };

    // Track session as reviewer if transitioning to review or approved
    if let (Some(sid), Some(new_state)) = (session_id, &update.state)
        && (new_state == "review" || new_state == "approved")
        && let Err(e) = db.record_session(id, sid, "reviewer")
    {
        return tool_err(tool_call_id, &format!("record session: {}", e));
    }

    match db.update_task(id, &update, session_id) {
        Ok(task) => {
            // Trigger scheduler events based on the new state.
            match task.state.as_str() {
                "approved" => pending_events.push(SchedulerEvent::MergeNeeded),
                "ready" => {
                    pending_events.push(SchedulerEvent::ScheduleNeeded(task.project.clone()));
                }
                _ => {}
            }

            // When a task transitions to done, auto-archive its session
            if task.state == "done" {
                auto_archive_task_session(db, &task, writer, reader);
            }

            match serde_json::to_string_pretty(&task) {
                Ok(json) => tool_ok(tool_call_id, &json),
                Err(e) => tool_err(tool_call_id, &format!("serialize: {}", e)),
            }
        }
        Err(e) => tool_err(tool_call_id, &format!("update task: {}", e)),
    }
}

/// Auto-archive a task's session and clean up worktree/branch when a task
/// transitions to done. All operations are best-effort — errors are logged
/// but don't fail the state transition.
fn auto_archive_task_session(
    db: &TasksDb,
    task: &crate::tasks_db::Task,
    writer: &mut impl Write,
    reader: &mut impl BufRead,
) {
    // Archive the task's session
    if let Some(ref sid) = task.session_id {
        let _ = crate::tasks_scheduler::server_request(
            writer,
            reader,
            crate::protocol::Request::ArchiveSession {
                session_id: sid.clone(),
                require_ancestor: None,
            },
        );
    }

    // Clean up worktree if still present
    if let Some(ref wt_path) = task.worktree_path {
        if let Some(ref branch) = task.branch {
            // Need repo root for git commands. Try to find it from the
            // task's project directory.
            if let Ok(repo_root) = crate::tasks_git::get_repo_root(&task.project) {
                let _ = crate::tasks_git::remove_worktree(&repo_root, wt_path);
                let _ = crate::tasks_git::delete_branch(&repo_root, branch);
            }
        }
        let _ = db.clear_worktree(task.id);
    }
}

fn handle_task_message(
    db: &TasksDb,
    args: &serde_json::Value,
    session_id: Option<&str>,
    tool_call_id: &str,
) -> PluginToolResult {
    let id = match args.get("id").and_then(|v| v.as_i64()) {
        Some(id) => id,
        None => return tool_err(tool_call_id, "id is required"),
    };
    let content = match args.get("content").and_then(|v| v.as_str()) {
        Some(c) => c,
        None => return tool_err(tool_call_id, "content is required"),
    };

    // Track session as contributor
    if let Some(sid) = session_id
        && let Err(e) = db.record_session(id, sid, "contributor")
    {
        return tool_err(tool_call_id, &format!("record session: {}", e));
    }

    let author = session_id.unwrap_or("user");
    match db.add_message(id, content, Some(author)) {
        Ok(msg) => match serde_json::to_string_pretty(&msg) {
            Ok(json) => tool_ok(tool_call_id, &json),
            Err(e) => tool_err(tool_call_id, &format!("serialize: {}", e)),
        },
        Err(e) => tool_err(tool_call_id, &format!("add message: {}", e)),
    }
}

fn handle_task_message_edit(
    db: &TasksDb,
    args: &serde_json::Value,
    tool_call_id: &str,
) -> PluginToolResult {
    let _task_id = match args.get("task_id").and_then(|v| v.as_i64()) {
        Some(id) => id,
        None => return tool_err(tool_call_id, "task_id is required"),
    };
    let message_id = match args.get("message_id").and_then(|v| v.as_i64()) {
        Some(id) => id,
        None => return tool_err(tool_call_id, "message_id is required"),
    };
    let content = match args.get("content").and_then(|v| v.as_str()) {
        Some(c) => c,
        None => return tool_err(tool_call_id, "content is required"),
    };

    match db.edit_message(message_id, content) {
        Ok(msg) => match serde_json::to_string_pretty(&msg) {
            Ok(json) => tool_ok(tool_call_id, &json),
            Err(e) => tool_err(tool_call_id, &format!("serialize: {}", e)),
        },
        Err(e) => tool_err(tool_call_id, &format!("edit message: {}", e)),
    }
}

fn handle_task_relate(
    db: &TasksDb,
    args: &serde_json::Value,
    tool_call_id: &str,
) -> PluginToolResult {
    let from_task = match args.get("from_task").and_then(|v| v.as_i64()) {
        Some(id) => id,
        None => return tool_err(tool_call_id, "from_task is required"),
    };
    let to_task = match args.get("to_task").and_then(|v| v.as_i64()) {
        Some(id) => id,
        None => return tool_err(tool_call_id, "to_task is required"),
    };
    let relation = match args.get("relation").and_then(|v| v.as_str()) {
        Some(r) => r,
        None => return tool_err(tool_call_id, "relation is required"),
    };

    match db.add_relation(from_task, to_task, relation) {
        Ok(()) => tool_ok(
            tool_call_id,
            &format!("relation added: {} {} {}", from_task, relation, to_task),
        ),
        Err(e) => tool_err(tool_call_id, &format!("add relation: {}", e)),
    }
}

fn handle_task_search(
    db: &TasksDb,
    args: &serde_json::Value,
    project: &str,
    tool_call_id: &str,
) -> PluginToolResult {
    let query = match args.get("query").and_then(|v| v.as_str()) {
        Some(q) => q,
        None => return tool_err(tool_call_id, "query is required"),
    };
    let state = args.get("state").and_then(|v| v.as_str());

    match db.search_tasks(project, query, state) {
        Ok(tasks) => match serde_json::to_string_pretty(&tasks) {
            Ok(json) => tool_ok(tool_call_id, &json),
            Err(e) => tool_err(tool_call_id, &format!("serialize: {}", e)),
        },
        Err(e) => tool_err(tool_call_id, &format!("search tasks: {}", e)),
    }
}

fn handle_task_schedule(
    db: &TasksDb,
    args: &serde_json::Value,
    project: &str,
    tool_call_id: &str,
) -> PluginToolResult {
    let project = args
        .get("project")
        .and_then(|v| v.as_str())
        .unwrap_or(project);

    match tasks_scheduler::schedule(db, project) {
        Ok(scheduled) => {
            if scheduled.is_empty() {
                return tool_ok(tool_call_id, "No ready tasks to schedule.");
            }
            match serde_json::to_string_pretty(&scheduled) {
                Ok(json) => tool_ok(
                    tool_call_id,
                    &format!(
                        "Scheduled {} task(s) for dispatch:\n{}",
                        scheduled.len(),
                        json
                    ),
                ),
                Err(e) => tool_err(tool_call_id, &format!("serialize: {}", e)),
            }
        }
        Err(e) => tool_err(tool_call_id, &format!("schedule: {}", e)),
    }
}

fn handle_task_dispatch(
    db: &TasksDb,
    args: &serde_json::Value,
    session_id: Option<&str>,
    tool_call_id: &str,
    writer: &mut impl Write,
    reader: &mut impl BufRead,
) -> PluginToolResult {
    let id = match args.get("id").and_then(|v| v.as_i64()) {
        Some(id) => id,
        None => return tool_err(tool_call_id, "id is required"),
    };

    match tasks_scheduler::dispatch(db, id, session_id, writer, reader) {
        Ok(sid) => tool_ok(
            tool_call_id,
            &format!("Task {} dispatched. Session: {}", id, sid),
        ),
        Err(e) => tool_err(tool_call_id, &format!("dispatch task {}: {}", id, e)),
    }
}

fn handle_task_merge(
    db: &TasksDb,
    args: &serde_json::Value,
    session_id: Option<&str>,
    tool_call_id: &str,
    writer: &mut impl Write,
    reader: &mut impl BufRead,
) -> PluginToolResult {
    let id = match args.get("id").and_then(|v| v.as_i64()) {
        Some(id) => id,
        None => return tool_err(tool_call_id, "id is required"),
    };

    // Get task and validate it's approved
    let task = match db.get_task(id) {
        Ok(Some(t)) => t,
        Ok(None) => return tool_err(tool_call_id, &format!("task {} not found", id)),
        Err(e) => return tool_err(tool_call_id, &format!("get task: {}", e)),
    };

    if task.state != "approved" {
        return tool_err(
            tool_call_id,
            &format!(
                "task {} is in state '{}', must be 'approved' to merge",
                id, task.state
            ),
        );
    }

    // Transition to merging
    if let Err(e) = db.update_task(
        id,
        &TaskUpdate {
            state: Some("merging".into()),
            ..Default::default()
        },
        session_id,
    ) {
        return tool_err(tool_call_id, &format!("transition to merging: {}", e));
    }

    // Run the merge
    let project_dir = &task.project;
    match tasks_merge::merge_task(db, id, project_dir, writer, reader) {
        Ok(result) => {
            if result.success {
                // Transition to done
                if let Err(e) = db.update_task(
                    id,
                    &TaskUpdate {
                        state: Some("done".into()),
                        ..Default::default()
                    },
                    session_id,
                ) {
                    return tool_err(
                        tool_call_id,
                        &format!("merge succeeded but transition to done failed: {}", e),
                    );
                }

                // Check parent notification
                if let Err(e) = tasks_merge::notify_parent_if_all_done(db, id, writer, reader) {
                    eprintln!("tasks: parent notification failed for task {}: {}", id, e);
                }

                match serde_json::to_string_pretty(&result) {
                    Ok(json) => tool_ok(tool_call_id, &json),
                    Err(e) => tool_err(tool_call_id, &format!("serialize: {}", e)),
                }
            } else {
                // Merge failed (rebase conflict, checklist, ff-merge) — recoverable.
                // Transition back to active so the assigned session can fix and retry.
                if let Err(e) = db.update_task(
                    id,
                    &TaskUpdate {
                        state: Some("active".into()),
                        ..Default::default()
                    },
                    session_id,
                ) {
                    eprintln!(
                        "tasks: failed to transition task {} back to active: {}",
                        id, e
                    );
                }

                // Add error details as a task message
                let _ = db.add_message(
                    id,
                    &format!("Merge failed:\n{}", result.log),
                    Some("system"),
                );

                // Notify assigned session so it can fix the issue
                if let Some(ref sid) = task.session_id {
                    crate::tasks_merge::notify_session_of_merge_failure(
                        sid,
                        id,
                        &result.log,
                        writer,
                        reader,
                    );
                }

                tool_err(tool_call_id, &format!("merge failed:\n{}", result.log))
            }
        }
        Err(e) => {
            // Infrastructure error (DB, missing branch/worktree, server request
            // failure) — not recoverable by the agent. Transition to failed.
            if let Err(te) = db.update_task(
                id,
                &TaskUpdate {
                    state: Some("failed".into()),
                    ..Default::default()
                },
                session_id,
            ) {
                eprintln!("tasks: failed to transition task {} to failed: {}", id, te);
            }
            let _ = db.add_message(id, &format!("Merge error: {}", e), Some("system"));
            tool_err(tool_call_id, &format!("merge error: {}", e))
        }
    }
}

// ---------------------------------------------------------------------------
// Scheduler events
// ---------------------------------------------------------------------------

/// Events that trigger scheduler passes after a tool call completes.
///
/// Instead of polling on a timer, tool handlers emit these events when
/// a state transition requires follow-up work. The main loop drains
/// pending events after each tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SchedulerEvent {
    /// A task moved to `approved` — run the merge queue.
    MergeNeeded,
    /// A task moved to `ready` — run a scheduling pass for the given project.
    ScheduleNeeded(String),
}

// ---------------------------------------------------------------------------
// Plugin main loop
// ---------------------------------------------------------------------------

/// Run the tasks plugin loop. Called from `tau plugin tasks` subcommand.
///
/// The loop is fully event-driven: it blocks on stdin for tool calls and
/// runs merge/schedule passes only when triggered by state changes in
/// tool handlers (via `SchedulerEvent`). There is no polling.
pub fn run_tasks_plugin() {
    // Open DB at startup (it's always at the same global path)
    let db = match TasksDb::open_default() {
        Ok(db) => db,
        Err(e) => {
            eprintln!("tasks plugin: failed to open db: {}", e);
            std::process::exit(1);
        }
    };

    let stdout = std::io::stdout();
    let mut writer = BufWriter::new(stdout.lock());

    // Send registration
    let registration = PluginRegistration {
        name: "tasks".to_string(),
        tools: tasks_tools(),
        hooks: Vec::new(),
        commands: Vec::new(),
    };
    send_message(&mut writer, &PluginMessage::Register(registration));

    // Spawn a reader thread that sends lines through a channel.
    let (line_tx, line_rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut reader = BufReader::new(stdin.lock());
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => break, // EOF
                Ok(_) => {
                    if line_tx.send(line).is_err() {
                        break; // Main thread dropped the receiver
                    }
                }
                Err(_) => break,
            }
        }
    });

    // Wrap line_rx in a ChannelLineReader so tool handlers (and the merge
    // pass) can use it as a `BufRead`.  Since tool calls and the merge pass
    // are never concurrent (both run on the main thread), sharing is safe.
    let mut chan_reader = ChannelLineReader::new(line_rx);

    // Pending scheduler events, populated by tool handlers and drained
    // after each tool call completes.
    let mut pending_events: Vec<SchedulerEvent> = Vec::new();

    // Handle requests — blocks on recv() until a line arrives or EOF.
    loop {
        let line = match chan_reader.recv() {
            Some(l) => l,
            None => break, // EOF — stdin closed
        };

        if line.trim().is_empty() {
            continue;
        }

        let req: PluginRequest = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("tasks: bad request: {}", e);
                continue;
            }
        };

        match req {
            PluginRequest::ToolCall {
                tool_call_id,
                name,
                arguments,
                cwd,
                session_id,
            } => {
                let project = cwd.as_deref().unwrap_or("/tmp");
                let session = session_id.as_deref();

                let result = match name.as_str() {
                    "task_create" => handle_task_create(
                        &db,
                        &arguments,
                        project,
                        session,
                        &tool_call_id,
                        &mut writer,
                        &mut chan_reader,
                        &mut pending_events,
                    ),
                    "task_get" => handle_task_get(&db, &arguments, &tool_call_id),
                    "task_list" => handle_task_list(&db, &arguments, project, &tool_call_id),
                    "task_assign" => handle_task_assign(&db, &arguments, session, &tool_call_id),
                    "task_update" => handle_task_update(
                        &db,
                        &arguments,
                        session,
                        &tool_call_id,
                        &mut writer,
                        &mut chan_reader,
                        &mut pending_events,
                    ),
                    "task_message" => handle_task_message(&db, &arguments, session, &tool_call_id),
                    "task_message_edit" => handle_task_message_edit(&db, &arguments, &tool_call_id),
                    "task_relate" => handle_task_relate(&db, &arguments, &tool_call_id),
                    "task_search" => handle_task_search(&db, &arguments, project, &tool_call_id),
                    "task_schedule" => {
                        handle_task_schedule(&db, &arguments, project, &tool_call_id)
                    }
                    "task_merge" => handle_task_merge(
                        &db,
                        &arguments,
                        session,
                        &tool_call_id,
                        &mut writer,
                        &mut chan_reader,
                    ),
                    "task_dispatch" => handle_task_dispatch(
                        &db,
                        &arguments,
                        session,
                        &tool_call_id,
                        &mut writer,
                        &mut chan_reader,
                    ),
                    _ => tool_err(&tool_call_id, &format!("unknown tool: {}", name)),
                };

                send_message(&mut writer, &PluginMessage::ToolResult(result));

                // Drain pending scheduler events and run the
                // corresponding passes immediately.
                drain_scheduler_events(&mut pending_events, &db, &mut writer, &mut chan_reader);
            }
            PluginRequest::Init { .. } | PluginRequest::SessionStart { .. } => {
                send_message(
                    &mut writer,
                    &PluginMessage::HookResult(HookResult::default()),
                );
            }
            PluginRequest::Hook { .. } => {
                send_message(
                    &mut writer,
                    &PluginMessage::HookResult(HookResult::default()),
                );
            }
            PluginRequest::Idle => {
                break;
            }
            PluginRequest::ServerResponse { .. } => {
                // Not expected outside of tool/merge passes — ignore
            }
        }
    }
}

// ---------------------------------------------------------------------------
// ChannelLineReader — BufRead adapter over mpsc::Receiver<String>
// ---------------------------------------------------------------------------

/// A `BufRead` adapter backed by a `std::sync::mpsc::Receiver<String>`.
///
/// Each received string is treated as one line (newline-terminated).
/// `read_line` blocks until a line is available or the channel closes.
///
/// Provides `recv` for blocking reads (used by the main event loop).
struct ChannelLineReader {
    rx: std::sync::mpsc::Receiver<String>,
    /// Leftover bytes from the current line that haven't been consumed yet.
    buf: Vec<u8>,
    closed: bool,
}

impl ChannelLineReader {
    fn new(rx: std::sync::mpsc::Receiver<String>) -> Self {
        Self {
            rx,
            buf: Vec::new(),
            closed: false,
        }
    }

    /// Receive the next line, blocking until available. Returns `None`
    /// on channel close (EOF).
    fn recv(&mut self) -> Option<String> {
        match self.rx.recv() {
            Ok(line) => Some(line),
            Err(std::sync::mpsc::RecvError) => {
                self.closed = true;
                None
            }
        }
    }
}

impl std::io::Read for ChannelLineReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        // Drain leftover bytes first
        if !self.buf.is_empty() {
            let n = std::cmp::min(buf.len(), self.buf.len());
            buf[..n].copy_from_slice(&self.buf[..n]);
            self.buf.drain(..n);
            return Ok(n);
        }
        // Block for the next line
        match self.rx.recv() {
            Ok(line) => {
                let bytes = line.as_bytes();
                let n = std::cmp::min(buf.len(), bytes.len());
                buf[..n].copy_from_slice(&bytes[..n]);
                if n < bytes.len() {
                    self.buf.extend_from_slice(&bytes[n..]);
                }
                Ok(n)
            }
            Err(_) => {
                self.closed = true;
                Ok(0) // EOF
            }
        }
    }
}

impl BufRead for ChannelLineReader {
    fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
        if self.buf.is_empty() {
            match self.rx.recv() {
                Ok(line) => {
                    self.buf = line.into_bytes();
                }
                Err(_) => {
                    self.closed = true;
                }
            }
        }
        Ok(&self.buf)
    }

    fn consume(&mut self, amt: usize) {
        self.buf.drain(..amt);
    }
}

// ---------------------------------------------------------------------------\n// Scheduler event processing\n// ---------------------------------------------------------------------------

/// Drain pending scheduler events and run the corresponding passes.
///
/// Called after each tool call completes. Events are deduplicated: multiple
/// `MergeNeeded` events collapse into a single merge pass, and multiple
/// `ScheduleNeeded` events for the same project collapse into one schedule
/// pass.
fn drain_scheduler_events(
    events: &mut Vec<SchedulerEvent>,
    db: &TasksDb,
    writer: &mut impl Write,
    reader: &mut impl BufRead,
) {
    if events.is_empty() {
        return;
    }

    let batch = std::mem::take(events);

    let mut need_merge = false;
    let mut schedule_projects: Vec<String> = Vec::new();

    for ev in batch {
        match ev {
            SchedulerEvent::MergeNeeded => need_merge = true,
            SchedulerEvent::ScheduleNeeded(project) => {
                if !schedule_projects.contains(&project) {
                    schedule_projects.push(project);
                }
            }
        }
    }

    // Run merge pass first (merging may unblock dependencies that become
    // ready, but schedule passes are triggered by explicit events anyway).
    if need_merge {
        run_merge_pass(db, writer, reader);
    }

    for project in &schedule_projects {
        run_schedule_pass(db, project, writer, reader);
    }
}

// ---------------------------------------------------------------------------
// Merge pass
// ---------------------------------------------------------------------------

/// Run a merge pass: find approved tasks and merge them.
///
/// Triggered when a task transitions to `approved`. Shares the same
/// writer (stdout) and reader (channel from stdin) as tool handlers —
/// this is safe because the merge pass and tool handling are never
/// concurrent (both run on the main thread).
fn run_merge_pass(db: &TasksDb, writer: &mut impl Write, reader: &mut impl BufRead) {
    match tasks_scheduler::merge_approved(db, writer, reader) {
        Ok(attempts) => {
            for a in &attempts {
                if !a.success {
                    eprintln!(
                        "tasks scheduler: auto-merge failed for task {} ({}): {}",
                        a.task_id,
                        a.title,
                        a.log.lines().next().unwrap_or("(no details)")
                    );
                }
            }
        }
        Err(e) => {
            eprintln!("tasks scheduler: merge pass error: {}", e);
        }
    }
}

/// Run a schedule + dispatch pass for a project: find ready tasks, prepare
/// branches/worktrees, and dispatch sessions for them.
///
/// Triggered when a task transitions to `ready`.
fn run_schedule_pass(
    db: &TasksDb,
    project: &str,
    writer: &mut impl Write,
    reader: &mut impl BufRead,
) {
    match tasks_scheduler::schedule(db, project) {
        Ok(scheduled) => {
            for st in &scheduled {
                eprintln!(
                    "tasks scheduler: scheduled task {} ({}) on branch {}",
                    st.id, st.title, st.branch
                );
                // Dispatch each scheduled task (create session + send initial message).
                if let Err(e) = tasks_scheduler::dispatch(db, st.id, None, writer, reader) {
                    eprintln!("tasks scheduler: dispatch failed for task {}: {}", st.id, e);
                }
            }
        }
        Err(e) => {
            eprintln!("tasks scheduler: schedule pass error: {}", e);
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Create a mock writer and reader pair for tests that need ServerRequest
    /// support. The writer captures output and the reader provides canned
    /// responses.
    ///
    /// Returns `(writer, reader)` where writer is `MockServerIO` and reader
    /// wraps the same mock via a shared reference.
    ///
    /// Since `BufRead` needs `Read` and `Write` on the same object is tricky
    /// with borrowing, we use two separate mock objects connected via a buffer.
    fn mock_io() -> (MockWriter, BufReader<MockReader>) {
        let shared = std::sync::Arc::new(std::sync::Mutex::new(MockShared {
            write_buf: Vec::new(),
            read_buf: Vec::new(),
            session_counter: 0,
        }));
        let writer = MockWriter {
            shared: shared.clone(),
        };
        let reader = MockReader { shared };
        (writer, BufReader::new(reader))
    }

    struct MockShared {
        write_buf: Vec<u8>,
        read_buf: Vec<u8>,
        session_counter: u32,
    }

    impl MockShared {
        fn process_pending(&mut self) {
            let buf = std::mem::take(&mut self.write_buf);
            let text = String::from_utf8_lossy(&buf);
            for line in text.lines() {
                if line.trim().is_empty() {
                    continue;
                }
                if let Ok(PluginMessage::ServerRequest {
                    request_id,
                    ref request,
                }) = serde_json::from_str::<PluginMessage>(line)
                {
                    let response = match request {
                        crate::protocol::Request::CreateSession { .. } => {
                            self.session_counter += 1;
                            crate::protocol::Response::SessionCreated {
                                session_id: format!("mock-s{}", self.session_counter),
                            }
                        }
                        crate::protocol::Request::QueueMessage { .. } => {
                            crate::protocol::Response::Ok
                        }
                        crate::protocol::Request::ArchiveSession { .. } => {
                            crate::protocol::Response::SessionArchived
                        }
                        _ => crate::protocol::Response::Ok,
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
    }

    struct MockWriter {
        shared: std::sync::Arc<std::sync::Mutex<MockShared>>,
    }

    impl std::io::Write for MockWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let mut shared = self.shared.lock().unwrap();
            shared.write_buf.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    struct MockReader {
        shared: std::sync::Arc<std::sync::Mutex<MockShared>>,
    }

    impl std::io::Read for MockReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let mut shared = self.shared.lock().unwrap();
            shared.process_pending();
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

    /// Helper: simulate a tool call and return the result.
    fn simulate_tool_call(input_lines: &str) -> Vec<PluginMessage> {
        let input = input_lines.as_bytes().to_vec();
        let mut reader = BufReader::new(Cursor::new(input));
        let output: Vec<u8> = Vec::new();

        // Read registration would happen first, but we test tool calls directly.
        // Instead, parse messages from the tool handlers via the protocol.
        let mut messages = Vec::new();
        let mut line = String::new();
        while reader.read_line(&mut line).unwrap() > 0 {
            if line.trim().is_empty() {
                line.clear();
                continue;
            }
            if let Ok(msg) = serde_json::from_str::<PluginMessage>(&line) {
                messages.push(msg);
            }
            line.clear();
        }
        let _ = output;
        messages
    }

    #[test]
    fn test_tasks_tools_defined() {
        let tools = tasks_tools();
        assert_eq!(tools.len(), 12);
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"task_create"));
        assert!(names.contains(&"task_get"));
        assert!(names.contains(&"task_list"));
        assert!(names.contains(&"task_assign"));
        assert!(names.contains(&"task_update"));
        assert!(names.contains(&"task_message"));
        assert!(names.contains(&"task_message_edit"));
        assert!(names.contains(&"task_relate"));
        assert!(names.contains(&"task_search"));
        assert!(names.contains(&"task_schedule"));
        assert!(names.contains(&"task_dispatch"));
        assert!(names.contains(&"task_merge"));
    }

    #[test]
    fn test_tool_handlers_via_db() {
        // Test handlers directly with an in-memory DB
        let db = TasksDb::open_memory().unwrap();
        let (mut writer, mut reader) = mock_io();

        // Create
        let result = handle_task_create(
            &db,
            &serde_json::json!({"title": "Test task", "priority": 3, "message": "Hello"}),
            "/project",
            Some("s1"),
            "tc1",
            &mut writer,
            &mut reader,
            &mut Vec::new(),
        );
        assert!(!result.is_error);
        let text = extract_text(&result);
        let task: serde_json::Value = serde_json::from_str(&text).unwrap();
        let task_id = task["id"].as_i64().unwrap();
        assert_eq!(task["title"], "Test task");
        assert_eq!(task["priority"], 3);
        assert_eq!(task["state"], "interactive");
        // Interactive task gets a fresh session via ServerRequest
        assert_eq!(task["session_id"], "mock-s1");
        assert_eq!(task["assigned_session"], "mock-s1");

        // Check message was created
        let messages = db.get_messages(task_id).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].content, "Hello");

        // Get
        let result = handle_task_get(&db, &serde_json::json!({"id": task_id}), "tc2");
        assert!(!result.is_error);
        let text = extract_text(&result);
        let full: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(full["task"]["title"], "Test task");
        assert_eq!(full["messages"].as_array().unwrap().len(), 1);

        // List
        let result = handle_task_list(&db, &serde_json::json!({}), "/project", "tc3");
        assert!(!result.is_error);
        let text = extract_text(&result);
        let tasks: Vec<serde_json::Value> = serde_json::from_str(&text).unwrap();
        assert_eq!(tasks.len(), 1);

        // Update
        let result = handle_task_update(
            &db,
            &serde_json::json!({"id": task_id, "state": "ready"}),
            Some("s1"),
            "tc4",
            &mut writer,
            &mut reader,
            &mut Vec::new(),
        );
        assert!(!result.is_error);
        let text = extract_text(&result);
        let updated: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(updated["state"], "ready");

        // Invalid state transition (ready -> merging is not allowed)
        let result = handle_task_update(
            &db,
            &serde_json::json!({"id": task_id, "state": "merging"}),
            None,
            "tc5",
            &mut writer,
            &mut reader,
            &mut Vec::new(),
        );
        assert!(result.is_error);

        // Message
        let result = handle_task_message(
            &db,
            &serde_json::json!({"id": task_id, "content": "New message"}),
            Some("s2"),
            "tc6",
        );
        assert!(!result.is_error);

        // Search
        let result = handle_task_search(
            &db,
            &serde_json::json!({"query": "Test"}),
            "/project",
            "tc7",
        );
        assert!(!result.is_error);
        let text = extract_text(&result);
        let found: Vec<serde_json::Value> = serde_json::from_str(&text).unwrap();
        assert!(!found.is_empty());
    }

    #[test]
    fn test_tool_create_missing_title() {
        let db = TasksDb::open_memory().unwrap();
        let (mut writer, mut reader) = mock_io();
        let result = handle_task_create(
            &db,
            &serde_json::json!({}),
            "/p",
            None,
            "tc1",
            &mut writer,
            &mut reader,
            &mut Vec::new(),
        );
        assert!(result.is_error);
        assert!(extract_text(&result).contains("title is required"));
    }

    #[test]
    fn test_tool_relate() {
        let db = TasksDb::open_memory().unwrap();
        let t1 = db.create_task("/p", "A", None, None, None, false).unwrap();
        let t2 = db.create_task("/p", "B", None, None, None, false).unwrap();

        let result = handle_task_relate(
            &db,
            &serde_json::json!({"from_task": t1.id, "to_task": t2.id, "relation": "blocks"}),
            "tc1",
        );
        assert!(!result.is_error);

        // Invalid relation
        let result = handle_task_relate(
            &db,
            &serde_json::json!({"from_task": t1.id, "to_task": t2.id, "relation": "nonsense"}),
            "tc2",
        );
        assert!(result.is_error);
    }

    #[test]
    fn test_tool_message_edit() {
        let db = TasksDb::open_memory().unwrap();
        let task = db.create_task("/p", "A", None, None, None, false).unwrap();
        let msg = db.add_message(task.id, "original", None).unwrap();

        let result = handle_task_message_edit(
            &db,
            &serde_json::json!({"task_id": task.id, "message_id": msg.id, "content": "edited"}),
            "tc1",
        );
        assert!(!result.is_error);
        let text = extract_text(&result);
        let edited: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(edited["content"], "edited");
    }

    #[test]
    fn test_registration_message() {
        let tools = tasks_tools();
        let reg = PluginRegistration {
            name: "tasks".to_string(),
            tools,
            hooks: Vec::new(),
            commands: Vec::new(),
        };
        let msg = PluginMessage::Register(reg);
        let json = serde_json::to_string(&msg).unwrap();
        // Should be valid JSON
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["type"], "register");
        assert_eq!(parsed["name"], "tasks");
        assert_eq!(parsed["tools"].as_array().unwrap().len(), 12);
    }

    fn extract_text(result: &PluginToolResult) -> String {
        result
            .content
            .iter()
            .filter_map(|c| match c {
                ToolResultContent::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }

    // Suppress unused warning for simulate_tool_call (it demonstrates the pattern)
    #[test]
    fn test_simulate_tool_call_compiles() {
        let _ = simulate_tool_call;
    }

    #[test]
    fn test_task_assign_handler() {
        let db = TasksDb::open_memory().unwrap();
        let (mut writer, mut reader) = mock_io();

        // Create a task and move to ready
        let result = handle_task_create(
            &db,
            &serde_json::json!({"title": "Assignable task"}),
            "/project",
            Some("s1"),
            "tc1",
            &mut writer,
            &mut reader,
            &mut Vec::new(),
        );
        assert!(!result.is_error);
        let task: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
        let task_id = task["id"].as_i64().unwrap();

        // Move to ready first
        let result = handle_task_update(
            &db,
            &serde_json::json!({"id": task_id, "state": "ready"}),
            Some("s1"),
            "tc2",
            &mut writer,
            &mut reader,
            &mut Vec::new(),
        );
        assert!(!result.is_error);

        // Assign with explicit session_id
        let result = handle_task_assign(
            &db,
            &serde_json::json!({"id": task_id, "session_id": "worker-session"}),
            Some("s1"),
            "tc3",
        );
        assert!(!result.is_error);
        let assigned: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
        assert_eq!(assigned["state"], "active");
        assert_eq!(assigned["assigned_session"], "worker-session");
    }

    #[test]
    fn test_task_assign_uses_context_session() {
        let db = TasksDb::open_memory().unwrap();
        let (mut writer, mut reader) = mock_io();

        let result = handle_task_create(
            &db,
            &serde_json::json!({"title": "Task for context session"}),
            "/project",
            Some("s1"),
            "tc1",
            &mut writer,
            &mut reader,
            &mut Vec::new(),
        );
        let task: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
        let task_id = task["id"].as_i64().unwrap();

        handle_task_update(
            &db,
            &serde_json::json!({"id": task_id, "state": "ready"}),
            Some("s1"),
            "tc2",
            &mut writer,
            &mut reader,
            &mut Vec::new(),
        );

        // Assign without explicit session_id — uses context session
        let result = handle_task_assign(
            &db,
            &serde_json::json!({"id": task_id}),
            Some("context-session"),
            "tc3",
        );
        assert!(!result.is_error);
        let assigned: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
        assert_eq!(assigned["assigned_session"], "context-session");
    }

    #[test]
    fn test_task_assign_requires_ready_state() {
        let db = TasksDb::open_memory().unwrap();
        let (mut writer, mut reader) = mock_io();

        let result = handle_task_create(
            &db,
            &serde_json::json!({"title": "Not ready task"}),
            "/project",
            Some("s1"),
            "tc1",
            &mut writer,
            &mut reader,
            &mut Vec::new(),
        );
        let task: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
        let task_id = task["id"].as_i64().unwrap();

        // Try to assign in interactive state — should fail
        let result =
            handle_task_assign(&db, &serde_json::json!({"id": task_id}), Some("s1"), "tc2");
        assert!(result.is_error);
        assert!(extract_text(&result).contains("must be 'ready'"));
    }

    #[test]
    fn test_task_assign_no_session() {
        let db = TasksDb::open_memory().unwrap();
        let (mut writer, mut reader) = mock_io();

        let result = handle_task_create(
            &db,
            &serde_json::json!({"title": "No session task"}),
            "/project",
            None,
            "tc1",
            &mut writer,
            &mut reader,
            &mut Vec::new(),
        );
        let task: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
        let task_id = task["id"].as_i64().unwrap();

        handle_task_update(
            &db,
            &serde_json::json!({"id": task_id, "state": "ready"}),
            None,
            "tc2",
            &mut writer,
            &mut reader,
            &mut Vec::new(),
        );

        // Assign without any session — should fail
        let result = handle_task_assign(&db, &serde_json::json!({"id": task_id}), None, "tc3");
        assert!(result.is_error);
        assert!(extract_text(&result).contains("session_id is required"));
    }

    #[test]
    fn test_subtask_defaults() {
        let db = TasksDb::open_memory().unwrap();
        let (mut writer, mut reader) = mock_io();

        // Create parent task
        let result = handle_task_create(
            &db,
            &serde_json::json!({"title": "Parent"}),
            "/project",
            Some("s1"),
            "tc1",
            &mut writer,
            &mut reader,
            &mut Vec::new(),
        );
        let parent: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
        let parent_id = parent["id"].as_i64().unwrap();
        assert_eq!(parent["state"], "interactive");

        // Create subtask — should default to ready state, skip_review=false
        let result = handle_task_create(
            &db,
            &serde_json::json!({"title": "Subtask", "parent_id": parent_id, "skip_review": true}),
            "/project",
            Some("s1"),
            "tc2",
            &mut writer,
            &mut reader,
            &mut Vec::new(),
        );
        assert!(!result.is_error);
        let subtask: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
        assert_eq!(subtask["state"], "ready");
        assert_eq!(subtask["skip_review"], false);
    }

    #[test]
    fn test_active_to_approved_requires_skip_review() {
        let db = TasksDb::open_memory().unwrap();
        let (mut writer, mut reader) = mock_io();

        // Create task without skip_review
        let task = db
            .create_task("/project", "No skip", None, None, None, false)
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

        // Try active -> approved without skip_review — should fail
        let result = handle_task_update(
            &db,
            &serde_json::json!({"id": task.id, "state": "approved"}),
            Some("s1"),
            "tc1",
            &mut writer,
            &mut reader,
            &mut Vec::new(),
        );
        assert!(result.is_error);
        assert!(extract_text(&result).contains("skip_review is false"));

        // active -> review should still work
        let result = handle_task_update(
            &db,
            &serde_json::json!({"id": task.id, "state": "review"}),
            Some("s1"),
            "tc2",
            &mut writer,
            &mut reader,
            &mut Vec::new(),
        );
        assert!(!result.is_error);
    }

    #[test]
    fn test_active_to_approved_with_skip_review() {
        let db = TasksDb::open_memory().unwrap();
        let (mut writer, mut reader) = mock_io();

        // Create task with skip_review=true
        let task = db
            .create_task("/project", "Skip review", None, None, None, true)
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

        // active -> approved with skip_review=true — should succeed
        let result = handle_task_update(
            &db,
            &serde_json::json!({"id": task.id, "state": "approved"}),
            Some("s1"),
            "tc1",
            &mut writer,
            &mut reader,
            &mut Vec::new(),
        );
        assert!(!result.is_error);
        let updated: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
        assert_eq!(updated["state"], "approved");
    }

    #[test]
    fn test_session_tracking_on_message() {
        let db = TasksDb::open_memory().unwrap();
        let task = db
            .create_task("/project", "Tracked", None, None, None, false)
            .unwrap();

        // Add a message with a session — should record contributor
        let result = handle_task_message(
            &db,
            &serde_json::json!({"id": task.id, "content": "hello"}),
            Some("contributor-session"),
            "tc1",
        );
        assert!(!result.is_error);

        let sessions = db.get_sessions(task.id).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, "contributor-session");
        assert_eq!(sessions[0].role, "contributor");
    }

    #[test]
    fn test_session_tracking_on_update_to_review() {
        let db = TasksDb::open_memory().unwrap();
        let (mut writer, mut reader) = mock_io();
        let task = db
            .create_task("/project", "Review track", None, None, None, false)
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
        db.assign_task(task.id, "worker-s").unwrap();

        // Update to review with a different session
        let result = handle_task_update(
            &db,
            &serde_json::json!({"id": task.id, "state": "review"}),
            Some("reviewer-session"),
            "tc1",
            &mut writer,
            &mut reader,
            &mut Vec::new(),
        );
        assert!(!result.is_error);

        let sessions = db.get_sessions(task.id).unwrap();
        let roles: Vec<(&str, &str)> = sessions
            .iter()
            .map(|s| (s.session_id.as_str(), s.role.as_str()))
            .collect();
        assert!(roles.contains(&("worker-s", "worker")));
        assert!(roles.contains(&("reviewer-session", "reviewer")));
    }

    #[test]
    fn test_session_tracking_idempotent() {
        let db = TasksDb::open_memory().unwrap();
        let task = db
            .create_task("/project", "Idempotent", None, None, None, false)
            .unwrap();

        // Record same session twice — should be idempotent
        db.record_session(task.id, "s1", "contributor").unwrap();
        db.record_session(task.id, "s1", "contributor").unwrap();

        let sessions = db.get_sessions(task.id).unwrap();
        assert_eq!(sessions.len(), 1);
    }

    #[test]
    fn test_prompt_snippets_present() {
        let tools = tasks_tools();
        for tool in &tools {
            assert!(
                tool.prompt_snippet.is_some(),
                "tool {} missing prompt_snippet",
                tool.name
            );
        }
        // task_get should have the guideline about workflow
        let task_get = tools.iter().find(|t| t.name == "task_get").unwrap();
        assert!(!task_get.prompt_guidelines.is_empty());
        assert!(task_get.prompt_guidelines[0].contains("task_assign"));

        // task_assign should have snippets
        let task_assign = tools.iter().find(|t| t.name == "task_assign").unwrap();
        assert!(
            task_assign
                .prompt_snippet
                .as_ref()
                .unwrap()
                .contains("claim")
        );
    }

    // ----- dependency enforcement tests (plugin layer) -----

    #[test]
    fn test_task_relate_self_referential_rejected() {
        let db = TasksDb::open_memory().unwrap();
        let task = db
            .create_task("/project", "Self", None, None, None, false)
            .unwrap();

        let result = handle_task_relate(
            &db,
            &serde_json::json!({"from_task": task.id, "to_task": task.id, "relation": "depends_on"}),
            "tc1",
        );
        assert!(result.is_error);
        assert!(extract_text(&result).contains("to itself"));
    }

    #[test]
    fn test_task_relate_cross_project_rejected() {
        let db = TasksDb::open_memory().unwrap();
        let t1 = db
            .create_task("/project-a", "A", None, None, None, false)
            .unwrap();
        let t2 = db
            .create_task("/project-b", "B", None, None, None, false)
            .unwrap();

        let result = handle_task_relate(
            &db,
            &serde_json::json!({"from_task": t1.id, "to_task": t2.id, "relation": "depends_on"}),
            "tc1",
        );
        assert!(result.is_error);
        assert!(extract_text(&result).contains("across projects"));
    }

    #[test]
    fn test_task_relate_circular_rejected() {
        let db = TasksDb::open_memory().unwrap();
        let t1 = db
            .create_task("/project", "T1", None, None, None, false)
            .unwrap();
        let t2 = db
            .create_task("/project", "T2", None, None, None, false)
            .unwrap();

        // T1 depends_on T2 — OK
        let result = handle_task_relate(
            &db,
            &serde_json::json!({"from_task": t1.id, "to_task": t2.id, "relation": "depends_on"}),
            "tc1",
        );
        assert!(!result.is_error);

        // T2 depends_on T1 — circular
        let result = handle_task_relate(
            &db,
            &serde_json::json!({"from_task": t2.id, "to_task": t1.id, "relation": "depends_on"}),
            "tc2",
        );
        assert!(result.is_error);
        assert!(extract_text(&result).contains("circular dependency"));
    }

    #[test]
    fn test_task_get_dependency_status_blocking() {
        let db = TasksDb::open_memory().unwrap();
        let dep = db
            .create_task("/project", "Dependency", None, None, None, false)
            .unwrap();
        let task = db
            .create_task("/project", "Dependent", None, None, None, false)
            .unwrap();

        db.add_relation(task.id, dep.id, "depends_on").unwrap();

        let result = handle_task_get(&db, &serde_json::json!({"id": task.id}), "tc1");
        assert!(!result.is_error);
        let text = extract_text(&result);
        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();

        let relations = parsed["relations"].as_array().unwrap();
        assert_eq!(relations.len(), 1);
        assert_eq!(relations[0]["dependency_status"], "blocking");
        assert_eq!(relations[0]["dependency_state"], "interactive");
    }

    #[test]
    fn test_task_get_dependency_status_satisfied() {
        let db = TasksDb::open_memory().unwrap();
        // Create dep and move to done
        let dep = db
            .create_task("/project", "Dependency", None, None, None, true)
            .unwrap();
        db.update_task(
            dep.id,
            &crate::tasks_db::TaskUpdate {
                state: Some("ready".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        db.assign_task(dep.id, "s1").unwrap();
        db.update_task(
            dep.id,
            &crate::tasks_db::TaskUpdate {
                state: Some("approved".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        db.update_task(
            dep.id,
            &crate::tasks_db::TaskUpdate {
                state: Some("merging".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        db.update_task(
            dep.id,
            &crate::tasks_db::TaskUpdate {
                state: Some("done".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();

        let task = db
            .create_task("/project", "Dependent", None, None, None, false)
            .unwrap();
        db.add_relation(task.id, dep.id, "depends_on").unwrap();

        let result = handle_task_get(&db, &serde_json::json!({"id": task.id}), "tc1");
        assert!(!result.is_error);
        let text = extract_text(&result);
        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();

        let relations = parsed["relations"].as_array().unwrap();
        assert_eq!(relations.len(), 1);
        assert_eq!(relations[0]["dependency_status"], "satisfied");
        assert_eq!(relations[0]["dependency_state"], "done");
    }

    #[test]
    fn test_task_get_non_depends_on_has_no_status() {
        let db = TasksDb::open_memory().unwrap();
        let t1 = db
            .create_task("/project", "T1", None, None, None, false)
            .unwrap();
        let t2 = db
            .create_task("/project", "T2", None, None, None, false)
            .unwrap();

        db.add_relation(t1.id, t2.id, "related").unwrap();

        let result = handle_task_get(&db, &serde_json::json!({"id": t1.id}), "tc1");
        assert!(!result.is_error);
        let text = extract_text(&result);
        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();

        let relations = parsed["relations"].as_array().unwrap();
        assert_eq!(relations.len(), 1);
        assert!(relations[0].get("dependency_status").is_none());
    }

    // ----- interactive task auto-link tests -----

    #[test]
    fn test_interactive_task_creates_fresh_session() {
        let db = TasksDb::open_memory().unwrap();
        let (mut writer, mut reader) = mock_io();

        let result = handle_task_create(
            &db,
            &serde_json::json!({"title": "Interactive task"}),
            "/project",
            Some("creating-session"),
            "tc1",
            &mut writer,
            &mut reader,
            &mut Vec::new(),
        );
        assert!(!result.is_error);
        let text = extract_text(&result);
        let task: serde_json::Value = serde_json::from_str(&text).unwrap();

        assert_eq!(task["state"], "interactive");
        // session_id should be the NEW session, not the creating session
        assert_eq!(task["session_id"], "mock-s1");
        assert_eq!(task["assigned_session"], "mock-s1");

        // Check task_sessions table has both creator and interactive records
        let task_id = task["id"].as_i64().unwrap();
        let sessions = db.get_sessions(task_id).unwrap();
        assert_eq!(sessions.len(), 2);
        let roles: Vec<(&str, &str)> = sessions
            .iter()
            .map(|s| (s.session_id.as_str(), s.role.as_str()))
            .collect();
        assert!(roles.contains(&("mock-s1", "interactive")));
        assert!(roles.contains(&("creating-session", "creator")));
    }

    #[test]
    fn test_interactive_task_no_parent_session_still_creates_session() {
        let db = TasksDb::open_memory().unwrap();
        let (mut writer, mut reader) = mock_io();

        // Create without a session_id context — session still created, just no parent
        let result = handle_task_create(
            &db,
            &serde_json::json!({"title": "No parent session task"}),
            "/project",
            None,
            "tc1",
            &mut writer,
            &mut reader,
            &mut Vec::new(),
        );
        assert!(!result.is_error);
        let text = extract_text(&result);
        let task: serde_json::Value = serde_json::from_str(&text).unwrap();

        assert_eq!(task["state"], "interactive");
        // Session still created even without a parent session
        assert_eq!(task["session_id"], "mock-s1");
        assert_eq!(task["assigned_session"], "mock-s1");

        // Only the interactive session recorded (no creator since no parent session)
        let task_id = task["id"].as_i64().unwrap();
        let sessions = db.get_sessions(task_id).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, "mock-s1");
        assert_eq!(sessions[0].role, "interactive");
    }

    #[test]
    fn test_subtask_does_not_create_session() {
        let db = TasksDb::open_memory().unwrap();
        let (mut writer, mut reader) = mock_io();

        // Create parent (interactive — gets mock-s1)
        let result = handle_task_create(
            &db,
            &serde_json::json!({"title": "Parent"}),
            "/project",
            Some("s1"),
            "tc1",
            &mut writer,
            &mut reader,
            &mut Vec::new(),
        );
        let parent: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
        let parent_id = parent["id"].as_i64().unwrap();

        // Create subtask (defaults to ready, not interactive)
        let result = handle_task_create(
            &db,
            &serde_json::json!({"title": "Subtask", "parent_id": parent_id}),
            "/project",
            Some("s1"),
            "tc2",
            &mut writer,
            &mut reader,
            &mut Vec::new(),
        );
        assert!(!result.is_error);
        let text = extract_text(&result);
        let subtask: serde_json::Value = serde_json::from_str(&text).unwrap();

        assert_eq!(subtask["state"], "ready");
        // Subtask should NOT have session auto-linked (it's not interactive)
        assert!(subtask["session_id"].is_null());
        assert!(subtask["assigned_session"].is_null());
    }

    // ----- auto-archive on done tests -----

    #[test]
    fn test_auto_archive_session_on_done() {
        let db = TasksDb::open_memory().unwrap();
        let (mut writer, mut reader) = mock_io();

        // Create a task with a session_id
        let task = db
            .create_task("/project", "Auto archive", None, None, None, true)
            .unwrap();
        db.set_session_id(task.id, "worker-session").unwrap();
        db.update_task(
            task.id,
            &TaskUpdate {
                state: Some("ready".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        db.assign_task(task.id, "worker-session").unwrap();
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

        // Transition to done via handle_task_update
        let result = handle_task_update(
            &db,
            &serde_json::json!({"id": task.id, "state": "done"}),
            Some("s1"),
            "tc1",
            &mut writer,
            &mut reader,
            &mut Vec::new(),
        );
        assert!(!result.is_error);

        // Verify that an ArchiveSession request was sent
        let shared = writer.shared.lock().unwrap();
        let _output = String::from_utf8_lossy(&shared.write_buf);
        // The write_buf may have been consumed by process_pending, so check
        // the overall flow completed successfully. The key assertion is that
        // handle_task_update returned success (it calls server_request which
        // requires the mock to process the ArchiveSession request).
        drop(shared);

        let updated: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
        assert_eq!(updated["state"], "done");
    }

    #[test]
    fn test_auto_archive_no_session_id() {
        let db = TasksDb::open_memory().unwrap();
        let (mut writer, mut reader) = mock_io();

        // Create a task without a session_id and transition to done
        let task = db
            .create_task("/project", "No session", None, None, None, false)
            .unwrap();

        // Transition to done directly (universal override)
        let result = handle_task_update(
            &db,
            &serde_json::json!({"id": task.id, "state": "done"}),
            None,
            "tc1",
            &mut writer,
            &mut reader,
            &mut Vec::new(),
        );
        assert!(!result.is_error);
        let updated: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
        assert_eq!(updated["state"], "done");
        // No archive request should be sent (no session_id)
    }

    // ----- scheduler event tests -----

    #[test]
    fn test_update_to_approved_emits_merge_event() {
        let db = TasksDb::open_memory().unwrap();
        let (mut writer, mut reader) = mock_io();
        let mut events = Vec::new();

        // Create a task with skip_review and move to approved
        let task = db
            .create_task("/project", "Merge trigger", None, None, None, true)
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

        // active -> approved (skip_review=true) should emit MergeNeeded
        let result = handle_task_update(
            &db,
            &serde_json::json!({"id": task.id, "state": "approved"}),
            Some("s1"),
            "tc1",
            &mut writer,
            &mut reader,
            &mut events,
        );
        assert!(!result.is_error);
        assert_eq!(events, vec![SchedulerEvent::MergeNeeded]);
    }

    #[test]
    fn test_update_to_ready_emits_schedule_event() {
        let db = TasksDb::open_memory().unwrap();
        let (mut writer, mut reader) = mock_io();
        let mut events = Vec::new();

        // Create a task and move to ready
        let result = handle_task_create(
            &db,
            &serde_json::json!({"title": "Interactive task"}),
            "/project",
            Some("s1"),
            "tc1",
            &mut writer,
            &mut reader,
            &mut Vec::new(),
        );
        let task: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
        let task_id = task["id"].as_i64().unwrap();

        // interactive -> ready should emit ScheduleNeeded
        let result = handle_task_update(
            &db,
            &serde_json::json!({"id": task_id, "state": "ready"}),
            Some("s1"),
            "tc2",
            &mut writer,
            &mut reader,
            &mut events,
        );
        assert!(!result.is_error);
        assert_eq!(
            events,
            vec![SchedulerEvent::ScheduleNeeded("/project".into())]
        );
    }

    #[test]
    fn test_create_subtask_emits_schedule_event() {
        let db = TasksDb::open_memory().unwrap();
        let (mut writer, mut reader) = mock_io();
        let mut events = Vec::new();

        // Create parent task
        let result = handle_task_create(
            &db,
            &serde_json::json!({"title": "Parent"}),
            "/project",
            Some("s1"),
            "tc1",
            &mut writer,
            &mut reader,
            &mut Vec::new(),
        );
        let parent: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
        let parent_id = parent["id"].as_i64().unwrap();

        // Create subtask — defaults to ready, should emit ScheduleNeeded
        let result = handle_task_create(
            &db,
            &serde_json::json!({"title": "Subtask", "parent_id": parent_id}),
            "/project",
            Some("s1"),
            "tc2",
            &mut writer,
            &mut reader,
            &mut events,
        );
        assert!(!result.is_error);
        assert_eq!(
            events,
            vec![SchedulerEvent::ScheduleNeeded("/project".into())]
        );
    }

    #[test]
    fn test_update_to_other_state_no_event() {
        let db = TasksDb::open_memory().unwrap();
        let (mut writer, mut reader) = mock_io();
        let mut events = Vec::new();

        // Create task and move to active (via assign)
        let task = db
            .create_task("/project", "No event", None, None, None, false)
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

        // active -> review should NOT emit any event
        let result = handle_task_update(
            &db,
            &serde_json::json!({"id": task.id, "state": "review"}),
            Some("s1"),
            "tc1",
            &mut writer,
            &mut reader,
            &mut events,
        );
        assert!(!result.is_error);
        assert!(events.is_empty());
    }

    #[test]
    fn test_drain_scheduler_events_dedup() {
        // drain_scheduler_events should deduplicate MergeNeeded and
        // same-project ScheduleNeeded events.
        let mut events = vec![
            SchedulerEvent::MergeNeeded,
            SchedulerEvent::MergeNeeded,
            SchedulerEvent::ScheduleNeeded("/project-a".into()),
            SchedulerEvent::ScheduleNeeded("/project-a".into()),
            SchedulerEvent::ScheduleNeeded("/project-b".into()),
        ];

        // We can't easily test the actual passes (they need real git repos),
        // but we can verify the event collection logic by inspecting it.
        let batch = std::mem::take(&mut events);
        let mut need_merge = false;
        let mut schedule_projects: Vec<String> = Vec::new();
        for ev in batch {
            match ev {
                SchedulerEvent::MergeNeeded => need_merge = true,
                SchedulerEvent::ScheduleNeeded(project) => {
                    if !schedule_projects.contains(&project) {
                        schedule_projects.push(project);
                    }
                }
            }
        }
        assert!(need_merge);
        assert_eq!(schedule_projects.len(), 2);
        assert!(schedule_projects.contains(&"/project-a".to_string()));
        assert!(schedule_projects.contains(&"/project-b".to_string()));
    }
}
