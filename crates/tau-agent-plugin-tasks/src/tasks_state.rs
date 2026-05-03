//! Typed state enum for the tasks state machine.
//!
//! Replaces the former stringly-typed `Task.state: String` with a proper
//! enum so the compiler enforces exhaustiveness on every match ladder,
//! transition table, and classifier.  See task #611 (from catalog #573)
//! for the rationale.
//!
//! Wire format: serde uses `#[serde(rename_all = "snake_case")]` so the
//! JSON representation is bit-identical to the legacy string form
//! (`"interactive"`, `"planning"`, ..., `"closed"`).  Same for SQLite:
//! [`TaskState`] implements `ToSql`/`FromSql` that round-trip through
//! the same lower-case snake strings, so the on-disk column layout
//! (still `TEXT`) is unchanged.

use std::fmt;
use std::str::FromStr;

use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};

/// The lifecycle state of a task.
///
/// See [`validate_transition`] for the allowed transitions between
/// variants.  The ordering of the variants below is roughly the forward
/// (happy-path) order but is not relied upon anywhere — matches are
/// exhaustive by name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Interactive,
    Planning,
    Refining,
    Ready,
    Active,
    Review,
    Approved,
    Merging,
    Failed,
    Merged,
    Done,
    Closed,
}

/// Error returned when parsing an unknown state string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidTaskState(pub String);

impl fmt::Display for InvalidTaskState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid task state: {}", self.0)
    }
}

impl std::error::Error for InvalidTaskState {}

impl TaskState {
    /// Every variant of [`TaskState`], in forward-lifecycle order.
    ///
    /// The ordering is cosmetic (used by tests and a few diagnostic
    /// paths) — consumers that care about semantic categories should use
    /// the classifier helpers ([`is_terminal`](Self::is_terminal),
    /// [`is_inflight`](Self::is_inflight),
    /// [`is_schedulable`](Self::is_schedulable)).
    pub const ALL: &'static [TaskState] = &[
        TaskState::Interactive,
        TaskState::Planning,
        TaskState::Refining,
        TaskState::Ready,
        TaskState::Active,
        TaskState::Review,
        TaskState::Approved,
        TaskState::Merging,
        TaskState::Failed,
        TaskState::Merged,
        TaskState::Done,
        TaskState::Closed,
    ];

    /// The canonical lower-case string form, matching the legacy wire
    /// and on-disk representation.
    pub fn as_str(self) -> &'static str {
        match self {
            TaskState::Interactive => "interactive",
            TaskState::Planning => "planning",
            TaskState::Refining => "refining",
            TaskState::Ready => "ready",
            TaskState::Active => "active",
            TaskState::Review => "review",
            TaskState::Approved => "approved",
            TaskState::Merging => "merging",
            TaskState::Failed => "failed",
            TaskState::Merged => "merged",
            TaskState::Done => "done",
            TaskState::Closed => "closed",
        }
    }

    /// Parse a state string (as written in the DB / on the wire).
    /// Returns a structured error for unknown variants.
    pub fn from_db_str(s: &str) -> Result<Self, InvalidTaskState> {
        match s {
            "interactive" => Ok(TaskState::Interactive),
            "planning" => Ok(TaskState::Planning),
            "refining" => Ok(TaskState::Refining),
            "ready" => Ok(TaskState::Ready),
            "active" => Ok(TaskState::Active),
            "review" => Ok(TaskState::Review),
            "approved" => Ok(TaskState::Approved),
            "merging" => Ok(TaskState::Merging),
            "failed" => Ok(TaskState::Failed),
            "merged" => Ok(TaskState::Merged),
            "done" => Ok(TaskState::Done),
            "closed" => Ok(TaskState::Closed),
            other => Err(InvalidTaskState(other.to_string())),
        }
    }

    /// Terminal states have no outgoing transitions the scheduler would
    /// follow automatically.  Used to suppress observational broadcasts
    /// on re-transition and to gate auto-archive.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            TaskState::Merged | TaskState::Done | TaskState::Closed | TaskState::Failed
        )
    }

    /// "In-flight" = the task is currently being worked by some live
    /// session.  Used by the scheduler for budget accounting and file-
    /// conflict detection against new dispatches.
    ///
    /// Note: `planning` is NOT included here — it is in-flight only
    /// when it has a session attached; the inflight check in
    /// [`TasksDb::count_inflight_tasks`] handles that special case in
    /// SQL.
    pub fn is_inflight(self) -> bool {
        matches!(
            self,
            TaskState::Active | TaskState::Review | TaskState::Merging | TaskState::Refining
        )
    }

    /// Schedulable = the scheduler may transition the task into a
    /// dispatched state on the next pass.
    pub fn is_schedulable(self) -> bool {
        matches!(self, TaskState::Ready | TaskState::Planning)
    }
}

