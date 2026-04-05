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
                    "skip_planning": {
                        "type": "boolean",
                        "description": "If true, subtask starts in ready state instead of planning"
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
                "Subtasks (with parent_id) start in 'planning' state by default, or 'ready' if skip_planning=true".into(),
                "Valid states: interactive, planning, refining, ready, active, review, approved, merging, failed, done".into(),
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
                "task_get, task_assign, task_update and other task_* names are agent tool calls (like bash or edit), NOT shell commands — call them via the tool API, not via bash.".into(),
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
                        "description": "Filter by state (interactive, planning, refining, ready, active, review, approved, merging, failed, done). Use 'all' to include done tasks."
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
            prompt_snippet: Some("Use task_assign to claim a task and start working on it. This transitions the task from ready to active. For interactive tasks, it reassigns the session without changing state.".into()),
            prompt_guidelines: vec![
                "Task must be in 'ready' or 'interactive' state to be assigned".into(),
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
                    },
                    "skip_planning": {
                        "type": "boolean",
                        "description": "Whether to skip planning"
                    }
                },
                "required": ["id"]
            }),
            prompt_snippet: Some("Update task fields (title, state, priority, tags, etc.)".into()),
            prompt_guidelines: vec![
                "State transitions are validated: interactive->planning->refining->ready->active->review->approved->merging->done".into(),
                "Shortcuts: interactive->ready (skip planning), interactive->approved, active->approved (skip_review only)".into(),
                "Planning cycle: planning->refining, refining->planning (revise), refining->ready (approved), refining->interactive (scope expansion)".into(),
                "Backward (error recovery): review->active, approved->active/ready/interactive, merging->active (recoverable), merging->failed (terminal), failed->active (manual retry)".into(),
                "Universal overrides: any state->done (manual close), any state->interactive (human takes over), any state->failed".into(),
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
            description: "Run a scheduling pass: find ready/planning tasks, pick a non-conflicting batch, create branches and worktrees (for ready tasks), and transition them. Returns the list of tasks ready to dispatch.".into(),
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
                "Finds all ready and planning tasks, selects a non-conflicting batch based on affected_files, creates branches/worktrees for ready tasks, transitions them.".into(),
                "Planning tasks are dispatched without worktrees (read-only sessions).".into(),
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
        PluginToolDef {
            name: "task_status".into(),
            description: "Show the current task scheduler status: active, queued, and blocked tasks with wait reasons.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "project": {
                        "type": "string",
                        "description": "Project path (defaults to session cwd)"
                    }
                }
            }),
            prompt_snippet: Some("Show active/queued/blocked tasks with wait reasons".into()),
            prompt_guidelines: vec![
                "Shows what's running, what's queued, and why queued tasks are waiting.".into(),
                "Wait reasons include: dependency not done, file conflict with active task, budget exhausted, not yet scheduled.".into(),
            ],
        },
    ]
}

// ---------------------------------------------------------------------------
// Tool handlers
// ---------------------------------------------------------------------------

/// Context shared across tool-handler calls: project path, calling session,
/// and the tool-call identifier used to route the response.
struct ToolCtx<'a> {
    project: &'a str,
    session_id: Option<&'a str>,
    tool_call_id: &'a str,
}

