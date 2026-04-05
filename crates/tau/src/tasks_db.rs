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
    pub branch: Option<String>,
    pub worktree_path: Option<String>,
    pub session_id: Option<String>,
    pub skip_review: bool,
    pub skip_planning: bool,
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
    pub skip_planning: Option<bool>,
}

/// Result of `assign_task`, containing the updated task plus information
/// needed for session reparenting (which requires RPC calls outside the DB
/// transaction).
#[derive(Debug, Clone)]
pub struct AssignResult {
    /// The updated task after assignment.
    pub task: Task,
    /// The old session_id before reassignment (if any).
    pub old_session_id: Option<String>,
    /// Descendant task sessions that should be reparented — each entry is
    /// the old session_id of a descendant that was parented under the old
    /// owner. Only populated for interactive task reassignments.
    pub descendant_old_sessions: Vec<String>,
}

// ---------------------------------------------------------------------------
// State transition validation
// ---------------------------------------------------------------------------

const VALID_STATES: &[&str] = &[
    "interactive",
    "planning",
    "refining",
    "ready",
    "active",
    "review",
    "approved",
    "merging",
    "failed",
    "done",
];

/// Check whether a state transition is allowed.
///
/// Forward (happy path):
///   interactive -> planning -> refining -> ready -> active -> review -> approved -> merging -> done
///
/// Planning/Refining cycle:
///   interactive -> planning   (user wants autonomous planning)
///   interactive -> refining   (user already wrote spec, wants LLM review)
///   planning -> refining      (plan complete)
///   refining -> planning      (plan needs revision, resume planning session)
///   refining -> ready         (plan approved, proceed to work)
///   refining -> interactive   (scope expansion needs human sign-off)
///
/// Shortcuts:
///   interactive -> ready      (skip planning entirely)
///   interactive -> approved   (skip straight to approval)
///   active -> approved        (only when skip_review=true, enforced in update_task)
///
/// Backward (error recovery / human override):
///   review -> active          (reviewer requests changes)
///   approved -> active        (merge error, agent needs to fix)
///   approved -> ready         (unapprove, send back to queue)
///   approved -> interactive   (needs redesign / human intervention)
///   merging -> active         (merge failure, rework)
///
/// Universal overrides (admin / bootstrap):
///   any state -> done         (manual close)
///   any state -> interactive  (human takes over)
///   any state -> failed       (terminal error)
pub fn validate_state_transition(from: &str, to: &str) -> bool {
    // Universal: any state can go to done, interactive, or failed (except self-loops)
    if from != to && (to == "done" || to == "interactive" || to == "failed") {
        return true;
    }

    matches!(
        (from, to),
        // Planning/Refining transitions
        ("interactive", "planning")
            | ("interactive", "refining")
            | ("planning", "refining")
            | ("refining", "planning")
            | ("refining", "ready")
            // Forward transitions
            | ("interactive", "ready")
            | ("interactive", "approved")
            | ("ready", "active")
            | ("active", "review")
            | ("active", "approved")
            | ("review", "approved")
            | ("approved", "merging")
            | ("merging", "done")
            // Backward transitions (error recovery)
            | ("review", "active")
            | ("approved", "active")
            | ("approved", "ready")
            | ("approved", "interactive")
            | ("merging", "active")
            | ("merging", "failed")
            | ("failed", "active")
    )
}

// ---------------------------------------------------------------------------
// Tree ordering
// ---------------------------------------------------------------------------

