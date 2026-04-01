//! SQLite-backed session persistence.
//!
//! Schema:
//! - `sessions`: one row per session (model, system prompt, metadata)
//! - `messages`: ordered messages per session, stored as JSON blobs

use std::path::PathBuf;

use rusqlite::{Connection, OptionalExtension, params};

use crate::types::{Message, Model};

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
        let conn = Connection::open(path)
            .map_err(|e| crate::Error::Io(format!("open db {}: {}", path.display(), e)))?;

        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")
            .map_err(|e| crate::Error::Io(format!("pragma: {}", e)))?;

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS sessions (
                id             TEXT PRIMARY KEY,
                model_json     TEXT NOT NULL,
                system_prompt  TEXT,
                cwd            TEXT,
                is_subscription INTEGER NOT NULL DEFAULT 0,
                created_at     INTEGER NOT NULL,
                parent_id      TEXT,
                child_budget   INTEGER NOT NULL DEFAULT 16
            );
            CREATE TABLE IF NOT EXISTS messages (
                id          INTEGER PRIMARY KEY,
                session_id  TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                message_json TEXT NOT NULL,
                created_at  INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_messages_session ON messages(session_id);",
        )
        .map_err(|e| crate::Error::Io(format!("create tables: {}", e)))?;

        // Migrations for existing DBs (ALTERs are no-ops if column already exists)
        let _ = conn.execute_batch("ALTER TABLE sessions ADD COLUMN cwd TEXT;");
        let _ = conn.execute_batch("ALTER TABLE sessions ADD COLUMN parent_id TEXT;");
        let _ = conn.execute_batch(
            "ALTER TABLE sessions ADD COLUMN child_budget INTEGER NOT NULL DEFAULT 16;",
        );

        // Create index after migrations ensure the column exists
        let _ = conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_sessions_parent ON sessions(parent_id);",
        );

        Ok(Self { conn })
    }

    /// Open an in-memory database (for tests).
    #[cfg(test)]
    pub fn open_memory() -> crate::Result<Self> {
        let path = PathBuf::from(":memory:");
        let conn = Connection::open_in_memory()
            .map_err(|e| crate::Error::Io(format!("open in-memory db: {}", e)))?;

        conn.execute_batch("PRAGMA foreign_keys=ON;")
            .map_err(|e| crate::Error::Io(format!("pragma: {}", e)))?;

        conn.execute_batch(
            "CREATE TABLE sessions (
                id             TEXT PRIMARY KEY,
                model_json     TEXT NOT NULL,
                system_prompt  TEXT,
                cwd            TEXT,
                is_subscription INTEGER NOT NULL DEFAULT 0,
                created_at     INTEGER NOT NULL,
                parent_id      TEXT,
                child_budget   INTEGER NOT NULL DEFAULT 16
            );
            CREATE TABLE messages (
                id          INTEGER PRIMARY KEY,
                session_id  TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                message_json TEXT NOT NULL,
                created_at  INTEGER NOT NULL
            );
            CREATE INDEX idx_messages_session ON messages(session_id);
            CREATE INDEX idx_sessions_parent ON sessions(parent_id);",
        )
        .map_err(|e| crate::Error::Io(format!("create tables: {}", e)))?;

        let _ = path; // suppress unused
        Ok(Self { conn })
    }

    // ----- sessions -----

    /// Create a session. Does not insert any messages.
    pub fn create_session(&self, session: &StoredSession) -> crate::Result<()> {
        let model_json = serde_json::to_string(&session.model)
            .map_err(|e| crate::Error::Parse(e.to_string()))?;
        self.conn
            .execute(
                "INSERT INTO sessions (id, model_json, system_prompt, cwd, is_subscription, created_at, parent_id, child_budget)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    session.id,
                    model_json,
                    session.system_prompt,
                    session.cwd,
                    session.is_subscription as i32,
                    session.created_at,
                    session.parent_id,
                    session.child_budget,
                ],
            )
            .map_err(|e| crate::Error::Io(format!("insert session: {}", e)))?;
        Ok(())
    }

    /// Load a session's metadata (without messages).
    pub fn get_session(&self, id: &str) -> crate::Result<Option<StoredSession>> {
        self.conn
            .query_row(
                "SELECT id, model_json, system_prompt, cwd, is_subscription, created_at, parent_id, child_budget
                 FROM sessions WHERE id = ?1",
                params![id],
                |row| {
                    let model_json: String = row.get(1)?;
                    let model: Model = serde_json::from_str(&model_json).map_err(|e| {
                        rusqlite::Error::FromSqlConversionFailure(
                            1,
                            rusqlite::types::Type::Text,
                            Box::new(e),
                        )
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
                    })
                },
            )
            .optional()
            .map_err(|e| crate::Error::Io(format!("get session: {}", e)))
    }

    /// List all sessions (metadata only, no messages).
    pub fn list_sessions(&self) -> crate::Result<Vec<StoredSession>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, model_json, system_prompt, cwd, is_subscription, created_at, parent_id, child_budget
                 FROM sessions ORDER BY created_at",
            )
            .map_err(|e| crate::Error::Io(format!("prepare list: {}", e)))?;

        let rows = stmt
            .query_map([], |row| {
                let model_json: String = row.get(1)?;
                let model: Model = serde_json::from_str(&model_json).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        1,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
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
                })
            })
            .map_err(|e| crate::Error::Io(format!("list sessions: {}", e)))?;

        let mut sessions = Vec::new();
        for row in rows {
            sessions.push(row.map_err(|e| crate::Error::Io(format!("read session row: {}", e)))?);
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
            .map_err(|e| crate::Error::Io(format!("last_message_time: {}", e)))?;
        Ok(result)
    }

    /// Delete a session and all its messages (CASCADE).
    pub fn delete_session(&self, id: &str) -> crate::Result<()> {
        self.conn
            .execute("DELETE FROM sessions WHERE id = ?1", params![id])
            .map_err(|e| crate::Error::Io(format!("delete session: {}", e)))?;
        Ok(())
    }

    /// Delete a session and all its descendants (recursive tree delete).
    pub fn delete_session_tree(&self, id: &str) -> crate::Result<()> {
        // Recursively delete children first
        let children = self.get_children(id)?;
        for child in &children {
            self.delete_session_tree(&child.id)?;
        }

        // Delete the session itself (CASCADE deletes its messages)
        self.delete_session(id)?;

        Ok(())
    }

    /// Get direct children of a session.
    pub fn get_children(&self, parent_id: &str) -> crate::Result<Vec<StoredSession>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, model_json, system_prompt, cwd, is_subscription, created_at, parent_id, child_budget
                 FROM sessions WHERE parent_id = ?1 ORDER BY created_at",
            )
            .map_err(|e| crate::Error::Io(format!("prepare children: {}", e)))?;

        let rows = stmt
            .query_map(params![parent_id], |row| {
                let model_json: String = row.get(1)?;
                let model: Model = serde_json::from_str(&model_json).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        1,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
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
                })
            })
            .map_err(|e| crate::Error::Io(format!("list children: {}", e)))?;

        let mut sessions = Vec::new();
        for row in rows {
            sessions.push(row.map_err(|e| crate::Error::Io(format!("read child row: {}", e)))?);
        }
        Ok(sessions)
    }

    /// Return IDs of child sessions whose last message is User or ToolResult,
    /// indicating they were interrupted mid-work and should be auto-resumed.
    pub fn sessions_needing_resume(&self) -> crate::Result<Vec<String>> {
        // Only consider child sessions (parent_id IS NOT NULL).
        // For each, get the last message and check its role.
        let mut stmt = self
            .conn
            .prepare(
                "SELECT s.id, m.message_json
                 FROM sessions s
                 JOIN messages m ON m.session_id = s.id
                 WHERE s.parent_id IS NOT NULL
                   AND m.id = (SELECT MAX(m2.id) FROM messages m2 WHERE m2.session_id = s.id)",
            )
            .map_err(|e| crate::Error::Io(format!("prepare sessions_needing_resume: {}", e)))?;

        let rows = stmt
            .query_map([], |row| {
                let id: String = row.get(0)?;
                let json: String = row.get(1)?;
                Ok((id, json))
            })
            .map_err(|e| crate::Error::Io(format!("query sessions_needing_resume: {}", e)))?;

        let mut result = Vec::new();
        for row in rows {
            let (id, json) =
                row.map_err(|e| crate::Error::Io(format!("read resume row: {}", e)))?;
            if let Ok(msg) = serde_json::from_str::<Message>(&json)
                && matches!(msg, Message::User(_) | Message::ToolResult(_))
            {
                result.push(id);
            }
        }
        Ok(result)
    }

    /// Count direct children of a session.
    pub fn child_count(&self, session_id: &str) -> crate::Result<usize> {
        let count: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sessions WHERE parent_id = ?1",
                params![session_id],
                |row| row.get(0),
            )
            .map_err(|e| crate::Error::Io(format!("child_count: {}", e)))?;
        Ok(count as usize)
    }

    /// Compute budget_used for a session (sum of 1 + child_budget for each direct child).
    pub fn budget_used(&self, session_id: &str) -> crate::Result<u32> {
        let used: i64 = self
            .conn
            .query_row(
                "SELECT COALESCE(SUM(1 + child_budget), 0) FROM sessions WHERE parent_id = ?1",
                params![session_id],
                |row| row.get(0),
            )
            .map_err(|e| crate::Error::Io(format!("budget_used: {}", e)))?;
        Ok(used as u32)
    }

    /// Update the working directory for a session.
    pub fn update_cwd(&self, session_id: &str, cwd: &str) -> crate::Result<()> {
        self.conn
            .execute(
                "UPDATE sessions SET cwd = ?1 WHERE id = ?2",
                params![cwd, session_id],
            )
            .map_err(|e| crate::Error::Io(format!("update cwd: {}", e)))?;
        Ok(())
    }

    /// Update the model for a session.
    pub fn update_model(&self, session_id: &str, model: &crate::types::Model) -> crate::Result<()> {
        let model_json =
            serde_json::to_string(model).map_err(|e| crate::Error::Parse(e.to_string()))?;
        self.conn
            .execute(
                "UPDATE sessions SET model_json = ?1 WHERE id = ?2",
                params![model_json, session_id],
            )
            .map_err(|e| crate::Error::Io(format!("update model: {}", e)))?;
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
            .map_err(|e| crate::Error::Io(e.to_string()))?;

        // Delete old messages (id < keep_from_id)
        self.conn
            .execute(
                "DELETE FROM messages WHERE session_id = ?1 AND id < ?2",
                params![session_id, keep_from_id],
            )
            .map_err(|e| {
                self.conn.execute_batch("ROLLBACK").ok();
                crate::Error::Io(format!("delete old messages: {}", e))
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
                crate::Error::Io(format!("insert summary: {}", e))
            })?;

        self.conn
            .execute_batch("COMMIT")
            .map_err(|e| crate::Error::Io(e.to_string()))?;

        Ok(())
    }

    /// Get the database row ID for a message at a given index in a session.
    pub fn get_message_row_id(&self, session_id: &str, index: usize) -> crate::Result<Option<i64>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id FROM messages WHERE session_id = ?1 ORDER BY id LIMIT 1 OFFSET ?2")
            .map_err(|e| crate::Error::Io(e.to_string()))?;
        stmt.query_row(params![session_id, index as i64], |row| row.get(0))
            .optional()
            .map_err(|e| crate::Error::Io(e.to_string()))
    }

    /// Get the next session id (max numeric suffix + 1).
    pub fn next_session_id(&self) -> crate::Result<String> {
        let max: Option<String> = self
            .conn
            .query_row(
                "SELECT id FROM sessions ORDER BY created_at DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| crate::Error::Io(format!("next id: {}", e)))?;

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
            .map_err(|e| crate::Error::Io(format!("insert message: {}", e)))?;
        Ok(())
    }

    /// Load all messages for a session, ordered by insertion.
    pub fn get_messages(&self, session_id: &str) -> crate::Result<Vec<Message>> {
        let mut stmt = self
            .conn
            .prepare("SELECT message_json FROM messages WHERE session_id = ?1 ORDER BY id")
            .map_err(|e| crate::Error::Io(format!("prepare messages: {}", e)))?;

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
            .map_err(|e| crate::Error::Io(format!("query messages: {}", e)))?;

        let mut messages = Vec::new();
        for row in rows {
            messages.push(row.map_err(|e| crate::Error::Io(format!("read message row: {}", e)))?);
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
            .map_err(|e| crate::Error::Io(format!("count messages: {}", e)))?;
        Ok(count as usize)
    }
}

fn default_db_path() -> PathBuf {
    if let Ok(data) = std::env::var("XDG_DATA_HOME") {
        PathBuf::from(data).join("tau").join("tau.db")
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home)
            .join(".local")
            .join("share")
            .join("tau")
            .join("tau.db")
    } else {
        PathBuf::from("/tmp").join("tau.db")
    }
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
            })
            .unwrap();
        }
        let sessions = db.list_sessions().unwrap();
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
        })
        .unwrap();

        let mut ids = db.sessions_needing_resume().unwrap();
        ids.sort();
        assert_eq!(ids, vec!["child1", "child3"]);
    }
}