impl fmt::Display for TaskState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for TaskState {
    type Err = InvalidTaskState;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::from_db_str(s)
    }
}

// --- SQLite round-trip ------------------------------------------------------

impl ToSql for TaskState {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.as_str()))
    }
}

impl FromSql for TaskState {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let s = value.as_str()?;
        TaskState::from_db_str(s).map_err(|e| FromSqlError::Other(Box::new(e)))
    }
}

// --- Transition validation --------------------------------------------------

/// Check whether a state transition is allowed.
///
/// Forward (happy path):
///   interactive -> planning -> refining -> ready -> active -> review -> approved -> merging -> merged
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
///   any state -> closed       (manual close)
///   any state -> interactive  (human takes over — except from merged)
///   any state -> failed       (terminal error)
///
/// Terminal states:
///   merged — fully terminal, no transitions out
///   done   — fully terminal (no_merge tasks), no transitions out
///   closed -> interactive     (reopen)
///   failed -> closed          (give up)
pub fn validate_transition(from: TaskState, to: TaskState) -> bool {
    use TaskState::*;

    // merged and done are fully terminal — no transitions out at all
    if from == Merged || from == Done {
        return false;
    }

    // Universal: any state can go to closed, interactive, or failed
    // (except self-loops, which are always rejected).
    if from != to && (to == Closed || to == Interactive || to == Failed) {
        return true;
    }

    matches!(
        (from, to),
        // Planning/Refining transitions
        (Interactive, Planning)
            | (Interactive, Refining)
            | (Planning, Refining)
            | (Refining, Planning)
            | (Refining, Ready)
            // Forward transitions
            | (Interactive, Ready)
            | (Interactive, Approved)
            | (Ready, Active)
            | (Active, Review)
            | (Active, Approved)
            | (Review, Approved)
            | (Approved, Merging)
            | (Approved, Done)
            | (Merging, Merged)
            // Backward transitions (error recovery)
            | (Active, Ready)
            | (Review, Active)
            | (Approved, Active)
            | (Approved, Ready)
            | (Approved, Interactive)
            | (Merging, Active)
            | (Merging, Failed)
            | (Failed, Active)
    )
}

/// Return `true` when transitioning `from -> to` means the task's
/// current `session_id` no longer refers to the session that is now
/// responsible for the task's phase.  On such transitions the DB layer
/// clears `tasks.session_id`.  See the pre-refactor doc-comment on
/// `tasks_db::should_clear_session_id_on_transition` for rationale.
pub fn should_clear_session_id_on_transition(from: TaskState, to: TaskState) -> bool {
    use TaskState::*;
    matches!(
        (from, to),
        (Planning, Refining) | (Refining, Ready) | (Active, Ready)
    )
}

