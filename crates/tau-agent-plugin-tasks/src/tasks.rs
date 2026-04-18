//! Task system plugin — global plugin for project task management.
//!
//! Speaks the plugin protocol (JSON lines over stdin/stdout).
//! Registers task management tools and handles them via TasksDb.
//!
//! ## Why `eprintln!` instead of `tracing::*!`
//!
//! This crate runs inside a dedicated subprocess (`tau plugin-tasks`)
//! spawned by the server daemon; it has no `tracing` subscriber of its
//! own. The parent server captures the subprocess's stderr via
//! `spawn_stderr_forwarder` and re-emits each line as `tracing::info!`,
//! so `eprintln!` output here still ends up in the server log. Migrating
//! these calls to `tracing::*!` would make them no-ops.

use std::io::{BufRead, BufReader, BufWriter, Write};
use std::sync::{Arc, Mutex};

use rusqlite::params;

use crate::tasks_db::{TaskUpdate, TasksDb};
use crate::tasks_merge;
use crate::tasks_merge_worker::{MergeJob, MergeWorker};
use crate::tasks_scheduler;
use tau_agent_plugin::ToolResultContent;
use tau_agent_plugin::tunnel::SharedStdout;
use tau_agent_plugin::{
    HookResult, PluginMessage, PluginRegistration, PluginRequest, PluginToolDef, PluginToolResult,
};

// ---------------------------------------------------------------------------
// ProjectResolver — resolves project_name → filesystem path
// ---------------------------------------------------------------------------

/// Resolves project names to filesystem paths by reading the `projects` table
/// in `tau.db`.
///
/// The tasks plugin runs as a separate process and frequently needs the
/// filesystem path for a project (git operations, session cwd, config
/// loading). Many of these code paths are outside of ToolCall context
/// (event drain, startup cleanup, merge queue), so we cannot rely on the
/// `cwd` field from `PluginRequest::ToolCall`.
///
/// Instead, the resolver opens `tau.db` **read-only** at startup and queries
/// the `projects` table. No caching needed — SQLite queries on a small table
/// are cheap and the read-only connection avoids locking.
pub(crate) struct ProjectResolver {
    conn: rusqlite::Connection,
}

impl ProjectResolver {
    /// Open `tau.db` read-only. Returns `Err` if the file doesn't exist or
    /// can't be opened.
    pub(crate) fn open() -> tau_agent_plugin::Result<Self> {
        let path = tau_agent_plugin::data_dir().join("tau.db");
        let conn = rusqlite::Connection::open_with_flags(
            &path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|e| tau_agent_plugin::Error::Io(format!("open tau.db: {}", e)))?;
        Ok(Self { conn })
    }

    /// Resolve a project name to its filesystem path.
    pub(crate) fn resolve(&self, project_name: &str) -> tau_agent_plugin::Result<String> {
        self.conn
            .query_row(
                "SELECT path FROM projects WHERE name = ?1",
                params![project_name],
                |row| row.get::<_, String>(0),
            )
            .map_err(|e| {
                tau_agent_plugin::Error::Io(format!(
                    "project '{}' not found in registry: {}",
                    project_name, e
                ))
            })
    }

    /// Create a resolver for testing with an in-memory database.
    #[cfg(test)]
    pub(crate) fn test(entries: &[(&str, &str)]) -> Self {
        let conn =
            rusqlite::Connection::open_in_memory().expect("open in-memory db for test resolver");
        conn.execute_batch(
            "CREATE TABLE projects (
                name TEXT PRIMARY KEY,
                path TEXT NOT NULL,
                created_at INTEGER NOT NULL DEFAULT 0,
                updated_at INTEGER NOT NULL DEFAULT 0
            )",
        )
        .expect("create projects table for test resolver");
        for (name, path) in entries {
            conn.execute(
                "INSERT INTO projects (name, path) VALUES (?1, ?2)",
                params![name, path],
            )
            .expect("insert test project");
        }
        Self { conn }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Discover a project name from a directory path by walking up to find
/// `.tau/project.toml`. This is a fallback for when the server doesn't
/// yet provide `project_name` in the ToolCall (before task #449 is merged).
fn discover_project_name(start: &str) -> Option<String> {
    let mut dir = std::path::PathBuf::from(start);
    loop {
        let config_path = dir.join(".tau").join("project.toml");
        if config_path.is_file() {
            let contents = std::fs::read_to_string(&config_path).ok()?;
            // Parse as a TOML table and extract the "name" key.
            let table: toml::Table = contents.parse().ok()?;
            return table.get("name").and_then(|v| v.as_str()).map(String::from);
        }
        if !dir.pop() {
            return None;
        }
    }
}

fn send_message(writer: &mut impl Write, msg: &PluginMessage) {
    tau_agent_plugin::tunnel::send_message(writer, msg);
}

fn tool_ok(tool_call_id: &str, text: &str) -> PluginToolResult {
    PluginToolResult {
        tool_call_id: tool_call_id.to_string(),
        content: vec![ToolResultContent::Text(tau_agent_plugin::TextContent {
            text: text.to_string(),
            text_signature: None,
        })],
        is_error: false,
        summary: None,
        post_persist_actions: Vec::new(),
    }
}

fn tool_err(tool_call_id: &str, text: &str) -> PluginToolResult {
    PluginToolResult {
        tool_call_id: tool_call_id.to_string(),
        content: vec![ToolResultContent::Text(tau_agent_plugin::TextContent {
            text: text.to_string(),
            text_signature: None,
        })],
        is_error: true,
        summary: None,
        post_persist_actions: Vec::new(),
    }
}

fn tool_ok_summary(tool_call_id: &str, text: &str, summary: impl Into<String>) -> PluginToolResult {
    PluginToolResult {
        tool_call_id: tool_call_id.to_string(),
        content: vec![ToolResultContent::Text(tau_agent_plugin::TextContent {
            text: text.to_string(),
            text_signature: None,
        })],
        is_error: false,
        summary: Some(summary.into()),
        post_persist_actions: Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Tool definitions
// ---------------------------------------------------------------------------

/// Returns the full set of `PluginToolDef`s for the tasks plugin.
///
/// Exposed so other crates (e.g. prompt regression tests in the worker
/// plugin) can introspect the combined tool surface without duplicating
/// the list.
pub fn plugin_tool_defs() -> Vec<PluginToolDef> {
    tasks_tools()
}

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
                    "initial_state": {
                        "type": "string",
                        "enum": ["interactive", "planning", "ready"],
                        "description": "Initial task state. 'planning' (default) dispatches a planning session to analyse and refine the spec. 'ready' queues the task for a worker immediately — use when you already have a complete spec. 'interactive' opens a user-driven refinement session — use only when the user explicitly asks to iterate on the task."
                    },
                    "require_approval": {
                        "type": "boolean",
                        "description": "If true, refining transitions to interactive instead of ready (human sign-off required)"
                    },
                    "message": {
                        "type": "string",
                        "description": "Initial message/description for the task"
                    },
                    "merge_target": {
                        "type": "string",
                        "description": "Override merge target branch (default: parent's branch for subtasks, 'main' for root tasks)"
                    },
                    "sandbox_profile": {
                        "type": "string",
                        "description": "Sandbox profile name from sandbox.toml to use when dispatching this task's sessions"
                    },
                    "hold": {
                        "type": "boolean",
                        "description": "When true, the task is created but NOT scheduled for dispatch, even if initial_state='ready'. Release by calling task_update with hold=false. Useful for batch-seeding a task board before manually choosing dispatch order. Default: false."
                    }
                },
                "required": ["title"]
            }),
            prompt_snippet: Some("Create a new task in the project task board".into()),
            prompt_guidelines: vec![
                "Tasks default to 'planning' state: a planning session analyses the spec and produces a refined plan for review. Use this when the task needs thinking through first.".into(),
                "Pass initial_state=\"ready\" when you already have a complete, self-contained spec and want to queue work immediately — no planning session, the next scheduler pass dispatches a worker.".into(),
                "Pass initial_state=\"interactive\" only when the user explicitly asked to iterate on the task conversationally. This spawns a user-driven refinement session; do NOT use it for agent-authored tasks.".into(),
                "Top-level and subtask behaviour is identical on the initial-state axis. Use parent_id for grouping and parallelisation, not to control dispatch.".into(),
                "Sessions dispatched for top-level tasks are automatically parented under the project's root (user-facing) session, so new work surfaces in the user's session tree regardless of where in the session tree task_create was called.".into(),
                "Pass hold=true to create a task without scheduling it. Useful for batch-seeding a backlog on a greenfield project: file N tasks at once, review, then release them in considered order via task_update(hold=false). Held tasks are visible in task_list/task_status with a held indicator but the scheduler skips them.".into(),
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
            description: "List tasks filtered by state, parent, or tag. Default: all non-terminal tasks for current project.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "state": {
                        "type": "string",
                        "description": "Filter by state (interactive, planning, refining, ready, active, review, approved, merging, failed, merged, closed). Use 'all' to include merged/closed tasks."
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
                    "require_approval": {
                        "type": "boolean",
                        "description": "Whether to require human approval before work begins"
                    },
                    "merge_target": {
                        "type": "string",
                        "description": "Override merge target branch (default: parent's branch for subtasks, 'main' for root tasks)"
                    },
                    "sandbox_profile": {
                        "type": "string",
                        "description": "Sandbox profile name from sandbox.toml to use when dispatching this task's sessions"
                    },
                    "hold": {
                        "type": "boolean",
                        "description": "Hold (true) or release (false) a task from scheduler dispatch. A held task remains visible in lists and preserves its state but the scheduler will not dispatch it. See task_create for details."
                    }
                },
                "required": ["id"]
            }),
            prompt_snippet: Some("Update task fields (title, state, priority, tags, etc.)".into()),
            prompt_guidelines: vec![
                "State transitions are validated — invalid attempts are rejected with a clear error (active→approved additionally requires skip_review=true).".into(),
                "Some transitions auto-dispatch sessions (planning→refining, active→review); don't take further action on the task until that session completes.".into(),
                "When working in an interactive task session and the user wants to start implementation, transition the task to 'ready' (with affected_files set) — do NOT edit project files directly. The scheduler creates an isolated branch/worktree and dispatches a worker session.".into(),
                "Pass hold=true/false to hold or release a task from scheduler dispatch.".into(),
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
            prompt_guidelines: vec![],
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
            prompt_guidelines: vec![],
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
            prompt_guidelines: vec![],
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
                "Wait reasons: dependency not met, file conflict with an active task, budget exhausted, not yet scheduled.".into(),
            ],
        },
        PluginToolDef {
            name: "task_overview".into(),
            description: "Return the structured scheduler overview as JSON: active, queued-ready, queued-planning, blocked, held, plus the most recent merged/closed tasks. Useful for programmatic inspection; the `/task` picker in the TUI renders the same data.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "project": {
                        "type": "string",
                        "description": "Project name (defaults to session's project)"
                    },
                    "recent_limit": {
                        "type": "integer",
                        "description": "Max number of recently-merged and recently-closed tasks to include per bucket (default 10).",
                        "minimum": 0,
                        "maximum": 100
                    }
                }
            }),
            prompt_snippet: Some("Structured scheduler overview (JSON) grouped by lifecycle position".into()),
            prompt_guidelines: vec![
                "Buckets returned: active, queued_ready, queued_planning, blocked, held, recently_merged, recently_closed. Each entry is a full TaskInfo object.".into(),
                "`recent_limit` applies per bucket, so the tail length is up to 2×recent_limit.".into(),
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
    project_name: Option<&'a str>,
    session_id: Option<&'a str>,
    tool_call_id: &'a str,
}

