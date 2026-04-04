//! SQLite-backed task persistence for the tau task system plugin.

use std::path::PathBuf;

use rusqlite::{Connection, OptionalExtension, params};

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Task {
    pub id: i64,
    pub project: String,
    pub title: String,
    pub state: String,
    pub priority: i64,
    pub parent_id: Option<i64>,
    pub tags: Option<serde_json::Value>,
    pub affected_files: Option<serde_json::Value>,
    pub assigned_session: Option<String>,
    pub branch: Option<String>,
    pub worktree_path: Option<String>,
    pub session_id: Option<String>,
    pub skip_review: bool,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TaskMessage {
    pub id: i64,
    pub task_id: i64,
    pub content: String,
    pub author: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TaskRelation {
    pub from_task: i64,
    pub to_task: i64,
    pub relation: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TaskSession {
    pub task_id: i64,
    pub session_id: String,
    pub role: String,
    pub created_at: i64,
}

/// Fields that can be updated on a task.
#[derive(Debug, Clone, Default)]
pub struct TaskUpdate {
    pub title: Option<String>,
    pub state: Option<String>,
    pub priority: Option<i64>,
    pub tags: Option<serde_json::Value>,
    pub affected_files: Option<serde_json::Value>,
    pub skip_review: Option<bool>,
}

// ---------------------------------------------------------------------------
// State transition validation
// ---------------------------------------------------------------------------

const VALID_STATES: &[&str] = &[
    "interactive",
    "ready",
    "active",
    "review",
    "approved",
    "merging",
    "done",
];

/// Check whether a state transition is allowed.
pub fn validate_state_transition(from: &str, to: &str) -> bool {
    matches!(
        (from, to),
        ("interactive", "ready")
            | ("interactive", "approved")
            | ("ready", "active")
            | ("active", "review")
            | ("active", "approved")
            | ("review", "approved")
            | ("review", "active")
            | ("approved", "merging")
            | ("merging", "done")
            | ("merging", "active")
    )
}

// ---------------------------------------------------------------------------
// Database
// ---------------------------------------------------------------------------

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS tasks (
    id INTEGER PRIMARY KEY,
    project TEXT NOT NULL,
    title TEXT NOT NULL,
    state TEXT NOT NULL DEFAULT 'interactive',
    priority INTEGER DEFAULT 0,
    parent_id INTEGER REFERENCES tasks(id),
    tags TEXT,
    affected_files TEXT,
    assigned_session TEXT,
    branch TEXT,
    worktree_path TEXT,
    session_id TEXT,
    skip_review INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS task_messages (
    id INTEGER PRIMARY KEY,
    task_id INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    content TEXT NOT NULL,
    author TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS task_relations (
    from_task INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    to_task INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    relation TEXT NOT NULL,
    PRIMARY KEY (from_task, to_task, relation)
);

CREATE TABLE IF NOT EXISTS task_sessions (
    task_id INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    session_id TEXT NOT NULL,
    role TEXT NOT NULL DEFAULT 'worker',
    created_at INTEGER NOT NULL,
    PRIMARY KEY (task_id, session_id)
);

CREATE TABLE IF NOT EXISTS task_history (
    id INTEGER PRIMARY KEY,
    task_id INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    field TEXT NOT NULL,
    old_value TEXT,
    new_value TEXT,
    session_id TEXT,
    created_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_tasks_project_state ON tasks(project, state);
CREATE INDEX IF NOT EXISTS idx_tasks_parent ON tasks(parent_id);
CREATE INDEX IF NOT EXISTS idx_task_messages_task ON task_messages(task_id);
CREATE INDEX IF NOT EXISTS idx_task_history_task ON task_history(task_id);
";

pub struct TasksDb {
    conn: Connection,
}

impl TasksDb {
    /// Open (or create) the database at the default path.
    pub fn open_default() -> crate::Result<Self> {
        let path = default_db_path();
        Self::open(&path)
    }

    /// Open (or create) the database at the given path.
    pub fn open(path: &PathBuf) -> crate::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| crate::Error::Io(format!("mkdir {}: {}", parent.display(), e)))?;
        }
        let conn = Connection::open(path)
            .map_err(|e| crate::Error::Io(format!("open tasks db {}: {}", path.display(), e)))?;

        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")
            .map_err(|e| crate::Error::Io(format!("pragma: {}", e)))?;

        conn.execute_batch(SCHEMA)
            .map_err(|e| crate::Error::Io(format!("create tables: {}", e)))?;

        Ok(Self { conn })
    }

    /// Open an in-memory database (for tests).
    #[cfg(test)]
    pub fn open_memory() -> crate::Result<Self> {
        let conn = Connection::open_in_memory()
            .map_err(|e| crate::Error::Io(format!("open in-memory tasks db: {}", e)))?;

        conn.execute_batch("PRAGMA foreign_keys=ON;")
            .map_err(|e| crate::Error::Io(format!("pragma: {}", e)))?;

        conn.execute_batch(SCHEMA)
            .map_err(|e| crate::Error::Io(format!("create tables: {}", e)))?;

        Ok(Self { conn })
    }

    // ----- tasks -----

    /// Create a new task. Returns the created task.
    ///
    /// Default state depends on context:
    /// - Tasks with a `parent_id` (subtasks) default to `ready` with `skip_review=false`
    /// - Top-level tasks default to `interactive`
    pub fn create_task(
        &self,
        project: &str,
        title: &str,
        priority: Option<i64>,
        parent_id: Option<i64>,
        tags: Option<&serde_json::Value>,
        skip_review: bool,
    ) -> crate::Result<Task> {
        let now = crate::types::timestamp_ms() as i64;
        let priority = priority.unwrap_or(0);
        let tags_str = tags
            .map(serde_json::to_string)
            .transpose()
            .map_err(|e| crate::Error::Parse(e.to_string()))?;

        // Subtasks default to 'ready' state and skip_review=false
        let default_state = if parent_id.is_some() {
            "ready"
        } else {
            "interactive"
        };
        let skip_review = if parent_id.is_some() {
            false
        } else {
            skip_review
        };

        self.conn
            .execute(
                "INSERT INTO tasks (project, title, state, priority, parent_id, tags, skip_review, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    project,
                    title,
                    default_state,
                    priority,
                    parent_id,
                    tags_str,
                    skip_review as i32,
                    now,
                    now,
                ],
            )
            .map_err(|e| crate::Error::Io(format!("insert task: {}", e)))?;

        let id = self.conn.last_insert_rowid();
        self.get_task(id)?
            .ok_or_else(|| crate::Error::Io("task not found after insert".into()))
    }

    /// Get a task by ID.
    pub fn get_task(&self, id: i64) -> crate::Result<Option<Task>> {
        self.conn
            .query_row(
                "SELECT id, project, title, state, priority, parent_id, tags, affected_files,
                        assigned_session, branch, worktree_path, session_id, skip_review,
                        created_at, updated_at
                 FROM tasks WHERE id = ?1",
                params![id],
                row_to_task,
            )
            .optional()
            .map_err(|e| crate::Error::Io(format!("get task: {}", e)))
    }

    /// List tasks with optional filters.
    pub fn list_tasks(
        &self,
        project: &str,
        state_filter: Option<&str>,
        parent_id_filter: Option<i64>,
        tag_filter: Option<&str>,
        limit: Option<i64>,
    ) -> crate::Result<Vec<Task>> {
        let mut sql = String::from(
            "SELECT id, project, title, state, priority, parent_id, tags, affected_files,
                    assigned_session, branch, worktree_path, session_id, skip_review,
                    created_at, updated_at
             FROM tasks WHERE project = ?1",
        );
        let mut param_idx = 2;
        let mut params_vec: Vec<Box<dyn rusqlite::types::ToSql>> =
            vec![Box::new(project.to_string())];

        if let Some(state) = state_filter {
            sql.push_str(&format!(" AND state = ?{}", param_idx));
            params_vec.push(Box::new(state.to_string()));
            param_idx += 1;
        } else {
            sql.push_str(" AND state != 'done'");
        }

        if let Some(pid) = parent_id_filter {
            sql.push_str(&format!(" AND parent_id = ?{}", param_idx));
            params_vec.push(Box::new(pid));
            param_idx += 1;
        }

        if let Some(tag) = tag_filter {
            // Search within JSON array stored in tags column
            sql.push_str(&format!(
                " AND EXISTS (SELECT 1 FROM json_each(tags) WHERE value = ?{})",
                param_idx
            ));
            params_vec.push(Box::new(tag.to_string()));
            param_idx += 1;
        }

        sql.push_str(" ORDER BY priority DESC, created_at ASC");

        if let Some(lim) = limit {
            sql.push_str(&format!(" LIMIT ?{}", param_idx));
            params_vec.push(Box::new(lim));
        }

        let params_refs: Vec<&dyn rusqlite::types::ToSql> =
            params_vec.iter().map(|p| p.as_ref()).collect();
        let mut stmt = self
            .conn
            .prepare(&sql)
            .map_err(|e| crate::Error::Io(format!("prepare list tasks: {}", e)))?;

        let rows = stmt
            .query_map(params_refs.as_slice(), row_to_task)
            .map_err(|e| crate::Error::Io(format!("list tasks: {}", e)))?;

        let mut tasks = Vec::new();
        for row in rows {
            tasks.push(row.map_err(|e| crate::Error::Io(format!("read task row: {}", e)))?);
        }
        Ok(tasks)
    }

    /// Update a task. Records changes in task_history.
    /// Validates state transitions if state is being changed.
    pub fn update_task(
        &self,
        id: i64,
        update: &TaskUpdate,
        session_id: Option<&str>,
    ) -> crate::Result<Task> {
        let task = self
            .get_task(id)?
            .ok_or_else(|| crate::Error::Io(format!("task {} not found", id)))?;

        // Validate state transition
        if let Some(ref new_state) = update.state {
            if !VALID_STATES.contains(&new_state.as_str()) {
                return Err(crate::Error::Io(format!("invalid state: {}", new_state)));
            }
            if !validate_state_transition(&task.state, new_state) {
                return Err(crate::Error::Io(format!(
                    "invalid state transition: {} -> {}",
                    task.state, new_state
                )));
            }
            // active -> approved requires skip_review=true
            if task.state == "active" && new_state == "approved" && !task.skip_review {
                return Err(crate::Error::Io(
                    "cannot transition active -> approved: skip_review is false, \
                     must go through review first"
                        .into(),
                ));
            }
        }

        let now = crate::types::timestamp_ms() as i64;

        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(|e| crate::Error::Io(format!("update_task begin: {}", e)))?;

        // Build SET clauses and record history
        let mut sets = vec!["updated_at = ?".to_string()];
        let mut params_vec: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(now)];

        macro_rules! update_field {
            ($field:ident, $col:expr, $old_val:expr) => {
                if let Some(ref val) = update.$field {
                    let old_str = $old_val;
                    let new_str = format!("{}", val);
                    params_vec.push(Box::new(val.clone()));
                    sets.push(format!("{} = ?", $col));
                    tx.execute(
                        "INSERT INTO task_history (task_id, field, old_value, new_value, session_id, created_at)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                        params![id, $col, old_str, new_str, session_id, now],
                    )
                    .map_err(|e| crate::Error::Io(format!("insert history: {}", e)))?;
                }
            };
        }

        update_field!(title, "title", Some(task.title.clone()));
        update_field!(state, "state", Some(task.state.clone()));
        update_field!(priority, "priority", Some(task.priority.to_string()));

        if let Some(ref val) = update.tags {
            let old_str = task.tags.as_ref().map(|v| v.to_string());
            let new_str = val.to_string();
            let json_str =
                serde_json::to_string(val).map_err(|e| crate::Error::Parse(e.to_string()))?;
            params_vec.push(Box::new(json_str));
            sets.push("tags = ?".to_string());
            tx.execute(
                "INSERT INTO task_history (task_id, field, old_value, new_value, session_id, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![id, "tags", old_str, new_str, session_id, now],
            )
            .map_err(|e| crate::Error::Io(format!("insert history: {}", e)))?;
        }

        if let Some(ref val) = update.affected_files {
            let old_str = task.affected_files.as_ref().map(|v| v.to_string());
            let new_str = val.to_string();
            let json_str =
                serde_json::to_string(val).map_err(|e| crate::Error::Parse(e.to_string()))?;
            params_vec.push(Box::new(json_str));
            sets.push("affected_files = ?".to_string());
            tx.execute(
                "INSERT INTO task_history (task_id, field, old_value, new_value, session_id, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![id, "affected_files", old_str, new_str, session_id, now],
            )
            .map_err(|e| crate::Error::Io(format!("insert history: {}", e)))?;
        }

        if let Some(val) = update.skip_review {
            let old_str = Some(task.skip_review.to_string());
            let new_str = val.to_string();
            params_vec.push(Box::new(val as i32));
            sets.push("skip_review = ?".to_string());
            tx.execute(
                "INSERT INTO task_history (task_id, field, old_value, new_value, session_id, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![id, "skip_review", old_str, new_str, session_id, now],
            )
            .map_err(|e| crate::Error::Io(format!("insert history: {}", e)))?;
        }

        if sets.len() == 1 {
            // Only updated_at, nothing else to update
            tx.commit()
                .map_err(|e| crate::Error::Io(format!("update_task commit: {}", e)))?;
            return self
                .get_task(id)?
                .ok_or_else(|| crate::Error::Io("task not found after update".into()));
        }

        // Build positional param placeholders
        let set_clause: String = sets
            .iter()
            .enumerate()
            .map(|(i, s)| s.replacen('?', &format!("?{}", i + 1), 1))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "UPDATE tasks SET {} WHERE id = ?{}",
            set_clause,
            params_vec.len() + 1
        );
        params_vec.push(Box::new(id));
        let params_refs: Vec<&dyn rusqlite::types::ToSql> =
            params_vec.iter().map(|p| p.as_ref()).collect();

        tx.execute(&sql, params_refs.as_slice())
            .map_err(|e| crate::Error::Io(format!("update task: {}", e)))?;

        tx.commit()
            .map_err(|e| crate::Error::Io(format!("update_task commit: {}", e)))?;

        self.get_task(id)?
            .ok_or_else(|| crate::Error::Io("task not found after update".into()))
    }

    // ----- messages -----

    /// Add a message to a task.
    pub fn add_message(
        &self,
        task_id: i64,
        content: &str,
        author: Option<&str>,
    ) -> crate::Result<TaskMessage> {
        // Verify task exists
        self.get_task(task_id)?
            .ok_or_else(|| crate::Error::Io(format!("task {} not found", task_id)))?;

        let now = crate::types::timestamp_ms() as i64;
        self.conn
            .execute(
                "INSERT INTO task_messages (task_id, content, author, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![task_id, content, author, now, now],
            )
            .map_err(|e| crate::Error::Io(format!("insert message: {}", e)))?;

        let id = self.conn.last_insert_rowid();
        Ok(TaskMessage {
            id,
            task_id,
            content: content.to_string(),
            author: author.map(String::from),
            created_at: now,
            updated_at: now,
        })
    }

    /// Edit a message's content.
    pub fn edit_message(&self, message_id: i64, content: &str) -> crate::Result<TaskMessage> {
        let now = crate::types::timestamp_ms() as i64;

        let updated = self
            .conn
            .execute(
                "UPDATE task_messages SET content = ?1, updated_at = ?2 WHERE id = ?3",
                params![content, now, message_id],
            )
            .map_err(|e| crate::Error::Io(format!("edit message: {}", e)))?;

        if updated == 0 {
            return Err(crate::Error::Io(format!(
                "message {} not found",
                message_id
            )));
        }

        self.conn
            .query_row(
                "SELECT id, task_id, content, author, created_at, updated_at
                 FROM task_messages WHERE id = ?1",
                params![message_id],
                row_to_message,
            )
            .map_err(|e| crate::Error::Io(format!("get edited message: {}", e)))
    }

    /// Get all messages for a task.
    pub fn get_messages(&self, task_id: i64) -> crate::Result<Vec<TaskMessage>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, task_id, content, author, created_at, updated_at
                 FROM task_messages WHERE task_id = ?1 ORDER BY id",
            )
            .map_err(|e| crate::Error::Io(format!("prepare get messages: {}", e)))?;

        let rows = stmt
            .query_map(params![task_id], row_to_message)
            .map_err(|e| crate::Error::Io(format!("get messages: {}", e)))?;

        let mut messages = Vec::new();
        for row in rows {
            messages.push(row.map_err(|e| crate::Error::Io(format!("read message row: {}", e)))?);
        }
        Ok(messages)
    }

    // ----- relations -----

    /// Add a relation between two tasks. Validates both exist.
    pub fn add_relation(&self, from_task: i64, to_task: i64, relation: &str) -> crate::Result<()> {
        // Validate relation type
        if !matches!(relation, "depends_on" | "blocks" | "related") {
            return Err(crate::Error::Io(format!(
                "invalid relation type: {}. Must be depends_on, blocks, or related",
                relation
            )));
        }

        // Validate both tasks exist
        self.get_task(from_task)?
            .ok_or_else(|| crate::Error::Io(format!("from_task {} not found", from_task)))?;
        self.get_task(to_task)?
            .ok_or_else(|| crate::Error::Io(format!("to_task {} not found", to_task)))?;

        self.conn
            .execute(
                "INSERT OR IGNORE INTO task_relations (from_task, to_task, relation)
                 VALUES (?1, ?2, ?3)",
                params![from_task, to_task, relation],
            )
            .map_err(|e| crate::Error::Io(format!("insert relation: {}", e)))?;

        Ok(())
    }

    /// Get all relations involving a task (from or to).
    pub fn get_relations(&self, task_id: i64) -> crate::Result<Vec<TaskRelation>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT from_task, to_task, relation FROM task_relations
                 WHERE from_task = ?1 OR to_task = ?1",
            )
            .map_err(|e| crate::Error::Io(format!("prepare get relations: {}", e)))?;

        let rows = stmt
            .query_map(params![task_id], |row| {
                Ok(TaskRelation {
                    from_task: row.get(0)?,
                    to_task: row.get(1)?,
                    relation: row.get(2)?,
                })
            })
            .map_err(|e| crate::Error::Io(format!("get relations: {}", e)))?;

        let mut relations = Vec::new();
        for row in rows {
            relations.push(row.map_err(|e| crate::Error::Io(format!("read relation row: {}", e)))?);
        }
        Ok(relations)
    }

    // ----- subtasks -----

    /// Get direct subtasks of a task.
    pub fn get_subtasks(&self, parent_id: i64) -> crate::Result<Vec<Task>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, project, title, state, priority, parent_id, tags, affected_files,
                        assigned_session, branch, worktree_path, session_id, skip_review,
                        created_at, updated_at
                 FROM tasks WHERE parent_id = ?1 ORDER BY priority DESC, created_at ASC",
            )
            .map_err(|e| crate::Error::Io(format!("prepare get subtasks: {}", e)))?;

        let rows = stmt
            .query_map(params![parent_id], row_to_task)
            .map_err(|e| crate::Error::Io(format!("get subtasks: {}", e)))?;

        let mut tasks = Vec::new();
        for row in rows {
            tasks.push(row.map_err(|e| crate::Error::Io(format!("read subtask row: {}", e)))?);
        }
        Ok(tasks)
    }

    // ----- assign -----

    /// Assign a task to a session. Validates task is in `ready` state,
    /// transitions to `active`, sets `assigned_session`, records in
    /// `task_sessions` and `task_history`.
    pub fn assign_task(&self, task_id: i64, session_id: &str) -> crate::Result<Task> {
        let task = self
            .get_task(task_id)?
            .ok_or_else(|| crate::Error::Io(format!("task {} not found", task_id)))?;

        if task.state != "ready" {
            return Err(crate::Error::Io(format!(
                "cannot assign task {}: state is '{}', must be 'ready'",
                task_id, task.state
            )));
        }

        let now = crate::types::timestamp_ms() as i64;

        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(|e| crate::Error::Io(format!("assign_task begin: {}", e)))?;

        tx.execute(
            "UPDATE tasks SET state = 'active', assigned_session = ?1, updated_at = ?2 \
             WHERE id = ?3",
            params![session_id, now, task_id],
        )
        .map_err(|e| crate::Error::Io(format!("assign task update: {}", e)))?;

        // Record state change in history
        tx.execute(
            "INSERT INTO task_history (task_id, field, old_value, new_value, session_id, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![task_id, "state", task.state, "active", session_id, now],
        )
        .map_err(|e| crate::Error::Io(format!("assign task history (state): {}", e)))?;

        // Record assigned_session change in history
        tx.execute(
            "INSERT INTO task_history (task_id, field, old_value, new_value, session_id, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                task_id,
                "assigned_session",
                task.assigned_session,
                session_id,
                session_id,
                now
            ],
        )
        .map_err(|e| crate::Error::Io(format!("assign task history (assigned): {}", e)))?;

        // Record in task_sessions
        tx.execute(
            "INSERT OR IGNORE INTO task_sessions (task_id, session_id, role, created_at) \
             VALUES (?1, ?2, 'worker', ?3)",
            params![task_id, session_id, now],
        )
        .map_err(|e| crate::Error::Io(format!("assign task session: {}", e)))?;

        tx.commit()
            .map_err(|e| crate::Error::Io(format!("assign_task commit: {}", e)))?;

        self.get_task(task_id)?
            .ok_or_else(|| crate::Error::Io("task not found after assign".into()))
    }

    // ----- session tracking -----

    /// Record a session's association with a task (idempotent — INSERT OR IGNORE).
    pub fn record_session(&self, task_id: i64, session_id: &str, role: &str) -> crate::Result<()> {
        let now = crate::types::timestamp_ms() as i64;
        self.conn
            .execute(
                "INSERT OR IGNORE INTO task_sessions (task_id, session_id, role, created_at) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![task_id, session_id, role, now],
            )
            .map_err(|e| crate::Error::Io(format!("record session: {}", e)))?;
        Ok(())
    }

    /// Get all sessions for a task.
    pub fn get_sessions(&self, task_id: i64) -> crate::Result<Vec<TaskSession>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT task_id, session_id, role, created_at \
                 FROM task_sessions WHERE task_id = ?1 ORDER BY created_at",
            )
            .map_err(|e| crate::Error::Io(format!("prepare get sessions: {}", e)))?;

        let rows = stmt
            .query_map(params![task_id], |row| {
                Ok(TaskSession {
                    task_id: row.get(0)?,
                    session_id: row.get(1)?,
                    role: row.get(2)?,
                    created_at: row.get(3)?,
                })
            })
            .map_err(|e| crate::Error::Io(format!("get sessions: {}", e)))?;

        let mut sessions = Vec::new();
        for row in rows {
            sessions.push(row.map_err(|e| crate::Error::Io(format!("read session row: {}", e)))?);
        }
        Ok(sessions)
    }

    // ----- search -----

    /// Search tasks by title and message content.
    pub fn search_tasks(
        &self,
        project: &str,
        query: &str,
        state_filter: Option<&str>,
    ) -> crate::Result<Vec<Task>> {
        let like_query = format!("%{}%", query);
        let mut tasks = Vec::new();

        if let Some(state) = state_filter {
            let sql = "SELECT DISTINCT t.id, t.project, t.title, t.state, t.priority, t.parent_id,
                    t.tags, t.affected_files, t.assigned_session, t.branch,
                    t.worktree_path, t.session_id, t.skip_review, t.created_at, t.updated_at
             FROM tasks t
             LEFT JOIN task_messages m ON m.task_id = t.id
             WHERE t.project = ?1 AND t.state = ?2
               AND (t.title LIKE ?3 OR m.content LIKE ?3)
             ORDER BY t.priority DESC, t.created_at ASC";
            let mut stmt = self
                .conn
                .prepare(sql)
                .map_err(|e| crate::Error::Io(format!("prepare search: {}", e)))?;
            let rows = stmt
                .query_map(params![project, state, like_query], row_to_task)
                .map_err(|e| crate::Error::Io(format!("search tasks: {}", e)))?;
            for row in rows {
                tasks.push(row.map_err(|e| crate::Error::Io(format!("read search row: {}", e)))?);
            }
        } else {
            let sql = "SELECT DISTINCT t.id, t.project, t.title, t.state, t.priority, t.parent_id,
                    t.tags, t.affected_files, t.assigned_session, t.branch,
                    t.worktree_path, t.session_id, t.skip_review, t.created_at, t.updated_at
             FROM tasks t
             LEFT JOIN task_messages m ON m.task_id = t.id
             WHERE t.project = ?1
               AND (t.title LIKE ?2 OR m.content LIKE ?2)
             ORDER BY t.priority DESC, t.created_at ASC";
            let mut stmt = self
                .conn
                .prepare(sql)
                .map_err(|e| crate::Error::Io(format!("prepare search: {}", e)))?;
            let rows = stmt
                .query_map(params![project, like_query], row_to_task)
                .map_err(|e| crate::Error::Io(format!("search tasks: {}", e)))?;
            for row in rows {
                tasks.push(row.map_err(|e| crate::Error::Io(format!("read search row: {}", e)))?);
            }
        }

        Ok(tasks)
    }

    // ----- git integration -----

    /// Set the branch name for a task.
    pub fn set_branch(&self, task_id: i64, branch: &str) -> crate::Result<()> {
        let now = crate::types::timestamp_ms() as i64;
        let updated = self
            .conn
            .execute(
                "UPDATE tasks SET branch = ?1, updated_at = ?2 WHERE id = ?3",
                params![branch, now, task_id],
            )
            .map_err(|e| crate::Error::Io(format!("set_branch: {}", e)))?;

        if updated == 0 {
            return Err(crate::Error::Io(format!("task {} not found", task_id)));
        }
        Ok(())
    }

    /// Set the worktree path for a task.
    pub fn set_worktree_path(&self, task_id: i64, path: &str) -> crate::Result<()> {
        let now = crate::types::timestamp_ms() as i64;
        let updated = self
            .conn
            .execute(
                "UPDATE tasks SET worktree_path = ?1, updated_at = ?2 WHERE id = ?3",
                params![path, now, task_id],
            )
            .map_err(|e| crate::Error::Io(format!("set_worktree_path: {}", e)))?;

        if updated == 0 {
            return Err(crate::Error::Io(format!("task {} not found", task_id)));
        }
        Ok(())
    }

    /// Get the merge target branch for a task.
    ///
    /// Returns the parent task's branch name if the task has a parent,
    /// or `"main"` if it is a root task. Errors if the parent exists but
    /// has no branch set.
    pub fn get_merge_target(&self, task_id: i64) -> crate::Result<String> {
        let task = self
            .get_task(task_id)?
            .ok_or_else(|| crate::Error::Io(format!("task {} not found", task_id)))?;

        match task.parent_id {
            None => Ok("main".to_string()),
            Some(pid) => {
                let parent = self
                    .get_task(pid)?
                    .ok_or_else(|| crate::Error::Io(format!("parent task {} not found", pid)))?;
                parent.branch.ok_or_else(|| {
                    crate::Error::Io(format!("parent task {} has no branch set", pid))
                })
            }
        }
    }

    /// Clear the worktree path for a task (set to NULL).
    pub fn clear_worktree(&self, task_id: i64) -> crate::Result<()> {
        let now = crate::types::timestamp_ms() as i64;
        let updated = self
            .conn
            .execute(
                "UPDATE tasks SET worktree_path = NULL, updated_at = ?1 WHERE id = ?2",
                params![now, task_id],
            )
            .map_err(|e| crate::Error::Io(format!("clear_worktree: {}", e)))?;

        if updated == 0 {
            return Err(crate::Error::Io(format!("task {} not found", task_id)));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Row mapping helpers
// ---------------------------------------------------------------------------

fn row_to_task(row: &rusqlite::Row<'_>) -> rusqlite::Result<Task> {
    let tags_str: Option<String> = row.get(6)?;
    let tags = tags_str.and_then(|s| serde_json::from_str(&s).ok());

    let affected_files_str: Option<String> = row.get(7)?;
    let affected_files = affected_files_str.and_then(|s| serde_json::from_str(&s).ok());

    Ok(Task {
        id: row.get(0)?,
        project: row.get(1)?,
        title: row.get(2)?,
        state: row.get(3)?,
        priority: row.get(4)?,
        parent_id: row.get(5)?,
        tags,
        affected_files,
        assigned_session: row.get(8)?,
        branch: row.get(9)?,
        worktree_path: row.get(10)?,
        session_id: row.get(11)?,
        skip_review: row.get::<_, i32>(12)? != 0,
        created_at: row.get(13)?,
        updated_at: row.get(14)?,
    })
}

fn row_to_message(row: &rusqlite::Row<'_>) -> rusqlite::Result<TaskMessage> {
    Ok(TaskMessage {
        id: row.get(0)?,
        task_id: row.get(1)?,
        content: row.get(2)?,
        author: row.get(3)?,
        created_at: row.get(4)?,
        updated_at: row.get(5)?,
    })
}

fn default_db_path() -> PathBuf {
    if let Ok(data) = std::env::var("XDG_DATA_HOME") {
        PathBuf::from(data).join("tau").join("tasks.db")
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home)
            .join(".local")
            .join("share")
            .join("tau")
            .join("tasks.db")
    } else {
        PathBuf::from("/tmp").join("tau-tasks.db")
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_and_get_task() {
        let db = TasksDb::open_memory().unwrap();
        let task = db
            .create_task(
                "/home/user/project",
                "Build feature X",
                Some(2),
                None,
                None,
                false,
            )
            .unwrap();

        assert_eq!(task.project, "/home/user/project");
        assert_eq!(task.title, "Build feature X");
        assert_eq!(task.state, "interactive");
        assert_eq!(task.priority, 2);
        assert!(task.parent_id.is_none());
        assert!(!task.skip_review);

        let loaded = db.get_task(task.id).unwrap().unwrap();
        assert_eq!(loaded.id, task.id);
        assert_eq!(loaded.title, "Build feature X");
    }

    #[test]
    fn test_create_task_with_tags() {
        let db = TasksDb::open_memory().unwrap();
        let tags = serde_json::json!(["backend", "urgent"]);
        let task = db
            .create_task("/project", "Tagged task", None, None, Some(&tags), false)
            .unwrap();

        assert_eq!(task.tags.unwrap(), serde_json::json!(["backend", "urgent"]));
    }

    #[test]
    fn test_list_tasks_filtered() {
        let db = TasksDb::open_memory().unwrap();

        let t1 = db
            .create_task("/project", "Task 1", Some(1), None, None, false)
            .unwrap();
        let _t2 = db
            .create_task("/project", "Task 2", Some(2), None, None, false)
            .unwrap();
        let _t3 = db
            .create_task("/other", "Task 3", None, None, None, false)
            .unwrap();

        // All non-done tasks for /project
        let tasks = db.list_tasks("/project", None, None, None, None).unwrap();
        assert_eq!(tasks.len(), 2);

        // Filter by state
        db.update_task(
            t1.id,
            &TaskUpdate {
                state: Some("ready".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        let tasks = db
            .list_tasks("/project", Some("ready"), None, None, None)
            .unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].title, "Task 1");

        // Filter by limit
        let tasks = db
            .list_tasks("/project", None, None, None, Some(1))
            .unwrap();
        assert_eq!(tasks.len(), 1);
    }

    #[test]
    fn test_list_tasks_by_tag() {
        let db = TasksDb::open_memory().unwrap();
        let tags = serde_json::json!(["backend", "urgent"]);
        db.create_task("/project", "Tagged", None, None, Some(&tags), false)
            .unwrap();
        db.create_task("/project", "Untagged", None, None, None, false)
            .unwrap();

        let tasks = db
            .list_tasks("/project", None, None, Some("backend"), None)
            .unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].title, "Tagged");

        let tasks = db
            .list_tasks("/project", None, None, Some("nonexistent"), None)
            .unwrap();
        assert_eq!(tasks.len(), 0);
    }

    #[test]
    fn test_update_task_records_history() {
        let db = TasksDb::open_memory().unwrap();
        let task = db
            .create_task("/project", "Original", None, None, None, false)
            .unwrap();

        let updated = db
            .update_task(
                task.id,
                &TaskUpdate {
                    title: Some("Updated title".into()),
                    priority: Some(5),
                    ..Default::default()
                },
                Some("s1"),
            )
            .unwrap();

        assert_eq!(updated.title, "Updated title");
        assert_eq!(updated.priority, 5);

        // Check history
        let mut stmt = db
            .conn
            .prepare(
                "SELECT field, old_value, new_value, session_id
                 FROM task_history WHERE task_id = ?1 ORDER BY id",
            )
            .unwrap();
        let history: Vec<(String, Option<String>, String, Option<String>)> = stmt
            .query_map(params![task.id], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

        assert_eq!(history.len(), 2);
        assert_eq!(history[0].0, "title");
        assert_eq!(history[0].1.as_deref(), Some("Original"));
        assert_eq!(history[0].2, "Updated title");
        assert_eq!(history[0].3.as_deref(), Some("s1"));
        assert_eq!(history[1].0, "priority");
    }

    #[test]
    fn test_state_transition_validation() {
        // Valid transitions
        assert!(validate_state_transition("interactive", "ready"));
        assert!(validate_state_transition("interactive", "approved"));
        assert!(validate_state_transition("ready", "active"));
        assert!(validate_state_transition("active", "review"));
        assert!(validate_state_transition("active", "approved"));
        assert!(validate_state_transition("review", "approved"));
        assert!(validate_state_transition("review", "active"));
        assert!(validate_state_transition("approved", "merging"));
        assert!(validate_state_transition("merging", "done"));
        assert!(validate_state_transition("merging", "active"));

        // Invalid transitions
        assert!(!validate_state_transition("interactive", "active"));
        assert!(!validate_state_transition("ready", "done"));
        assert!(!validate_state_transition("done", "interactive"));
        assert!(!validate_state_transition("active", "ready"));
        assert!(!validate_state_transition("review", "ready"));
    }

    #[test]
    fn test_state_transition_rejected() {
        let db = TasksDb::open_memory().unwrap();
        let task = db
            .create_task("/project", "Test", None, None, None, false)
            .unwrap();

        // interactive -> active is invalid
        let err = db
            .update_task(
                task.id,
                &TaskUpdate {
                    state: Some("active".into()),
                    ..Default::default()
                },
                None,
            )
            .unwrap_err();
        assert!(err.to_string().contains("invalid state transition"));

        // interactive -> ready is valid
        db.update_task(
            task.id,
            &TaskUpdate {
                state: Some("ready".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();
    }

    #[test]
    fn test_invalid_state_name() {
        let db = TasksDb::open_memory().unwrap();
        let task = db
            .create_task("/project", "Test", None, None, None, false)
            .unwrap();

        let err = db
            .update_task(
                task.id,
                &TaskUpdate {
                    state: Some("bogus".into()),
                    ..Default::default()
                },
                None,
            )
            .unwrap_err();
        assert!(err.to_string().contains("invalid state"));
    }

    #[test]
    fn test_messages() {
        let db = TasksDb::open_memory().unwrap();
        let task = db
            .create_task("/project", "Test", None, None, None, false)
            .unwrap();

        let msg1 = db
            .add_message(task.id, "First message", Some("user"))
            .unwrap();
        assert_eq!(msg1.content, "First message");
        assert_eq!(msg1.author.as_deref(), Some("user"));

        let _msg2 = db
            .add_message(task.id, "Second message", Some("s1"))
            .unwrap();

        let messages = db.get_messages(task.id).unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].content, "First message");
        assert_eq!(messages[1].content, "Second message");

        // Edit message
        let edited = db.edit_message(msg1.id, "Edited first").unwrap();
        assert_eq!(edited.content, "Edited first");
        assert!(edited.updated_at >= msg1.updated_at);

        let messages = db.get_messages(task.id).unwrap();
        assert_eq!(messages[0].content, "Edited first");
    }

    #[test]
    fn test_edit_nonexistent_message() {
        let db = TasksDb::open_memory().unwrap();
        let err = db.edit_message(99999, "content").unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn test_message_for_nonexistent_task() {
        let db = TasksDb::open_memory().unwrap();
        let err = db.add_message(99999, "content", None).unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn test_relations() {
        let db = TasksDb::open_memory().unwrap();
        let t1 = db
            .create_task("/project", "Task 1", None, None, None, false)
            .unwrap();
        let t2 = db
            .create_task("/project", "Task 2", None, None, None, false)
            .unwrap();

        db.add_relation(t1.id, t2.id, "depends_on").unwrap();

        let rels = db.get_relations(t1.id).unwrap();
        assert_eq!(rels.len(), 1);
        assert_eq!(rels[0].from_task, t1.id);
        assert_eq!(rels[0].to_task, t2.id);
        assert_eq!(rels[0].relation, "depends_on");

        // Also visible from the other side
        let rels = db.get_relations(t2.id).unwrap();
        assert_eq!(rels.len(), 1);
    }

    #[test]
    fn test_relation_validates_tasks() {
        let db = TasksDb::open_memory().unwrap();
        let t1 = db
            .create_task("/project", "Task 1", None, None, None, false)
            .unwrap();

        let err = db.add_relation(t1.id, 99999, "depends_on").unwrap_err();
        assert!(err.to_string().contains("not found"));

        let err = db.add_relation(99999, t1.id, "blocks").unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn test_invalid_relation_type() {
        let db = TasksDb::open_memory().unwrap();
        let t1 = db
            .create_task("/project", "Task 1", None, None, None, false)
            .unwrap();
        let t2 = db
            .create_task("/project", "Task 2", None, None, None, false)
            .unwrap();

        let err = db.add_relation(t1.id, t2.id, "invalid").unwrap_err();
        assert!(err.to_string().contains("invalid relation type"));
    }

    #[test]
    fn test_search() {
        let db = TasksDb::open_memory().unwrap();
        let t1 = db
            .create_task("/project", "Build the API", None, None, None, false)
            .unwrap();
        let _t2 = db
            .create_task("/project", "Write docs", None, None, None, false)
            .unwrap();
        let t3 = db
            .create_task("/project", "Something else", None, None, None, false)
            .unwrap();

        // Add a message mentioning API to t3
        db.add_message(t3.id, "This relates to the API layer", None)
            .unwrap();

        // Search for "API" should find t1 (title) and t3 (message)
        let results = db.search_tasks("/project", "API", None).unwrap();
        assert_eq!(results.len(), 2);
        let ids: Vec<i64> = results.iter().map(|t| t.id).collect();
        assert!(ids.contains(&t1.id));
        assert!(ids.contains(&t3.id));

        // Search in different project
        let results = db.search_tasks("/other", "API", None).unwrap();
        assert_eq!(results.len(), 0);
    }

    #[test]
    fn test_subtasks() {
        let db = TasksDb::open_memory().unwrap();
        let parent = db
            .create_task("/project", "Parent", None, None, None, false)
            .unwrap();
        let _child1 = db
            .create_task("/project", "Child 1", Some(2), Some(parent.id), None, false)
            .unwrap();
        let _child2 = db
            .create_task("/project", "Child 2", Some(1), Some(parent.id), None, false)
            .unwrap();

        let subtasks = db.get_subtasks(parent.id).unwrap();
        assert_eq!(subtasks.len(), 2);
        // Higher priority first
        assert_eq!(subtasks[0].title, "Child 1");
        assert_eq!(subtasks[1].title, "Child 2");
    }

    #[test]
    fn test_list_by_parent() {
        let db = TasksDb::open_memory().unwrap();
        let parent = db
            .create_task("/project", "Parent", None, None, None, false)
            .unwrap();
        db.create_task("/project", "Child 1", None, Some(parent.id), None, false)
            .unwrap();
        db.create_task("/project", "Child 2", None, Some(parent.id), None, false)
            .unwrap();
        db.create_task("/project", "Other", None, None, None, false)
            .unwrap();

        let tasks = db
            .list_tasks("/project", None, Some(parent.id), None, None)
            .unwrap();
        assert_eq!(tasks.len(), 2);
    }

    #[test]
    fn test_update_nonexistent_task() {
        let db = TasksDb::open_memory().unwrap();
        let err = db
            .update_task(
                99999,
                &TaskUpdate {
                    title: Some("new".into()),
                    ..Default::default()
                },
                None,
            )
            .unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn test_cascade_delete() {
        let db = TasksDb::open_memory().unwrap();
        let task = db
            .create_task("/project", "Test", None, None, None, false)
            .unwrap();
        db.add_message(task.id, "msg", None).unwrap();

        // Direct delete via SQL
        db.conn
            .execute("DELETE FROM tasks WHERE id = ?1", params![task.id])
            .unwrap();

        // Messages should be cascade-deleted
        let messages = db.get_messages(task.id).unwrap();
        assert!(messages.is_empty());
    }

    #[test]
    fn test_skip_review_roundtrip() {
        let db = TasksDb::open_memory().unwrap();
        let task = db
            .create_task("/project", "Test", None, None, None, true)
            .unwrap();
        assert!(task.skip_review);

        let updated = db
            .update_task(
                task.id,
                &TaskUpdate {
                    skip_review: Some(false),
                    ..Default::default()
                },
                None,
            )
            .unwrap();
        assert!(!updated.skip_review);
    }

    // ----- session integration tests -----

    #[test]
    fn test_assign_task() {
        let db = TasksDb::open_memory().unwrap();
        let task = db
            .create_task("/project", "Test", None, None, None, false)
            .unwrap();
        assert_eq!(task.state, "interactive");

        // Move to ready first
        db.update_task(
            task.id,
            &TaskUpdate {
                state: Some("ready".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();

        // Assign
        let assigned = db.assign_task(task.id, "session-1").unwrap();
        assert_eq!(assigned.state, "active");
        assert_eq!(assigned.assigned_session.as_deref(), Some("session-1"));

        // Check task_sessions
        let sessions = db.get_sessions(task.id).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, "session-1");
        assert_eq!(sessions[0].role, "worker");

        // Check history includes state and assigned_session changes
        let mut stmt = db
            .conn
            .prepare("SELECT field, new_value FROM task_history WHERE task_id = ?1 ORDER BY id")
            .unwrap();
        let history: Vec<(String, String)> = stmt
            .query_map(params![task.id], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert!(history.iter().any(|(f, v)| f == "state" && v == "active"));
        assert!(
            history
                .iter()
                .any(|(f, v)| f == "assigned_session" && v == "session-1")
        );
    }

    #[test]
    fn test_assign_task_wrong_state() {
        let db = TasksDb::open_memory().unwrap();
        let task = db
            .create_task("/project", "Test", None, None, None, false)
            .unwrap();

        let err = db.assign_task(task.id, "s1").unwrap_err();
        assert!(err.to_string().contains("must be 'ready'"));
    }

    #[test]
    fn test_assign_task_nonexistent() {
        let db = TasksDb::open_memory().unwrap();
        let err = db.assign_task(99999, "s1").unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn test_record_session_idempotent() {
        let db = TasksDb::open_memory().unwrap();
        let task = db
            .create_task("/project", "Test", None, None, None, false)
            .unwrap();

        db.record_session(task.id, "s1", "contributor").unwrap();
        db.record_session(task.id, "s1", "contributor").unwrap();

        let sessions = db.get_sessions(task.id).unwrap();
        assert_eq!(sessions.len(), 1);
    }

    #[test]
    fn test_get_sessions_multiple_roles() {
        let db = TasksDb::open_memory().unwrap();
        let task = db
            .create_task("/project", "Test", None, None, None, false)
            .unwrap();

        db.record_session(task.id, "s1", "worker").unwrap();
        db.record_session(task.id, "s2", "reviewer").unwrap();
        db.record_session(task.id, "s3", "contributor").unwrap();

        let sessions = db.get_sessions(task.id).unwrap();
        assert_eq!(sessions.len(), 3);
        let roles: Vec<&str> = sessions.iter().map(|s| s.role.as_str()).collect();
        assert!(roles.contains(&"worker"));
        assert!(roles.contains(&"reviewer"));
        assert!(roles.contains(&"contributor"));
    }

    #[test]
    fn test_subtask_defaults_to_ready() {
        let db = TasksDb::open_memory().unwrap();
        let parent = db
            .create_task("/project", "Parent", None, None, None, false)
            .unwrap();
        assert_eq!(parent.state, "interactive");

        // Subtask: even with skip_review=true, it gets forced to false
        let child = db
            .create_task("/project", "Child", None, Some(parent.id), None, true)
            .unwrap();
        assert_eq!(child.state, "ready");
        assert!(!child.skip_review);
    }

    #[test]
    fn test_active_to_approved_blocked_without_skip_review() {
        let db = TasksDb::open_memory().unwrap();
        let task = db
            .create_task("/project", "Test", None, None, None, false)
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

        let err = db
            .update_task(
                task.id,
                &TaskUpdate {
                    state: Some("approved".into()),
                    ..Default::default()
                },
                Some("s1"),
            )
            .unwrap_err();
        assert!(err.to_string().contains("skip_review is false"));
    }

    #[test]
    fn test_active_to_approved_allowed_with_skip_review() {
        let db = TasksDb::open_memory().unwrap();
        let task = db
            .create_task("/project", "Test", None, None, None, true)
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

        let result = db
            .update_task(
                task.id,
                &TaskUpdate {
                    state: Some("approved".into()),
                    ..Default::default()
                },
                Some("s1"),
            )
            .unwrap();
        assert_eq!(result.state, "approved");
    }

    // ----- git integration tests -----

    #[test]
    fn test_set_branch() {
        let db = TasksDb::open_memory().unwrap();
        let task = db
            .create_task("/project", "Test", None, None, None, false)
            .unwrap();
        assert!(task.branch.is_none());

        db.set_branch(task.id, "task-1").unwrap();

        let loaded = db.get_task(task.id).unwrap().unwrap();
        assert_eq!(loaded.branch.as_deref(), Some("task-1"));
    }

    #[test]
    fn test_set_branch_nonexistent_task() {
        let db = TasksDb::open_memory().unwrap();
        let err = db.set_branch(99999, "task-99999").unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn test_set_worktree_path() {
        let db = TasksDb::open_memory().unwrap();
        let task = db
            .create_task("/project", "Test", None, None, None, false)
            .unwrap();
        assert!(task.worktree_path.is_none());

        db.set_worktree_path(task.id, "/home/user/project-task-1")
            .unwrap();

        let loaded = db.get_task(task.id).unwrap().unwrap();
        assert_eq!(
            loaded.worktree_path.as_deref(),
            Some("/home/user/project-task-1")
        );
    }

    #[test]
    fn test_set_worktree_path_nonexistent_task() {
        let db = TasksDb::open_memory().unwrap();
        let err = db.set_worktree_path(99999, "/tmp/wt").unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn test_clear_worktree() {
        let db = TasksDb::open_memory().unwrap();
        let task = db
            .create_task("/project", "Test", None, None, None, false)
            .unwrap();

        db.set_worktree_path(task.id, "/home/user/project-task-1")
            .unwrap();
        let loaded = db.get_task(task.id).unwrap().unwrap();
        assert!(loaded.worktree_path.is_some());

        db.clear_worktree(task.id).unwrap();
        let loaded = db.get_task(task.id).unwrap().unwrap();
        assert!(loaded.worktree_path.is_none());
    }

    #[test]
    fn test_clear_worktree_nonexistent_task() {
        let db = TasksDb::open_memory().unwrap();
        let err = db.clear_worktree(99999).unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn test_get_merge_target_root_task() {
        let db = TasksDb::open_memory().unwrap();
        let task = db
            .create_task("/project", "Root task", None, None, None, false)
            .unwrap();

        let target = db.get_merge_target(task.id).unwrap();
        assert_eq!(target, "main");
    }

    #[test]
    fn test_get_merge_target_subtask() {
        let db = TasksDb::open_memory().unwrap();
        let parent = db
            .create_task("/project", "Parent", None, None, None, false)
            .unwrap();
        db.set_branch(parent.id, "task-1").unwrap();

        let child = db
            .create_task("/project", "Child", None, Some(parent.id), None, false)
            .unwrap();

        let target = db.get_merge_target(child.id).unwrap();
        assert_eq!(target, "task-1");
    }

    #[test]
    fn test_get_merge_target_parent_no_branch() {
        let db = TasksDb::open_memory().unwrap();
        let parent = db
            .create_task("/project", "Parent", None, None, None, false)
            .unwrap();
        // Don't set a branch on parent

        let child = db
            .create_task("/project", "Child", None, Some(parent.id), None, false)
            .unwrap();

        let err = db.get_merge_target(child.id).unwrap_err();
        assert!(err.to_string().contains("no branch set"));
    }

    #[test]
    fn test_get_merge_target_nonexistent_task() {
        let db = TasksDb::open_memory().unwrap();
        let err = db.get_merge_target(99999).unwrap_err();
        assert!(err.to_string().contains("not found"));
    }
}
