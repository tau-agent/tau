//! SQLite-backed session persistence.
//!
//! Schema:
//! - `sessions`: one row per session (model, system prompt, metadata)
//! - `messages`: ordered messages per session, stored as JSON blobs

use std::collections::HashSet;
use std::path::PathBuf;

use rusqlite::{Connection, OptionalExtension, params};

use crate::types::{Message, Model, UserMessage};

/// Does this message, being the *last* persisted message of a session,
/// indicate that an agent turn was interrupted and should be resumed?
///
/// Matches:
/// * `User`       — agent was about to (or was) processing this turn
/// * `ToolResult` — agent was about to make the next LLM call
/// * `Assistant` with `stop_reason == ToolUse` — tool_uses dangling
///   (repair inserts stub tool_results, then resume runs the next call)
/// * `Assistant` with `stop_reason == Error` — stream died mid-flight;
///   retrying is safe because the stub would otherwise be the final word
pub(crate) fn message_indicates_incomplete_turn(msg: &Message) -> bool {
    use crate::types::StopReason;
    match msg {
        Message::User(_) | Message::ToolResult(_) => true,
        Message::Assistant(a) => matches!(a.stop_reason, StopReason::ToolUse | StopReason::Error),
        _ => false,
    }
}

/// Convert a rusqlite error into `crate::Error::Io` with a contextual prefix.
fn db_err(ctx: &str) -> impl FnOnce(rusqlite::Error) -> crate::Error + '_ {
    move |e| crate::Error::Io(format!("{}: {}", ctx, e))
}

/// Lightweight stats computed via SQL aggregates (no full JSON deserialisation).
#[derive(Debug, Clone, Default)]
pub struct DbSessionStats {
    pub message_count: usize,
    pub user_messages: usize,
    pub assistant_messages: usize,
    pub tool_calls: usize,
    pub tool_results: usize,
    pub tokens_input: u64,
    pub tokens_output: u64,
    pub tokens_cache_read: u64,
    pub tokens_cache_write: u64,
    pub cost: f64,
    pub last_message_time: Option<i64>,
    /// `input + cache_read + cache_write` from the last non-error assistant msg.
    pub last_input_tokens: Option<u64>,
}

/// Stored session metadata (no messages — those live in the messages table).
#[derive(Debug, Clone)]
pub struct StoredSession {
    pub id: String,
    pub model: Model,
    pub system_prompt: Option<String>,
    pub cwd: Option<String>,
    pub is_subscription: bool,
    pub created_at: i64,
    pub parent_id: Option<String>,
    pub child_budget: u32,
    pub tagline: Option<String>,
    pub archived: bool,
    /// How the most recent **finished** turn ended.
    /// `None` means the session has never completed a turn.
    /// Possible values: `"completed"`, `"error"`, `"cancelled"`, `"max_turns"`.
    /// Updated once per turn at completion; independent of `last_phase`.
    pub last_exit_status: Option<String>,
    /// Phase as of the most recent `emit_phase` call.
    /// **May be stale** if the server crashed or restarted mid-turn — a non-idle
    /// value does NOT imply a turn is currently running.  Use the in-memory
    /// `State::live_sessions` set (exposed as `SessionInfo::is_live`) to
    /// determine whether a session is genuinely active right now.
    pub last_phase: Option<String>,
    /// When true, auto-archive this session (and its subtree) after completion+join.
    pub auto_archive: bool,
    /// When true, notify parent session on child completion.
    pub notify_parent: bool,
    pub project_name: Option<String>,
}

/// Project-wide aggregate stats (totals across every session, archived
/// included).  Produced by [`Db::project_stats`].
#[derive(Debug, Clone, Default)]
pub struct DbProjectStats {
    /// Number of sessions (archived + active) belonging to the project.
    pub session_count: usize,
    /// Total messages across those sessions.
    pub message_count: usize,
    pub tokens_input: u64,
    pub tokens_output: u64,
    pub tokens_cache_read: u64,
    pub tokens_cache_write: u64,
    pub cost: f64,
    /// Unix-seconds timestamp of the most recent message across all
    /// sessions, or `None` when no sessions have any messages.
    pub last_message_time: Option<i64>,
}

/// Stored project metadata.
#[derive(Debug, Clone)]
pub struct Project {
    pub name: String,
    pub path: String,
    pub last_seen: i64,
    pub created_at: i64,
}

/// SELECT column list shared across all queries that return `StoredSession` rows.
const SESSION_COLUMNS: &str = "id, model_json, system_prompt, cwd, is_subscription, created_at, parent_id, child_budget, tagline, archived, last_exit_status, last_phase, auto_archive, notify_parent, project_name";

/// Map a `rusqlite::Row` (selected with [`SESSION_COLUMNS`]) into a [`StoredSession`].
fn row_to_session(row: &rusqlite::Row) -> rusqlite::Result<StoredSession> {
    let model_json: String = row.get(1)?;
    let model: Model = serde_json::from_str(&model_json).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(1, rusqlite::types::Type::Text, Box::new(e))
    })?;
    Ok(StoredSession {
        id: row.get(0)?,
        model,
        system_prompt: row.get(2)?,
        cwd: row.get(3)?,
        is_subscription: row.get::<_, i32>(4)? != 0,
        created_at: row.get(5)?,
        parent_id: row.get(6)?,
        child_budget: row.get::<_, i32>(7)? as u32,
        tagline: row.get(8)?,
        archived: row.get::<_, i32>(9)? != 0,
        last_exit_status: row.get(10)?,
        last_phase: row.get(11)?,
        auto_archive: row.get::<_, i32>(12)? != 0,
        notify_parent: row.get::<_, i32>(13)? != 0,
        project_name: row.get(14)?,
    })
}

pub struct Db {
    conn: Connection,
}

impl Db {
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
        let conn =
            Connection::open(path).map_err(db_err(&format!("open db {}", path.display())))?;

        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")
            .map_err(db_err("pragma"))?;

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS sessions (
                id             TEXT PRIMARY KEY,
                model_json     TEXT NOT NULL,
                system_prompt  TEXT,
                cwd            TEXT,
                is_subscription INTEGER NOT NULL DEFAULT 0,
                created_at     INTEGER NOT NULL,
                parent_id      TEXT,
                child_budget   INTEGER NOT NULL DEFAULT 16,
                tagline        TEXT,
                archived       INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE IF NOT EXISTS messages (
                id          INTEGER PRIMARY KEY,
                session_id  TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                message_json TEXT NOT NULL,
                created_at  INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_messages_session ON messages(session_id);
            CREATE TABLE IF NOT EXISTS queued_messages (
                id INTEGER PRIMARY KEY,
                target_session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                content TEXT NOT NULL,
                sender_info TEXT,
                created_at INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_queued_target ON queued_messages(target_session_id);
            CREATE TABLE IF NOT EXISTS projects (
                name        TEXT PRIMARY KEY,
                path        TEXT NOT NULL UNIQUE,
                last_seen   INTEGER NOT NULL,
                created_at  INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS migrations (
                name       TEXT PRIMARY KEY,
                applied_at INTEGER NOT NULL
            );",
        )
        .map_err(db_err("create tables"))?;

        // Migrations for existing DBs (ALTERs are no-ops if column already exists)
        let _ = conn.execute_batch("ALTER TABLE sessions ADD COLUMN cwd TEXT;");
        let _ = conn.execute_batch("ALTER TABLE sessions ADD COLUMN parent_id TEXT;");
        let _ = conn.execute_batch(
            "ALTER TABLE sessions ADD COLUMN child_budget INTEGER NOT NULL DEFAULT 16;",
        );
        let _ = conn.execute_batch("ALTER TABLE sessions ADD COLUMN tagline TEXT;");
        let _ = conn
            .execute_batch("ALTER TABLE sessions ADD COLUMN archived INTEGER NOT NULL DEFAULT 0;");
        let _ = conn.execute_batch("ALTER TABLE sessions ADD COLUMN last_exit_status TEXT;");
        let _ = conn.execute_batch("ALTER TABLE sessions ADD COLUMN last_phase TEXT;");
        let _ = conn.execute_batch(
            "ALTER TABLE sessions ADD COLUMN auto_archive INTEGER NOT NULL DEFAULT 0;",
        );
        let _ = conn.execute_batch(
            "ALTER TABLE sessions ADD COLUMN notify_parent INTEGER NOT NULL DEFAULT 1;",
        );
        let _ = conn.execute_batch("ALTER TABLE sessions ADD COLUMN project_name TEXT;");
        // Phase 1 seamless-restart: per-session opt-out for auto-resume.
        // When a session is explicitly marked `resume_on_restart = 0`, the
        // startup scan will skip it. Defaults to 1 so existing sessions
        // retain the legacy "resume-by-default" behaviour.
        let _ = conn.execute_batch(
            "ALTER TABLE sessions ADD COLUMN resume_on_restart INTEGER NOT NULL DEFAULT 1;",
        );

        // Create index after migrations ensure the column exists
        let _ = conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_sessions_parent ON sessions(parent_id);",
        );

        // queued_messages migration for existing DBs
        let _ = conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS queued_messages (
                id INTEGER PRIMARY KEY,
                target_session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                content TEXT NOT NULL,
                sender_info TEXT,
                created_at INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_queued_target ON queued_messages(target_session_id);",
        );

        Ok(Self { conn })
    }

    /// Open an in-memory database (for tests).
    #[cfg(test)]
    pub fn open_memory() -> crate::Result<Self> {
        let path = PathBuf::from(":memory:");
        let conn = Connection::open_in_memory().map_err(db_err("open in-memory db"))?;

        conn.execute_batch("PRAGMA foreign_keys=ON;")
            .map_err(db_err("pragma"))?;

        conn.execute_batch(
            "CREATE TABLE sessions (
                id             TEXT PRIMARY KEY,
                model_json     TEXT NOT NULL,
                system_prompt  TEXT,
                cwd            TEXT,
                is_subscription INTEGER NOT NULL DEFAULT 0,
                created_at     INTEGER NOT NULL,
                parent_id      TEXT,
                child_budget   INTEGER NOT NULL DEFAULT 16,
                tagline        TEXT,
                archived       INTEGER NOT NULL DEFAULT 0,
                last_exit_status TEXT,
                last_phase     TEXT,
                auto_archive   INTEGER NOT NULL DEFAULT 0,
                notify_parent  INTEGER NOT NULL DEFAULT 1,
                project_name   TEXT,
                resume_on_restart INTEGER NOT NULL DEFAULT 1
            );
            CREATE TABLE messages (
                id          INTEGER PRIMARY KEY,
                session_id  TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                message_json TEXT NOT NULL,
                created_at  INTEGER NOT NULL
            );
            CREATE INDEX idx_messages_session ON messages(session_id);
            CREATE INDEX idx_sessions_parent ON sessions(parent_id);
            CREATE TABLE queued_messages (
                id INTEGER PRIMARY KEY,
                target_session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                content TEXT NOT NULL,
                sender_info TEXT,
                created_at INTEGER NOT NULL
            );
            CREATE INDEX idx_queued_target ON queued_messages(target_session_id);
            CREATE TABLE projects (
                name        TEXT PRIMARY KEY,
                path        TEXT NOT NULL UNIQUE,
                last_seen   INTEGER NOT NULL,
                created_at  INTEGER NOT NULL
            );
            CREATE TABLE migrations (
                name       TEXT PRIMARY KEY,
                applied_at INTEGER NOT NULL
            );",
        )
        .map_err(db_err("create tables"))?;

        let _ = path; // suppress unused
        Ok(Self { conn })
    }

    /// Borrow the underlying SQLite connection.
    ///
    /// This is intended for read-only analytics queries (see [`crate::profile`])
    /// that don't fit cleanly behind a typed accessor on `Db`. Callers must not
    /// mutate any tables owned by `Db` itself; views and idempotent
    /// `CREATE VIEW IF NOT EXISTS` statements are fine.
    pub fn conn(&self) -> &rusqlite::Connection {
        &self.conn
    }

    // ----- sessions -----

