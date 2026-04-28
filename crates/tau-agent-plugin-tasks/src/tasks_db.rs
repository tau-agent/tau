//! SQLite-backed task persistence for the tau task system plugin.

use std::path::PathBuf;

use rusqlite::{Connection, OptionalExtension, params};

use crate::err::plugin_io_err;
use crate::tasks_state::{TaskState, should_clear_session_id_on_transition, validate_transition};

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Task {
    pub id: i64,
    pub project_name: String,
    pub title: String,
    pub state: TaskState,
    pub priority: i64,
    pub parent_id: Option<i64>,
    pub tags: Option<serde_json::Value>,
    pub affected_files: Option<serde_json::Value>,
    pub branch: Option<String>,
    pub merge_target: Option<String>,
    pub worktree_path: Option<String>,
    pub session_id: Option<String>,
    pub skip_review: bool,
    pub require_approval: bool,
    pub sandbox_profile: Option<String>,
    /// When true, the task is NOT scheduled for dispatch even if its state
    /// would otherwise be schedulable. Orthogonal to state — a held task
    /// remains visible in listings and preserves its state, but the
    /// scheduler's eligibility predicate skips it. Released via
    /// `update_task` with `held: Some(false)`.
    pub held: bool,
    /// The task's *placeholder session* — a non-LLM (`model = "log"`)
    /// session that owns every other session spawned for this task
    /// (planner, refiner, worker, reviewer, merge, future automation).
    /// `None` for tasks created before task #561 introduced placeholders;
    /// dispatch helpers fall back to the legacy parenting rule in that
    /// case. See `create_placeholder_session` in `tasks.rs`.
    pub placeholder_session_id: Option<String>,
    /// Set when a `ready`-state task was filed without an
    /// `affected_files` list and so was auto-routed through planning
    /// (task #596). Used by [`build_planning_message`] to add a
    /// gentle nudge: "the caller filed this as ready and thought the
    /// spec was complete — treat their scope as authoritative, but
    /// still do the full planning exploration." The task otherwise
    /// walks the normal `planning → refining → ready` flow, identical
    /// to any planning-originated task. Defaults to `false` for tasks
    /// that took the normal initial_state path.
    pub auto_downgraded_from_ready: bool,
    /// Project the task was *filed from* — i.e. the calling session's
    /// project at the time `task_create` ran. Distinct from
    /// [`Task::project_name`], which is the project the task targets
    /// (where the work runs). Equal to `project_name` for same-project
    /// filing, different for cross-project filing (see #750). `None` for
    /// tasks created before #758 introduced the column — we don't
    /// backfill historical provenance.
    ///
    /// Write-once at create time. Reparenting (#751) does not modify it;
    /// the semantics are "where this task was originally filed from".
    pub filed_by_project: Option<String>,
    /// Session id of the caller that ran `task_create`. May be the
    /// session of an LLM agent, an interactive user session, or
    /// (rarely) internal scheduler machinery. `None` for tasks created
    /// before #758, or when no calling session was available.
    ///
    /// Write-once at create time; see [`Task::filed_by_project`].
    pub filed_by_session_id: Option<String>,
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

/// A single row from `task_history` — one update event (state transition,
/// priority bump, etc.) recorded by `update_task`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TaskHistoryEntry {
    /// Field that was updated: "state", "priority", "held", "affected_files",
    /// "title", ...
    pub field: String,
    pub old_value: Option<String>,
    pub new_value: Option<String>,
    /// Session that performed the update, if known.
    pub session_id: Option<String>,
    /// Unix millis.
    pub created_at: i64,
}

/// Provenance recorded at task-create time: who *filed* this task.
///
/// Both fields are write-once and immutable after creation —
/// reparenting (#751) does not modify them; the semantics are "where
/// this task was originally filed from." See [`Task::filed_by_project`]
/// and [`Task::filed_by_session_id`] for the read side.
///
/// Both fields are optional: pre-#758 callers (and tests that don't
/// care) pass [`FiledBy::default()`].
#[derive(Debug, Clone, Default)]
pub struct FiledBy<'a> {
    /// The *caller's* project at file-time. Distinct from the task's
    /// target `project_name`. Equal for same-project filing,
    /// different for cross-project filing (see #750).
    pub project: Option<&'a str>,
    /// Session id of the caller that ran `task_create`.
    pub session_id: Option<&'a str>,
}

/// Fields that can be updated on a task.
#[derive(Debug, Clone, Default)]
pub struct TaskUpdate {
    pub title: Option<String>,
    pub state: Option<TaskState>,
    pub priority: Option<i64>,
    pub tags: Option<serde_json::Value>,
    pub affected_files: Option<serde_json::Value>,
    pub skip_review: Option<bool>,
    pub require_approval: Option<bool>,
    pub merge_target: Option<String>,
    pub sandbox_profile: Option<String>,
    pub held: Option<bool>,
    /// Reparent the task to a different project. The handler layer is
    /// responsible for safety checks (no branch/worktree, schedulable
    /// state) and for resolving the name against `ProjectResolver`. At
    /// this layer we just write the column.
    pub project_name: Option<String>,
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

/// Column list for all `SELECT ... FROM tasks` queries that feed into
/// [`row_to_task`]. The order here must match `row_to_task`'s column-index
/// accesses — tests exercise `row_to_task` via column ordinals, so any drift
/// will surface there.
const TASK_COLUMNS: &str = "id, project_name, title, state, priority, \
    parent_id, tags, affected_files, branch, merge_target, worktree_path, \
    session_id, skip_review, require_approval, sandbox_profile, held, \
    placeholder_session_id, auto_downgraded_from_ready, \
    filed_by_project, filed_by_session_id, created_at, \
    updated_at";

// The canonical valid-state set and transition predicates now live in
// `crate::tasks_state` as exhaustive enum matches.  They are imported
// at the top of this file and used throughout `update_task`, `assign_task`,
// and the scheduler.  See the module docs and
// [`crate::tasks_state::validate_transition`] for the state-machine
// specification.

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
    project_name TEXT NOT NULL,
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
    held INTEGER NOT NULL DEFAULT 0,
    placeholder_session_id TEXT,
    auto_downgraded_from_ready INTEGER NOT NULL DEFAULT 0,
    filed_by_project TEXT,
    filed_by_session_id TEXT,
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

CREATE INDEX IF NOT EXISTS idx_tasks_project_state ON tasks(project_name, state);
CREATE INDEX IF NOT EXISTS idx_tasks_parent ON tasks(parent_id);
CREATE INDEX IF NOT EXISTS idx_task_messages_task ON task_messages(task_id);
CREATE INDEX IF NOT EXISTS idx_task_history_task ON task_history(task_id);
";

pub struct TasksDb {
    pub(crate) conn: Connection,
}

impl TasksDb {
    /// Open (or create) the database at the default path.
    pub fn open_default() -> tau_agent_plugin::Result<Self> {
        let path = default_db_path();
        Self::open(&path)
    }