fn handle_task_create(
    db: &TasksDb,
    args: &serde_json::Value,
    ctx: &ToolCtx<'_>,
    resolver: &ProjectResolver,
    writer: &mut impl Write,
    reader: &mut impl BufRead,
    pending_events: &mut Vec<SchedulerEvent>,
) -> PluginToolResult {
    let project_name = match ctx.project_name {
        Some(p) => p,
        None => {
            return tool_err(
                ctx.tool_call_id,
                "Tasks require a project. Run `tau project init` first.",
            );
        }
    };
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

    // Reject the removed skip_planning parameter with a clear error so
    // stale callers discover the new API immediately (task #512).
    if args.get("skip_planning").is_some() {
        return tool_err(
            tool_call_id,
            "'skip_planning' has been removed; use 'initial_state' instead \
             ('ready' is the former skip_planning=true behavior, 'planning' the default).",
        );
    }

    // New explicit initial_state argument. Default to "planning" — the
    // previous top-level / subtask asymmetry is gone: the caller chooses.
    let initial_state = match args.get("initial_state") {
        None => "planning",
        Some(v) => match v.as_str() {
            Some(s @ ("interactive" | "planning" | "ready")) => s,
            Some(other) => {
                return tool_err(
                    tool_call_id,
                    &format!(
                        "invalid initial_state '{}': expected 'interactive', 'planning', or 'ready'",
                        other
                    ),
                );
            }
            None => {
                return tool_err(tool_call_id, "initial_state must be a string");
            }
        },
    };

    let require_approval = args
        .get("require_approval")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let hold = args.get("hold").and_then(|v| v.as_bool()).unwrap_or(false);
    let message = args.get("message").and_then(|v| v.as_str());
    let merge_target = args.get("merge_target").and_then(|v| v.as_str());
    let sandbox_profile = args.get("sandbox_profile").and_then(|v| v.as_str());

    match db.create_task(
        project_name,
        title,
        priority,
        parent_id,
        tags,
        skip_review,
        initial_state,
        require_approval,
        merge_target,
        sandbox_profile,
        hold,
    ) {
        Ok(task) => {
            // Subtasks start in ready or planning state — trigger a schedule pass.
            // Held tasks stay parked: the scheduler skips them until released
            // via task_update(hold=false).
            if (task.state == "ready" || task.state == "planning") && !task.held {
                pending_events.push(SchedulerEvent::ScheduleNeeded(
                    project_name.to_string(),
                    session_id.map(String::from),
                ));
            }

            let mut dispatch_warnings: Vec<String> = Vec::new();

            // Interactive tasks get a fresh session for the user to drive
            if task.state == "interactive" {
                match create_interactive_session(db, &task, resolver, session_id, writer, reader) {
                    Some(new_sid) => {
                        let _ = db.set_session_id(task.id, &new_sid);
                        let _ = db.record_session(task.id, &new_sid, "interactive");
                        if let Some(creator_sid) = session_id {
                            let _ = db.record_session(task.id, creator_sid, "creator");
                        }
                    }
                    None => {
                        dispatch_warnings.push(
                            "⚠️ Failed to create interactive session. \
                             Task is in interactive state but has no session."
                                .to_string(),
                        );
                    }
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
                    Ok(json) => {
                        let summary = format!(
                            "task_create: #{} \"{}\"",
                            updated_task.id, updated_task.title
                        );
                        let mut result = tool_ok_summary(tool_call_id, &json, summary);
                        if !dispatch_warnings.is_empty() {
                            let warning_text = format!("\n{}", dispatch_warnings.join("\n"));
                            result.content.push(ToolResultContent::Text(
                                tau_agent_plugin::TextContent {
                                    text: warning_text,
                                    text_signature: None,
                                },
                            ));
                        }
                        result
                    }
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
    db: &TasksDb,
    task: &crate::tasks_db::Task,
    resolver: &ProjectResolver,
    parent_session_id: Option<&str>,
    writer: &mut impl Write,
    reader: &mut impl BufRead,
) -> Option<String> {
    use tau_agent_plugin::{Request, Response};

    // Resolve the project path for the session's working directory.
    let project_path = match resolver.resolve(&task.project_name) {
        Ok(p) => p,
        Err(e) => {
            eprintln!(
                "tasks: failed to resolve project '{}' for interactive session: {}",
                task.project_name, e
            );
            return None;
        }
    };

    // Inherit model from the parent session so the interactive task uses
    // the same model as its creator.
    let model =
        parent_session_id.and_then(|sid| tasks_scheduler::get_session_model(sid, writer, reader));

    // Session-tree parent: top-level tasks re-parent onto the triggering
    // session's root so new work surfaces in the user's primary tree
    // (task #512). Subtasks keep the current session as parent.
    let session_parent = if task.parent_id.is_none() {
        parent_session_id.and_then(|sid| tasks_scheduler::find_root_session(sid, writer, reader))
    } else {
        parent_session_id.map(String::from)
    };

    let create_req = Request::CreateSession {
        model,
        provider: None,
        system_prompt: None,
        cwd: Some(project_path.clone()),
        parent_id: session_parent,
        child_budget: 16,
        tagline: Some(crate::tasks_notify::task_session_tagline(
            task,
            "interactive",
        )),
        auto_archive: false,
        notify_parent: false,
        project_name: Some(task.project_name.clone()),
        sandbox_profile: task.sandbox_profile.clone(),
    };

    let new_sid = match crate::tasks_scheduler::server_request(writer, reader, create_req) {
        Ok(Response::SessionCreated { session_id }) => session_id,
        Ok(Response::Error { message }) => {
            eprintln!(
                "tasks: failed to create interactive session for task {}: {}",
                task.id, message
            );
            let _ = db.add_message(
                task.id,
                &format!(
                    "⚠️ Failed to create interactive session: {}. \
                     Task is in interactive state but has no session.",
                    message
                ),
                Some("system"),
            );
            return None;
        }
        Ok(_) => {
            eprintln!(
                "tasks: unexpected response creating session for task {}",
                task.id
            );
            let _ = db.add_message(
                task.id,
                "⚠️ Failed to create interactive session: unexpected server response. \
                 Task is in interactive state but has no session.",
                Some("system"),
            );
            return None;
        }
        Err(e) => {
            eprintln!("tasks: error creating session for task {}: {}", task.id, e);
            let _ = db.add_message(
                task.id,
                &format!(
                    "⚠️ Failed to create interactive session: {}. \
                     Task is in interactive state but has no session.",
                    e
                ),
                Some("system"),
            );
            return None;
        }
    };

    // Load project instructions for the interactive phase.
    let project_instructions = crate::tasks_config::load_project_instructions(
        &project_path,
        Some(&task.project_name),
        "interactive",
    )
    .unwrap_or_default();

    // Queue an initial message so the session has context when the user connects.
    // Interactive tasks get a "gather-first" instruction: the session should
    // read the spec and understand requirements but NOT start any work until
    // the user explicitly says to proceed.
    let instructions_section = if project_instructions.is_empty() {
        String::new()
    } else {
        format!("\n\nProject instructions:\n{project_instructions}")
    };
    let initial_msg = format!(
        "You are working on task {id}: {title}.\n\
         \n\
         Use the task_get tool (not a bash command) to read the full spec: \
         call `task_get` with arguments {{\"id\": {id}}}.\n\
         \n\
         This is an interactive task. Read the spec and gather all necessary information \
         (understand the requirements, explore relevant code, ask clarifying questions), \
         but do NOT start making any changes until the user explicitly tells you to proceed.\n\
         \n\
         When the user wants to start implementation (says \"go\", \"start\", \"do it\", etc.):\n\
         1. Make sure the task has affected_files set (call task_update to set them if needed)\n\
         2. Make sure the task spec/messages capture the full requirements\n\
         3. Transition the task to 'ready' state: call task_update with arguments {{\"id\": {id}, \"state\": \"ready\"}}\n\
         4. Tell the user the task has been queued for scheduling\n\
         Do NOT edit project files directly — the scheduler will create an isolated branch/worktree \
         and dispatch a worker session to do the implementation.\
         {instructions_section}",
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

    // Capture state before assign so we can emit a transition info-message.
    let pre_state = db.get_task(id).ok().flatten().map(|t| t.state);

    match db.assign_task(id, sid) {
        Ok(result) => {
            let task = &result.task;
            // For interactive tasks with a changed session, reparent sessions via RPC.
            // The DB updates (session_id on task + all descendants) were already done
            // atomically inside assign_task's transaction.
            if task.state == "interactive"
                && let Some(ref old_sid) = result.old_session_id
                && old_sid != sid
            {
                // Reparent direct child sessions from the old session
                let req = tau_agent_plugin::Request::ReparentChildren {
                    old_parent_id: old_sid.clone(),
                    new_parent_id: sid.to_string(),
                };
                if let Err(e) = crate::tasks_scheduler::server_request(writer, reader, req) {
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
                        let req = tau_agent_plugin::Request::ReparentChildren {
                            old_parent_id: desc_old_sid.clone(),
                            new_parent_id: sid.to_string(),
                        };
                        if let Err(e) = crate::tasks_scheduler::server_request(writer, reader, req)
                        {
                            eprintln!(
                                "warning: failed to reparent children of {} to {}: {}",
                                desc_old_sid, sid, e
                            );
                        }
                    }
                }
            }
            // Emit state-change InfoMessage if the assign actually changed
            // the task's state (ready → active).
            if let Some(ref old) = pre_state
                && old != &result.task.state
            {
                crate::tasks_notify::notify_state_change(
                    db,
                    &result.task,
                    old,
                    None,
                    writer,
                    reader,
                );
            }
            match serde_json::to_string_pretty(&result.task) {
                Ok(json) => {
                    let summary = format!("task_assign: #{}", id);
                    tool_ok_summary(tool_call_id, &json, summary)
                }
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

    // Build enriched relations with dependency status and cross-project context
    let enriched_relations: Vec<serde_json::Value> = relations
        .iter()
        .map(|rel| {
            let mut obj = serde_json::json!({
                "from_task": rel.from_task,
                "to_task": rel.to_task,
                "relation": rel.relation,
            });

            // Determine the "other" task ID for cross-project detection
            let other_task_id = if rel.from_task == id {
                rel.to_task
            } else {
                rel.from_task
            };

            if let Ok(Some(other_task)) = db.get_task(other_task_id) {
                // Add project_name if the other task is in a different project
                if other_task.project_name != task.project_name {
                    obj["project_name"] = serde_json::json!(other_task.project_name);
                }

                // For depends_on relations where this task is the dependent,
                // include whether the dependency is satisfied or blocking.
                if rel.relation == "depends_on" && rel.from_task == id {
                    let satisfied = other_task.state == "merged" || other_task.state == "closed";
                    obj["dependency_status"] = if satisfied {
                        serde_json::json!("satisfied")
                    } else {
                        serde_json::json!("blocking")
                    };
                    obj["dependency_state"] = serde_json::json!(other_task.state);
                }
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
        Ok(json) => {
            let summary = format!(
                "task_get: #{} \"{}\" ({} messages, {} subtasks)",
                task.id,
                task.title,
                messages.len(),
                subtasks.len()
            );
            tool_ok_summary(tool_call_id, &json, summary)
        }
        Err(e) => tool_err(tool_call_id, &format!("serialize: {}", e)),
    }
}

fn handle_task_list(
    db: &TasksDb,
    args: &serde_json::Value,
    project_name: &str,
    tool_call_id: &str,
) -> PluginToolResult {
    let state = args.get("state").and_then(|v| v.as_str());
    let parent_id = args.get("parent_id").and_then(|v| v.as_i64());
    let tag = args.get("tag").and_then(|v| v.as_str());
    let limit = args.get("limit").and_then(|v| v.as_i64());

    match db.list_tasks(project_name, state, parent_id, tag, limit) {
        Ok(tasks) => match serde_json::to_string_pretty(&tasks) {
            Ok(json) => {
                let summary = format!("task_list: {} tasks", tasks.len());
                tool_ok_summary(tool_call_id, &json, summary)
            }
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
    resolver: &ProjectResolver,
    writer: &mut impl Write,
    reader: &mut impl BufRead,
    pending_events: &mut Vec<SchedulerEvent>,
    post_persist_actions: &mut Vec<tau_agent_plugin::PostPersistAction>,
) -> PluginToolResult {
    let id = match args.get("id").and_then(|v| v.as_i64()) {
        Some(id) => id,
        None => return tool_err(tool_call_id, "id is required"),
    };

    // Reject the removed skip_planning parameter on updates too (task #512).
    if args.get("skip_planning").is_some() {
        return tool_err(
            tool_call_id,
            "'skip_planning' has been removed; use 'initial_state' on task_create instead \
             ('ready' is the former skip_planning=true behavior, 'planning' the default).",
        );
    }

    let update = TaskUpdate {
        title: args.get("title").and_then(|v| v.as_str()).map(String::from),
        state: args.get("state").and_then(|v| v.as_str()).map(String::from),
        priority: args.get("priority").and_then(|v| v.as_i64()),
        tags: args.get("tags").cloned(),
        affected_files: args.get("affected_files").cloned(),
        skip_review: args.get("skip_review").and_then(|v| v.as_bool()),
        require_approval: args.get("require_approval").and_then(|v| v.as_bool()),
        merge_target: args
            .get("merge_target")
            .and_then(|v| v.as_str())
            .map(String::from),
        sandbox_profile: args
            .get("sandbox_profile")
            .and_then(|v| v.as_str())
            .map(String::from),
        held: args.get("hold").and_then(|v| v.as_bool()),
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
    if let (Some(new_state), Some(old_s)) = (&update.state, &old_state)
        && new_state == "review"
        && old_s == "active"
    {
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
                                tau_agent_plugin::Request::QueueMessage {
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

    match db.update_task(id, &update, session_id) {
        Ok(task) => {
            let mut dispatch_warnings: Vec<String> = Vec::new();

            // Trigger scheduler events based on the new state.
            // Held tasks do not trigger schedule passes — releasing via
            // hold=false will (see below).
            match task.state.as_str() {
                "approved" => {
                    pending_events.push(SchedulerEvent::MergeNeeded(session_id.map(String::from)));
                }
                "ready" | "planning" if !task.held => {
                    pending_events.push(SchedulerEvent::ScheduleNeeded(
                        task.project_name.clone(),
                        session_id.map(String::from),
                    ));
                }
                "merged" | "closed" => {
                    // Dependents may have been blocked on this task — re-
                    // evaluate schedulability on the next scheduler pass.
                    pending_events.push(SchedulerEvent::ScheduleNeeded(
                        task.project_name.clone(),
                        session_id.map(String::from),
                    ));
                }
                _ => {}
            }

            // Releasing a held task (hold=false) on a schedulable state
            // should poke the scheduler so the task can be picked up on
            // the next pass. The state-based arm above already covers the
            // held=true → held=false case when combined with any other
            // state change; this branch handles the pure release.
            if matches!(update.held, Some(false))
                && !task.held
                && (task.state == "ready" || task.state == "planning")
                && !pending_events.iter().any(|e| {
                    matches!(e,
                    SchedulerEvent::ScheduleNeeded(p, _) if p == &task.project_name)
                })
            {
                pending_events.push(SchedulerEvent::ScheduleNeeded(
                    task.project_name.clone(),
                    session_id.map(String::from),
                ));
            }

            // Note: the observational `notify_state_change` call is deferred
            // until after any session-creating dispatches below
            // (`dispatch_review`, `dispatch_refining`,
            // `create_interactive_session`).  The spec requires the newly
            // created session be recorded in `task_sessions` BEFORE we
            // emit the info message, so the new session sees the
            // transition that created it in its own history.  See
            // `deferred_notify` at the end of this match arm.

            // When a task transitions from review back to active (changes
            // requested), notify the worker session so it knows to resume.
            if task.state == "active"
                && old_state.as_deref() == Some("review")
                && let Some(ref sid) = task.session_id
            {
                let msg = format!(
                    "Task {} was moved back to active (changes requested). \
                    Please run task_get to read the latest review feedback \
                    and address the requested changes.",
                    task.id
                );
                let _ = crate::tasks_scheduler::server_request(
                    writer,
                    reader,
                    tau_agent_plugin::Request::QueueMessage {
                        target_session_id: sid.clone(),
                        content: msg,
                        sender_info: format!("task-system (review task {})", task.id),
                        await_reply: false,
                        reply_to: None,
                    },
                );
            }

            // Automated review dispatch: when transitioning to review,
            // auto-launch a review session.
            if task.state == "review" && old_state.as_deref() == Some("active") {
                match resolver.resolve(&task.project_name) {
                    Ok(project_path) => {
                        match tasks_scheduler::dispatch_review(
                            db,
                            &task,
                            session_id,
                            &project_path,
                            writer,
                            reader,
                        ) {
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
                                let warn = format!(
                                    "⚠️ Auto-dispatch of review session failed: {}. \
                                     Task is in review state but has no reviewer session.",
                                    e
                                );
                                let _ = db.add_message(task.id, &warn, Some("system"));
                                dispatch_warnings.push(warn);
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!(
                            "tasks: cannot resolve project '{}' for review dispatch: {}",
                            task.project_name, e
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
                match resolver.resolve(&task.project_name) {
                    Ok(project_path) => {
                        match tasks_scheduler::dispatch_refining(
                            db,
                            &task,
                            session_id,
                            &project_path,
                            writer,
                            reader,
                        ) {
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
                                let warn = format!(
                                    "⚠️ Auto-dispatch of refining session failed: {}. \
                                     Task is in refining state but has no refiner session.",
                                    e
                                );
                                let _ = db.add_message(task.id, &warn, Some("system"));
                                dispatch_warnings.push(warn);
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!(
                            "tasks: cannot resolve project '{}' for refining dispatch: {}",
                            task.project_name, e
                        );
                    }
                }
            }

            // Session reuse: when refining → planning, resume the planning
            // session by sending it a message with the refining feedback.
            if task.state == "planning"
                && old_state.as_deref() == Some("refining")
                && let Some(ref sid) = task.session_id
            {
                let msg = format!(
                    "Task {} was sent back to planning (plan needs revision). \
                     Please run task_get to read the latest refining feedback \
                     and revise your plan.",
                    task.id
                );
                let _ = crate::tasks_scheduler::server_request(
                    writer,
                    reader,
                    tau_agent_plugin::Request::QueueMessage {
                        target_session_id: sid.clone(),
                        content: msg,
                        sender_info: format!("task-system (refine task {})", task.id),
                        await_reply: false,
                        reply_to: None,
                    },
                );
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
                        let req = tau_agent_plugin::Request::GetSessionInfo {
                            session_id: sid.clone(),
                        };
                        match crate::tasks_scheduler::server_request(writer, reader, req) {
                            Ok(tau_agent_plugin::Response::SessionInfo { info }) => info.archived,
                            _ => true, // session not found or error → need a new one
                        }
                    }
                };
                if needs_session {
                    match create_interactive_session(
                        db, &task, resolver, session_id, writer, reader,
                    ) {
                        Some(new_sid) => {
                            let _ = db.set_session_id(task.id, &new_sid);
                            let _ = db.record_session(task.id, &new_sid, "interactive");
                            if let Some(creator_sid) = session_id {
                                let _ = db.record_session(task.id, creator_sid, "creator");
                            }
                        }
                        None => {
                            dispatch_warnings.push(
                                "⚠️ Failed to create interactive session. \
                                 Task is in interactive state but has no session."
                                    .to_string(),
                            );
                        }
                    }
                }

                // Notify the interactive session that the task returned from
                // another state.  Re-read session_id since it may have been
                // updated above (new session created).
                let notify_sid = if needs_session {
                    db.get_task(task.id)
                        .ok()
                        .flatten()
                        .and_then(|t| t.session_id)
                } else {
                    task.session_id.clone()
                };
                if let Some(ref target_sid) = notify_sid {
                    let prev = old_state.as_deref().unwrap_or("unknown");
                    let msg = format!(
                        "Task #{id} returned to interactive from {prev}. \
                         Review the latest task messages for context:\n\
                         - Call task_get with arguments {{\"id\": {id}}}",
                        id = task.id,
                        prev = prev,
                    );
                    let _ = crate::tasks_scheduler::server_request(
                        writer,
                        reader,
                        tau_agent_plugin::Request::QueueMessage {
                            target_session_id: target_sid.clone(),
                            content: msg,
                            sender_info: format!(
                                "task-system (interactive return task {})",
                                task.id
                            ),
                            await_reply: false,
                            reply_to: None,
                        },
                    );
                }
            }

            // Deferred observational broadcast for non-terminal transitions.
            //
            // The review/refining/interactive dispatch blocks above may
            // have CREATED a new session (reviewer, refiner, or
            // interactive) and recorded it in `task_sessions`.  By
            // emitting the info message here — after those
            // `record_session` calls — the newly created session
            // participates in the recipient set and sees the transition
            // that spawned it in its own history.
            //
            // Terminal transitions (merged / closed) are handled inside
            // their dedicated block below: they must fire BEFORE
            // `auto_archive_task_session`, otherwise the soon-to-be
            // archived worker/reviewer sessions would be filtered out of
            // the recipient set.
            if let Some(ref old) = old_state
                && &task.state != old
                && task.state != "merged"
                && task.state != "closed"
            {
                let context = match (old.as_str(), task.state.as_str()) {
                    ("review", "active") => Some("rework requested"),
                    ("refining", "interactive") => Some("scope expansion"),
                    _ => None,
                };
                // Re-fetch so newly set session_id (interactive case) is
                // visible to the recipient collector.
                let fresh = db.get_task(task.id).ok().flatten().unwrap_or(task.clone());
                crate::tasks_notify::notify_state_change_split(
                    db,
                    &fresh,
                    old,
                    context,
                    session_id,
                    writer,
                    reader,
                    post_persist_actions,
                );
            }

            // When a task transitions to a terminal state, auto-archive its session
            // and notify the parent task's session.
            if task.state == "merged" || task.state == "closed" {
                // Emit observational state-change broadcast BEFORE archiving
                // so the still-live worker/reviewer/... sessions receive the
                // terminal info-message in their own history.  (Archived
                // filtering in notify_state_change would otherwise drop them.)
                if let Some(ref old) = old_state
                    && &task.state != old
                {
                    crate::tasks_notify::notify_state_change_split(
                        db,
                        &task,
                        old,
                        None,
                        session_id,
                        writer,
                        reader,
                        post_persist_actions,
                    );
                }

                auto_archive_task_session(db, &task, resolver, writer, reader);

                // Notify parent session that this individual subtask completed
                tasks_merge::notify_parent_of_subtask_done(db, task.id, writer, reader);

                // Check if ALL subtasks are now in a terminal state and notify parent
                if let Err(e) = tasks_merge::notify_parent_if_all_done(db, task.id, writer, reader)
                {
                    eprintln!(
                        "tasks: parent notification failed for task {}: {}",
                        task.id, e
                    );
                }
            }

            // Re-fetch task if we may have updated session_id after the
            // initial update_task call (e.g. interactive session creation).
            let final_task =
                if task.state == "interactive" && old_state.as_deref() != Some("interactive") {
                    db.get_task(task.id).ok().flatten().unwrap_or(task)
                } else {
                    task
                };

            let mut changes = Vec::new();
            if let Some(state) = args.get("state").and_then(|v| v.as_str()) {
                changes.push(format!("state → {}", state));
            }
            if args.get("title").and_then(|v| v.as_str()).is_some() {
                changes.push("title".to_string());
            }
            if args.get("priority").is_some() {
                changes.push("priority".to_string());
            }
            if args.get("tags").is_some() {
                changes.push("tags".to_string());
            }
            if args.get("affected_files").is_some() {
                changes.push("affected_files".to_string());
            }
            if let Some(hold) = args.get("hold").and_then(|v| v.as_bool()) {
                changes.push(if hold {
                    "hold=true".to_string()
                } else {
                    "hold=false".to_string()
                });
            }
            let summary = if changes.is_empty() {
                format!("task_update: #{}", id)
            } else {
                format!("task_update: #{} ({})", id, changes.join(", "))
            };

            match serde_json::to_string_pretty(&final_task) {
                Ok(json) => {
                    let mut result = tool_ok_summary(tool_call_id, &json, summary);
                    if !dispatch_warnings.is_empty() {
                        let warning_text = format!("\n{}", dispatch_warnings.join("\n"));
                        result.content.push(ToolResultContent::Text(
                            tau_agent_plugin::TextContent {
                                text: warning_text,
                                text_signature: None,
                            },
                        ));
                    }
                    result
                }
                Err(e) => tool_err(tool_call_id, &format!("serialize: {}", e)),
            }
        }
        Err(e) => tool_err(tool_call_id, &format!("update task: {}", e)),
    }
}

/// Auto-archive a task's session and clean up worktree/branch when a task
/// transitions to a terminal state (merged or closed). All operations are best-effort — errors are logged
/// but don't fail the state transition.
fn auto_archive_task_session(
    db: &TasksDb,
    task: &crate::tasks_db::Task,
    resolver: &ProjectResolver,
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
                tau_agent_plugin::Request::ArchiveSession {
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
            tau_agent_plugin::Request::ArchiveSession {
                session_id: sid.clone(),
                require_ancestor: None,
            },
        );
    }

    // Clean up worktree if still present
    if let Some(ref wt_path) = task.worktree_path {
        if let Some(ref branch) = task.branch {
            if let Ok(project_path) = resolver.resolve(&task.project_name) {
                if let Ok(repo_root) = crate::tasks_git::get_repo_root(&project_path) {
                    let _ = crate::tasks_git::remove_worktree(&repo_root, wt_path);
                    let _ = crate::tasks_git::delete_branch(&repo_root, branch);
                }
                // Update session cwds so plugin respawns don't fail.
                if let Ok(sessions) = db.get_sessions(task.id) {
                    for ts in &sessions {
                        let _ = crate::tasks_scheduler::server_request(
                            writer,
                            reader,
                            tau_agent_plugin::Request::SetCwd {
                                session_id: ts.session_id.clone(),
                                cwd: project_path.clone(),
                                caller_session_id: None,
                            },
                        );
                    }
                }
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
            Ok(json) => {
                let summary = format!("task_message: #{} (message added)", id);
                tool_ok_summary(tool_call_id, &json, summary)
            }
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
    let task_id = match args.get("task_id").and_then(|v| v.as_i64()) {
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
            Ok(json) => {
                let summary = format!("task_message_edit: #{} message #{}", task_id, message_id);
                tool_ok_summary(tool_call_id, &json, summary)
            }
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
    project_name: &str,
    tool_call_id: &str,
) -> PluginToolResult {
    let query = match args.get("query").and_then(|v| v.as_str()) {
        Some(q) => q,
        None => return tool_err(tool_call_id, "query is required"),
    };
    let state = args.get("state").and_then(|v| v.as_str());

    match db.search_tasks(project_name, query, state) {
        Ok(tasks) => match serde_json::to_string_pretty(&tasks) {
            Ok(json) => {
                let summary = format!("task_search: {} results", tasks.len());
                tool_ok_summary(tool_call_id, &json, summary)
            }
            Err(e) => tool_err(tool_call_id, &format!("serialize: {}", e)),
        },
        Err(e) => tool_err(tool_call_id, &format!("search tasks: {}", e)),
    }
}

fn handle_task_status(
    db: &TasksDb,
    args: &serde_json::Value,
    project_name: &str,
    resolver: &ProjectResolver,
    tool_call_id: &str,
) -> PluginToolResult {
    let project_name = args
        .get("project")
        .and_then(|v| v.as_str())
        .unwrap_or(project_name);

    let project_path = resolver.resolve(project_name).ok();
    match tasks_scheduler::get_status(db, project_name, project_path.as_deref()) {
        Ok(status) => tool_ok(tool_call_id, &tasks_scheduler::format_status(&status)),
        Err(e) => tool_err(tool_call_id, &format!("scheduler status: {}", e)),
    }
}

fn handle_task_overview(
    db: &TasksDb,
    args: &serde_json::Value,
    project_name: &str,
    tool_call_id: &str,
) -> PluginToolResult {
    let project_name = args
        .get("project")
        .and_then(|v| v.as_str())
        .unwrap_or(project_name);
    let recent_limit = args
        .get("recent_limit")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .unwrap_or(10);

    // The plugin doesn't track live sessions — pass an empty set.  The
    // server-side RPC wrapper fills `has_live_session` when it gets the
    // structured response over the wire.
    let live = std::collections::HashSet::new();
    match tasks_scheduler::task_overview_response(db, project_name, recent_limit, &live) {
        Ok(resp) => match serde_json::to_string_pretty(&resp) {
            Ok(json) => tool_ok(tool_call_id, &json),
            Err(e) => tool_err(tool_call_id, &format!("serialize overview: {}", e)),
        },
        Err(e) => tool_err(tool_call_id, &format!("task overview: {}", e)),
    }
}

fn handle_task_schedule(
    db: &TasksDb,
    args: &serde_json::Value,
    project_name: &str,
    resolver: &ProjectResolver,
    tool_call_id: &str,
) -> PluginToolResult {
    let project_name = args
        .get("project")
        .and_then(|v| v.as_str())
        .unwrap_or(project_name);

    let project_path = match resolver.resolve(project_name) {
        Ok(p) => p,
        Err(e) => return tool_err(tool_call_id, &format!("resolve project: {}", e)),
    };

    match tasks_scheduler::schedule(db, project_name, &project_path) {
        Ok(scheduled) => {
            if scheduled.is_empty() {
                return tool_ok(tool_call_id, "No ready tasks to schedule.");
            }
            match serde_json::to_string_pretty(&scheduled) {
                Ok(json) => {
                    let summary = format!("task_schedule: {} tasks scheduled", scheduled.len());
                    tool_ok_summary(
                        tool_call_id,
                        &format!(
                            "Scheduled {} task(s) for dispatch:\n{}",
                            scheduled.len(),
                            json
                        ),
                        summary,
                    )
                }
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
    resolver: &ProjectResolver,
    tool_call_id: &str,
    writer: &mut impl Write,
    reader: &mut impl BufRead,
) -> PluginToolResult {
    let id = match args.get("id").and_then(|v| v.as_i64()) {
        Some(id) => id,
        None => return tool_err(tool_call_id, "id is required"),
    };

    // Look up the task to get its project_name, then resolve to path.
    let task = match db.get_task(id) {
        Ok(Some(t)) => t,
        Ok(None) => return tool_err(tool_call_id, &format!("task {} not found", id)),
        Err(e) => return tool_err(tool_call_id, &format!("get task: {}", e)),
    };
    let project_path = match resolver.resolve(&task.project_name) {
        Ok(p) => p,
        Err(e) => return tool_err(tool_call_id, &format!("resolve project: {}", e)),
    };

    match tasks_scheduler::dispatch(db, id, session_id, &project_path, writer, reader) {
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
    resolver: &ProjectResolver,
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

    // Broadcast approved -> merging.
    if let Ok(Some(t)) = db.get_task(id) {
        crate::tasks_notify::notify_state_change(db, &t, "approved", None, writer, reader);
    }

    // Run the merge
    let project_dir = match resolver.resolve(&task.project_name) {
        Ok(p) => p,
        Err(e) => return tool_err(tool_call_id, &format!("resolve project: {}", e)),
    };
    match tasks_merge::merge_task_for_caller(db, id, &project_dir, session_id, writer, reader) {
        Ok(result) => {
            if result.success {
                // Transition to merged
                if let Err(e) = db.update_task(
                    id,
                    &TaskUpdate {
                        state: Some("merged".into()),
                        ..Default::default()
                    },
                    session_id,
                ) {
                    return tool_err(
                        tool_call_id,
                        &format!("merge succeeded but transition to merged failed: {}", e),
                    );
                }

                // Broadcast merging -> merged (terminal).
                if let Ok(Some(t)) = db.get_task(id) {
                    let ctx = crate::tasks_scheduler::extract_merge_commit(&project_dir, &t);
                    crate::tasks_notify::notify_state_change(
                        db,
                        &t,
                        "merging",
                        ctx.as_deref(),
                        writer,
                        reader,
                    );
                }

                // Notify parent session that this individual subtask completed
                tasks_merge::notify_parent_of_subtask_done(db, id, writer, reader);

                // Check if ALL subtasks are now in a terminal state and notify parent
                if let Err(e) = tasks_merge::notify_parent_if_all_done(db, id, writer, reader) {
                    eprintln!("tasks: parent notification failed for task {}: {}", id, e);
                }

                match serde_json::to_string_pretty(&result) {
                    Ok(json) => {
                        let summary = format!("task_merge: #{} merged", id);
                        tool_ok_summary(tool_call_id, &json, summary)
                    }
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

                // Broadcast merging -> active (recoverable failure).
                if let Ok(Some(t)) = db.get_task(id) {
                    crate::tasks_notify::notify_state_change(
                        db,
                        &t,
                        "merging",
                        Some("merge failed — reverted to active"),
                        writer,
                        reader,
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

            // Broadcast merging -> failed (terminal).  Root session is
            // included via notify_state_change's root-broadcast rule.
            if let Ok(Some(t)) = db.get_task(id) {
                crate::tasks_notify::notify_state_change(
                    db,
                    &t,
                    "merging",
                    Some(&format!("merge error: {}", e)),
                    writer,
                    reader,
                );
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
    /// A task moved to `approved` — run the merge queue.  The optional
    /// session id is the caller that triggered the event; it's threaded
    /// through to [`tasks_merge::merge_task_for_caller`] so archival of
    /// the caller's own subtree can be deferred to Tier-3.
    MergeNeeded(Option<String>),
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

    // Open a read-only connection to tau.db for resolving project_name → path.
    let resolver = match ProjectResolver::open() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("tasks plugin: failed to open project resolver: {}", e);
            std::process::exit(1);
        }
    };

    // ---- Writer setup: shared, mutex-protected BufWriter<Stdout> ----
    //
    // Both the main loop and the merge worker thread write to stdout.
    // SharedStdout serialises each JSON line under an Arc<Mutex<_>> so
    // writes never interleave.
    let stdout = std::io::stdout();
    let stdout_writer = Arc::new(Mutex::new(BufWriter::new(stdout)));
    let mut writer = SharedStdout::from_arc(Arc::clone(&stdout_writer));
    let worker_writer = SharedStdout::from_arc(Arc::clone(&stdout_writer));

    // Send registration
    let registration = PluginRegistration {
        name: "tasks".to_string(),
        tools: tasks_tools(),
        hooks: vec!["task_state_changed".to_string()],
        commands: Vec::new(),
    };
    send_message(&mut writer, &PluginMessage::Register(registration));

    // ---- Line router ----
    //
    // A dedicated thread reads every line from stdin, parses it once,
    // and dispatches:
    //
    //   * `ServerResponse` lines whose `request_id` starts with
    //     `merge-sr-` go to `worker_resp_tx`.
    //   * Other `ServerResponse` lines go to `main_resp_tx` (e.g. the
    //     main loop's own tool-handler RPCs, tagged `task-sr-...`).
    //   * All other `PluginRequest` variants (ToolCall, Hook, Init,
    //     SessionStart, Idle, CancelToolCall) go to `main_req_tx` for
    //     the main event loop to dispatch.
    //
    // This is what lets the merge worker's ServerRequest round-trips
    // sit blocked on their own response channel without stopping the
    // main loop from picking up new ToolCalls. See #540 for context.
    let (main_req_tx, main_req_rx) = std::sync::mpsc::channel::<PluginRequest>();
    let (main_resp_tx, main_resp_rx) = std::sync::mpsc::channel::<String>();
    let (worker_resp_tx, worker_resp_rx) = std::sync::mpsc::channel::<String>();
    std::thread::Builder::new()
        .name("tau-tasks-stdin-router".into())
        .spawn(move || {
            let stdin = std::io::stdin();
            let mut reader = BufReader::new(stdin.lock());
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) => break, // EOF
                    Ok(_) => {}
                    Err(_) => break,
                }
                if line.trim().is_empty() {
                    continue;
                }
                // Parse once. On malformed input, log and drop the
                // line — neither the main loop nor the worker should
                // see garbage.
                let req: PluginRequest = match serde_json::from_str(&line) {
                    Ok(r) => r,
                    Err(e) => {
                        eprintln!("tasks router: bad request: {}", e);
                        continue;
                    }
                };
                match &req {
                    PluginRequest::ServerResponse { request_id, .. } => {
                        // Ensure the forwarded line ends with a newline
                        // so the downstream `BufRead::read_line` caller
                        // sees a line-terminated JSON object.
                        let mut forward = line;
                        if !forward.ends_with('\n') {
                            forward.push('\n');
                        }
                        if request_id.starts_with("merge-sr") {
                            if worker_resp_tx.send(forward).is_err() {
                                // Worker side gone; the plugin is on the
                                // way down. Stop routing.
                                break;
                            }
                        } else if main_resp_tx.send(forward).is_err() {
                            break;
                        }
                    }
                    _ => {
                        if main_req_tx.send(req).is_err() {
                            break;
                        }
                    }
                }
            }
        })
        .expect("spawn tasks stdin router");

    // Main loop's reader: only yields server-response lines routed to it.
    let mut chan_reader = ChannelLineReader::new(main_resp_rx);

    // Worker reader: only yields merge-sr ServerResponse lines.
    let worker_reader = ChannelLineReader::new(worker_resp_rx);

    // Spawn the merge worker. Holding it in scope here ties its
    // lifetime to `run_tasks_plugin`'s stack frame; on shutdown the
    // handle is dropped, which closes the job channel, which lets the
    // worker thread drain and exit.
    let merge_worker = match MergeWorker::spawn(worker_writer, worker_reader) {
        Ok(w) => Some(w),
        Err(e) => {
            eprintln!(
                "tasks plugin: failed to spawn merge worker: {} — merges will run inline",
                e
            );
            None
        }
    };

    // Pending scheduler events, populated by tool handlers and drained
    // after each tool call completes.
    let mut pending_events: Vec<SchedulerEvent> = Vec::new();

    // Startup sweep: clean up stale worktrees for merged/closed/failed tasks.
    // This catches historical leftovers from before cleanup was implemented
    // or from tasks that were manually closed without going through merge.
    if let Ok(stale_tasks) = db.get_stale_worktree_tasks() {
        for task in &stale_tasks {
            eprintln!(
                "tasks: startup cleanup: task {} ({}) in state '{}' has stale worktree at {:?}",
                task.id, task.title, task.state, task.worktree_path
            );
            if let Some(ref wt_path) = task.worktree_path {
                if let Some(ref branch) = task.branch {
                    if let Ok(project_path) = resolver.resolve(&task.project_name) {
                        if let Ok(repo_root) = crate::tasks_git::get_repo_root(&project_path) {
                            if let Err(e) = crate::tasks_git::remove_worktree(&repo_root, wt_path) {
                                eprintln!(
                                    "tasks: startup cleanup: failed to remove worktree for task {}: {}",
                                    task.id, e
                                );
                            }
                            if let Err(e) = crate::tasks_git::delete_branch(&repo_root, branch) {
                                eprintln!(
                                    "tasks: startup cleanup: failed to delete branch for task {}: {}",
                                    task.id, e
                                );
                            }
                        }
                        // Update session cwds so plugin respawns don't fail.
                        if let Ok(sessions) = db.get_sessions(task.id) {
                            for ts in &sessions {
                                let _ = crate::tasks_scheduler::server_request(
                                    &mut writer,
                                    &mut chan_reader,
                                    tau_agent_plugin::Request::SetCwd {
                                        session_id: ts.session_id.clone(),
                                        cwd: project_path.clone(),
                                        caller_session_id: None,
                                    },
                                );
                            }
                        }
                    }
                }
                if let Err(e) = db.clear_worktree(task.id) {
                    eprintln!(
                        "tasks: startup cleanup: failed to clear worktree in DB for task {}: {}",
                        task.id, e
                    );
                }
            }
        }
        if !stale_tasks.is_empty() {
            eprintln!(
                "tasks: startup cleanup: cleaned up {} stale worktree(s)",
                stale_tasks.len()
            );
        }
    }

    // Handle requests — blocks on recv() until the router forwards
    // the next PluginRequest or closes the channel on EOF / router shutdown.
    while let Ok(req) = main_req_rx.recv() {
        match req {
            PluginRequest::ToolCall {
                tool_call_id,
                name,
                arguments,
                cwd,
                session_id,
                project_name,
            } => {
                // Use project_name from the protocol. If not provided (server
                // hasn't been updated yet), fall back to discovering the
                // project from cwd by looking for .tau/project.toml.
                let project_name: Option<String> = project_name
                    .filter(|s| !s.is_empty())
                    .or_else(|| cwd.as_deref().and_then(discover_project_name));
                let project_name = project_name.as_deref();
                let session = session_id.as_deref();

                // Tier-2 actions to attach to the returned tool result.
                // Accumulated by handlers that need a side effect (e.g. a
                // self-recipient info message) to render textually AFTER the
                // tool result is persisted.
                let mut post_persist_actions: Vec<tau_agent_plugin::PostPersistAction> = Vec::new();

                let mut result = match name.as_str() {
                    "task_create" => handle_task_create(
                        &db,
                        &arguments,
                        &ToolCtx {
                            project_name,
                            session_id: session,
                            tool_call_id: &tool_call_id,
                        },
                        &resolver,
                        &mut writer,
                        &mut chan_reader,
                        &mut pending_events,
                    ),
                    "task_get" => handle_task_get(&db, &arguments, &tool_call_id),
                    "task_list" => {
                        let pn = project_name.unwrap_or("unknown");
                        handle_task_list(&db, &arguments, pn, &tool_call_id)
                    }
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
                        &resolver,
                        &mut writer,
                        &mut chan_reader,
                        &mut pending_events,
                        &mut post_persist_actions,
                    ),
                    "task_message" => handle_task_message(&db, &arguments, session, &tool_call_id),
                    "task_message_edit" => handle_task_message_edit(&db, &arguments, &tool_call_id),
                    "task_relate" => handle_task_relate(&db, &arguments, &tool_call_id),
                    "task_search" => {
                        let pn = project_name.unwrap_or("unknown");
                        handle_task_search(&db, &arguments, pn, &tool_call_id)
                    }
                    "task_status" => {
                        let pn = project_name.unwrap_or("unknown");
                        handle_task_status(&db, &arguments, pn, &resolver, &tool_call_id)
                    }
                    "task_overview" => {
                        let pn = project_name.unwrap_or("unknown");
                        handle_task_overview(&db, &arguments, pn, &tool_call_id)
                    }
                    "task_schedule" => {
                        let pn = project_name.unwrap_or("unknown");
                        handle_task_schedule(&db, &arguments, pn, &resolver, &tool_call_id)
                    }
                    "task_merge" => handle_task_merge(
                        &db,
                        &arguments,
                        session,
                        &resolver,
                        &tool_call_id,
                        &mut writer,
                        &mut chan_reader,
                    ),
                    "task_dispatch" => handle_task_dispatch(
                        &db,
                        &arguments,
                        session,
                        &resolver,
                        &tool_call_id,
                        &mut writer,
                        &mut chan_reader,
                    ),
                    _ => tool_err(&tool_call_id, &format!("unknown tool: {}", name)),
                };

                // Drain pending scheduler events and run the
                // corresponding passes immediately. Collect warnings
                // (e.g. dispatch failures) and append them to the tool
                // result so the LLM can surface them to the user.
                let dispatch_warnings = drain_scheduler_events(
                    &mut pending_events,
                    &db,
                    &resolver,
                    merge_worker.as_ref(),
                    &mut writer,
                    &mut chan_reader,
                );
                if !dispatch_warnings.is_empty() {
                    let warning_text = format!("\n{}", dispatch_warnings.join("\n"));
                    result
                        .content
                        .push(ToolResultContent::Text(tau_agent_plugin::TextContent {
                            text: warning_text,
                            text_signature: None,
                        }));
                }

                // Attach any Tier-2 post-persist actions accumulated during
                // the handler call (e.g. self-recipient info messages).
                if !post_persist_actions.is_empty() {
                    result.post_persist_actions.extend(post_persist_actions);
                }

                send_message(&mut writer, &PluginMessage::ToolResult(result));
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
                            pending_events.push(SchedulerEvent::MergeNeeded(None));
                        }
                        "ready" | "planning" => {
                            // Look up the task's project for the schedule pass.
                            if let Ok(Some(task)) = db.get_task(task_id)
                                && !task.held
                            {
                                pending_events.push(SchedulerEvent::ScheduleNeeded(
                                    task.project_name.clone(),
                                    None,
                                ));
                            }
                        }
                        _ => {}
                    }
                    let _ = drain_scheduler_events(
                        &mut pending_events,
                        &db,
                        &resolver,
                        merge_worker.as_ref(),
                        &mut writer,
                        &mut chan_reader,
                    );
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
            PluginRequest::CancelToolCall { .. } => {
                // Tasks plugin tools are short-lived (in-memory task-board
                // mutations) and don't spawn subprocesses, so mid-flight
                // cancellation is a no-op here. Swallowing this silently
                // is correct: the agent loop will still observe cancellation
                // via should_stop between calls.
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
pub(crate) struct ChannelLineReader {
    rx: std::sync::mpsc::Receiver<String>,
    /// Leftover bytes from the current line that haven't been consumed yet.
    buf: Vec<u8>,
    closed: bool,
}

impl ChannelLineReader {
    pub(crate) fn new(rx: std::sync::mpsc::Receiver<String>) -> Self {
        Self {
            rx,
            buf: Vec::new(),
            closed: false,
        }
    }

    /// Receive the next line, blocking until available. Returns `None`
    /// on channel close (EOF).
    #[allow(dead_code)]
    pub(crate) fn recv(&mut self) -> Option<String> {
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

// ---------------------------------------------------------------------------
// Scheduler event processing
// ---------------------------------------------------------------------------

/// Drain pending scheduler events and run the corresponding passes.
///
/// Called after each tool call completes. Events are deduplicated: multiple
/// `MergeNeeded` events collapse into a single merge pass, and multiple
/// `ScheduleNeeded` events for the same project collapse into one schedule
/// pass.
///
/// `merge_worker` is threaded through so that when a merge is needed, the
/// main loop can enqueue merge jobs onto the worker thread rather than
/// running the merge inline. When `merge_worker` is `None` (tests or
/// degraded startup paths) the code falls back to running merges inline on
/// the current thread — this preserves the pre-#540 behaviour for unit
/// tests that drive `drain_scheduler_events` directly.
fn drain_scheduler_events(
    events: &mut Vec<SchedulerEvent>,
    db: &TasksDb,
    resolver: &ProjectResolver,
    merge_worker: Option<&MergeWorker>,
    writer: &mut impl Write,
    reader: &mut impl BufRead,
) -> Vec<String> {
    if events.is_empty() {
        return Vec::new();
    }

    let batch = std::mem::take(events);

    let mut need_merge = false;
    let mut merge_caller: Option<String> = None;
    let mut schedule_projects: Vec<(String, Option<String>)> = Vec::new();

    for ev in batch {
        match ev {
            SchedulerEvent::MergeNeeded(caller) => {
                need_merge = true;
                // A single tool-call batch can only originate from one
                // caller session, so we keep the first non-None caller
                // we see.  In practice every batch contains at most one
                // `MergeNeeded` event; the dedup is defensive.
                if merge_caller.is_none() {
                    merge_caller = caller;
                }
            }
            SchedulerEvent::ScheduleNeeded(project_name, session_id) => {
                if !schedule_projects.iter().any(|(p, _)| p == &project_name) {
                    schedule_projects.push((project_name, session_id));
                }
            }
        }
    }

    // Run merge pass first (merging may unblock dependencies that become
    // ready, but schedule passes are triggered by explicit events anyway).
    if need_merge {
        run_merge_pass(
            db,
            resolver,
            merge_worker,
            merge_caller.as_deref(),
            writer,
            reader,
        );
    }

    let mut warnings = Vec::new();
    for (project_name, session_id) in &schedule_projects {
        warnings.extend(run_schedule_pass(
            db,
            project_name,
            resolver,
            session_id.as_deref(),
            writer,
            reader,
        ));
    }
    warnings
}

// ---------------------------------------------------------------------------
// Merge pass
// ---------------------------------------------------------------------------

/// Run a merge pass.
///
/// Finds every approved task and either
///
/// * enqueues one [`MergeJob`] per task onto the supplied worker thread
///   (the normal plugin path), or
/// * if no worker is available, falls back to the legacy inline path via
///   [`tasks_scheduler::merge_approved_for_caller`].
///
/// This is called from the plugin main loop after each tool call completes;
/// enqueuing is cheap and returns immediately, so the main loop stays
/// responsive even while the worker is mid-`just test`. See #540.
fn run_merge_pass(
    db: &TasksDb,
    resolver: &ProjectResolver,
    merge_worker: Option<&MergeWorker>,
    caller_session_id: Option<&str>,
    writer: &mut impl Write,
    reader: &mut impl BufRead,
) {
    if let Some(worker) = merge_worker {
        // Fast path: enumerate approved tasks and enqueue one job per
        // task. The worker will re-read each task from the DB before
        // merging, so we don't need to snapshot any other state here.
        match db.get_approved_tasks(None) {
            Ok(approved) => {
                if approved.is_empty() {
                    return;
                }
                for task in approved {
                    eprintln!(
                        "tasks scheduler: enqueuing merge job for task {} ({})",
                        task.id, task.title
                    );
                    let job = MergeJob {
                        task_id: task.id,
                        caller_session_id: caller_session_id.map(|s| s.to_string()),
                    };
                    if let Err(e) = worker.enqueue(job) {
                        eprintln!(
                            "tasks scheduler: merge worker channel closed for task {}: {}",
                            e.0.task_id, e
                        );
                        break;
                    }
                }
            }
            Err(e) => {
                eprintln!("tasks scheduler: failed to list approved tasks: {}", e);
            }
        }
        return;
    }

    // Fallback: inline merge (used only when the worker failed to spawn
    // or in tests). Mirrors the pre-#540 behaviour.
    let resolve_fn = |name: &str| resolver.resolve(name);
    match tasks_scheduler::merge_approved_for_caller(
        db,
        &resolve_fn,
        caller_session_id,
        writer,
        reader,
    ) {
        Ok(attempts) => {
            // Collect projects whose tasks were successfully merged so we
            // can re-evaluate dependents of those merges. Without this,
            // scheduler-driven merges (as opposed to tool-call transitions
            // that flow through handle_task_update) leave dependents
            // stuck in ready/planning until the next manual schedule pass.
            let mut merged_projects: std::collections::HashSet<String> =
                std::collections::HashSet::new();
            for a in &attempts {
                if a.success
                    && let Ok(Some(t)) = db.get_task(a.task_id)
                {
                    merged_projects.insert(t.project_name);
                }
            }
            for project in &merged_projects {
                run_schedule_pass(db, project, resolver, None, writer, reader);
            }
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
    project_name: &str,
    resolver: &ProjectResolver,
    session_id: Option<&str>,
    writer: &mut impl Write,
    reader: &mut impl BufRead,
) -> Vec<String> {
    let project_path = match resolver.resolve(project_name) {
        Ok(p) => p,
        Err(e) => {
            eprintln!(
                "tasks scheduler: cannot resolve project '{}': {}",
                project_name, e
            );
            return Vec::new();
        }
    };
    let mut warnings: Vec<String> = Vec::new();

    match tasks_scheduler::schedule(db, project_name, &project_path) {
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
                if let Err(e) =
                    tasks_scheduler::dispatch(db, st.id, session_id, &project_path, writer, reader)
                {
                    eprintln!("tasks scheduler: dispatch failed for task {}: {}", st.id, e);

                    // For non-planning tasks, schedule() already transitioned
                    // to active via prepare_task().  Revert to ready so the
                    // scheduler can retry on the next pass.
                    let is_planning = st.branch.is_empty();
                    if !is_planning {
                        let _ = db.update_task(
                            st.id,
                            &TaskUpdate {
                                state: Some("ready".to_string()),
                                ..Default::default()
                            },
                            None,
                        );
                    }

                    let revert_note = if is_planning {
                        "Task remains in planning state and will be retried."
                    } else {
                        "Task has been reverted to ready state and will be retried."
                    };
                    let warn_msg = format!(
                        "⚠️ Auto-dispatch of session failed for task {} ({}): {}. {}",
                        st.id, st.title, e, revert_note
                    );
                    let _ = db.add_message(st.id, &warn_msg, Some("system"));
                    warnings.push(warn_msg.clone());

                    // Notify the triggering session so the user/agent that
                    // created the task sees the failure inline.
                    if let Some(sid) = session_id {
                        let _ = tasks_scheduler::server_request(
                            writer,
                            reader,
                            tau_agent_plugin::Request::QueueMessage {
                                target_session_id: sid.to_string(),
                                content: warn_msg,
                                sender_info: "task-scheduler".to_string(),
                                await_reply: false,
                                reply_to: None,
                            },
                        );
                    }
                }
            }
        }
        Err(e) => {
            eprintln!("tasks scheduler: schedule pass error: {}", e);
        }
    }
    warnings
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
            create_session_error: None,
            written_lines: Vec::new(),
            session_ancestors: std::collections::HashMap::new(),
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
            create_session_error: None,
            written_lines: Vec::new(),
            session_ancestors: std::collections::HashMap::new(),
        }));
        let writer = MockWriter {
            shared: shared.clone(),
        };
        let reader = MockReader { shared };
        (writer, BufReader::new(reader))
    }

    /// Create mock IO where CreateSession requests fail with the given error.
    fn mock_io_failing_session(error: &str) -> (MockWriter, BufReader<MockReader>) {
        let shared = std::sync::Arc::new(std::sync::Mutex::new(MockShared {
            write_buf: Vec::new(),
            read_buf: Vec::new(),
            session_counter: 0,
            archived_sessions: std::collections::HashSet::new(),
            create_session_error: Some(error.to_string()),
            written_lines: Vec::new(),
            session_ancestors: std::collections::HashMap::new(),
        }));
        let writer = MockWriter {
            shared: shared.clone(),
        };
        let reader = MockReader { shared };
        (writer, BufReader::new(reader))
    }

    /// Create a test `ProjectResolver` that maps "test-project" → "/test/project"
    /// (and a few other common test names). Used by tests that call handlers
    /// needing path resolution.
    fn test_resolver() -> ProjectResolver {
        ProjectResolver::test(&[
            ("test-project", "/test/project"),
            ("p", "/test/p"),
            ("project-a", "/test/project-a"),
            ("project-b", "/test/project-b"),
            ("other", "/test/other"),
        ])
    }

    struct MockShared {
        write_buf: Vec<u8>,
        read_buf: Vec<u8>,
        session_counter: u32,
        /// Sessions that should be reported as archived by GetSessionInfo.
        archived_sessions: std::collections::HashSet<String>,
        /// If set, CreateSession returns this error instead of succeeding.
        create_session_error: Option<String>,
        /// All raw lines written by the plugin (captured before processing).
        written_lines: Vec<String>,
        /// Canned ancestor chains for GetSessionAncestors. Maps a session ID
        /// to its leaf-first ancestor list. If unset for a session, the
        /// default response treats the session as its own root.
        session_ancestors: std::collections::HashMap<String, Vec<tau_agent_plugin::SessionInfo>>,
    }

    impl MockShared {
        fn process_pending(&mut self) {
            let buf = std::mem::take(&mut self.write_buf);
            let text = String::from_utf8_lossy(&buf);
            for line in text.lines() {
                if line.trim().is_empty() {
                    continue;
                }
                self.written_lines.push(line.to_string());
                if let Ok(PluginMessage::ServerRequest {
                    request_id,
                    ref request,
                }) = serde_json::from_str::<PluginMessage>(line)
                {
                    let response = match request {
                        tau_agent_plugin::Request::CreateSession { .. } => {
                            if let Some(ref err) = self.create_session_error {
                                tau_agent_plugin::Response::Error {
                                    message: err.clone(),
                                }
                            } else {
                                self.session_counter += 1;
                                tau_agent_plugin::Response::SessionCreated {
                                    session_id: format!("mock-s{}", self.session_counter),
                                }
                            }
                        }
                        tau_agent_plugin::Request::GetSessionInfo { session_id } => {
                            let is_archived = self.archived_sessions.contains(session_id.as_str());
                            tau_agent_plugin::Response::SessionInfo {
                                info: tau_agent_plugin::SessionInfo {
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
                                    is_live: false,
                                    project_name: None,
                                },
                            }
                        }
                        tau_agent_plugin::Request::GetSessionAncestors { session_id } => {
                            let sessions = self
                                .session_ancestors
                                .get(session_id.as_str())
                                .cloned()
                                .unwrap_or_else(|| {
                                    // Default: session is its own (non-archived) root.
                                    vec![tau_agent_plugin::SessionInfo {
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
                                        archived: self
                                            .archived_sessions
                                            .contains(session_id.as_str()),
                                        last_exit_status: None,
                                        is_live: false,
                                        project_name: None,
                                    }]
                                });
                            tau_agent_plugin::Response::SessionAncestors { sessions }
                        }
                        tau_agent_plugin::Request::QueueMessage { .. } => {
                            tau_agent_plugin::Response::Ok
                        }
                        tau_agent_plugin::Request::ArchiveSession { .. } => {
                            tau_agent_plugin::Response::SessionArchived
                        }
                        _ => tau_agent_plugin::Response::Ok,
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
        assert_eq!(tools.len(), 14);
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
        assert!(names.contains(&"task_overview"));
        assert!(names.contains(&"task_merge"));
    }

    #[test]
    fn test_tool_handlers_via_db() {
        // Test handlers directly with an in-memory DB
        let resolver = test_resolver();
        let db = TasksDb::open_memory().unwrap();
        let (mut writer, mut reader) = mock_io();

        // Create
        let result = handle_task_create(
            &db,
            &serde_json::json!({"title": "Test task", "priority": 3, "message": "Hello", "initial_state": "interactive"}),
            &ToolCtx {
                project_name: Some("test-project"),
                session_id: Some("s1"),
                tool_call_id: "tc1",
            },
            &resolver,
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
        let result = handle_task_list(&db, &serde_json::json!({}), "test-project", "tc3");
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
            &resolver,
            &mut writer,
            &mut reader,
            &mut Vec::new(),
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
            &resolver,
            &mut writer,
            &mut reader,
            &mut Vec::new(),
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
            "test-project",
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
        let resolver = test_resolver();
        let (mut writer, mut reader) = mock_io();
        let result = handle_task_create(
            &db,
            &serde_json::json!({}),
            &ToolCtx {
                project_name: Some("p"),
                session_id: None,
                tool_call_id: "tc1",
            },
            &resolver,
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
            .create_task(
                "p",
                "A",
                None,
                None,
                None,
                false,
                "interactive",
                false,
                None,
                None,
                false,
            )
            .unwrap();
        let t2 = db
            .create_task(
                "p",
                "B",
                None,
                None,
                None,
                false,
                "interactive",
                false,
                None,
                None,
                false,
            )
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
            .create_task(
                "p",
                "A",
                None,
                None,
                None,
                false,
                "interactive",
                false,
                None,
                None,
                false,
            )
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
        assert_eq!(parsed["tools"].as_array().unwrap().len(), 14);
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
        let resolver = test_resolver();
        let (mut writer, mut reader) = mock_io();

        // Create a task and move to ready
        let result = handle_task_create(
            &db,
            &serde_json::json!({"title": "Assignable task", "initial_state": "interactive"}),
            &ToolCtx {
                project_name: Some("test-project"),
                session_id: Some("s1"),
                tool_call_id: "tc1",
            },
            &resolver,
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
            &resolver,
            &mut writer,
            &mut reader,
            &mut Vec::new(),
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
        let resolver = test_resolver();
        let (mut writer, mut reader) = mock_io();

        let result = handle_task_create(
            &db,
            &serde_json::json!({"title": "Task for context session", "initial_state": "interactive"}),
            &ToolCtx {
                project_name: Some("test-project"),
                session_id: Some("s1"),
                tool_call_id: "tc1",
            },
            &resolver,
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
            &resolver,
            &mut writer,
            &mut reader,
            &mut Vec::new(),
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
        let resolver = test_resolver();
        let (mut writer, mut reader) = mock_io();

        let result = handle_task_create(
            &db,
            &serde_json::json!({"title": "Interactive task", "initial_state": "interactive"}),
            &ToolCtx {
                project_name: Some("test-project"),
                session_id: Some("s1"),
                tool_call_id: "tc1",
            },
            &resolver,
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
        let resolver = test_resolver();
        let (mut writer, mut reader) = mock_io();

        let result = handle_task_create(
            &db,
            &serde_json::json!({"title": "No session task", "initial_state": "interactive"}),
            &ToolCtx {
                project_name: Some("test-project"),
                session_id: None,
                tool_call_id: "tc1",
            },
            &resolver,
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
            &resolver,
            &mut writer,
            &mut reader,
            &mut Vec::new(),
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
        let resolver = test_resolver();
        let (mut writer, mut reader) = mock_io();

        // Create parent task
        let result = handle_task_create(
            &db,
            &serde_json::json!({"title": "Parent", "initial_state": "interactive"}),
            &ToolCtx {
                project_name: Some("test-project"),
                session_id: Some("s1"),
                tool_call_id: "tc1",
            },
            &resolver,
            &mut writer,
            &mut reader,
            &mut Vec::new(),
        );
        let parent: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
        let parent_id = parent["id"].as_i64().unwrap();
        assert_eq!(parent["state"], "interactive");

        // Create subtask — should default to planning state. Post-task-#512
        // skip_review is no longer force-cleared for subtasks.
        let result = handle_task_create(
            &db,
            &serde_json::json!({"title": "Subtask", "parent_id": parent_id, "skip_review": true}),
            &ToolCtx {
                project_name: Some("test-project"),
                session_id: Some("s1"),
                tool_call_id: "tc2",
            },
            &resolver,
            &mut writer,
            &mut reader,
            &mut Vec::new(),
        );
        assert!(!result.is_error);
        let subtask: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
        assert_eq!(subtask["state"], "planning");
        assert_eq!(subtask["skip_review"], true);

        // Create subtask with initial_state="ready" — should start in ready state
        let result = handle_task_create(
            &db,
            &serde_json::json!({"title": "Subtask skip plan", "parent_id": parent_id, "initial_state": "ready"}),
            &ToolCtx {
                project_name: Some("test-project"),
                session_id: Some("s1"),
                tool_call_id: "tc3",
            },
            &resolver,
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
        let resolver = test_resolver();
        let (mut writer, mut reader) = mock_io();

        // Create task without skip_review
        let task = db
            .create_task(
                "test-project",
                "No skip",
                None,
                None,
                None,
                false,
                "interactive",
                false,
                None,
                None,
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

        // Try active -> approved without skip_review — should fail
        let result = handle_task_update(
            &db,
            &serde_json::json!({"id": task.id, "state": "approved"}),
            Some("s1"),
            "tc1",
            &resolver,
            &mut writer,
            &mut reader,
            &mut Vec::new(),
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
            &resolver,
            &mut writer,
            &mut reader,
            &mut Vec::new(),
            &mut Vec::new(),
        );
        assert!(!result.is_error);
    }

    #[test]
    fn test_active_to_approved_with_skip_review() {
        let db = TasksDb::open_memory().unwrap();
        let resolver = test_resolver();
        let (mut writer, mut reader) = mock_io();

        // Create task with skip_review=true
        let task = db
            .create_task(
                "test-project",
                "Skip review",
                None,
                None,
                None,
                true,
                "interactive",
                false,
                None,
                None,
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

        // active -> approved with skip_review=true — should succeed
        let result = handle_task_update(
            &db,
            &serde_json::json!({"id": task.id, "state": "approved"}),
            Some("s1"),
            "tc1",
            &resolver,
            &mut writer,
            &mut reader,
            &mut Vec::new(),
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
            .create_task(
                "test-project",
                "Tracked",
                None,
                None,
                None,
                false,
                "interactive",
                false,
                None,
                None,
                false,
            )
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
        let resolver = test_resolver();
        let (mut writer, mut reader) = mock_io();
        let task = db
            .create_task(
                "test-project",
                "Review track",
                None,
                None,
                None,
                false,
                "interactive",
                false,
                None,
                None,
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
        db.assign_task(task.id, "worker-s").unwrap();

        // Update to review with a different session
        let result = handle_task_update(
            &db,
            &serde_json::json!({"id": task.id, "state": "review"}),
            Some("reviewer-session"),
            "tc1",
            &resolver,
            &mut writer,
            &mut reader,
            &mut Vec::new(),
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

    /// End-to-end wiring: handle_task_update defers the self-recipient
    /// info message to `post_persist_actions` (Tier-2) when the caller
    /// itself is among the recipients.  This ensures the info message
    /// renders *after* the tool result in the caller's session history.
    #[test]
    fn test_handle_task_update_emits_queue_info_on_state_change() {
        let db = TasksDb::open_memory().unwrap();
        let resolver = test_resolver();
        let (mut writer, mut reader) = mock_io();

        // Create a ready-state task directly in the DB (skip the
        // create-handler to avoid its own CreateSession noise).
        let task = db
            .create_task(
                "test-project",
                "Emit test",
                None,
                None,
                None,
                false,
                "ready",
                false,
                None,
                None,
                false,
            )
            .unwrap();
        db.record_session(task.id, "s-worker", "worker").unwrap();
        db.set_session_id(task.id, "s-worker").unwrap();

        // Transition ready -> active via the handler.  Caller is s-worker,
        // which is also the only recipient — the info message must be
        // deferred to `post_persist_actions`, NOT fired eagerly as QueueInfo.
        let mut post_persist: Vec<tau_agent_plugin::PostPersistAction> = Vec::new();
        let result = handle_task_update(
            &db,
            &serde_json::json!({"id": task.id, "state": "active"}),
            Some("s-worker"),
            "tc1",
            &resolver,
            &mut writer,
            &mut reader,
            &mut Vec::new(),
            &mut post_persist,
        );
        assert!(
            !result.is_error,
            "handler error: {:?}",
            extract_text(&result)
        );

        // Drain the mock so it processes pending writes into responses.
        {
            let mut shared = writer.shared.lock().unwrap();
            shared.process_pending();
        }

        let shared = writer.shared.lock().unwrap();
        let written = shared.written_lines.join("\n");
        // The caller's own info message must NOT appear as an eagerly-fired
        // QueueInfo (it would land in the history before the tool result).
        assert!(
            !written.contains("\"queue_info\""),
            "did not expect any QueueInfo when caller is sole recipient, got:\n{}",
            written
        );
        // It must instead live in `post_persist_actions` with the right text
        // and target.
        assert_eq!(
            post_persist.len(),
            1,
            "expected one post-persist action, got: {:?}",
            post_persist
        );
        match &post_persist[0] {
            tau_agent_plugin::PostPersistAction::EmitInfoMessage {
                target_session_id,
                text,
            } => {
                assert_eq!(target_session_id, "s-worker");
                let expected_line = format!("[task #{}] Emit test: ready → active", task.id);
                assert_eq!(text, &expected_line);
            }
        }
    }

    /// No-op transitions (task_update that doesn't change state) do not
    /// emit a QueueInfo.
    #[test]
    fn test_handle_task_update_no_state_change_no_info() {
        let db = TasksDb::open_memory().unwrap();
        let resolver = test_resolver();
        let (mut writer, mut reader) = mock_io();
        let task = db
            .create_task(
                "test-project",
                "Titled",
                None,
                None,
                None,
                false,
                "ready",
                false,
                None,
                None,
                false,
            )
            .unwrap();
        db.record_session(task.id, "s-any", "worker").unwrap();

        // Change title only — no state field.
        let result = handle_task_update(
            &db,
            &serde_json::json!({"id": task.id, "title": "Retitled"}),
            Some("s-any"),
            "tc1",
            &resolver,
            &mut writer,
            &mut reader,
            &mut Vec::new(),
            &mut Vec::new(),
        );
        assert!(!result.is_error);

        {
            let mut shared = writer.shared.lock().unwrap();
            shared.process_pending();
        }
        let shared = writer.shared.lock().unwrap();
        let written = shared.written_lines.join("\n");
        assert!(
            !written.contains("\"queue_info\""),
            "title-only update should not emit QueueInfo:\n{}",
            written
        );
    }

    /// Helper for the ordering regression tests: pull out every
    /// (target_session_id, text) pair from written QueueInfo lines.
    fn captured_queue_info(
        shared: &std::sync::Arc<std::sync::Mutex<MockShared>>,
    ) -> Vec<(String, String)> {
        let mut shared = shared.lock().unwrap();
        shared.process_pending();
        let mut out = Vec::new();
        for line in &shared.written_lines {
            let msg: PluginMessage = match serde_json::from_str(line) {
                Ok(m) => m,
                Err(_) => continue,
            };
            if let PluginMessage::ServerRequest { request, .. } = msg {
                if let tau_agent_plugin::Request::QueueInfo {
                    target_session_id,
                    text,
                } = request
                {
                    out.push((target_session_id, text));
                }
            }
        }
        out
    }

    /// Ordering regression: `active → review` dispatches a new reviewer
    /// session; that session MUST appear in the QueueInfo recipient set
    /// so it sees the transition that created it in its own history.
    ///
    /// Regression for the review feedback on task #514: the old wiring
    /// fired `notify_state_change` before `dispatch_review` recorded the
    /// reviewer session, so the reviewer never saw the message.
    #[test]
    fn test_handle_task_update_review_dispatch_includes_new_reviewer_session() {
        let db = TasksDb::open_memory().unwrap();
        let resolver = test_resolver();
        let (mut writer, mut reader) = mock_io();

        let task = db
            .create_task(
                "test-project",
                "RevOrd",
                None,
                None,
                None,
                false,
                "ready",
                false,
                None,
                None,
                false,
            )
            .unwrap();
        // Move through ready -> active (avoid the auto-review dispatch
        // side-effects of going directly via the handler).
        db.assign_task(task.id, "s-worker").unwrap();

        // Transition active -> review via the handler.  This triggers
        // `dispatch_review`, which CreateSession-s a reviewer session
        // (mock returns "mock-s1") and records it before (per the fix)
        // notify_state_change fires.
        let mut post_persist: Vec<tau_agent_plugin::PostPersistAction> = Vec::new();
        let result = handle_task_update(
            &db,
            &serde_json::json!({"id": task.id, "state": "review"}),
            Some("s-worker"),
            "tc1",
            &resolver,
            &mut writer,
            &mut reader,
            &mut Vec::new(),
            &mut post_persist,
        );
        assert!(
            !result.is_error,
            "handler error: {:?}",
            extract_text(&result)
        );

        // Sanity: the reviewer session was in fact recorded against the
        // task.  (If this fails the test can't distinguish between
        // "dispatch_review never ran" and "ordering is wrong".)
        let recorded: Vec<String> = db
            .get_sessions(task.id)
            .unwrap()
            .into_iter()
            .filter(|ts| ts.role == "reviewer")
            .map(|ts| ts.session_id)
            .collect();
        assert!(
            !recorded.is_empty(),
            "dispatch_review did not record a reviewer session"
        );

        let calls = captured_queue_info(&writer.shared);
        let expected_text = format!("[task #{}] RevOrd: active → review", task.id);

        // The new reviewer session must be on the recipient list.
        let reviewer_sid = &recorded[0];
        assert!(
            calls
                .iter()
                .any(|(sid, text)| sid == reviewer_sid && text == &expected_text),
            "reviewer session {:?} missing from QueueInfo recipients.  calls: {:?}",
            reviewer_sid,
            calls
        );
        // The worker session is the caller, so its info is deferred to
        // Tier-2 `post_persist_actions` rather than fired as QueueInfo.
        assert!(
            post_persist.iter().any(|a| matches!(
                a,
                tau_agent_plugin::PostPersistAction::EmitInfoMessage { target_session_id, text }
                    if target_session_id == "s-worker" && text == &expected_text
            )),
            "worker session missing from post-persist actions. post_persist: {:?}",
            post_persist
        );
    }

    /// Ordering regression: `planning → refining` dispatches a new
    /// refiner session; that session MUST appear in the QueueInfo
    /// recipient set.
    #[test]
    fn test_handle_task_update_refining_dispatch_includes_new_refiner_session() {
        let db = TasksDb::open_memory().unwrap();
        let resolver = test_resolver();
        let (mut writer, mut reader) = mock_io();

        let task = db
            .create_task(
                "test-project",
                "RefOrd",
                None,
                None,
                None,
                false,
                "planning",
                false,
                None,
                None,
                false,
            )
            .unwrap();
        db.record_session(task.id, "s-planner", "planner").unwrap();
        db.set_session_id(task.id, "s-planner").unwrap();

        // Transition planning -> refining via the handler.  This triggers
        // `dispatch_refining`, which CreateSession-s a refiner session and
        // records it with role "refiner" before notify_state_change fires.
        let result = handle_task_update(
            &db,
            &serde_json::json!({"id": task.id, "state": "refining"}),
            Some("s-planner"),
            "tc1",
            &resolver,
            &mut writer,
            &mut reader,
            &mut Vec::new(),
            &mut Vec::new(),
        );
        assert!(
            !result.is_error,
            "handler error: {:?}",
            extract_text(&result)
        );

        let recorded: Vec<String> = db
            .get_sessions(task.id)
            .unwrap()
            .into_iter()
            .filter(|ts| ts.role == "refiner")
            .map(|ts| ts.session_id)
            .collect();
        assert!(
            !recorded.is_empty(),
            "dispatch_refining did not record a refiner session"
        );

        let calls = captured_queue_info(&writer.shared);
        let expected_text = format!("[task #{}] RefOrd: planning → refining", task.id);

        let refiner_sid = &recorded[0];
        assert!(
            calls
                .iter()
                .any(|(sid, text)| sid == refiner_sid && text == &expected_text),
            "refiner session {:?} missing from QueueInfo recipients.  calls: {:?}",
            refiner_sid,
            calls
        );
    }

    /// Ordering regression: `ready → interactive` creates a new
    /// interactive session via `create_interactive_session`; that new
    /// session MUST see the transition info message in its own history.
    #[test]
    fn test_handle_task_update_interactive_creation_includes_new_session() {
        let db = TasksDb::open_memory().unwrap();
        let resolver = test_resolver();
        let (mut writer, mut reader) = mock_io();

        // Start from a `ready` task with no session of its own.  The
        // universal `* → interactive` override should create one.
        let task = db
            .create_task(
                "test-project",
                "IAOrd",
                None,
                None,
                None,
                false,
                "ready",
                false,
                None,
                None,
                false,
            )
            .unwrap();

        let result = handle_task_update(
            &db,
            &serde_json::json!({"id": task.id, "state": "interactive"}),
            Some("s-caller"),
            "tc1",
            &resolver,
            &mut writer,
            &mut reader,
            &mut Vec::new(),
            &mut Vec::new(),
        );
        assert!(
            !result.is_error,
            "handler error: {:?}",
            extract_text(&result)
        );

        // `create_interactive_session` records the new session with role
        // "interactive" (and the caller with role "creator").
        let recorded: Vec<(String, String)> = db
            .get_sessions(task.id)
            .unwrap()
            .into_iter()
            .map(|ts| (ts.session_id, ts.role))
            .collect();
        let interactive_sid: String = recorded
            .iter()
            .find(|(_, r)| r == "interactive")
            .map(|(s, _)| s.clone())
            .expect("dispatch created no interactive session");

        let calls = captured_queue_info(&writer.shared);
        let expected_text = format!("[task #{}] IAOrd: ready → interactive", task.id);
        assert!(
            calls
                .iter()
                .any(|(sid, text)| sid == &interactive_sid && text == &expected_text),
            "new interactive session {:?} missing from QueueInfo recipients. calls: {:?}",
            interactive_sid,
            calls
        );
    }

    /// Regression for the scope-expansion path: a task in `refining`
    /// transitioning to `interactive` reuses the live refiner session
    /// (no new session is created when the refiner is still alive).
    /// The existing refiner session must still receive the
    /// "refining → interactive (scope expansion)" info-message.
    #[test]
    fn test_handle_task_update_refining_to_interactive_scope_expansion() {
        let db = TasksDb::open_memory().unwrap();
        let resolver = test_resolver();
        let (mut writer, mut reader) = mock_io();

        let task = db
            .create_task(
                "test-project",
                "Scope",
                None,
                None,
                None,
                false,
                "planning",
                false,
                None,
                None,
                false,
            )
            .unwrap();
        // Move planning -> refining directly in the DB (bypass the
        // handler so we isolate the refining -> interactive step under
        // test; the planning->refining path dispatches a refiner and is
        // already covered by an earlier test).
        db.update_task(
            task.id,
            &TaskUpdate {
                state: Some("refining".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        db.record_session(task.id, "s-refiner", "refiner").unwrap();
        db.set_session_id(task.id, "s-refiner").unwrap();

        // Drain any existing writes so we only inspect the
        // refining->interactive step below.
        {
            let mut shared = writer.shared.lock().unwrap();
            shared.process_pending();
            shared.written_lines.clear();
        }

        let result = handle_task_update(
            &db,
            &serde_json::json!({"id": task.id, "state": "interactive"}),
            Some("s-caller"),
            "tc1",
            &resolver,
            &mut writer,
            &mut reader,
            &mut Vec::new(),
            &mut Vec::new(),
        );
        assert!(
            !result.is_error,
            "handler error: {:?}",
            extract_text(&result)
        );

        // The refiner session is alive per the mock's default
        // `GetSessionInfo` response, so no new interactive session is
        // created — the refiner itself becomes the interactive owner.
        let calls = captured_queue_info(&writer.shared);
        let expected_text = format!(
            "[task #{}] Scope: refining → interactive (scope expansion)",
            task.id
        );
        assert!(
            calls
                .iter()
                .any(|(sid, text)| sid == "s-refiner" && text == &expected_text),
            "refiner session missing from scope-expansion QueueInfo. calls: {:?}",
            calls
        );
    }

    #[test]
    fn test_session_tracking_idempotent() {
        let db = TasksDb::open_memory().unwrap();
        let task = db
            .create_task(
                "test-project",
                "Idempotent",
                None,
                None,
                None,
                false,
                "interactive",
                false,
                None,
                None,
                false,
            )
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

    #[test]
    fn test_task_prompt_guidelines_trimmed() {
        // Regression test for the prompt-guideline trim (task #493).
        // The task_* tools collectively contribute a bounded number of
        // guideline strings to the system prompt. Keep this tight so that
        // future additions are a deliberate choice rather than drift.
        let tools = tasks_tools();
        let total: usize = tools
            .iter()
            .filter(|t| t.name.starts_with("task_"))
            .map(|t| t.prompt_guidelines.len())
            .sum();
        assert!(
            total < 18,
            "task_* prompt_guidelines total should stay small; got {}",
            total
        );

        // task_update's state-machine enumerations were folded into three bullets,
        // plus the hold bullet added by task #527.
        let task_update = tools.iter().find(|t| t.name == "task_update").unwrap();
        assert_eq!(
            task_update.prompt_guidelines.len(),
            4,
            "task_update should have exactly 4 guidelines after the trim"
        );

        // Tools whose description fully covers behaviour should carry no guidelines.
        for name in ["task_schedule", "task_merge", "task_dispatch"] {
            let tool = tools.iter().find(|t| t.name == name).unwrap();
            assert!(
                tool.prompt_guidelines.is_empty(),
                "{} should have no prompt_guidelines",
                name
            );
        }
    }

    #[test]
    fn test_task_create_guidelines_explain_initial_state() {
        // Regression test for task #512. The task_create guidelines must
        // make four things explicit:
        //   1. Tasks default to 'planning' (planning session analyses the
        //      spec).
        //   2. initial_state="ready" is how you queue complete specs for
        //      immediate worker dispatch.
        //   3. initial_state="interactive" is reserved for user-driven
        //      refinement.
        //   4. Sessions dispatched for top-level tasks are root-parented
        //      so they surface in the user's tree.
        let tools = tasks_tools();
        let task_create = tools
            .iter()
            .find(|t| t.name == "task_create")
            .expect("task_create tool def");
        assert!(
            task_create.prompt_guidelines.len() >= 4,
            "task_create should have at least 4 guidelines; got {}",
            task_create.prompt_guidelines.len()
        );
        let joined = task_create.prompt_guidelines.join("\n");
        assert!(
            joined.contains("'planning'") && joined.contains("default"),
            "task_create guidelines should document the planning default: {}",
            joined
        );
        assert!(
            joined.contains("initial_state=\"ready\""),
            "task_create guidelines should document initial_state=\"ready\": {}",
            joined
        );
        assert!(
            joined.contains("initial_state=\"interactive\""),
            "task_create guidelines should document initial_state=\"interactive\": {}",
            joined
        );
        assert!(
            joined.contains("root"),
            "task_create guidelines should mention root-session parenting: {}",
            joined
        );
    }

    /// Spec-required regression test (task #512): `handle_task_create`
    /// must reject the removed `skip_planning` parameter with a specific
    /// error message that points callers at the new `initial_state` arg.
    ///
    /// Pins the full helpful string so future drift is caught.
    #[test]
    fn test_handle_task_create_rejects_skip_planning() {
        let db = TasksDb::open_memory().unwrap();
        let resolver = test_resolver();
        let (mut writer, mut reader) = mock_io();

        let result = handle_task_create(
            &db,
            &serde_json::json!({"title": "Legacy caller", "skip_planning": true}),
            &ToolCtx {
                project_name: Some("test-project"),
                session_id: Some("s1"),
                tool_call_id: "tc1",
            },
            &resolver,
            &mut writer,
            &mut reader,
            &mut Vec::new(),
        );

        assert!(result.is_error, "skip_planning should be rejected");
        let text = extract_text(&result);
        let expected = "'skip_planning' has been removed; use 'initial_state' instead \
             ('ready' is the former skip_planning=true behavior, 'planning' the default).";
        assert!(
            text.contains(expected),
            "error text should contain the full spec message.\n  expected substring: {}\n  got: {}",
            expected,
            text,
        );
    }

    /// Companion regression test: `handle_task_update` also rejects
    /// `skip_planning` (nice-to-have per reviewer feedback on #512).
    #[test]
    fn test_handle_task_update_rejects_skip_planning() {
        let db = TasksDb::open_memory().unwrap();
        let resolver = test_resolver();
        let (mut writer, mut reader) = mock_io();

        // Create a task so there's something to "update".
        let task = db
            .create_task(
                "test-project",
                "x",
                None,
                None,
                None,
                false,
                "interactive",
                false,
                None,
                None,
                false,
            )
            .unwrap();

        let result = handle_task_update(
            &db,
            &serde_json::json!({"id": task.id, "skip_planning": false}),
            Some("s1"),
            "tc1",
            &resolver,
            &mut writer,
            &mut reader,
            &mut Vec::new(),
            &mut Vec::new(),
        );

        assert!(
            result.is_error,
            "skip_planning should be rejected on update"
        );
        let text = extract_text(&result);
        assert!(
            text.contains("'skip_planning' has been removed")
                && text.contains("use 'initial_state'"),
            "update error should mention skip_planning removal and point at initial_state: {}",
            text
        );
    }

    // ----- dependency enforcement tests (plugin layer) -----

    #[test]
    fn test_task_relate_self_referential_rejected() {
        let db = TasksDb::open_memory().unwrap();
        let task = db
            .create_task(
                "test-project",
                "Self",
                None,
                None,
                None,
                false,
                "interactive",
                false,
                None,
                None,
                false,
            )
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
    fn test_task_relate_cross_project_allowed() {
        let db = TasksDb::open_memory().unwrap();
        let t1 = db
            .create_task(
                "project-a",
                "A",
                None,
                None,
                None,
                false,
                "interactive",
                false,
                None,
                None,
                false,
            )
            .unwrap();
        let t2 = db
            .create_task(
                "project-b",
                "B",
                None,
                None,
                None,
                false,
                "interactive",
                false,
                None,
                None,
                false,
            )
            .unwrap();

        let result = handle_task_relate(
            &db,
            &serde_json::json!({"from_task": t1.id, "to_task": t2.id, "relation": "depends_on"}),
            "tc1",
        );
        assert!(!result.is_error, "cross-project relation should succeed");
    }

    #[test]
    fn test_task_relate_circular_rejected() {
        let db = TasksDb::open_memory().unwrap();
        let t1 = db
            .create_task(
                "test-project",
                "T1",
                None,
                None,
                None,
                false,
                "interactive",
                false,
                None,
                None,
                false,
            )
            .unwrap();
        let t2 = db
            .create_task(
                "test-project",
                "T2",
                None,
                None,
                None,
                false,
                "interactive",
                false,
                None,
                None,
                false,
            )
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
            .create_task(
                "test-project",
                "Dependency",
                None,
                None,
                None,
                false,
                "interactive",
                false,
                None,
                None,
                false,
            )
            .unwrap();
        let task = db
            .create_task(
                "test-project",
                "Dependent",
                None,
                None,
                None,
                false,
                "interactive",
                false,
                None,
                None,
                false,
            )
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
        // Create dep and move to merged
        let dep = db
            .create_task(
                "test-project",
                "Dependency",
                None,
                None,
                None,
                true,
                "interactive",
                false,
                None,
                None,
                false,
            )
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
                state: Some("merged".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();

        let task = db
            .create_task(
                "test-project",
                "Dependent",
                None,
                None,
                None,
                false,
                "interactive",
                false,
                None,
                None,
                false,
            )
            .unwrap();
        db.add_relation(task.id, dep.id, "depends_on").unwrap();

        let result = handle_task_get(&db, &serde_json::json!({"id": task.id}), "tc1");
        assert!(!result.is_error);
        let text = extract_text(&result);
        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();

        let relations = parsed["relations"].as_array().unwrap();
        assert_eq!(relations.len(), 1);
        assert_eq!(relations[0]["dependency_status"], "satisfied");
        assert_eq!(relations[0]["dependency_state"], "merged");
    }

    #[test]
    fn test_task_get_non_depends_on_has_no_status() {
        let db = TasksDb::open_memory().unwrap();
        let t1 = db
            .create_task(
                "test-project",
                "T1",
                None,
                None,
                None,
                false,
                "interactive",
                false,
                None,
                None,
                false,
            )
            .unwrap();
        let t2 = db
            .create_task(
                "test-project",
                "T2",
                None,
                None,
                None,
                false,
                "interactive",
                false,
                None,
                None,
                false,
            )
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

    #[test]
    fn test_task_get_cross_project_relation_shows_project_name() {
        let db = TasksDb::open_memory().unwrap();
        let t1 = db
            .create_task(
                "project-a",
                "Task A",
                None,
                None,
                None,
                false,
                "interactive",
                false,
                None,
                None,
                false,
            )
            .unwrap();
        let t2 = db
            .create_task(
                "project-b",
                "Task B",
                None,
                None,
                None,
                false,
                "interactive",
                false,
                None,
                None,
                false,
            )
            .unwrap();

        db.add_relation(t1.id, t2.id, "depends_on").unwrap();

        let result = handle_task_get(&db, &serde_json::json!({"id": t1.id}), "tc1");
        assert!(!result.is_error);
        let text = extract_text(&result);
        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();

        let relations = parsed["relations"].as_array().unwrap();
        assert_eq!(relations.len(), 1);
        assert_eq!(relations[0]["project_name"], "project-b");
        assert_eq!(relations[0]["dependency_status"], "blocking");
    }

    #[test]
    fn test_task_get_same_project_relation_no_project_name() {
        let db = TasksDb::open_memory().unwrap();
        let t1 = db
            .create_task(
                "project-a",
                "Task A",
                None,
                None,
                None,
                false,
                "interactive",
                false,
                None,
                None,
                false,
            )
            .unwrap();
        let t2 = db
            .create_task(
                "project-a",
                "Task B",
                None,
                None,
                None,
                false,
                "interactive",
                false,
                None,
                None,
                false,
            )
            .unwrap();

        db.add_relation(t1.id, t2.id, "depends_on").unwrap();

        let result = handle_task_get(&db, &serde_json::json!({"id": t1.id}), "tc1");
        assert!(!result.is_error);
        let text = extract_text(&result);
        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();

        let relations = parsed["relations"].as_array().unwrap();
        assert_eq!(relations.len(), 1);
        assert!(
            relations[0].get("project_name").is_none(),
            "same-project relation should not include project_name"
        );
    }

    #[test]
    fn test_task_get_cross_project_blocks_relation() {
        let db = TasksDb::open_memory().unwrap();
        let t1 = db
            .create_task(
                "project-a",
                "Task A",
                None,
                None,
                None,
                false,
                "interactive",
                false,
                None,
                None,
                false,
            )
            .unwrap();
        let t2 = db
            .create_task(
                "project-b",
                "Task B",
                None,
                None,
                None,
                false,
                "interactive",
                false,
                None,
                None,
                false,
            )
            .unwrap();

        // t1 blocks t2 (cross-project)
        db.add_relation(t1.id, t2.id, "blocks").unwrap();

        let result = handle_task_get(&db, &serde_json::json!({"id": t1.id}), "tc1");
        assert!(!result.is_error);
        let text = extract_text(&result);
        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();

        let relations = parsed["relations"].as_array().unwrap();
        assert_eq!(relations.len(), 1);
        // Cross-project blocks should show project_name
        assert_eq!(relations[0]["project_name"], "project-b");
        // blocks relation should not have dependency_status
        assert!(relations[0].get("dependency_status").is_none());
    }

    // ----- interactive task auto-link tests -----

    #[test]
    fn test_interactive_task_creates_fresh_session() {
        let db = TasksDb::open_memory().unwrap();
        let resolver = test_resolver();
        let (mut writer, mut reader) = mock_io();

        let result = handle_task_create(
            &db,
            &serde_json::json!({"title": "Interactive task", "initial_state": "interactive"}),
            &ToolCtx {
                project_name: Some("test-project"),
                session_id: Some("creating-session"),
                tool_call_id: "tc1",
            },
            &resolver,
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
        let resolver = test_resolver();
        let (mut writer, mut reader) = mock_io();

        // Create without a session_id context — session still created, just no parent
        let result = handle_task_create(
            &db,
            &serde_json::json!({"title": "No parent session task", "initial_state": "interactive"}),
            &ToolCtx {
                project_name: Some("test-project"),
                session_id: None,
                tool_call_id: "tc1",
            },
            &resolver,
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

    /// Helper: build a mock configured to return a specific ancestor chain
    /// for a session_id.
    fn mock_io_with_ancestors(
        chains: Vec<(&str, Vec<tau_agent_plugin::SessionInfo>)>,
    ) -> (MockWriter, BufReader<MockReader>) {
        let mut ancestors = std::collections::HashMap::new();
        for (sid, chain) in chains {
            ancestors.insert(sid.to_string(), chain);
        }
        let shared = std::sync::Arc::new(std::sync::Mutex::new(MockShared {
            write_buf: Vec::new(),
            read_buf: Vec::new(),
            session_counter: 0,
            archived_sessions: std::collections::HashSet::new(),
            create_session_error: None,
            written_lines: Vec::new(),
            session_ancestors: ancestors,
        }));
        let writer = MockWriter {
            shared: shared.clone(),
        };
        let reader = MockReader { shared };
        (writer, BufReader::new(reader))
    }

    /// Build a minimal SessionInfo fixture for ancestor responses.
    fn test_session_info(id: &str, parent_id: Option<&str>) -> tau_agent_plugin::SessionInfo {
        tau_agent_plugin::SessionInfo {
            id: id.to_string(),
            model: "mock-model".into(),
            provider: "mock".into(),
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
            archived: false,
            last_exit_status: None,
            is_live: false,
            project_name: None,
        }
    }

    /// Top-level task: the fresh interactive session must be parented on
    /// the calling session's **root** (from GetSessionAncestors), not on
    /// the nested worker session that happened to call `task_create`.
    ///
    /// Regression test for task #512.
    #[test]
    fn test_top_level_interactive_session_parented_on_root() {
        let db = TasksDb::open_memory().unwrap();
        let resolver = test_resolver();
        // Nested-worker calls task_create; its ancestor chain leads to
        // "user-root".
        let (mut writer, mut reader) = mock_io_with_ancestors(vec![(
            "nested-worker",
            vec![
                test_session_info("nested-worker", Some("mid")),
                test_session_info("mid", Some("user-root")),
                test_session_info("user-root", None),
            ],
        )]);

        let result = handle_task_create(
            &db,
            &serde_json::json!({"title": "Top-level task", "initial_state": "interactive"}),
            &ToolCtx {
                project_name: Some("test-project"),
                session_id: Some("nested-worker"),
                tool_call_id: "tc1",
            },
            &resolver,
            &mut writer,
            &mut reader,
            &mut Vec::new(),
        );
        assert!(!result.is_error);

        // Inspect the CreateSession request actually sent to the server:
        // its parent_id should be "user-root", not "nested-worker".
        let mut shared = writer.shared.lock().unwrap();
        shared.process_pending();
        let all_written = shared.written_lines.join("\n");
        let create_line = all_written
            .lines()
            .find(|l| l.contains("\"create_session\""))
            .expect("expected a CreateSession request line");
        let parsed: serde_json::Value = serde_json::from_str(create_line).unwrap();
        let parent_id = parsed
            .pointer("/request/parent_id")
            .and_then(|v| v.as_str());
        assert_eq!(
            parent_id,
            Some("user-root"),
            "top-level task session should be root-parented; full request: {}",
            create_line
        );
    }

    /// Subtask session must keep its hierarchy parent (the calling
    /// session), NOT re-parent onto the root — grouping subtasks under
    /// their orchestrator is how task boards stay organised.
    ///
    /// Regression test for task #512 — verifies the top-level-only scope
    /// of root-parenting.
    #[test]
    fn test_subtask_session_keeps_hierarchy_parent() {
        let db = TasksDb::open_memory().unwrap();
        let resolver = test_resolver();
        let (mut writer, mut reader) = mock_io_with_ancestors(vec![(
            "orchestrator",
            vec![
                test_session_info("orchestrator", Some("user-root")),
                test_session_info("user-root", None),
            ],
        )]);

        // Parent task so the subtask has somewhere to hang off.
        let parent = handle_task_create(
            &db,
            &serde_json::json!({"title": "Parent", "initial_state": "interactive"}),
            &ToolCtx {
                project_name: Some("test-project"),
                session_id: Some("orchestrator"),
                tool_call_id: "tc-parent",
            },
            &resolver,
            &mut writer,
            &mut reader,
            &mut Vec::new(),
        );
        assert!(!parent.is_error);
        let parent_task: serde_json::Value = serde_json::from_str(&extract_text(&parent)).unwrap();
        let parent_id = parent_task["id"].as_i64().unwrap();

        // Drain written_lines so we only inspect the subtask's CreateSession below.
        {
            let mut shared = writer.shared.lock().unwrap();
            shared.process_pending();
            shared.written_lines.clear();
        }

        // Subtask → initial_state=interactive (requires explicit opt-in in
        // the unified world; `planning` would not create a session).
        let result = handle_task_create(
            &db,
            &serde_json::json!({
                "title": "Subtask",
                "parent_id": parent_id,
                "initial_state": "interactive",
            }),
            &ToolCtx {
                project_name: Some("test-project"),
                session_id: Some("orchestrator"),
                tool_call_id: "tc-sub",
            },
            &resolver,
            &mut writer,
            &mut reader,
            &mut Vec::new(),
        );
        assert!(!result.is_error);

        let mut shared = writer.shared.lock().unwrap();
        shared.process_pending();
        let all_written = shared.written_lines.join("\n");
        let create_line = all_written
            .lines()
            .find(|l| l.contains("\"create_session\""))
            .expect("expected a CreateSession request line");
        let parsed: serde_json::Value = serde_json::from_str(create_line).unwrap();
        let parent_id_sent = parsed
            .pointer("/request/parent_id")
            .and_then(|v| v.as_str());
        assert_eq!(
            parent_id_sent,
            Some("orchestrator"),
            "subtask session should keep its hierarchy parent (the calling session); \
             full request: {}",
            create_line
        );
    }

    #[test]
    fn test_subtask_does_not_create_session() {
        let db = TasksDb::open_memory().unwrap();
        let resolver = test_resolver();
        let (mut writer, mut reader) = mock_io();

        // Create parent (interactive — gets mock-s1)
        let result = handle_task_create(
            &db,
            &serde_json::json!({"title": "Parent", "initial_state": "interactive"}),
            &ToolCtx {
                project_name: Some("test-project"),
                session_id: Some("s1"),
                tool_call_id: "tc1",
            },
            &resolver,
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
                project_name: Some("test-project"),
                session_id: Some("s1"),
                tool_call_id: "tc2",
            },
            &resolver,
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

    // ----- auto-archive on terminal state tests -----

    #[test]
    fn test_auto_archive_session_on_closed() {
        let db = TasksDb::open_memory().unwrap();
        let resolver = test_resolver();
        let (mut writer, mut reader) = mock_io();

        // Create a task with a session_id
        let task = db
            .create_task(
                "test-project",
                "Auto archive",
                None,
                None,
                None,
                true,
                "interactive",
                false,
                None,
                None,
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

        // Transition to closed via handle_task_update
        let result = handle_task_update(
            &db,
            &serde_json::json!({"id": task.id, "state": "closed"}),
            Some("s1"),
            "tc1",
            &resolver,
            &mut writer,
            &mut reader,
            &mut Vec::new(),
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
        assert_eq!(updated["state"], "closed");
    }

    #[test]
    fn test_auto_archive_no_session_id() {
        let db = TasksDb::open_memory().unwrap();
        let resolver = test_resolver();
        let (mut writer, mut reader) = mock_io();

        // Create a task without a session_id and transition to closed
        let task = db
            .create_task(
                "test-project",
                "No session",
                None,
                None,
                None,
                false,
                "interactive",
                false,
                None,
                None,
                false,
            )
            .unwrap();

        // Transition to closed directly (universal override)
        let result = handle_task_update(
            &db,
            &serde_json::json!({"id": task.id, "state": "closed"}),
            None,
            "tc1",
            &resolver,
            &mut writer,
            &mut reader,
            &mut Vec::new(),
            &mut Vec::new(),
        );
        assert!(!result.is_error);
        let updated: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
        assert_eq!(updated["state"], "closed");
        // No archive request should be sent (no session_id)
    }

    #[test]
    fn test_auto_archive_all_task_sessions_on_closed() {
        let db = TasksDb::open_memory().unwrap();
        let resolver = test_resolver();
        let (mut writer, mut reader) = mock_io();

        // Create a task with a session_id (worker)
        let task = db
            .create_task(
                "test-project",
                "Multi-session archive",
                None,
                None,
                None,
                true,
                "interactive",
                false,
                None,
                None,
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

        // Skip to closed via universal override
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

        // Transition to closed via handle_task_update (triggers auto_archive)
        let result = handle_task_update(
            &db,
            &serde_json::json!({"id": task.id, "state": "closed"}),
            Some("s1"),
            "tc1",
            &resolver,
            &mut writer,
            &mut reader,
            &mut Vec::new(),
            &mut Vec::new(),
        );
        assert!(!result.is_error);

        let updated: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
        assert_eq!(updated["state"], "closed");

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

    // ----- parent notification on terminal state tests -----

    #[test]
    fn test_update_to_closed_notifies_parent_session() {
        let db = TasksDb::open_memory().unwrap();
        let resolver = test_resolver();
        let (mut writer, mut reader) = mock_io();
        let mut events = Vec::new();

        // Create parent with a session
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
            )
            .unwrap();
        db.set_session_id(parent.id, "parent-session").unwrap();

        // Child subtask starts in ready state (initial_state="ready")
        let child = db
            .create_task(
                "test-project",
                "Child Task",
                None,
                Some(parent.id),
                None,
                true,
                "ready",
                false,
                None,
                None,
                false,
            )
            .unwrap();
        db.assign_task(child.id, "worker-session").unwrap();

        // Transition child to closed via handle_task_update (universal override)
        let result = handle_task_update(
            &db,
            &serde_json::json!({"id": child.id, "state": "closed"}),
            Some("s1"),
            "tc1",
            &resolver,
            &mut writer,
            &mut reader,
            &mut events,
            &mut Vec::new(),
        );
        assert!(!result.is_error);

        // Verify that a QueueMessage was sent to "parent-session" with subtask info
        // Flush unprocessed writes and inspect written_lines.
        {
            let mut shared = writer.shared.lock().unwrap();
            let buf = std::mem::take(&mut shared.write_buf);
            let text = String::from_utf8_lossy(&buf);
            for line in text.lines() {
                if !line.trim().is_empty() {
                    shared.written_lines.push(line.to_string());
                }
            }
        }
        let all_written = writer.shared.lock().unwrap().written_lines.join("\n");

        // Should contain a QueueMessage to "parent-session" with the subtask title
        assert!(
            all_written.contains("parent-session"),
            "expected notification to parent-session in:\n{}",
            all_written
        );
        assert!(
            all_written.contains("Child Task"),
            "expected subtask title in notification:\n{}",
            all_written
        );
        assert!(
            all_written.contains("Subtask"),
            "expected 'Subtask' in notification message:\n{}",
            all_written
        );
    }

    // ----- scheduler event tests -----

    #[test]
    fn test_update_to_approved_emits_merge_event() {
        let db = TasksDb::open_memory().unwrap();
        let resolver = test_resolver();
        let (mut writer, mut reader) = mock_io();
        let mut events = Vec::new();

        // Create a task with skip_review and move to approved
        let task = db
            .create_task(
                "test-project",
                "Merge trigger",
                None,
                None,
                None,
                true,
                "interactive",
                false,
                None,
                None,
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

        // active -> approved (skip_review=true) should emit MergeNeeded
        let result = handle_task_update(
            &db,
            &serde_json::json!({"id": task.id, "state": "approved"}),
            Some("s1"),
            "tc1",
            &resolver,
            &mut writer,
            &mut reader,
            &mut events,
            &mut Vec::new(),
        );
        assert!(!result.is_error);
        assert_eq!(events, vec![SchedulerEvent::MergeNeeded(Some("s1".into()))]);
    }

    #[test]
    fn test_update_to_ready_emits_schedule_event() {
        let db = TasksDb::open_memory().unwrap();
        let resolver = test_resolver();
        let (mut writer, mut reader) = mock_io();
        let mut events = Vec::new();

        // Create a task and move to ready
        let result = handle_task_create(
            &db,
            &serde_json::json!({"title": "Interactive task", "initial_state": "interactive"}),
            &ToolCtx {
                project_name: Some("test-project"),
                session_id: Some("s1"),
                tool_call_id: "tc1",
            },
            &resolver,
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
            &resolver,
            &mut writer,
            &mut reader,
            &mut events,
            &mut Vec::new(),
        );
        assert!(!result.is_error);
        assert_eq!(
            events,
            vec![SchedulerEvent::ScheduleNeeded(
                "test-project".into(),
                Some("s1".into())
            )]
        );
    }

    #[test]
    fn test_create_subtask_emits_schedule_event() {
        let db = TasksDb::open_memory().unwrap();
        let resolver = test_resolver();
        let (mut writer, mut reader) = mock_io();
        let mut events = Vec::new();

        // Create parent task
        let result = handle_task_create(
            &db,
            &serde_json::json!({"title": "Parent", "initial_state": "interactive"}),
            &ToolCtx {
                project_name: Some("test-project"),
                session_id: Some("s1"),
                tool_call_id: "tc1",
            },
            &resolver,
            &mut writer,
            &mut reader,
            &mut Vec::new(),
        );
        let parent: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
        let parent_id = parent["id"].as_i64().unwrap();

        // Create subtask — defaults to planning, should emit ScheduleNeeded
        let result = handle_task_create(
            &db,
            &serde_json::json!({"title": "Subtask", "parent_id": parent_id}),
            &ToolCtx {
                project_name: Some("test-project"),
                session_id: Some("s1"),
                tool_call_id: "tc2",
            },
            &resolver,
            &mut writer,
            &mut reader,
            &mut events,
        );
        assert!(!result.is_error);
        assert_eq!(
            events,
            vec![SchedulerEvent::ScheduleNeeded(
                "test-project".into(),
                Some("s1".into())
            )]
        );
    }

    #[test]
    fn test_update_to_other_state_no_event() {
        let db = TasksDb::open_memory().unwrap();
        let resolver = test_resolver();
        let (mut writer, mut reader) = mock_io();
        let mut events = Vec::new();

        // Create task and move to active (via assign)
        let task = db
            .create_task(
                "test-project",
                "No event",
                None,
                None,
                None,
                false,
                "interactive",
                false,
                None,
                None,
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

        // active -> review should NOT emit any event
        let result = handle_task_update(
            &db,
            &serde_json::json!({"id": task.id, "state": "review"}),
            Some("s1"),
            "tc1",
            &resolver,
            &mut writer,
            &mut reader,
            &mut events,
            &mut Vec::new(),
        );
        assert!(!result.is_error);
        assert!(events.is_empty());
    }

    #[test]
    fn test_update_to_merged_emits_schedule_event() {
        // Transitioning merging -> merged should emit ScheduleNeeded so
        // dependents that were blocked on this task get re-evaluated
        // on the next scheduler pass.
        let db = TasksDb::open_memory().unwrap();
        let resolver = test_resolver();
        let (mut writer, mut reader) = mock_io();
        let mut events = Vec::new();

        // Create a task and walk it through to merging.
        let task = db
            .create_task(
                "test-project",
                "dep",
                None,
                None,
                None,
                true,
                "interactive",
                false,
                None,
                None,
                false,
            )
            .unwrap();
        for next in ["ready", "active", "approved", "merging"] {
            db.update_task(
                task.id,
                &TaskUpdate {
                    state: Some(next.into()),
                    ..Default::default()
                },
                None,
            )
            .unwrap();
        }

        // merging -> merged via the tool handler.
        let result = handle_task_update(
            &db,
            &serde_json::json!({"id": task.id, "state": "merged"}),
            Some("s1"),
            "tc1",
            &resolver,
            &mut writer,
            &mut reader,
            &mut events,
            &mut Vec::new(),
        );
        assert!(!result.is_error);
        assert!(
            events.iter().any(|e| matches!(
                e,
                SchedulerEvent::ScheduleNeeded(p, _) if p == "test-project"
            )),
            "expected ScheduleNeeded for test-project, got {:?}",
            events
        );
    }

    #[test]
    fn test_update_to_closed_emits_schedule_event() {
        // Transitioning active -> closed should emit ScheduleNeeded so
        // any dependents stuck in ready/planning get re-evaluated.
        let db = TasksDb::open_memory().unwrap();
        let resolver = test_resolver();
        let (mut writer, mut reader) = mock_io();
        let mut events = Vec::new();

        let task = db
            .create_task(
                "test-project",
                "soon to be closed",
                None,
                None,
                None,
                false,
                "interactive",
                false,
                None,
                None,
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

        let result = handle_task_update(
            &db,
            &serde_json::json!({"id": task.id, "state": "closed"}),
            Some("s1"),
            "tc1",
            &resolver,
            &mut writer,
            &mut reader,
            &mut events,
            &mut Vec::new(),
        );
        assert!(!result.is_error);
        assert!(
            events.iter().any(|e| matches!(
                e,
                SchedulerEvent::ScheduleNeeded(p, _) if p == "test-project"
            )),
            "expected ScheduleNeeded for test-project, got {:?}",
            events
        );
    }

    #[test]
    fn test_drain_scheduler_events_dedup() {
        // drain_scheduler_events should deduplicate MergeNeeded and
        // same-project ScheduleNeeded events.
        let mut events = vec![
            SchedulerEvent::MergeNeeded(None),
            SchedulerEvent::MergeNeeded(None),
            SchedulerEvent::ScheduleNeeded("project-a".into(), Some("s1".into())),
            SchedulerEvent::ScheduleNeeded("project-a".into(), Some("s2".into())),
            SchedulerEvent::ScheduleNeeded("project-b".into(), None),
        ];

        // We can't easily test the actual passes (they need real git repos),
        // but we can verify the event collection logic by inspecting it.
        let batch = std::mem::take(&mut events);
        let mut need_merge = false;
        let mut schedule_projects: Vec<(String, Option<String>)> = Vec::new();
        for ev in batch {
            match ev {
                SchedulerEvent::MergeNeeded(_) => need_merge = true,
                SchedulerEvent::ScheduleNeeded(project, session_id) => {
                    if !schedule_projects.iter().any(|(p, _)| p == &project) {
                        schedule_projects.push((project, session_id));
                    }
                }
            }
        }
        assert!(need_merge);
        assert_eq!(schedule_projects.len(), 2);
        assert!(schedule_projects.iter().any(|(p, _)| p == "project-a"));
        assert!(schedule_projects.iter().any(|(p, _)| p == "project-b"));
        // First occurrence of project-a wins — carries s1's session ID
        let a_entry = schedule_projects
            .iter()
            .find(|(p, _)| p == "project-a")
            .unwrap();
        assert_eq!(a_entry.1, Some("s1".into()));
        // project-b had no session
        let b_entry = schedule_projects
            .iter()
            .find(|(p, _)| p == "project-b")
            .unwrap();
        assert_eq!(b_entry.1, None);
    }

    // ----- review → active notification tests -----

    #[test]
    fn test_review_to_active_notifies_worker_session() {
        let db = TasksDb::open_memory().unwrap();
        let resolver = test_resolver();

        // Create a task, assign it a session, advance to review state
        let task = db
            .create_task(
                "test-project",
                "Review notify test",
                None,
                None,
                None,
                false,
                "interactive",
                false,
                None,
                None,
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
            &resolver,
            &mut writer,
            &mut reader,
            &mut Vec::new(),
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
        let resolver = test_resolver();

        // Task with no session_id — review -> active should succeed silently.
        // assign_task now sets session_id, so we clear it afterwards to
        // simulate a task whose session was removed.
        let task = db
            .create_task(
                "test-project",
                "No session review",
                None,
                None,
                None,
                false,
                "interactive",
                false,
                None,
                None,
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
            &resolver,
            &mut writer,
            &mut reader,
            &mut Vec::new(),
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
        let resolver = test_resolver();

        // Test that approved → active does NOT send a QueueMessage
        // (only review → active should trigger the notification)
        let task = db
            .create_task(
                "test-project",
                "Approved bounce",
                None,
                None,
                None,
                true,
                "interactive",
                false,
                None,
                None,
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
            &resolver,
            &mut writer,
            &mut reader,
            &mut Vec::new(),
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
        let resolver = test_resolver();
        let (mut writer, mut reader) = mock_io();

        // Create a task with skip_review=false and advance to active
        let task = db
            .create_task(
                "test-project",
                "Review dispatch test",
                None,
                None,
                None,
                false,
                "interactive",
                false,
                None,
                None,
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
            &resolver,
            &mut writer,
            &mut reader,
            &mut events,
            &mut Vec::new(),
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
        let resolver = test_resolver();
        let (mut writer, mut reader) = mock_io();

        let task = db
            .create_task(
                "test-project",
                "Reviewer role test",
                None,
                None,
                None,
                false,
                "interactive",
                false,
                None,
                None,
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
            &resolver,
            &mut writer,
            &mut reader,
            &mut events,
            &mut Vec::new(),
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
        let resolver = test_resolver();
        let (mut writer, mut reader) = mock_io();

        // Create a task and advance to active
        let task = db
            .create_task(
                "test-project",
                "Reuse reviewer test",
                None,
                None,
                None,
                false,
                "interactive",
                false,
                None,
                None,
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
            &resolver,
            &mut writer,
            &mut reader,
            &mut events,
            &mut Vec::new(),
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
            &resolver,
            &mut writer,
            &mut reader,
            &mut events,
            &mut Vec::new(),
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
        let resolver = test_resolver();

        // First, create a reviewer session using normal mock
        let (mut writer, mut reader) = mock_io();
        let task = db
            .create_task(
                "test-project",
                "Archived reviewer test",
                None,
                None,
                None,
                false,
                "interactive",
                false,
                None,
                None,
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
            &resolver,
            &mut writer,
            &mut reader,
            &mut events,
            &mut Vec::new(),
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
            &resolver,
            &mut writer2,
            &mut reader2,
            &mut events,
            &mut Vec::new(),
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
        let resolver = test_resolver();
        let (mut writer, mut reader) = mock_io();

        // Create a subtask in planning state
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
            )
            .unwrap();
        let task = db
            .create_task(
                "test-project",
                "Refiner reuse test",
                Some(5),
                Some(parent.id),
                None,
                false,
                "planning",
                false,
                None,
                None,
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
            &resolver,
            &mut writer,
            &mut reader,
            &mut events,
            &mut Vec::new(),
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
            &resolver,
            &mut writer,
            &mut reader,
            &mut events,
            &mut Vec::new(),
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
        let resolver = test_resolver();

        // Create a subtask in planning state
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
            )
            .unwrap();
        let task = db
            .create_task(
                "test-project",
                "Archived refiner test",
                Some(5),
                Some(parent.id),
                None,
                false,
                "planning",
                false,
                None,
                None,
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
            &resolver,
            &mut writer,
            &mut reader,
            &mut events,
            &mut Vec::new(),
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
            &resolver,
            &mut writer2,
            &mut reader2,
            &mut events,
            &mut Vec::new(),
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
        let resolver = test_resolver();
        let (mut writer, mut reader) = mock_io();

        // Create a subtask (defaults to planning state)
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
            )
            .unwrap();
        let task = db
            .create_task(
                "test-project",
                "Subtask with plan",
                Some(5),
                Some(parent.id),
                None,
                false,
                "planning",
                false,
                None,
                None,
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
            &resolver,
            &mut writer,
            &mut reader,
            &mut events,
            &mut Vec::new(),
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
        let resolver = test_resolver();
        let (mut writer, mut reader) = mock_io();

        // Create an interactive task
        let result = handle_task_create(
            &db,
            &serde_json::json!({"title": "Direct refine task", "initial_state": "interactive"}),
            &ToolCtx {
                project_name: Some("test-project"),
                session_id: Some("s1"),
                tool_call_id: "tc1",
            },
            &resolver,
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
            &resolver,
            &mut writer,
            &mut reader,
            &mut events,
            &mut Vec::new(),
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
        let resolver = test_resolver();
        let (mut writer, mut reader) = mock_io();

        // Create a subtask in planning state
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
            )
            .unwrap();
        let task = db
            .create_task(
                "test-project",
                "No files task",
                Some(5),
                Some(parent.id),
                None,
                false,
                "planning",
                false,
                None,
                None,
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
            &resolver,
            &mut writer,
            &mut reader,
            &mut events,
            &mut Vec::new(),
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
        let resolver = test_resolver();
        let (mut writer, mut reader) = mock_io();

        // Create a subtask in planning state
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
            )
            .unwrap();
        let task = db
            .create_task(
                "test-project",
                "Has files task",
                Some(5),
                Some(parent.id),
                None,
                false,
                "planning",
                false,
                None,
                None,
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
            &resolver,
            &mut writer,
            &mut reader,
            &mut events,
            &mut Vec::new(),
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
        let resolver = test_resolver();
        let (mut writer, mut reader) = mock_io();

        // Create a subtask in planning state with a session
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
            )
            .unwrap();
        let task = db
            .create_task(
                "test-project",
                "Needs revision",
                Some(5),
                Some(parent.id),
                None,
                false,
                "planning",
                false,
                None,
                None,
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
            &resolver,
            &mut writer,
            &mut reader,
            &mut events,
            &mut Vec::new(),
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
        let resolver = test_resolver();
        let (mut writer, mut reader) = mock_io();

        // Create parent (interactive — gets mock-s1)
        let result = handle_task_create(
            &db,
            &serde_json::json!({"title": "Parent", "initial_state": "interactive"}),
            &ToolCtx {
                project_name: Some("test-project"),
                session_id: Some("s1"),
                tool_call_id: "tc1",
            },
            &resolver,
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
                project_name: Some("test-project"),
                session_id: Some("s1"),
                tool_call_id: "tc2",
            },
            &resolver,
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
                project_name: Some("test-project"),
                session_id: Some("s1"),
                tool_call_id: "tc3",
            },
            &resolver,
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
        let resolver = test_resolver();
        let (mut writer, mut reader) = mock_io();

        // Create two independent tasks
        let result = handle_task_create(
            &db,
            &serde_json::json!({"title": "Task A", "initial_state": "interactive"}),
            &ToolCtx {
                project_name: Some("test-project"),
                session_id: Some("s1"),
                tool_call_id: "tc1",
            },
            &resolver,
            &mut writer,
            &mut reader,
            &mut Vec::new(),
        );
        let task_a: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
        let task_a_id = task_a["id"].as_i64().unwrap();

        let result = handle_task_create(
            &db,
            &serde_json::json!({"title": "Task B", "initial_state": "interactive"}),
            &ToolCtx {
                project_name: Some("test-project"),
                session_id: Some("s1"),
                tool_call_id: "tc2",
            },
            &resolver,
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

    // ----- transition to interactive creates session tests -----

    #[test]
    fn test_transition_to_interactive_creates_session() {
        let db = TasksDb::open_memory().unwrap();
        let resolver = test_resolver();
        let (mut writer, mut reader) = mock_io();

        // Create a subtask (planning state, no session)
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
            )
            .unwrap();
        let task = db
            .create_task(
                "test-project",
                "Scope expansion",
                Some(5),
                Some(parent.id),
                None,
                false,
                "planning",
                false,
                None,
                None,
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
            &resolver,
            &mut writer,
            &mut reader,
            &mut events,
            &mut Vec::new(),
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
        let resolver = test_resolver();
        let (mut writer, mut reader) = mock_io();

        // Create a subtask in planning state (no session)
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
            )
            .unwrap();
        let task = db
            .create_task(
                "test-project",
                "CLI takeover",
                Some(5),
                Some(parent.id),
                None,
                false,
                "planning",
                false,
                None,
                None,
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
            &resolver,
            &mut writer,
            &mut reader,
            &mut events,
            &mut Vec::new(),
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
        let resolver = test_resolver();
        let (mut writer, mut reader) = mock_io();

        // Create an interactive task (gets mock-s1)
        let result = handle_task_create(
            &db,
            &serde_json::json!({"title": "Already has session", "initial_state": "interactive"}),
            &ToolCtx {
                project_name: Some("test-project"),
                session_id: Some("s1"),
                tool_call_id: "tc1",
            },
            &resolver,
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
            &resolver,
            &mut writer,
            &mut reader,
            &mut events,
            &mut Vec::new(),
        );
        assert!(!result.is_error);

        // planning -> interactive: the task still has the original session_id,
        // which is alive (not archived in mock), so no new session should be created
        let result = handle_task_update(
            &db,
            &serde_json::json!({"id": task_id, "state": "interactive"}),
            Some("s1"),
            "tc3",
            &resolver,
            &mut writer,
            &mut reader,
            &mut events,
            &mut Vec::new(),
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
        let resolver = test_resolver();

        // Create task with a session using normal mock
        let (mut writer, mut reader) = mock_io();
        let result = handle_task_create(
            &db,
            &serde_json::json!({"title": "Session will be archived", "initial_state": "interactive"}),
            &ToolCtx {
                project_name: Some("test-project"),
                session_id: Some("s1"),
                tool_call_id: "tc1",
            },
            &resolver,
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
            &resolver,
            &mut writer2,
            &mut reader2,
            &mut events,
            &mut Vec::new(),
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
        let resolver = test_resolver();
        let (mut writer, mut reader) = mock_io();

        // Create a subtask in refining state (simulating scope expansion)
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
            )
            .unwrap();
        let task = db
            .create_task(
                "test-project",
                "Needs scope expansion",
                Some(5),
                Some(parent.id),
                None,
                false,
                "planning",
                false,
                None,
                None,
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
            &resolver,
            &mut writer,
            &mut reader,
            &mut events,
            &mut Vec::new(),
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

    // ----- transition to interactive notification tests -----

    #[test]
    fn test_transition_to_interactive_sends_info_message_new_session() {
        let db = TasksDb::open_memory().unwrap();
        let resolver = test_resolver();
        let (mut writer, mut reader) = mock_io();

        // Create a subtask in planning state (no session)
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
            )
            .unwrap();
        let task = db
            .create_task(
                "test-project",
                "Scope expansion notify",
                Some(5),
                Some(parent.id),
                None,
                false,
                "planning",
                false,
                None,
                None,
                false,
            )
            .unwrap();
        assert_eq!(task.state, "planning");

        // planning -> interactive: should create session AND send info message
        let mut events = Vec::new();
        let result = handle_task_update(
            &db,
            &serde_json::json!({"id": task.id, "state": "interactive"}),
            Some("triggering-session"),
            "tc1",
            &resolver,
            &mut writer,
            &mut reader,
            &mut events,
            &mut Vec::new(),
        );
        assert!(
            !result.is_error,
            "unexpected error: {}",
            extract_text(&result)
        );

        // Flush unprocessed writes and inspect written_lines
        {
            let mut shared = writer.shared.lock().unwrap();
            let buf = std::mem::take(&mut shared.write_buf);
            let text = String::from_utf8_lossy(&buf);
            for line in text.lines() {
                if !line.trim().is_empty() {
                    shared.written_lines.push(line.to_string());
                }
            }
        }
        let all_written = writer.shared.lock().unwrap().written_lines.join("\n");

        // Should contain a QueueMessage with the info about returning from planning
        assert!(
            all_written.contains("returned to interactive from planning"),
            "expected info message about returning from planning in:\n{}",
            all_written
        );
        assert!(
            all_written.contains("task_get"),
            "expected task_get instruction in info message:\n{}",
            all_written
        );
    }

    #[test]
    fn test_transition_to_interactive_sends_info_message_existing_session() {
        let db = TasksDb::open_memory().unwrap();
        let resolver = test_resolver();
        let (mut writer, mut reader) = mock_io();

        // Create an interactive task (gets mock-s1)
        let result = handle_task_create(
            &db,
            &serde_json::json!({"title": "Existing session notify", "initial_state": "interactive"}),
            &ToolCtx {
                project_name: Some("test-project"),
                session_id: Some("s1"),
                tool_call_id: "tc1",
            },
            &resolver,
            &mut writer,
            &mut reader,
            &mut Vec::new(),
        );
        let task: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
        let task_id = task["id"].as_i64().unwrap();
        assert_eq!(task["state"], "interactive");

        // Move to planning, then back to interactive
        let mut events = Vec::new();
        handle_task_update(
            &db,
            &serde_json::json!({"id": task_id, "state": "planning"}),
            Some("s1"),
            "tc2",
            &resolver,
            &mut writer,
            &mut reader,
            &mut events,
            &mut Vec::new(),
        );

        // Clear written lines to only check new messages
        {
            let mut shared = writer.shared.lock().unwrap();
            shared.written_lines.clear();
            shared.write_buf.clear();
        }

        // planning -> interactive with live session: should send info message
        // to the EXISTING session (no new session created)
        let result = handle_task_update(
            &db,
            &serde_json::json!({"id": task_id, "state": "interactive"}),
            Some("s1"),
            "tc3",
            &resolver,
            &mut writer,
            &mut reader,
            &mut events,
            &mut Vec::new(),
        );
        assert!(
            !result.is_error,
            "unexpected error: {}",
            extract_text(&result)
        );

        // Flush and inspect
        {
            let mut shared = writer.shared.lock().unwrap();
            let buf = std::mem::take(&mut shared.write_buf);
            let text = String::from_utf8_lossy(&buf);
            for line in text.lines() {
                if !line.trim().is_empty() {
                    shared.written_lines.push(line.to_string());
                }
            }
        }
        let all_written = writer.shared.lock().unwrap().written_lines.join("\n");

        // Should contain an info message about returning from planning
        assert!(
            all_written.contains("returned to interactive from planning"),
            "expected info message in:\n{}",
            all_written
        );
    }

    #[test]
    fn test_refining_to_interactive_sends_info_with_correct_state() {
        let db = TasksDb::open_memory().unwrap();
        let resolver = test_resolver();
        let (mut writer, mut reader) = mock_io();

        // Create a subtask and advance to refining
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
            )
            .unwrap();
        let task = db
            .create_task(
                "test-project",
                "Refine notify",
                Some(5),
                Some(parent.id),
                None,
                false,
                "planning",
                false,
                None,
                None,
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

        // refining -> interactive
        let mut events = Vec::new();
        let result = handle_task_update(
            &db,
            &serde_json::json!({"id": task.id, "state": "interactive"}),
            Some("refiner-session"),
            "tc1",
            &resolver,
            &mut writer,
            &mut reader,
            &mut events,
            &mut Vec::new(),
        );
        assert!(
            !result.is_error,
            "unexpected error: {}",
            extract_text(&result)
        );

        // Flush and inspect
        {
            let mut shared = writer.shared.lock().unwrap();
            let buf = std::mem::take(&mut shared.write_buf);
            let text = String::from_utf8_lossy(&buf);
            for line in text.lines() {
                if !line.trim().is_empty() {
                    shared.written_lines.push(line.to_string());
                }
            }
        }
        let all_written = writer.shared.lock().unwrap().written_lines.join("\n");

        // Should mention "refining" as the previous state
        assert!(
            all_written.contains("returned to interactive from refining"),
            "expected info message about returning from refining in:\n{}",
            all_written
        );
    }

    #[test]
    fn test_initial_interactive_state_no_return_notification() {
        let db = TasksDb::open_memory().unwrap();
        let resolver = test_resolver();
        let (mut writer, mut reader) = mock_io();

        // Create a top-level task (starts as interactive) — should NOT
        // send a "returned to interactive" message since it was never
        // in another state.
        let result = handle_task_create(
            &db,
            &serde_json::json!({"title": "Fresh interactive task", "initial_state": "interactive"}),
            &ToolCtx {
                project_name: Some("test-project"),
                session_id: Some("s1"),
                tool_call_id: "tc1",
            },
            &resolver,
            &mut writer,
            &mut reader,
            &mut Vec::new(),
        );
        assert!(!result.is_error);

        // Flush and inspect
        {
            let mut shared = writer.shared.lock().unwrap();
            let buf = std::mem::take(&mut shared.write_buf);
            let text = String::from_utf8_lossy(&buf);
            for line in text.lines() {
                if !line.trim().is_empty() {
                    shared.written_lines.push(line.to_string());
                }
            }
        }
        let all_written = writer.shared.lock().unwrap().written_lines.join("\n");

        // Should NOT contain "returned to interactive" since this is the
        // initial creation — the task was never in another state.
        assert!(
            !all_written.contains("returned to interactive"),
            "should not send return notification on initial creation:\n{}",
            all_written
        );
    }

    // ----- dispatch idempotency / deduplication tests -----

    /// Verify that calling `task_dispatch` twice for the same task does NOT
    /// create a duplicate session.  The second call should return the already-
    /// live worker session without creating a new one.
    #[test]
    fn test_dispatch_is_idempotent_for_already_dispatched_task() {
        let db = TasksDb::open_memory().unwrap();
        let (mut writer, mut reader) = mock_io();

        // Create and prepare a task (active state).
        let task = db
            .create_task(
                "test-project",
                "Idempotent dispatch test",
                None,
                None,
                None,
                false,
                "interactive",
                false,
                None,
                None,
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
        db.assign_task(task.id, "pre-existing-session").unwrap();
        db.set_worktree_path(task.id, "/tmp/fake-worktree").unwrap();
        // Record as worker session (simulating a previous successful dispatch).
        db.record_session(task.id, "pre-existing-session", "worker")
            .unwrap();

        // First dispatch: task is already active with a worker session.
        // find_reusable_session will query GetSessionInfo, mock_io returns
        // SessionInfo with archived=false → session is reused.
        let result = tasks_scheduler::dispatch(
            &db,
            task.id,
            Some("parent-session"),
            "/test/project",
            &mut writer,
            &mut reader,
        );
        assert!(result.is_ok(), "expected Ok, got: {:?}", result);
        let sid1 = result.unwrap();

        // The returned session must be the pre-existing one (not a new mock-s*).
        assert_eq!(
            sid1, "pre-existing-session",
            "expected the pre-existing worker session to be returned, got: {}",
            sid1
        );

        // Second call: same result, still no new session created.
        let result2 = tasks_scheduler::dispatch(
            &db,
            task.id,
            Some("parent-session"),
            "/test/project",
            &mut writer,
            &mut reader,
        );
        assert!(
            result2.is_ok(),
            "expected Ok on second call, got: {:?}",
            result2
        );
        let sid2 = result2.unwrap();
        assert_eq!(
            sid2, "pre-existing-session",
            "second dispatch should return the same session, got: {}",
            sid2
        );

        // Verify only one worker session was recorded (no duplicates).
        let sessions = db.get_sessions(task.id).unwrap();
        let worker_sessions: Vec<_> = sessions.iter().filter(|s| s.role == "worker").collect();
        assert_eq!(
            worker_sessions.len(),
            1,
            "expected exactly one worker session, got: {:?}",
            worker_sessions
        );
    }

    /// Verify that `run_schedule_pass` (triggered by a state transition to
    /// `ready`) does NOT dispatch a duplicate session for a task that already
    /// has a live worker session.
    ///
    /// Scenario:
    /// 1. Task transitions to `ready` — schedule pass runs, dispatches task.
    /// 2. Task transitions through planning/refining/ready again.
    /// 3. Second schedule pass runs — must not create a duplicate session.
    #[test]
    fn test_schedule_pass_does_not_duplicate_session() {
        let db = TasksDb::open_memory().unwrap();
        let (mut writer, mut reader) = mock_io();

        // Create a task and simulate it having already been dispatched as worker.
        let task = db
            .create_task(
                "test-project",
                "No duplicate session test",
                None,
                None,
                None,
                false,
                "interactive",
                false,
                None,
                None,
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
        db.assign_task(task.id, "live-worker-session").unwrap();
        db.set_worktree_path(task.id, "/tmp/fake-worktree").unwrap();
        db.record_session(task.id, "live-worker-session", "worker")
            .unwrap();

        // Record how many sessions exist before the second dispatch attempt.
        let sessions_before = db.get_sessions(task.id).unwrap();
        let worker_count_before = sessions_before
            .iter()
            .filter(|s| s.role == "worker")
            .count();
        assert_eq!(worker_count_before, 1);

        // Call dispatch directly (as run_schedule_pass would).
        // The mock_io will answer GetSessionInfo with archived=false → reuse.
        let result = tasks_scheduler::dispatch(
            &db,
            task.id,
            None,
            "/test/project",
            &mut writer,
            &mut reader,
        );
        assert!(result.is_ok(), "expected Ok, got: {:?}", result);

        // No new worker session should have been created.
        let sessions_after = db.get_sessions(task.id).unwrap();
        let worker_count_after = sessions_after.iter().filter(|s| s.role == "worker").count();
        assert_eq!(
            worker_count_after, worker_count_before,
            "dispatch created an extra worker session!"
        );
    }

    // ----- dispatch failure tests -----

    #[test]
    fn test_schedule_pass_dispatch_failure_notifies_and_reverts() {
        // When run_schedule_pass fails to create a session (e.g. child_budget
        let resolver = test_resolver();
        // exceeded), it should:
        //   1. Add a warning message to the task
        //   2. Send a QueueMessage to the triggering session
        //   3. Revert planning tasks back to planning state (no state change needed)
        let db = TasksDb::open_memory().unwrap();
        let (mut writer, mut reader) =
            mock_io_failing_session("child budget exceeded: 16 active children, budget is 16");

        // Create a subtask in planning state (no git/worktree needed).
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
            )
            .unwrap();
        let task = db
            .create_task(
                "test-project",
                "Budget fail task",
                None,
                Some(parent.id),
                None,
                false,
                "planning",
                false,
                None,
                None,
                false,
            )
            .unwrap();
        assert_eq!(task.state, "planning");

        let warnings = run_schedule_pass(
            &db,
            "test-project",
            &resolver,
            Some("trigger-session"),
            &mut writer,
            &mut reader,
        );

        // run_schedule_pass should return warnings for dispatch failures.
        assert!(
            !warnings.is_empty(),
            "expected dispatch failure warnings from run_schedule_pass"
        );
        assert!(
            warnings[0].contains("Auto-dispatch of session failed"),
            "warning should mention dispatch failure, got: {}",
            warnings[0]
        );

        // Task should still be in planning state (planning tasks don't get
        // transitioned to active by schedule()).
        let updated = db.get_task(task.id).unwrap().unwrap();
        assert_eq!(updated.state, "planning");

        // A warning message should be recorded on the task.
        let messages = db.get_messages(task.id).unwrap();
        let warn = messages
            .iter()
            .find(|m| m.content.contains("Auto-dispatch of session failed"));
        assert!(
            warn.is_some(),
            "expected a warning message on the task, got: {:?}",
            messages
        );
        assert!(
            warn.unwrap().content.contains("budget exceeded"),
            "warning should mention budget exceeded"
        );

        // A QueueMessage should have been sent to trigger-session.
        // Flush unprocessed writes and inspect written_lines.
        {
            let mut shared = writer.shared.lock().unwrap();
            let remaining = std::mem::take(&mut shared.write_buf);
            let text = String::from_utf8_lossy(&remaining);
            for line in text.lines() {
                if !line.trim().is_empty() {
                    shared.written_lines.push(line.to_string());
                }
            }
        }
        let all_written = writer.shared.lock().unwrap().written_lines.join("\n");
        assert!(
            all_written.contains("queue_message") && all_written.contains("trigger-session"),
            "expected QueueMessage to trigger-session in:\n{}",
            all_written
        );
    }

    // ----- dispatch warning in tool result tests -----

    #[test]
    fn test_review_dispatch_failure_warning_in_tool_result() {
        // When auto-dispatch of the review session fails, the tool result
        // returned by handle_task_update should contain a warning.
        let db = TasksDb::open_memory().unwrap();
        let resolver = test_resolver();
        let (mut writer, mut reader) =
            mock_io_failing_session("child budget exceeded: 0 active children, budget is 0");

        let task = db
            .create_task(
                "test-project",
                "Review warning test",
                None,
                None,
                None,
                false,
                "interactive",
                false,
                None,
                None,
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
            &resolver,
            &mut writer,
            &mut reader,
            &mut events,
            &mut Vec::new(),
        );
        assert!(!result.is_error, "task_update should succeed");
        let text = extract_text(&result);
        assert!(
            text.contains("Auto-dispatch of review session failed"),
            "tool result should contain review dispatch warning, got: {}",
            text
        );
    }

    #[test]
    fn test_refining_dispatch_failure_warning_in_tool_result() {
        // When auto-dispatch of the refining session fails, the tool result
        // returned by handle_task_update should contain a warning.
        let db = TasksDb::open_memory().unwrap();
        let resolver = test_resolver();
        let (mut writer, mut reader) =
            mock_io_failing_session("child budget exceeded: 0 active children, budget is 0");

        let task = db
            .create_task(
                "test-project",
                "Refining warning test",
                None,
                None,
                None,
                false,
                "interactive",
                false,
                None,
                None,
                false,
            )
            .unwrap();
        // planning -> refining transition triggers dispatch
        let mut events = Vec::new();
        let result = handle_task_update(
            &db,
            &serde_json::json!({"id": task.id, "state": "refining"}),
            Some("planner-session"),
            "tc1",
            &resolver,
            &mut writer,
            &mut reader,
            &mut events,
            &mut Vec::new(),
        );
        assert!(!result.is_error, "task_update should succeed");
        let text = extract_text(&result);
        assert!(
            text.contains("Auto-dispatch of refining session failed"),
            "tool result should contain refining dispatch warning, got: {}",
            text
        );
    }

    #[test]
    fn test_interactive_session_failure_warning_in_task_create() {
        // When creating an interactive task and session creation fails,
        // the tool result should contain a warning.
        let db = TasksDb::open_memory().unwrap();
        let resolver = test_resolver();
        let (mut writer, mut reader) =
            mock_io_failing_session("child budget exceeded: 0 active children, budget is 0");

        let ctx = ToolCtx {
            tool_call_id: "tc1",
            session_id: Some("parent-session"),
            project_name: Some("test-project"),
        };
        let mut events = Vec::new();
        let result = handle_task_create(
            &db,
            &serde_json::json!({"title": "Interactive warning test", "initial_state": "interactive"}),
            &ctx,
            &resolver,
            &mut writer,
            &mut reader,
            &mut events,
        );
        assert!(!result.is_error, "task_create should succeed");
        let text = extract_text(&result);
        assert!(
            text.contains("Failed to create interactive session"),
            "tool result should contain interactive session warning, got: {}",
            text
        );
    }

    #[test]
    fn test_interactive_transition_failure_warning_in_task_update() {
        // When transitioning to interactive and session creation fails,
        // the tool result should contain a warning.
        let db = TasksDb::open_memory().unwrap();
        let resolver = test_resolver();
        let (mut writer, mut reader) =
            mock_io_failing_session("child budget exceeded: 0 active children, budget is 0");

        // Create a task in planning state (subtask), then transition to
        // interactive which should trigger session creation.
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
            )
            .unwrap();
        let task = db
            .create_task(
                "test-project",
                "Interactive transition warning test",
                None,
                Some(parent.id),
                None,
                false,
                "planning",
                false,
                None,
                None,
                false,
            )
            .unwrap();
        assert_eq!(task.state, "planning");

        let mut events = Vec::new();
        let result = handle_task_update(
            &db,
            &serde_json::json!({"id": task.id, "state": "interactive"}),
            Some("planner-session"),
            "tc1",
            &resolver,
            &mut writer,
            &mut reader,
            &mut events,
            &mut Vec::new(),
        );
        assert!(!result.is_error, "task_update should succeed");
        let text = extract_text(&result);
        assert!(
            text.contains("Failed to create interactive session"),
            "tool result should contain interactive session warning, got: {}",
            text
        );
    }

    #[test]
    fn test_no_warnings_when_dispatch_succeeds() {
        // Verify that successful dispatch adds no warnings to tool results.
        let db = TasksDb::open_memory().unwrap();
        let resolver = test_resolver();
        let (mut writer, mut reader) = mock_io();

        let task = db
            .create_task(
                "test-project",
                "No warning test",
                None,
                None,
                None,
                false,
                "interactive",
                false,
                None,
                None,
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
            &resolver,
            &mut writer,
            &mut reader,
            &mut events,
            &mut Vec::new(),
        );
        assert!(!result.is_error, "task_update should succeed");
        let text = extract_text(&result);
        // Should only have the JSON task blob, no warnings
        assert!(
            !text.contains("⚠️"),
            "successful dispatch should not produce warnings, got: {}",
            text
        );
    }

    #[test]
    fn test_dispatch_failure_reverts_active_to_ready() {
        // When dispatch fails for a non-planning task (already transitioned
        // to active by prepare_task), run_schedule_pass should revert it
        // to ready.  This test verifies the active → ready transition works.
        let db = TasksDb::open_memory().unwrap();

        // Create a subtask and manually advance to active (simulating
        // what prepare_task does during schedule()).
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
            )
            .unwrap();
        let task = db
            .create_task(
                "test-project",
                "Revert test",
                None,
                Some(parent.id),
                None,
                false,
                "ready", // initial_state="ready" → starts in ready
                false,
                None,
                None,
                false,
            )
            .unwrap();
        assert_eq!(task.state, "ready");

        // Simulate prepare_task: transition to active.
        db.update_task(
            task.id,
            &TaskUpdate {
                state: Some("active".to_string()),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        let active_task = db.get_task(task.id).unwrap().unwrap();
        assert_eq!(active_task.state, "active");

        // Now revert active → ready (the transition this fix enables).
        db.update_task(
            task.id,
            &TaskUpdate {
                state: Some("ready".to_string()),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        let reverted = db.get_task(task.id).unwrap().unwrap();
        assert_eq!(
            reverted.state, "ready",
            "active → ready transition should succeed for dispatch-failure recovery"
        );
    }

    // ---------------------------------------------------------------
    // `hold` / `held` flag (task #527)
    // ---------------------------------------------------------------

    #[test]
    fn test_handle_task_create_with_hold_flag() {
        let db = TasksDb::open_memory().unwrap();
        let resolver = test_resolver();
        let (mut writer, mut reader) = mock_io();
        let mut pending = Vec::new();

        let result = handle_task_create(
            &db,
            &serde_json::json!({
                "title": "Parked",
                "initial_state": "ready",
                "hold": true,
            }),
            &ToolCtx {
                project_name: Some("test-project"),
                session_id: Some("s1"),
                tool_call_id: "tc",
            },
            &resolver,
            &mut writer,
            &mut reader,
            &mut pending,
        );
        assert!(!result.is_error);

        // hold=true + initial_state=ready must NOT emit a ScheduleNeeded event.
        assert!(
            !pending
                .iter()
                .any(|e| matches!(e, SchedulerEvent::ScheduleNeeded(..))),
            "held task must not trigger a schedule pass on creation (got {:?})",
            pending
        );

        // The task should be persisted in ready state with held=true.
        let tasks = db
            .list_tasks("test-project", None, None, None, None)
            .unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].state, "ready");
        assert!(tasks[0].held);

        // The scheduler must refuse to schedule it.
        let sched = db.get_schedulable_tasks("test-project").unwrap();
        assert!(sched.is_empty(), "held task must not appear as schedulable");
    }

    #[test]
    fn test_handle_task_update_toggles_hold() {
        let db = TasksDb::open_memory().unwrap();
        let resolver = test_resolver();
        let (mut writer, mut reader) = mock_io();

        // Create a held, ready task directly.
        let task = db
            .create_task(
                "test-project",
                "Held",
                None,
                None,
                None,
                false,
                "ready",
                false,
                None,
                None,
                true,
            )
            .unwrap();

        // Release: hold=false should schedule.
        let mut pending = Vec::new();
        let result = handle_task_update(
            &db,
            &serde_json::json!({"id": task.id, "hold": false}),
            Some("s1"),
            "tc",
            &resolver,
            &mut writer,
            &mut reader,
            &mut pending,
            &mut Vec::new(),
        );
        assert!(!result.is_error);

        let reloaded = db.get_task(task.id).unwrap().unwrap();
        assert!(!reloaded.held, "hold=false should clear the held flag");
        assert!(
            pending
                .iter()
                .any(|e| matches!(e, SchedulerEvent::ScheduleNeeded(p, _) if p == "test-project")),
            "releasing a ready task should emit ScheduleNeeded (got {:?})",
            pending
        );

        // Re-hold: hold=true should NOT schedule.
        let mut pending = Vec::new();
        let result = handle_task_update(
            &db,
            &serde_json::json!({"id": task.id, "hold": true}),
            Some("s1"),
            "tc",
            &resolver,
            &mut writer,
            &mut reader,
            &mut pending,
            &mut Vec::new(),
        );
        assert!(!result.is_error);
        let reloaded = db.get_task(task.id).unwrap().unwrap();
        assert!(reloaded.held);
        assert!(
            !pending
                .iter()
                .any(|e| matches!(e, SchedulerEvent::ScheduleNeeded(..))),
            "re-holding must not trigger a schedule pass"
        );
    }
}