    /// Create a session. Does not insert any messages.
    pub fn create_session(&self, session: &StoredSession) -> crate::Result<()> {
        let model_json = serde_json::to_string(&session.model)
            .map_err(|e| crate::Error::Parse(e.to_string()))?;
        self.conn
            .execute(
                "INSERT INTO sessions (id, model_json, system_prompt, cwd, is_subscription, created_at, parent_id, child_budget, tagline, archived, last_exit_status, last_phase, auto_archive, notify_parent, project_name)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
                params![
                    session.id,
                    model_json,
                    session.system_prompt,
                    session.cwd,
                    session.is_subscription as i32,
                    session.created_at,
                    session.parent_id,
                    session.child_budget,
                    session.tagline,
                    session.archived as i32,
                    session.last_exit_status,
                    session.last_phase,
                    session.auto_archive as i32,
                    session.notify_parent as i32,
                    session.project_name,
                ],
            )
            .map_err(db_err("insert session"))?;
        Ok(())
    }

    /// Load a session's metadata (without messages).
    pub fn get_session(&self, id: &str) -> crate::Result<Option<StoredSession>> {
        let sql = format!("SELECT {} FROM sessions WHERE id = ?1", SESSION_COLUMNS);
        self.conn
            .query_row(&sql, params![id], row_to_session)
            .optional()
            .map_err(db_err("get session"))
    }

    /// List all sessions (metadata only, no messages).
    /// List sessions (metadata only, no messages).
    /// If `include_archived` is false, archived sessions are excluded.
    pub fn list_sessions(&self, include_archived: bool) -> crate::Result<Vec<StoredSession>> {
        let sql = if include_archived {
            format!(
                "SELECT {} FROM sessions ORDER BY created_at",
                SESSION_COLUMNS
            )
        } else {
            format!(
                "SELECT {} FROM sessions WHERE archived = 0 ORDER BY created_at",
                SESSION_COLUMNS
            )
        };
        let mut stmt = self.conn.prepare(&sql).map_err(db_err("prepare list"))?;

        let rows = stmt
            .query_map([], row_to_session)
            .map_err(db_err("list sessions"))?;

        let mut sessions = Vec::new();
        for row in rows {
            sessions.push(row.map_err(db_err("read session row"))?);
        }
        Ok(sessions)
    }

    /// List sessions for a specific project.
    pub fn list_sessions_by_project(
        &self,
        project_name: &str,
        include_archived: bool,
    ) -> crate::Result<Vec<StoredSession>> {
        let sql = if include_archived {
            format!(
                "SELECT {} FROM sessions WHERE project_name = ?1 ORDER BY created_at",
                SESSION_COLUMNS
            )
        } else {
            format!(
                "SELECT {} FROM sessions WHERE project_name = ?1 AND archived = 0 ORDER BY created_at",
                SESSION_COLUMNS
            )
        };
        let mut stmt = self
            .conn
            .prepare(&sql)
            .map_err(db_err("prepare list by project"))?;

        let rows = stmt
            .query_map(params![project_name], row_to_session)
            .map_err(db_err("list sessions by project"))?;

        let mut sessions = Vec::new();
        for row in rows {
            sessions.push(row.map_err(db_err("read session row"))?);
        }
        Ok(sessions)
    }

    /// Get the timestamp of the last message in a session (or None if no messages).
    pub fn last_message_time(&self, session_id: &str) -> crate::Result<Option<i64>> {
        let result = self
            .conn
            .query_row(
                "SELECT MAX(created_at) FROM messages WHERE session_id = ?1",
                [session_id],
                |row| row.get::<_, Option<i64>>(0),
            )
            .map_err(db_err("last_message_time"))?;
        Ok(result)
    }

    /// Delete a session and all its messages (CASCADE).
    pub fn delete_session(&self, id: &str) -> crate::Result<()> {
        self.conn
            .execute("DELETE FROM sessions WHERE id = ?1", params![id])
            .map_err(db_err("delete session"))?;
        Ok(())
    }

    /// Apply a SQL statement to every session in the subtree rooted at `id`
    /// (inclusive), wrapped in a single transaction.
    ///
    /// `sql` must contain exactly one `?1` parameter placeholder bound to the
    /// session id.  `label` is used for error-context messages.
    fn apply_to_subtree(&self, id: &str, sql: &str, label: &str) -> crate::Result<()> {
        let ids = self.get_subtree_ids(id)?;
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(db_err(&format!("{} begin", label)))?;

        for sid in &ids {
            tx.execute(sql, params![sid])
                .map_err(db_err(&format!("{} session", label)))?;
        }

        tx.commit().map_err(db_err(&format!("{} commit", label)))?;
        Ok(())
    }

    /// Delete a session and all its descendants (recursive tree delete).
    ///
    /// Wrapped in a single transaction so a crash cannot leave orphaned subtrees.
    pub fn delete_session_tree(&self, id: &str) -> crate::Result<()> {
        self.apply_to_subtree(id, "DELETE FROM sessions WHERE id = ?1", "delete_tree")
    }

    /// Archive a session and all its descendants (recursive).
    ///
    /// Sets the `archived` flag to 1 for the entire subtree inside a single
    /// transaction so a crash cannot leave a partially-archived tree.
    pub fn archive_session_tree(&self, id: &str) -> crate::Result<()> {
        self.apply_to_subtree(
            id,
            "UPDATE sessions SET archived = 1 WHERE id = ?1",
            "archive_tree",
        )
    }

    /// Restore (un-archive) a session and all its descendants.
    ///
    /// Sets the `archived` flag to 0 for the entire subtree inside a single
    /// transaction.
    pub fn restore_session_tree(&self, id: &str) -> crate::Result<()> {
        self.apply_to_subtree(
            id,
            "UPDATE sessions SET archived = 0 WHERE id = ?1",
            "restore_tree",
        )
    }

    /// Collect all session IDs in the subtree rooted at `id` (inclusive).
    pub fn get_subtree_ids(&self, id: &str) -> crate::Result<Vec<String>> {
        let mut ids = vec![id.to_string()];
        let children = self.get_children(id)?;
        for child in &children {
            ids.extend(self.get_subtree_ids(&child.id)?);
        }
        Ok(ids)
    }

    /// Get direct children of a session.
    pub fn get_children(&self, parent_id: &str) -> crate::Result<Vec<StoredSession>> {
        let sql = format!(
            "SELECT {} FROM sessions WHERE parent_id = ?1 ORDER BY created_at",
            SESSION_COLUMNS
        );
        let mut stmt = self
            .conn
            .prepare(&sql)
            .map_err(db_err("prepare children"))?;

        let rows = stmt
            .query_map(params![parent_id], row_to_session)
            .map_err(db_err("list children"))?;

        let mut sessions = Vec::new();
        for row in rows {
            sessions.push(row.map_err(db_err("read child row"))?);
        }
        Ok(sessions)
    }

    /// Check if `session_id` is a descendant of `ancestor_id` by walking the
    /// parent_id chain.
    pub fn is_descendant(&self, session_id: &str, ancestor_id: &str) -> crate::Result<bool> {
        let mut current = session_id.to_string();
        loop {
            if current == ancestor_id {
                return Ok(true);
            }
            match self.get_session(&current)? {
                Some(session) => match session.parent_id {
                    Some(pid) => current = pid,
                    None => return Ok(false),
                },
                None => return Ok(false),
            }
        }
    }

    /// Return IDs of non-archived sessions whose last message indicates
    /// an incomplete turn (user waiting for model, tool_result waiting
    /// for next LLM call, or an assistant message that left tool_uses
    /// dangling / was cut off by a transport error). These are candidates
    /// for auto-resume on server startup.
    ///
    /// Filters:
    /// * `s.archived = 0`
    /// * `s.resume_on_restart = 1`  (users can opt out)
    /// * `s.last_exit_status IS NULL OR != 'completed'`
    ///   — already-completed turns are never retried
    /// * last message timestamp (or session creation) within the last
    ///   `max_age_secs` seconds — stale sessions stay dormant
    pub fn sessions_needing_resume(&self) -> crate::Result<Vec<String>> {
        // Default: 24h cutoff per the Phase 1 spec. Kept internal so the
        // public API stays ergonomic; tests use `sessions_needing_resume_with_cutoff`.
        const MAX_AGE_SECS: i64 = 24 * 3600;
        self.sessions_needing_resume_with_cutoff(MAX_AGE_SECS)
    }

    /// Same as [`sessions_needing_resume`] but with an explicit recency
    /// cutoff (seconds). A `max_age_secs <= 0` disables the filter.
    pub fn sessions_needing_resume_with_cutoff(
        &self,
        max_age_secs: i64,
    ) -> crate::Result<Vec<String>> {
        let now_ms = crate::types::timestamp_ms() as i64;
        let min_ts_ms = if max_age_secs > 0 {
            now_ms - max_age_secs * 1000
        } else {
            i64::MIN
        };

        let mut stmt = self
            .conn
            .prepare(
                "SELECT s.id, m.message_json, COALESCE(m.created_at, s.created_at) AS last_ts
                 FROM sessions s
                 LEFT JOIN messages m
                     ON m.session_id = s.id
                    AND m.id = (SELECT MAX(m2.id) FROM messages m2 WHERE m2.session_id = s.id)
                 WHERE s.archived = 0
                   AND COALESCE(s.resume_on_restart, 1) = 1
                   AND (s.last_exit_status IS NULL OR s.last_exit_status != 'completed')
                   AND COALESCE(m.created_at, s.created_at) >= ?1",
            )
            .map_err(db_err("prepare sessions_needing_resume"))?;

        let rows = stmt
            .query_map(params![min_ts_ms], |row| {
                let id: String = row.get(0)?;
                let json: Option<String> = row.get(1)?;
                Ok((id, json))
            })
            .map_err(db_err("query sessions_needing_resume"))?;

        let mut result = Vec::new();
        for row in rows {
            let (id, json) = row.map_err(db_err("read resume row"))?;
            let Some(json) = json else {
                // No messages yet — nothing to resume.
                continue;
            };
            let Ok(msg) = serde_json::from_str::<Message>(&json) else {
                continue;
            };
            if message_indicates_incomplete_turn(&msg) {
                result.push(id);
            }
        }
        Ok(result)
    }

    /// Set the `resume_on_restart` flag for a session.
    pub fn set_resume_on_restart(&self, session_id: &str, enabled: bool) -> crate::Result<()> {
        let v: i32 = if enabled { 1 } else { 0 };
        self.update_session_field(session_id, "resume_on_restart", &v)
    }