/// Reorder tasks into a depth-first tree, returning `(depth, task)` pairs.
///
/// Top-level tasks (those with `parent_id = None` or whose parent is not in the
/// input list) appear as roots. Children appear immediately after their parent.
/// Siblings at each level are sorted by priority descending, then by id ascending.
pub fn tree_order(tasks: Vec<Task>) -> Vec<(usize, Task)> {
    use std::collections::{HashMap, HashSet};

    if tasks.is_empty() {
        return Vec::new();
    }

    // Set of all task ids present in the input.
    let ids: HashSet<i64> = tasks.iter().map(|t| t.id).collect();

    // Group children by parent_id.
    let mut children_map: HashMap<Option<i64>, Vec<Task>> = HashMap::new();
    for task in tasks {
        let key = match task.parent_id {
            Some(pid) if ids.contains(&pid) => Some(pid),
            _ => None, // treat as root
        };
        children_map.entry(key).or_default().push(task);
    }

    // Sort each group: priority desc, then id asc.
    for group in children_map.values_mut() {
        group.sort_by(|a, b| b.priority.cmp(&a.priority).then(a.id.cmp(&b.id)));
    }

    // Walk depth-first from roots.
    let mut result = Vec::new();
    let mut stack: Vec<(usize, Task)> = Vec::new();

    // Seed with roots in reverse order (so first pops out first).
    if let Some(mut roots) = children_map.remove(&None) {
        for task in roots.drain(..).rev() {
            stack.push((0, task));
        }
    }

    while let Some((depth, task)) = stack.pop() {
        let task_id = task.id;
        result.push((depth, task));

        // Push children in reverse order so they pop in sorted order.
        if let Some(mut kids) = children_map.remove(&Some(task_id)) {
            for child in kids.drain(..).rev() {
                stack.push((depth + 1, child));
            }
        }
    }

    result
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
    branch TEXT,
    worktree_path TEXT,
    session_id TEXT,
    skip_review INTEGER NOT NULL DEFAULT 0,
    skip_planning INTEGER NOT NULL DEFAULT 0,
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

        Self::migrate(&conn)?;

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

        Self::migrate(&conn)?;

        Ok(Self { conn })
    }

    /// Run schema migrations. Currently handles:
    /// - Dropping the `assigned_session` column (consolidated into `session_id`).
    /// - Adding the `skip_planning` column.
    fn migrate(conn: &Connection) -> crate::Result<()> {
        // Check if assigned_session column still exists.
        let has_assigned_session: bool = conn
            .prepare(
                "SELECT COUNT(*) FROM pragma_table_info('tasks') WHERE name = 'assigned_session'",
            )
            .and_then(|mut stmt| stmt.query_row([], |row| row.get::<_, i64>(0)))
            .map(|count| count > 0)
            .unwrap_or(false);

        if has_assigned_session {
            // Copy assigned_session values into session_id where they differ,
            // then drop the column.
            conn.execute_batch(
                "UPDATE tasks SET session_id = assigned_session \
                 WHERE assigned_session IS NOT NULL AND (session_id IS NULL OR session_id != assigned_session); \
                 ALTER TABLE tasks DROP COLUMN assigned_session;"
            )
            .map_err(|e| crate::Error::Io(format!("migrate assigned_session: {}", e)))?;
        }

        // Add skip_planning column if it doesn't exist.
        let has_skip_planning: bool = conn
            .prepare("SELECT COUNT(*) FROM pragma_table_info('tasks') WHERE name = 'skip_planning'")
            .and_then(|mut stmt| stmt.query_row([], |row| row.get::<_, i64>(0)))
            .map(|count| count > 0)
            .unwrap_or(false);

        if !has_skip_planning {
            conn.execute_batch(
                "ALTER TABLE tasks ADD COLUMN skip_planning INTEGER NOT NULL DEFAULT 0;",
            )
            .map_err(|e| crate::Error::Io(format!("migrate skip_planning: {}", e)))?;
        }

        Ok(())
    }

    // ----- tasks -----

    /// Create a new task. Returns the created task.
    ///
    /// Default state depends on context:
    /// - Tasks with a `parent_id` (subtasks) default to `planning` state
    ///   (or `ready` if `skip_planning` is true)
    /// - Top-level tasks default to `interactive`
    #[allow(clippy::too_many_arguments)]
    pub fn create_task(
        &self,
        project: &str,
        title: &str,
        priority: Option<i64>,
        parent_id: Option<i64>,
        tags: Option<&serde_json::Value>,
        skip_review: bool,
        skip_planning: bool,
    ) -> crate::Result<Task> {
        let now = crate::types::timestamp_ms() as i64;
        let priority = priority.unwrap_or(0);
        let tags_str = tags
            .map(serde_json::to_string)
            .transpose()
            .map_err(|e| crate::Error::Parse(e.to_string()))?;

        // Subtasks default to 'planning' state (or 'ready' if skip_planning).
        // skip_review is always forced to false for subtasks.
        let (default_state, skip_review, skip_planning) = if parent_id.is_some() {
            let state = if skip_planning { "ready" } else { "planning" };
            (state, false, skip_planning)
        } else {
            ("interactive", skip_review, skip_planning)
        };

        self.conn
            .execute(
                "INSERT INTO tasks (project, title, state, priority, parent_id, tags, skip_review, skip_planning, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    project,
                    title,
                    default_state,
                    priority,
                    parent_id,
                    tags_str,
                    skip_review as i32,
                    skip_planning as i32,
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
                        branch, worktree_path, session_id, skip_review, skip_planning,
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
                    branch, worktree_path, session_id, skip_review, skip_planning,
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
            // refining -> ready requires non-empty affected_files
            if task.state == "refining" && new_state == "ready" {
                let has_files = match &task.affected_files {
                    Some(serde_json::Value::Array(arr)) => !arr.is_empty(),
                    _ => false,
                };
                // Also check if the update itself sets affected_files
                let update_has_files = match &update.affected_files {
                    Some(serde_json::Value::Array(arr)) => !arr.is_empty(),
                    _ => false,
                };
                if !has_files && !update_has_files {
                    return Err(crate::Error::Io(
                        "cannot transition refining -> ready: affected_files must be \
                         set and non-empty before a task can proceed to ready"
                            .into(),
                    ));
                }
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

        if let Some(val) = update.skip_planning {
            let old_str = Some(task.skip_planning.to_string());
            let new_str = val.to_string();
            params_vec.push(Box::new(val as i32));
            sets.push("skip_planning = ?".to_string());
            tx.execute(
                "INSERT INTO task_history (task_id, field, old_value, new_value, session_id, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![id, "skip_planning", old_str, new_str, session_id, now],
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

    /// Add a relation between two tasks. Validates both exist, are in the
    /// same project, are not self-referential, and (for `depends_on`) do not
    /// create a cycle.
    pub fn add_relation(&self, from_task: i64, to_task: i64, relation: &str) -> crate::Result<()> {
        // Validate relation type
        if !matches!(relation, "depends_on" | "blocks" | "related") {
            return Err(crate::Error::Io(format!(
                "invalid relation type: {}. Must be depends_on, blocks, or related",
                relation
            )));
        }

        // Prevent self-referential relations
        if from_task == to_task {
            return Err(crate::Error::Io(
                "cannot create a relation from a task to itself".into(),
            ));
        }

        // Use IMMEDIATE transaction so the cycle check + insert are atomic.
        // This prevents a concurrent process from inserting a relation that
        // creates a cycle between our check and our insert.
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(|e| crate::Error::Io(format!("add_relation begin: {}", e)))?;

        // Validate both tasks exist
        let from = tx
            .query_row(
                "SELECT project FROM tasks WHERE id = ?1",
                params![from_task],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|e| crate::Error::Io(format!("check from_task: {}", e)))?
            .ok_or_else(|| crate::Error::Io(format!("from_task {} not found", from_task)))?;

        let to = tx
            .query_row(
                "SELECT project FROM tasks WHERE id = ?1",
                params![to_task],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|e| crate::Error::Io(format!("check to_task: {}", e)))?
            .ok_or_else(|| crate::Error::Io(format!("to_task {} not found", to_task)))?;

        // Both tasks must be in the same project
        if from != to {
            return Err(crate::Error::Io(format!(
                "cannot relate tasks across projects: '{}' and '{}'",
                from, to
            )));
        }

        // Prevent circular dependencies for depends_on.
        // BFS from to_task following depends_on edges; if we reach from_task
        // there would be a cycle.
        if relation == "depends_on" {
            use std::collections::{HashSet, VecDeque};

            let mut visited = HashSet::new();
            let mut queue = VecDeque::new();
            queue.push_back(to_task);
            visited.insert(to_task);

            while let Some(current) = queue.pop_front() {
                let mut stmt = tx
                    .prepare(
                        "SELECT to_task FROM task_relations
                         WHERE from_task = ?1 AND relation = 'depends_on'",
                    )
                    .map_err(|e| crate::Error::Io(format!("prepare cycle check: {}", e)))?;

                let deps: Vec<i64> = stmt
                    .query_map(params![current], |row| row.get(0))
                    .map_err(|e| crate::Error::Io(format!("cycle check: {}", e)))?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|e| crate::Error::Io(format!("read cycle check row: {}", e)))?;

                for dep in deps {
                    if dep == from_task {
                        return Err(crate::Error::Io(format!(
                            "circular dependency: task {} transitively depends on task {}",
                            to_task, from_task
                        )));
                    }
                    if visited.insert(dep) {
                        queue.push_back(dep);
                    }
                }
            }
        }

        tx.execute(
            "INSERT OR IGNORE INTO task_relations (from_task, to_task, relation)
             VALUES (?1, ?2, ?3)",
            params![from_task, to_task, relation],
        )
        .map_err(|e| crate::Error::Io(format!("insert relation: {}", e)))?;

        tx.commit()
            .map_err(|e| crate::Error::Io(format!("add_relation commit: {}", e)))?;

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

    /// Get tasks that this task depends on that are NOT yet done.
    /// Returns tasks where: relation(this_task, dep, 'depends_on') AND dep.state != 'done'
    pub fn get_blocking_dependencies(&self, task_id: i64) -> crate::Result<Vec<Task>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT t.id, t.project, t.title, t.state, t.priority, t.parent_id,
                        t.tags, t.affected_files, t.branch,
                        t.worktree_path, t.session_id, t.skip_review, t.skip_planning, t.created_at,
                        t.updated_at
                 FROM task_relations r
                 JOIN tasks t ON t.id = r.to_task
                 WHERE r.from_task = ?1 AND r.relation = 'depends_on' AND t.state != 'done'",
            )
            .map_err(|e| crate::Error::Io(format!("prepare get_blocking_dependencies: {}", e)))?;

        let rows = stmt
            .query_map(params![task_id], row_to_task)
            .map_err(|e| crate::Error::Io(format!("get_blocking_dependencies: {}", e)))?;

        let mut tasks = Vec::new();
        for row in rows {
            tasks.push(
                row.map_err(|e| crate::Error::Io(format!("read blocking dependency row: {}", e)))?,
            );
        }
        Ok(tasks)
    }

    /// Get tasks that are ready or in planning state AND have no unfinished
    /// dependencies.
    ///
    /// Planning-state tasks are included so the scheduler can dispatch
    /// planning sessions for them (without creating worktrees).
    pub fn get_schedulable_tasks(&self, project: &str) -> crate::Result<Vec<Task>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT t.id, t.project, t.title, t.state, t.priority, t.parent_id,
                        t.tags, t.affected_files, t.branch,
                        t.worktree_path, t.session_id, t.skip_review, t.skip_planning, t.created_at,
                        t.updated_at
                 FROM tasks t
                 WHERE t.project = ?1 AND t.state IN ('ready', 'planning')
                   AND NOT EXISTS (
                       SELECT 1 FROM task_relations r
                       JOIN tasks dep ON dep.id = r.to_task
                       WHERE r.from_task = t.id
                         AND r.relation = 'depends_on'
                         AND dep.state != 'done'
                   )
                 ORDER BY t.priority DESC, t.created_at ASC",
            )
            .map_err(|e| crate::Error::Io(format!("prepare get_schedulable_tasks: {}", e)))?;

        let rows = stmt
            .query_map(params![project], row_to_task)
            .map_err(|e| crate::Error::Io(format!("get_schedulable_tasks: {}", e)))?;

        let mut tasks = Vec::new();
        for row in rows {
            tasks.push(
                row.map_err(|e| crate::Error::Io(format!("read schedulable task row: {}", e)))?,
            );
        }
        Ok(tasks)
    }

    /// Get all tasks in `approved` state, optionally filtered by project.
    /// Used by the scheduler to find tasks ready for auto-merge.
    pub fn get_approved_tasks(&self, project: Option<&str>) -> crate::Result<Vec<Task>> {
        let (sql, params_vec): (String, Vec<Box<dyn rusqlite::types::ToSql>>) = match project {
            Some(p) => (
                "SELECT id, project, title, state, priority, parent_id, tags, affected_files,
                        branch, worktree_path, session_id, skip_review, skip_planning,
                        created_at, updated_at
                 FROM tasks
                 WHERE state = 'approved' AND project = ?1
                 ORDER BY priority DESC, created_at ASC"
                    .to_string(),
                vec![Box::new(p.to_string())],
            ),
            None => (
                "SELECT id, project, title, state, priority, parent_id, tags, affected_files,
                        branch, worktree_path, session_id, skip_review, skip_planning,
                        created_at, updated_at
                 FROM tasks
                 WHERE state = 'approved'
                 ORDER BY priority DESC, created_at ASC"
                    .to_string(),
                vec![],
            ),
        };

        let params_refs: Vec<&dyn rusqlite::types::ToSql> =
            params_vec.iter().map(|p| p.as_ref()).collect();
        let mut stmt = self
            .conn
            .prepare(&sql)
            .map_err(|e| crate::Error::Io(format!("prepare get_approved_tasks: {}", e)))?;

        let rows = stmt
            .query_map(params_refs.as_slice(), row_to_task)
            .map_err(|e| crate::Error::Io(format!("get_approved_tasks: {}", e)))?;

        let mut tasks = Vec::new();
        for row in rows {
            tasks
                .push(row.map_err(|e| crate::Error::Io(format!("read approved task row: {}", e)))?);
        }
        Ok(tasks)
    }

    /// Check if `from` transitively depends on `to` via `depends_on` relations.
    /// Uses BFS from `from` following depends_on edges. Returns true if `to` is
    /// reachable.
    pub fn has_transitive_dependency(&self, from: i64, to: i64) -> crate::Result<bool> {
        use std::collections::{HashSet, VecDeque};

        let mut visited = HashSet::new();
        let mut queue = VecDeque::new();
        queue.push_back(from);
        visited.insert(from);

        while let Some(current) = queue.pop_front() {
            let mut stmt = self
                .conn
                .prepare(
                    "SELECT to_task FROM task_relations
                     WHERE from_task = ?1 AND relation = 'depends_on'",
                )
                .map_err(|e| {
                    crate::Error::Io(format!("prepare has_transitive_dependency: {}", e))
                })?;

            let deps: Vec<i64> = stmt
                .query_map(params![current], |row| row.get(0))
                .map_err(|e| crate::Error::Io(format!("has_transitive_dependency: {}", e)))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| crate::Error::Io(format!("read transitive dependency row: {}", e)))?;

            for dep in deps {
                if dep == to {
                    return Ok(true);
                }
                if visited.insert(dep) {
                    queue.push_back(dep);
                }
            }
        }

        Ok(false)
    }

    // ----- subtasks -----

    /// Get direct subtasks of a task.
    pub fn get_subtasks(&self, parent_id: i64) -> crate::Result<Vec<Task>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, project, title, state, priority, parent_id, tags, affected_files,
                        branch, worktree_path, session_id, skip_review, skip_planning,
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

    /// Get all descendant tasks (recursive subtree) of a task.
    ///
    /// Uses iterative BFS to collect all tasks whose parent chain leads back
    /// to `root_id`. Does NOT include the root task itself.
    pub fn get_descendant_tasks(&self, root_id: i64) -> crate::Result<Vec<Task>> {
        let mut descendants = Vec::new();
        let mut queue = std::collections::VecDeque::new();
        queue.push_back(root_id);

        while let Some(parent_id) = queue.pop_front() {
            let children = self.get_subtasks(parent_id)?;
            for child in children {
                queue.push_back(child.id);
                descendants.push(child);
            }
        }

        Ok(descendants)
    }

    // ----- assign -----

    /// Assign a task to a session. Validates task is in `ready` state,
    /// transitions to `active`, sets `session_id`, records in
    /// `task_sessions` and `task_history`.
    ///
    /// For interactive tasks being reassigned, also updates `session_id` on
    /// all descendant tasks within the same transaction. Returns an
    /// `AssignResult` containing the updated task plus information needed
    /// for session reparenting (which requires RPC calls outside the DB).
    pub fn assign_task(&self, task_id: i64, session_id: &str) -> crate::Result<AssignResult> {
        let task = self
            .get_task(task_id)?
            .ok_or_else(|| crate::Error::Io(format!("task {} not found", task_id)))?;

        if task.state != "ready" && task.state != "interactive" {
            return Err(crate::Error::Io(format!(
                "cannot assign task {}: state is '{}', must be 'ready' or 'interactive'",
                task_id, task.state
            )));
        }

        let now = crate::types::timestamp_ms() as i64;
        // Interactive tasks stay interactive; ready tasks transition to active
        let new_state = if task.state == "interactive" {
            "interactive"
        } else {
            "active"
        };

        let old_session_id = task.session_id.clone();
        let session_changed = old_session_id.as_deref() != Some(session_id);

        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(|e| crate::Error::Io(format!("assign_task begin: {}", e)))?;

        tx.execute(
            "UPDATE tasks SET state = ?1, session_id = ?2, updated_at = ?3 \
             WHERE id = ?4",
            params![new_state, session_id, now, task_id],
        )
        .map_err(|e| crate::Error::Io(format!("assign task update: {}", e)))?;

        // Record state change in history (only if state actually changed)
        if new_state != task.state {
            tx.execute(
                "INSERT INTO task_history (task_id, field, old_value, new_value, session_id, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![task_id, "state", task.state, new_state, session_id, now],
            )
            .map_err(|e| crate::Error::Io(format!("assign task history (state): {}", e)))?;
        }

        // Record session_id change in history
        tx.execute(
            "INSERT INTO task_history (task_id, field, old_value, new_value, session_id, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                task_id,
                "session_id",
                task.session_id,
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

        // For interactive tasks with a changed session, update all descendant
        // tasks' session_id within this same transaction (atomic).
        let mut descendant_old_sessions = Vec::new();
        if task.state == "interactive" && session_changed {
            if let Some(ref old_sid) = old_session_id {
                // Collect descendant task IDs using BFS via direct SQL within
                // the transaction (can't call self.get_subtasks inside tx).
                let descendant_ids = {
                    let mut ids = Vec::new();
                    let mut queue = std::collections::VecDeque::new();
                    queue.push_back(task_id);
                    while let Some(pid) = queue.pop_front() {
                        let mut stmt = tx
                            .prepare("SELECT id, session_id FROM tasks WHERE parent_id = ?1")
                            .map_err(|e| {
                                crate::Error::Io(format!("prepare descendant query: {}", e))
                            })?;
                        let rows: Vec<(i64, Option<String>)> = stmt
                            .query_map(params![pid], |row| Ok((row.get(0)?, row.get(1)?)))
                            .map_err(|e| crate::Error::Io(format!("query descendants: {}", e)))?
                            .collect::<Result<Vec<_>, _>>()
                            .map_err(|e| crate::Error::Io(format!("read descendant row: {}", e)))?;
                        for (child_id, child_session) in rows {
                            // Track old sessions that were parented under
                            // the old owner (for reparenting RPCs later).
                            //
                            // Pragmatic simplification: we compare the
                            // descendant task's `session_id` field against
                            // the old owner's session_id. Ideally we'd
                            // check the actual session's `parent_id`
                            // (a session-level property), but that requires
                            // an RPC which we can't do inside a DB
                            // transaction. In practice, tasks dispatched by
                            // the same owner will have matching session_ids.
                            if let Some(ref cs) = child_session {
                                if cs == old_sid {
                                    descendant_old_sessions.push(cs.clone());
                                }
                            }
                            ids.push(child_id);
                            queue.push_back(child_id);
                        }
                    }
                    ids
                };

                // Update session_id on all descendants and record in task_sessions.
                for desc_id in &descendant_ids {
                    tx.execute(
                        "UPDATE tasks SET session_id = ?1, updated_at = ?2 WHERE id = ?3",
                        params![session_id, now, desc_id],
                    )
                    .map_err(|e| {
                        crate::Error::Io(format!("update descendant {} session: {}", desc_id, e))
                    })?;

                    tx.execute(
                        "INSERT OR IGNORE INTO task_sessions (task_id, session_id, role, created_at) \
                         VALUES (?1, ?2, 'assigned', ?3)",
                        params![desc_id, session_id, now],
                    )
                    .map_err(|e| {
                        crate::Error::Io(format!(
                            "record descendant {} session: {}",
                            desc_id, e
                        ))
                    })?;
                }
            }
        }

        tx.commit()
            .map_err(|e| crate::Error::Io(format!("assign_task commit: {}", e)))?;

        let updated_task = self
            .get_task(task_id)?
            .ok_or_else(|| crate::Error::Io("task not found after assign".into()))?;

        Ok(AssignResult {
            task: updated_task,
            old_session_id,
            descendant_old_sessions,
        })
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
                    t.tags, t.affected_files, t.branch,
                    t.worktree_path, t.session_id, t.skip_review, t.skip_planning, t.created_at, t.updated_at
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
                    t.tags, t.affected_files, t.branch,
                    t.worktree_path, t.session_id, t.skip_review, t.skip_planning, t.created_at, t.updated_at
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
    /// or `"main"` if it is a root task. Falls back to `"main"` if the
    /// parent has no branch set (e.g. interactive tasks).
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
                Ok(parent.branch.unwrap_or_else(|| "main".to_string()))
            }
        }
    }

    /// Set the session_id for a task (the session working on it).
    pub fn set_session_id(&self, task_id: i64, session_id: &str) -> crate::Result<()> {
        let now = crate::types::timestamp_ms() as i64;
        let updated = self
            .conn
            .execute(
                "UPDATE tasks SET session_id = ?1, updated_at = ?2 WHERE id = ?3",
                params![session_id, now, task_id],
            )
            .map_err(|e| crate::Error::Io(format!("set_session_id: {}", e)))?;

        if updated == 0 {
            return Err(crate::Error::Io(format!("task {} not found", task_id)));
        }
        Ok(())
    }

    /// Clear the session_id for a task (set to NULL).
    pub fn clear_session_id(&self, task_id: i64) -> crate::Result<()> {
        let now = crate::types::timestamp_ms() as i64;
        let updated = self
            .conn
            .execute(
                "UPDATE tasks SET session_id = NULL, updated_at = ?1 WHERE id = ?2",
                params![now, task_id],
            )
            .map_err(|e| crate::Error::Io(format!("clear_session_id: {}", e)))?;

        if updated == 0 {
            return Err(crate::Error::Io(format!("task {} not found", task_id)));
        }
        Ok(())
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
        branch: row.get(8)?,
        worktree_path: row.get(9)?,
        session_id: row.get(10)?,
        skip_review: row.get::<_, i32>(11)? != 0,
        skip_planning: row.get::<_, i32>(12)? != 0,
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
            .create_task(
                "/project",
                "Tagged task",
                None,
                None,
                Some(&tags),
                false,
                false,
            )
            .unwrap();

        assert_eq!(task.tags.unwrap(), serde_json::json!(["backend", "urgent"]));
    }

    #[test]
    fn test_list_tasks_filtered() {
        let db = TasksDb::open_memory().unwrap();

        let t1 = db
            .create_task("/project", "Task 1", Some(1), None, None, false, false)
            .unwrap();
        let _t2 = db
            .create_task("/project", "Task 2", Some(2), None, None, false, false)
            .unwrap();
        let _t3 = db
            .create_task("/other", "Task 3", None, None, None, false, false)
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
        db.create_task("/project", "Tagged", None, None, Some(&tags), false, false)
            .unwrap();
        db.create_task("/project", "Untagged", None, None, None, false, false)
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
            .create_task("/project", "Original", None, None, None, false, false)
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
        // Forward transitions
        assert!(validate_state_transition("interactive", "ready"));
        assert!(validate_state_transition("interactive", "approved"));
        assert!(validate_state_transition("ready", "active"));
        assert!(validate_state_transition("active", "review"));
        assert!(validate_state_transition("active", "approved"));
        assert!(validate_state_transition("review", "approved"));
        assert!(validate_state_transition("approved", "merging"));
        assert!(validate_state_transition("merging", "done"));

        // Planning/Refining transitions
        assert!(validate_state_transition("interactive", "planning"));
        assert!(validate_state_transition("interactive", "refining"));
        assert!(validate_state_transition("planning", "refining"));
        assert!(validate_state_transition("refining", "planning"));
        assert!(validate_state_transition("refining", "ready"));

        // Backward transitions (error recovery)
        assert!(validate_state_transition("review", "active"));
        assert!(validate_state_transition("approved", "active"));
        assert!(validate_state_transition("approved", "ready"));
        assert!(validate_state_transition("approved", "interactive"));
        assert!(validate_state_transition("merging", "active"));
        assert!(validate_state_transition("merging", "failed"));
        assert!(validate_state_transition("failed", "active"));

        // Universal overrides: any state -> done
        assert!(validate_state_transition("interactive", "done"));
        assert!(validate_state_transition("planning", "done"));
        assert!(validate_state_transition("refining", "done"));
        assert!(validate_state_transition("ready", "done"));
        assert!(validate_state_transition("active", "done"));
        assert!(validate_state_transition("review", "done"));
        assert!(validate_state_transition("approved", "done"));

        // Universal overrides: any state -> interactive
        assert!(validate_state_transition("planning", "interactive"));
        assert!(validate_state_transition("refining", "interactive"));
        assert!(validate_state_transition("ready", "interactive"));
        assert!(validate_state_transition("active", "interactive"));
        assert!(validate_state_transition("review", "interactive"));
        assert!(validate_state_transition("approved", "interactive"));
        assert!(validate_state_transition("done", "interactive"));

        // Universal overrides: any state -> failed
        assert!(validate_state_transition("planning", "failed"));
        assert!(validate_state_transition("refining", "failed"));
        assert!(validate_state_transition("active", "failed"));
        assert!(validate_state_transition("review", "failed"));

        // Self-loops are not allowed
        assert!(!validate_state_transition("done", "done"));
        assert!(!validate_state_transition("interactive", "interactive"));
        assert!(!validate_state_transition("planning", "planning"));
        assert!(!validate_state_transition("refining", "refining"));

        // Skip transitions that don't make sense
        assert!(!validate_state_transition("interactive", "active"));
        assert!(!validate_state_transition("interactive", "merging"));
        assert!(!validate_state_transition("planning", "active")); // must go through refining/ready
        assert!(!validate_state_transition("planning", "review"));
        assert!(!validate_state_transition("refining", "active")); // must go through ready

        // failed state transitions
        assert!(validate_state_transition("merging", "failed"));
        assert!(validate_state_transition("failed", "active"));
        assert!(validate_state_transition("failed", "done")); // universal
        assert!(validate_state_transition("failed", "interactive")); // universal
        assert!(!validate_state_transition("failed", "merging"));
        assert!(!validate_state_transition("failed", "approved"));
    }

    #[test]
    fn test_state_transition_rejected() {
        let db = TasksDb::open_memory().unwrap();
        let task = db
            .create_task("/project", "Test", None, None, None, false, false)
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
    fn test_backward_transitions_for_error_recovery() {
        let db = TasksDb::open_memory().unwrap();

        // Create a task and advance it to approved
        let task = db
            .create_task("/project", "Test", None, None, None, false, false)
            .unwrap();
        // interactive -> ready -> active -> review -> approved
        for state in ["ready", "active", "review", "approved"] {
            db.update_task(
                task.id,
                &TaskUpdate {
                    state: Some(state.into()),
                    ..Default::default()
                },
                None,
            )
            .unwrap();
        }
        assert_eq!(db.get_task(task.id).unwrap().unwrap().state, "approved");

        // approved -> active (merge error, agent needs to fix)
        db.update_task(
            task.id,
            &TaskUpdate {
                state: Some("active".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        assert_eq!(db.get_task(task.id).unwrap().unwrap().state, "active");

        // Back to approved via review
        db.update_task(
            task.id,
            &TaskUpdate {
                state: Some("review".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        db.update_task(
            task.id,
            &TaskUpdate {
                state: Some("approved".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();

        // approved -> ready (unapprove, send back to queue)
        db.update_task(
            task.id,
            &TaskUpdate {
                state: Some("ready".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        assert_eq!(db.get_task(task.id).unwrap().unwrap().state, "ready");

        // ready -> active -> review -> approved
        for state in ["active", "review", "approved"] {
            db.update_task(
                task.id,
                &TaskUpdate {
                    state: Some(state.into()),
                    ..Default::default()
                },
                None,
            )
            .unwrap();
        }

        // approved -> interactive (needs redesign / human intervention)
        db.update_task(
            task.id,
            &TaskUpdate {
                state: Some("interactive".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        assert_eq!(db.get_task(task.id).unwrap().unwrap().state, "interactive");
    }

    #[test]
    fn test_universal_done_override() {
        let db = TasksDb::open_memory().unwrap();

        // Any state -> done should work
        for start_state in ["interactive", "ready", "active", "review", "approved"] {
            let task = db
                .create_task(
                    "/project",
                    &format!("Test {}", start_state),
                    None,
                    None,
                    None,
                    false,
                    false,
                )
                .unwrap();

            // Advance to the start state
            let path_to_state: &[&str] = match start_state {
                "interactive" => &[],
                "ready" => &["ready"],
                "active" => &["ready", "active"],
                "review" => &["ready", "active", "review"],
                "approved" => &["ready", "active", "review", "approved"],
                _ => unreachable!(),
            };
            for state in path_to_state {
                db.update_task(
                    task.id,
                    &TaskUpdate {
                        state: Some((*state).into()),
                        ..Default::default()
                    },
                    None,
                )
                .unwrap();
            }

            // -> done
            db.update_task(
                task.id,
                &TaskUpdate {
                    state: Some("done".into()),
                    ..Default::default()
                },
                None,
            )
            .unwrap();
            assert_eq!(db.get_task(task.id).unwrap().unwrap().state, "done");
        }
    }

    #[test]
    fn test_universal_interactive_override() {
        let db = TasksDb::open_memory().unwrap();

        // Any state -> interactive should work
        for start_state in ["ready", "active", "review", "approved", "done"] {
            let task = db
                .create_task(
                    "/project",
                    &format!("Test {}", start_state),
                    None,
                    None,
                    None,
                    false,
                    false,
                )
                .unwrap();

            // Advance to the start state
            let path_to_state: &[&str] = match start_state {
                "ready" => &["ready"],
                "active" => &["ready", "active"],
                "review" => &["ready", "active", "review"],
                "approved" => &["ready", "active", "review", "approved"],
                "done" => &["done"], // uses universal override
                _ => unreachable!(),
            };
            for state in path_to_state {
                db.update_task(
                    task.id,
                    &TaskUpdate {
                        state: Some((*state).into()),
                        ..Default::default()
                    },
                    None,
                )
                .unwrap();
            }

            // -> interactive
            db.update_task(
                task.id,
                &TaskUpdate {
                    state: Some("interactive".into()),
                    ..Default::default()
                },
                None,
            )
            .unwrap();
            assert_eq!(db.get_task(task.id).unwrap().unwrap().state, "interactive");
        }
    }

    #[test]
    fn test_self_loop_transitions_rejected() {
        let db = TasksDb::open_memory().unwrap();
        let task = db
            .create_task("/project", "Test", None, None, None, false, false)
            .unwrap();

        // interactive -> interactive is a self-loop, should be rejected
        let err = db
            .update_task(
                task.id,
                &TaskUpdate {
                    state: Some("interactive".into()),
                    ..Default::default()
                },
                None,
            )
            .unwrap_err();
        assert!(err.to_string().contains("invalid state transition"));
    }

    #[test]
    fn test_invalid_state_name() {
        let db = TasksDb::open_memory().unwrap();
        let task = db
            .create_task("/project", "Test", None, None, None, false, false)
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
            .create_task("/project", "Test", None, None, None, false, false)
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
            .create_task("/project", "Task 1", None, None, None, false, false)
            .unwrap();
        let t2 = db
            .create_task("/project", "Task 2", None, None, None, false, false)
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
            .create_task("/project", "Task 1", None, None, None, false, false)
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
            .create_task("/project", "Task 1", None, None, None, false, false)
            .unwrap();
        let t2 = db
            .create_task("/project", "Task 2", None, None, None, false, false)
            .unwrap();

        let err = db.add_relation(t1.id, t2.id, "invalid").unwrap_err();
        assert!(err.to_string().contains("invalid relation type"));
    }

    #[test]
    fn test_search() {
        let db = TasksDb::open_memory().unwrap();
        let t1 = db
            .create_task("/project", "Build the API", None, None, None, false, false)
            .unwrap();
        let _t2 = db
            .create_task("/project", "Write docs", None, None, None, false, false)
            .unwrap();
        let t3 = db
            .create_task("/project", "Something else", None, None, None, false, false)
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
            .create_task("/project", "Parent", None, None, None, false, false)
            .unwrap();
        let _child1 = db
            .create_task(
                "/project",
                "Child 1",
                Some(2),
                Some(parent.id),
                None,
                false,
                false,
            )
            .unwrap();
        let _child2 = db
            .create_task(
                "/project",
                "Child 2",
                Some(1),
                Some(parent.id),
                None,
                false,
                false,
            )
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
            .create_task("/project", "Parent", None, None, None, false, false)
            .unwrap();
        db.create_task(
            "/project",
            "Child 1",
            None,
            Some(parent.id),
            None,
            false,
            false,
        )
        .unwrap();
        db.create_task(
            "/project",
            "Child 2",
            None,
            Some(parent.id),
            None,
            false,
            false,
        )
        .unwrap();
        db.create_task("/project", "Other", None, None, None, false, false)
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
            .create_task("/project", "Test", None, None, None, false, false)
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
            .create_task("/project", "Test", None, None, None, true, false)
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
            .create_task("/project", "Test", None, None, None, false, false)
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
        let assigned = db.assign_task(task.id, "session-1").unwrap().task;
        assert_eq!(assigned.state, "active");
        assert_eq!(assigned.session_id.as_deref(), Some("session-1"));

        // Check task_sessions
        let sessions = db.get_sessions(task.id).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, "session-1");
        assert_eq!(sessions[0].role, "worker");

        // Check history includes state and session_id changes
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
                .any(|(f, v)| f == "session_id" && v == "session-1")
        );
    }

    #[test]
    fn test_assign_task_wrong_state() {
        let db = TasksDb::open_memory().unwrap();
        let task = db
            .create_task("/project", "Test", None, None, None, false, false)
            .unwrap();

        // Move to ready then active — active tasks can't be assigned
        db.update_task(
            task.id,
            &TaskUpdate {
                state: Some("ready".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        db.assign_task(task.id, "s0").unwrap();
        // Now task is active — assigning again should fail
        let err = db.assign_task(task.id, "s1").unwrap_err();
        assert!(err.to_string().contains("must be 'ready' or 'interactive'"));
    }

    #[test]
    fn test_assign_interactive_task() {
        let db = TasksDb::open_memory().unwrap();
        let task = db
            .create_task("/project", "Test", None, None, None, false, false)
            .unwrap();
        assert_eq!(task.state, "interactive");

        // Assigning an interactive task should succeed and stay interactive
        let assigned = db.assign_task(task.id, "s1").unwrap().task;
        assert_eq!(assigned.state, "interactive");
        assert_eq!(assigned.session_id.as_deref(), Some("s1"));
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
            .create_task("/project", "Test", None, None, None, false, false)
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
            .create_task("/project", "Test", None, None, None, false, false)
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
    fn test_subtask_defaults_to_planning() {
        let db = TasksDb::open_memory().unwrap();
        let parent = db
            .create_task("/project", "Parent", None, None, None, false, false)
            .unwrap();
        assert_eq!(parent.state, "interactive");

        // Subtask defaults to planning state; skip_review is forced to false
        let child = db
            .create_task(
                "/project",
                "Child",
                None,
                Some(parent.id),
                None,
                true,
                false,
            )
            .unwrap();
        assert_eq!(child.state, "planning");
        assert!(!child.skip_review);
    }

    #[test]
    fn test_subtask_with_skip_planning_defaults_to_ready() {
        let db = TasksDb::open_memory().unwrap();
        let parent = db
            .create_task("/project", "Parent", None, None, None, false, false)
            .unwrap();

        // Subtask with skip_planning=true starts in ready state
        let child = db
            .create_task(
                "/project",
                "Child",
                None,
                Some(parent.id),
                None,
                false,
                true,
            )
            .unwrap();
        assert_eq!(child.state, "ready");
        assert!(child.skip_planning);
    }

    #[test]
    fn test_top_level_task_ignores_skip_planning() {
        let db = TasksDb::open_memory().unwrap();

        // Top-level task with skip_planning=true should still start in 'interactive'
        let task = db
            .create_task("/project", "Top level", None, None, None, false, true)
            .unwrap();
        assert_eq!(task.state, "interactive");
        assert!(task.skip_planning);
    }

    #[test]
    fn test_skip_planning_roundtrip() {
        let db = TasksDb::open_memory().unwrap();
        let task = db
            .create_task("/project", "Test", None, None, None, false, true)
            .unwrap();
        assert!(task.skip_planning);

        let updated = db
            .update_task(
                task.id,
                &TaskUpdate {
                    skip_planning: Some(false),
                    ..Default::default()
                },
                None,
            )
            .unwrap();
        assert!(!updated.skip_planning);
    }

    #[test]
    fn test_active_to_approved_blocked_without_skip_review() {
        let db = TasksDb::open_memory().unwrap();
        let task = db
            .create_task("/project", "Test", None, None, None, false, false)
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
            .create_task("/project", "Test", None, None, None, true, false)
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
            .create_task("/project", "Test", None, None, None, false, false)
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
            .create_task("/project", "Test", None, None, None, false, false)
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
            .create_task("/project", "Test", None, None, None, false, false)
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
            .create_task("/project", "Root task", None, None, None, false, false)
            .unwrap();

        let target = db.get_merge_target(task.id).unwrap();
        assert_eq!(target, "main");
    }

    #[test]
    fn test_get_merge_target_subtask() {
        let db = TasksDb::open_memory().unwrap();
        let parent = db
            .create_task("/project", "Parent", None, None, None, false, false)
            .unwrap();
        db.set_branch(parent.id, "task-1").unwrap();

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

        let target = db.get_merge_target(child.id).unwrap();
        assert_eq!(target, "task-1");
    }

    #[test]
    fn test_get_merge_target_parent_no_branch() {
        let db = TasksDb::open_memory().unwrap();
        let parent = db
            .create_task("/project", "Parent", None, None, None, false, false)
            .unwrap();
        // Don't set a branch on parent — should fall back to "main"

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

        let target = db.get_merge_target(child.id).unwrap();
        assert_eq!(target, "main");
    }

    #[test]
    fn test_get_merge_target_nonexistent_task() {
        let db = TasksDb::open_memory().unwrap();
        let err = db.get_merge_target(99999).unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    // ----- dependency enforcement tests -----

    /// Helper: create a task and move it to a given state using valid transitions.
    fn create_task_in_state(db: &TasksDb, project: &str, title: &str, state: &str) -> Task {
        let task = db
            .create_task(project, title, None, None, None, true, false)
            .unwrap();
        let transitions: &[&str] = match state {
            "interactive" => &[],
            "ready" => &["ready"],
            "active" => &["ready"],
            "review" => &["ready"],
            "approved" => &["ready"],
            "failed" => &["ready"],
            "done" => &["ready"],
            _ => panic!("unsupported target state: {}", state),
        };
        for &s in transitions {
            db.update_task(
                task.id,
                &TaskUpdate {
                    state: Some(s.into()),
                    ..Default::default()
                },
                None,
            )
            .unwrap();
        }
        // For states beyond "ready", use assign_task and further transitions
        match state {
            "active" => {
                db.assign_task(task.id, "test-session").unwrap();
            }
            "review" => {
                db.assign_task(task.id, "test-session").unwrap();
                db.update_task(
                    task.id,
                    &TaskUpdate {
                        state: Some("review".into()),
                        ..Default::default()
                    },
                    None,
                )
                .unwrap();
            }
            "approved" => {
                db.assign_task(task.id, "test-session").unwrap();
                db.update_task(
                    task.id,
                    &TaskUpdate {
                        state: Some("approved".into()),
                        ..Default::default()
                    },
                    None,
                )
                .unwrap();
            }
            "failed" => {
                db.assign_task(task.id, "test-session").unwrap();
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
                db.update_task(
                    task.id,
                    &TaskUpdate {
                        state: Some("failed".into()),
                        ..Default::default()
                    },
                    None,
                )
                .unwrap();
            }
            "done" => {
                db.assign_task(task.id, "test-session").unwrap();
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
                db.update_task(
                    task.id,
                    &TaskUpdate {
                        state: Some("done".into()),
                        ..Default::default()
                    },
                    None,
                )
                .unwrap();
            }
            _ => {}
        }
        db.get_task(task.id).unwrap().unwrap()
    }

    #[test]
    fn test_get_blocking_dependencies_with_unmet_dep() {
        let db = TasksDb::open_memory().unwrap();
        let dep = create_task_in_state(&db, "/project", "Dependency", "ready");
        let task = create_task_in_state(&db, "/project", "Dependent", "ready");

        db.add_relation(task.id, dep.id, "depends_on").unwrap();

        let blocking = db.get_blocking_dependencies(task.id).unwrap();
        assert_eq!(blocking.len(), 1);
        assert_eq!(blocking[0].id, dep.id);
    }

    #[test]
    fn test_get_blocking_dependencies_with_met_dep() {
        let db = TasksDb::open_memory().unwrap();
        let dep = create_task_in_state(&db, "/project", "Dependency", "done");
        let task = create_task_in_state(&db, "/project", "Dependent", "ready");

        db.add_relation(task.id, dep.id, "depends_on").unwrap();

        let blocking = db.get_blocking_dependencies(task.id).unwrap();
        assert!(blocking.is_empty());
    }

    #[test]
    fn test_get_blocking_dependencies_no_deps() {
        let db = TasksDb::open_memory().unwrap();
        let task = create_task_in_state(&db, "/project", "No deps", "ready");

        let blocking = db.get_blocking_dependencies(task.id).unwrap();
        assert!(blocking.is_empty());
    }

    #[test]
    fn test_get_blocking_dependencies_ignores_non_depends_on() {
        let db = TasksDb::open_memory().unwrap();
        let other = create_task_in_state(&db, "/project", "Related", "ready");
        let task = create_task_in_state(&db, "/project", "Task", "ready");

        // "related" relation should NOT count as a blocking dependency
        db.add_relation(task.id, other.id, "related").unwrap();

        let blocking = db.get_blocking_dependencies(task.id).unwrap();
        assert!(blocking.is_empty());
    }

    #[test]
    fn test_get_schedulable_tasks_with_unmet_dep() {
        let db = TasksDb::open_memory().unwrap();
        let dep = create_task_in_state(&db, "/project", "Dependency", "active");
        let task = create_task_in_state(&db, "/project", "Blocked task", "ready");

        db.add_relation(task.id, dep.id, "depends_on").unwrap();

        let schedulable = db.get_schedulable_tasks("/project").unwrap();
        // dep is active (not ready), task is blocked — neither should be schedulable
        let ids: Vec<i64> = schedulable.iter().map(|t| t.id).collect();
        assert!(!ids.contains(&task.id));
    }

    #[test]
    fn test_get_schedulable_tasks_with_met_dep() {
        let db = TasksDb::open_memory().unwrap();
        let dep = create_task_in_state(&db, "/project", "Dependency", "done");
        let task = create_task_in_state(&db, "/project", "Unblocked task", "ready");

        db.add_relation(task.id, dep.id, "depends_on").unwrap();

        let schedulable = db.get_schedulable_tasks("/project").unwrap();
        let ids: Vec<i64> = schedulable.iter().map(|t| t.id).collect();
        assert!(ids.contains(&task.id));
    }

    #[test]
    fn test_get_schedulable_tasks_no_deps() {
        let db = TasksDb::open_memory().unwrap();
        let task = create_task_in_state(&db, "/project", "Independent task", "ready");

        let schedulable = db.get_schedulable_tasks("/project").unwrap();
        let ids: Vec<i64> = schedulable.iter().map(|t| t.id).collect();
        assert!(ids.contains(&task.id));
    }

    #[test]
    fn test_get_schedulable_tasks_mixed() {
        let db = TasksDb::open_memory().unwrap();
        let dep = create_task_in_state(&db, "/project", "Dependency", "active");
        let blocked = create_task_in_state(&db, "/project", "Blocked", "ready");
        let free = create_task_in_state(&db, "/project", "Free", "ready");

        db.add_relation(blocked.id, dep.id, "depends_on").unwrap();

        let schedulable = db.get_schedulable_tasks("/project").unwrap();
        let ids: Vec<i64> = schedulable.iter().map(|t| t.id).collect();
        assert!(!ids.contains(&blocked.id));
        assert!(ids.contains(&free.id));
    }

    #[test]
    fn test_get_schedulable_tasks_only_ready() {
        let db = TasksDb::open_memory().unwrap();
        // Active task should NOT appear in schedulable
        let _active = create_task_in_state(&db, "/project", "Active", "active");
        // Interactive task should NOT appear
        let _interactive = create_task_in_state(&db, "/project", "Interactive", "interactive");
        // Only this ready one should appear
        let ready = create_task_in_state(&db, "/project", "Ready", "ready");

        let schedulable = db.get_schedulable_tasks("/project").unwrap();
        assert_eq!(schedulable.len(), 1);
        assert_eq!(schedulable[0].id, ready.id);
    }

    #[test]
    fn test_get_schedulable_tasks_project_scoped() {
        let db = TasksDb::open_memory().unwrap();
        let _task_a = create_task_in_state(&db, "/project-a", "Task A", "ready");
        let _task_b = create_task_in_state(&db, "/project-b", "Task B", "ready");

        let schedulable = db.get_schedulable_tasks("/project-a").unwrap();
        assert_eq!(schedulable.len(), 1);
        assert_eq!(schedulable[0].title, "Task A");
    }

    #[test]
    fn test_self_referential_relation_rejected() {
        let db = TasksDb::open_memory().unwrap();
        let task = db
            .create_task("/project", "Self ref", None, None, None, false, false)
            .unwrap();

        let err = db.add_relation(task.id, task.id, "depends_on").unwrap_err();
        assert!(err.to_string().contains("relation from a task to itself"));
    }

    #[test]
    fn test_cross_project_relation_rejected() {
        let db = TasksDb::open_memory().unwrap();
        let t1 = db
            .create_task("/project-a", "Task A", None, None, None, false, false)
            .unwrap();
        let t2 = db
            .create_task("/project-b", "Task B", None, None, None, false, false)
            .unwrap();

        let err = db.add_relation(t1.id, t2.id, "depends_on").unwrap_err();
        assert!(err.to_string().contains("across projects"));
    }

    #[test]
    fn test_circular_dependency_direct() {
        let db = TasksDb::open_memory().unwrap();
        let t1 = db
            .create_task("/project", "Task 1", None, None, None, false, false)
            .unwrap();
        let t2 = db
            .create_task("/project", "Task 2", None, None, None, false, false)
            .unwrap();

        // A depends_on B — OK
        db.add_relation(t1.id, t2.id, "depends_on").unwrap();

        // B depends_on A — should be rejected (cycle)
        let err = db.add_relation(t2.id, t1.id, "depends_on").unwrap_err();
        assert!(err.to_string().contains("circular dependency"));
    }

    #[test]
    fn test_circular_dependency_transitive() {
        let db = TasksDb::open_memory().unwrap();
        let t1 = db
            .create_task("/project", "Task 1", None, None, None, false, false)
            .unwrap();
        let t2 = db
            .create_task("/project", "Task 2", None, None, None, false, false)
            .unwrap();
        let t3 = db
            .create_task("/project", "Task 3", None, None, None, false, false)
            .unwrap();

        // A -> B -> C
        db.add_relation(t1.id, t2.id, "depends_on").unwrap();
        db.add_relation(t2.id, t3.id, "depends_on").unwrap();

        // C -> A would create a cycle
        let err = db.add_relation(t3.id, t1.id, "depends_on").unwrap_err();
        assert!(err.to_string().contains("circular dependency"));
    }

    #[test]
    fn test_circular_dependency_not_triggered_for_non_depends_on() {
        let db = TasksDb::open_memory().unwrap();
        let t1 = db
            .create_task("/project", "Task 1", None, None, None, false, false)
            .unwrap();
        let t2 = db
            .create_task("/project", "Task 2", None, None, None, false, false)
            .unwrap();

        // A depends_on B
        db.add_relation(t1.id, t2.id, "depends_on").unwrap();

        // B "related" A — should succeed (cycle check only for depends_on)
        db.add_relation(t2.id, t1.id, "related").unwrap();
    }

    #[test]
    fn test_has_transitive_dependency_no_path() {
        let db = TasksDb::open_memory().unwrap();
        let t1 = db
            .create_task("/project", "Task 1", None, None, None, false, false)
            .unwrap();
        let t2 = db
            .create_task("/project", "Task 2", None, None, None, false, false)
            .unwrap();

        assert!(!db.has_transitive_dependency(t1.id, t2.id).unwrap());
    }

    #[test]
    fn test_has_transitive_dependency_direct() {
        let db = TasksDb::open_memory().unwrap();
        let t1 = db
            .create_task("/project", "Task 1", None, None, None, false, false)
            .unwrap();
        let t2 = db
            .create_task("/project", "Task 2", None, None, None, false, false)
            .unwrap();

        db.add_relation(t1.id, t2.id, "depends_on").unwrap();

        assert!(db.has_transitive_dependency(t1.id, t2.id).unwrap());
        assert!(!db.has_transitive_dependency(t2.id, t1.id).unwrap());
    }

    #[test]
    fn test_has_transitive_dependency_chain() {
        let db = TasksDb::open_memory().unwrap();
        let t1 = db
            .create_task("/project", "Task 1", None, None, None, false, false)
            .unwrap();
        let t2 = db
            .create_task("/project", "Task 2", None, None, None, false, false)
            .unwrap();
        let t3 = db
            .create_task("/project", "Task 3", None, None, None, false, false)
            .unwrap();
        let t4 = db
            .create_task("/project", "Task 4", None, None, None, false, false)
            .unwrap();

        // 1 -> 2 -> 3 -> 4
        db.add_relation(t1.id, t2.id, "depends_on").unwrap();
        db.add_relation(t2.id, t3.id, "depends_on").unwrap();
        db.add_relation(t3.id, t4.id, "depends_on").unwrap();

        assert!(db.has_transitive_dependency(t1.id, t4.id).unwrap());
        assert!(db.has_transitive_dependency(t1.id, t3.id).unwrap());
        assert!(db.has_transitive_dependency(t2.id, t4.id).unwrap());
        assert!(!db.has_transitive_dependency(t4.id, t1.id).unwrap());
    }

    #[test]
    fn test_get_blocking_dependencies_multiple() {
        let db = TasksDb::open_memory().unwrap();
        let dep1 = create_task_in_state(&db, "/project", "Dep 1", "active");
        let dep2 = create_task_in_state(&db, "/project", "Dep 2", "done");
        let dep3 = create_task_in_state(&db, "/project", "Dep 3", "ready");
        let task = create_task_in_state(&db, "/project", "Main task", "ready");

        db.add_relation(task.id, dep1.id, "depends_on").unwrap();
        db.add_relation(task.id, dep2.id, "depends_on").unwrap();
        db.add_relation(task.id, dep3.id, "depends_on").unwrap();

        let blocking = db.get_blocking_dependencies(task.id).unwrap();
        // dep1 (active) and dep3 (ready) are blocking; dep2 (done) is not
        assert_eq!(blocking.len(), 2);
        let blocking_ids: Vec<i64> = blocking.iter().map(|t| t.id).collect();
        assert!(blocking_ids.contains(&dep1.id));
        assert!(blocking_ids.contains(&dep3.id));
        assert!(!blocking_ids.contains(&dep2.id));
    }

    #[test]
    fn test_circular_dependency_diamond() {
        // Diamond: A -> B, A -> C, B -> D, C -> D
        // Then D -> A should be rejected (cycle through either path)
        let db = TasksDb::open_memory().unwrap();
        let a = db
            .create_task("/p", "A", None, None, None, false, false)
            .unwrap();
        let b = db
            .create_task("/p", "B", None, None, None, false, false)
            .unwrap();
        let c = db
            .create_task("/p", "C", None, None, None, false, false)
            .unwrap();
        let d = db
            .create_task("/p", "D", None, None, None, false, false)
            .unwrap();

        db.add_relation(a.id, b.id, "depends_on").unwrap();
        db.add_relation(a.id, c.id, "depends_on").unwrap();
        db.add_relation(b.id, d.id, "depends_on").unwrap();
        db.add_relation(c.id, d.id, "depends_on").unwrap();

        // D -> A creates a cycle
        let err = db.add_relation(d.id, a.id, "depends_on").unwrap_err();
        assert!(err.to_string().contains("circular dependency"));

        // But D -> (new task E) is fine
        let e = db
            .create_task("/p", "E", None, None, None, false, false)
            .unwrap();
        db.add_relation(d.id, e.id, "depends_on").unwrap();
    }

    #[test]
    fn test_circular_dependency_mid_chain() {
        // Chain: A -> B -> C -> D -> E
        // Adding E -> C should be rejected (cycle C -> D -> E -> C)
        let db = TasksDb::open_memory().unwrap();
        let a = db
            .create_task("/p", "A", None, None, None, false, false)
            .unwrap();
        let b = db
            .create_task("/p", "B", None, None, None, false, false)
            .unwrap();
        let c = db
            .create_task("/p", "C", None, None, None, false, false)
            .unwrap();
        let d = db
            .create_task("/p", "D", None, None, None, false, false)
            .unwrap();
        let e = db
            .create_task("/p", "E", None, None, None, false, false)
            .unwrap();

        db.add_relation(a.id, b.id, "depends_on").unwrap();
        db.add_relation(b.id, c.id, "depends_on").unwrap();
        db.add_relation(c.id, d.id, "depends_on").unwrap();
        db.add_relation(d.id, e.id, "depends_on").unwrap();

        let err = db.add_relation(e.id, c.id, "depends_on").unwrap_err();
        assert!(err.to_string().contains("circular dependency"));
    }

    #[test]
    fn test_no_false_positive_cycle_with_blocks() {
        // A depends_on B, B blocks A — no real cycle since blocks is informational
        let db = TasksDb::open_memory().unwrap();
        let a = db
            .create_task("/p", "A", None, None, None, false, false)
            .unwrap();
        let b = db
            .create_task("/p", "B", None, None, None, false, false)
            .unwrap();

        db.add_relation(a.id, b.id, "depends_on").unwrap();
        // blocks is informational, shouldn't trigger cycle detection
        db.add_relation(b.id, a.id, "blocks").unwrap();
    }

    #[test]
    fn test_parallel_non_conflicting_deps_allowed() {
        // A -> C and B -> C is fine (convergent, no cycle)
        let db = TasksDb::open_memory().unwrap();
        let a = db
            .create_task("/p", "A", None, None, None, false, false)
            .unwrap();
        let b = db
            .create_task("/p", "B", None, None, None, false, false)
            .unwrap();
        let c = db
            .create_task("/p", "C", None, None, None, false, false)
            .unwrap();

        db.add_relation(a.id, c.id, "depends_on").unwrap();
        db.add_relation(b.id, c.id, "depends_on").unwrap();
        // And C -> D is fine (no cycle)
        let d = db
            .create_task("/p", "D", None, None, None, false, false)
            .unwrap();
        db.add_relation(c.id, d.id, "depends_on").unwrap();
    }

    // ----- get_approved_tasks tests -----

    #[test]
    fn test_get_approved_tasks_empty() {
        let db = TasksDb::open_memory().unwrap();
        let tasks = db.get_approved_tasks(None).unwrap();
        assert!(tasks.is_empty());
    }

    #[test]
    fn test_get_approved_tasks_returns_only_approved() {
        let db = TasksDb::open_memory().unwrap();

        // Create tasks in various states
        let t1 = db
            .create_task("/project", "Ready task", None, None, None, true, false)
            .unwrap();
        db.update_task(
            t1.id,
            &TaskUpdate {
                state: Some("ready".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();

        let t2 = db
            .create_task("/project", "Approved task", None, None, None, true, false)
            .unwrap();
        db.update_task(
            t2.id,
            &TaskUpdate {
                state: Some("ready".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        db.assign_task(t2.id, "s1").unwrap();
        db.update_task(
            t2.id,
            &TaskUpdate {
                state: Some("approved".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();

        let t3 = db
            .create_task(
                "/project",
                "Another approved",
                None,
                None,
                None,
                true,
                false,
            )
            .unwrap();
        db.update_task(
            t3.id,
            &TaskUpdate {
                state: Some("ready".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        db.assign_task(t3.id, "s2").unwrap();
        db.update_task(
            t3.id,
            &TaskUpdate {
                state: Some("approved".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();

        // All projects
        let approved = db.get_approved_tasks(None).unwrap();
        assert_eq!(approved.len(), 2);

        // Specific project
        let approved = db.get_approved_tasks(Some("/project")).unwrap();
        assert_eq!(approved.len(), 2);

        // Non-existent project
        let approved = db.get_approved_tasks(Some("/other")).unwrap();
        assert!(approved.is_empty());
    }

    #[test]
    fn test_get_approved_tasks_filters_by_project() {
        let db = TasksDb::open_memory().unwrap();

        let t1 = db
            .create_task("/project-a", "Task A", None, None, None, true, false)
            .unwrap();
        db.update_task(
            t1.id,
            &TaskUpdate {
                state: Some("ready".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        db.assign_task(t1.id, "s1").unwrap();
        db.update_task(
            t1.id,
            &TaskUpdate {
                state: Some("approved".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();

        let t2 = db
            .create_task("/project-b", "Task B", None, None, None, true, false)
            .unwrap();
        db.update_task(
            t2.id,
            &TaskUpdate {
                state: Some("ready".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        db.assign_task(t2.id, "s2").unwrap();
        db.update_task(
            t2.id,
            &TaskUpdate {
                state: Some("approved".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();

        let a_tasks = db.get_approved_tasks(Some("/project-a")).unwrap();
        assert_eq!(a_tasks.len(), 1);
        assert_eq!(a_tasks[0].id, t1.id);

        let b_tasks = db.get_approved_tasks(Some("/project-b")).unwrap();
        assert_eq!(b_tasks.len(), 1);
        assert_eq!(b_tasks[0].id, t2.id);

        let all_tasks = db.get_approved_tasks(None).unwrap();
        assert_eq!(all_tasks.len(), 2);
    }

    #[test]
    fn test_get_approved_tasks_priority_ordering() {
        let db = TasksDb::open_memory().unwrap();

        let t1 = db
            .create_task("/project", "Low priority", Some(1), None, None, true, false)
            .unwrap();
        db.update_task(
            t1.id,
            &TaskUpdate {
                state: Some("ready".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        db.assign_task(t1.id, "s1").unwrap();
        db.update_task(
            t1.id,
            &TaskUpdate {
                state: Some("approved".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();

        let t2 = db
            .create_task(
                "/project",
                "High priority",
                Some(10),
                None,
                None,
                true,
                false,
            )
            .unwrap();
        db.update_task(
            t2.id,
            &TaskUpdate {
                state: Some("ready".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        db.assign_task(t2.id, "s2").unwrap();
        db.update_task(
            t2.id,
            &TaskUpdate {
                state: Some("approved".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();

        let approved = db.get_approved_tasks(None).unwrap();
        assert_eq!(approved.len(), 2);
        // Higher priority should come first
        assert_eq!(approved[0].id, t2.id);
        assert_eq!(approved[1].id, t1.id);
    }

    fn make_task(id: i64, parent_id: Option<i64>, priority: i64) -> Task {
        Task {
            id,
            project: "test".into(),
            title: format!("Task {}", id),
            state: "ready".into(),
            priority,
            parent_id,
            tags: None,
            affected_files: None,
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
    fn test_tree_order_empty() {
        let result = tree_order(vec![]);
        assert!(result.is_empty());
    }

    #[test]
    fn test_tree_order_flat() {
        let tasks = vec![
            make_task(1, None, 0),
            make_task(2, None, 5),
            make_task(3, None, 0),
        ];
        let result = tree_order(tasks);
        // sorted by priority desc, then id asc
        assert_eq!(result[0].0, 0); // depth
        assert_eq!(result[0].1.id, 2); // highest priority
        assert_eq!(result[1].1.id, 1);
        assert_eq!(result[2].1.id, 3);
    }

    #[test]
    fn test_tree_order_parent_child() {
        let tasks = vec![
            make_task(1, None, 0),
            make_task(2, Some(1), 5),
            make_task(3, Some(1), 7),
            make_task(4, None, 0),
        ];
        let result = tree_order(tasks);
        // Roots: 1, 4 (same priority, id order)
        // Children of 1: 3 (pri 7), 2 (pri 5)
        assert_eq!(result.len(), 4);
        assert_eq!((result[0].0, result[0].1.id), (0, 1));
        assert_eq!((result[1].0, result[1].1.id), (1, 3)); // higher priority child first
        assert_eq!((result[2].0, result[2].1.id), (1, 2));
        assert_eq!((result[3].0, result[3].1.id), (0, 4));
    }

    #[test]
    fn test_tree_order_nested() {
        let tasks = vec![
            make_task(1, None, 0),
            make_task(2, Some(1), 0),
            make_task(3, Some(2), 0),
        ];
        let result = tree_order(tasks);
        assert_eq!(result.len(), 3);
        assert_eq!((result[0].0, result[0].1.id), (0, 1));
        assert_eq!((result[1].0, result[1].1.id), (1, 2));
        assert_eq!((result[2].0, result[2].1.id), (2, 3));
    }

    #[test]
    fn test_tree_order_orphan_parent_not_in_list() {
        // Task 5 has parent_id=99 but 99 is not in the list; treat as root
        let tasks = vec![make_task(1, None, 0), make_task(5, Some(99), 0)];
        let result = tree_order(tasks);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].0, 0);
        assert_eq!(result[1].0, 0);
    }

    // ----- get_descendant_tasks tests -----

    #[test]
    fn test_get_descendant_tasks_single_level() {
        let db = TasksDb::open_memory().unwrap();
        let parent = db
            .create_task("/project", "Parent", None, None, None, false, false)
            .unwrap();
        let child1 = db
            .create_task(
                "/project",
                "Child 1",
                None,
                Some(parent.id),
                None,
                false,
                false,
            )
            .unwrap();
        let child2 = db
            .create_task(
                "/project",
                "Child 2",
                None,
                Some(parent.id),
                None,
                false,
                false,
            )
            .unwrap();

        let descendants = db.get_descendant_tasks(parent.id).unwrap();
        assert_eq!(descendants.len(), 2);
        let ids: Vec<i64> = descendants.iter().map(|t| t.id).collect();
        assert!(ids.contains(&child1.id));
        assert!(ids.contains(&child2.id));
    }

    #[test]
    fn test_get_descendant_tasks_nested() {
        let db = TasksDb::open_memory().unwrap();
        let root = db
            .create_task("/project", "Root", None, None, None, false, false)
            .unwrap();
        let child = db
            .create_task("/project", "Child", None, Some(root.id), None, false, false)
            .unwrap();
        let grandchild = db
            .create_task(
                "/project",
                "Grandchild",
                None,
                Some(child.id),
                None,
                false,
                false,
            )
            .unwrap();

        let descendants = db.get_descendant_tasks(root.id).unwrap();
        assert_eq!(descendants.len(), 2);
        let ids: Vec<i64> = descendants.iter().map(|t| t.id).collect();
        assert!(ids.contains(&child.id));
        assert!(ids.contains(&grandchild.id));
    }

    #[test]
    fn test_get_descendant_tasks_no_children() {
        let db = TasksDb::open_memory().unwrap();
        let task = db
            .create_task("/project", "Leaf", None, None, None, false, false)
            .unwrap();

        let descendants = db.get_descendant_tasks(task.id).unwrap();
        assert!(descendants.is_empty());
    }

    // ----- assign_task sets session_id tests -----

    #[test]
    fn test_assign_task_sets_session_id() {
        let db = TasksDb::open_memory().unwrap();
        let task = db
            .create_task("/project", "Test", None, None, None, false, false)
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

        let assigned = db.assign_task(task.id, "session-1").unwrap().task;
        assert_eq!(assigned.session_id.as_deref(), Some("session-1"));

        // Verify history records session_id change
        let mut stmt = db
            .conn
            .prepare("SELECT field, new_value FROM task_history WHERE task_id = ?1 ORDER BY id")
            .unwrap();
        let history: Vec<(String, String)> = stmt
            .query_map(params![task.id], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert!(
            history
                .iter()
                .any(|(f, v)| f == "session_id" && v == "session-1")
        );
    }

    #[test]
    fn test_clear_session_id() {
        let db = TasksDb::open_memory().unwrap();
        let task = db
            .create_task("/project", "Test", None, None, None, false, false)
            .unwrap();
        db.set_session_id(task.id, "s1").unwrap();
        assert_eq!(
            db.get_task(task.id).unwrap().unwrap().session_id.as_deref(),
            Some("s1")
        );

        db.clear_session_id(task.id).unwrap();
        assert!(db.get_task(task.id).unwrap().unwrap().session_id.is_none());
    }

    // ----- planning/refining cycle tests -----

    #[test]
    fn test_planning_refining_cycle() {
        let db = TasksDb::open_memory().unwrap();
        let task = db
            .create_task("/project", "Planned task", None, None, None, false, false)
            .unwrap();
        assert_eq!(task.state, "interactive");

        // interactive -> planning
        db.update_task(
            task.id,
            &TaskUpdate {
                state: Some("planning".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        assert_eq!(db.get_task(task.id).unwrap().unwrap().state, "planning");

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
        assert_eq!(db.get_task(task.id).unwrap().unwrap().state, "refining");

        // refining -> planning (revision needed)
        db.update_task(
            task.id,
            &TaskUpdate {
                state: Some("planning".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        assert_eq!(db.get_task(task.id).unwrap().unwrap().state, "planning");

        // planning -> refining -> ready
        db.update_task(
            task.id,
            &TaskUpdate {
                state: Some("refining".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        db.update_task(
            task.id,
            &TaskUpdate {
                state: Some("ready".into()),
                affected_files: Some(serde_json::json!(["src/main.rs"])),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        assert_eq!(db.get_task(task.id).unwrap().unwrap().state, "ready");
    }

    #[test]
    fn test_refining_to_interactive_scope_expansion() {
        let db = TasksDb::open_memory().unwrap();
        let task = db
            .create_task(
                "/project",
                "Expanding scope",
                None,
                None,
                None,
                false,
                false,
            )
            .unwrap();

        // interactive -> planning -> refining -> interactive (scope expansion)
        for state in ["planning", "refining", "interactive"] {
            db.update_task(
                task.id,
                &TaskUpdate {
                    state: Some(state.into()),
                    ..Default::default()
                },
                None,
            )
            .unwrap();
        }
        assert_eq!(db.get_task(task.id).unwrap().unwrap().state, "interactive");
    }

    #[test]
    fn test_interactive_to_refining_directly() {
        let db = TasksDb::open_memory().unwrap();
        let task = db
            .create_task("/project", "Spec review", None, None, None, false, false)
            .unwrap();

        // interactive -> refining (user already wrote spec, wants LLM review)
        db.update_task(
            task.id,
            &TaskUpdate {
                state: Some("refining".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        assert_eq!(db.get_task(task.id).unwrap().unwrap().state, "refining");
    }

    #[test]
    fn test_planning_cannot_skip_to_active() {
        let db = TasksDb::open_memory().unwrap();
        let task = db
            .create_task("/project", "Bad skip", None, None, None, false, false)
            .unwrap();
        db.update_task(
            task.id,
            &TaskUpdate {
                state: Some("planning".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();

        // planning -> active is NOT valid (must go through refining/ready)
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
    }

    #[test]
    fn test_refining_to_ready_rejected_without_affected_files() {
        let db = TasksDb::open_memory().unwrap();
        let task = db
            .create_task("/project", "No files", None, None, None, false, false)
            .unwrap();

        // interactive -> planning -> refining
        db.update_task(
            task.id,
            &TaskUpdate {
                state: Some("planning".into()),
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

        // refining -> ready without affected_files should fail
        let err = db
            .update_task(
                task.id,
                &TaskUpdate {
                    state: Some("ready".into()),
                    ..Default::default()
                },
                None,
            )
            .unwrap_err();
        assert!(
            err.to_string().contains("affected_files"),
            "expected affected_files error, got: {}",
            err
        );
    }

    #[test]
    fn test_refining_to_ready_rejected_with_empty_affected_files() {
        let db = TasksDb::open_memory().unwrap();
        let task = db
            .create_task("/project", "Empty files", None, None, None, false, false)
            .unwrap();

        // interactive -> planning -> refining, set affected_files to empty array
        db.update_task(
            task.id,
            &TaskUpdate {
                state: Some("planning".into()),
                affected_files: Some(serde_json::json!([])),
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

        // refining -> ready with empty affected_files should fail
        let err = db
            .update_task(
                task.id,
                &TaskUpdate {
                    state: Some("ready".into()),
                    ..Default::default()
                },
                None,
            )
            .unwrap_err();
        assert!(
            err.to_string().contains("affected_files"),
            "expected affected_files error, got: {}",
            err
        );
    }

    #[test]
    fn test_refining_to_ready_succeeds_with_affected_files() {
        let db = TasksDb::open_memory().unwrap();
        let task = db
            .create_task("/project", "Has files", None, None, None, false, false)
            .unwrap();

        // interactive -> planning -> refining with affected_files set
        db.update_task(
            task.id,
            &TaskUpdate {
                state: Some("planning".into()),
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

        // refining -> ready with affected_files set should succeed
        let updated = db
            .update_task(
                task.id,
                &TaskUpdate {
                    state: Some("ready".into()),
                    ..Default::default()
                },
                None,
            )
            .unwrap();
        assert_eq!(updated.state, "ready");
    }

    #[test]
    fn test_refining_to_ready_succeeds_when_files_set_in_same_update() {
        let db = TasksDb::open_memory().unwrap();
        let task = db
            .create_task(
                "/project",
                "Files in update",
                None,
                None,
                None,
                false,
                false,
            )
            .unwrap();

        // interactive -> planning -> refining (no affected_files yet)
        db.update_task(
            task.id,
            &TaskUpdate {
                state: Some("planning".into()),
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

        // refining -> ready with affected_files set in the same update should succeed
        let updated = db
            .update_task(
                task.id,
                &TaskUpdate {
                    state: Some("ready".into()),
                    affected_files: Some(serde_json::json!(["src/lib.rs"])),
                    ..Default::default()
                },
                None,
            )
            .unwrap();
        assert_eq!(updated.state, "ready");
    }

    #[test]
    fn test_universal_failed_override() {
        let db = TasksDb::open_memory().unwrap();

        // Any state -> failed should work
        for start_state in [
            "interactive",
            "planning",
            "refining",
            "ready",
            "active",
            "review",
        ] {
            let task = db
                .create_task(
                    "/project",
                    &format!("Test {}", start_state),
                    None,
                    None,
                    None,
                    true,
                    true,
                )
                .unwrap();

            let transitions: &[&str] = match start_state {
                "interactive" => &[],
                "planning" => &["planning"],
                "refining" => &["planning", "refining"],
                "ready" => &["ready"],
                "active" => &["ready"],
                "review" => &["ready"],
                _ => unreachable!(),
            };
            for &s in transitions {
                db.update_task(
                    task.id,
                    &TaskUpdate {
                        state: Some(s.into()),
                        ..Default::default()
                    },
                    None,
                )
                .unwrap();
            }
            match start_state {
                "active" | "review" => {
                    db.assign_task(task.id, "test-session").unwrap();
                    if start_state == "review" {
                        db.update_task(
                            task.id,
                            &TaskUpdate {
                                state: Some("review".into()),
                                ..Default::default()
                            },
                            None,
                        )
                        .unwrap();
                    }
                }
                _ => {}
            }

            // -> failed
            db.update_task(
                task.id,
                &TaskUpdate {
                    state: Some("failed".into()),
                    ..Default::default()
                },
                None,
            )
            .unwrap();
            assert_eq!(db.get_task(task.id).unwrap().unwrap().state, "failed");
        }
    }
}