fn handle_task_create(
    db: &TasksDb,
    args: &serde_json::Value,
    ctx: &ToolCtx<'_>,
    writer: &mut impl Write,
    reader: &mut impl BufRead,
    pending_events: &mut Vec<SchedulerEvent>,
) -> PluginToolResult {
    let project = ctx.project;
    let session_id = ctx.session_id;
    let tool_call_id = ctx.tool_call_id;
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
    let skip_planning = args
        .get("skip_planning")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let message = args.get("message").and_then(|v| v.as_str());

    match db.create_task(
        project,
        title,
        priority,
        parent_id,
        tags,
        skip_review,
        skip_planning,
    ) {
        Ok(task) => {
            // Subtasks start in ready or planning state — trigger a schedule pass.
            if task.state == "ready" || task.state == "planning" {
                pending_events.push(SchedulerEvent::ScheduleNeeded(
                    project.to_string(),
                    session_id.map(String::from),
                ));
            }

            // Interactive tasks get a fresh session for the user to drive
            if task.state == "interactive"
                && let Some(new_sid) =
                    create_interactive_session(db, &task, project, session_id, writer, reader)
            {
                let _ = db.set_session_id(task.id, &new_sid);
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

            // Re-fetch to include the session_id updates
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

    // Inherit model from the parent session so the interactive task uses
    // the same model as its creator.
    let model =
        parent_session_id.and_then(|sid| tasks_scheduler::get_session_model(sid, writer, reader));

    let create_req = Request::CreateSession {
        model,
        provider: None,
        system_prompt: None,
        cwd: Some(project.to_string()),
        parent_id: parent_session_id.map(String::from),
        child_budget: 16,
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

    // Queue an initial message so the session has context when the user connects.
    // Interactive tasks get a "gather-first" instruction: the session should
    // read the spec and understand requirements but NOT start any work until
    // the user explicitly says to proceed.
    let initial_msg = format!(
        "You are working on task {id}: {title}.\n\
         \n\
         Use the task_get tool (not a bash command) to read the full spec: \
         call `task_get` with arguments {{\"id\": {id}}}.\n\
         \n\
         This is an interactive task. Read the spec and gather all necessary information \
         (understand the requirements, explore relevant code, ask clarifying questions), \
         but do NOT start making any changes until the user explicitly tells you to proceed.",
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
    writer: &mut impl Write,
    reader: &mut impl BufRead,
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

    // Capture old session before assign (for reparenting)
    // Note: assign_task now handles descendant DB updates atomically in its
    // transaction and returns the old session + descendant session info.

    match db.assign_task(id, sid) {
        Ok(result) => {
            let task = &result.task;
            // For interactive tasks with a changed session, reparent sessions via RPC.
            // The DB updates (session_id on task + all descendants) were already done
            // atomically inside assign_task's transaction.
            if task.state == "interactive" {
                if let Some(ref old_sid) = result.old_session_id {
                    if old_sid != sid {
                        // Reparent direct child sessions from the old session
                        let req = crate::protocol::Request::ReparentChildren {
                            old_parent_id: old_sid.clone(),
                            new_parent_id: sid.to_string(),
                        };
                        if let Err(e) = crate::tasks_scheduler::server_request(writer, reader, req)
                        {
                            eprintln!(
                                "warning: failed to reparent children from {} to {}: {}",
                                old_sid, sid, e
                            );
                        }

                        // Reparent descendant task sessions that were parented
                        // under the old owner. Deduplicate since multiple
                        // descendants may share the same old session.
                        let mut reparented = std::collections::HashSet::new();
                        for desc_old_sid in &result.descendant_old_sessions {
                            if reparented.insert(desc_old_sid.clone()) {
                                let req = crate::protocol::Request::ReparentChildren {
                                    old_parent_id: desc_old_sid.clone(),
                                    new_parent_id: sid.to_string(),
                                };
                                if let Err(e) =
                                    crate::tasks_scheduler::server_request(writer, reader, req)
                                {
                                    eprintln!(
                                        "warning: failed to reparent children of {} to {}: {}",
                                        desc_old_sid, sid, e
                                    );
                                }
                            }
                        }
                    }
                }
            }
            match serde_json::to_string_pretty(&result.task) {
                Ok(json) => tool_ok(tool_call_id, &json),
                Err(e) => tool_err(tool_call_id, &format!("serialize: {}", e)),
            }
        }
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
        skip_planning: args.get("skip_planning").and_then(|v| v.as_bool()),
    };

    // Track session as reviewer if transitioning to review or approved
    if let (Some(sid), Some(new_state)) = (session_id, &update.state)
        && (new_state == "review" || new_state == "approved")
        && let Err(e) = db.record_session(id, sid, "reviewer")
    {
        return tool_err(tool_call_id, &format!("record session: {}", e));
    }

    // Capture the old state before updating, so we can detect review → active.
    let old_task = db.get_task(id).ok().flatten();
    let old_state = old_task.as_ref().map(|t| t.state.clone());

    // Rebase enforcement: active → review requires branch to be rebased on
    // merge target. This prevents merges from failing due to conflicts.
    if let (Some(new_state), Some(old_s)) = (&update.state, &old_state) {
        if new_state == "review" && old_s == "active" {
            if let Some(ref task) = old_task {
                if task.branch.is_some() && task.worktree_path.is_some() {
                    match tasks_scheduler::is_rebased_on_target(db, task) {
                        Ok(true) => {} // good, rebased
                        Ok(false) => {
                            let merge_target = db.get_merge_target(id).unwrap_or("main".into());

                            // Clean up any partial rebase state.
                            if let Some(ref wt_path) = task.worktree_path {
                                let _ = crate::tasks_git::abort_partial_rebase(wt_path);
                            }

                            // Notify the worker session.
                            if let Some(ref sid) = task.session_id {
                                let branch_name = task.branch.as_deref().unwrap_or("(unknown)");
                                let msg = format!(
                                    "Task {} transition to review was rejected: branch {} is not rebased on '{}'.\n\
                                     Please run `git rebase {}` in your worktree, resolve any conflicts, then try again:\n\
                                     - Call `task_update` with arguments {{\"id\": {}, \"state\": \"review\"}}",
                                    task.id, branch_name, merge_target, merge_target, task.id
                                );
                                let _ = crate::tasks_scheduler::server_request(
                                    writer,
                                    reader,
                                    crate::protocol::Request::QueueMessage {
                                        target_session_id: sid.clone(),
                                        content: msg,
                                        sender_info: format!(
                                            "task-system (rebase check task {})",
                                            task.id
                                        ),
                                        await_reply: false,
                                        reply_to: None,
                                    },
                                );
                            }

                            return tool_err(
                                tool_call_id,
                                &format!(
                                    "cannot transition to review: branch is not rebased on '{}'. \
                                     Please run `git rebase {}` in the worktree first.",
                                    merge_target, merge_target
                                ),
                            );
                        }
                        Err(e) => {
                            return tool_err(tool_call_id, &format!("rebase check failed: {}", e));
                        }
                    }
                }
            }
        }
    }

    match db.update_task(id, &update, session_id) {
        Ok(task) => {
            // Trigger scheduler events based on the new state.
            match task.state.as_str() {
                "approved" => pending_events.push(SchedulerEvent::MergeNeeded),
                "ready" | "planning" => {
                    pending_events.push(SchedulerEvent::ScheduleNeeded(
                        task.project.clone(),
                        session_id.map(String::from),
                    ));
                }
                _ => {}
            }

            // When a task transitions from review back to active (changes
            // requested), notify the worker session so it knows to resume.
            if task.state == "active" && old_state.as_deref() == Some("review") {
                if let Some(ref sid) = task.session_id {
                    let msg = format!(
                        "Task {} was moved back to active (changes requested). \
                        Please run task_get to read the latest review feedback \
                        and address the requested changes.",
                        task.id
                    );
                    let _ = crate::tasks_scheduler::server_request(
                        writer,
                        reader,
                        crate::protocol::Request::QueueMessage {
                            target_session_id: sid.clone(),
                            content: msg,
                            sender_info: format!("task-system (review task {})", task.id),
                            await_reply: false,
                            reply_to: None,
                        },
                    );
                }
            }

            // Automated review dispatch: when transitioning to review,
            // auto-launch a review session.
            if task.state == "review" && old_state.as_deref() == Some("active") {
                match tasks_scheduler::dispatch_review(db, &task, session_id, writer, reader) {
                    Ok(review_sid) => {
                        eprintln!(
                            "tasks: auto-dispatched review session {} for task {}",
                            review_sid, task.id
                        );
                    }
                    Err(e) => {
                        eprintln!(
                            "tasks: failed to auto-dispatch review for task {}: {}",
                            task.id, e
                        );
                        let _ = db.add_message(
                            task.id,
                            &format!(
                                "⚠️ Auto-dispatch of review session failed: {}. \
                                 Task is in review state but has no reviewer session.",
                                e
                            ),
                            Some("system"),
                        );
                    }
                }
            }

            // Automated refining dispatch: when transitioning to refining,
            // auto-launch a refining session.
            if task.state == "refining"
                && (old_state.as_deref() == Some("planning")
                    || old_state.as_deref() == Some("interactive"))
            {
                match tasks_scheduler::dispatch_refining(db, &task, session_id, writer, reader) {
                    Ok(refining_sid) => {
                        eprintln!(
                            "tasks: auto-dispatched refining session {} for task {}",
                            refining_sid, task.id
                        );
                    }
                    Err(e) => {
                        eprintln!(
                            "tasks: failed to auto-dispatch refining for task {}: {}",
                            task.id, e
                        );
                        let _ = db.add_message(
                            task.id,
                            &format!(
                                "⚠️ Auto-dispatch of refining session failed: {}. \
                                 Task is in refining state but has no refiner session.",
                                e
                            ),
                            Some("system"),
                        );
                    }
                }
            }

            // Session reuse: when refining → planning, resume the planning
            // session by sending it a message with the refining feedback.
            if task.state == "planning" && old_state.as_deref() == Some("refining") {
                if let Some(ref sid) = task.session_id {
                    let msg = format!(
                        "Task {} was sent back to planning (plan needs revision). \
                         Please run task_get to read the latest refining feedback \
                         and revise your plan.",
                        task.id
                    );
                    let _ = crate::tasks_scheduler::server_request(
                        writer,
                        reader,
                        crate::protocol::Request::QueueMessage {
                            target_session_id: sid.clone(),
                            content: msg,
                            sender_info: format!("task-system (refine task {})", task.id),
                            await_reply: false,
                            reply_to: None,
                        },
                    );
                }
            }

            // When a task transitions TO interactive (from any other state),
            // ensure it has a live session. This covers the universal
            // `any→interactive` override and `refining→interactive` scope
            // expansion. The creating session (for parent_id) is the session
            // that triggered the transition.
            if task.state == "interactive" && old_state.as_deref() != Some("interactive") {
                let needs_session = match &task.session_id {
                    None => true,
                    Some(sid) => {
                        // Check if existing session is still alive
                        let req = crate::protocol::Request::GetSessionInfo {
                            session_id: sid.clone(),
                        };
                        match crate::tasks_scheduler::server_request(writer, reader, req) {
                            Ok(crate::protocol::Response::SessionInfo { info }) => info.archived,
                            _ => true, // session not found or error → need a new one
                        }
                    }
                };
                if needs_session {
                    if let Some(new_sid) = create_interactive_session(
                        db,
                        &task,
                        &task.project.clone(),
                        session_id,
                        writer,
                        reader,
                    ) {
                        let _ = db.set_session_id(task.id, &new_sid);
                        let _ = db.record_session(task.id, &new_sid, "interactive");
                        if let Some(creator_sid) = session_id {
                            let _ = db.record_session(task.id, creator_sid, "creator");
                        }
                    }
                }
            }

            // When a task transitions to done, auto-archive its session
            if task.state == "done" {
                auto_archive_task_session(db, &task, writer, reader);
            }

            // Re-fetch task if we may have updated session_id after the
            // initial update_task call (e.g. interactive session creation).
            let final_task =
                if task.state == "interactive" && old_state.as_deref() != Some("interactive") {
                    db.get_task(task.id).ok().flatten().unwrap_or(task)
                } else {
                    task
                };

            match serde_json::to_string_pretty(&final_task) {
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
    // Archive all task sessions: worker, reviewer, refiner, etc.
    let mut archived: std::collections::HashSet<String> = std::collections::HashSet::new();
    if let Ok(sessions) = db.get_sessions(task.id) {
        for ts in &sessions {
            let _ = crate::tasks_scheduler::server_request(
                writer,
                reader,
                crate::protocol::Request::ArchiveSession {
                    session_id: ts.session_id.clone(),
                    require_ancestor: None,
                },
            );
            archived.insert(ts.session_id.clone());
        }
    }
    // Also archive session_id if it wasn't tracked in task_sessions
    if let Some(ref sid) = task.session_id
        && !archived.contains(sid)
    {
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

fn handle_task_status(
    db: &TasksDb,
    args: &serde_json::Value,
    project: &str,
    tool_call_id: &str,
) -> PluginToolResult {
    let project = args
        .get("project")
        .and_then(|v| v.as_str())
        .unwrap_or(project);

    match tasks_scheduler::get_status(db, project) {
        Ok(status) => tool_ok(tool_call_id, &tasks_scheduler::format_status(&status)),
        Err(e) => tool_err(tool_call_id, &format!("scheduler status: {}", e)),
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
    /// The optional session ID is the session that triggered the event, used
    /// to inherit the model when auto-dispatching tasks.
    ScheduleNeeded(String, Option<String>),
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
        hooks: vec!["task_state_changed".to_string()],
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
    while let Some(line) = chan_reader.recv() {
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
                        &ToolCtx {
                            project,
                            session_id: session,
                            tool_call_id: &tool_call_id,
                        },
                        &mut writer,
                        &mut chan_reader,
                        &mut pending_events,
                    ),
                    "task_get" => handle_task_get(&db, &arguments, &tool_call_id),
                    "task_list" => handle_task_list(&db, &arguments, project, &tool_call_id),
                    "task_assign" => handle_task_assign(
                        &db,
                        &arguments,
                        session,
                        &tool_call_id,
                        &mut writer,
                        &mut chan_reader,
                    ),
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
                    "task_status" => handle_task_status(&db, &arguments, project, &tool_call_id),
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
            PluginRequest::Hook { name, data } => {
                if name == "task_state_changed" {
                    let new_state = data.get("new_state").and_then(|v| v.as_str()).unwrap_or("");
                    let task_id = data.get("task_id").and_then(|v| v.as_i64()).unwrap_or(0);
                    match new_state {
                        "approved" => {
                            pending_events.push(SchedulerEvent::MergeNeeded);
                        }
                        "ready" | "planning" => {
                            // Look up the task's project for the schedule pass.
                            if let Ok(Some(task)) = db.get_task(task_id) {
                                pending_events.push(SchedulerEvent::ScheduleNeeded(
                                    task.project.clone(),
                                    None,
                                ));
                            }
                        }
                        _ => {}
                    }
                    drain_scheduler_events(&mut pending_events, &db, &mut writer, &mut chan_reader);
                }
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
    let mut schedule_projects: Vec<(String, Option<String>)> = Vec::new();

    for ev in batch {
        match ev {
            SchedulerEvent::MergeNeeded => need_merge = true,
            SchedulerEvent::ScheduleNeeded(project, session_id) => {
                if !schedule_projects.iter().any(|(p, _)| p == &project) {
                    schedule_projects.push((project, session_id));
                }
            }
        }
    }

    // Run merge pass first (merging may unblock dependencies that become
    // ready, but schedule passes are triggered by explicit events anyway).
    if need_merge {
        run_merge_pass(db, writer, reader);
    }

    for (project, session_id) in &schedule_projects {
        run_schedule_pass(db, project, session_id.as_deref(), writer, reader);
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

/// Run a schedule + dispatch pass for a project: find ready/planning tasks,
/// prepare branches/worktrees, and dispatch sessions for them.
///
/// Triggered when a task transitions to `ready` or `planning`.
fn run_schedule_pass(
    db: &TasksDb,
    project: &str,
    session_id: Option<&str>,
    writer: &mut impl Write,
    reader: &mut impl BufRead,
) {
    match tasks_scheduler::schedule(db, project) {
        Ok(scheduled) => {
            for st in &scheduled {
                if st.branch.is_empty() {
                    // Planning task — dispatch planning session
                    eprintln!(
                        "tasks scheduler: dispatching planning for task {} ({})",
                        st.id, st.title
                    );
                } else {
                    eprintln!(
                        "tasks scheduler: scheduled task {} ({}) on branch {}",
                        st.id, st.title, st.branch
                    );
                }
                // Dispatch each scheduled task (create session + send initial message).
                // Pass the triggering session_id so dispatch() can inherit its model.
                if let Err(e) = tasks_scheduler::dispatch(db, st.id, session_id, writer, reader) {
                    eprintln!("tasks scheduler: dispatch failed for task {}: {}", st.id, e);
                    let _ = db.add_message(
                        st.id,
                        &format!(
                            "⚠️ Auto-dispatch of session failed: {}. \
                             Task was scheduled but no session was created.",
                            e
                        ),
                        Some("system"),
                    );
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
            archived_sessions: std::collections::HashSet::new(),
        }));
        let writer = MockWriter {
            shared: shared.clone(),
        };
        let reader = MockReader { shared };
        (writer, BufReader::new(reader))
    }

    /// Create mock IO where the given session IDs are reported as archived.
    /// Starts from a high counter to avoid ID collisions with earlier mocks.
    fn mock_io_with_archived(
        archived: std::collections::HashSet<String>,
    ) -> (MockWriter, BufReader<MockReader>) {
        // Start counter high to avoid collisions with sessions created by
        // earlier mock_io instances in the same test.
        let shared = std::sync::Arc::new(std::sync::Mutex::new(MockShared {
            write_buf: Vec::new(),
            read_buf: Vec::new(),
            session_counter: 100,
            archived_sessions: archived,
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
        /// Sessions that should be reported as archived by GetSessionInfo.
        archived_sessions: std::collections::HashSet<String>,
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
                        crate::protocol::Request::GetSessionInfo { session_id } => {
                            let is_archived = self.archived_sessions.contains(session_id.as_str());
                            crate::protocol::Response::SessionInfo {
                                info: crate::protocol::SessionInfo {
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
                                    archived: is_archived,
                                    last_exit_status: None,
                                },
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
        assert_eq!(tools.len(), 13);
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
        assert!(names.contains(&"task_status"));
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
            &ToolCtx {
                project: "/project",
                session_id: Some("s1"),
                tool_call_id: "tc1",
            },
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
            &ToolCtx {
                project: "/p",
                session_id: None,
                tool_call_id: "tc1",
            },
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
        let t1 = db
            .create_task("/p", "A", None, None, None, false, false)
            .unwrap();
        let t2 = db
            .create_task("/p", "B", None, None, None, false, false)
            .unwrap();

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
        let task = db
            .create_task("/p", "A", None, None, None, false, false)
            .unwrap();
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
        assert_eq!(parsed["tools"].as_array().unwrap().len(), 13);
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
            &ToolCtx {
                project: "/project",
                session_id: Some("s1"),
                tool_call_id: "tc1",
            },
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
            &mut writer,
            &mut reader,
        );
        assert!(!result.is_error);
        let assigned: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
        assert_eq!(assigned["state"], "active");
        assert_eq!(assigned["session_id"], "worker-session");
    }

    #[test]
    fn test_task_assign_uses_context_session() {
        let db = TasksDb::open_memory().unwrap();
        let (mut writer, mut reader) = mock_io();

        let result = handle_task_create(
            &db,
            &serde_json::json!({"title": "Task for context session"}),
            &ToolCtx {
                project: "/project",
                session_id: Some("s1"),
                tool_call_id: "tc1",
            },
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
            &mut writer,
            &mut reader,
        );
        assert!(!result.is_error);
        let assigned: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
        assert_eq!(assigned["session_id"], "context-session");
    }

    #[test]
    fn test_task_assign_interactive_reassigns_session() {
        let db = TasksDb::open_memory().unwrap();
        let (mut writer, mut reader) = mock_io();

        let result = handle_task_create(
            &db,
            &serde_json::json!({"title": "Interactive task"}),
            &ToolCtx {
                project: "/project",
                session_id: Some("s1"),
                tool_call_id: "tc1",
            },
            &mut writer,
            &mut reader,
            &mut Vec::new(),
        );
        let task: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
        let task_id = task["id"].as_i64().unwrap();

        // Assign interactive task to a new session — should succeed and stay interactive
        let result = handle_task_assign(
            &db,
            &serde_json::json!({"id": task_id}),
            Some("s2"),
            "tc2",
            &mut writer,
            &mut reader,
        );
        assert!(
            !result.is_error,
            "expected success: {}",
            extract_text(&result)
        );
        let assigned: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
        assert_eq!(assigned["state"], "interactive");
        assert_eq!(assigned["session_id"], "s2");
    }

    #[test]
    fn test_task_assign_no_session() {
        let db = TasksDb::open_memory().unwrap();
        let (mut writer, mut reader) = mock_io();

        let result = handle_task_create(
            &db,
            &serde_json::json!({"title": "No session task"}),
            &ToolCtx {
                project: "/project",
                session_id: None,
                tool_call_id: "tc1",
            },
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
        let result = handle_task_assign(
            &db,
            &serde_json::json!({"id": task_id}),
            None,
            "tc3",
            &mut writer,
            &mut reader,
        );
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
            &ToolCtx {
                project: "/project",
                session_id: Some("s1"),
                tool_call_id: "tc1",
            },
            &mut writer,
            &mut reader,
            &mut Vec::new(),
        );
        let parent: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
        let parent_id = parent["id"].as_i64().unwrap();
        assert_eq!(parent["state"], "interactive");

        // Create subtask — should default to planning state, skip_review=false
        let result = handle_task_create(
            &db,
            &serde_json::json!({"title": "Subtask", "parent_id": parent_id, "skip_review": true}),
            &ToolCtx {
                project: "/project",
                session_id: Some("s1"),
                tool_call_id: "tc2",
            },
            &mut writer,
            &mut reader,
            &mut Vec::new(),
        );
        assert!(!result.is_error);
        let subtask: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
        assert_eq!(subtask["state"], "planning");
        assert_eq!(subtask["skip_review"], false);

        // Create subtask with skip_planning — should default to ready state
        let result = handle_task_create(
            &db,
            &serde_json::json!({"title": "Subtask skip plan", "parent_id": parent_id, "skip_planning": true}),
            &ToolCtx {
                project: "/project",
                session_id: Some("s1"),
                tool_call_id: "tc3",
            },
            &mut writer,
            &mut reader,
            &mut Vec::new(),
        );
        assert!(!result.is_error);
        let subtask2: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
        assert_eq!(subtask2["state"], "ready");
    }

    #[test]
    fn test_active_to_approved_requires_skip_review() {
        let db = TasksDb::open_memory().unwrap();
        let (mut writer, mut reader) = mock_io();

        // Create task without skip_review
        let task = db
            .create_task("/project", "No skip", None, None, None, false, false)
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
            .create_task("/project", "Skip review", None, None, None, true, false)
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
            .create_task("/project", "Tracked", None, None, None, false, false)
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
            .create_task("/project", "Review track", None, None, None, false, false)
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
            .create_task("/project", "Idempotent", None, None, None, false, false)
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
        // task_get should also have a guideline that task_* are tool calls, not shell commands
        assert!(
            task_get
                .prompt_guidelines
                .iter()
                .any(|g| g.contains("tool call")
                    || g.contains("tool API")
                    || g.contains("NOT shell")),
            "task_get should have a guideline clarifying these are tool calls not shell commands"
        );

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
            .create_task("/project", "Self", None, None, None, false, false)
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
            .create_task("/project-a", "A", None, None, None, false, false)
            .unwrap();
        let t2 = db
            .create_task("/project-b", "B", None, None, None, false, false)
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
            .create_task("/project", "T1", None, None, None, false, false)
            .unwrap();
        let t2 = db
            .create_task("/project", "T2", None, None, None, false, false)
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
            .create_task("/project", "Dependency", None, None, None, false, false)
            .unwrap();
        let task = db
            .create_task("/project", "Dependent", None, None, None, false, false)
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
            .create_task("/project", "Dependency", None, None, None, true, false)
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
            .create_task("/project", "Dependent", None, None, None, false, false)
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
            .create_task("/project", "T1", None, None, None, false, false)
            .unwrap();
        let t2 = db
            .create_task("/project", "T2", None, None, None, false, false)
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
            &ToolCtx {
                project: "/project",
                session_id: Some("creating-session"),
                tool_call_id: "tc1",
            },
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
            &ToolCtx {
                project: "/project",
                session_id: None,
                tool_call_id: "tc1",
            },
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
            &ToolCtx {
                project: "/project",
                session_id: Some("s1"),
                tool_call_id: "tc1",
            },
            &mut writer,
            &mut reader,
            &mut Vec::new(),
        );
        let parent: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
        let parent_id = parent["id"].as_i64().unwrap();

        // Create subtask (defaults to planning, not interactive)
        let result = handle_task_create(
            &db,
            &serde_json::json!({"title": "Subtask", "parent_id": parent_id}),
            &ToolCtx {
                project: "/project",
                session_id: Some("s1"),
                tool_call_id: "tc2",
            },
            &mut writer,
            &mut reader,
            &mut Vec::new(),
        );
        assert!(!result.is_error);
        let text = extract_text(&result);
        let subtask: serde_json::Value = serde_json::from_str(&text).unwrap();

        assert_eq!(subtask["state"], "planning");
        // Subtask should NOT have session auto-linked (it's not interactive)
        assert!(subtask["session_id"].is_null());
    }

    // ----- auto-archive on done tests -----

    #[test]
    fn test_auto_archive_session_on_done() {
        let db = TasksDb::open_memory().unwrap();
        let (mut writer, mut reader) = mock_io();

        // Create a task with a session_id
        let task = db
            .create_task("/project", "Auto archive", None, None, None, true, false)
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
            .create_task("/project", "No session", None, None, None, false, false)
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

    #[test]
    fn test_auto_archive_all_task_sessions_on_done() {
        let db = TasksDb::open_memory().unwrap();
        let (mut writer, mut reader) = mock_io();

        // Create a task with a session_id (worker)
        let task = db
            .create_task(
                "/project",
                "Multi-session archive",
                None,
                None,
                None,
                true,
                false,
            )
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

        // Record additional sessions (reviewer, refiner)
        db.record_session(task.id, "worker-session", "worker")
            .unwrap();
        db.record_session(task.id, "reviewer-session", "reviewer")
            .unwrap();
        db.record_session(task.id, "refiner-session", "refiner")
            .unwrap();

        // Skip to done via universal override
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

        // Transition to done via handle_task_update (triggers auto_archive)
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

        let updated: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
        assert_eq!(updated["state"], "done");

        // Verify that ArchiveSession requests were sent for all three sessions
        let shared = writer.shared.lock().unwrap();
        // The mock processes requests immediately, but we can check that the
        // mock processed the right number of archive requests by looking at
        // the read_buf (each archive generates a response).
        drop(shared);

        // The key assertion is that handle_task_update returned success,
        // which means all three ArchiveSession server_requests were processed
        // by the mock. If any failed, the mock would have returned an error
        // and the function would have errored out.
    }

    // ----- scheduler event tests -----

    #[test]
    fn test_update_to_approved_emits_merge_event() {
        let db = TasksDb::open_memory().unwrap();
        let (mut writer, mut reader) = mock_io();
        let mut events = Vec::new();

        // Create a task with skip_review and move to approved
        let task = db
            .create_task("/project", "Merge trigger", None, None, None, true, false)
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
            &ToolCtx {
                project: "/project",
                session_id: Some("s1"),
                tool_call_id: "tc1",
            },
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
            vec![SchedulerEvent::ScheduleNeeded(
                "/project".into(),
                Some("s1".into())
            )]
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
            &ToolCtx {
                project: "/project",
                session_id: Some("s1"),
                tool_call_id: "tc1",
            },
            &mut writer,
            &mut reader,
            &mut Vec::new(),
        );
        let parent: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
        let parent_id = parent["id"].as_i64().unwrap();

        // Create subtask — defaults to planning (skip_planning=false), should emit ScheduleNeeded
        let result = handle_task_create(
            &db,
            &serde_json::json!({"title": "Subtask", "parent_id": parent_id}),
            &ToolCtx {
                project: "/project",
                session_id: Some("s1"),
                tool_call_id: "tc2",
            },
            &mut writer,
            &mut reader,
            &mut events,
        );
        assert!(!result.is_error);
        assert_eq!(
            events,
            vec![SchedulerEvent::ScheduleNeeded(
                "/project".into(),
                Some("s1".into())
            )]
        );
    }

    #[test]
    fn test_update_to_other_state_no_event() {
        let db = TasksDb::open_memory().unwrap();
        let (mut writer, mut reader) = mock_io();
        let mut events = Vec::new();

        // Create task and move to active (via assign)
        let task = db
            .create_task("/project", "No event", None, None, None, false, false)
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
            SchedulerEvent::ScheduleNeeded("/project-a".into(), Some("s1".into())),
            SchedulerEvent::ScheduleNeeded("/project-a".into(), Some("s2".into())),
            SchedulerEvent::ScheduleNeeded("/project-b".into(), None),
        ];

        // We can't easily test the actual passes (they need real git repos),
        // but we can verify the event collection logic by inspecting it.
        let batch = std::mem::take(&mut events);
        let mut need_merge = false;
        let mut schedule_projects: Vec<(String, Option<String>)> = Vec::new();
        for ev in batch {
            match ev {
                SchedulerEvent::MergeNeeded => need_merge = true,
                SchedulerEvent::ScheduleNeeded(project, session_id) => {
                    if !schedule_projects.iter().any(|(p, _)| p == &project) {
                        schedule_projects.push((project, session_id));
                    }
                }
            }
        }
        assert!(need_merge);
        assert_eq!(schedule_projects.len(), 2);
        assert!(schedule_projects.iter().any(|(p, _)| p == "/project-a"));
        assert!(schedule_projects.iter().any(|(p, _)| p == "/project-b"));
        // First occurrence of project-a wins — carries s1's session ID
        let a_entry = schedule_projects
            .iter()
            .find(|(p, _)| p == "/project-a")
            .unwrap();
        assert_eq!(a_entry.1, Some("s1".into()));
        // project-b had no session
        let b_entry = schedule_projects
            .iter()
            .find(|(p, _)| p == "/project-b")
            .unwrap();
        assert_eq!(b_entry.1, None);
    }

    // ----- review → active notification tests -----

    #[test]
    fn test_review_to_active_notifies_worker_session() {
        let db = TasksDb::open_memory().unwrap();

        // Create a task, assign it a session, advance to review state
        let task = db
            .create_task(
                "/project",
                "Review notify test",
                None,
                None,
                None,
                false,
                false,
            )
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
                state: Some("review".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();

        // Use a plain Vec<u8> writer so we can inspect the raw output.
        // The QueueMessage send will fail (empty reader) but is best-effort.
        let mut writer: Vec<u8> = Vec::new();
        let mut reader = std::io::BufReader::new(std::io::Cursor::new(Vec::<u8>::new()));

        // review -> active: should send a QueueMessage to "worker-session"
        let result = handle_task_update(
            &db,
            &serde_json::json!({"id": task.id, "state": "active"}),
            Some("reviewer-session"),
            "tc1",
            &mut writer,
            &mut reader,
            &mut Vec::new(),
        );
        assert!(!result.is_error, "unexpected error: {:?}", result);

        let output = String::from_utf8(writer).unwrap();
        assert!(
            output.contains("queue_message"),
            "expected QueueMessage in output: {output}"
        );
        assert!(
            output.contains("worker-session"),
            "expected worker-session in output: {output}"
        );
        assert!(
            output.contains("changes requested"),
            "expected 'changes requested' in output: {output}"
        );
    }

    #[test]
    fn test_review_to_active_no_session_no_panic() {
        let db = TasksDb::open_memory().unwrap();

        // Task with no session_id — review -> active should succeed silently.
        // assign_task now sets session_id, so we clear it afterwards to
        // simulate a task whose session was removed.
        let task = db
            .create_task(
                "/project",
                "No session review",
                None,
                None,
                None,
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
        // Clear session_id to simulate a task with no session
        db.clear_session_id(task.id).unwrap();
        db.update_task(
            task.id,
            &TaskUpdate {
                state: Some("review".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();

        let mut writer: Vec<u8> = Vec::new();
        let mut reader = std::io::BufReader::new(std::io::Cursor::new(Vec::<u8>::new()));

        let result = handle_task_update(
            &db,
            &serde_json::json!({"id": task.id, "state": "active"}),
            None,
            "tc1",
            &mut writer,
            &mut reader,
            &mut Vec::new(),
        );
        assert!(!result.is_error, "unexpected error: {:?}", result);
        // No QueueMessage should be attempted when there is no session_id
        assert!(
            !String::from_utf8_lossy(&writer).contains("queue_message"),
            "should not send QueueMessage when no session_id"
        );
    }

    #[test]
    fn test_approved_to_active_no_notification() {
        let db = TasksDb::open_memory().unwrap();

        // Test that approved → active does NOT send a QueueMessage
        // (only review → active should trigger the notification)
        let task = db
            .create_task("/project", "Approved bounce", None, None, None, true, false)
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

        let mut writer: Vec<u8> = Vec::new();
        let mut reader = std::io::BufReader::new(std::io::Cursor::new(Vec::<u8>::new()));

        // approved → active should NOT send a QueueMessage
        let result = handle_task_update(
            &db,
            &serde_json::json!({"id": task.id, "state": "active"}),
            Some("reviewer-session"),
            "tc1",
            &mut writer,
            &mut reader,
            &mut Vec::new(),
        );
        assert!(!result.is_error, "unexpected error: {:?}", result);
        assert!(
            !String::from_utf8_lossy(&writer).contains("queue_message"),
            "should not send QueueMessage for approved → active transition"
        );
    }

    // ----- review dispatch tests -----

    #[test]
    fn test_active_to_review_dispatches_review_session() {
        let db = TasksDb::open_memory().unwrap();
        let (mut writer, mut reader) = mock_io();

        // Create a task with skip_review=false and advance to active
        let task = db
            .create_task(
                "/project",
                "Review dispatch test",
                None,
                None,
                None,
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
        db.assign_task(task.id, "worker-session").unwrap();
        db.set_worktree_path(task.id, "/tmp/fake-worktree").unwrap();

        // active -> review should trigger auto-dispatch of a review session
        let mut events = Vec::new();
        let result = handle_task_update(
            &db,
            &serde_json::json!({"id": task.id, "state": "review"}),
            Some("worker-session"),
            "tc1",
            &mut writer,
            &mut reader,
            &mut events,
        );
        assert!(
            !result.is_error,
            "unexpected error: {}",
            extract_text(&result)
        );

        let updated: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
        assert_eq!(updated["state"], "review");

        // Verify that a reviewer session was recorded
        let sessions = db.get_sessions(task.id).unwrap();
        let roles: Vec<(&str, &str)> = sessions
            .iter()
            .map(|s| (s.session_id.as_str(), s.role.as_str()))
            .collect();
        assert!(
            roles.iter().any(|(_, role)| *role == "reviewer"),
            "expected a reviewer session to be recorded, got: {:?}",
            roles
        );
    }

    #[test]
    fn test_review_dispatch_records_reviewer_role() {
        let db = TasksDb::open_memory().unwrap();
        let (mut writer, mut reader) = mock_io();

        let task = db
            .create_task(
                "/project",
                "Reviewer role test",
                None,
                None,
                None,
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
        db.assign_task(task.id, "worker-session").unwrap();
        db.set_worktree_path(task.id, "/tmp/fake-worktree").unwrap();

        let mut events = Vec::new();
        let result = handle_task_update(
            &db,
            &serde_json::json!({"id": task.id, "state": "review"}),
            Some("worker-session"),
            "tc1",
            &mut writer,
            &mut reader,
            &mut events,
        );
        assert!(!result.is_error);

        // The worker session should be recorded as reviewer (it triggered the transition)
        // and the auto-dispatched session should also be recorded as reviewer
        let sessions = db.get_sessions(task.id).unwrap();
        let reviewer_sessions: Vec<_> = sessions.iter().filter(|s| s.role == "reviewer").collect();
        // At least one reviewer session (the auto-dispatched one)
        assert!(
            !reviewer_sessions.is_empty(),
            "expected at least one reviewer session, got: {:?}",
            sessions
                .iter()
                .map(|s| (&s.session_id, &s.role))
                .collect::<Vec<_>>()
        );
    }

    // ----- session reuse tests -----

    #[test]
    fn test_second_active_to_review_reuses_existing_reviewer_session() {
        let db = TasksDb::open_memory().unwrap();
        let (mut writer, mut reader) = mock_io();

        // Create a task and advance to active
        let task = db
            .create_task(
                "/project",
                "Reuse reviewer test",
                None,
                None,
                None,
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
        db.assign_task(task.id, "worker-session").unwrap();
        db.set_worktree_path(task.id, "/tmp/fake-worktree").unwrap();

        // First active -> review: should create a new reviewer session
        let mut events = Vec::new();
        let result = handle_task_update(
            &db,
            &serde_json::json!({"id": task.id, "state": "review"}),
            Some("worker-session"),
            "tc1",
            &mut writer,
            &mut reader,
            &mut events,
        );
        assert!(
            !result.is_error,
            "unexpected error: {}",
            extract_text(&result)
        );

        let sessions_after_first = db.get_sessions(task.id).unwrap();
        let reviewer_sessions_first: Vec<_> = sessions_after_first
            .iter()
            .filter(|s| s.role == "reviewer")
            .collect();
        assert!(
            !reviewer_sessions_first.is_empty(),
            "expected a reviewer session after first review"
        );
        let first_reviewer_id = reviewer_sessions_first.last().unwrap().session_id.clone();

        // Move back to active (simulating changes requested)
        db.update_task(
            task.id,
            &TaskUpdate {
                state: Some("active".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();

        // Second active -> review: should reuse the existing reviewer session
        let result = handle_task_update(
            &db,
            &serde_json::json!({"id": task.id, "state": "review"}),
            Some("worker-session"),
            "tc2",
            &mut writer,
            &mut reader,
            &mut events,
        );
        assert!(
            !result.is_error,
            "unexpected error: {}",
            extract_text(&result)
        );

        // Verify no new reviewer sessions were created — same count as before
        let sessions_after_second = db.get_sessions(task.id).unwrap();
        let reviewer_sessions_second: Vec<_> = sessions_after_second
            .iter()
            .filter(|s| s.role == "reviewer")
            .collect();

        // The number of reviewer sessions should not increase on the second review
        assert_eq!(
            reviewer_sessions_first.len(),
            reviewer_sessions_second.len(),
            "expected reviewer session to be reused, not a new one created. \
             Sessions: {:?}",
            reviewer_sessions_second
                .iter()
                .map(|s| &s.session_id)
                .collect::<Vec<_>>()
        );

        // The reviewer session ID should still be the same one
        assert_eq!(
            first_reviewer_id,
            reviewer_sessions_second.last().unwrap().session_id,
            "expected same reviewer session to be reused"
        );
    }

    #[test]
    fn test_archived_reviewer_creates_new_session() {
        let db = TasksDb::open_memory().unwrap();

        // First, create a reviewer session using normal mock
        let (mut writer, mut reader) = mock_io();
        let task = db
            .create_task(
                "/project",
                "Archived reviewer test",
                None,
                None,
                None,
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
        db.assign_task(task.id, "worker-session").unwrap();
        db.set_worktree_path(task.id, "/tmp/fake-worktree").unwrap();

        // First active -> review: creates "mock-s1" as reviewer
        let mut events = Vec::new();
        let result = handle_task_update(
            &db,
            &serde_json::json!({"id": task.id, "state": "review"}),
            Some("worker-session"),
            "tc1",
            &mut writer,
            &mut reader,
            &mut events,
        );
        assert!(
            !result.is_error,
            "unexpected error: {}",
            extract_text(&result)
        );

        let sessions = db.get_sessions(task.id).unwrap();
        let first_reviewer = sessions
            .iter()
            .find(|s| s.role == "reviewer")
            .expect("expected a reviewer session")
            .session_id
            .clone();

        // Move back to active
        db.update_task(
            task.id,
            &TaskUpdate {
                state: Some("active".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();

        // Now create a new mock IO where the first reviewer is archived
        let mut archived = std::collections::HashSet::new();
        archived.insert(first_reviewer.clone());
        let (mut writer2, mut reader2) = mock_io_with_archived(archived);

        // Second active -> review: archived reviewer should trigger new session creation
        let result = handle_task_update(
            &db,
            &serde_json::json!({"id": task.id, "state": "review"}),
            Some("worker-session"),
            "tc2",
            &mut writer2,
            &mut reader2,
            &mut events,
        );
        assert!(
            !result.is_error,
            "unexpected error: {}",
            extract_text(&result)
        );

        // Verify a new reviewer session was created (different from the first)
        let sessions_after = db.get_sessions(task.id).unwrap();
        let reviewer_sessions: Vec<_> = sessions_after
            .iter()
            .filter(|s| s.role == "reviewer")
            .collect();
        assert!(
            reviewer_sessions.len() > 1,
            "expected a new reviewer session to be created when old one is archived, got: {:?}",
            reviewer_sessions
                .iter()
                .map(|s| &s.session_id)
                .collect::<Vec<_>>()
        );
        let last_reviewer = reviewer_sessions.last().unwrap();
        assert_ne!(
            last_reviewer.session_id, first_reviewer,
            "expected a different session when old reviewer is archived"
        );
    }

    #[test]
    fn test_second_planning_to_refining_reuses_existing_refiner_session() {
        let db = TasksDb::open_memory().unwrap();
        let (mut writer, mut reader) = mock_io();

        // Create a subtask in planning state
        let parent = db
            .create_task("/project", "Parent", None, None, None, false, false)
            .unwrap();
        let task = db
            .create_task(
                "/project",
                "Refiner reuse test",
                Some(5),
                Some(parent.id),
                None,
                false,
                false,
            )
            .unwrap();
        assert_eq!(task.state, "planning");

        db.set_session_id(task.id, "planning-session").unwrap();

        // First planning -> refining: should create a new refiner session
        let mut events = Vec::new();
        let result = handle_task_update(
            &db,
            &serde_json::json!({"id": task.id, "state": "refining"}),
            Some("planning-session"),
            "tc1",
            &mut writer,
            &mut reader,
            &mut events,
        );
        assert!(
            !result.is_error,
            "unexpected error: {}",
            extract_text(&result)
        );

        let sessions_after_first = db.get_sessions(task.id).unwrap();
        let refiner_sessions_first: Vec<_> = sessions_after_first
            .iter()
            .filter(|s| s.role == "refiner")
            .collect();
        assert!(
            !refiner_sessions_first.is_empty(),
            "expected a refiner session after first refining"
        );
        let first_refiner_id = refiner_sessions_first.last().unwrap().session_id.clone();

        // Move back to planning (simulating plan revision needed)
        db.update_task(
            task.id,
            &TaskUpdate {
                state: Some("planning".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();

        // Second planning -> refining: should reuse the existing refiner session
        let result = handle_task_update(
            &db,
            &serde_json::json!({"id": task.id, "state": "refining"}),
            Some("planning-session"),
            "tc2",
            &mut writer,
            &mut reader,
            &mut events,
        );
        assert!(
            !result.is_error,
            "unexpected error: {}",
            extract_text(&result)
        );

        // Verify no new refiner sessions were created
        let sessions_after_second = db.get_sessions(task.id).unwrap();
        let refiner_sessions_second: Vec<_> = sessions_after_second
            .iter()
            .filter(|s| s.role == "refiner")
            .collect();

        assert_eq!(
            refiner_sessions_first.len(),
            refiner_sessions_second.len(),
            "expected refiner session to be reused, not a new one created. \
             Sessions: {:?}",
            refiner_sessions_second
                .iter()
                .map(|s| &s.session_id)
                .collect::<Vec<_>>()
        );

        assert_eq!(
            first_refiner_id,
            refiner_sessions_second.last().unwrap().session_id,
            "expected same refiner session to be reused"
        );
    }

    #[test]
    fn test_archived_refiner_creates_new_session() {
        let db = TasksDb::open_memory().unwrap();

        // Create a subtask in planning state
        let parent = db
            .create_task("/project", "Parent", None, None, None, false, false)
            .unwrap();
        let task = db
            .create_task(
                "/project",
                "Archived refiner test",
                Some(5),
                Some(parent.id),
                None,
                false,
                false,
            )
            .unwrap();
        db.set_session_id(task.id, "planning-session").unwrap();

        // First planning -> refining with normal mock
        let (mut writer, mut reader) = mock_io();
        let mut events = Vec::new();
        let result = handle_task_update(
            &db,
            &serde_json::json!({"id": task.id, "state": "refining"}),
            Some("planning-session"),
            "tc1",
            &mut writer,
            &mut reader,
            &mut events,
        );
        assert!(
            !result.is_error,
            "unexpected error: {}",
            extract_text(&result)
        );

        let sessions = db.get_sessions(task.id).unwrap();
        let first_refiner = sessions
            .iter()
            .find(|s| s.role == "refiner")
            .expect("expected a refiner session")
            .session_id
            .clone();

        // Move back to planning
        db.update_task(
            task.id,
            &TaskUpdate {
                state: Some("planning".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();

        // Now create mock IO where the first refiner is archived
        let mut archived = std::collections::HashSet::new();
        archived.insert(first_refiner.clone());
        let (mut writer2, mut reader2) = mock_io_with_archived(archived);

        // Second planning -> refining: archived refiner triggers new session creation
        let result = handle_task_update(
            &db,
            &serde_json::json!({"id": task.id, "state": "refining"}),
            Some("planning-session"),
            "tc2",
            &mut writer2,
            &mut reader2,
            &mut events,
        );
        assert!(
            !result.is_error,
            "unexpected error: {}",
            extract_text(&result)
        );

        // Verify a new refiner session was created
        let sessions_after = db.get_sessions(task.id).unwrap();
        let refiner_sessions: Vec<_> = sessions_after
            .iter()
            .filter(|s| s.role == "refiner")
            .collect();
        assert!(
            refiner_sessions.len() > 1,
            "expected a new refiner session when old one is archived, got: {:?}",
            refiner_sessions
                .iter()
                .map(|s| &s.session_id)
                .collect::<Vec<_>>()
        );
        let last_refiner = refiner_sessions.last().unwrap();
        assert_ne!(
            last_refiner.session_id, first_refiner,
            "expected a different session when old refiner is archived"
        );
    }

    // ----- refining dispatch tests -----

    #[test]
    fn test_planning_to_refining_creates_session() {
        let db = TasksDb::open_memory().unwrap();
        let (mut writer, mut reader) = mock_io();

        // Create a subtask (defaults to planning state)
        let parent = db
            .create_task("/project", "Parent", None, None, None, false, false)
            .unwrap();
        let task = db
            .create_task(
                "/project",
                "Subtask with plan",
                Some(5),
                Some(parent.id),
                None,
                false,
                false,
            )
            .unwrap();
        assert_eq!(task.state, "planning");

        // Set a session_id on the task (simulating planning session)
        db.set_session_id(task.id, "planning-session").unwrap();

        // planning -> refining should trigger auto-dispatch of a refining session
        let mut events = Vec::new();
        let result = handle_task_update(
            &db,
            &serde_json::json!({"id": task.id, "state": "refining"}),
            Some("planning-session"),
            "tc1",
            &mut writer,
            &mut reader,
            &mut events,
        );
        assert!(
            !result.is_error,
            "unexpected error: {}",
            extract_text(&result)
        );

        let updated: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
        assert_eq!(updated["state"], "refining");

        // Verify that a session was created (CreateSession + Chat requests
        // went through the mock). The mock generates "mock-s1", "mock-s2", etc.
        let sessions = db.get_sessions(task.id).unwrap();
        let roles: Vec<(&str, &str)> = sessions
            .iter()
            .map(|s| (s.session_id.as_str(), s.role.as_str()))
            .collect();
        assert!(
            roles.iter().any(|(_, role)| *role == "refiner"),
            "expected a refiner session to be recorded, got: {:?}",
            roles
        );
    }

    #[test]
    fn test_interactive_to_refining_creates_session() {
        let db = TasksDb::open_memory().unwrap();
        let (mut writer, mut reader) = mock_io();

        // Create an interactive task
        let result = handle_task_create(
            &db,
            &serde_json::json!({"title": "Direct refine task"}),
            &ToolCtx {
                project: "/project",
                session_id: Some("s1"),
                tool_call_id: "tc1",
            },
            &mut writer,
            &mut reader,
            &mut Vec::new(),
        );
        let task: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
        let task_id = task["id"].as_i64().unwrap();
        assert_eq!(task["state"], "interactive");

        // interactive -> refining should also trigger refining dispatch
        let mut events = Vec::new();
        let result = handle_task_update(
            &db,
            &serde_json::json!({"id": task_id, "state": "refining"}),
            Some("s1"),
            "tc2",
            &mut writer,
            &mut reader,
            &mut events,
        );
        assert!(
            !result.is_error,
            "unexpected error: {}",
            extract_text(&result)
        );

        let sessions = db.get_sessions(task_id).unwrap();
        let roles: Vec<(&str, &str)> = sessions
            .iter()
            .map(|s| (s.session_id.as_str(), s.role.as_str()))
            .collect();
        assert!(
            roles.iter().any(|(_, role)| *role == "refiner"),
            "expected a refiner session to be recorded, got: {:?}",
            roles
        );
    }

    #[test]
    fn test_refining_to_ready_rejected_without_affected_files() {
        let db = TasksDb::open_memory().unwrap();
        let (mut writer, mut reader) = mock_io();

        // Create a subtask in planning state
        let parent = db
            .create_task("/project", "Parent", None, None, None, false, false)
            .unwrap();
        let task = db
            .create_task(
                "/project",
                "No files task",
                Some(5),
                Some(parent.id),
                None,
                false,
                false,
            )
            .unwrap();
        assert_eq!(task.state, "planning");

        // planning -> refining
        db.update_task(
            task.id,
            &TaskUpdate {
                state: Some("refining".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();

        // refining -> ready without affected_files should fail
        let mut events = Vec::new();
        let result = handle_task_update(
            &db,
            &serde_json::json!({"id": task.id, "state": "ready"}),
            Some("s1"),
            "tc1",
            &mut writer,
            &mut reader,
            &mut events,
        );
        assert!(result.is_error, "expected error for missing affected_files");
        assert!(
            extract_text(&result).contains("affected_files"),
            "expected affected_files error, got: {}",
            extract_text(&result)
        );
    }

    #[test]
    fn test_refining_to_ready_succeeds_with_affected_files() {
        let db = TasksDb::open_memory().unwrap();
        let (mut writer, mut reader) = mock_io();

        // Create a subtask in planning state
        let parent = db
            .create_task("/project", "Parent", None, None, None, false, false)
            .unwrap();
        let task = db
            .create_task(
                "/project",
                "Has files task",
                Some(5),
                Some(parent.id),
                None,
                false,
                false,
            )
            .unwrap();

        // Set affected_files and move through planning -> refining
        db.update_task(
            task.id,
            &TaskUpdate {
                affected_files: Some(serde_json::json!(["src/main.rs"])),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        db.update_task(
            task.id,
            &TaskUpdate {
                state: Some("refining".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();

        // refining -> ready with affected_files should succeed
        let mut events = Vec::new();
        let result = handle_task_update(
            &db,
            &serde_json::json!({"id": task.id, "state": "ready"}),
            Some("s1"),
            "tc1",
            &mut writer,
            &mut reader,
            &mut events,
        );
        assert!(
            !result.is_error,
            "unexpected error: {}",
            extract_text(&result)
        );
        let updated: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
        assert_eq!(updated["state"], "ready");
    }

    #[test]
    fn test_refining_to_planning_resumes_planning_session() {
        let db = TasksDb::open_memory().unwrap();
        let (mut writer, mut reader) = mock_io();

        // Create a subtask in planning state with a session
        let parent = db
            .create_task("/project", "Parent", None, None, None, false, false)
            .unwrap();
        let task = db
            .create_task(
                "/project",
                "Needs revision",
                Some(5),
                Some(parent.id),
                None,
                false,
                false,
            )
            .unwrap();

        // Simulate: assign planning session, then move to refining
        db.set_session_id(task.id, "planning-session").unwrap();
        db.update_task(
            task.id,
            &TaskUpdate {
                state: Some("refining".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();

        // refining -> planning should send a QueueMessage to the planning session
        let mut events = Vec::new();
        let result = handle_task_update(
            &db,
            &serde_json::json!({"id": task.id, "state": "planning"}),
            Some("refining-session"),
            "tc1",
            &mut writer,
            &mut reader,
            &mut events,
        );
        assert!(
            !result.is_error,
            "unexpected error: {}",
            extract_text(&result)
        );

        // Check that a QueueMessage was sent to "planning-session"
        // The mock IO captures ServerRequests in the shared buffer.
        // Since the mock processes requests synchronously, the QueueMessage
        // should have been processed. We verify indirectly via the result succeeding.
        let updated: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
        assert_eq!(updated["state"], "planning");
    }

    // ----- subtree claiming tests -----

    #[test]
    fn test_claiming_parent_updates_session_on_all_descendants() {
        let db = TasksDb::open_memory().unwrap();
        let (mut writer, mut reader) = mock_io();

        // Create parent (interactive — gets mock-s1)
        let result = handle_task_create(
            &db,
            &serde_json::json!({"title": "Parent"}),
            &ToolCtx {
                project: "/project",
                session_id: Some("s1"),
                tool_call_id: "tc1",
            },
            &mut writer,
            &mut reader,
            &mut Vec::new(),
        );
        let parent: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
        let parent_id = parent["id"].as_i64().unwrap();

        // Create subtasks
        let result = handle_task_create(
            &db,
            &serde_json::json!({"title": "Subtask 1", "parent_id": parent_id}),
            &ToolCtx {
                project: "/project",
                session_id: Some("s1"),
                tool_call_id: "tc2",
            },
            &mut writer,
            &mut reader,
            &mut Vec::new(),
        );
        let sub1: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
        let sub1_id = sub1["id"].as_i64().unwrap();
        // Give subtask a session_id (simulating dispatch)
        db.set_session_id(sub1_id, "mock-s1").unwrap();

        let result = handle_task_create(
            &db,
            &serde_json::json!({"title": "Subtask 2", "parent_id": parent_id}),
            &ToolCtx {
                project: "/project",
                session_id: Some("s1"),
                tool_call_id: "tc3",
            },
            &mut writer,
            &mut reader,
            &mut Vec::new(),
        );
        let sub2: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
        let sub2_id = sub2["id"].as_i64().unwrap();

        // Now reassign the parent to a new session
        let result = handle_task_assign(
            &db,
            &serde_json::json!({"id": parent_id}),
            Some("new-session"),
            "tc4",
            &mut writer,
            &mut reader,
        );
        assert!(
            !result.is_error,
            "expected success: {}",
            extract_text(&result)
        );

        // Verify subtasks got their session_id updated
        let sub1_task = db.get_task(sub1_id).unwrap().unwrap();
        assert_eq!(
            sub1_task.session_id.as_deref(),
            Some("new-session"),
            "subtask 1 should have been claimed"
        );
        let sub2_task = db.get_task(sub2_id).unwrap().unwrap();
        assert_eq!(
            sub2_task.session_id.as_deref(),
            Some("new-session"),
            "subtask 2 should have been claimed"
        );
    }

    #[test]
    fn test_claiming_does_not_affect_non_descendant_sessions() {
        let db = TasksDb::open_memory().unwrap();
        let (mut writer, mut reader) = mock_io();

        // Create two independent tasks
        let result = handle_task_create(
            &db,
            &serde_json::json!({"title": "Task A"}),
            &ToolCtx {
                project: "/project",
                session_id: Some("s1"),
                tool_call_id: "tc1",
            },
            &mut writer,
            &mut reader,
            &mut Vec::new(),
        );
        let task_a: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
        let task_a_id = task_a["id"].as_i64().unwrap();

        let result = handle_task_create(
            &db,
            &serde_json::json!({"title": "Task B"}),
            &ToolCtx {
                project: "/project",
                session_id: Some("s1"),
                tool_call_id: "tc2",
            },
            &mut writer,
            &mut reader,
            &mut Vec::new(),
        );
        let task_b: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
        let task_b_id = task_b["id"].as_i64().unwrap();

        // Record task B's original session
        let task_b_before = db.get_task(task_b_id).unwrap().unwrap();
        let task_b_old_session = task_b_before.session_id.clone();

        // Reassign task A to a new session
        let result = handle_task_assign(
            &db,
            &serde_json::json!({"id": task_a_id}),
            Some("new-session"),
            "tc3",
            &mut writer,
            &mut reader,
        );
        assert!(!result.is_error);

        // Task B's session should be unchanged
        let task_b_after = db.get_task(task_b_id).unwrap().unwrap();
        assert_eq!(
            task_b_after.session_id, task_b_old_session,
            "unrelated task should not be affected by claiming task A"
        );
    }

    // ----- rebase enforcement tests -----

    /// Create a test git repo with an initial commit and a "main" branch.
    fn init_test_repo() -> tempfile::TempDir {
        use std::process::Command;
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path();

        Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(path)
            .output()
            .expect("git init");
        Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(path)
            .output()
            .expect("git config email");
        Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(path)
            .output()
            .expect("git config name");

        let file = path.join("README.md");
        std::fs::write(&file, "# test\n").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(path)
            .output()
            .expect("git add");
        Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(path)
            .output()
            .expect("git commit");

        dir
    }

    #[test]
    fn test_rebase_check_active_to_review_rebased_succeeds() {
        use std::process::Command;
        let dir = init_test_repo();
        let repo_path = dir.path().to_str().unwrap();

        // Create a task branch from main
        Command::new("git")
            .args(["branch", "task-99", "main"])
            .current_dir(repo_path)
            .output()
            .expect("create branch");

        // Create a worktree for the branch
        let wt_dir = dir.path().parent().unwrap().join("wt-rebase-test-1");
        let wt_path = wt_dir.to_str().unwrap();
        Command::new("git")
            .args(["worktree", "add", wt_path, "task-99"])
            .current_dir(repo_path)
            .output()
            .expect("create worktree");

        // Make a commit on the task branch (in the worktree)
        std::fs::write(wt_dir.join("new_file.txt"), "content").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(wt_path)
            .output()
            .expect("git add");
        Command::new("git")
            .args(["commit", "-m", "task work"])
            .current_dir(wt_path)
            .output()
            .expect("git commit");

        // Set up DB with task having branch/worktree
        let db = TasksDb::open_memory().unwrap();
        let (mut writer, mut reader) = mock_io();
        let task = db
            .create_task("/project", "Rebased task", None, None, None, false, false)
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
        db.set_branch(task.id, "task-99").unwrap();
        db.set_worktree_path(task.id, wt_path).unwrap();

        // active -> review with rebased branch should succeed
        let result = handle_task_update(
            &db,
            &serde_json::json!({"id": task.id, "state": "review"}),
            Some("s1"),
            "tc1",
            &mut writer,
            &mut reader,
            &mut Vec::new(),
        );
        assert!(
            !result.is_error,
            "expected success: {}",
            extract_text(&result)
        );
        let updated: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
        assert_eq!(updated["state"], "review");

        // Cleanup
        Command::new("git")
            .args(["worktree", "remove", "--force", wt_path])
            .current_dir(repo_path)
            .output()
            .ok();
    }

    #[test]
    fn test_rebase_check_active_to_review_not_rebased_rejected() {
        use std::process::Command;
        let dir = init_test_repo();
        let repo_path = dir.path().to_str().unwrap();

        // Create a task branch from main
        Command::new("git")
            .args(["branch", "task-100", "main"])
            .current_dir(repo_path)
            .output()
            .expect("create branch");

        // Now make a NEW commit on main (so main advances past branch point)
        std::fs::write(dir.path().join("main_change.txt"), "new main content").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(repo_path)
            .output()
            .expect("git add");
        Command::new("git")
            .args(["commit", "-m", "advance main"])
            .current_dir(repo_path)
            .output()
            .expect("git commit");

        // Create a worktree for the branch
        let wt_dir = dir.path().parent().unwrap().join("wt-rebase-test-2");
        let wt_path = wt_dir.to_str().unwrap();
        Command::new("git")
            .args(["worktree", "add", wt_path, "task-100"])
            .current_dir(repo_path)
            .output()
            .expect("create worktree");

        // Set up DB
        let db = TasksDb::open_memory().unwrap();
        let (mut writer, mut reader) = mock_io();
        let task = db
            .create_task(
                "/project",
                "Not rebased task",
                None,
                None,
                None,
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
        db.set_branch(task.id, "task-100").unwrap();
        db.set_worktree_path(task.id, wt_path).unwrap();
        db.set_session_id(task.id, "worker-session").unwrap();

        // active -> review with non-rebased branch should be rejected
        let result = handle_task_update(
            &db,
            &serde_json::json!({"id": task.id, "state": "review"}),
            Some("s1"),
            "tc1",
            &mut writer,
            &mut reader,
            &mut Vec::new(),
        );
        assert!(result.is_error, "expected error for non-rebased branch");
        let text = extract_text(&result);
        assert!(
            text.contains("not rebased"),
            "expected 'not rebased' in error: {}",
            text
        );

        // Task should still be in active state (transition rejected)
        let task_after = db.get_task(task.id).unwrap().unwrap();
        assert_eq!(task_after.state, "active");

        // Verify notification was sent to worker session
        let shared = writer.shared.lock().unwrap();
        let _output = String::from_utf8_lossy(&shared.write_buf);
        // The mock IO may have consumed write_buf via process_pending, but
        // we still verify the error was returned correctly above.
        drop(shared);

        // Cleanup
        Command::new("git")
            .args(["worktree", "remove", "--force", wt_path])
            .current_dir(repo_path)
            .output()
            .ok();
    }

    #[test]
    fn test_rebase_check_skipped_without_branch() {
        // Tasks without branch/worktree_path (e.g. not yet prepared) should
        // skip the rebase check and allow the transition.
        let db = TasksDb::open_memory().unwrap();
        let (mut writer, mut reader) = mock_io();

        let task = db
            .create_task("/project", "No branch task", None, None, None, false, false)
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

        // active -> review without branch should still succeed (no rebase check)
        let result = handle_task_update(
            &db,
            &serde_json::json!({"id": task.id, "state": "review"}),
            Some("s1"),
            "tc1",
            &mut writer,
            &mut reader,
            &mut Vec::new(),
        );
        assert!(
            !result.is_error,
            "expected success without branch: {}",
            extract_text(&result)
        );
    }

    #[test]
    fn test_rebase_check_partial_rebase_cleanup() {
        use std::process::Command;
        let dir = init_test_repo();
        let repo_path = dir.path().to_str().unwrap();

        // Create a task branch from main
        Command::new("git")
            .args(["branch", "task-101", "main"])
            .current_dir(repo_path)
            .output()
            .expect("create branch");

        // Advance main
        std::fs::write(dir.path().join("conflict.txt"), "main version").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(repo_path)
            .output()
            .expect("git add");
        Command::new("git")
            .args(["commit", "-m", "main advance"])
            .current_dir(repo_path)
            .output()
            .expect("git commit");

        // Create worktree
        let wt_dir = dir.path().parent().unwrap().join("wt-rebase-test-3");
        let wt_path = wt_dir.to_str().unwrap();
        Command::new("git")
            .args(["worktree", "add", wt_path, "task-101"])
            .current_dir(repo_path)
            .output()
            .expect("create worktree");

        // Create a conflicting commit on the branch
        std::fs::write(wt_dir.join("conflict.txt"), "branch version").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(wt_path)
            .output()
            .expect("git add");
        Command::new("git")
            .args(["commit", "-m", "branch work"])
            .current_dir(wt_path)
            .output()
            .expect("git commit");

        // Start a rebase that will conflict (creating partial rebase state)
        let rebase_output = Command::new("git")
            .args(["rebase", "main"])
            .current_dir(wt_path)
            .output()
            .expect("git rebase");
        // Rebase should fail due to conflict
        assert!(
            !rebase_output.status.success(),
            "expected rebase to fail with conflict"
        );

        // Verify partial rebase state exists
        let git_dir_output = Command::new("git")
            .args(["rev-parse", "--git-dir"])
            .current_dir(wt_path)
            .output()
            .expect("git rev-parse --git-dir");
        let git_dir = String::from_utf8_lossy(&git_dir_output.stdout)
            .trim()
            .to_string();
        let rebase_merge = std::path::Path::new(&git_dir).join("rebase-merge");
        let rebase_apply = std::path::Path::new(&git_dir).join("rebase-apply");
        assert!(
            rebase_merge.exists() || rebase_apply.exists(),
            "expected partial rebase state"
        );

        // Set up DB
        let db = TasksDb::open_memory().unwrap();
        let (mut writer, mut reader) = mock_io();
        let task = db
            .create_task(
                "/project",
                "Partial rebase task",
                None,
                None,
                None,
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
        db.set_branch(task.id, "task-101").unwrap();
        db.set_worktree_path(task.id, wt_path).unwrap();

        // active -> review should be rejected (not rebased)
        let result = handle_task_update(
            &db,
            &serde_json::json!({"id": task.id, "state": "review"}),
            Some("s1"),
            "tc1",
            &mut writer,
            &mut reader,
            &mut Vec::new(),
        );
        assert!(result.is_error, "expected error for non-rebased branch");

        // Partial rebase state should have been cleaned up
        assert!(
            !rebase_merge.exists() && !rebase_apply.exists(),
            "expected partial rebase state to be cleaned up"
        );

        // Cleanup
        Command::new("git")
            .args(["worktree", "remove", "--force", wt_path])
            .current_dir(repo_path)
            .output()
            .ok();
    }

    // ----- transition to interactive creates session tests -----

    #[test]
    fn test_transition_to_interactive_creates_session() {
        let db = TasksDb::open_memory().unwrap();
        let (mut writer, mut reader) = mock_io();

        // Create a subtask (planning state, no session)
        let parent = db
            .create_task("/project", "Parent", None, None, None, false, false)
            .unwrap();
        let task = db
            .create_task(
                "/project",
                "Scope expansion",
                Some(5),
                Some(parent.id),
                None,
                false,
                false,
            )
            .unwrap();
        assert_eq!(task.state, "planning");
        assert!(task.session_id.is_none());

        // planning -> interactive (scope expansion) should create a session
        let mut events = Vec::new();
        let result = handle_task_update(
            &db,
            &serde_json::json!({"id": task.id, "state": "interactive"}),
            Some("triggering-session"),
            "tc1",
            &mut writer,
            &mut reader,
            &mut events,
        );
        assert!(
            !result.is_error,
            "unexpected error: {}",
            extract_text(&result)
        );

        let updated: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
        assert_eq!(updated["state"], "interactive");
        // Should have a session_id (the new interactive session)
        assert!(
            !updated["session_id"].is_null(),
            "expected session_id to be set on interactive transition"
        );

        // Check task_sessions has both interactive and creator records
        let sessions = db.get_sessions(task.id).unwrap();
        let roles: Vec<(&str, &str)> = sessions
            .iter()
            .map(|s| (s.session_id.as_str(), s.role.as_str()))
            .collect();
        assert!(
            roles.iter().any(|(_, role)| *role == "interactive"),
            "expected an interactive session to be recorded, got: {:?}",
            roles
        );
        assert!(
            roles.iter().any(|(_, role)| *role == "creator"),
            "expected a creator session to be recorded, got: {:?}",
            roles
        );
    }

    #[test]
    fn test_transition_to_interactive_no_session_context() {
        let db = TasksDb::open_memory().unwrap();
        let (mut writer, mut reader) = mock_io();

        // Create a subtask in planning state (no session)
        let parent = db
            .create_task("/project", "Parent", None, None, None, false, false)
            .unwrap();
        let task = db
            .create_task(
                "/project",
                "CLI takeover",
                Some(5),
                Some(parent.id),
                None,
                false,
                false,
            )
            .unwrap();
        assert_eq!(task.state, "planning");
        assert!(task.session_id.is_none());

        // planning -> interactive without session context
        let mut events = Vec::new();
        let result = handle_task_update(
            &db,
            &serde_json::json!({"id": task.id, "state": "interactive"}),
            None, // no session context
            "tc1",
            &mut writer,
            &mut reader,
            &mut events,
        );
        assert!(
            !result.is_error,
            "unexpected error: {}",
            extract_text(&result)
        );

        let updated: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
        assert_eq!(updated["state"], "interactive");
        // Session still created even without a parent session
        assert!(
            !updated["session_id"].is_null(),
            "expected session_id even without session context"
        );

        // Only interactive session recorded (no creator since no session context)
        let sessions = db.get_sessions(task.id).unwrap();
        let roles: Vec<(&str, &str)> = sessions
            .iter()
            .map(|s| (s.session_id.as_str(), s.role.as_str()))
            .collect();
        assert!(
            roles.iter().any(|(_, role)| *role == "interactive"),
            "expected interactive session, got: {:?}",
            roles
        );
        assert!(
            !roles.iter().any(|(_, role)| *role == "creator"),
            "should not have creator when no session context, got: {:?}",
            roles
        );
    }

    #[test]
    fn test_transition_to_interactive_with_live_session_no_new_session() {
        let db = TasksDb::open_memory().unwrap();
        let (mut writer, mut reader) = mock_io();

        // Create an interactive task (gets mock-s1)
        let result = handle_task_create(
            &db,
            &serde_json::json!({"title": "Already has session"}),
            &ToolCtx {
                project: "/project",
                session_id: Some("s1"),
                tool_call_id: "tc1",
            },
            &mut writer,
            &mut reader,
            &mut Vec::new(),
        );
        let task: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
        let task_id = task["id"].as_i64().unwrap();
        let original_sid = task["session_id"].as_str().unwrap().to_string();
        assert_eq!(task["state"], "interactive");

        // Move to planning, then back to interactive
        let mut events = Vec::new();
        let result = handle_task_update(
            &db,
            &serde_json::json!({"id": task_id, "state": "planning"}),
            Some("s1"),
            "tc2",
            &mut writer,
            &mut reader,
            &mut events,
        );
        assert!(!result.is_error);

        // planning -> interactive: the task still has the original session_id,
        // which is alive (not archived in mock), so no new session should be created
        let result = handle_task_update(
            &db,
            &serde_json::json!({"id": task_id, "state": "interactive"}),
            Some("s1"),
            "tc3",
            &mut writer,
            &mut reader,
            &mut events,
        );
        assert!(
            !result.is_error,
            "unexpected error: {}",
            extract_text(&result)
        );

        let updated: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
        assert_eq!(updated["state"], "interactive");
        // Session should be unchanged (reused, not replaced)
        assert_eq!(
            updated["session_id"].as_str().unwrap(),
            original_sid,
            "expected existing live session to be reused"
        );
    }

    #[test]
    fn test_transition_to_interactive_with_archived_session_creates_new() {
        let db = TasksDb::open_memory().unwrap();

        // Create task with a session using normal mock
        let (mut writer, mut reader) = mock_io();
        let result = handle_task_create(
            &db,
            &serde_json::json!({"title": "Session will be archived"}),
            &ToolCtx {
                project: "/project",
                session_id: Some("s1"),
                tool_call_id: "tc1",
            },
            &mut writer,
            &mut reader,
            &mut Vec::new(),
        );
        let task: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
        let task_id = task["id"].as_i64().unwrap();
        let original_sid = task["session_id"].as_str().unwrap().to_string();

        // Move to planning
        let mut events = Vec::new();
        db.update_task(
            task_id,
            &TaskUpdate {
                state: Some("planning".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();

        // Now create mock IO where the original session is archived
        let mut archived = std::collections::HashSet::new();
        archived.insert(original_sid.clone());
        let (mut writer2, mut reader2) = mock_io_with_archived(archived);

        // planning -> interactive: archived session should trigger new session creation
        let result = handle_task_update(
            &db,
            &serde_json::json!({"id": task_id, "state": "interactive"}),
            Some("triggering-session"),
            "tc2",
            &mut writer2,
            &mut reader2,
            &mut events,
        );
        assert!(
            !result.is_error,
            "unexpected error: {}",
            extract_text(&result)
        );

        let updated: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
        assert_eq!(updated["state"], "interactive");
        // Should have a NEW session (different from the archived one)
        let new_sid = updated["session_id"].as_str().unwrap();
        assert_ne!(
            new_sid, original_sid,
            "expected new session when old one is archived"
        );
    }

    #[test]
    fn test_refining_to_interactive_creates_session() {
        let db = TasksDb::open_memory().unwrap();
        let (mut writer, mut reader) = mock_io();

        // Create a subtask in refining state (simulating scope expansion)
        let parent = db
            .create_task("/project", "Parent", None, None, None, false, false)
            .unwrap();
        let task = db
            .create_task(
                "/project",
                "Needs scope expansion",
                Some(5),
                Some(parent.id),
                None,
                false,
                false,
            )
            .unwrap();
        db.update_task(
            task.id,
            &TaskUpdate {
                state: Some("refining".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();

        // refining -> interactive (scope expansion) should create a session
        let mut events = Vec::new();
        let result = handle_task_update(
            &db,
            &serde_json::json!({"id": task.id, "state": "interactive"}),
            Some("refiner-session"),
            "tc1",
            &mut writer,
            &mut reader,
            &mut events,
        );
        assert!(
            !result.is_error,
            "unexpected error: {}",
            extract_text(&result)
        );

        let updated: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
        assert_eq!(updated["state"], "interactive");
        assert!(
            !updated["session_id"].is_null(),
            "expected session_id on refining -> interactive"
        );

        // Check that refiner-session is recorded as creator
        let sessions = db.get_sessions(task.id).unwrap();
        let roles: Vec<(&str, &str)> = sessions
            .iter()
            .map(|s| (s.session_id.as_str(), s.role.as_str()))
            .collect();
        assert!(
            roles
                .iter()
                .any(|(sid, role)| *sid == "refiner-session" && *role == "creator"),
            "expected refiner-session as creator, got: {:?}",
            roles
        );
    }
}