    /// Count direct non-archived children of a session.
    pub fn child_count(&self, session_id: &str) -> crate::Result<usize> {
        let count: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sessions WHERE parent_id = ?1 AND archived = 0",
                params![session_id],
                |row| row.get(0),
            )
            .map_err(db_err("child_count"))?;
        Ok(count as usize)
    }

    /// Compute budget_used for a session (sum of 1 + child_budget for each non-archived direct child).
    pub fn budget_used(&self, session_id: &str) -> crate::Result<u32> {
        let used: i64 = self
            .conn
            .query_row(
                "SELECT COALESCE(SUM(1 + child_budget), 0) FROM sessions WHERE parent_id = ?1 AND archived = 0",
                params![session_id],
                |row| row.get(0),
            )
            .map_err(db_err("budget_used"))?;
        Ok(used as u32)
    }

    /// Update a single column on the `sessions` table by id.
    fn update_session_field(
        &self,
        id: &str,
        column: &str,
        value: &dyn rusqlite::types::ToSql,
    ) -> crate::Result<()> {
        let sql = format!("UPDATE sessions SET {} = ?1 WHERE id = ?2", column);
        self.conn
            .execute(&sql, params![value, id])
            .map_err(db_err(&format!("update session {}", column)))?;
        Ok(())
    }

    /// Update the working directory for a session.
    pub fn update_cwd(&self, session_id: &str, cwd: &str) -> crate::Result<()> {
        self.update_session_field(session_id, "cwd", &cwd)
    }

    /// Re-parent all child sessions from one parent to another.
    pub fn reparent_children(&self, old_parent_id: &str, new_parent_id: &str) -> crate::Result<()> {
        self.conn
            .execute(
                "UPDATE sessions SET parent_id = ?1 WHERE parent_id = ?2",
                params![new_parent_id, old_parent_id],
            )
            .map_err(db_err("reparent children"))?;
        Ok(())
    }

    /// Update the model for a session.
    pub fn update_model(&self, session_id: &str, model: &crate::types::Model) -> crate::Result<()> {
        let model_json =
            serde_json::to_string(model).map_err(|e| crate::Error::Parse(e.to_string()))?;
        self.update_session_field(session_id, "model_json", &model_json)
    }

    /// Update the tagline for a session.
    pub fn update_tagline(&self, session_id: &str, tagline: &str) -> crate::Result<()> {
        self.update_session_field(session_id, "tagline", &tagline)
    }

    /// Update the system prompt for a session.
    pub fn update_system_prompt(&self, session_id: &str, system_prompt: &str) -> crate::Result<()> {
        self.update_session_field(session_id, "system_prompt", &system_prompt)
    }

    /// Update the last exit status for a session.
    pub fn update_exit_status(&self, session_id: &str, status: &str) -> crate::Result<()> {
        self.update_session_field(session_id, "last_exit_status", &status)
    }

    /// Update the persisted agent phase for a session.
    pub fn update_phase(&self, session_id: &str, phase: &str) -> crate::Result<()> {
        self.update_session_field(session_id, "last_phase", &phase)
    }

    /// Reset `last_phase` to `"idle"` for every session.
    /// Called on clean shutdown so that only crashes leave non-idle persisted phases.
    pub fn reset_all_phases(&self) -> crate::Result<()> {
        self.conn
            .execute("UPDATE sessions SET last_phase = 'idle' WHERE last_phase IS NOT NULL AND last_phase != 'idle'", [])
            .map_err(db_err("reset all phases"))?;
        Ok(())
    }

    /// Replace messages before `keep_from_id` with a compaction summary.
    /// Deletes old messages and inserts the summary in a transaction.
    pub fn compact_session(
        &self,
        session_id: &str,
        summary: &str,
        keep_from_id: i64,
        tokens_before: u64,
    ) -> crate::Result<()> {
        let summary_msg = Message::CompactionSummary(crate::types::CompactionSummaryMessage {
            summary: summary.to_string(),
            tokens_before,
            timestamp: crate::types::timestamp_ms(),
        });
        let summary_json =
            serde_json::to_string(&summary_msg).map_err(|e| crate::Error::Parse(e.to_string()))?;
        let now = crate::types::timestamp_ms() as i64;

        self.conn
            .execute_batch("BEGIN")
            .map_err(db_err("compact begin"))?;

        // Delete old messages (id < keep_from_id)
        self.conn
            .execute(
                "DELETE FROM messages WHERE session_id = ?1 AND id < ?2",
                params![session_id, keep_from_id],
            )
            .map_err(|e| {
                self.conn.execute_batch("ROLLBACK").ok();
                db_err("delete old messages")(e)
            })?;

        // Insert compaction summary before the kept messages
        // Use a negative id to ensure it sorts before existing messages
        self.conn
            .execute(
                "INSERT INTO messages (session_id, message_json, created_at) 
                 SELECT ?1, ?2, ?3
                 WHERE NOT EXISTS (
                     SELECT 1 FROM messages WHERE session_id = ?1 AND id < ?4
                 )",
                params![session_id, summary_json, now, keep_from_id],
            )
            .map_err(|e| {
                self.conn.execute_batch("ROLLBACK").ok();
                db_err("insert summary")(e)
            })?;

        self.conn
            .execute_batch("COMMIT")
            .map_err(db_err("compact commit"))?;

        Ok(())
    }

    /// Get the database row ID for a message at a given index in a session.
    pub fn get_message_row_id(&self, session_id: &str, index: usize) -> crate::Result<Option<i64>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id FROM messages WHERE session_id = ?1 ORDER BY id LIMIT 1 OFFSET ?2")
            .map_err(db_err("prepare message_row_id"))?;
        stmt.query_row(params![session_id, index as i64], |row| row.get(0))
            .optional()
            .map_err(db_err("get message_row_id"))
    }

    /// Get the next session id (max numeric suffix + 1).
    pub fn next_session_id(&self) -> crate::Result<String> {
        let max: Option<String> = self
            .conn
            .query_row(
                "SELECT id FROM sessions ORDER BY CAST(SUBSTR(id, 2) AS INTEGER) DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(db_err("next id"))?;

        let next = match max {
            Some(id) => {
                let n: u64 = id.trim_start_matches('s').parse().unwrap_or(0);
                n + 1
            }
            None => 1,
        };
        Ok(format!("s{}", next))
    }

    // ----- messages -----

    /// Append a message to a session.
    pub fn append_message(&self, session_id: &str, message: &Message) -> crate::Result<()> {
        let json =
            serde_json::to_string(message).map_err(|e| crate::Error::Parse(e.to_string()))?;
        let now = crate::types::timestamp_ms() as i64;
        self.conn
            .execute(
                "INSERT INTO messages (session_id, message_json, created_at) VALUES (?1, ?2, ?3)",
                params![session_id, json, now],
            )
            .map_err(db_err("insert message"))?;
        Ok(())
    }

    /// Load all messages for a session, ordered by insertion.
    pub fn get_messages(&self, session_id: &str) -> crate::Result<Vec<Message>> {
        let mut stmt = self
            .conn
            .prepare("SELECT message_json FROM messages WHERE session_id = ?1 ORDER BY id")
            .map_err(db_err("prepare messages"))?;

        let rows = stmt
            .query_map(params![session_id], |row| {
                let json: String = row.get(0)?;
                let msg: Message = serde_json::from_str(&json).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        0,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })?;
                Ok(msg)
            })
            .map_err(db_err("query messages"))?;

        let mut messages = Vec::new();
        for row in rows {
            messages.push(row.map_err(db_err("read message row"))?);
        }
        Ok(messages)
    }

    /// Count messages in a session.
    pub fn message_count(&self, session_id: &str) -> crate::Result<usize> {
        let count: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE session_id = ?1",
                params![session_id],
                |row| row.get(0),
            )
            .map_err(db_err("count messages"))?;
        Ok(count as usize)
    }

    /// Lightweight session stats computed via SQL — avoids deserializing every
    /// message JSON blob (the old `compute_stats` path).
    ///
    /// Returns `None` when the session has no messages at all.
    pub fn session_stats(&self, session_id: &str) -> crate::Result<Option<DbSessionStats>> {
        // Main aggregate: counts by role, token/cost sums from assistant usage.
        let row: Option<DbSessionStats> = self
            .conn
            .query_row(
                "SELECT
                    COUNT(*)                                                          AS message_count,
                    SUM(CASE WHEN json_extract(message_json, '$.role') = 'user'       THEN 1 ELSE 0 END) AS user_messages,
                    SUM(CASE WHEN json_extract(message_json, '$.role') = 'assistant'  THEN 1 ELSE 0 END) AS assistant_messages,
                    SUM(CASE WHEN json_extract(message_json, '$.role') = 'tool_result' THEN 1 ELSE 0 END) AS tool_results,
                    COALESCE(SUM(CASE WHEN json_extract(message_json, '$.role') = 'assistant'
                        THEN json_extract(message_json, '$.usage.input')       ELSE 0 END), 0) AS tokens_input,
                    COALESCE(SUM(CASE WHEN json_extract(message_json, '$.role') = 'assistant'
                        THEN json_extract(message_json, '$.usage.output')      ELSE 0 END), 0) AS tokens_output,
                    COALESCE(SUM(CASE WHEN json_extract(message_json, '$.role') = 'assistant'
                        THEN json_extract(message_json, '$.usage.cache_read')  ELSE 0 END), 0) AS tokens_cache_read,
                    COALESCE(SUM(CASE WHEN json_extract(message_json, '$.role') = 'assistant'
                        THEN json_extract(message_json, '$.usage.cache_write') ELSE 0 END), 0) AS tokens_cache_write,
                    COALESCE(SUM(CASE WHEN json_extract(message_json, '$.role') = 'assistant'
                        THEN json_extract(message_json, '$.usage.cost.total')  ELSE 0 END), 0.0) AS cost,
                    MAX(created_at)                                                   AS last_message_time
                 FROM messages
                 WHERE session_id = ?1",
                params![session_id],
                |row| {
                    let count: i64 = row.get(0)?;
                    if count == 0 {
                        return Ok(None);
                    }
                    Ok(Some(DbSessionStats {
                        message_count: count as usize,
                        user_messages: row.get::<_, i64>(1)? as usize,
                        assistant_messages: row.get::<_, i64>(2)? as usize,
                        tool_results: row.get::<_, i64>(3)? as usize,
                        tokens_input: row.get::<_, i64>(4)? as u64,
                        tokens_output: row.get::<_, i64>(5)? as u64,
                        tokens_cache_read: row.get::<_, i64>(6)? as u64,
                        tokens_cache_write: row.get::<_, i64>(7)? as u64,
                        cost: row.get(8)?,
                        last_message_time: row.get(9)?,
                        tool_calls: 0,           // filled in below
                        last_input_tokens: None,  // filled in below
                    }))
                },
            )
            .map_err(db_err("session_stats"))?;

        let Some(mut stats) = row else {
            return Ok(None);
        };

        // Tool-call count: count content-array elements with type=tool_call
        // across all assistant messages.
        stats.tool_calls = self
            .conn
            .query_row(
                "SELECT COALESCE(SUM(tc), 0) FROM (
                     SELECT (
                         SELECT COUNT(*) FROM json_each(json_extract(m.message_json, '$.content'))
                         WHERE json_extract(value, '$.type') = 'tool_call'
                     ) AS tc
                     FROM messages m
                     WHERE m.session_id = ?1
                       AND json_extract(m.message_json, '$.role') = 'assistant'
                 )",
                params![session_id],
                |row| row.get::<_, i64>(0),
            )
            .map_err(db_err("session_stats tool_calls"))? as usize;

        // Last input tokens: from the last assistant message that didn't error/abort.
        stats.last_input_tokens = self
            .conn
            .query_row(
                "SELECT json_extract(message_json, '$.usage.input'),
                        json_extract(message_json, '$.usage.cache_read'),
                        json_extract(message_json, '$.usage.cache_write')
                 FROM messages
                 WHERE session_id = ?1
                   AND json_extract(message_json, '$.role') = 'assistant'
                   AND json_extract(message_json, '$.stop_reason') NOT IN ('error', 'aborted')
                 ORDER BY id DESC
                 LIMIT 1",
                params![session_id],
                |row| {
                    let input: i64 = row.get(0)?;
                    let cache_read: i64 = row.get(1)?;
                    let cache_write: i64 = row.get(2)?;
                    Ok(Some((input + cache_read + cache_write) as u64))
                },
            )
            .optional()
            .map_err(db_err("session_stats last_input"))?
            .flatten();

        Ok(Some(stats))
    }

    /// Project-wide aggregate stats: totals across **every** session
    /// belonging to `project_name`, including archived ones.
    ///
    /// Archived sessions are intentionally included — they are part of
    /// historical spend.  Returns an all-zero `DbProjectStats` when the
    /// project has no sessions; this is not an error.
    pub fn project_stats(&self, project_name: &str) -> crate::Result<DbProjectStats> {
        let stats: DbProjectStats = self
            .conn
            .query_row(
                "SELECT
                    COUNT(DISTINCT s.id)                                      AS session_count,
                    COUNT(m.rowid)                                            AS message_count,
                    COALESCE(SUM(CASE WHEN json_extract(m.message_json, '$.role') = 'assistant'
                         THEN json_extract(m.message_json, '$.usage.input')       ELSE 0 END), 0) AS tokens_input,
                    COALESCE(SUM(CASE WHEN json_extract(m.message_json, '$.role') = 'assistant'
                         THEN json_extract(m.message_json, '$.usage.output')      ELSE 0 END), 0) AS tokens_output,
                    COALESCE(SUM(CASE WHEN json_extract(m.message_json, '$.role') = 'assistant'
                         THEN json_extract(m.message_json, '$.usage.cache_read')  ELSE 0 END), 0) AS tokens_cache_read,
                    COALESCE(SUM(CASE WHEN json_extract(m.message_json, '$.role') = 'assistant'
                         THEN json_extract(m.message_json, '$.usage.cache_write') ELSE 0 END), 0) AS tokens_cache_write,
                    COALESCE(SUM(CASE WHEN json_extract(m.message_json, '$.role') = 'assistant'
                         THEN json_extract(m.message_json, '$.usage.cost.total')  ELSE 0 END), 0.0) AS cost,
                    MAX(m.created_at)                                         AS last_message_time
                 FROM sessions s
                 LEFT JOIN messages m ON m.session_id = s.id
                 WHERE s.project_name = ?1",
                params![project_name],
                |row| {
                    Ok(DbProjectStats {
                        session_count: row.get::<_, i64>(0)? as usize,
                        message_count: row.get::<_, i64>(1)? as usize,
                        tokens_input: row.get::<_, i64>(2)? as u64,
                        tokens_output: row.get::<_, i64>(3)? as u64,
                        tokens_cache_read: row.get::<_, i64>(4)? as u64,
                        tokens_cache_write: row.get::<_, i64>(5)? as u64,
                        cost: row.get(6)?,
                        last_message_time: row.get(7)?,
                    })
                },
            )
            .map_err(db_err("project_stats"))?;
        Ok(stats)
    }

    // ----- queued_messages -----

    /// Queue a message for delivery to a target session.
    /// Returns the row id of the inserted message.
    pub fn queue_message(
        &self,
        target_session_id: &str,
        content: &str,
        sender_info: &str,
    ) -> crate::Result<i64> {
        let now = crate::types::timestamp_ms() as i64;
        self.conn
            .execute(
                "INSERT INTO queued_messages (target_session_id, content, sender_info, created_at)
                 VALUES (?1, ?2, ?3, ?4)",
                params![target_session_id, content, sender_info, now],
            )
            .map_err(db_err("queue_message"))?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Drain all queued messages for a session: atomically SELECT, INSERT as
    /// User messages into the messages table, and DELETE from queued_messages.
    /// Returns the persisted `Message::User` entries.
    pub fn drain_queued_messages(&self, session_id: &str) -> crate::Result<Vec<Message>> {
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(db_err("drain begin"))?;

        let rows: Vec<(i64, String, Option<String>)> = {
            let mut stmt = tx
                .prepare(
                    "SELECT id, content, sender_info FROM queued_messages
                     WHERE target_session_id = ?1 ORDER BY id",
                )
                .map_err(db_err("drain select"))?;
            let mapped = stmt
                .query_map(params![session_id], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                })
                .map_err(db_err("drain query"))?;
            mapped
                .collect::<Result<Vec<_>, _>>()
                .map_err(db_err("drain row"))?
        };

        if rows.is_empty() {
            return Ok(Vec::new());
        }

        let mut messages = Vec::with_capacity(rows.len());
        let now = crate::types::timestamp_ms() as i64;
        let mut ids = Vec::with_capacity(rows.len());

        for (id, content, sender_info) in &rows {
            ids.push(*id);
            let text = if let Some(info) = sender_info {
                format!("[from {}] {}", info, content)
            } else {
                content.clone()
            };
            let msg = Message::User(UserMessage::text(&text));
            let json =
                serde_json::to_string(&msg).map_err(|e| crate::Error::Parse(e.to_string()))?;
            tx.execute(
                "INSERT INTO messages (session_id, message_json, created_at)
                     VALUES (?1, ?2, ?3)",
                params![session_id, json, now],
            )
            .map_err(db_err("drain insert message"))?;
            messages.push(msg);
        }

        // Delete drained rows by collected IDs
        for id in &ids {
            tx.execute("DELETE FROM queued_messages WHERE id = ?1", params![id])
                .map_err(db_err("drain delete"))?;
        }

        tx.commit().map_err(db_err("drain commit"))?;

        Ok(messages)
    }

    /// Check whether a session has any queued messages.
    pub fn has_queued_messages(&self, session_id: &str) -> crate::Result<bool> {
        let exists: bool = self
            .conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM queued_messages WHERE target_session_id = ?1)",
                params![session_id],
                |row| row.get(0),
            )
            .map_err(db_err("has_queued_messages"))?;
        Ok(exists)
    }

    /// Delete archived sessions older than `older_than_ms` (millisecond timestamp).
    ///
    /// Relies on `ON DELETE CASCADE` to clean up associated messages and
    /// queued_messages rows.  Returns the number of sessions deleted.
    pub fn gc_archived_sessions(&self, older_than_ms: u64) -> crate::Result<usize> {
        let count = self
            .conn
            .execute(
                "DELETE FROM sessions WHERE archived = 1 AND created_at < ?1",
                params![older_than_ms as i64],
            )
            .map_err(db_err("gc_archived_sessions"))?;
        Ok(count)
    }

    /// Return ids of sessions that are candidates for empty-session GC.
    ///
    /// A row qualifies iff:
    /// - `archived = 0` (we never auto-delete archived sessions — the
    ///   user explicitly archived those; it's their call when to
    ///   evict them, handled by [`Self::gc_archived_sessions`]).
    /// - There are no rows in `messages` for this session id.
    /// - There is no row in `sessions` whose `parent_id` equals this
    ///   id.  The child check intentionally ignores `c.archived`:
    ///   even an archived child counts as a "do not delete the
    ///   parent" signal.
    /// - `created_at < now_ms - grace_secs * 1000`, so we don't race
    ///   a session that was just created and is about to receive its
    ///   first message.
    ///
    /// Caller is responsible for excluding live sessions and sessions
    /// referenced by active tasks; see [`Self::gc_empty_sessions`].
    pub fn list_empty_sessions(&self, grace_secs: i64) -> crate::Result<Vec<String>> {
        let now_ms = crate::types::timestamp_ms() as i64;
        let cutoff = now_ms.saturating_sub(grace_secs.saturating_mul(1000));
        let mut stmt = self
            .conn
            .prepare(
                "SELECT s.id
                 FROM sessions s
                 WHERE s.archived = 0
                   AND s.created_at < ?1
                   AND NOT EXISTS (SELECT 1 FROM messages m WHERE m.session_id = s.id)
                   AND NOT EXISTS (SELECT 1 FROM sessions c WHERE c.parent_id  = s.id)",
            )
            .map_err(db_err("prepare list_empty_sessions"))?;
        let rows = stmt
            .query_map(params![cutoff], |row| row.get::<_, String>(0))
            .map_err(db_err("list_empty_sessions"))?;
        let mut ids = Vec::new();
        for row in rows {
            ids.push(row.map_err(db_err("list_empty_sessions row"))?);
        }
        Ok(ids)
    }

    /// GC empty sessions, excluding any id present in `live` or
    /// `task_owned`.
    ///
    /// Calls [`Self::delete_session`] (not the tree variant — by
    /// definition there are no children).  Per-id delete failures are
    /// logged at `warn` and skipped; one bad row will not abort the
    /// batch.  Returns the ids that were *successfully* deleted, for
    /// caller-side info-level logging.
    pub fn gc_empty_sessions(
        &self,
        grace_secs: i64,
        live: &HashSet<String>,
        task_owned: &HashSet<String>,
    ) -> crate::Result<Vec<String>> {
        let candidates = self.list_empty_sessions(grace_secs)?;
        let mut deleted = Vec::new();
        for id in candidates {
            if live.contains(&id) || task_owned.contains(&id) {
                continue;
            }
            match self.delete_session(&id) {
                Ok(()) => deleted.push(id),
                Err(e) => {
                    tracing::warn!(
                        session_id = %id,
                        error = %e,
                        "gc_empty_sessions: delete_session failed; skipping"
                    );
                }
            }
        }
        Ok(deleted)
    }

    /// Return session IDs that have pending queued messages.
    pub fn sessions_with_queued_messages(&self) -> crate::Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT DISTINCT target_session_id FROM queued_messages")
            .map_err(db_err("sessions_with_queued"))?;
        let rows = stmt
            .query_map([], |row| row.get(0))
            .map_err(db_err("sessions_with_queued query"))?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row.map_err(db_err("sessions_with_queued row"))?);
        }
        Ok(result)
    }

    // ----- projects -----

    /// Create a new project. Errors on duplicate name or path.
    pub fn create_project(&self, name: &str, path: &str) -> crate::Result<Project> {
        let now = crate::types::timestamp_ms() as i64;
        self.conn
            .execute(
                "INSERT INTO projects (name, path, last_seen, created_at) VALUES (?1, ?2, ?3, ?4)",
                params![name, path, now, now],
            )
            .map_err(db_err("insert project"))?;
        Ok(Project {
            name: name.to_string(),
            path: path.to_string(),
            last_seen: now,
            created_at: now,
        })
    }

    /// Get a project by name.
    pub fn get_project(&self, name: &str) -> crate::Result<Option<Project>> {
        self.conn
            .query_row(
                "SELECT name, path, last_seen, created_at FROM projects WHERE name = ?1",
                params![name],
                |row| {
                    Ok(Project {
                        name: row.get(0)?,
                        path: row.get(1)?,
                        last_seen: row.get(2)?,
                        created_at: row.get(3)?,
                    })
                },
            )
            .optional()
            .map_err(db_err("get project"))
    }

    /// Get a project by its path.
    pub fn get_project_by_path(&self, path: &str) -> crate::Result<Option<Project>> {
        self.conn
            .query_row(
                "SELECT name, path, last_seen, created_at FROM projects WHERE path = ?1",
                params![path],
                |row| {
                    Ok(Project {
                        name: row.get(0)?,
                        path: row.get(1)?,
                        last_seen: row.get(2)?,
                        created_at: row.get(3)?,
                    })
                },
            )
            .optional()
            .map_err(db_err("get project by path"))
    }

    /// List all projects, most recently seen first.
    pub fn list_projects(&self) -> crate::Result<Vec<Project>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT name, path, last_seen, created_at FROM projects ORDER BY last_seen DESC",
            )
            .map_err(db_err("prepare list projects"))?;
        let rows = stmt
            .query_map([], |row| {
                Ok(Project {
                    name: row.get(0)?,
                    path: row.get(1)?,
                    last_seen: row.get(2)?,
                    created_at: row.get(3)?,
                })
            })
            .map_err(db_err("list projects"))?;
        let mut projects = Vec::new();
        for row in rows {
            projects.push(row.map_err(db_err("read project row"))?);
        }
        Ok(projects)
    }

    /// Rename a project.
    pub fn rename_project(&self, old_name: &str, new_name: &str) -> crate::Result<()> {
        self.conn
            .execute(
                "UPDATE projects SET name = ?1 WHERE name = ?2",
                params![new_name, old_name],
            )
            .map_err(db_err("rename project"))?;
        Ok(())
    }

    /// Update the last_seen timestamp for a project to now.
    pub fn update_project_last_seen(&self, name: &str) -> crate::Result<()> {
        let now = crate::types::timestamp_ms() as i64;
        self.conn
            .execute(
                "UPDATE projects SET last_seen = ?1 WHERE name = ?2",
                params![now, name],
            )
            .map_err(db_err("update project last_seen"))?;
        Ok(())
    }

    /// Delete a project by name.
    pub fn delete_project(&self, name: &str) -> crate::Result<()> {
        self.conn
            .execute("DELETE FROM projects WHERE name = ?1", params![name])
            .map_err(db_err("delete project"))?;
        Ok(())
    }

    // ----- migrations -----

    /// Ensure the migrations table exists (for CLI usage outside of Db::open).
    pub fn ensure_migrations_table(&self) -> crate::Result<()> {
        self.conn
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS migrations (
                    name       TEXT PRIMARY KEY,
                    applied_at INTEGER NOT NULL
                );",
            )
            .map_err(db_err("create migrations table"))?;
        Ok(())
    }

    /// Check if a named migration has been applied.
    pub fn has_migration(&self, name: &str) -> crate::Result<bool> {
        let exists: bool = self
            .conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM migrations WHERE name = ?1)",
                params![name],
                |row| row.get(0),
            )
            .map_err(db_err("has_migration"))?;
        Ok(exists)
    }

    /// Record a migration as having been applied.
    pub fn record_migration(&self, name: &str) -> crate::Result<()> {
        let now = crate::types::timestamp_ms() as i64;
        self.conn
            .execute(
                "INSERT OR IGNORE INTO migrations (name, applied_at) VALUES (?1, ?2)",
                params![name, now],
            )
            .map_err(db_err("record_migration"))?;
        Ok(())
    }

    // ----- migration helpers -----

    /// Get all sessions with their `cwd` (including archived sessions).
    pub fn get_all_session_cwds(&self) -> crate::Result<Vec<(String, Option<String>)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, cwd FROM sessions")
            .map_err(db_err("prepare get_all_session_cwds"))?;
        let rows = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .map_err(db_err("query get_all_session_cwds"))?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row.map_err(db_err("read session cwd row"))?);
        }
        Ok(result)
    }

    /// Set the `project_name` for a session.
    pub fn set_session_project_name(&self, id: &str, name: &str) -> crate::Result<()> {
        self.update_session_field(id, "project_name", &name)
    }

    /// Run a closure inside a transaction.
    ///
    /// The closure receives a reference to `Self` — all DB operations within
    /// the closure share the same transaction. Commits on success, rolls back
    /// on error.
    pub fn in_transaction<F, T>(&self, f: F) -> crate::Result<T>
    where
        F: FnOnce(&Self) -> crate::Result<T>,
    {
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(db_err("begin transaction"))?;
        let result = f(self)?;
        tx.commit().map_err(db_err("commit transaction"))?;
        Ok(result)
    }
}