    /// Open (or create) the database at the given path.
    pub fn open(path: &PathBuf) -> tau_agent_plugin::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                tau_agent_plugin::Error::Io(format!("mkdir {}: {}", parent.display(), e))
            })?;
        }
        let conn = Connection::open(path).map_err(|e| {
            tau_agent_plugin::Error::Io(format!("open tasks db {}: {}", path.display(), e))
        })?;

        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")
            .map_err(plugin_io_err("pragma"))?;

        conn.execute_batch(SCHEMA)
            .map_err(plugin_io_err("create tables"))?;

        Self::migrate(&conn)?;

        Ok(Self { conn })
    }

    /// Open an in-memory database (for tests).
    #[cfg(test)]
    pub fn open_memory() -> tau_agent_plugin::Result<Self> {
        let conn =
            Connection::open_in_memory().map_err(plugin_io_err("open in-memory tasks db"))?;

        conn.execute_batch("PRAGMA foreign_keys=ON;")
            .map_err(plugin_io_err("pragma"))?;

        conn.execute_batch(SCHEMA)
            .map_err(plugin_io_err("create tables"))?;

        Self::migrate(&conn)?;

        Ok(Self { conn })
    }

    /// Run schema migrations. Currently handles:
    /// - Dropping the `assigned_session` column (consolidated into `session_id`).
    /// - Dropping the `skip_planning` column (replaced by `task_create`'s
    ///   explicit `initial_state` argument; see task #512).
    fn migrate(conn: &Connection) -> tau_agent_plugin::Result<()> {
        // Rename project → project_name if the old column still exists.
        let has_old_project: bool = conn
            .prepare("SELECT COUNT(*) FROM pragma_table_info('tasks') WHERE name = 'project'")
            .and_then(|mut stmt| stmt.query_row([], |row| row.get::<_, i64>(0)))
            .map(|count| count > 0)
            .unwrap_or(false);

        if has_old_project {
            conn.execute_batch(
                "ALTER TABLE tasks RENAME COLUMN project TO project_name; \
                 DROP INDEX IF EXISTS idx_tasks_project_state; \
                 CREATE INDEX IF NOT EXISTS idx_tasks_project_state ON tasks(project_name, state);",
            )
            .map_err(plugin_io_err("migrate project -> project_name"))?;
        }

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
            .map_err(plugin_io_err("migrate assigned_session"))?;
        }

        // Drop the legacy `skip_planning` column if an older schema still
        // has it. Task #512 replaced the column with the `initial_state`
        // argument on `task_create`; fresh DBs never create it (see SCHEMA
        // above), so the `if has_skip_planning` guard makes this a no-op
        // on new installs.
        let has_skip_planning: bool = conn
            .prepare("SELECT COUNT(*) FROM pragma_table_info('tasks') WHERE name = 'skip_planning'")
            .and_then(|mut stmt| stmt.query_row([], |row| row.get::<_, i64>(0)))
            .map(|count| count > 0)
            .unwrap_or(false);

        if has_skip_planning {
            conn.execute_batch("ALTER TABLE tasks DROP COLUMN skip_planning;")
                .map_err(plugin_io_err("migrate drop skip_planning"))?;
        }

        // Add require_approval column if it doesn't exist.
        let has_require_approval: bool = conn
            .prepare(
                "SELECT COUNT(*) FROM pragma_table_info('tasks') WHERE name = 'require_approval'",
            )
            .and_then(|mut stmt| stmt.query_row([], |row| row.get::<_, i64>(0)))
            .map(|count| count > 0)
            .unwrap_or(false);

        if !has_require_approval {
            conn.execute_batch(
                "ALTER TABLE tasks ADD COLUMN require_approval INTEGER NOT NULL DEFAULT 0;",
            )
            .map_err(plugin_io_err("migrate require_approval"))?;
        }

        // Add merge_target column if it doesn't exist.
        let has_merge_target: bool = conn
            .prepare("SELECT COUNT(*) FROM pragma_table_info('tasks') WHERE name = 'merge_target'")
            .and_then(|mut stmt| stmt.query_row([], |row| row.get::<_, i64>(0)))
            .map(|count| count > 0)
            .unwrap_or(false);

        if !has_merge_target {
            conn.execute_batch("ALTER TABLE tasks ADD COLUMN merge_target TEXT;")
                .map_err(plugin_io_err("migrate merge_target"))?;
        }

        // Add sandbox_profile column if it doesn't exist.
        let has_sandbox_profile: bool = conn
            .prepare(
                "SELECT COUNT(*) FROM pragma_table_info('tasks') WHERE name = 'sandbox_profile'",
            )
            .and_then(|mut stmt| stmt.query_row([], |row| row.get::<_, i64>(0)))
            .map(|count| count > 0)
            .unwrap_or(false);

        if !has_sandbox_profile {
            conn.execute_batch("ALTER TABLE tasks ADD COLUMN sandbox_profile TEXT;")
                .map_err(plugin_io_err("migrate sandbox_profile"))?;
        }

        // Add held column if it doesn't exist. Introduced by task #527 to
        // let callers batch-seed a task board without racing the scheduler.
        let has_held: bool = conn
            .prepare("SELECT COUNT(*) FROM pragma_table_info('tasks') WHERE name = 'held'")
            .and_then(|mut stmt| stmt.query_row([], |row| row.get::<_, i64>(0)))
            .map(|count| count > 0)
            .unwrap_or(false);

        if !has_held {
            conn.execute_batch("ALTER TABLE tasks ADD COLUMN held INTEGER NOT NULL DEFAULT 0;")
                .map_err(plugin_io_err("migrate held"))?;
        }

        // Add placeholder_session_id column if it doesn't exist.
        // Introduced by task #561: every new task gets a non-LLM parent
        // session that hosts all task-related sub-sessions (planner,
        // worker, reviewer, merge, …). Existing (in-flight) tasks get
        // NULL and fall back to the legacy parenting rule.
        let has_placeholder_session_id: bool = conn
            .prepare(
                "SELECT COUNT(*) FROM pragma_table_info('tasks') WHERE name = 'placeholder_session_id'",
            )
            .and_then(|mut stmt| stmt.query_row([], |row| row.get::<_, i64>(0)))
            .map(|count| count > 0)
            .unwrap_or(false);

        if !has_placeholder_session_id {
            conn.execute_batch("ALTER TABLE tasks ADD COLUMN placeholder_session_id TEXT;")
                .map_err(plugin_io_err("migrate placeholder_session_id"))?;
        }

        // Add auto_downgraded_from_ready column if it doesn't exist.
        // Introduced by task #596: when a caller files a task with
        // `initial_state = ready` but no `affected_files`, we route the
        // task through planning to populate the file list (so the
        // scheduler can run it in parallel with disjoint tasks). The
        // flag tells the planner prompt builder to emit a focused
        // "only populate affected_files, then transition to ready"
        // section instead of the standard planning prompt.
        let has_auto_downgraded: bool = conn
            .prepare(
                "SELECT COUNT(*) FROM pragma_table_info('tasks') WHERE name = 'auto_downgraded_from_ready'",
            )
            .and_then(|mut stmt| stmt.query_row([], |row| row.get::<_, i64>(0)))
            .map(|count| count > 0)
            .unwrap_or(false);

        if !has_auto_downgraded {
            conn.execute_batch(
                "ALTER TABLE tasks ADD COLUMN auto_downgraded_from_ready INTEGER NOT NULL DEFAULT 0;",
            )
            .map_err(|e| {
                tau_agent_plugin::Error::Io(format!(
                    "migrate auto_downgraded_from_ready: {}",
                    e
                ))
            })?;
        }

        // Add filed_by_project / filed_by_session_id columns if they
        // don't exist. Introduced by task #758: every new task records
        // *where it was filed from* (the caller's project + session id)
        // separately from `project_name` (the project the task
        // targets). Pre-#758 rows get NULL for both — we don't
        // backfill historical provenance from session ancestry.
        let has_filed_by_project: bool = conn
            .prepare(
                "SELECT COUNT(*) FROM pragma_table_info('tasks') WHERE name = 'filed_by_project'",
            )
            .and_then(|mut stmt| stmt.query_row([], |row| row.get::<_, i64>(0)))
            .map(|count| count > 0)
            .unwrap_or(false);

        if !has_filed_by_project {
            conn.execute_batch("ALTER TABLE tasks ADD COLUMN filed_by_project TEXT;")
                .map_err(plugin_io_err("migrate filed_by_project"))?;
        }

        let has_filed_by_session_id: bool = conn
            .prepare(
                "SELECT COUNT(*) FROM pragma_table_info('tasks') WHERE name = 'filed_by_session_id'",
            )
            .and_then(|mut stmt| stmt.query_row([], |row| row.get::<_, i64>(0)))
            .map(|count| count > 0)
            .unwrap_or(false);

        if !has_filed_by_session_id {
            conn.execute_batch("ALTER TABLE tasks ADD COLUMN filed_by_session_id TEXT;")
                .map_err(plugin_io_err("migrate filed_by_session_id"))?;
        }

        // Migrate done -> merged/closed terminal states.
        let has_done: bool = conn
            .prepare("SELECT COUNT(*) FROM tasks WHERE state = 'done'")
            .and_then(|mut stmt| stmt.query_row([], |row| row.get::<_, i64>(0)))
            .map(|count| count > 0)
            .unwrap_or(false);

        if has_done {
            conn.execute_batch(
                "UPDATE tasks SET state = 'merged' WHERE state = 'done' AND id IN (
                    SELECT DISTINCT task_id FROM task_history WHERE field = 'state' AND new_value = 'merging'
                );
                UPDATE tasks SET state = 'closed' WHERE state = 'done';",
            )
            .map_err(plugin_io_err("migrate done to merged/closed"))?;
        }

        Ok(())
    }

    // ----- tasks -----

    /// Create a new task. Returns the created task.
    ///
    /// `initial_state` selects the starting state and must be one of
    /// `"interactive"`, `"planning"`, or `"ready"`. The choice applies
    /// uniformly to top-level tasks and subtasks — there is no automatic
    /// parent-based divergence anymore (see task #512).
    #[allow(clippy::too_many_arguments)]
    pub fn create_task(
        &self,
        project_name: &str,
        title: &str,
        priority: Option<i64>,
        parent_id: Option<i64>,
        tags: Option<&serde_json::Value>,
        skip_review: bool,
        initial_state: &str,
        require_approval: bool,
        merge_target: Option<&str>,
        sandbox_profile: Option<&str>,
        held: bool,
        affected_files: Option<&serde_json::Value>,
        auto_downgraded_from_ready: bool,
        filed_by: FiledBy<'_>,
    ) -> tau_agent_plugin::Result<Task> {
        let now = tau_agent_plugin::timestamp_ms() as i64;
        let priority = priority.unwrap_or(0);
        let tags_str = tags
            .map(serde_json::to_string)
            .transpose()
            .map_err(|e| tau_agent_plugin::Error::Parse(e.to_string()))?;
        let affected_files_str = affected_files
            .map(serde_json::to_string)
            .transpose()
            .map_err(|e| tau_agent_plugin::Error::Parse(e.to_string()))?;

        // Validate initial_state and apply uniformly regardless of parent_id.
        // Only `interactive`, `planning`, and `ready` are valid initial
        // states — the rest of the state machine is reachable only via
        // `update_task`'s validated transitions.
        let parsed = TaskState::from_db_str(initial_state).map_err(|_| {
            tau_agent_plugin::Error::Parse(format!(
                "invalid initial_state '{}': expected 'interactive', 'planning', or 'ready'",
                initial_state
            ))
        })?;
        let default_state = match parsed {
            TaskState::Interactive | TaskState::Planning | TaskState::Ready => parsed,
            _ => {
                return Err(tau_agent_plugin::Error::Parse(format!(
                    "invalid initial_state '{}': expected 'interactive', 'planning', or 'ready'",
                    initial_state
                )));
            }
        };

        self.conn
            .execute(
                "INSERT INTO tasks (project_name, title, state, priority, parent_id, tags, affected_files, skip_review, require_approval, merge_target, sandbox_profile, held, auto_downgraded_from_ready, filed_by_project, filed_by_session_id, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
                params![
                    project_name,
                    title,
                    default_state,
                    priority,
                    parent_id,
                    tags_str,
                    affected_files_str,
                    skip_review as i32,
                    require_approval as i32,
                    merge_target,
                    sandbox_profile,
                    held as i32,
                    auto_downgraded_from_ready as i32,
                    filed_by.project,
                    filed_by.session_id,
                    now,
                    now,
                ],
            )
            .map_err(plugin_io_err("insert task"))?;

        let id = self.conn.last_insert_rowid();
        self.get_task(id)?
            .ok_or_else(|| tau_agent_plugin::Error::Io("task not found after insert".into()))
    }

    /// Get a task by ID.
    pub fn get_task(&self, id: i64) -> tau_agent_plugin::Result<Option<Task>> {
        self.conn
            .query_row(
                &format!("SELECT {} FROM tasks WHERE id = ?1", TASK_COLUMNS),
                params![id],
                row_to_task,
            )
            .optional()
            .map_err(plugin_io_err("get task"))
    }

    /// List tasks with optional filters.
    pub fn list_tasks(
        &self,
        project_name: &str,
        state_filter: Option<&str>,
        parent_id_filter: Option<i64>,
        tag_filter: Option<&str>,
        limit: Option<i64>,
    ) -> tau_agent_plugin::Result<Vec<Task>> {
        let mut sql = format!("SELECT {} FROM tasks WHERE project_name = ?1", TASK_COLUMNS);
        let mut param_idx = 2;
        let mut params_vec: Vec<Box<dyn rusqlite::types::ToSql>> =
            vec![Box::new(project_name.to_string())];

        if let Some(state) = state_filter {
            if state != "all" {
                sql.push_str(&format!(" AND state = ?{}", param_idx));
                params_vec.push(Box::new(state.to_string()));
                param_idx += 1;
            }
            // "all" — no state filter, include merged/closed tasks
        } else {
            sql.push_str(" AND state NOT IN ('merged', 'closed')");
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
            .map_err(plugin_io_err("prepare list tasks"))?;

        let rows = stmt
            .query_map(params_refs.as_slice(), row_to_task)
            .map_err(plugin_io_err("list tasks"))?;

        let mut tasks = Vec::new();
        for row in rows {
            tasks.push(row.map_err(plugin_io_err("read task row"))?);
        }
        Ok(tasks)
    }

    /// List the most recently-updated tasks in a terminal state.
    ///
    /// Intended for the task-overview "recently completed" tail: pass
    /// `state = "merged"` or `state = "closed"` and a small `limit`.
    /// Rows are ordered by `updated_at DESC` (newest first).
    pub fn list_recent_by_state(
        &self,
        project_name: &str,
        state: &str,
        limit: usize,
    ) -> tau_agent_plugin::Result<Vec<Task>> {
        let sql = format!(
            "SELECT {} FROM tasks
                   WHERE project_name = ?1 AND state = ?2
                   ORDER BY updated_at DESC, id DESC
                   LIMIT ?3",
            TASK_COLUMNS
        );

        let mut stmt = self
            .conn
            .prepare(&sql)
            .map_err(plugin_io_err("prepare list_recent_by_state"))?;
        let rows = stmt
            .query_map(params![project_name, state, limit as i64], row_to_task)
            .map_err(plugin_io_err("list_recent_by_state"))?;

        let mut tasks = Vec::with_capacity(limit);
        for row in rows {
            tasks.push(row.map_err(plugin_io_err("read task row"))?);
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
    ) -> tau_agent_plugin::Result<Task> {
        let task = self
            .get_task(id)?
            .ok_or_else(|| tau_agent_plugin::Error::Io(format!("task {} not found", id)))?;

        // Validate state transition
        if let Some(new_state) = update.state {
            if !validate_transition(task.state, new_state) {
                return Err(tau_agent_plugin::Error::Io(format!(
                    "invalid state transition: {} -> {}",
                    task.state, new_state
                )));
            }
            // active -> approved requires skip_review=true
            if task.state == TaskState::Active
                && new_state == TaskState::Approved
                && !task.skip_review
            {
                return Err(tau_agent_plugin::Error::Io(
                    "cannot transition active -> approved: skip_review is false, \
                     must go through review first"
                        .into(),
                ));
            }
            // refining -> ready requires non-empty affected_files
            if task.state == TaskState::Refining && new_state == TaskState::Ready {
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
                    return Err(tau_agent_plugin::Error::Io(
                        "cannot transition refining -> ready: affected_files must be \
                         set and non-empty before a task can proceed to ready"
                            .into(),
                    ));
                }
            }
        }

        let now = tau_agent_plugin::timestamp_ms() as i64;

        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(plugin_io_err("update_task begin"))?;

        // Build SET clauses and record history
        let mut sets = vec!["updated_at = ?".to_string()];
        let mut params_vec: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(now)];

        macro_rules! update_field {
            ($field:ident, $col:expr, $old_val:expr) => {
                if let Some(ref val) = update.$field {
                    let old_str = $old_val;
                    let new_str = val.to_string();
                    params_vec.push(Box::new(val.clone()));
                    sets.push(format!("{} = ?", $col));
                    tx.execute(
                        "INSERT INTO task_history (task_id, field, old_value, new_value, session_id, created_at)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                        params![id, $col, old_str, new_str, session_id, now],
                    )
                    .map_err(plugin_io_err("insert history"))?;
                }
            };
        }

        update_field!(title, "title", Some(task.title.clone()));
        update_field!(state, "state", Some(task.state.as_str().to_string()));
        update_field!(priority, "priority", Some(task.priority.to_string()));

        // Phase-completing transitions clear `tasks.session_id` so the
        // scheduler never sees a stale reference to a session that has
        // already finished its phase.  See
        // [`should_clear_session_id_on_transition`] for the rationale and
        // the exhaustive list of transitions that trigger the clear.
        // Task #577 — companion to #572.
        if let Some(new_state) = update.state
            && should_clear_session_id_on_transition(task.state, new_state)
            && task.session_id.is_some()
        {
            let old_sid = task.session_id.clone();
            sets.push("session_id = NULL".to_string());
            tx.execute(
                "INSERT INTO task_history (task_id, field, old_value, new_value, session_id, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![id, "session_id", old_sid, Option::<String>::None, session_id, now],
            )
            .map_err(plugin_io_err("insert history"))?;
        }

        if let Some(ref val) = update.tags {
            let old_str = task.tags.as_ref().map(|v| v.to_string());
            let new_str = val.to_string();
            let json_str = serde_json::to_string(val)
                .map_err(|e| tau_agent_plugin::Error::Parse(e.to_string()))?;
            params_vec.push(Box::new(json_str));
            sets.push("tags = ?".to_string());
            tx.execute(
                "INSERT INTO task_history (task_id, field, old_value, new_value, session_id, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![id, "tags", old_str, new_str, session_id, now],
            )
            .map_err(plugin_io_err("insert history"))?;
        }

        if let Some(ref val) = update.affected_files {
            let old_str = task.affected_files.as_ref().map(|v| v.to_string());
            let new_str = val.to_string();
            let json_str = serde_json::to_string(val)
                .map_err(|e| tau_agent_plugin::Error::Parse(e.to_string()))?;
            params_vec.push(Box::new(json_str));
            sets.push("affected_files = ?".to_string());
            tx.execute(
                "INSERT INTO task_history (task_id, field, old_value, new_value, session_id, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![id, "affected_files", old_str, new_str, session_id, now],
            )
            .map_err(plugin_io_err("insert history"))?;
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
            .map_err(plugin_io_err("insert history"))?;
        }

        if let Some(val) = update.require_approval {
            let old_str = Some(task.require_approval.to_string());
            let new_str = val.to_string();
            params_vec.push(Box::new(val as i32));
            sets.push("require_approval = ?".to_string());
            tx.execute(
                "INSERT INTO task_history (task_id, field, old_value, new_value, session_id, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![id, "require_approval", old_str, new_str, session_id, now],
            )
            .map_err(plugin_io_err("insert history"))?;
        }

        if let Some(val) = update.held {
            let old_str = Some(task.held.to_string());
            let new_str = val.to_string();
            params_vec.push(Box::new(val as i32));
            sets.push("held = ?".to_string());
            tx.execute(
                "INSERT INTO task_history (task_id, field, old_value, new_value, session_id, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![id, "held", old_str, new_str, session_id, now],
            )
            .map_err(plugin_io_err("insert history"))?;
        }

        if let Some(ref val) = update.merge_target {
            let old_str = task.merge_target.clone();
            let new_str = val.to_string();
            params_vec.push(Box::new(val.clone()));
            sets.push("merge_target = ?".to_string());
            tx.execute(
                "INSERT INTO task_history (task_id, field, old_value, new_value, session_id, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![id, "merge_target", old_str, new_str, session_id, now],
            )
            .map_err(plugin_io_err("insert history"))?;
        }

        if let Some(ref val) = update.sandbox_profile {
            let old_str = task.sandbox_profile.clone();
            let new_str = val.clone();
            params_vec.push(Box::new(val.clone()));
            sets.push("sandbox_profile = ?".to_string());
            tx.execute(
                "INSERT INTO task_history (task_id, field, old_value, new_value, session_id, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![id, "sandbox_profile", old_str, new_str, session_id, now],
            )
            .map_err(plugin_io_err("insert history"))?;
        }

        // Reparenting (task #751): the handler validates that the task
        // has no branch/worktree and is in a schedulable state before
        // setting this; here we only mirror the column update + history
        // insert pattern used by every other field.
        if let Some(ref val) = update.project_name {
            let old_str = Some(task.project_name.clone());
            let new_str = val.clone();
            params_vec.push(Box::new(val.clone()));
            sets.push("project_name = ?".to_string());
            tx.execute(
                "INSERT INTO task_history (task_id, field, old_value, new_value, session_id, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![id, "project_name", old_str, new_str, session_id, now],
            )
            .map_err(plugin_io_err("insert history"))?;
        }

        if sets.len() == 1 {
            // Only updated_at, nothing else to update
            tx.commit().map_err(plugin_io_err("update_task commit"))?;
            return self
                .get_task(id)?
                .ok_or_else(|| tau_agent_plugin::Error::Io("task not found after update".into()));
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
            .map_err(plugin_io_err("update task"))?;

        tx.commit().map_err(plugin_io_err("update_task commit"))?;

        self.get_task(id)?
            .ok_or_else(|| tau_agent_plugin::Error::Io("task not found after update".into()))
    }

    // ----- messages -----

    /// Add a message to a task.
    pub fn add_message(
        &self,
        task_id: i64,
        content: &str,
        author: Option<&str>,
    ) -> tau_agent_plugin::Result<TaskMessage> {
        // Verify task exists
        self.get_task(task_id)?
            .ok_or_else(|| tau_agent_plugin::Error::Io(format!("task {} not found", task_id)))?;

        let now = tau_agent_plugin::timestamp_ms() as i64;
        self.conn
            .execute(
                "INSERT INTO task_messages (task_id, content, author, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![task_id, content, author, now, now],
            )
            .map_err(plugin_io_err("insert message"))?;

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
    pub fn edit_message(
        &self,
        message_id: i64,
        content: &str,
    ) -> tau_agent_plugin::Result<TaskMessage> {
        let now = tau_agent_plugin::timestamp_ms() as i64;

        let updated = self
            .conn
            .execute(
                "UPDATE task_messages SET content = ?1, updated_at = ?2 WHERE id = ?3",
                params![content, now, message_id],
            )
            .map_err(plugin_io_err("edit message"))?;

        if updated == 0 {
            return Err(tau_agent_plugin::Error::Io(format!(
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
            .map_err(plugin_io_err("get edited message"))
    }

    /// Get all messages for a task.
    pub fn get_messages(&self, task_id: i64) -> tau_agent_plugin::Result<Vec<TaskMessage>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, task_id, content, author, created_at, updated_at
                 FROM task_messages WHERE task_id = ?1 ORDER BY id",
            )
            .map_err(plugin_io_err("prepare get messages"))?;

        let rows = stmt
            .query_map(params![task_id], row_to_message)
            .map_err(plugin_io_err("get messages"))?;

        let mut messages = Vec::new();
        for row in rows {
            messages.push(row.map_err(plugin_io_err("read message row"))?);
        }
        Ok(messages)
    }

    // ----- relations -----

    /// Add a relation between two tasks. Validates both exist, are in the
    /// same project, are not self-referential, and (for `depends_on`) do not
    /// create a cycle.
    pub fn add_relation(
        &self,
        from_task: i64,
        to_task: i64,
        relation: &str,
    ) -> tau_agent_plugin::Result<()> {
        // Validate relation type
        if !matches!(relation, "depends_on" | "blocks" | "related") {
            return Err(tau_agent_plugin::Error::Io(format!(
                "invalid relation type: {}. Must be depends_on, blocks, or related",
                relation
            )));
        }

        // Prevent self-referential relations
        if from_task == to_task {
            return Err(tau_agent_plugin::Error::Io(
                "cannot create a relation from a task to itself".into(),
            ));
        }

        // Use IMMEDIATE transaction so the cycle check + insert are atomic.
        // This prevents a concurrent process from inserting a relation that
        // creates a cycle between our check and our insert.
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(plugin_io_err("add_relation begin"))?;

        // Validate both tasks exist (cross-project relations are allowed)
        tx.query_row(
            "SELECT 1 FROM tasks WHERE id = ?1",
            params![from_task],
            |_row| Ok(()),
        )
        .optional()
        .map_err(plugin_io_err("check from_task"))?
        .ok_or_else(|| tau_agent_plugin::Error::Io(format!("from_task {} not found", from_task)))?;

        tx.query_row(
            "SELECT 1 FROM tasks WHERE id = ?1",
            params![to_task],
            |_row| Ok(()),
        )
        .optional()
        .map_err(plugin_io_err("check to_task"))?
        .ok_or_else(|| tau_agent_plugin::Error::Io(format!("to_task {} not found", to_task)))?;

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
                    .map_err(plugin_io_err("prepare cycle check"))?;

                let deps: Vec<i64> = stmt
                    .query_map(params![current], |row| row.get(0))
                    .map_err(plugin_io_err("cycle check"))?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(plugin_io_err("read cycle check row"))?;

                for dep in deps {
                    if dep == from_task {
                        return Err(tau_agent_plugin::Error::Io(format!(
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
        .map_err(plugin_io_err("insert relation"))?;

        tx.commit().map_err(plugin_io_err("add_relation commit"))?;

        Ok(())
    }

    /// Get all relations involving a task (from or to).
    pub fn get_relations(&self, task_id: i64) -> tau_agent_plugin::Result<Vec<TaskRelation>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT from_task, to_task, relation FROM task_relations
                 WHERE from_task = ?1 OR to_task = ?1",
            )
            .map_err(plugin_io_err("prepare get relations"))?;

        let rows = stmt
            .query_map(params![task_id], |row| {
                Ok(TaskRelation {
                    from_task: row.get(0)?,
                    to_task: row.get(1)?,
                    relation: row.get(2)?,
                })
            })
            .map_err(plugin_io_err("get relations"))?;

        let mut relations = Vec::new();
        for row in rows {
            relations.push(row.map_err(plugin_io_err("read relation row"))?);
        }
        Ok(relations)
    }

    /// Get tasks that this task depends on that are NOT yet in a terminal state.
    /// Returns tasks where: relation(this_task, dep, 'depends_on') AND dep.state NOT IN ('merged', 'closed')
    pub fn get_blocking_dependencies(&self, task_id: i64) -> tau_agent_plugin::Result<Vec<Task>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT t.id, t.project_name, t.title, t.state, t.priority, t.parent_id,
                        t.tags, t.affected_files, t.branch, t.merge_target,
                        t.worktree_path, t.session_id, t.skip_review, t.require_approval, t.sandbox_profile, t.held, t.placeholder_session_id, t.auto_downgraded_from_ready, t.filed_by_project, t.filed_by_session_id, t.created_at,
                        t.updated_at
                 FROM task_relations r
                 JOIN tasks t ON t.id = r.to_task
                 WHERE r.from_task = ?1 AND r.relation = 'depends_on' AND t.state NOT IN ('merged', 'closed')",
            )
            .map_err(plugin_io_err("prepare get_blocking_dependencies"))?;

        let rows = stmt
            .query_map(params![task_id], row_to_task)
            .map_err(plugin_io_err("get_blocking_dependencies"))?;

        let mut tasks = Vec::new();
        for row in rows {
            tasks.push(row.map_err(plugin_io_err("read blocking dependency row"))?);
        }
        Ok(tasks)
    }

    /// Get tasks that are ready or in planning state AND have no unfinished
    /// dependencies.
    ///
    /// Planning-state tasks are included so the scheduler can dispatch
    /// planning sessions for them (without creating worktrees).
    pub fn get_schedulable_tasks(&self, project_name: &str) -> tau_agent_plugin::Result<Vec<Task>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT t.id, t.project_name, t.title, t.state, t.priority, t.parent_id,
                        t.tags, t.affected_files, t.branch, t.merge_target,
                        t.worktree_path, t.session_id, t.skip_review, t.require_approval, t.sandbox_profile, t.held, t.placeholder_session_id, t.auto_downgraded_from_ready, t.filed_by_project, t.filed_by_session_id, t.created_at,
                        t.updated_at
                 FROM tasks t
                 WHERE t.project_name = ?1 AND t.state IN ('ready', 'planning')
                   AND NOT t.held
                   AND NOT EXISTS (
                       SELECT 1 FROM task_relations r
                       JOIN tasks dep ON dep.id = r.to_task
                       WHERE r.from_task = t.id
                         AND r.relation = 'depends_on'
                         AND dep.state NOT IN ('merged', 'closed')
                   )
                 ORDER BY t.priority DESC, t.created_at ASC",
            )
            .map_err(plugin_io_err("prepare get_schedulable_tasks"))?;

        let rows = stmt
            .query_map(params![project_name], row_to_task)
            .map_err(plugin_io_err("get_schedulable_tasks"))?;

        let mut tasks = Vec::new();
        for row in rows {
            tasks.push(row.map_err(plugin_io_err("read schedulable task row"))?);
        }
        Ok(tasks)
    }

    /// Get all tasks in `approved` state, optionally filtered by project.
    /// Used by the scheduler to find tasks ready for auto-merge.
    pub fn get_approved_tasks(
        &self,
        project_name: Option<&str>,
    ) -> tau_agent_plugin::Result<Vec<Task>> {
        let (sql, params_vec): (String, Vec<Box<dyn rusqlite::types::ToSql>>) = match project_name {
            Some(p) => (
                format!(
                    "SELECT {} FROM tasks
                 WHERE state = 'approved' AND project_name = ?1
                 ORDER BY priority DESC, created_at ASC",
                    TASK_COLUMNS
                ),
                vec![Box::new(p.to_string())],
            ),
            None => (
                format!(
                    "SELECT {} FROM tasks
                 WHERE state = 'approved'
                 ORDER BY priority DESC, created_at ASC",
                    TASK_COLUMNS
                ),
                vec![],
            ),
        };

        let params_refs: Vec<&dyn rusqlite::types::ToSql> =
            params_vec.iter().map(|p| p.as_ref()).collect();
        let mut stmt = self
            .conn
            .prepare(&sql)
            .map_err(plugin_io_err("prepare get_approved_tasks"))?;

        let rows = stmt
            .query_map(params_refs.as_slice(), row_to_task)
            .map_err(plugin_io_err("get_approved_tasks"))?;

        let mut tasks = Vec::new();
        for row in rows {
            tasks.push(row.map_err(plugin_io_err("read approved task row"))?);
        }
        Ok(tasks)
    }

    /// Count tasks in in-flight states that are actually consuming resources.
    /// Tasks in active, review, merging, or refining always count. Planning
    /// tasks only count if they have an assigned session (i.e. a planner is
    /// actively running). Idle planning tasks without sessions are just
    /// queued and should not block the budget.
    pub fn count_inflight_tasks(&self, project_name: &str) -> tau_agent_plugin::Result<usize> {
        let count: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM tasks
                 WHERE project_name = ?1
                   AND (state IN ('refining', 'active', 'review', 'merging')
                        OR (state = 'planning' AND session_id IS NOT NULL))",
                params![project_name],
                |row| row.get(0),
            )
            .map_err(plugin_io_err("count_inflight_tasks"))?;
        Ok(count as usize)
    }

    /// Get all tasks in in-flight states (active, review, merging, refining).
    /// Used by the scheduler to check affected_files conflicts against
    /// already-active tasks before dispatching new ones.
    pub fn get_inflight_tasks(&self, project_name: &str) -> tau_agent_plugin::Result<Vec<Task>> {
        let mut stmt = self
            .conn
            .prepare(&format!(
                "SELECT {} FROM tasks
                 WHERE project_name = ?1
                   AND state IN ('active', 'review', 'merging', 'refining')
                 ORDER BY priority DESC, created_at ASC",
                TASK_COLUMNS
            ))
            .map_err(plugin_io_err("prepare get_inflight_tasks"))?;

        let rows = stmt
            .query_map(params![project_name], row_to_task)
            .map_err(plugin_io_err("get_inflight_tasks"))?;

        let mut tasks = Vec::new();
        for row in rows {
            tasks.push(row.map_err(plugin_io_err("read inflight task row"))?);
        }
        Ok(tasks)
    }

    /// Check if `from` transitively depends on `to` via `depends_on` relations.
    /// Uses BFS from `from` following depends_on edges. Returns true if `to` is
    /// reachable.
    pub fn has_transitive_dependency(&self, from: i64, to: i64) -> tau_agent_plugin::Result<bool> {
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
                .map_err(plugin_io_err("prepare has_transitive_dependency"))?;

            let deps: Vec<i64> = stmt
                .query_map(params![current], |row| row.get(0))
                .map_err(plugin_io_err("has_transitive_dependency"))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(plugin_io_err("read transitive dependency row"))?;

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
    pub fn get_subtasks(&self, parent_id: i64) -> tau_agent_plugin::Result<Vec<Task>> {
        let mut stmt = self
            .conn
            .prepare(&format!(
                "SELECT {} FROM tasks WHERE parent_id = ?1 ORDER BY priority DESC, created_at ASC",
                TASK_COLUMNS
            ))
            .map_err(plugin_io_err("prepare get subtasks"))?;

        let rows = stmt
            .query_map(params![parent_id], row_to_task)
            .map_err(plugin_io_err("get subtasks"))?;

        let mut tasks = Vec::new();
        for row in rows {
            tasks.push(row.map_err(plugin_io_err("read subtask row"))?);
        }
        Ok(tasks)
    }

    /// Get all descendant tasks (recursive subtree) of a task.
    ///
    /// Uses iterative BFS to collect all tasks whose parent chain leads back
    /// to `root_id`. Does NOT include the root task itself.
    pub fn get_descendant_tasks(&self, root_id: i64) -> tau_agent_plugin::Result<Vec<Task>> {
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
    pub fn assign_task(
        &self,
        task_id: i64,
        session_id: &str,
    ) -> tau_agent_plugin::Result<AssignResult> {
        let task = self
            .get_task(task_id)?
            .ok_or_else(|| tau_agent_plugin::Error::Io(format!("task {} not found", task_id)))?;

        if task.state != TaskState::Ready && task.state != TaskState::Interactive {
            return Err(tau_agent_plugin::Error::Io(format!(
                "cannot assign task {}: state is '{}', must be 'ready' or 'interactive'",
                task_id, task.state
            )));
        }

        let now = tau_agent_plugin::timestamp_ms() as i64;
        // Interactive tasks stay interactive; ready tasks transition to active
        let new_state = if task.state == TaskState::Interactive {
            TaskState::Interactive
        } else {
            TaskState::Active
        };

        let old_session_id = task.session_id.clone();
        let session_changed = old_session_id.as_deref() != Some(session_id);

        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(plugin_io_err("assign_task begin"))?;

        tx.execute(
            "UPDATE tasks SET state = ?1, session_id = ?2, updated_at = ?3 \
             WHERE id = ?4",
            params![new_state, session_id, now, task_id],
        )
        .map_err(plugin_io_err("assign task update"))?;

        // Record state change in history (only if state actually changed)
        if new_state != task.state {
            tx.execute(
                "INSERT INTO task_history (task_id, field, old_value, new_value, session_id, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![task_id, "state", task.state.as_str(), new_state.as_str(), session_id, now],
            )
            .map_err(plugin_io_err("assign task history (state)"))?;
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
        .map_err(plugin_io_err("assign task history (assigned)"))?;

        // Record in task_sessions
        tx.execute(
            "INSERT OR IGNORE INTO task_sessions (task_id, session_id, role, created_at) \
             VALUES (?1, ?2, 'worker', ?3)",
            params![task_id, session_id, now],
        )
        .map_err(plugin_io_err("assign task session"))?;

        // For interactive tasks with a changed session, update all descendant
        // tasks' session_id within this same transaction (atomic).
        let mut descendant_old_sessions = Vec::new();
        if task.state == TaskState::Interactive
            && session_changed
            && let Some(ref old_sid) = old_session_id
        {
            // Collect descendant task IDs using BFS via direct SQL within
            // the transaction (can't call self.get_subtasks inside tx).
            let descendant_ids = {
                let mut ids = Vec::new();
                let mut queue = std::collections::VecDeque::new();
                queue.push_back(task_id);
                while let Some(pid) = queue.pop_front() {
                    let mut stmt = tx
                        .prepare("SELECT id, session_id FROM tasks WHERE parent_id = ?1")
                        .map_err(plugin_io_err("prepare descendant query"))?;
                    let rows: Vec<(i64, Option<String>)> = stmt
                        .query_map(params![pid], |row| Ok((row.get(0)?, row.get(1)?)))
                        .map_err(plugin_io_err("query descendants"))?
                        .collect::<Result<Vec<_>, _>>()
                        .map_err(plugin_io_err("read descendant row"))?;
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
                        if let Some(ref cs) = child_session
                            && cs == old_sid
                        {
                            descendant_old_sessions.push(cs.clone());
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
                    tau_agent_plugin::Error::Io(format!(
                        "update descendant {} session: {}",
                        desc_id, e
                    ))
                })?;

                tx.execute(
                    "INSERT OR IGNORE INTO task_sessions (task_id, session_id, role, created_at) \
                         VALUES (?1, ?2, 'assigned', ?3)",
                    params![desc_id, session_id, now],
                )
                .map_err(|e| {
                    tau_agent_plugin::Error::Io(format!(
                        "record descendant {} session: {}",
                        desc_id, e
                    ))
                })?;
            }
        }

        tx.commit().map_err(plugin_io_err("assign_task commit"))?;

        let updated_task = self
            .get_task(task_id)?
            .ok_or_else(|| tau_agent_plugin::Error::Io("task not found after assign".into()))?;

        Ok(AssignResult {
            task: updated_task,
            old_session_id,
            descendant_old_sessions,
        })
    }

    // ----- session tracking -----

    /// Record a session's association with a task (idempotent — INSERT OR IGNORE).
    pub fn record_session(
        &self,
        task_id: i64,
        session_id: &str,
        role: &str,
    ) -> tau_agent_plugin::Result<()> {
        let now = tau_agent_plugin::timestamp_ms() as i64;
        self.conn
            .execute(
                "INSERT OR IGNORE INTO task_sessions (task_id, session_id, role, created_at) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![task_id, session_id, role, now],
            )
            .map_err(plugin_io_err("record session"))?;
        Ok(())
    }

    /// Return `(task_id, session_id)` for every row in `task_sessions` that
    /// belongs to a task in `project_name`.  Used by the server to compute
    /// a per-task "has a live session right now" flag by intersecting with
    /// `live_sessions` in shared state — cheaper than N per-task queries.
    pub fn list_project_task_sessions(
        &self,
        project_name: &str,
    ) -> tau_agent_plugin::Result<Vec<(i64, String)>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT ts.task_id, ts.session_id \
                 FROM task_sessions ts \
                 INNER JOIN tasks t ON t.id = ts.task_id \
                 WHERE t.project_name = ?1",
            )
            .map_err(plugin_io_err("prepare list_project_task_sessions"))?;

        let rows = stmt
            .query_map(params![project_name], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(plugin_io_err("list_project_task_sessions"))?;

        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(plugin_io_err("read project task session row"))?);
        }
        Ok(out)
    }

    /// Return every session id referenced by a task whose state is NOT in
    /// (`merged`, `closed`, `failed`).  The returned set is the union of:
    ///
    /// - `tasks.session_id`
    /// - `tasks.placeholder_session_id`
    /// - `task_sessions.session_id` (the historical join table)
    ///
    /// for non-terminal tasks across **all** projects (no project filter
    /// — the server's empty-session GC scans the whole sessions DB).
    ///
    /// **Terminal-state policy note.**  Most other queries in this file
    /// treat only `merged` and `closed` as terminal (e.g.
    /// `get_blocking_dependencies`, the `list_tasks` non-terminal
    /// filter).  This function additionally treats `failed` as terminal
    /// because a failed task is no longer an active worker — its
    /// session, if otherwise empty, is fair game for cleanup.  Future
    /// readers grepping for `state NOT IN ('merged','closed')` should be
    /// aware of this deliberate departure.
    pub fn list_protected_session_ids(
        &self,
    ) -> tau_agent_plugin::Result<std::collections::HashSet<String>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT session_id FROM tasks \
                 WHERE session_id IS NOT NULL \
                   AND state NOT IN ('merged', 'closed', 'failed') \
                 UNION \
                 SELECT placeholder_session_id FROM tasks \
                 WHERE placeholder_session_id IS NOT NULL \
                   AND state NOT IN ('merged', 'closed', 'failed') \
                 UNION \
                 SELECT ts.session_id FROM task_sessions ts \
                 INNER JOIN tasks t ON t.id = ts.task_id \
                 WHERE t.state NOT IN ('merged', 'closed', 'failed')",
            )
            .map_err(plugin_io_err("prepare list_protected_session_ids"))?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(plugin_io_err("list_protected_session_ids"))?;
        let mut out = std::collections::HashSet::new();
        for row in rows {
            out.insert(row.map_err(plugin_io_err("read protected session row"))?);
        }
        Ok(out)
    }

    /// Get all sessions for a task.
    pub fn get_sessions(&self, task_id: i64) -> tau_agent_plugin::Result<Vec<TaskSession>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT task_id, session_id, role, created_at \
                 FROM task_sessions WHERE task_id = ?1 ORDER BY created_at",
            )
            .map_err(plugin_io_err("prepare get sessions"))?;

        let rows = stmt
            .query_map(params![task_id], |row| {
                Ok(TaskSession {
                    task_id: row.get(0)?,
                    session_id: row.get(1)?,
                    role: row.get(2)?,
                    created_at: row.get(3)?,
                })
            })
            .map_err(plugin_io_err("get sessions"))?;

        let mut sessions = Vec::new();
        for row in rows {
            sessions.push(row.map_err(plugin_io_err("read session row"))?);
        }
        Ok(sessions)
    }

    /// Get the task history log for a task — every recorded update in
    /// chronological order (oldest first).  Limited to the most recent
    /// 200 entries to bound payload size; most tasks have far fewer.
    pub fn get_history(&self, task_id: i64) -> tau_agent_plugin::Result<Vec<TaskHistoryEntry>> {
        // Grab the most recent 200 entries (ORDER BY id DESC LIMIT 200), then
        // flip to chronological before returning.  `id` is monotonic per-row,
        // so this is equivalent to ORDER BY created_at in practice.
        let mut stmt = self
            .conn
            .prepare(
                "SELECT field, old_value, new_value, session_id, created_at \
                 FROM task_history WHERE task_id = ?1 \
                 ORDER BY id DESC LIMIT 200",
            )
            .map_err(plugin_io_err("prepare get history"))?;

        let rows = stmt
            .query_map(params![task_id], |row| {
                Ok(TaskHistoryEntry {
                    field: row.get(0)?,
                    old_value: row.get(1)?,
                    new_value: row.get(2)?,
                    session_id: row.get(3)?,
                    created_at: row.get(4)?,
                })
            })
            .map_err(plugin_io_err("get history"))?;

        let mut entries = Vec::new();
        for row in rows {
            entries.push(row.map_err(plugin_io_err("read history row"))?);
        }
        entries.reverse();
        Ok(entries)
    }

    /// Find the most recent session for a task with the given role.
    ///
    /// Returns the session_id of the most recently recorded session matching
    /// the specified role, or `None` if no such session exists.
    pub fn find_latest_session_by_role(
        &self,
        task_id: i64,
        role: &str,
    ) -> tau_agent_plugin::Result<Option<String>> {
        let result = self
            .conn
            .query_row(
                "SELECT session_id FROM task_sessions \
                 WHERE task_id = ?1 AND role = ?2 \
                 ORDER BY created_at DESC LIMIT 1",
                params![task_id, role],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(plugin_io_err("find latest session by role"))?;
        Ok(result)
    }

    // ----- search -----

    /// Search tasks by title and message content.
    pub fn search_tasks(
        &self,
        project_name: &str,
        query: &str,
        state_filter: Option<&str>,
    ) -> tau_agent_plugin::Result<Vec<Task>> {
        let like_query = format!("%{}%", query);
        let mut tasks = Vec::new();

        if let Some(state) = state_filter {
            let sql = "SELECT DISTINCT t.id, t.project_name, t.title, t.state, t.priority, t.parent_id,
                    t.tags, t.affected_files, t.branch, t.merge_target,
                    t.worktree_path, t.session_id, t.skip_review, t.require_approval, t.sandbox_profile, t.held, t.placeholder_session_id, t.auto_downgraded_from_ready, t.filed_by_project, t.filed_by_session_id, t.created_at, t.updated_at
             FROM tasks t
             LEFT JOIN task_messages m ON m.task_id = t.id
             WHERE t.project_name = ?1 AND t.state = ?2
               AND (t.title LIKE ?3 OR m.content LIKE ?3)
             ORDER BY t.priority DESC, t.created_at ASC";
            let mut stmt = self
                .conn
                .prepare(sql)
                .map_err(plugin_io_err("prepare search"))?;
            let rows = stmt
                .query_map(params![project_name, state, like_query], row_to_task)
                .map_err(plugin_io_err("search tasks"))?;
            for row in rows {
                tasks.push(row.map_err(plugin_io_err("read search row"))?);
            }
        } else {
            let sql = "SELECT DISTINCT t.id, t.project_name, t.title, t.state, t.priority, t.parent_id,
                    t.tags, t.affected_files, t.branch, t.merge_target,
                    t.worktree_path, t.session_id, t.skip_review, t.require_approval, t.sandbox_profile, t.held, t.placeholder_session_id, t.auto_downgraded_from_ready, t.filed_by_project, t.filed_by_session_id, t.created_at, t.updated_at
             FROM tasks t
             LEFT JOIN task_messages m ON m.task_id = t.id
             WHERE t.project_name = ?1
               AND (t.title LIKE ?2 OR m.content LIKE ?2)
             ORDER BY t.priority DESC, t.created_at ASC";
            let mut stmt = self
                .conn
                .prepare(sql)
                .map_err(plugin_io_err("prepare search"))?;
            let rows = stmt
                .query_map(params![project_name, like_query], row_to_task)
                .map_err(plugin_io_err("search tasks"))?;
            for row in rows {
                tasks.push(row.map_err(plugin_io_err("read search row"))?);
            }
        }

        Ok(tasks)
    }

    // ----- git integration -----

    /// Set the branch name for a task.
    pub fn set_branch(&self, task_id: i64, branch: &str) -> tau_agent_plugin::Result<()> {
        let now = tau_agent_plugin::timestamp_ms() as i64;
        let updated = self
            .conn
            .execute(
                "UPDATE tasks SET branch = ?1, updated_at = ?2 WHERE id = ?3",
                params![branch, now, task_id],
            )
            .map_err(plugin_io_err("set_branch"))?;

        if updated == 0 {
            return Err(tau_agent_plugin::Error::Io(format!(
                "task {} not found",
                task_id
            )));
        }
        Ok(())
    }

    /// Set the worktree path for a task.
    pub fn set_worktree_path(&self, task_id: i64, path: &str) -> tau_agent_plugin::Result<()> {
        let now = tau_agent_plugin::timestamp_ms() as i64;
        let updated = self
            .conn
            .execute(
                "UPDATE tasks SET worktree_path = ?1, updated_at = ?2 WHERE id = ?3",
                params![path, now, task_id],
            )
            .map_err(plugin_io_err("set_worktree_path"))?;

        if updated == 0 {
            return Err(tau_agent_plugin::Error::Io(format!(
                "task {} not found",
                task_id
            )));
        }
        Ok(())
    }

    /// Get the merge target branch for a task.
    ///
    /// Returns the parent task's branch name if the task has a parent,
    /// or `"main"` if it is a root task. Falls back to `"main"` if the
    /// parent has no branch set (e.g. interactive tasks).
    pub fn get_merge_target(&self, task_id: i64) -> tau_agent_plugin::Result<String> {
        let task = self
            .get_task(task_id)?
            .ok_or_else(|| tau_agent_plugin::Error::Io(format!("task {} not found", task_id)))?;

        // Explicit override wins
        if let Some(ref target) = task.merge_target {
            return Ok(target.clone());
        }

        // Default: parent's branch (subtask) or "main" (root)
        match task.parent_id {
            None => Ok("main".to_string()),
            Some(pid) => {
                let parent = self.get_task(pid)?.ok_or_else(|| {
                    tau_agent_plugin::Error::Io(format!("parent task {} not found", pid))
                })?;
                Ok(parent.branch.as_deref().unwrap_or("main").to_string())
            }
        }
    }

    /// Set the session_id for a task (the session working on it).
    pub fn set_session_id(&self, task_id: i64, session_id: &str) -> tau_agent_plugin::Result<()> {
        let now = tau_agent_plugin::timestamp_ms() as i64;
        let updated = self
            .conn
            .execute(
                "UPDATE tasks SET session_id = ?1, updated_at = ?2 WHERE id = ?3",
                params![session_id, now, task_id],
            )
            .map_err(plugin_io_err("set_session_id"))?;

        if updated == 0 {
            return Err(tau_agent_plugin::Error::Io(format!(
                "task {} not found",
                task_id
            )));
        }
        Ok(())
    }

    /// Clear the session_id for a task (set to NULL).
    pub fn clear_session_id(&self, task_id: i64) -> tau_agent_plugin::Result<()> {
        let now = tau_agent_plugin::timestamp_ms() as i64;
        let updated = self
            .conn
            .execute(
                "UPDATE tasks SET session_id = NULL, updated_at = ?1 WHERE id = ?2",
                params![now, task_id],
            )
            .map_err(plugin_io_err("clear_session_id"))?;

        if updated == 0 {
            return Err(tau_agent_plugin::Error::Io(format!(
                "task {} not found",
                task_id
            )));
        }
        Ok(())
    }

    /// Set the placeholder_session_id for a task — the sid of the task's
    /// non-LLM (`model = "log"`) placeholder session that parents every
    /// task-spawned session (planner, worker, reviewer, refiner, merge,
    /// …). See task #561.
    pub fn set_placeholder_session_id(
        &self,
        task_id: i64,
        session_id: &str,
    ) -> tau_agent_plugin::Result<()> {
        let now = tau_agent_plugin::timestamp_ms() as i64;
        let updated = self
            .conn
            .execute(
                "UPDATE tasks SET placeholder_session_id = ?1, updated_at = ?2 WHERE id = ?3",
                params![session_id, now, task_id],
            )
            .map_err(plugin_io_err("set_placeholder_session_id"))?;

        if updated == 0 {
            return Err(tau_agent_plugin::Error::Io(format!(
                "task {} not found",
                task_id
            )));
        }
        Ok(())
    }

    /// Find tasks that appear to be stuck: in `active` state with
    /// `session_id IS NULL` and `updated_at` older than `max_age_ms`
    /// milliseconds before `now_ms`.
    ///
    /// The task scheduler's watchdog uses this to detect the
    /// "scheduler prepared a task for dispatch but the worker session
    /// was never created" failure mode: `prepare_task` atomically
    /// flips the state to `active` and creates the branch/worktree,
    /// but the subsequent `dispatch()` call — which creates the
    /// worker session and writes `session_id` — may never run (e.g.
    /// because the enclosing schedule pass crashed, the plugin was
    /// restarted, or a hook delivering `ScheduleNeeded` was dropped).
    /// A task sitting in this state for more than a handful of
    /// seconds almost certainly needs a re-dispatch nudge.
    ///
    /// See task #572.
    pub fn get_stuck_active_tasks(
        &self,
        now_ms: i64,
        max_age_ms: i64,
    ) -> tau_agent_plugin::Result<Vec<Task>> {
        let cutoff = now_ms.saturating_sub(max_age_ms);
        let mut stmt = self
            .conn
            .prepare(&format!(
                "SELECT {} FROM tasks
                 WHERE state = 'active' AND session_id IS NULL AND updated_at <= ?1",
                TASK_COLUMNS
            ))
            .map_err(plugin_io_err("prepare stuck active query"))?;

        let rows = stmt
            .query_map(params![cutoff], row_to_task)
            .map_err(plugin_io_err("query stuck active"))?;

        let mut tasks = Vec::new();
        for row in rows {
            tasks.push(row.map_err(plugin_io_err("read stuck active task"))?);
        }
        Ok(tasks)
    }

    /// Find tasks in `ready` with a non-NULL `session_id` that have
    /// been idle longer than `max_age_ms`.  Used by the Scenario B
    /// watchdog (task #577) to recover tasks whose `session_id`
    /// still points at a finished planner/refiner/reviewer and that
    /// the caller believes the scheduler may be skipping.
    ///
    /// The query deliberately filters only on `state = 'ready'` and
    /// `session_id IS NOT NULL` — not held, still non-terminal —
    /// mirroring the schedulability conditions in
    /// [`get_schedulable_tasks`].  Liveness of the referenced
    /// session is checked by the caller via `GetSessionInfo`
    /// because it is an RPC round-trip, not a DB property.
    pub fn get_stuck_ready_tasks(
        &self,
        now_ms: i64,
        max_age_ms: i64,
    ) -> tau_agent_plugin::Result<Vec<Task>> {
        let cutoff = now_ms.saturating_sub(max_age_ms);
        let mut stmt = self
            .conn
            .prepare(&format!(
                "SELECT {} FROM tasks
                 WHERE state = 'ready' AND session_id IS NOT NULL
                   AND NOT held AND updated_at <= ?1",
                TASK_COLUMNS
            ))
            .map_err(plugin_io_err("prepare stuck ready query"))?;

        let rows = stmt
            .query_map(params![cutoff], row_to_task)
            .map_err(plugin_io_err("query stuck ready"))?;

        let mut tasks = Vec::new();
        for row in rows {
            tasks.push(row.map_err(plugin_io_err("read stuck ready task"))?);
        }
        Ok(tasks)
    }

    /// Find tasks stuck in `merging` for longer than `max_age_ms`.
    ///
    /// A healthy merge transitions through `merging` in seconds (the
    /// merge worker flips `Approved → Merging`, runs the merge, then
    /// flips `Merging → Merged`). A task that lingers here is a strong
    /// signal that something went wrong post-rebase — the file claim
    /// is still held but no more progress will happen on its own.
    ///
    /// Unlike [`get_stuck_active_tasks`], we do NOT filter on
    /// `session_id IS NULL`: the merge worker doesn't touch
    /// `task.session_id`, so a stuck merging task may have any value
    /// there (typically the worker session id from when the task was
    /// in `active`).
    ///
    /// See task #850.
    pub fn get_stuck_merging_tasks(
        &self,
        now_ms: i64,
        max_age_ms: i64,
    ) -> tau_agent_plugin::Result<Vec<Task>> {
        let cutoff = now_ms.saturating_sub(max_age_ms);
        let mut stmt = self
            .conn
            .prepare(&format!(
                "SELECT {} FROM tasks
                 WHERE state = 'merging' AND updated_at <= ?1",
                TASK_COLUMNS
            ))
            .map_err(plugin_io_err("prepare stuck merging query"))?;

        let rows = stmt
            .query_map(params![cutoff], row_to_task)
            .map_err(plugin_io_err("query stuck merging"))?;

        let mut tasks = Vec::new();
        for row in rows {
            tasks.push(row.map_err(plugin_io_err("read stuck merging task"))?);
        }
        Ok(tasks)
    }

    /// Find tasks in terminal states (merged/closed/failed) that still have a worktree_path set.
    /// Used for startup cleanup of stale worktrees.
    pub fn get_stale_worktree_tasks(&self) -> tau_agent_plugin::Result<Vec<Task>> {
        let mut stmt = self
            .conn
            .prepare(&format!(
                "SELECT {} FROM tasks
                 WHERE state IN ('merged', 'closed', 'failed') AND worktree_path IS NOT NULL",
                TASK_COLUMNS
            ))
            .map_err(plugin_io_err("prepare stale worktree query"))?;

        let rows = stmt
            .query_map([], row_to_task)
            .map_err(plugin_io_err("query stale worktrees"))?;

        let mut tasks = Vec::new();
        for row in rows {
            tasks.push(row.map_err(plugin_io_err("read stale task"))?);
        }
        Ok(tasks)
    }

    /// Clear the worktree path for a task (set to NULL).
    pub fn clear_worktree(&self, task_id: i64) -> tau_agent_plugin::Result<()> {
        let now = tau_agent_plugin::timestamp_ms() as i64;
        let updated = self
            .conn
            .execute(
                "UPDATE tasks SET worktree_path = NULL, updated_at = ?1 WHERE id = ?2",
                params![now, task_id],
            )
            .map_err(plugin_io_err("clear_worktree"))?;

        if updated == 0 {
            return Err(tau_agent_plugin::Error::Io(format!(
                "task {} not found",
                task_id
            )));
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
        project_name: row.get(1)?,
        title: row.get(2)?,
        state: row.get(3)?,
        priority: row.get(4)?,
        parent_id: row.get(5)?,
        tags,
        affected_files,
        branch: row.get(8)?,
        merge_target: row.get(9)?,
        worktree_path: row.get(10)?,
        session_id: row.get(11)?,
        skip_review: row.get::<_, i32>(12)? != 0,
        require_approval: row.get::<_, i32>(13)? != 0,
        sandbox_profile: row.get(14)?,
        held: row.get::<_, i32>(15)? != 0,
        placeholder_session_id: row.get(16)?,
        auto_downgraded_from_ready: row.get::<_, i32>(17)? != 0,
        filed_by_project: row.get(18)?,
        filed_by_session_id: row.get(19)?,
        created_at: row.get(20)?,
        updated_at: row.get(21)?,
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
    tau_agent_plugin::data_dir().join("tasks.db")
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
                "my-project",
                "Build feature X",
                Some(2),
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
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();

        assert_eq!(task.project_name, "my-project");
        assert_eq!(task.title, "Build feature X");
        assert_eq!(task.state, TaskState::Interactive);
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
                "test-project",
                "Tagged task",
                None,
                None,
                Some(&tags),
                false,
                "interactive",
                false,
                None,
                None,
                false,
                None,
                false,
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();

        assert_eq!(task.tags.unwrap(), serde_json::json!(["backend", "urgent"]));
    }

    #[test]
    fn test_list_tasks_filtered() {
        let db = TasksDb::open_memory().unwrap();

        let t1 = db
            .create_task(
                "test-project",
                "Task 1",
                Some(1),
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
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        let _t2 = db
            .create_task(
                "test-project",
                "Task 2",
                Some(2),
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
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        let _t3 = db
            .create_task(
                "other",
                "Task 3",
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
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();

        // All non-terminal tasks for /project
        let tasks = db
            .list_tasks("test-project", None, None, None, None)
            .unwrap();
        assert_eq!(tasks.len(), 2);

        // Filter by state
        db.update_task(
            t1.id,
            &TaskUpdate {
                state: Some(TaskState::Ready),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        let tasks = db
            .list_tasks("test-project", Some("ready"), None, None, None)
            .unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].title, "Task 1");

        // Filter by limit
        let tasks = db
            .list_tasks("test-project", None, None, None, Some(1))
            .unwrap();
        assert_eq!(tasks.len(), 1);
    }

    #[test]
    fn test_list_tasks_by_tag() {
        let db = TasksDb::open_memory().unwrap();
        let tags = serde_json::json!(["backend", "urgent"]);
        db.create_task(
            "test-project",
            "Tagged",
            None,
            None,
            Some(&tags),
            false,
            "interactive",
            false,
            None,
            None,
            false,
            None,
            false,
            crate::tasks_db::FiledBy::default(),
        )
        .unwrap();
        db.create_task(
            "test-project",
            "Untagged",
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
            crate::tasks_db::FiledBy::default(),
        )
        .unwrap();

        let tasks = db
            .list_tasks("test-project", None, None, Some("backend"), None)
            .unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].title, "Tagged");

        let tasks = db
            .list_tasks("test-project", None, None, Some("nonexistent"), None)
            .unwrap();
        assert_eq!(tasks.len(), 0);
    }

    #[test]
    fn test_update_task_records_history() {
        let db = TasksDb::open_memory().unwrap();
        let task = db
            .create_task(
                "test-project",
                "Original",
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
                crate::tasks_db::FiledBy::default(),
            )
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
        use crate::tasks_state::TaskState::*;
        use crate::tasks_state::validate_transition as v;
        // Forward transitions
        assert!(v(Interactive, Ready));
        assert!(v(Interactive, Approved));
        assert!(v(Ready, Active));
        assert!(v(Active, Review));
        assert!(v(Active, Approved));
        assert!(v(Review, Approved));
        assert!(v(Approved, Merging));
        assert!(v(Merging, Merged));

        // Planning/Refining transitions
        assert!(v(Interactive, Planning));
        assert!(v(Interactive, Refining));
        assert!(v(Planning, Refining));
        assert!(v(Refining, Planning));
        assert!(v(Refining, Ready));

        // Backward transitions (error recovery)
        assert!(v(Active, Ready));
        assert!(v(Review, Active));
        assert!(v(Approved, Active));
        assert!(v(Approved, Ready));
        assert!(v(Approved, Interactive));
        assert!(v(Merging, Active));
        assert!(v(Merging, Failed));
        assert!(v(Failed, Active));

        // Universal overrides: any state -> closed
        assert!(v(Interactive, Closed));
        assert!(v(Planning, Closed));
        assert!(v(Refining, Closed));
        assert!(v(Ready, Closed));
        assert!(v(Active, Closed));
        assert!(v(Review, Closed));
        assert!(v(Approved, Closed));
        assert!(v(Failed, Closed));

        // Universal overrides: any state -> interactive
        assert!(v(Planning, Interactive));
        assert!(v(Refining, Interactive));
        assert!(v(Ready, Interactive));
        assert!(v(Active, Interactive));
        assert!(v(Review, Interactive));
        assert!(v(Approved, Interactive));
        assert!(v(Closed, Interactive));
        // merged is fully terminal
        assert!(!v(Merged, Interactive));
        assert!(!v(Merged, Closed));
        assert!(!v(Merged, Failed));

        // Universal overrides: any state -> failed
        assert!(v(Planning, Failed));
        assert!(v(Refining, Failed));
        assert!(v(Active, Failed));
        assert!(v(Review, Failed));

        // Self-loops are not allowed
        assert!(!v(Merged, Merged));
        assert!(!v(Closed, Closed));
        assert!(!v(Interactive, Interactive));
        assert!(!v(Planning, Planning));
        assert!(!v(Refining, Refining));

        // Skip transitions that don't make sense
        assert!(!v(Interactive, Active));
        assert!(!v(Interactive, Merging));
        assert!(!v(Planning, Active)); // must go through refining/ready
        assert!(!v(Planning, Review));
        assert!(!v(Refining, Active)); // must go through ready

        // failed state transitions
        assert!(v(Merging, Failed));
        assert!(v(Failed, Active));
        assert!(v(Failed, Closed)); // universal
        assert!(v(Failed, Interactive)); // universal
        assert!(!v(Failed, Merging));
        assert!(!v(Failed, Approved));
    }

    #[test]
    fn test_state_transition_rejected() {
        let db = TasksDb::open_memory().unwrap();
        let task = db
            .create_task(
                "test-project",
                "Test",
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
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();

        // interactive -> active is invalid
        let err = db
            .update_task(
                task.id,
                &TaskUpdate {
                    state: Some(TaskState::Active),
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
                state: Some(TaskState::Ready),
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
            .create_task(
                "test-project",
                "Test",
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
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        // interactive -> ready -> active -> review -> approved
        for state in [
            TaskState::Ready,
            TaskState::Active,
            TaskState::Review,
            TaskState::Approved,
        ] {
            db.update_task(
                task.id,
                &TaskUpdate {
                    state: Some(state),
                    ..Default::default()
                },
                None,
            )
            .unwrap();
        }
        assert_eq!(
            db.get_task(task.id).unwrap().unwrap().state,
            TaskState::Approved
        );

        // approved -> active (merge error, agent needs to fix)
        db.update_task(
            task.id,
            &TaskUpdate {
                state: Some(TaskState::Active),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        assert_eq!(
            db.get_task(task.id).unwrap().unwrap().state,
            TaskState::Active
        );

        // Back to approved via review
        db.update_task(
            task.id,
            &TaskUpdate {
                state: Some(TaskState::Review),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        db.update_task(
            task.id,
            &TaskUpdate {
                state: Some(TaskState::Approved),
                ..Default::default()
            },
            None,
        )
        .unwrap();

        // approved -> ready (unapprove, send back to queue)
        db.update_task(
            task.id,
            &TaskUpdate {
                state: Some(TaskState::Ready),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        assert_eq!(
            db.get_task(task.id).unwrap().unwrap().state,
            TaskState::Ready
        );

        // ready -> active -> review -> approved
        for state in [TaskState::Active, TaskState::Review, TaskState::Approved] {
            db.update_task(
                task.id,
                &TaskUpdate {
                    state: Some(state),
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
                state: Some(TaskState::Interactive),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        assert_eq!(
            db.get_task(task.id).unwrap().unwrap().state,
            TaskState::Interactive
        );
    }

    #[test]
    fn test_universal_closed_override() {
        let db = TasksDb::open_memory().unwrap();

        // Any state -> closed should work
        for start_state in [
            TaskState::Interactive,
            TaskState::Ready,
            TaskState::Active,
            TaskState::Review,
            TaskState::Approved,
        ] {
            let task = db
                .create_task(
                    "test-project",
                    &format!("Test {}", start_state),
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
                    crate::tasks_db::FiledBy::default(),
                )
                .unwrap();

            // Advance to the start state
            let path_to_state: &[TaskState] = match start_state {
                TaskState::Interactive => &[],
                TaskState::Ready => &[TaskState::Ready],
                TaskState::Active => &[TaskState::Ready, TaskState::Active],
                TaskState::Review => &[TaskState::Ready, TaskState::Active, TaskState::Review],
                TaskState::Approved => &[
                    TaskState::Ready,
                    TaskState::Active,
                    TaskState::Review,
                    TaskState::Approved,
                ],
                _ => unreachable!(),
            };
            for state in path_to_state {
                db.update_task(
                    task.id,
                    &TaskUpdate {
                        state: Some(*state),
                        ..Default::default()
                    },
                    None,
                )
                .unwrap();
            }

            // -> closed
            db.update_task(
                task.id,
                &TaskUpdate {
                    state: Some(TaskState::Closed),
                    ..Default::default()
                },
                None,
            )
            .unwrap();
            assert_eq!(
                db.get_task(task.id).unwrap().unwrap().state,
                TaskState::Closed
            );
        }
    }

    #[test]
    fn test_universal_interactive_override() {
        let db = TasksDb::open_memory().unwrap();

        // Any state -> interactive should work
        for start_state in [
            TaskState::Ready,
            TaskState::Active,
            TaskState::Review,
            TaskState::Approved,
            TaskState::Closed,
        ] {
            let task = db
                .create_task(
                    "test-project",
                    &format!("Test {}", start_state),
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
                    crate::tasks_db::FiledBy::default(),
                )
                .unwrap();

            // Advance to the start state
            let path_to_state: &[TaskState] = match start_state {
                TaskState::Ready => &[TaskState::Ready],
                TaskState::Active => &[TaskState::Ready, TaskState::Active],
                TaskState::Review => &[TaskState::Ready, TaskState::Active, TaskState::Review],
                TaskState::Approved => &[
                    TaskState::Ready,
                    TaskState::Active,
                    TaskState::Review,
                    TaskState::Approved,
                ],
                TaskState::Closed => &[TaskState::Closed], // uses universal override
                _ => unreachable!(),
            };
            for state in path_to_state {
                db.update_task(
                    task.id,
                    &TaskUpdate {
                        state: Some(*state),
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
                    state: Some(TaskState::Interactive),
                    ..Default::default()
                },
                None,
            )
            .unwrap();
            assert_eq!(
                db.get_task(task.id).unwrap().unwrap().state,
                TaskState::Interactive
            );
        }
    }

    #[test]
    fn test_self_loop_transitions_rejected() {
        let db = TasksDb::open_memory().unwrap();
        let task = db
            .create_task(
                "test-project",
                "Test",
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
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();

        // interactive -> interactive is a self-loop, should be rejected
        let err = db
            .update_task(
                task.id,
                &TaskUpdate {
                    state: Some(TaskState::Interactive),
                    ..Default::default()
                },
                None,
            )
            .unwrap_err();
        assert!(err.to_string().contains("invalid state transition"));
    }

    /// With the typed [`TaskState`] enum (task #611) an "invalid state
    /// name" is now a compile-time error at every construction site, not
    /// a runtime one.  The parsing path into the enum lives in
    /// [`TaskState::from_db_str`]; see `tasks_state::tests` for coverage
    /// of the unknown-string rejection.  This test keeps the historical
    /// name and instead confirms that the parser returns an error (as
    /// consumed by `handle_task_update`).
    #[test]
    fn test_invalid_state_name() {
        let _db = TasksDb::open_memory().unwrap();
        let err = crate::tasks_state::TaskState::from_db_str("bogus").unwrap_err();
        assert_eq!(err.0, "bogus");
        assert!(format!("{}", err).contains("bogus"));
    }

    #[test]
    fn test_messages() {
        let db = TasksDb::open_memory().unwrap();
        let task = db
            .create_task(
                "test-project",
                "Test",
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
                crate::tasks_db::FiledBy::default(),
            )
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
            .create_task(
                "test-project",
                "Task 1",
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
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        let t2 = db
            .create_task(
                "test-project",
                "Task 2",
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
                crate::tasks_db::FiledBy::default(),
            )
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
            .create_task(
                "test-project",
                "Task 1",
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
                crate::tasks_db::FiledBy::default(),
            )
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
            .create_task(
                "test-project",
                "Task 1",
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
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        let t2 = db
            .create_task(
                "test-project",
                "Task 2",
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
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();

        let err = db.add_relation(t1.id, t2.id, "invalid").unwrap_err();
        assert!(err.to_string().contains("invalid relation type"));
    }

    #[test]
    fn test_search() {
        let db = TasksDb::open_memory().unwrap();
        let t1 = db
            .create_task(
                "test-project",
                "Build the API",
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
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        let _t2 = db
            .create_task(
                "test-project",
                "Write docs",
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
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        let t3 = db
            .create_task(
                "test-project",
                "Something else",
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
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();

        // Add a message mentioning API to t3
        db.add_message(t3.id, "This relates to the API layer", None)
            .unwrap();

        // Search for "API" should find t1 (title) and t3 (message)
        let results = db.search_tasks("test-project", "API", None).unwrap();
        assert_eq!(results.len(), 2);
        let ids: Vec<i64> = results.iter().map(|t| t.id).collect();
        assert!(ids.contains(&t1.id));
        assert!(ids.contains(&t3.id));

        // Search in different project
        let results = db.search_tasks("other", "API", None).unwrap();
        assert_eq!(results.len(), 0);
    }

    #[test]
    fn test_subtasks() {
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
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        let _child1 = db
            .create_task(
                "test-project",
                "Child 1",
                Some(2),
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
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        let _child2 = db
            .create_task(
                "test-project",
                "Child 2",
                Some(1),
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
                crate::tasks_db::FiledBy::default(),
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
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        db.create_task(
            "test-project",
            "Child 1",
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
            crate::tasks_db::FiledBy::default(),
        )
        .unwrap();
        db.create_task(
            "test-project",
            "Child 2",
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
            crate::tasks_db::FiledBy::default(),
        )
        .unwrap();
        db.create_task(
            "test-project",
            "Other",
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
            crate::tasks_db::FiledBy::default(),
        )
        .unwrap();

        let tasks = db
            .list_tasks("test-project", None, Some(parent.id), None, None)
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
            .create_task(
                "test-project",
                "Test",
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
                crate::tasks_db::FiledBy::default(),
            )
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
            .create_task(
                "test-project",
                "Test",
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
                crate::tasks_db::FiledBy::default(),
            )
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
            .create_task(
                "test-project",
                "Test",
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
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        assert_eq!(task.state, TaskState::Interactive);

        // Move to ready first
        db.update_task(
            task.id,
            &TaskUpdate {
                state: Some(TaskState::Ready),
                ..Default::default()
            },
            None,
        )
        .unwrap();

        // Assign
        let assigned = db.assign_task(task.id, "session-1").unwrap().task;
        assert_eq!(assigned.state, TaskState::Active);
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
            .create_task(
                "test-project",
                "Test",
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
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();

        // Move to ready then active — active tasks can't be assigned
        db.update_task(
            task.id,
            &TaskUpdate {
                state: Some(TaskState::Ready),
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
            .create_task(
                "test-project",
                "Test",
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
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        assert_eq!(task.state, TaskState::Interactive);

        // Assigning an interactive task should succeed and stay interactive
        let assigned = db.assign_task(task.id, "s1").unwrap().task;
        assert_eq!(assigned.state, TaskState::Interactive);
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
            .create_task(
                "test-project",
                "Test",
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
                crate::tasks_db::FiledBy::default(),
            )
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
            .create_task(
                "test-project",
                "Test",
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
                crate::tasks_db::FiledBy::default(),
            )
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
    fn test_get_history_returns_updates_in_chronological_order() {
        let db = TasksDb::open_memory().unwrap();
        let task = db
            .create_task(
                "test-project",
                "Original",
                Some(3),
                None,
                None,
                false,
                "ready",
                false,
                None,
                None,
                false,
                None,
                false,
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();

        // Apply a few updates; each one records a row in task_history.
        db.update_task(
            task.id,
            &TaskUpdate {
                priority: Some(7),
                ..Default::default()
            },
            Some("s1"),
        )
        .unwrap();
        db.update_task(
            task.id,
            &TaskUpdate {
                state: Some(TaskState::Active),
                ..Default::default()
            },
            Some("s2"),
        )
        .unwrap();
        db.update_task(
            task.id,
            &TaskUpdate {
                title: Some("Renamed".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();

        let history = db.get_history(task.id).unwrap();
        assert_eq!(history.len(), 3, "three updates expected");

        assert_eq!(history[0].field, "priority");
        assert_eq!(history[0].old_value.as_deref(), Some("3"));
        assert_eq!(history[0].new_value.as_deref(), Some("7"));
        assert_eq!(history[0].session_id.as_deref(), Some("s1"));

        assert_eq!(history[1].field, "state");
        assert_eq!(history[1].old_value.as_deref(), Some("ready"));
        assert_eq!(history[1].new_value.as_deref(), Some("active"));
        assert_eq!(history[1].session_id.as_deref(), Some("s2"));

        assert_eq!(history[2].field, "title");
        assert_eq!(history[2].new_value.as_deref(), Some("Renamed"));
        assert!(history[2].session_id.is_none());
    }

    #[test]
    fn test_get_history_empty_for_untouched_task() {
        let db = TasksDb::open_memory().unwrap();
        let task = db
            .create_task(
                "test-project",
                "Test",
                None,
                None,
                None,
                false,
                "ready",
                false,
                None,
                None,
                false,
                None,
                false,
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();

        // create_task does not write to task_history.
        let history = db.get_history(task.id).unwrap();
        assert!(history.is_empty());
    }

    #[test]
    fn test_list_project_task_sessions_filters_by_project() {
        let db = TasksDb::open_memory().unwrap();
        let t1 = db
            .create_task(
                "proj-a",
                "a",
                None,
                None,
                None,
                false,
                "ready",
                false,
                None,
                None,
                false,
                None,
                false,
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        let t2 = db
            .create_task(
                "proj-a",
                "b",
                None,
                None,
                None,
                false,
                "ready",
                false,
                None,
                None,
                false,
                None,
                false,
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        let t3 = db
            .create_task(
                "proj-b",
                "c",
                None,
                None,
                None,
                false,
                "ready",
                false,
                None,
                None,
                false,
                None,
                false,
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();

        db.record_session(t1.id, "s-a1", "worker").unwrap();
        db.record_session(t1.id, "s-a2", "reviewer").unwrap();
        db.record_session(t2.id, "s-a3", "worker").unwrap();
        db.record_session(t3.id, "s-b1", "worker").unwrap();

        let mut rows = db.list_project_task_sessions("proj-a").unwrap();
        rows.sort();
        assert_eq!(
            rows,
            vec![
                (t1.id, "s-a1".to_string()),
                (t1.id, "s-a2".to_string()),
                (t2.id, "s-a3".to_string()),
            ]
        );

        let rows_b = db.list_project_task_sessions("proj-b").unwrap();
        assert_eq!(rows_b, vec![(t3.id, "s-b1".to_string())]);

        let empty = db.list_project_task_sessions("proj-c").unwrap();
        assert!(empty.is_empty());
    }

    #[test]
    fn test_list_protected_session_ids() {
        use std::collections::HashSet;

        let db = TasksDb::open_memory().unwrap();

        // Helper: stamp a task to a particular state via raw SQL.
        // (Skips `validate_transition` which we don't want to fight
        // for terminal states.)
        let set_state = |id: i64, state: &str| {
            db.conn
                .execute(
                    "UPDATE tasks SET state = ?1 WHERE id = ?2",
                    params![state, id],
                )
                .unwrap();
        };

        // Non-terminal task with session_id + placeholder + a task_sessions row.
        let active = db
            .create_task(
                "p",
                "active",
                None,
                None,
                None,
                false,
                "ready",
                false,
                None,
                None,
                false,
                None,
                false,
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        db.set_session_id(active.id, "s-active").unwrap();
        db.set_placeholder_session_id(active.id, "s-active-ph")
            .unwrap();
        db.record_session(active.id, "s-active-ts", "worker")
            .unwrap();
        set_state(active.id, "active");

        // Planning task with placeholder only.
        let planning = db
            .create_task(
                "p",
                "planning",
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
                false,
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        db.set_placeholder_session_id(planning.id, "s-plan-ph")
            .unwrap();

        // Review task with session id.
        let review = db
            .create_task(
                "p",
                "review",
                None,
                None,
                None,
                false,
                "ready",
                false,
                None,
                None,
                false,
                None,
                false,
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        db.set_session_id(review.id, "s-review").unwrap();
        set_state(review.id, "review");

        // Merged task with session id + task_sessions row -> NOT protected.
        let merged = db
            .create_task(
                "p",
                "merged",
                None,
                None,
                None,
                false,
                "ready",
                false,
                None,
                None,
                false,
                None,
                false,
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        db.set_session_id(merged.id, "s-merged").unwrap();
        db.record_session(merged.id, "s-merged-ts", "worker")
            .unwrap();
        set_state(merged.id, "merged");

        // Closed task -> NOT protected.
        let closed = db
            .create_task(
                "p",
                "closed",
                None,
                None,
                None,
                false,
                "ready",
                false,
                None,
                None,
                false,
                None,
                false,
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        db.set_session_id(closed.id, "s-closed").unwrap();
        set_state(closed.id, "closed");

        // Failed task -> NOT protected (deliberate departure from
        // get_blocking_dependencies' terminal set).
        let failed = db
            .create_task(
                "p",
                "failed",
                None,
                None,
                None,
                false,
                "ready",
                false,
                None,
                None,
                false,
                None,
                false,
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        db.set_session_id(failed.id, "s-failed").unwrap();
        db.record_session(failed.id, "s-failed-ts", "worker")
            .unwrap();
        set_state(failed.id, "failed");

        let got = db.list_protected_session_ids().unwrap();
        let expected: HashSet<String> = [
            "s-active",
            "s-active-ph",
            "s-active-ts",
            "s-plan-ph",
            "s-review",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        assert_eq!(got, expected);
    }

    #[test]
    fn test_find_latest_session_by_role() {
        let db = TasksDb::open_memory().unwrap();
        let task = db
            .create_task(
                "test-project",
                "Test",
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
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();

        // No sessions yet
        assert_eq!(
            db.find_latest_session_by_role(task.id, "reviewer").unwrap(),
            None
        );

        // Record two reviewer sessions
        db.record_session(task.id, "s1", "reviewer").unwrap();
        // Small sleep to ensure different timestamps
        std::thread::sleep(std::time::Duration::from_millis(5));
        db.record_session(task.id, "s2", "reviewer").unwrap();
        db.record_session(task.id, "s3", "worker").unwrap();

        // Should return the most recent reviewer
        assert_eq!(
            db.find_latest_session_by_role(task.id, "reviewer").unwrap(),
            Some("s2".into())
        );

        // Should return worker
        assert_eq!(
            db.find_latest_session_by_role(task.id, "worker").unwrap(),
            Some("s3".into())
        );

        // No refiner sessions
        assert_eq!(
            db.find_latest_session_by_role(task.id, "refiner").unwrap(),
            None
        );
    }

    #[test]
    fn test_subtask_defaults_to_planning() {
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
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        assert_eq!(parent.state, TaskState::Interactive);

        // Subtasks default to planning state when initial_state="planning".
        // Post-task-#512, skip_review is NOT force-set to false for subtasks —
        // behaviour on the skip_review / require_approval axes is uniform with
        // top-level tasks and simply reflects the caller's arguments.
        let child = db
            .create_task(
                "test-project",
                "Child",
                None,
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
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        assert_eq!(child.state, TaskState::Planning);
        assert!(child.skip_review);
    }

    #[test]
    fn test_create_task_initial_state_ready_subtask() {
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
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();

        // Subtask with initial_state="ready" starts in ready state.
        let child = db
            .create_task(
                "test-project",
                "Child",
                None,
                Some(parent.id),
                None,
                false,
                "ready",
                false,
                None,
                None,
                false,
                None,
                false,
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        assert_eq!(child.state, TaskState::Ready);
    }

    #[test]
    fn test_create_task_initial_state_applies_to_top_level() {
        let db = TasksDb::open_memory().unwrap();

        // Top-level task with initial_state="ready" — no automatic forcing
        // to "interactive" any more (task #512 unified the behaviour).
        let task = db
            .create_task(
                "test-project",
                "Top level ready",
                None,
                None,
                None,
                false,
                "ready",
                false,
                None,
                None,
                false,
                None,
                false,
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        assert_eq!(task.state, TaskState::Ready);

        let task = db
            .create_task(
                "test-project",
                "Top level planning",
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
                false,
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        assert_eq!(task.state, TaskState::Planning);

        let task = db
            .create_task(
                "test-project",
                "Top level interactive",
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
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        assert_eq!(task.state, TaskState::Interactive);
    }

    #[test]
    fn test_create_task_initial_state_invalid_rejected() {
        let db = TasksDb::open_memory().unwrap();
        let err = db
            .create_task(
                "test-project",
                "Bad",
                None,
                None,
                None,
                false,
                "bogus",
                false,
                None,
                None,
                false,
                None,
                false,
                crate::tasks_db::FiledBy::default(),
            )
            .expect_err("invalid initial_state should fail");
        let msg = err.to_string();
        assert!(
            msg.contains("invalid initial_state") && msg.contains("bogus"),
            "unexpected error message: {}",
            msg
        );
    }

    #[test]
    fn test_require_approval_roundtrip() {
        let db = TasksDb::open_memory().unwrap();
        let task = db
            .create_task(
                "test-project",
                "Test",
                None,
                None,
                None,
                false,
                "interactive",
                true,
                None,
                None,
                false,
                None,
                false,
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        assert!(task.require_approval);

        let updated = db
            .update_task(
                task.id,
                &TaskUpdate {
                    require_approval: Some(false),
                    ..Default::default()
                },
                None,
            )
            .unwrap();
        assert!(!updated.require_approval);
    }

    #[test]
    fn test_require_approval_default_false() {
        let db = TasksDb::open_memory().unwrap();
        let task = db
            .create_task(
                "test-project",
                "Test",
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
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        assert!(!task.require_approval);
    }

    #[test]
    fn test_sandbox_profile_roundtrip() {
        let db = TasksDb::open_memory().unwrap();
        let task = db
            .create_task(
                "test-project",
                "Test",
                None,
                None,
                None,
                false,
                "interactive",
                false,
                None,
                Some("restricted"),
                false,
                None,
                false,
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        assert_eq!(task.sandbox_profile.as_deref(), Some("restricted"));

        let updated = db
            .update_task(
                task.id,
                &TaskUpdate {
                    sandbox_profile: Some("permissive".into()),
                    ..Default::default()
                },
                None,
            )
            .unwrap();
        assert_eq!(updated.sandbox_profile.as_deref(), Some("permissive"));
    }

    #[test]
    fn test_sandbox_profile_default_none() {
        let db = TasksDb::open_memory().unwrap();
        let task = db
            .create_task(
                "test-project",
                "Test",
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
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        assert!(task.sandbox_profile.is_none());
    }

    #[test]
    fn test_active_to_approved_blocked_without_skip_review() {
        let db = TasksDb::open_memory().unwrap();
        let task = db
            .create_task(
                "test-project",
                "Test",
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
                crate::tasks_db::FiledBy::default(),
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

        let err = db
            .update_task(
                task.id,
                &TaskUpdate {
                    state: Some(TaskState::Approved),
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
            .create_task(
                "test-project",
                "Test",
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
                crate::tasks_db::FiledBy::default(),
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

        let result = db
            .update_task(
                task.id,
                &TaskUpdate {
                    state: Some(TaskState::Approved),
                    ..Default::default()
                },
                Some("s1"),
            )
            .unwrap();
        assert_eq!(result.state, TaskState::Approved);
    }

    // ----- git integration tests -----

    #[test]
    fn test_set_branch() {
        let db = TasksDb::open_memory().unwrap();
        let task = db
            .create_task(
                "test-project",
                "Test",
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
                crate::tasks_db::FiledBy::default(),
            )
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
            .create_task(
                "test-project",
                "Test",
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
                crate::tasks_db::FiledBy::default(),
            )
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
            .create_task(
                "test-project",
                "Test",
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
                crate::tasks_db::FiledBy::default(),
            )
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
    fn test_get_stale_worktree_tasks() {
        let db = TasksDb::open_memory().unwrap();

        // Task 1: closed with worktree (stale)
        let t1 = db
            .create_task(
                "test-project",
                "Closed with worktree",
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
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        db.set_worktree_path(t1.id, "/tmp/wt-1").unwrap();
        db.update_task(
            t1.id,
            &TaskUpdate {
                state: Some(TaskState::Closed),
                ..Default::default()
            },
            None,
        )
        .unwrap();

        // Task 2: failed with worktree (stale)
        let t2 = db
            .create_task(
                "test-project",
                "Failed with worktree",
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
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        db.set_worktree_path(t2.id, "/tmp/wt-2").unwrap();
        db.update_task(
            t2.id,
            &TaskUpdate {
                state: Some(TaskState::Failed),
                ..Default::default()
            },
            None,
        )
        .unwrap();

        // Task 3: closed without worktree (not stale)
        let t3 = db
            .create_task(
                "test-project",
                "Closed no worktree",
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
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        db.update_task(
            t3.id,
            &TaskUpdate {
                state: Some(TaskState::Closed),
                ..Default::default()
            },
            None,
        )
        .unwrap();

        // Task 4: active with worktree (not stale — still in progress)
        let t4 = db
            .create_task(
                "test-project",
                "Active with worktree",
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
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        db.set_worktree_path(t4.id, "/tmp/wt-4").unwrap();
        db.update_task(
            t4.id,
            &TaskUpdate {
                state: Some(TaskState::Ready),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        db.assign_task(t4.id, "s1").unwrap();

        let stale = db.get_stale_worktree_tasks().unwrap();
        let ids: Vec<i64> = stale.iter().map(|t| t.id).collect();
        assert!(
            ids.contains(&t1.id),
            "closed task with worktree should be stale"
        );
        assert!(
            ids.contains(&t2.id),
            "failed task with worktree should be stale"
        );
        assert!(
            !ids.contains(&t3.id),
            "closed task without worktree should not be stale"
        );
        assert!(
            !ids.contains(&t4.id),
            "active task with worktree should not be stale"
        );
        assert_eq!(stale.len(), 2);
    }

    #[test]
    fn test_get_merge_target_root_task() {
        let db = TasksDb::open_memory().unwrap();
        let task = db
            .create_task(
                "test-project",
                "Root task",
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
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();

        let target = db.get_merge_target(task.id).unwrap();
        assert_eq!(target, "main");
    }

    #[test]
    fn test_get_merge_target_subtask() {
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
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        db.set_branch(parent.id, "task-1").unwrap();

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
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();

        let target = db.get_merge_target(child.id).unwrap();
        assert_eq!(target, "task-1");
    }

    #[test]
    fn test_get_merge_target_parent_no_branch() {
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
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        // Don't set a branch on parent — should fall back to "main"

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
                crate::tasks_db::FiledBy::default(),
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

    #[test]
    fn test_get_merge_target_explicit_override_root() {
        let db = TasksDb::open_memory().unwrap();
        let task = db
            .create_task(
                "test-project",
                "Root with override",
                None,
                None,
                None,
                false,
                "interactive",
                false,
                Some("develop"),
                None,
                false,
                None,
                false,
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();

        let target = db.get_merge_target(task.id).unwrap();
        assert_eq!(target, "develop");
    }

    #[test]
    fn test_get_merge_target_explicit_override_subtask() {
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
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        db.set_branch(parent.id, "task-1").unwrap();

        // Subtask with explicit merge_target should override parent's branch
        let child = db
            .create_task(
                "test-project",
                "Child with override",
                None,
                Some(parent.id),
                None,
                false,
                "planning",
                false,
                Some("main"),
                None,
                false,
                None,
                false,
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();

        let target = db.get_merge_target(child.id).unwrap();
        assert_eq!(target, "main");
    }

    #[test]
    fn test_merge_target_roundtrip_create() {
        let db = TasksDb::open_memory().unwrap();
        let task = db
            .create_task(
                "test-project",
                "With merge target",
                None,
                None,
                None,
                false,
                "interactive",
                false,
                Some("release/v2"),
                None,
                false,
                None,
                false,
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();

        assert_eq!(task.merge_target.as_deref(), Some("release/v2"));
    }

    #[test]
    fn test_merge_target_roundtrip_update() {
        let db = TasksDb::open_memory().unwrap();
        let task = db
            .create_task(
                "test-project",
                "Update merge target",
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
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        assert!(task.merge_target.is_none());

        let updated = db
            .update_task(
                task.id,
                &TaskUpdate {
                    merge_target: Some("develop".into()),
                    ..Default::default()
                },
                None,
            )
            .unwrap();
        assert_eq!(updated.merge_target.as_deref(), Some("develop"));

        // Verify get_merge_target uses the override
        let target = db.get_merge_target(task.id).unwrap();
        assert_eq!(target, "develop");
    }

    #[test]
    fn test_merge_target_null_preserves_default_behavior() {
        let db = TasksDb::open_memory().unwrap();
        // Root task without merge_target should still return "main"
        let task = db
            .create_task(
                "test-project",
                "Default behavior",
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
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        assert!(task.merge_target.is_none());
        assert_eq!(db.get_merge_target(task.id).unwrap(), "main");

        // Subtask without merge_target should derive from parent branch
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
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        db.set_branch(parent.id, "feature-branch").unwrap();

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
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        assert!(child.merge_target.is_none());
        assert_eq!(db.get_merge_target(child.id).unwrap(), "feature-branch");
    }

    // ----- dependency enforcement tests -----

    /// Helper: create a task and move it to a given state using valid transitions.
    fn create_task_in_state(db: &TasksDb, project: &str, title: &str, state: &str) -> Task {
        let task = db
            .create_task(
                project,
                title,
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
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        let transitions: &[TaskState] = match state {
            "interactive" => &[],
            "ready" => &[TaskState::Ready],
            "active" => &[TaskState::Ready],
            "review" => &[TaskState::Ready],
            "approved" => &[TaskState::Ready],
            "failed" => &[TaskState::Ready],
            "merged" => &[TaskState::Ready],
            "closed" => &[TaskState::Ready],
            _ => panic!("unsupported target state: {}", state),
        };
        for &s in transitions {
            db.update_task(
                task.id,
                &TaskUpdate {
                    state: Some(s),
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
                        state: Some(TaskState::Review),
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
                        state: Some(TaskState::Approved),
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
                        state: Some(TaskState::Approved),
                        ..Default::default()
                    },
                    None,
                )
                .unwrap();
                db.update_task(
                    task.id,
                    &TaskUpdate {
                        state: Some(TaskState::Merging),
                        ..Default::default()
                    },
                    None,
                )
                .unwrap();
                db.update_task(
                    task.id,
                    &TaskUpdate {
                        state: Some(TaskState::Failed),
                        ..Default::default()
                    },
                    None,
                )
                .unwrap();
            }
            "merged" => {
                db.assign_task(task.id, "test-session").unwrap();
                db.update_task(
                    task.id,
                    &TaskUpdate {
                        state: Some(TaskState::Approved),
                        ..Default::default()
                    },
                    None,
                )
                .unwrap();
                db.update_task(
                    task.id,
                    &TaskUpdate {
                        state: Some(TaskState::Merging),
                        ..Default::default()
                    },
                    None,
                )
                .unwrap();
                db.update_task(
                    task.id,
                    &TaskUpdate {
                        state: Some(TaskState::Merged),
                        ..Default::default()
                    },
                    None,
                )
                .unwrap();
            }
            "closed" => {
                db.update_task(
                    task.id,
                    &TaskUpdate {
                        state: Some(TaskState::Closed),
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
        let dep = create_task_in_state(&db, "test-project", "Dependency", "ready");
        let task = create_task_in_state(&db, "test-project", "Dependent", "ready");

        db.add_relation(task.id, dep.id, "depends_on").unwrap();

        let blocking = db.get_blocking_dependencies(task.id).unwrap();
        assert_eq!(blocking.len(), 1);
        assert_eq!(blocking[0].id, dep.id);
    }

    #[test]
    fn test_get_blocking_dependencies_with_met_dep() {
        let db = TasksDb::open_memory().unwrap();
        let dep = create_task_in_state(&db, "test-project", "Dependency", "merged");
        let task = create_task_in_state(&db, "test-project", "Dependent", "ready");

        db.add_relation(task.id, dep.id, "depends_on").unwrap();

        let blocking = db.get_blocking_dependencies(task.id).unwrap();
        assert!(blocking.is_empty());
    }

    #[test]
    fn test_get_blocking_dependencies_no_deps() {
        let db = TasksDb::open_memory().unwrap();
        let task = create_task_in_state(&db, "test-project", "No deps", "ready");

        let blocking = db.get_blocking_dependencies(task.id).unwrap();
        assert!(blocking.is_empty());
    }

    #[test]
    fn test_get_blocking_dependencies_ignores_non_depends_on() {
        let db = TasksDb::open_memory().unwrap();
        let other = create_task_in_state(&db, "test-project", "Related", "ready");
        let task = create_task_in_state(&db, "test-project", "Task", "ready");

        // "related" relation should NOT count as a blocking dependency
        db.add_relation(task.id, other.id, "related").unwrap();

        let blocking = db.get_blocking_dependencies(task.id).unwrap();
        assert!(blocking.is_empty());
    }

    #[test]
    fn test_get_schedulable_tasks_with_unmet_dep() {
        let db = TasksDb::open_memory().unwrap();
        let dep = create_task_in_state(&db, "test-project", "Dependency", "active");
        let task = create_task_in_state(&db, "test-project", "Blocked task", "ready");

        db.add_relation(task.id, dep.id, "depends_on").unwrap();

        let schedulable = db.get_schedulable_tasks("test-project").unwrap();
        // dep is active (not ready), task is blocked — neither should be schedulable
        let ids: Vec<i64> = schedulable.iter().map(|t| t.id).collect();
        assert!(!ids.contains(&task.id));
    }

    #[test]
    fn test_get_schedulable_tasks_with_met_dep() {
        let db = TasksDb::open_memory().unwrap();
        let dep = create_task_in_state(&db, "test-project", "Dependency", "merged");
        let task = create_task_in_state(&db, "test-project", "Unblocked task", "ready");

        db.add_relation(task.id, dep.id, "depends_on").unwrap();

        let schedulable = db.get_schedulable_tasks("test-project").unwrap();
        let ids: Vec<i64> = schedulable.iter().map(|t| t.id).collect();
        assert!(ids.contains(&task.id));
    }

    #[test]
    fn test_get_schedulable_tasks_no_deps() {
        let db = TasksDb::open_memory().unwrap();
        let task = create_task_in_state(&db, "test-project", "Independent task", "ready");

        let schedulable = db.get_schedulable_tasks("test-project").unwrap();
        let ids: Vec<i64> = schedulable.iter().map(|t| t.id).collect();
        assert!(ids.contains(&task.id));
    }

    #[test]
    fn test_get_schedulable_tasks_mixed() {
        let db = TasksDb::open_memory().unwrap();
        let dep = create_task_in_state(&db, "test-project", "Dependency", "active");
        let blocked = create_task_in_state(&db, "test-project", "Blocked", "ready");
        let free = create_task_in_state(&db, "test-project", "Free", "ready");

        db.add_relation(blocked.id, dep.id, "depends_on").unwrap();

        let schedulable = db.get_schedulable_tasks("test-project").unwrap();
        let ids: Vec<i64> = schedulable.iter().map(|t| t.id).collect();
        assert!(!ids.contains(&blocked.id));
        assert!(ids.contains(&free.id));
    }

    #[test]
    fn test_get_schedulable_tasks_only_ready() {
        let db = TasksDb::open_memory().unwrap();
        // Active task should NOT appear in schedulable
        let _active = create_task_in_state(&db, "test-project", "Active", "active");
        // Interactive task should NOT appear
        let _interactive = create_task_in_state(&db, "test-project", "Interactive", "interactive");
        // Only this ready one should appear
        let ready = create_task_in_state(&db, "test-project", "Ready", "ready");

        let schedulable = db.get_schedulable_tasks("test-project").unwrap();
        assert_eq!(schedulable.len(), 1);
        assert_eq!(schedulable[0].id, ready.id);
    }

    #[test]
    fn test_get_schedulable_tasks_project_scoped() {
        let db = TasksDb::open_memory().unwrap();
        let _task_a = create_task_in_state(&db, "project-a", "Task A", "ready");
        let _task_b = create_task_in_state(&db, "project-b", "Task B", "ready");

        let schedulable = db.get_schedulable_tasks("project-a").unwrap();
        assert_eq!(schedulable.len(), 1);
        assert_eq!(schedulable[0].title, "Task A");
    }

    #[test]
    fn test_self_referential_relation_rejected() {
        let db = TasksDb::open_memory().unwrap();
        let task = db
            .create_task(
                "test-project",
                "Self ref",
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
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();

        let err = db.add_relation(task.id, task.id, "depends_on").unwrap_err();
        assert!(err.to_string().contains("relation from a task to itself"));
    }

    #[test]
    fn test_cross_project_relation_allowed() {
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
                None,
                false,
                crate::tasks_db::FiledBy::default(),
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
                None,
                false,
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();

        // Cross-project relations should succeed
        db.add_relation(t1.id, t2.id, "depends_on").unwrap();

        let relations = db.get_relations(t1.id).unwrap();
        assert_eq!(relations.len(), 1);
        assert_eq!(relations[0].from_task, t1.id);
        assert_eq!(relations[0].to_task, t2.id);
        assert_eq!(relations[0].relation, "depends_on");
    }

    #[test]
    fn test_cross_project_cycle_detection() {
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
                None,
                false,
                crate::tasks_db::FiledBy::default(),
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
                None,
                false,
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();

        // A depends_on B (cross-project) should work
        db.add_relation(t1.id, t2.id, "depends_on").unwrap();

        // B depends_on A (cross-project) should fail — cycle
        let err = db.add_relation(t2.id, t1.id, "depends_on").unwrap_err();
        assert!(err.to_string().contains("circular dependency"));
    }

    #[test]
    fn test_circular_dependency_direct() {
        let db = TasksDb::open_memory().unwrap();
        let t1 = db
            .create_task(
                "test-project",
                "Task 1",
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
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        let t2 = db
            .create_task(
                "test-project",
                "Task 2",
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
                crate::tasks_db::FiledBy::default(),
            )
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
            .create_task(
                "test-project",
                "Task 1",
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
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        let t2 = db
            .create_task(
                "test-project",
                "Task 2",
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
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        let t3 = db
            .create_task(
                "test-project",
                "Task 3",
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
                crate::tasks_db::FiledBy::default(),
            )
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
            .create_task(
                "test-project",
                "Task 1",
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
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        let t2 = db
            .create_task(
                "test-project",
                "Task 2",
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
                crate::tasks_db::FiledBy::default(),
            )
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
            .create_task(
                "test-project",
                "Task 1",
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
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        let t2 = db
            .create_task(
                "test-project",
                "Task 2",
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
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();

        assert!(!db.has_transitive_dependency(t1.id, t2.id).unwrap());
    }

    #[test]
    fn test_has_transitive_dependency_direct() {
        let db = TasksDb::open_memory().unwrap();
        let t1 = db
            .create_task(
                "test-project",
                "Task 1",
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
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        let t2 = db
            .create_task(
                "test-project",
                "Task 2",
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
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();

        db.add_relation(t1.id, t2.id, "depends_on").unwrap();

        assert!(db.has_transitive_dependency(t1.id, t2.id).unwrap());
        assert!(!db.has_transitive_dependency(t2.id, t1.id).unwrap());
    }

    #[test]
    fn test_has_transitive_dependency_chain() {
        let db = TasksDb::open_memory().unwrap();
        let t1 = db
            .create_task(
                "test-project",
                "Task 1",
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
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        let t2 = db
            .create_task(
                "test-project",
                "Task 2",
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
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        let t3 = db
            .create_task(
                "test-project",
                "Task 3",
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
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        let t4 = db
            .create_task(
                "test-project",
                "Task 4",
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
                crate::tasks_db::FiledBy::default(),
            )
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
        let dep1 = create_task_in_state(&db, "test-project", "Dep 1", "active");
        let dep2 = create_task_in_state(&db, "test-project", "Dep 2", "merged");
        let dep3 = create_task_in_state(&db, "test-project", "Dep 3", "ready");
        let task = create_task_in_state(&db, "test-project", "Main task", "ready");

        db.add_relation(task.id, dep1.id, "depends_on").unwrap();
        db.add_relation(task.id, dep2.id, "depends_on").unwrap();
        db.add_relation(task.id, dep3.id, "depends_on").unwrap();

        let blocking = db.get_blocking_dependencies(task.id).unwrap();
        // dep1 (active) and dep3 (ready) are blocking; dep2 (merged) is not
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
                None,
                false,
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        let b = db
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
                None,
                false,
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        let c = db
            .create_task(
                "p",
                "C",
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
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        let d = db
            .create_task(
                "p",
                "D",
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
                crate::tasks_db::FiledBy::default(),
            )
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
            .create_task(
                "p",
                "E",
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
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        db.add_relation(d.id, e.id, "depends_on").unwrap();
    }

    #[test]
    fn test_circular_dependency_mid_chain() {
        // Chain: A -> B -> C -> D -> E
        // Adding E -> C should be rejected (cycle C -> D -> E -> C)
        let db = TasksDb::open_memory().unwrap();
        let a = db
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
                None,
                false,
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        let b = db
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
                None,
                false,
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        let c = db
            .create_task(
                "p",
                "C",
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
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        let d = db
            .create_task(
                "p",
                "D",
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
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        let e = db
            .create_task(
                "p",
                "E",
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
                crate::tasks_db::FiledBy::default(),
            )
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
                None,
                false,
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        let b = db
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
                None,
                false,
                crate::tasks_db::FiledBy::default(),
            )
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
                None,
                false,
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        let b = db
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
                None,
                false,
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        let c = db
            .create_task(
                "p",
                "C",
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
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();

        db.add_relation(a.id, c.id, "depends_on").unwrap();
        db.add_relation(b.id, c.id, "depends_on").unwrap();
        // And C -> D is fine (no cycle)
        let d = db
            .create_task(
                "p",
                "D",
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
                crate::tasks_db::FiledBy::default(),
            )
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
            .create_task(
                "test-project",
                "Ready task",
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
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        db.update_task(
            t1.id,
            &TaskUpdate {
                state: Some(TaskState::Ready),
                ..Default::default()
            },
            None,
        )
        .unwrap();

        let t2 = db
            .create_task(
                "test-project",
                "Approved task",
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
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        db.update_task(
            t2.id,
            &TaskUpdate {
                state: Some(TaskState::Ready),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        db.assign_task(t2.id, "s1").unwrap();
        db.update_task(
            t2.id,
            &TaskUpdate {
                state: Some(TaskState::Approved),
                ..Default::default()
            },
            None,
        )
        .unwrap();

        let t3 = db
            .create_task(
                "test-project",
                "Another approved",
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
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        db.update_task(
            t3.id,
            &TaskUpdate {
                state: Some(TaskState::Ready),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        db.assign_task(t3.id, "s2").unwrap();
        db.update_task(
            t3.id,
            &TaskUpdate {
                state: Some(TaskState::Approved),
                ..Default::default()
            },
            None,
        )
        .unwrap();

        // All projects
        let approved = db.get_approved_tasks(None).unwrap();
        assert_eq!(approved.len(), 2);

        // Specific project
        let approved = db.get_approved_tasks(Some("test-project")).unwrap();
        assert_eq!(approved.len(), 2);

        // Non-existent project
        let approved = db.get_approved_tasks(Some("other")).unwrap();
        assert!(approved.is_empty());
    }

    #[test]
    fn test_get_approved_tasks_filters_by_project() {
        let db = TasksDb::open_memory().unwrap();

        let t1 = db
            .create_task(
                "project-a",
                "Task A",
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
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        db.update_task(
            t1.id,
            &TaskUpdate {
                state: Some(TaskState::Ready),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        db.assign_task(t1.id, "s1").unwrap();
        db.update_task(
            t1.id,
            &TaskUpdate {
                state: Some(TaskState::Approved),
                ..Default::default()
            },
            None,
        )
        .unwrap();

        let t2 = db
            .create_task(
                "project-b",
                "Task B",
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
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        db.update_task(
            t2.id,
            &TaskUpdate {
                state: Some(TaskState::Ready),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        db.assign_task(t2.id, "s2").unwrap();
        db.update_task(
            t2.id,
            &TaskUpdate {
                state: Some(TaskState::Approved),
                ..Default::default()
            },
            None,
        )
        .unwrap();

        let a_tasks = db.get_approved_tasks(Some("project-a")).unwrap();
        assert_eq!(a_tasks.len(), 1);
        assert_eq!(a_tasks[0].id, t1.id);

        let b_tasks = db.get_approved_tasks(Some("project-b")).unwrap();
        assert_eq!(b_tasks.len(), 1);
        assert_eq!(b_tasks[0].id, t2.id);

        let all_tasks = db.get_approved_tasks(None).unwrap();
        assert_eq!(all_tasks.len(), 2);
    }

    #[test]
    fn test_get_approved_tasks_priority_ordering() {
        let db = TasksDb::open_memory().unwrap();

        let t1 = db
            .create_task(
                "test-project",
                "Low priority",
                Some(1),
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
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        db.update_task(
            t1.id,
            &TaskUpdate {
                state: Some(TaskState::Ready),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        db.assign_task(t1.id, "s1").unwrap();
        db.update_task(
            t1.id,
            &TaskUpdate {
                state: Some(TaskState::Approved),
                ..Default::default()
            },
            None,
        )
        .unwrap();

        let t2 = db
            .create_task(
                "test-project",
                "High priority",
                Some(10),
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
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        db.update_task(
            t2.id,
            &TaskUpdate {
                state: Some(TaskState::Ready),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        db.assign_task(t2.id, "s2").unwrap();
        db.update_task(
            t2.id,
            &TaskUpdate {
                state: Some(TaskState::Approved),
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
            project_name: "test".into(),
            title: format!("Task {}", id),
            state: TaskState::Ready,
            priority,
            parent_id,
            tags: None,
            affected_files: None,
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
            filed_by_project: None,
            filed_by_session_id: None,
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
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        let child1 = db
            .create_task(
                "test-project",
                "Child 1",
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
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        let child2 = db
            .create_task(
                "test-project",
                "Child 2",
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
                crate::tasks_db::FiledBy::default(),
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
            .create_task(
                "test-project",
                "Root",
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
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        let child = db
            .create_task(
                "test-project",
                "Child",
                None,
                Some(root.id),
                None,
                false,
                "planning",
                false,
                None,
                None,
                false,
                None,
                false,
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        let grandchild = db
            .create_task(
                "test-project",
                "Grandchild",
                None,
                Some(child.id),
                None,
                false,
                "planning",
                false,
                None,
                None,
                false,
                None,
                false,
                crate::tasks_db::FiledBy::default(),
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
            .create_task(
                "test-project",
                "Leaf",
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
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();

        let descendants = db.get_descendant_tasks(task.id).unwrap();
        assert!(descendants.is_empty());
    }

    // ----- assign_task sets session_id tests -----

    #[test]
    fn test_assign_task_sets_session_id() {
        let db = TasksDb::open_memory().unwrap();
        let task = db
            .create_task(
                "test-project",
                "Test",
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
                crate::tasks_db::FiledBy::default(),
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
            .create_task(
                "test-project",
                "Test",
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
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        db.set_session_id(task.id, "s1").unwrap();
        assert_eq!(
            db.get_task(task.id).unwrap().unwrap().session_id.as_deref(),
            Some("s1")
        );

        db.clear_session_id(task.id).unwrap();
        assert!(db.get_task(task.id).unwrap().unwrap().session_id.is_none());
    }

    // ---------------------------------------------------------------
    // Task #577 — phase-completing transitions clear session_id
    // ---------------------------------------------------------------

    /// Helper: create a task with `initial_state = interactive`,
    /// transition it to `desired` state, and seed a stale
    /// `session_id` so tests can exercise the clearing logic.
    fn make_task_in_state(db: &TasksDb, title: &str, desired: &str) -> Task {
        let task = db
            .create_task(
                "test-project",
                title,
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
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        // Move through the states that are needed to reach `desired`.
        // `interactive -> desired` covers most one-hop transitions the
        // tests care about; for multi-hop chains we explicitly step.
        let path: &[TaskState] = match desired {
            "planning" => &[TaskState::Planning],
            "refining" => &[TaskState::Refining],
            "ready" => &[TaskState::Ready],
            "active" => &[TaskState::Ready, TaskState::Active],
            "review" => &[TaskState::Ready, TaskState::Active, TaskState::Review],
            other => panic!("make_task_in_state: unsupported target {}", other),
        };
        for step in path {
            let affected_files = if *step == TaskState::Ready {
                Some(serde_json::json!(["src/lib.rs"]))
            } else {
                None
            };
            db.update_task(
                task.id,
                &TaskUpdate {
                    state: Some(*step),
                    affected_files,
                    ..Default::default()
                },
                None,
            )
            .unwrap();
        }
        db.get_task(task.id).unwrap().unwrap()
    }

    #[test]
    fn test_transition_planning_to_refining_clears_session_id() {
        let db = TasksDb::open_memory().unwrap();
        let task = make_task_in_state(&db, "planning", "planning");
        db.set_session_id(task.id, "s-planner").unwrap();

        db.update_task(
            task.id,
            &TaskUpdate {
                state: Some(TaskState::Refining),
                ..Default::default()
            },
            None,
        )
        .unwrap();

        let got = db.get_task(task.id).unwrap().unwrap();
        assert_eq!(got.state, TaskState::Refining);
        assert!(
            got.session_id.is_none(),
            "planning -> refining should clear session_id (was {:?})",
            got.session_id
        );

        // History should contain a session_id clear entry.
        let mut stmt = db
            .conn
            .prepare(
                "SELECT field, old_value, new_value FROM task_history \
                 WHERE task_id = ?1 AND field = 'session_id' ORDER BY id",
            )
            .unwrap();
        let history: Vec<(String, Option<String>, Option<String>)> = stmt
            .query_map(params![task.id], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert!(
            history.iter().any(|(f, old, new)| f == "session_id"
                && old.as_deref() == Some("s-planner")
                && new.is_none()),
            "expected a history entry clearing session_id from s-planner, got {:?}",
            history
        );
    }

    #[test]
    fn test_transition_refining_to_ready_clears_session_id() {
        let db = TasksDb::open_memory().unwrap();
        let task = make_task_in_state(&db, "refining", "refining");
        db.set_session_id(task.id, "s-refiner").unwrap();

        db.update_task(
            task.id,
            &TaskUpdate {
                state: Some(TaskState::Ready),
                affected_files: Some(serde_json::json!(["src/foo.rs"])),
                ..Default::default()
            },
            None,
        )
        .unwrap();

        let got = db.get_task(task.id).unwrap().unwrap();
        assert_eq!(got.state, TaskState::Ready);
        assert!(
            got.session_id.is_none(),
            "refining -> ready should clear session_id (was {:?})",
            got.session_id
        );
    }

    #[test]
    fn test_transition_active_to_ready_clears_session_id() {
        let db = TasksDb::open_memory().unwrap();
        let task = make_task_in_state(&db, "active", "active");
        db.set_session_id(task.id, "s-worker").unwrap();

        db.update_task(
            task.id,
            &TaskUpdate {
                state: Some(TaskState::Ready),
                ..Default::default()
            },
            None,
        )
        .unwrap();

        let got = db.get_task(task.id).unwrap().unwrap();
        assert!(
            got.session_id.is_none(),
            "active -> ready should clear session_id (was {:?})",
            got.session_id
        );
    }

    #[test]
    fn test_direct_planning_to_ready_rejected_by_validator() {
        // The validator doesn't allow this transition at all — the
        // scheduler always routes through refining.  Guard against
        // a future loosening of validation that would bypass the
        // clear (callers must add the transition to
        // `should_clear_session_id_on_transition` too).
        assert!(!crate::tasks_state::validate_transition(
            TaskState::Planning,
            TaskState::Ready
        ));
        assert!(!crate::tasks_state::validate_transition(
            TaskState::Review,
            TaskState::Ready
        ));
    }

    #[test]
    fn test_transition_refining_to_planning_preserves_session_id() {
        // `refining -> planning` resumes the planner session via
        // `task.session_id` — must NOT clear.
        let db = TasksDb::open_memory().unwrap();
        let task = make_task_in_state(&db, "resume", "refining");
        db.set_session_id(task.id, "s-planner").unwrap();

        db.update_task(
            task.id,
            &TaskUpdate {
                state: Some(TaskState::Planning),
                ..Default::default()
            },
            None,
        )
        .unwrap();

        let got = db.get_task(task.id).unwrap().unwrap();
        assert_eq!(got.state, TaskState::Planning);
        assert_eq!(
            got.session_id.as_deref(),
            Some("s-planner"),
            "refining -> planning must preserve session_id so the \
             handler can resume the planner"
        );
    }

    #[test]
    fn test_transition_review_to_active_preserves_session_id() {
        // `review -> active` lets the worker resume; handler queues a
        // message at `task.session_id`, so must NOT clear.
        let db = TasksDb::open_memory().unwrap();
        let task = make_task_in_state(&db, "rework", "review");
        db.set_session_id(task.id, "s-worker").unwrap();

        db.update_task(
            task.id,
            &TaskUpdate {
                state: Some(TaskState::Active),
                ..Default::default()
            },
            None,
        )
        .unwrap();

        let got = db.get_task(task.id).unwrap().unwrap();
        assert_eq!(got.state, TaskState::Active);
        assert_eq!(got.session_id.as_deref(), Some("s-worker"));
    }

    #[test]
    fn test_clear_on_transition_is_idempotent_when_already_null() {
        // If `session_id` was already NULL (e.g. because a previous
        // watchdog cleared it), the transition-level clear must be a
        // no-op and not write a spurious history entry.
        let db = TasksDb::open_memory().unwrap();
        let task = make_task_in_state(&db, "clean", "refining");
        assert!(task.session_id.is_none());

        db.update_task(
            task.id,
            &TaskUpdate {
                state: Some(TaskState::Ready),
                affected_files: Some(serde_json::json!(["x.rs"])),
                ..Default::default()
            },
            None,
        )
        .unwrap();

        let mut stmt = db
            .conn
            .prepare(
                "SELECT COUNT(*) FROM task_history \
                 WHERE task_id = ?1 AND field = 'session_id'",
            )
            .unwrap();
        let count: i64 = stmt.query_row(params![task.id], |row| row.get(0)).unwrap();
        assert_eq!(
            count, 0,
            "no session_id history entries should be written when \
             the field was already NULL"
        );
    }

    // ----- planning/refining cycle tests -----

    #[test]
    fn test_planning_refining_cycle() {
        let db = TasksDb::open_memory().unwrap();
        let task = db
            .create_task(
                "test-project",
                "Planned task",
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
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        assert_eq!(task.state, TaskState::Interactive);

        // interactive -> planning
        db.update_task(
            task.id,
            &TaskUpdate {
                state: Some(TaskState::Planning),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        assert_eq!(
            db.get_task(task.id).unwrap().unwrap().state,
            TaskState::Planning
        );

        // planning -> refining
        db.update_task(
            task.id,
            &TaskUpdate {
                state: Some(TaskState::Refining),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        assert_eq!(
            db.get_task(task.id).unwrap().unwrap().state,
            TaskState::Refining
        );

        // refining -> planning (revision needed)
        db.update_task(
            task.id,
            &TaskUpdate {
                state: Some(TaskState::Planning),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        assert_eq!(
            db.get_task(task.id).unwrap().unwrap().state,
            TaskState::Planning
        );

        // planning -> refining -> ready
        db.update_task(
            task.id,
            &TaskUpdate {
                state: Some(TaskState::Refining),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        db.update_task(
            task.id,
            &TaskUpdate {
                state: Some(TaskState::Ready),
                affected_files: Some(serde_json::json!(["src/main.rs"])),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        assert_eq!(
            db.get_task(task.id).unwrap().unwrap().state,
            TaskState::Ready
        );
    }

    #[test]
    fn test_refining_to_interactive_scope_expansion() {
        let db = TasksDb::open_memory().unwrap();
        let task = db
            .create_task(
                "test-project",
                "Expanding scope",
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
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();

        // interactive -> planning -> refining -> interactive (scope expansion)
        for state in [
            TaskState::Planning,
            TaskState::Refining,
            TaskState::Interactive,
        ] {
            db.update_task(
                task.id,
                &TaskUpdate {
                    state: Some(state),
                    ..Default::default()
                },
                None,
            )
            .unwrap();
        }
        assert_eq!(
            db.get_task(task.id).unwrap().unwrap().state,
            TaskState::Interactive
        );
    }

    #[test]
    fn test_interactive_to_refining_directly() {
        let db = TasksDb::open_memory().unwrap();
        let task = db
            .create_task(
                "test-project",
                "Spec review",
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
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();

        // interactive -> refining (user already wrote spec, wants LLM review)
        db.update_task(
            task.id,
            &TaskUpdate {
                state: Some(TaskState::Refining),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        assert_eq!(
            db.get_task(task.id).unwrap().unwrap().state,
            TaskState::Refining
        );
    }

    #[test]
    fn test_planning_cannot_skip_to_active() {
        let db = TasksDb::open_memory().unwrap();
        let task = db
            .create_task(
                "test-project",
                "Bad skip",
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
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        db.update_task(
            task.id,
            &TaskUpdate {
                state: Some(TaskState::Planning),
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
                    state: Some(TaskState::Active),
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
            .create_task(
                "test-project",
                "No files",
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
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();

        // interactive -> planning -> refining
        db.update_task(
            task.id,
            &TaskUpdate {
                state: Some(TaskState::Planning),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        db.update_task(
            task.id,
            &TaskUpdate {
                state: Some(TaskState::Refining),
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
                    state: Some(TaskState::Ready),
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
            .create_task(
                "test-project",
                "Empty files",
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
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();

        // interactive -> planning -> refining, set affected_files to empty array
        db.update_task(
            task.id,
            &TaskUpdate {
                state: Some(TaskState::Planning),
                affected_files: Some(serde_json::json!([])),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        db.update_task(
            task.id,
            &TaskUpdate {
                state: Some(TaskState::Refining),
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
                    state: Some(TaskState::Ready),
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
            .create_task(
                "test-project",
                "Has files",
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
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();

        // interactive -> planning -> refining with affected_files set
        db.update_task(
            task.id,
            &TaskUpdate {
                state: Some(TaskState::Planning),
                affected_files: Some(serde_json::json!(["src/main.rs"])),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        db.update_task(
            task.id,
            &TaskUpdate {
                state: Some(TaskState::Refining),
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
                    state: Some(TaskState::Ready),
                    ..Default::default()
                },
                None,
            )
            .unwrap();
        assert_eq!(updated.state, TaskState::Ready);
    }

    #[test]
    fn test_refining_to_ready_succeeds_when_files_set_in_same_update() {
        let db = TasksDb::open_memory().unwrap();
        let task = db
            .create_task(
                "test-project",
                "Files in update",
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
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();

        // interactive -> planning -> refining (no affected_files yet)
        db.update_task(
            task.id,
            &TaskUpdate {
                state: Some(TaskState::Planning),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        db.update_task(
            task.id,
            &TaskUpdate {
                state: Some(TaskState::Refining),
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
                    state: Some(TaskState::Ready),
                    affected_files: Some(serde_json::json!(["src/lib.rs"])),
                    ..Default::default()
                },
                None,
            )
            .unwrap();
        assert_eq!(updated.state, TaskState::Ready);
    }

    #[test]
    fn test_universal_failed_override() {
        let db = TasksDb::open_memory().unwrap();

        // Any state -> failed should work
        for start_state in [
            TaskState::Interactive,
            TaskState::Planning,
            TaskState::Refining,
            TaskState::Ready,
            TaskState::Active,
            TaskState::Review,
        ] {
            let task = db
                .create_task(
                    "test-project",
                    &format!("Test {}", start_state),
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
                    crate::tasks_db::FiledBy::default(),
                )
                .unwrap();

            let transitions: &[TaskState] = match start_state {
                TaskState::Interactive => &[],
                TaskState::Planning => &[TaskState::Planning],
                TaskState::Refining => &[TaskState::Planning, TaskState::Refining],
                TaskState::Ready => &[TaskState::Ready],
                TaskState::Active => &[TaskState::Ready],
                TaskState::Review => &[TaskState::Ready],
                _ => unreachable!(),
            };
            for &s in transitions {
                db.update_task(
                    task.id,
                    &TaskUpdate {
                        state: Some(s),
                        ..Default::default()
                    },
                    None,
                )
                .unwrap();
            }
            match start_state {
                TaskState::Active | TaskState::Review => {
                    db.assign_task(task.id, "test-session").unwrap();
                    if start_state == TaskState::Review {
                        db.update_task(
                            task.id,
                            &TaskUpdate {
                                state: Some(TaskState::Review),
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
                    state: Some(TaskState::Failed),
                    ..Default::default()
                },
                None,
            )
            .unwrap();
            assert_eq!(
                db.get_task(task.id).unwrap().unwrap().state,
                TaskState::Failed
            );
        }
    }

    #[test]
    fn test_count_inflight_planning_without_session() {
        // Planning tasks without a session should NOT count as inflight
        let db = TasksDb::open_memory().unwrap();
        let project = "test-project";

        // Create a task and move it to planning state (interactive -> planning)
        let task = db
            .create_task(
                project,
                "Plan something",
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
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        db.update_task(
            task.id,
            &TaskUpdate {
                state: Some(TaskState::Planning),
                ..Default::default()
            },
            None,
        )
        .unwrap();

        // Planning without session -> should NOT count
        assert_eq!(db.count_inflight_tasks(project).unwrap(), 0);
    }

    #[test]
    fn test_count_inflight_planning_with_session() {
        // Planning tasks WITH a session should count as inflight
        let db = TasksDb::open_memory().unwrap();
        let project = "test-project";

        let task = db
            .create_task(
                project,
                "Plan something",
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
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        db.update_task(
            task.id,
            &TaskUpdate {
                state: Some(TaskState::Planning),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        db.set_session_id(task.id, "s123").unwrap();

        // Planning with session -> should count
        assert_eq!(db.count_inflight_tasks(project).unwrap(), 1);
    }

    #[test]
    fn test_count_inflight_active_always_counts() {
        // Active tasks always count regardless of session
        let db = TasksDb::open_memory().unwrap();
        let project = "test-project";

        let task = db
            .create_task(
                project,
                "Do work",
                None,
                None,
                None,
                false,
                "ready",
                false,
                None,
                None,
                false,
                None,
                false,
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        // Assign to move to active
        db.assign_task(task.id, "s456").unwrap();

        assert_eq!(db.count_inflight_tasks(project).unwrap(), 1);
    }

    #[test]
    fn test_count_inflight_cross_project_isolation() {
        // Inflight tasks in one project should not affect another
        let db = TasksDb::open_memory().unwrap();

        let task_a = db
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
                None,
                false,
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        db.update_task(
            task_a.id,
            &TaskUpdate {
                state: Some(TaskState::Ready),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        db.assign_task(task_a.id, "s100").unwrap();

        let task_b = db
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
                None,
                false,
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        db.update_task(
            task_b.id,
            &TaskUpdate {
                state: Some(TaskState::Planning),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        db.set_session_id(task_b.id, "s200").unwrap();

        assert_eq!(db.count_inflight_tasks("project-a").unwrap(), 1);
        assert_eq!(db.count_inflight_tasks("project-b").unwrap(), 1);
        assert_eq!(db.count_inflight_tasks("project-c").unwrap(), 0);
    }

    #[test]
    fn test_migrate_project_to_project_name() {
        // Simulate a database with the old "project" column name.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")
            .unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS tasks (
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
            CREATE TABLE IF NOT EXISTS task_history (
                id INTEGER PRIMARY KEY,
                task_id INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
                field TEXT NOT NULL,
                old_value TEXT,
                new_value TEXT,
                created_at INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_tasks_project_state ON tasks(project, state);
            INSERT INTO tasks (project, title, state, priority, skip_review, skip_planning, created_at, updated_at)
                VALUES ('test-project', 'Test task', 'active', 0, 0, 0, 1000, 1000);",
        )
        .unwrap();

        // Verify old column exists
        let has_old: bool = conn
            .prepare("SELECT COUNT(*) FROM pragma_table_info('tasks') WHERE name = 'project'")
            .and_then(|mut stmt| stmt.query_row([], |row| row.get::<_, i64>(0)))
            .map(|c| c > 0)
            .unwrap();
        assert!(has_old);

        // Run migration
        TasksDb::migrate(&conn).unwrap();

        // Verify new column exists and old is gone
        let has_new: bool = conn
            .prepare("SELECT COUNT(*) FROM pragma_table_info('tasks') WHERE name = 'project_name'")
            .and_then(|mut stmt| stmt.query_row([], |row| row.get::<_, i64>(0)))
            .map(|c| c > 0)
            .unwrap();
        assert!(has_new, "project_name column should exist after migration");

        let has_old_after: bool = conn
            .prepare("SELECT COUNT(*) FROM pragma_table_info('tasks') WHERE name = 'project'")
            .and_then(|mut stmt| stmt.query_row([], |row| row.get::<_, i64>(0)))
            .map(|c| c > 0)
            .unwrap();
        assert!(
            !has_old_after,
            "old project column should be gone after migration"
        );

        // Verify data is preserved
        let name: String = conn
            .query_row("SELECT project_name FROM tasks WHERE id = 1", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(name, "test-project");

        // Verify the legacy skip_planning column was dropped (task #512).
        let has_skip_planning: bool = conn
            .prepare("SELECT COUNT(*) FROM pragma_table_info('tasks') WHERE name = 'skip_planning'")
            .and_then(|mut stmt| stmt.query_row([], |row| row.get::<_, i64>(0)))
            .map(|c| c > 0)
            .unwrap();
        assert!(
            !has_skip_planning,
            "legacy skip_planning column should be dropped by migrate()"
        );
    }

    // ---------------------------------------------------------------
    // `held` flag (task #527)
    // ---------------------------------------------------------------

    #[test]
    fn test_create_task_held_persists() {
        let db = TasksDb::open_memory().unwrap();
        let task = db
            .create_task(
                "proj",
                "Parked",
                None,
                None,
                None,
                false,
                "ready",
                false,
                None,
                None,
                true, // held,
                None,
                false,
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();

        assert!(task.held, "newly-created task should carry the held flag");
        let loaded = db.get_task(task.id).unwrap().unwrap();
        assert!(loaded.held, "held flag must persist across reload");
    }

    #[test]
    fn test_get_schedulable_skips_held_tasks() {
        let db = TasksDb::open_memory().unwrap();
        let visible = db
            .create_task(
                "proj",
                "Visible",
                None,
                None,
                None,
                false,
                "ready",
                false,
                None,
                None,
                false,
                None,
                false,
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        let parked = db
            .create_task(
                "proj",
                "Parked",
                None,
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
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();

        let sched = db.get_schedulable_tasks("proj").unwrap();
        let ids: Vec<i64> = sched.iter().map(|t| t.id).collect();
        assert!(
            ids.contains(&visible.id),
            "unheld ready task should be schedulable"
        );
        assert!(
            !ids.contains(&parked.id),
            "held task must be excluded from get_schedulable_tasks"
        );
    }

    #[test]
    fn test_update_task_toggles_held() {
        let db = TasksDb::open_memory().unwrap();
        let task = db
            .create_task(
                "proj",
                "Toggle",
                None,
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
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        assert!(task.held);

        let released = db
            .update_task(
                task.id,
                &TaskUpdate {
                    held: Some(false),
                    ..Default::default()
                },
                None,
            )
            .unwrap();
        assert!(!released.held, "held=false should clear the flag");

        // Now get_schedulable_tasks should include it.
        let sched = db.get_schedulable_tasks("proj").unwrap();
        assert!(
            sched.iter().any(|t| t.id == task.id),
            "released task should become schedulable"
        );

        // Reapply the hold.
        let held_again = db
            .update_task(
                task.id,
                &TaskUpdate {
                    held: Some(true),
                    ..Default::default()
                },
                None,
            )
            .unwrap();
        assert!(held_again.held);
    }

    /// Task #751: setting `project_name` on `update_task` writes the new
    /// project to the row and records a history entry. The DB layer is
    /// dumb here — safety checks live in `handle_task_update` — so this
    /// test exercises only the column write.
    #[test]
    fn test_update_task_sets_project_name() {
        let db = TasksDb::open_memory().unwrap();
        let task = db
            .create_task(
                "old-project",
                "Reparent me",
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
                false,
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        assert_eq!(task.project_name, "old-project");

        let updated = db
            .update_task(
                task.id,
                &TaskUpdate {
                    project_name: Some("new-project".into()),
                    ..Default::default()
                },
                Some("s1"),
            )
            .unwrap();
        assert_eq!(updated.project_name, "new-project");

        // Reload to confirm the column actually persisted.
        let reloaded = db.get_task(task.id).unwrap().unwrap();
        assert_eq!(reloaded.project_name, "new-project");

        // History records the field change with both old and new values.
        let mut stmt = db
            .conn
            .prepare(
                "SELECT field, old_value, new_value, session_id
                 FROM task_history WHERE task_id = ?1 AND field = 'project_name'",
            )
            .expect("prepare history query");
        let row: (String, Option<String>, String, Option<String>) = stmt
            .query_row(params![task.id], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })
            .expect("history row exists");
        assert_eq!(row.0, "project_name");
        assert_eq!(row.1.as_deref(), Some("old-project"));
        assert_eq!(row.2, "new-project");
        assert_eq!(row.3.as_deref(), Some("s1"));
    }

    /// Task #751 regression: leaving `project_name = None` must not touch
    /// the column. The default-initialised TaskUpdate that every other
    /// caller uses must never accidentally clear/rewrite the project.
    #[test]
    fn test_update_task_leaves_project_name_unchanged_when_none() {
        let db = TasksDb::open_memory().unwrap();
        let task = db
            .create_task(
                "original",
                "Untouched",
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
                false,
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();

        let updated = db
            .update_task(
                task.id,
                &TaskUpdate {
                    title: Some("Renamed".into()),
                    project_name: None,
                    ..Default::default()
                },
                None,
            )
            .unwrap();
        assert_eq!(updated.project_name, "original");
        assert_eq!(updated.title, "Renamed");

        // No history entry for project_name.
        let count: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM task_history
                 WHERE task_id = ?1 AND field = 'project_name'",
                params![task.id],
                |row| row.get(0),
            )
            .expect("count history");
        assert_eq!(
            count, 0,
            "project_name = None must not touch the column or record history"
        );
    }

    #[test]
    fn test_held_migration_adds_column_to_legacy_db() {
        // Build a legacy schema that predates the held column.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE tasks (
                id INTEGER PRIMARY KEY,
                project_name TEXT NOT NULL,
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
                require_approval INTEGER NOT NULL DEFAULT 0,
                merge_target TEXT,
                sandbox_profile TEXT,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );
            CREATE TABLE task_history (
                id INTEGER PRIMARY KEY,
                task_id INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
                field TEXT NOT NULL,
                old_value TEXT,
                new_value TEXT,
                session_id TEXT,
                created_at INTEGER NOT NULL
            );",
        )
        .unwrap();

        // Before migration there is no held column.
        let has_held: bool = conn
            .prepare("SELECT COUNT(*) FROM pragma_table_info('tasks') WHERE name = 'held'")
            .and_then(|mut stmt| stmt.query_row([], |row| row.get::<_, i64>(0)))
            .map(|c| c > 0)
            .unwrap();
        assert!(!has_held);

        TasksDb::migrate(&conn).unwrap();

        let has_held: bool = conn
            .prepare("SELECT COUNT(*) FROM pragma_table_info('tasks') WHERE name = 'held'")
            .and_then(|mut stmt| stmt.query_row([], |row| row.get::<_, i64>(0)))
            .map(|c| c > 0)
            .unwrap();
        assert!(has_held, "migrate() should add the held column");
    }

    #[test]
    fn test_placeholder_session_id_migration_adds_column_to_legacy_db() {
        // Build a legacy schema that predates the placeholder_session_id
        // column (task #561).
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE tasks (
                id INTEGER PRIMARY KEY,
                project_name TEXT NOT NULL,
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
                require_approval INTEGER NOT NULL DEFAULT 0,
                merge_target TEXT,
                sandbox_profile TEXT,
                held INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );
            CREATE TABLE task_history (
                id INTEGER PRIMARY KEY,
                task_id INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
                field TEXT NOT NULL,
                old_value TEXT,
                new_value TEXT,
                session_id TEXT,
                created_at INTEGER NOT NULL
            );",
        )
        .unwrap();

        // Insert a row so we can confirm the migration leaves existing
        // data intact (with placeholder_session_id = NULL).
        conn.execute(
            "INSERT INTO tasks (project_name, title, state, created_at, updated_at) \
             VALUES ('proj', 'legacy task', 'ready', 0, 0)",
            [],
        )
        .unwrap();

        let has_col_before: bool = conn
            .prepare(
                "SELECT COUNT(*) FROM pragma_table_info('tasks') \
                 WHERE name = 'placeholder_session_id'",
            )
            .and_then(|mut stmt| stmt.query_row([], |row| row.get::<_, i64>(0)))
            .map(|c| c > 0)
            .unwrap();
        assert!(!has_col_before);

        TasksDb::migrate(&conn).unwrap();

        let has_col_after: bool = conn
            .prepare(
                "SELECT COUNT(*) FROM pragma_table_info('tasks') \
                 WHERE name = 'placeholder_session_id'",
            )
            .and_then(|mut stmt| stmt.query_row([], |row| row.get::<_, i64>(0)))
            .map(|c| c > 0)
            .unwrap();
        assert!(
            has_col_after,
            "migrate() should add the placeholder_session_id column"
        );

        // Legacy row preserved with NULL placeholder_session_id.
        let placeholder: Option<String> = conn
            .prepare("SELECT placeholder_session_id FROM tasks WHERE title = 'legacy task'")
            .and_then(|mut stmt| stmt.query_row([], |row| row.get(0)))
            .unwrap();
        assert!(placeholder.is_none());
    }

    #[test]
    fn test_set_placeholder_session_id_roundtrip() {
        let db = TasksDb::open_memory().unwrap();
        let task = db
            .create_task(
                "proj",
                "p",
                None,
                None,
                None,
                false,
                "ready",
                false,
                None,
                None,
                false,
                None,
                false,
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        assert!(task.placeholder_session_id.is_none());

        db.set_placeholder_session_id(task.id, "placeholder-sid")
            .unwrap();
        let got = db.get_task(task.id).unwrap().unwrap();
        assert_eq!(
            got.placeholder_session_id.as_deref(),
            Some("placeholder-sid")
        );
    }

    #[test]
    fn test_list_recent_by_state_orders_newest_first_and_limits() {
        let db = TasksDb::open_memory().unwrap();

        // Seed 4 merged tasks with distinct updated_at.
        let mut ids = Vec::new();
        for i in 0..4 {
            let t = db
                .create_task(
                    "proj",
                    &format!("merged #{}", i),
                    None,
                    None,
                    None,
                    false,
                    "ready",
                    false,
                    None,
                    None,
                    false,
                    None,
                    false,
                    crate::tasks_db::FiledBy::default(),
                )
                .unwrap();
            // Directly set state + updated_at to sidestep state-machine checks.
            db.conn
                .execute(
                    "UPDATE tasks SET state = 'merged', updated_at = ?1 WHERE id = ?2",
                    params![1_000_000i64 + i as i64 * 1000, t.id],
                )
                .unwrap();
            ids.push(t.id);
        }

        // Add a non-merged task that should NOT appear.
        let _other = db
            .create_task(
                "proj",
                "still ready",
                None,
                None,
                None,
                false,
                "ready",
                false,
                None,
                None,
                false,
                None,
                false,
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        // And a merged task in a different project that should be excluded.
        let other_proj = db
            .create_task(
                "other",
                "other merged",
                None,
                None,
                None,
                false,
                "ready",
                false,
                None,
                None,
                false,
                None,
                false,
                crate::tasks_db::FiledBy::default(),
            )
            .unwrap();
        db.conn
            .execute(
                "UPDATE tasks SET state = 'merged' WHERE id = ?1",
                params![other_proj.id],
            )
            .unwrap();

        let recent = db.list_recent_by_state("proj", "merged", 10).unwrap();
        assert_eq!(recent.len(), 4);
        // Newest first -> last inserted id first.
        assert_eq!(recent[0].id, ids[3]);
        assert_eq!(recent[1].id, ids[2]);
        assert_eq!(recent[2].id, ids[1]);
        assert_eq!(recent[3].id, ids[0]);

        // Limit is applied.
        let limited = db.list_recent_by_state("proj", "merged", 2).unwrap();
        assert_eq!(limited.len(), 2);
        assert_eq!(limited[0].id, ids[3]);
        assert_eq!(limited[1].id, ids[2]);

        // Other project is isolated.
        let other_recent = db.list_recent_by_state("other", "merged", 10).unwrap();
        assert_eq!(other_recent.len(), 1);
    }

    // ---------------------------------------------------------------
    // filed_by_project / filed_by_session_id (task #758)
    // ---------------------------------------------------------------

    /// `create_task` round-trips a populated `FiledBy` through
    /// `get_task` — both columns survive insert + reload.
    #[test]
    fn test_create_task_filed_by_round_trips() {
        let db = TasksDb::open_memory().unwrap();
        let task = db
            .create_task(
                "target-proj",
                "From elsewhere",
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
                FiledBy {
                    project: Some("caller-proj"),
                    session_id: Some("sCaller"),
                },
            )
            .unwrap();
        assert_eq!(task.filed_by_project.as_deref(), Some("caller-proj"));
        assert_eq!(task.filed_by_session_id.as_deref(), Some("sCaller"));
        // Sanity: target stays in `project_name`.
        assert_eq!(task.project_name, "target-proj");

        let loaded = db.get_task(task.id).unwrap().unwrap();
        assert_eq!(loaded.filed_by_project.as_deref(), Some("caller-proj"));
        assert_eq!(loaded.filed_by_session_id.as_deref(), Some("sCaller"));
    }

    /// `FiledBy::default()` produces NULLs in the columns — mirrors
    /// the historical pre-#758 behaviour for tests / fallback paths.
    #[test]
    fn test_create_task_filed_by_default_writes_null() {
        let db = TasksDb::open_memory().unwrap();
        let task = db
            .create_task(
                "proj",
                "No filer",
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
                FiledBy::default(),
            )
            .unwrap();
        assert!(task.filed_by_project.is_none());
        assert!(task.filed_by_session_id.is_none());
    }

    /// Migration from a pre-#758 schema (no `filed_by_*` columns) adds
    /// the columns and existing rows get NULL. New rows post-migration
    /// can populate them as usual.
    #[test]
    fn test_migrate_adds_filed_by_columns() {
        // Build a schema that matches the post-auto_downgraded shape
        // but is missing the new columns. We lean on the existing
        // migration's `ADD COLUMN ...` step; the rest of the schema
        // is the current one (the migration is idempotent for
        // already-present columns).
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")
            .unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS tasks (
                id INTEGER PRIMARY KEY,
                project_name TEXT NOT NULL,
                title TEXT NOT NULL,
                state TEXT NOT NULL DEFAULT 'interactive',
                priority INTEGER DEFAULT 0,
                parent_id INTEGER REFERENCES tasks(id),
                tags TEXT,
                affected_files TEXT,
                branch TEXT,
                merge_target TEXT,
                worktree_path TEXT,
                session_id TEXT,
                skip_review INTEGER NOT NULL DEFAULT 0,
                require_approval INTEGER NOT NULL DEFAULT 0,
                sandbox_profile TEXT,
                held INTEGER NOT NULL DEFAULT 0,
                placeholder_session_id TEXT,
                auto_downgraded_from_ready INTEGER NOT NULL DEFAULT 0,
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
            CREATE TABLE IF NOT EXISTS task_history (
                id INTEGER PRIMARY KEY,
                task_id INTEGER NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
                field TEXT NOT NULL,
                old_value TEXT,
                new_value TEXT,
                created_at INTEGER NOT NULL
            );
            INSERT INTO tasks (project_name, title, created_at, updated_at)
                VALUES ('proj', 'pre-migration row', 1000, 1000);",
        )
        .unwrap();

        // Sanity: columns are NOT present before migration.
        let has_col = |name: &str| -> bool {
            conn.prepare(&format!(
                "SELECT COUNT(*) FROM pragma_table_info('tasks') WHERE name = '{}'",
                name
            ))
            .and_then(|mut stmt| stmt.query_row([], |row| row.get::<_, i64>(0)))
            .map(|c| c > 0)
            .unwrap_or(false)
        };
        assert!(!has_col("filed_by_project"));
        assert!(!has_col("filed_by_session_id"));

        TasksDb::migrate(&conn).unwrap();

        assert!(
            has_col("filed_by_project"),
            "migration must add filed_by_project"
        );
        assert!(
            has_col("filed_by_session_id"),
            "migration must add filed_by_session_id"
        );

        // Existing row's new columns are NULL (we don't backfill).
        let (fp, fs): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT filed_by_project, filed_by_session_id FROM tasks WHERE id = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert!(fp.is_none(), "pre-migration row must keep NULL filer");
        assert!(fs.is_none(), "pre-migration row must keep NULL filer sid");

        // Post-migration writes can populate the columns. Use a raw
        // INSERT against the migrated `conn` so we don't need a
        // separate `TasksDb` constructor that wraps an existing
        // connection.
        let now = 2000_i64;
        conn.execute(
            "INSERT INTO tasks (project_name, title, filed_by_project, filed_by_session_id, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                "proj",
                "post-migration row",
                "caller",
                "s1",
                now,
                now,
            ],
        )
        .unwrap();
        let (fp2, fs2): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT filed_by_project, filed_by_session_id FROM tasks WHERE id = 2",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(fp2.as_deref(), Some("caller"));
        assert_eq!(fs2.as_deref(), Some("s1"));
    }
}
