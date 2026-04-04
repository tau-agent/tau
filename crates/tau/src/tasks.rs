//! Task system plugin — global plugin for project task management.
//!
//! Speaks the plugin protocol (JSON lines over stdin/stdout).
//! Registers task management tools and handles them via TasksDb.

use std::io::{BufRead, BufReader, BufWriter, Write};

use crate::plugin::{
    HookResult, PluginMessage, PluginRegistration, PluginRequest, PluginToolDef, PluginToolResult,
};
use crate::tasks_db::{TaskUpdate, TasksDb};
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
                "Tasks start in 'interactive' state for spec refinement".into(),
                "Valid states: interactive, ready, active, review, approved, merging, done".into(),
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
            prompt_snippet: Some("Get full details of a task including messages and relations".into()),
            prompt_guidelines: vec![],
        },
        PluginToolDef {
            name: "task_list".into(),
            description: "List tasks filtered by state, parent, or tag. Default: all non-done tasks for current project.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "state": {
                        "type": "string",
                        "description": "Filter by state (interactive, ready, active, review, approved, merging, done)"
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
                "Some shortcuts: interactive->approved, active->approved (skip_review), review->active (rework)".into(),
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
            if let Some(msg_content) = message {
                let author = session_id.unwrap_or("user");
                if let Err(e) = db.add_message(task.id, msg_content, Some(author)) {
                    return tool_err(
                        tool_call_id,
                        &format!("task created (id={}) but message failed: {}", task.id, e),
                    );
                }
            }
            match serde_json::to_string_pretty(&task) {
                Ok(json) => tool_ok(tool_call_id, &json),
                Err(e) => tool_err(tool_call_id, &format!("serialize: {}", e)),
            }
        }
        Err(e) => tool_err(tool_call_id, &format!("create task: {}", e)),
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

    let result = serde_json::json!({
        "task": task,
        "messages": messages,
        "relations": relations,
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

    match db.update_task(id, &update, session_id) {
        Ok(task) => match serde_json::to_string_pretty(&task) {
            Ok(json) => tool_ok(tool_call_id, &json),
            Err(e) => tool_err(tool_call_id, &format!("serialize: {}", e)),
        },
        Err(e) => tool_err(tool_call_id, &format!("update task: {}", e)),
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

// ---------------------------------------------------------------------------
// Plugin main loop
// ---------------------------------------------------------------------------

/// Run the tasks plugin loop. Called from `tau plugin tasks` subcommand.
pub fn run_tasks_plugin() {
    // Open DB at startup (it's always at the same global path)
    let db = match TasksDb::open_default() {
        Ok(db) => db,
        Err(e) => {
            eprintln!("tasks plugin: failed to open db: {}", e);
            std::process::exit(1);
        }
    };

    let stdin = std::io::stdin();
    let mut reader = BufReader::new(stdin.lock());
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

    // Handle requests
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
                    "task_create" => {
                        handle_task_create(&db, &arguments, project, session, &tool_call_id)
                    }
                    "task_get" => handle_task_get(&db, &arguments, &tool_call_id),
                    "task_list" => handle_task_list(&db, &arguments, project, &tool_call_id),
                    "task_update" => handle_task_update(&db, &arguments, session, &tool_call_id),
                    "task_message" => handle_task_message(&db, &arguments, session, &tool_call_id),
                    "task_message_edit" => handle_task_message_edit(&db, &arguments, &tool_call_id),
                    "task_relate" => handle_task_relate(&db, &arguments, &tool_call_id),
                    "task_search" => handle_task_search(&db, &arguments, project, &tool_call_id),
                    _ => tool_err(&tool_call_id, &format!("unknown tool: {}", name)),
                };

                send_message(&mut writer, &PluginMessage::ToolResult(result));
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
                // Not expected for this plugin — ignore
            }
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
        assert_eq!(tools.len(), 8);
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"task_create"));
        assert!(names.contains(&"task_get"));
        assert!(names.contains(&"task_list"));
        assert!(names.contains(&"task_update"));
        assert!(names.contains(&"task_message"));
        assert!(names.contains(&"task_message_edit"));
        assert!(names.contains(&"task_relate"));
        assert!(names.contains(&"task_search"));
    }

    #[test]
    fn test_tool_handlers_via_db() {
        // Test handlers directly with an in-memory DB
        let db = TasksDb::open_memory().unwrap();

        // Create
        let result = handle_task_create(
            &db,
            &serde_json::json!({"title": "Test task", "priority": 3, "message": "Hello"}),
            "/project",
            Some("s1"),
            "tc1",
        );
        assert!(!result.is_error);
        let text = extract_text(&result);
        let task: serde_json::Value = serde_json::from_str(&text).unwrap();
        let task_id = task["id"].as_i64().unwrap();
        assert_eq!(task["title"], "Test task");
        assert_eq!(task["priority"], 3);
        assert_eq!(task["state"], "interactive");

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
        );
        assert!(!result.is_error);
        let text = extract_text(&result);
        let updated: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(updated["state"], "ready");

        // Invalid state transition
        let result = handle_task_update(
            &db,
            &serde_json::json!({"id": task_id, "state": "done"}),
            None,
            "tc5",
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
        let result = handle_task_create(&db, &serde_json::json!({}), "/p", None, "tc1");
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
        assert_eq!(parsed["tools"].as_array().unwrap().len(), 8);
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
}