fn default_db_path() -> PathBuf {
    crate::paths::data_dir().join("tau.db")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::*;

    fn test_model() -> Model {
        Model {
            id: "test-model".into(),
            name: "Test".into(),
            api: "test".into(),
            provider: "test".into(),
            base_url: "http://localhost".into(),
            thinking: ThinkingStyle::None,
            cost: ModelCost {
                input: 1.0,
                output: 2.0,
                cache_read: 0.1,
                cache_write: 1.25,
            },
            context_window: 100_000,
            max_tokens: 4096,
            headers: Default::default(),
        }
    }

    #[test]
    fn create_and_load_session() {
        let db = Db::open_memory().unwrap();
        let session = StoredSession {
            id: "s1".into(),
            model: test_model(),
            system_prompt: Some("Be helpful.".into()),
            cwd: None,
            is_subscription: true,
            created_at: 1000,
            parent_id: None,
            child_budget: 0,
            tagline: None,
            archived: false,
            last_exit_status: None,
            last_phase: None,
            auto_archive: false,
            notify_parent: true,
            project_name: None,
        };
        db.create_session(&session).unwrap();

        let loaded = db.get_session("s1").unwrap().unwrap();
        assert_eq!(loaded.id, "s1");
        assert_eq!(loaded.model.id, "test-model");
        assert_eq!(loaded.system_prompt.as_deref(), Some("Be helpful."));
        assert!(loaded.is_subscription);
    }

    #[test]
    fn append_and_load_messages() {
        let db = Db::open_memory().unwrap();
        let session = StoredSession {
            id: "s1".into(),
            model: test_model(),
            system_prompt: None,
            cwd: None,
            is_subscription: false,
            created_at: 1000,
            parent_id: None,
            child_budget: 0,
            tagline: None,
            archived: false,
            last_exit_status: None,
            last_phase: None,
            auto_archive: false,
            notify_parent: true,
            project_name: None,
        };
        db.create_session(&session).unwrap();

        let user_msg = Message::User(UserMessage::text("hello"));
        let mut assistant = AssistantMessage::empty("test", "test", "test-model");
        assistant.usage.input = 100;
        assistant.usage.output = 50;
        let assistant_msg = Message::Assistant(assistant);

        db.append_message("s1", &user_msg).unwrap();
        db.append_message("s1", &assistant_msg).unwrap();

        let messages = db.get_messages("s1").unwrap();
        assert_eq!(messages.len(), 2);
        assert!(matches!(&messages[0], Message::User(_)));
        assert!(matches!(&messages[1], Message::Assistant(_)));

        assert_eq!(db.message_count("s1").unwrap(), 2);
    }

    #[test]
    fn delete_cascades() {
        let db = Db::open_memory().unwrap();
        let session = StoredSession {
            id: "s1".into(),
            model: test_model(),
            system_prompt: None,
            cwd: None,
            is_subscription: false,
            created_at: 1000,
            parent_id: None,
            child_budget: 0,
            tagline: None,
            archived: false,
            last_exit_status: None,
            last_phase: None,
            auto_archive: false,
            notify_parent: true,
            project_name: None,
        };
        db.create_session(&session).unwrap();
        db.append_message("s1", &Message::User(UserMessage::text("hi")))
            .unwrap();

        db.delete_session("s1").unwrap();
        assert!(db.get_session("s1").unwrap().is_none());
        assert_eq!(db.get_messages("s1").unwrap().len(), 0);
    }

    #[test]
    fn list_sessions_ordered() {
        let db = Db::open_memory().unwrap();
        for (id, ts) in [("s2", 2000i64), ("s1", 1000), ("s3", 3000)] {
            db.create_session(&StoredSession {
                id: id.into(),
                model: test_model(),
                system_prompt: None,
                cwd: None,
                is_subscription: false,
                created_at: ts,
                parent_id: None,
                child_budget: 0,
                tagline: None,
                archived: false,
                last_exit_status: None,
                last_phase: None,
                auto_archive: false,
                notify_parent: true,
                project_name: None,
            })
            .unwrap();
        }
        let sessions = db.list_sessions(true).unwrap();
        let ids: Vec<&str> = sessions.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["s1", "s2", "s3"]);
    }

    #[test]
    fn next_session_id() {
        let db = Db::open_memory().unwrap();
        assert_eq!(db.next_session_id().unwrap(), "s1");

        db.create_session(&StoredSession {
            id: "s5".into(),
            model: test_model(),
            system_prompt: None,
            cwd: None,
            is_subscription: false,
            created_at: 1000,
            parent_id: None,
            child_budget: 0,
            tagline: None,
            archived: false,
            last_exit_status: None,
            last_phase: None,
            auto_archive: false,
            notify_parent: true,
            project_name: None,
        })
        .unwrap();
        assert_eq!(db.next_session_id().unwrap(), "s6");
    }

    #[test]
    fn child_sessions_and_budget() {
        let db = Db::open_memory().unwrap();

        // Create parent with budget of 5
        db.create_session(&StoredSession {
            id: "root".into(),
            model: test_model(),
            system_prompt: None,
            cwd: None,
            is_subscription: false,
            created_at: 1000,
            parent_id: None,
            child_budget: 5,
            tagline: None,
            archived: false,
            last_exit_status: None,
            last_phase: None,
            auto_archive: false,
            notify_parent: true,
            project_name: None,
        })
        .unwrap();

        // budget_used starts at 0
        assert_eq!(db.budget_used("root").unwrap(), 0);
        assert_eq!(db.child_count("root").unwrap(), 0);

        // Create a leaf child (budget=0, cost=1)
        db.create_session(&StoredSession {
            id: "c1".into(),
            model: test_model(),
            system_prompt: None,
            cwd: None,
            is_subscription: false,
            created_at: 2000,
            parent_id: Some("root".into()),
            child_budget: 0,
            tagline: None,
            archived: false,
            last_exit_status: None,
            last_phase: None,
            auto_archive: false,
            notify_parent: true,
            project_name: None,
        })
        .unwrap();

        assert_eq!(db.budget_used("root").unwrap(), 1); // 1 + 0
        assert_eq!(db.child_count("root").unwrap(), 1);

        // Create a child with its own budget (cost = 1 + 2 = 3)
        db.create_session(&StoredSession {
            id: "c2".into(),
            model: test_model(),
            system_prompt: None,
            cwd: None,
            is_subscription: false,
            created_at: 3000,
            parent_id: Some("root".into()),
            child_budget: 2,
            tagline: None,
            archived: false,
            last_exit_status: None,
            last_phase: None,
            auto_archive: false,
            notify_parent: true,
            project_name: None,
        })
        .unwrap();

        assert_eq!(db.budget_used("root").unwrap(), 4); // 1 + (1+2)
        assert_eq!(db.child_count("root").unwrap(), 2);

        // get_children returns both
        let children = db.get_children("root").unwrap();
        assert_eq!(children.len(), 2);
        assert_eq!(children[0].id, "c1");
        assert_eq!(children[1].id, "c2");
    }

    #[test]
    fn delete_session_tree() {
        let db = Db::open_memory().unwrap();

        db.create_session(&StoredSession {
            id: "root".into(),
            model: test_model(),
            system_prompt: None,
            cwd: None,
            is_subscription: false,
            created_at: 1000,
            parent_id: None,
            child_budget: 10,
            tagline: None,
            archived: false,
            last_exit_status: None,
            last_phase: None,
            auto_archive: false,
            notify_parent: true,
            project_name: None,
        })
        .unwrap();

        db.create_session(&StoredSession {
            id: "c1".into(),
            model: test_model(),
            system_prompt: None,
            cwd: None,
            is_subscription: false,
            created_at: 2000,
            parent_id: Some("root".into()),
            child_budget: 3,
            tagline: None,
            archived: false,
            last_exit_status: None,
            last_phase: None,
            auto_archive: false,
            notify_parent: true,
            project_name: None,
        })
        .unwrap();

        // Grandchild
        db.create_session(&StoredSession {
            id: "gc1".into(),
            model: test_model(),
            system_prompt: None,
            cwd: None,
            is_subscription: false,
            created_at: 3000,
            parent_id: Some("c1".into()),
            child_budget: 0,
            tagline: None,
            archived: false,
            last_exit_status: None,
            last_phase: None,
            auto_archive: false,
            notify_parent: true,
            project_name: None,
        })
        .unwrap();

        // Delete root -- should delete entire tree
        db.delete_session_tree("root").unwrap();
        assert!(db.get_session("root").unwrap().is_none());
        assert!(db.get_session("c1").unwrap().is_none());
        assert!(db.get_session("gc1").unwrap().is_none());
    }

    #[test]
    fn delete_subtree_preserves_parent() {
        let db = Db::open_memory().unwrap();

        db.create_session(&StoredSession {
            id: "root".into(),
            model: test_model(),
            system_prompt: None,
            cwd: None,
            is_subscription: false,
            created_at: 1000,
            parent_id: None,
            child_budget: 5,
            tagline: None,
            archived: false,
            last_exit_status: None,
            last_phase: None,
            auto_archive: false,
            notify_parent: true,
            project_name: None,
        })
        .unwrap();

        db.create_session(&StoredSession {
            id: "c1".into(),
            model: test_model(),
            system_prompt: None,
            cwd: None,
            is_subscription: false,
            created_at: 2000,
            parent_id: Some("root".into()),
            child_budget: 0,
            tagline: None,
            archived: false,
            last_exit_status: None,
            last_phase: None,
            auto_archive: false,
            notify_parent: true,
            project_name: None,
        })
        .unwrap();

        // Delete child only -- parent survives, budget_used drops
        assert_eq!(db.budget_used("root").unwrap(), 1);
        db.delete_session_tree("c1").unwrap();
        assert!(db.get_session("root").unwrap().is_some());
        assert!(db.get_session("c1").unwrap().is_none());
        assert_eq!(db.budget_used("root").unwrap(), 0);
    }

    #[test]
    fn sessions_needing_resume() {
        let db = Db::open_memory().unwrap();

        // Create a top-level session ending with User message (should NOT be resumed)
        db.create_session(&StoredSession {
            id: "top".into(),
            model: test_model(),
            system_prompt: None,
            cwd: None,
            is_subscription: false,
            created_at: 1000,
            parent_id: None,
            child_budget: 5,
            tagline: None,
            archived: false,
            last_exit_status: None,
            last_phase: None,
            auto_archive: false,
            notify_parent: true,
            project_name: None,
        })
        .unwrap();
        db.append_message("top", &Message::User(UserMessage::text("hello")))
            .unwrap();

        // Create a child session ending with User message (should be resumed)
        db.create_session(&StoredSession {
            id: "child1".into(),
            model: test_model(),
            system_prompt: None,
            cwd: None,
            is_subscription: false,
            created_at: 2000,
            parent_id: Some("top".into()),
            child_budget: 0,
            tagline: None,
            archived: false,
            last_exit_status: None,
            last_phase: None,
            auto_archive: false,
            notify_parent: true,
            project_name: None,
        })
        .unwrap();
        db.append_message("child1", &Message::User(UserMessage::text("work")))
            .unwrap();

        // Create a child session ending with Assistant message (should NOT be resumed)
        db.create_session(&StoredSession {
            id: "child2".into(),
            model: test_model(),
            system_prompt: None,
            cwd: None,
            is_subscription: false,
            created_at: 3000,
            parent_id: Some("top".into()),
            child_budget: 0,
            tagline: None,
            archived: false,
            last_exit_status: None,
            last_phase: None,
            auto_archive: false,
            notify_parent: true,
            project_name: None,
        })
        .unwrap();
        db.append_message(
            "child2",
            &Message::Assistant(AssistantMessage::empty("test", "test", "test-model")),
        )
        .unwrap();

        // Create a child session ending with ToolResult (should be resumed)
        db.create_session(&StoredSession {
            id: "child3".into(),
            model: test_model(),
            system_prompt: None,
            cwd: None,
            is_subscription: false,
            created_at: 4000,
            parent_id: Some("top".into()),
            child_budget: 0,
            tagline: None,
            archived: false,
            last_exit_status: None,
            last_phase: None,
            auto_archive: false,
            notify_parent: true,
            project_name: None,
        })
        .unwrap();
        db.append_message(
            "child3",
            &Message::ToolResult(ToolResultMessage {
                tool_call_id: "tc1".into(),
                tool_name: "bash".into(),
                content: vec![ToolResultContent::Text(TextContent {
                    text: "ok".into(),
                    text_signature: None,
                })],
                details: None,
                is_error: false,
                timestamp: 4001,
                duration_ms: None,
                summary: None,
                post_persist_actions: Vec::new(),
            }),
        )
        .unwrap();

        // Create a child session with no messages (should NOT be resumed)
        db.create_session(&StoredSession {
            id: "child4".into(),
            model: test_model(),
            system_prompt: None,
            cwd: None,
            is_subscription: false,
            created_at: 5000,
            parent_id: Some("top".into()),
            child_budget: 0,
            tagline: None,
            archived: false,
            last_exit_status: None,
            last_phase: None,
            auto_archive: false,
            notify_parent: true,
            project_name: None,
        })
        .unwrap();

        let mut ids = db.sessions_needing_resume().unwrap();
        ids.sort();
        // Phase 1 seamless-restart: top-level sessions are now in scope
        // too. child2 (assistant stop_reason=Stop) is still skipped, as
        // is child4 (empty).
        assert_eq!(ids, vec!["child1", "child3", "top"]);
    }

    #[test]
    fn sessions_needing_resume_skips_completed_and_archived() {
        let db = Db::open_memory().unwrap();

        // completed session — should be skipped even though last msg is user
        db.create_session(&StoredSession {
            id: "completed".into(),
            model: test_model(),
            system_prompt: None,
            cwd: None,
            is_subscription: false,
            created_at: crate::types::timestamp_ms() as i64,
            parent_id: None,
            child_budget: 0,
            tagline: None,
            archived: false,
            last_exit_status: Some("completed".into()),
            last_phase: None,
            auto_archive: false,
            notify_parent: true,
            project_name: None,
        })
        .unwrap();
        db.append_message("completed", &Message::User(UserMessage::text("hi")))
            .unwrap();

        // archived session — should be skipped
        db.create_session(&StoredSession {
            id: "archived".into(),
            model: test_model(),
            system_prompt: None,
            cwd: None,
            is_subscription: false,
            created_at: crate::types::timestamp_ms() as i64,
            parent_id: None,
            child_budget: 0,
            tagline: None,
            archived: true,
            last_exit_status: None,
            last_phase: None,
            auto_archive: false,
            notify_parent: true,
            project_name: None,
        })
        .unwrap();
        db.append_message("archived", &Message::User(UserMessage::text("hi")))
            .unwrap();

        // resumable session — sanity check
        db.create_session(&StoredSession {
            id: "live".into(),
            model: test_model(),
            system_prompt: None,
            cwd: None,
            is_subscription: false,
            created_at: crate::types::timestamp_ms() as i64,
            parent_id: None,
            child_budget: 0,
            tagline: None,
            archived: false,
            last_exit_status: None,
            last_phase: None,
            auto_archive: false,
            notify_parent: true,
            project_name: None,
        })
        .unwrap();
        db.append_message("live", &Message::User(UserMessage::text("hi")))
            .unwrap();

        let ids = db.sessions_needing_resume().unwrap();
        assert_eq!(ids, vec!["live"]);
    }

    #[test]
    fn sessions_needing_resume_skips_opt_out_and_old_sessions() {
        let db = Db::open_memory().unwrap();
        let now_ms = crate::types::timestamp_ms() as i64;

        // opt-out session — should be skipped even when recent
        db.create_session(&StoredSession {
            id: "opt_out".into(),
            model: test_model(),
            system_prompt: None,
            cwd: None,
            is_subscription: false,
            created_at: now_ms,
            parent_id: None,
            child_budget: 0,
            tagline: None,
            archived: false,
            last_exit_status: None,
            last_phase: None,
            auto_archive: false,
            notify_parent: true,
            project_name: None,
        })
        .unwrap();
        db.append_message("opt_out", &Message::User(UserMessage::text("hi")))
            .unwrap();
        db.set_resume_on_restart("opt_out", false).unwrap();

        // stale session — should be skipped by the 24h cutoff
        db.create_session(&StoredSession {
            id: "stale".into(),
            model: test_model(),
            system_prompt: None,
            cwd: None,
            is_subscription: false,
            created_at: now_ms - 48 * 3600 * 1000,
            parent_id: None,
            child_budget: 0,
            tagline: None,
            archived: false,
            last_exit_status: None,
            last_phase: None,
            auto_archive: false,
            notify_parent: true,
            project_name: None,
        })
        .unwrap();
        // The message is persisted with "now" as created_at so we also
        // insert it 2 days ago via the explicit cutoff query below.

        // Fresh session — should be resumed
        db.create_session(&StoredSession {
            id: "fresh".into(),
            model: test_model(),
            system_prompt: None,
            cwd: None,
            is_subscription: false,
            created_at: now_ms,
            parent_id: None,
            child_budget: 0,
            tagline: None,
            archived: false,
            last_exit_status: None,
            last_phase: None,
            auto_archive: false,
            notify_parent: true,
            project_name: None,
        })
        .unwrap();
        db.append_message("fresh", &Message::User(UserMessage::text("hi")))
            .unwrap();

        let ids = db.sessions_needing_resume().unwrap();
        // opt_out is filtered; stale has no messages AND created_at is
        // 48h ago so it's outside the 24h cutoff. Only `fresh` remains.
        assert_eq!(ids, vec!["fresh"]);
    }

    #[test]
    fn sessions_needing_resume_matches_assistant_tool_use_and_error() {
        let db = Db::open_memory().unwrap();
        let now_ms = crate::types::timestamp_ms() as i64;

        // Assistant with stop_reason=ToolUse: should be resumed (repair
        // will stub the missing tool_result, then the next LLM call fires).
        db.create_session(&StoredSession {
            id: "tooluse".into(),
            model: test_model(),
            system_prompt: None,
            cwd: None,
            is_subscription: false,
            created_at: now_ms,
            parent_id: None,
            child_budget: 0,
            tagline: None,
            archived: false,
            last_exit_status: None,
            last_phase: None,
            auto_archive: false,
            notify_parent: true,
            project_name: None,
        })
        .unwrap();
        let mut asst = AssistantMessage::empty("test", "test", "test-model");
        asst.stop_reason = StopReason::ToolUse;
        db.append_message("tooluse", &Message::Assistant(asst))
            .unwrap();

        // Assistant with stop_reason=Error: should be resumed (stream
        // died mid-flight).
        db.create_session(&StoredSession {
            id: "error".into(),
            model: test_model(),
            system_prompt: None,
            cwd: None,
            is_subscription: false,
            created_at: now_ms,
            parent_id: None,
            child_budget: 0,
            tagline: None,
            archived: false,
            last_exit_status: None,
            last_phase: None,
            auto_archive: false,
            notify_parent: true,
            project_name: None,
        })
        .unwrap();
        let mut asst = AssistantMessage::empty("test", "test", "test-model");
        asst.stop_reason = StopReason::Error;
        db.append_message("error", &Message::Assistant(asst))
            .unwrap();

        // Assistant with stop_reason=Stop: should NOT be resumed.
        db.create_session(&StoredSession {
            id: "ok".into(),
            model: test_model(),
            system_prompt: None,
            cwd: None,
            is_subscription: false,
            created_at: now_ms,
            parent_id: None,
            child_budget: 0,
            tagline: None,
            archived: false,
            last_exit_status: None,
            last_phase: None,
            auto_archive: false,
            notify_parent: true,
            project_name: None,
        })
        .unwrap();
        db.append_message(
            "ok",
            &Message::Assistant(AssistantMessage::empty("test", "test", "test-model")),
        )
        .unwrap();

        let mut ids = db.sessions_needing_resume().unwrap();
        ids.sort();
        assert_eq!(ids, vec!["error", "tooluse"]);
    }

    #[test]
    fn queue_message_basic() {
        let db = Db::open_memory().unwrap();
        db.create_session(&StoredSession {
            id: "s1".into(),
            model: test_model(),
            system_prompt: None,
            cwd: None,
            is_subscription: false,
            created_at: 1000,
            parent_id: None,
            child_budget: 0,
            tagline: None,
            archived: false,
            last_exit_status: None,
            last_phase: None,
            auto_archive: false,
            notify_parent: true,
            project_name: None,
        })
        .unwrap();

        let id = db
            .queue_message("s1", "hello from parent", "parent:s0")
            .unwrap();
        assert!(id > 0);
        assert!(db.has_queued_messages("s1").unwrap());
        assert!(!db.has_queued_messages("s_nonexistent").unwrap());

        let sessions = db.sessions_with_queued_messages().unwrap();
        assert_eq!(sessions, vec!["s1"]);
    }

    #[test]
    fn drain_queued_messages_basic() {
        let db = Db::open_memory().unwrap();
        db.create_session(&StoredSession {
            id: "s1".into(),
            model: test_model(),
            system_prompt: None,
            cwd: None,
            is_subscription: false,
            created_at: 1000,
            parent_id: None,
            child_budget: 0,
            tagline: None,
            archived: false,
            last_exit_status: None,
            last_phase: None,
            auto_archive: false,
            notify_parent: true,
            project_name: None,
        })
        .unwrap();

        db.queue_message("s1", "msg1", "sender1").unwrap();
        db.queue_message("s1", "msg2", "sender2").unwrap();

        let messages = db.drain_queued_messages("s1").unwrap();
        assert_eq!(messages.len(), 2);

        // Messages should be User messages with sender_info prefix
        if let Message::User(u) = &messages[0] {
            let text: String = u
                .content
                .iter()
                .filter_map(|c| match c {
                    UserContent::Text(t) => Some(t.text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("");
            assert!(text.contains("[from sender1]"));
            assert!(text.contains("msg1"));
        } else {
            panic!("expected User message");
        }

        // Queue should be empty now
        assert!(!db.has_queued_messages("s1").unwrap());
        assert!(db.sessions_with_queued_messages().unwrap().is_empty());

        // Messages should have been persisted to the messages table
        let persisted = db.get_messages("s1").unwrap();
        assert_eq!(persisted.len(), 2);
    }

    #[test]
    fn drain_empty_queue() {
        let db = Db::open_memory().unwrap();
        db.create_session(&StoredSession {
            id: "s1".into(),
            model: test_model(),
            system_prompt: None,
            cwd: None,
            is_subscription: false,
            created_at: 1000,
            parent_id: None,
            child_budget: 0,
            tagline: None,
            archived: false,
            last_exit_status: None,
            last_phase: None,
            auto_archive: false,
            notify_parent: true,
            project_name: None,
        })
        .unwrap();

        let messages = db.drain_queued_messages("s1").unwrap();
        assert!(messages.is_empty());
    }

    #[test]
    fn drain_does_not_affect_other_sessions() {
        let db = Db::open_memory().unwrap();
        for id in ["s1", "s2"] {
            db.create_session(&StoredSession {
                id: id.into(),
                model: test_model(),
                system_prompt: None,
                cwd: None,
                is_subscription: false,
                created_at: 1000,
                parent_id: None,
                child_budget: 0,
                tagline: None,
                archived: false,
                last_exit_status: None,
                last_phase: None,
                auto_archive: false,
                notify_parent: true,
                project_name: None,
            })
            .unwrap();
        }

        db.queue_message("s1", "for s1", "x").unwrap();
        db.queue_message("s2", "for s2", "y").unwrap();

        let drained = db.drain_queued_messages("s1").unwrap();
        assert_eq!(drained.len(), 1);

        // s2 still has its message
        assert!(db.has_queued_messages("s2").unwrap());
        let s2_msgs = db.drain_queued_messages("s2").unwrap();
        assert_eq!(s2_msgs.len(), 1);
    }

    #[test]
    fn queued_messages_cascade_on_session_delete() {
        let db = Db::open_memory().unwrap();
        db.create_session(&StoredSession {
            id: "s1".into(),
            model: test_model(),
            system_prompt: None,
            cwd: None,
            is_subscription: false,
            created_at: 1000,
            parent_id: None,
            child_budget: 0,
            tagline: None,
            archived: false,
            last_exit_status: None,
            last_phase: None,
            auto_archive: false,
            notify_parent: true,
            project_name: None,
        })
        .unwrap();

        db.queue_message("s1", "will be deleted", "x").unwrap();
        assert!(db.has_queued_messages("s1").unwrap());

        db.delete_session("s1").unwrap();
        // CASCADE should have removed queued messages too
        assert!(!db.has_queued_messages("s1").unwrap());
    }

    #[test]
    fn session_stats_empty() {
        let db = Db::open_memory().unwrap();
        db.create_session(&StoredSession {
            id: "s1".into(),
            model: test_model(),
            system_prompt: None,
            cwd: None,
            is_subscription: false,
            created_at: 1000,
            parent_id: None,
            child_budget: 0,
            tagline: None,
            archived: false,
            last_exit_status: None,
            last_phase: None,
            auto_archive: false,
            notify_parent: true,
            project_name: None,
        })
        .unwrap();

        // No messages → None
        assert!(db.session_stats("s1").unwrap().is_none());
    }

    #[test]
    fn session_stats_basic() {
        let db = Db::open_memory().unwrap();
        db.create_session(&StoredSession {
            id: "s1".into(),
            model: test_model(),
            system_prompt: None,
            cwd: None,
            is_subscription: false,
            created_at: 1000,
            parent_id: None,
            child_budget: 0,
            tagline: None,
            archived: false,
            last_exit_status: None,
            last_phase: None,
            auto_archive: false,
            notify_parent: true,
            project_name: None,
        })
        .unwrap();

        // Add a user message
        db.append_message("s1", &Message::User(UserMessage::text("hello")))
            .unwrap();

        // Add an assistant message with usage
        let mut asst = AssistantMessage::empty("test", "test", "test-model");
        asst.usage.input = 100;
        asst.usage.output = 50;
        asst.usage.cache_read = 10;
        asst.usage.cache_write = 5;
        asst.usage.cost.total = 0.42;
        asst.content.push(AssistantContent::Text(TextContent {
            text: "hi".into(),
            text_signature: None,
        }));
        asst.content.push(AssistantContent::ToolCall(ToolCall {
            id: "tc1".into(),
            name: "bash".into(),
            arguments: serde_json::json!({"cmd": "ls"}),
        }));
        db.append_message("s1", &Message::Assistant(asst)).unwrap();

        // Add a tool result
        db.append_message(
            "s1",
            &Message::ToolResult(ToolResultMessage {
                tool_call_id: "tc1".into(),
                tool_name: "bash".into(),
                content: vec![ToolResultContent::Text(TextContent {
                    text: "ok".into(),
                    text_signature: None,
                })],
                details: None,
                is_error: false,
                timestamp: 2000,
                duration_ms: None,
                summary: None,
                post_persist_actions: Vec::new(),
            }),
        )
        .unwrap();

        let stats = db.session_stats("s1").unwrap().unwrap();
        assert_eq!(stats.message_count, 3);
        assert_eq!(stats.user_messages, 1);
        assert_eq!(stats.assistant_messages, 1);
        assert_eq!(stats.tool_calls, 1);
        assert_eq!(stats.tool_results, 1);
        assert_eq!(stats.tokens_input, 100);
        assert_eq!(stats.tokens_output, 50);
        assert_eq!(stats.tokens_cache_read, 10);
        assert_eq!(stats.tokens_cache_write, 5);
        assert!((stats.cost - 0.42).abs() < 1e-6);
        assert!(stats.last_message_time.is_some());
        // last_input_tokens = input + cache_read + cache_write = 115
        assert_eq!(stats.last_input_tokens, Some(115));
    }

    #[test]
    fn session_stats_last_input_skips_errors() {
        let db = Db::open_memory().unwrap();
        db.create_session(&StoredSession {
            id: "s1".into(),
            model: test_model(),
            system_prompt: None,
            cwd: None,
            is_subscription: false,
            created_at: 1000,
            parent_id: None,
            child_budget: 0,
            tagline: None,
            archived: false,
            last_exit_status: None,
            last_phase: None,
            auto_archive: false,
            notify_parent: true,
            project_name: None,
        })
        .unwrap();

        // Good assistant message
        let mut good = AssistantMessage::empty("test", "test", "test-model");
        good.usage.input = 100;
        good.usage.cache_read = 20;
        db.append_message("s1", &Message::Assistant(good)).unwrap();

        // Error assistant message (should be skipped for last_input_tokens)
        let mut bad = AssistantMessage::empty("test", "test", "test-model");
        bad.usage.input = 999;
        bad.stop_reason = StopReason::Error;
        db.append_message("s1", &Message::Assistant(bad)).unwrap();

        let stats = db.session_stats("s1").unwrap().unwrap();
        // Should use the good message's tokens, not the error one
        assert_eq!(stats.last_input_tokens, Some(120)); // 100 + 20 + 0
    }

    /// Build a minimal session row for the project-stats tests below.
    /// Defaults mirror the other tests in this module; callers override
    /// the id / project_name / archived as needed.
    fn make_stored(id: &str, project: Option<&str>, archived: bool) -> StoredSession {
        StoredSession {
            id: id.into(),
            model: test_model(),
            system_prompt: None,
            cwd: None,
            is_subscription: false,
            created_at: 1000,
            parent_id: None,
            child_budget: 0,
            tagline: None,
            archived,
            last_exit_status: None,
            last_phase: None,
            auto_archive: false,
            notify_parent: true,
            project_name: project.map(|p| p.to_string()),
        }
    }

    #[test]
    fn project_stats_aggregates_sessions_and_includes_archived() {
        let db = Db::open_memory().unwrap();

        // Three sessions:
        //   s1, s2 — project "alpha" (s2 archived).
        //   s3    — project "beta" (should be ignored for alpha query).
        db.create_session(&make_stored("s1", Some("alpha"), false))
            .unwrap();
        db.create_session(&make_stored("s2", Some("alpha"), true))
            .unwrap();
        db.create_session(&make_stored("s3", Some("beta"), false))
            .unwrap();

        // s1: user + one assistant turn.
        db.append_message("s1", &Message::User(UserMessage::text("hi")))
            .unwrap();
        let mut a1 = AssistantMessage::empty("test", "test", "test-model");
        a1.usage.input = 100;
        a1.usage.output = 50;
        a1.usage.cache_read = 10;
        a1.usage.cache_write = 5;
        a1.usage.cost.total = 0.10;
        db.append_message("s1", &Message::Assistant(a1)).unwrap();

        // s2 (archived): one assistant turn — should be *included*.
        let mut a2 = AssistantMessage::empty("test", "test", "test-model");
        a2.usage.input = 200;
        a2.usage.output = 25;
        a2.usage.cache_read = 0;
        a2.usage.cache_write = 0;
        a2.usage.cost.total = 0.25;
        db.append_message("s2", &Message::Assistant(a2)).unwrap();

        // s3 (other project): noise — must not leak into alpha totals.
        let mut a3 = AssistantMessage::empty("test", "test", "test-model");
        a3.usage.input = 9_999;
        a3.usage.cost.total = 9.99;
        db.append_message("s3", &Message::Assistant(a3)).unwrap();

        let alpha = db.project_stats("alpha").unwrap();
        assert_eq!(alpha.session_count, 2);
        // 1 user + 1 assistant (s1) + 1 assistant (s2) = 3 messages
        assert_eq!(alpha.message_count, 3);
        assert_eq!(alpha.tokens_input, 300);
        assert_eq!(alpha.tokens_output, 75);
        assert_eq!(alpha.tokens_cache_read, 10);
        assert_eq!(alpha.tokens_cache_write, 5);
        assert!((alpha.cost - 0.35).abs() < 1e-6);
        assert!(alpha.last_message_time.is_some());

        // Per-session sums must match the aggregate.
        let s1 = db.session_stats("s1").unwrap().unwrap();
        let s2 = db.session_stats("s2").unwrap().unwrap();
        assert_eq!(alpha.tokens_input, s1.tokens_input + s2.tokens_input);
        assert_eq!(alpha.tokens_output, s1.tokens_output + s2.tokens_output);
        assert_eq!(
            alpha.tokens_cache_read,
            s1.tokens_cache_read + s2.tokens_cache_read
        );
        assert_eq!(
            alpha.tokens_cache_write,
            s1.tokens_cache_write + s2.tokens_cache_write
        );
        assert!((alpha.cost - (s1.cost + s2.cost)).abs() < 1e-6);
        assert_eq!(alpha.message_count, s1.message_count + s2.message_count);
    }

    #[test]
    fn project_stats_unknown_project_returns_zero() {
        let db = Db::open_memory().unwrap();
        // No sessions at all — must not error.
        let stats = db.project_stats("nope").unwrap();
        assert_eq!(stats.session_count, 0);
        assert_eq!(stats.message_count, 0);
        assert_eq!(stats.tokens_input, 0);
        assert_eq!(stats.tokens_output, 0);
        assert_eq!(stats.tokens_cache_read, 0);
        assert_eq!(stats.tokens_cache_write, 0);
        assert_eq!(stats.cost, 0.0);
        assert!(stats.last_message_time.is_none());
    }

    #[test]
    fn project_stats_session_without_messages() {
        // A freshly created session with no messages should still contribute
        // to session_count but leave all totals at zero.
        let db = Db::open_memory().unwrap();
        db.create_session(&make_stored("s1", Some("alpha"), false))
            .unwrap();
        let stats = db.project_stats("alpha").unwrap();
        assert_eq!(stats.session_count, 1);
        assert_eq!(stats.message_count, 0);
        assert_eq!(stats.tokens_input, 0);
        assert_eq!(stats.cost, 0.0);
        assert!(stats.last_message_time.is_none());
    }

    #[test]
    fn test_create_and_get_project() {
        let db = Db::open_memory().unwrap();
        let project = db.create_project("myproj", "/tmp/myproj").unwrap();
        assert_eq!(project.name, "myproj");
        assert_eq!(project.path, "/tmp/myproj");
        assert!(project.created_at > 0);
        assert_eq!(project.last_seen, project.created_at);

        let loaded = db.get_project("myproj").unwrap().unwrap();
        assert_eq!(loaded.name, "myproj");
        assert_eq!(loaded.path, "/tmp/myproj");
        assert_eq!(loaded.created_at, project.created_at);

        // Non-existent returns None
        assert!(db.get_project("nope").unwrap().is_none());
    }

    #[test]
    fn test_create_duplicate_name_fails() {
        let db = Db::open_memory().unwrap();
        db.create_project("dup", "/tmp/a").unwrap();
        assert!(db.create_project("dup", "/tmp/b").is_err());
    }

    #[test]
    fn test_create_duplicate_path_fails() {
        let db = Db::open_memory().unwrap();
        db.create_project("proj1", "/tmp/same").unwrap();
        assert!(db.create_project("proj2", "/tmp/same").is_err());
    }

    #[test]
    fn test_get_project_by_path() {
        let db = Db::open_memory().unwrap();
        db.create_project("alpha", "/tmp/alpha").unwrap();
        db.create_project("beta", "/tmp/beta").unwrap();

        let found = db.get_project_by_path("/tmp/beta").unwrap().unwrap();
        assert_eq!(found.name, "beta");

        assert!(db.get_project_by_path("/tmp/nope").unwrap().is_none());
    }

    #[test]
    fn test_list_projects() {
        let db = Db::open_memory().unwrap();
        let _p1 = db.create_project("first", "/tmp/first").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        let _p2 = db.create_project("second", "/tmp/second").unwrap();

        let list = db.list_projects().unwrap();
        assert_eq!(list.len(), 2);
        // ORDER BY last_seen DESC — second (created later) is first
        assert_eq!(list[0].name, "second");
        assert_eq!(list[1].name, "first");

        // After updating first's last_seen, it should come first
        std::thread::sleep(std::time::Duration::from_millis(10));
        db.update_project_last_seen("first").unwrap();
        let list = db.list_projects().unwrap();
        assert_eq!(list[0].name, "first");
        assert_eq!(list[1].name, "second");
    }

    #[test]
    fn test_rename_project() {
        let db = Db::open_memory().unwrap();
        db.create_project("old_name", "/tmp/proj").unwrap();
        db.rename_project("old_name", "new_name").unwrap();

        assert!(db.get_project("old_name").unwrap().is_none());
        let renamed = db.get_project("new_name").unwrap().unwrap();
        assert_eq!(renamed.path, "/tmp/proj");
    }

    #[test]
    fn test_update_last_seen() {
        let db = Db::open_memory().unwrap();
        let project = db.create_project("proj", "/tmp/proj").unwrap();
        let original_last_seen = project.last_seen;

        std::thread::sleep(std::time::Duration::from_millis(10));
        db.update_project_last_seen("proj").unwrap();

        let updated = db.get_project("proj").unwrap().unwrap();
        assert!(updated.last_seen > original_last_seen);
        assert_eq!(updated.created_at, project.created_at);
    }

    #[test]
    fn test_delete_project() {
        let db = Db::open_memory().unwrap();
        db.create_project("doomed", "/tmp/doomed").unwrap();
        assert!(db.get_project("doomed").unwrap().is_some());

        db.delete_project("doomed").unwrap();
        assert!(db.get_project("doomed").unwrap().is_none());

        // Deleting non-existent is a no-op (not an error)
        db.delete_project("doomed").unwrap();
    }

    #[test]
    fn gc_archived_sessions() {
        let db = Db::open_memory().unwrap();

        // Create an old archived session (created 10 days ago)
        let ten_days_ago = (crate::types::timestamp_ms() as i64) - 10 * 86_400_000;
        db.create_session(&StoredSession {
            id: "old_archived".into(),
            model: test_model(),
            system_prompt: None,
            cwd: None,
            is_subscription: false,
            created_at: ten_days_ago,
            parent_id: None,
            child_budget: 0,
            tagline: None,
            archived: true,
            last_exit_status: None,
            last_phase: None,
            auto_archive: false,
            notify_parent: true,
            project_name: None,
        })
        .unwrap();
        db.append_message("old_archived", &Message::User(UserMessage::text("hello")))
            .unwrap();

        // Create a recent archived session (created 1 day ago)
        let one_day_ago = (crate::types::timestamp_ms() as i64) - 86_400_000;
        db.create_session(&StoredSession {
            id: "new_archived".into(),
            model: test_model(),
            system_prompt: None,
            cwd: None,
            is_subscription: false,
            created_at: one_day_ago,
            parent_id: None,
            child_budget: 0,
            tagline: None,
            archived: true,
            last_exit_status: None,
            last_phase: None,
            auto_archive: false,
            notify_parent: true,
            project_name: None,
        })
        .unwrap();

        // Create an old non-archived session (should not be deleted)
        db.create_session(&StoredSession {
            id: "old_active".into(),
            model: test_model(),
            system_prompt: None,
            cwd: None,
            is_subscription: false,
            created_at: ten_days_ago,
            parent_id: None,
            child_budget: 0,
            tagline: None,
            archived: false,
            last_exit_status: None,
            last_phase: None,
            auto_archive: false,
            notify_parent: true,
            project_name: None,
        })
        .unwrap();

        // GC with threshold of 7 days
        let threshold_ms = (crate::types::timestamp_ms() as i64) - 7 * 86_400_000;
        let deleted = db.gc_archived_sessions(threshold_ms as u64).unwrap();
        assert_eq!(deleted, 1, "should delete only the old archived session");

        // Verify correct sessions remain
        assert!(db.get_session("old_archived").unwrap().is_none());
        assert!(db.get_session("new_archived").unwrap().is_some());
        assert!(db.get_session("old_active").unwrap().is_some());

        // Verify cascade deleted messages
        assert!(db.get_messages("old_archived").unwrap().is_empty());
    }

    // ----- gc_empty_sessions -----

    fn empty_session(id: &str, parent: Option<&str>, archived: bool, age_ms: i64) -> StoredSession {
        let now = crate::types::timestamp_ms() as i64;
        StoredSession {
            id: id.into(),
            model: test_model(),
            system_prompt: None,
            cwd: None,
            is_subscription: false,
            created_at: now - age_ms,
            parent_id: parent.map(|s| s.to_string()),
            child_budget: 0,
            tagline: None,
            archived,
            last_exit_status: None,
            last_phase: None,
            auto_archive: false,
            notify_parent: true,
            project_name: None,
        }
    }

    #[test]
    fn gc_empty_past_grace_deleted() {
        let db = Db::open_memory().unwrap();
        // 1 hour old, no messages, no children
        db.create_session(&empty_session("s1", None, false, 3_600_000))
            .unwrap();
        let live = HashSet::new();
        let owned = HashSet::new();
        let deleted = db.gc_empty_sessions(60, &live, &owned).unwrap();
        assert_eq!(deleted, vec!["s1".to_string()]);
        assert!(db.get_session("s1").unwrap().is_none());
    }

    #[test]
    fn gc_empty_within_grace_kept() {
        let db = Db::open_memory().unwrap();
        // Just created (age 0).
        db.create_session(&empty_session("s1", None, false, 0))
            .unwrap();
        let live = HashSet::new();
        let owned = HashSet::new();
        let deleted = db.gc_empty_sessions(60, &live, &owned).unwrap();
        assert!(
            deleted.is_empty(),
            "within-grace session must not be deleted"
        );
        assert!(db.get_session("s1").unwrap().is_some());
    }

    #[test]
    fn gc_empty_archived_kept() {
        let db = Db::open_memory().unwrap();
        db.create_session(&empty_session("s1", None, true, 3_600_000))
            .unwrap();
        let live = HashSet::new();
        let owned = HashSet::new();
        let deleted = db.gc_empty_sessions(60, &live, &owned).unwrap();
        assert!(
            deleted.is_empty(),
            "archived sessions must not be auto-GC'd by gc_empty_sessions"
        );
        assert!(db.get_session("s1").unwrap().is_some());
    }

    #[test]
    fn gc_empty_leaf_first_then_parent_next_pass() {
        let db = Db::open_memory().unwrap();
        // A is parent, B is child. Both empty, both old.
        db.create_session(&empty_session("A", None, false, 3_600_000))
            .unwrap();
        db.create_session(&empty_session("B", Some("A"), false, 3_600_000))
            .unwrap();
        let live = HashSet::new();
        let owned = HashSet::new();
        // First pass: only the leaf B is eligible.
        let deleted = db.gc_empty_sessions(60, &live, &owned).unwrap();
        assert_eq!(deleted, vec!["B".to_string()]);
        assert!(db.get_session("A").unwrap().is_some());
        assert!(db.get_session("B").unwrap().is_none());
        // Second pass: A is now a leaf and gets cleaned up.
        let deleted = db.gc_empty_sessions(60, &live, &owned).unwrap();
        assert_eq!(deleted, vec!["A".to_string()]);
        assert!(db.get_session("A").unwrap().is_none());
    }

    #[test]
    fn gc_empty_with_archived_child_kept() {
        // The child check intentionally ignores archived: even an
        // archived child blocks deletion of its (live) parent.
        let db = Db::open_memory().unwrap();
        db.create_session(&empty_session("A", None, false, 3_600_000))
            .unwrap();
        db.create_session(&empty_session("B", Some("A"), true, 3_600_000))
            .unwrap();
        let live = HashSet::new();
        let owned = HashSet::new();
        let deleted = db.gc_empty_sessions(60, &live, &owned).unwrap();
        // B is archived (gc_empty skips it) and A has a child -> nothing deleted.
        assert!(deleted.is_empty());
        assert!(db.get_session("A").unwrap().is_some());
        assert!(db.get_session("B").unwrap().is_some());
    }

    #[test]
    fn gc_empty_non_empty_kept() {
        let db = Db::open_memory().unwrap();
        db.create_session(&empty_session("s1", None, false, 3_600_000))
            .unwrap();
        db.append_message("s1", &Message::User(UserMessage::text("hi")))
            .unwrap();
        let live = HashSet::new();
        let owned = HashSet::new();
        let deleted = db.gc_empty_sessions(60, &live, &owned).unwrap();
        assert!(deleted.is_empty());
        assert!(db.get_session("s1").unwrap().is_some());
    }

    #[test]
    fn gc_empty_live_set_kept() {
        let db = Db::open_memory().unwrap();
        db.create_session(&empty_session("s1", None, false, 3_600_000))
            .unwrap();
        let mut live = HashSet::new();
        live.insert("s1".to_string());
        let owned = HashSet::new();
        let deleted = db.gc_empty_sessions(60, &live, &owned).unwrap();
        assert!(deleted.is_empty());
        assert!(db.get_session("s1").unwrap().is_some());
    }

    #[test]
    fn gc_empty_task_owned_kept() {
        let db = Db::open_memory().unwrap();
        db.create_session(&empty_session("s1", None, false, 3_600_000))
            .unwrap();
        let live = HashSet::new();
        let mut owned = HashSet::new();
        owned.insert("s1".to_string());
        let deleted = db.gc_empty_sessions(60, &live, &owned).unwrap();
        assert!(deleted.is_empty());
        assert!(db.get_session("s1").unwrap().is_some());
    }

    #[test]
    fn gc_empty_zero_grace_kept_only_if_blocked() {
        // grace_secs=0 is allowed (used by tests). All eligible sessions
        // (those with created_at strictly less than now) are deletable;
        // live/owned sets still protect them.
        let db = Db::open_memory().unwrap();
        // Use age=1ms so created_at < now even at zero grace.
        db.create_session(&empty_session("s1", None, false, 1))
            .unwrap();
        let live = HashSet::new();
        let owned = HashSet::new();
        let deleted = db.gc_empty_sessions(0, &live, &owned).unwrap();
        assert_eq!(deleted, vec!["s1".to_string()]);
    }
}