// --- Tests ------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_as_str_from_db_str() {
        for &state in TaskState::ALL {
            let s = state.as_str();
            assert_eq!(TaskState::from_db_str(s), Ok(state));
            // FromStr should agree.
            assert_eq!(s.parse::<TaskState>(), Ok(state));
            // Display should agree.
            assert_eq!(format!("{}", state), s);
        }
    }

    #[test]
    fn serde_snake_case_matches_legacy() {
        let expected: &[(TaskState, &str)] = &[
            (TaskState::Interactive, "\"interactive\""),
            (TaskState::Planning, "\"planning\""),
            (TaskState::Refining, "\"refining\""),
            (TaskState::Ready, "\"ready\""),
            (TaskState::Active, "\"active\""),
            (TaskState::Review, "\"review\""),
            (TaskState::Approved, "\"approved\""),
            (TaskState::Merging, "\"merging\""),
            (TaskState::Failed, "\"failed\""),
            (TaskState::Merged, "\"merged\""),
            (TaskState::Done, "\"done\""),
            (TaskState::Closed, "\"closed\""),
        ];
        for (state, json) in expected {
            let serialized = serde_json::to_string(state).expect("serialize");
            assert_eq!(&serialized, json, "serialize {:?}", state);
            let deserialized: TaskState = serde_json::from_str(json).expect("deserialize");
            assert_eq!(deserialized, *state, "deserialize {}", json);
        }
    }

    #[test]
    fn unknown_string_is_error() {
        let err = TaskState::from_db_str("bogus").unwrap_err();
        assert_eq!(err.0, "bogus");
        assert!(format!("{}", err).contains("bogus"));
    }

    #[test]
    fn sqlite_roundtrip_every_variant() {
        let conn = rusqlite::Connection::open_in_memory().expect("open sqlite");
        conn.execute("CREATE TABLE t (state TEXT NOT NULL)", [])
            .expect("create");
        for &state in TaskState::ALL {
            conn.execute(
                "INSERT INTO t (state) VALUES (?1)",
                rusqlite::params![state],
            )
            .expect("insert");
            let read: TaskState = conn
                .query_row("SELECT state FROM t ORDER BY rowid DESC LIMIT 1", [], |r| {
                    r.get(0)
                })
                .expect("read");
            assert_eq!(read, state);
        }
    }

    /// Exhaustive parity check: the enum-based validator must agree with
    /// a hand-transcribed reference table for every (from, to) pair.
    ///
    /// The reference table below is the canonical truth — any planned
    /// change to the state machine must update it in lockstep with
    /// [`validate_transition`].
    #[test]
    fn validate_transition_matches_reference_table() {
        use TaskState::*;
        // Explicit list of every valid (from, to) pair.  No self-loops.
        const VALID: &[(TaskState, TaskState)] = &[
            // Planning/refining cycle
            (Interactive, Planning),
            (Interactive, Refining),
            (Planning, Refining),
            (Refining, Planning),
            (Refining, Ready),
            // Forward
            (Interactive, Ready),
            (Interactive, Approved),
            (Ready, Active),
            (Active, Review),
            (Active, Approved),
            (Review, Approved),
            (Approved, Merging),
            (Approved, Done),
            (Merging, Merged),
            // Backward / error recovery
            (Active, Ready),
            (Review, Active),
            (Approved, Active),
            (Approved, Ready),
            (Approved, Interactive),
            (Merging, Active),
            (Merging, Failed),
            (Failed, Active),
            // Universal "any -> closed" (except from merged, and no self-loop)
            (Interactive, Closed),
            (Planning, Closed),
            (Refining, Closed),
            (Ready, Closed),
            (Active, Closed),
            (Review, Closed),
            (Approved, Closed),
            (Merging, Closed),
            (Failed, Closed),
            // Universal "any -> interactive" (except from merged and from interactive itself)
            (Planning, Interactive),
            (Refining, Interactive),
            (Ready, Interactive),
            (Active, Interactive),
            (Review, Interactive),
            (Merging, Interactive),
            (Failed, Interactive),
            (Closed, Interactive),
            // Universal "any -> failed" (except from merged and from failed itself)
            (Interactive, Failed),
            (Planning, Failed),
            (Refining, Failed),
            (Ready, Failed),
            (Active, Failed),
            (Review, Failed),
            (Approved, Failed),
            (Closed, Failed),
        ];

        use std::collections::HashSet;
        let valid: HashSet<(TaskState, TaskState)> = VALID.iter().copied().collect();
        for &from in TaskState::ALL {
            for &to in TaskState::ALL {
                let allowed = validate_transition(from, to);
                let expected = valid.contains(&(from, to));
                assert_eq!(
                    allowed, expected,
                    "transition {:?} -> {:?}: expected allowed={}, got {}",
                    from, to, expected, allowed,
                );
            }
        }
    }

    #[test]
    fn merged_is_fully_terminal() {
        for &to in TaskState::ALL {
            assert!(
                !validate_transition(TaskState::Merged, to),
                "merged -> {:?} must be rejected",
                to
            );
        }
    }

    #[test]
    fn done_is_fully_terminal() {
        for &to in TaskState::ALL {
            assert!(
                !validate_transition(TaskState::Done, to),
                "done -> {:?} must be rejected",
                to
            );
        }
    }

    #[test]
    fn clear_session_id_set_is_exact() {
        use TaskState::*;
        let want: &[(TaskState, TaskState)] =
            &[(Planning, Refining), (Refining, Ready), (Active, Ready)];
        for &from in TaskState::ALL {
            for &to in TaskState::ALL {
                let clears = should_clear_session_id_on_transition(from, to);
                let expected = want.contains(&(from, to));
                assert_eq!(clears, expected, "{:?} -> {:?}", from, to);
            }
        }
    }

    #[test]
    fn classifier_sets() {
        use TaskState::*;
        for &s in TaskState::ALL {
            let t = matches!(s, Merged | Done | Closed | Failed);
            assert_eq!(s.is_terminal(), t, "terminal {:?}", s);
            let i = matches!(s, Active | Review | Merging | Refining);
            assert_eq!(s.is_inflight(), i, "inflight {:?}", s);
            let sc = matches!(s, Ready | Planning);
            assert_eq!(s.is_schedulable(), sc, "schedulable {:?}", s);
        }
    }
}
