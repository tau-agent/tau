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
    pub is_subscription: bool,
    pub created_at: u64,
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
                is_subscription INTEGER NOT NULL DEFAULT 0,
                created_at     INTEGER NOT NULL
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
                is_subscription INTEGER NOT NULL DEFAULT 0,
                created_at     INTEGER NOT NULL
            );
            CREATE TABLE messages (
                id          INTEGER PRIMARY KEY,
                session_id  TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                message_json TEXT NOT NULL,
                created_at  INTEGER NOT NULL
            );
            CREATE INDEX idx_messages_session ON messages(session_id);",
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
                "INSERT INTO sessions (id, model_json, system_prompt, is_subscription, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    session.id,
                    model_json,
                    session.system_prompt,
                    session.is_subscription as i32,
                    session.created_at,
                ],
            )
            .map_err(|e| crate::Error::Io(format!("insert session: {}", e)))?;
        Ok(())
    }

    /// Load a session's metadata (without messages).
    pub fn get_session(&self, id: &str) -> crate::Result<Option<StoredSession>> {
        self.conn
            .query_row(
                "SELECT id, model_json, system_prompt, is_subscription, created_at
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
                        is_subscription: row.get::<_, i32>(3)? != 0,
                        created_at: row.get(4)?,
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
                "SELECT id, model_json, system_prompt, is_subscription, created_at
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
                    is_subscription: row.get::<_, i32>(3)? != 0,
                    created_at: row.get(4)?,
                })
            })
            .map_err(|e| crate::Error::Io(format!("list sessions: {}", e)))?;

        let mut sessions = Vec::new();
        for row in rows {
            sessions.push(row.map_err(|e| crate::Error::Io(format!("read session row: {}", e)))?);
        }
        Ok(sessions)
    }

    /// Delete a session and all its messages (CASCADE).
    pub fn delete_session(&self, id: &str) -> crate::Result<()> {
        self.conn
            .execute("DELETE FROM sessions WHERE id = ?1", params![id])
            .map_err(|e| crate::Error::Io(format!("delete session: {}", e)))?;
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
        let now = crate::types::timestamp_ms();
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
            reasoning: false,
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
            is_subscription: true,
            created_at: 1000,
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
            is_subscription: false,
            created_at: 1000,
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
            is_subscription: false,
            created_at: 1000,
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
        for (id, ts) in [("s2", 2000u64), ("s1", 1000), ("s3", 3000)] {
            db.create_session(&StoredSession {
                id: id.into(),
                model: test_model(),
                system_prompt: None,
                is_subscription: false,
                created_at: ts,
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
            is_subscription: false,
            created_at: 1000,
        })
        .unwrap();
        assert_eq!(db.next_session_id().unwrap(), "s6");
    }
}
