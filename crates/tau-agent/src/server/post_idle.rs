//! Tier-3 post-idle action drain.
//!
//! Actions enqueued via [`Request::EnqueuePostIdleAction`](crate::protocol::Request::EnqueuePostIdleAction)
//! land in [`State::post_idle_queue`](super::state::State::post_idle_queue)
//! keyed by the caller's session id.  Once the caller's agent loop exits
//! (session lock released), [`drain`] pops the queue and executes each
//! action.  Because this runs **after** the lock drops, actions may freely
//! grab session locks, hit the DB, and archive subtrees — none of which are
//! safe while the agent loop is alive.
//!
//! # Retry / give-up policy
//!
//! `ArchiveTaskSessions` retries individual session archives up to
//! [`ARCHIVE_MAX_RETRIES`] times on transient "busy" errors (session still
//! listed in `live_sessions`), with [`ARCHIVE_RETRY_DELAY_MS`] between
//! attempts.
//!
//! # Re-entrancy
//!
//! Actions executed during a drain round may themselves enqueue further
//! post-idle work.  [`drain`] therefore loops up to [`MAX_ROUNDS`] times,
//! draining the queue afresh each round, to catch those follow-ups without
//! risking an infinite loop.

use super::state::{SharedState, lock_state};
use crate::types::PostIdleAction;

/// Maximum number of drain rounds.  Actions that enqueue further work
/// during execution get drained on the next round, up to this cap.
const MAX_ROUNDS: usize = 5;

/// Max retries for transient "busy" failures when archiving a session.
const ARCHIVE_MAX_RETRIES: u32 = 20;

/// Delay between archive retry attempts.
const ARCHIVE_RETRY_DELAY_MS: u64 = 1000;

/// Tunables for archival retry behaviour.  In production we use the
/// constants above (20 attempts, 1s apart); tests override these to avoid
/// 20-second test durations.
#[derive(Debug, Clone, Copy)]
struct ArchiveRetryPolicy {
    max_retries: u32,
    delay_ms: u64,
}

impl ArchiveRetryPolicy {
    const fn production() -> Self {
        Self {
            max_retries: ARCHIVE_MAX_RETRIES,
            delay_ms: ARCHIVE_RETRY_DELAY_MS,
        }
    }
}

/// Drain all post-idle actions enqueued for `session_id`.  Call this after
/// the agent loop has exited and the session's lock has been released.
pub(super) async fn drain(state: &SharedState, session_id: &str) {
    drain_with_policy(state, session_id, ArchiveRetryPolicy::production()).await;
}

async fn drain_with_policy(state: &SharedState, session_id: &str, policy: ArchiveRetryPolicy) {
    for _round in 0..MAX_ROUNDS {
        let batch = {
            let mut st = lock_state(state);
            match st.post_idle_queue.remove(session_id) {
                Some(v) if !v.is_empty() => v,
                _ => return,
            }
        };

        for action in batch {
            execute_action(state, &action, policy).await;
        }
    }

    // If we're still producing work after MAX_ROUNDS, drop the remaining
    // actions with a warning — almost certainly a bug in whatever is
    // enqueueing them.
    let remaining = {
        let mut st = lock_state(state);
        st.post_idle_queue.remove(session_id).unwrap_or_default()
    };
    if !remaining.is_empty() {
        tracing::warn!(
            session_id = %session_id,
            remaining = remaining.len(),
            "post-idle drain exceeded max rounds, dropping remaining actions"
        );
    }
}

async fn execute_action(state: &SharedState, action: &PostIdleAction, policy: ArchiveRetryPolicy) {
    match action {
        PostIdleAction::ArchiveTaskSessions { task_id } => {
            let sessions = match crate::tasks_db::TasksDb::open_default() {
                Ok(db) => match db.get_sessions(*task_id) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!(task_id = *task_id, %e, "post-idle: get_sessions failed");
                        return;
                    }
                },
                Err(e) => {
                    tracing::warn!(task_id = *task_id, %e, "post-idle: open tasks db failed");
                    return;
                }
            };
            execute_archive_task_sessions(state, &sessions, policy).await;
        }
    }
}

async fn execute_archive_task_sessions(
    state: &SharedState,
    task_sessions: &[crate::tasks_db::TaskSession],
    policy: ArchiveRetryPolicy,
) {
    // Archivable roles only (worker, planner, reviewer, refiner, log) —
    // orchestrator / user sessions are left alone.
    let (to_archive, _to_skip) = crate::tasks_merge::sessions_to_archive(task_sessions);
    for ts in to_archive {
        archive_with_retries(state, &ts.session_id, policy).await;
    }
}

/// Archive a session with bounded retries on "busy" errors.
///
/// "Busy" here means the session is still listed in `live_sessions`
/// (a turn is running).  Because this drain is invoked *after* the
/// caller's own lock is released, the only way this trips is if a
/// concurrent turn on the same session started — rare, but the retry
/// keeps us honest.  After `policy.max_retries` attempts we give up with
/// a warning rather than hang.
async fn archive_with_retries(state: &SharedState, session_id: &str, policy: ArchiveRetryPolicy) {
    for attempt in 0..policy.max_retries {
        let busy = {
            let st = lock_state(state);
            st.live_sessions.contains(session_id)
        };
        if !busy {
            let st = lock_state(state);
            if let Err(e) = st.db.archive_session_tree(session_id) {
                tracing::warn!(%session_id, %e, "post-idle: archive_session_tree failed");
            }
            return;
        }
        tracing::debug!(
            %session_id,
            attempt,
            "post-idle: session still live, retrying archive"
        );
        smol::Timer::after(std::time::Duration::from_millis(policy.delay_ms)).await;
    }
    tracing::warn!(
        %session_id,
        max_retries = policy.max_retries,
        "post-idle: giving up on archive after exhausting retries"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::provider::ProviderRegistry;
    use crate::server::state::{SharedState, State};
    use crate::types::{Model, ModelCost, PostIdleAction, ThinkingStyle};
    use std::collections::{HashMap, HashSet};
    use std::sync::{Arc, Mutex};

    fn mk_model() -> Model {
        Model {
            id: "test/test".into(),
            name: "test".into(),
            api: "anthropic".into(),
            provider: "test".into(),
            base_url: "".into(),
            thinking: ThinkingStyle::default(),
            cost: ModelCost::default(),
            context_window: 100_000,
            max_tokens: 4096,
            headers: HashMap::new(),
        }
    }

    fn mk_state() -> SharedState {
        let db = Db::open_memory().expect("open memory db");
        Arc::new(Mutex::new(State {
            db,
            registry: ProviderRegistry::new(),
            auth: crate::auth::AuthStorage::open_default(),
            config: crate::config::Config::default(),
            global_aliases: HashMap::new(),
            default_model: mk_model(),
            all_models: vec![mk_model()],
            usage_cache: None,
            cancel_flags: HashMap::new(),
            has_queued: HashMap::new(),
            subscribers: HashMap::new(),
            phases: HashMap::new(),
            live_sessions: HashSet::new(),
            waited_sessions: HashSet::new(),
            session_done_waiters: Vec::new(),
            reply_waiters: HashMap::new(),
            next_msg_id: 0,
            post_idle_queue: HashMap::new(),
        }))
    }

    fn insert_session(state: &SharedState, id: &str) {
        let st = lock_state(state);
        st.db
            .create_session(&crate::db::StoredSession {
                id: id.to_string(),
                model: mk_model(),
                system_prompt: None,
                cwd: None,
                is_subscription: false,
                created_at: 0,
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
            .expect("create session");
    }

    /// Fast retry policy for tests (10ms delay, 3 attempts).
    fn fast_policy() -> ArchiveRetryPolicy {
        ArchiveRetryPolicy {
            max_retries: 3,
            delay_ms: 10,
        }
    }

    /// `drain` on an empty queue is a cheap no-op.
    #[test]
    fn drain_empty_queue_is_noop() {
        smol::block_on(async {
            let state = mk_state();
            drain(&state, "s-nobody").await;
            // Nothing to assert: should complete without panic or hang.
            let st = lock_state(&state);
            assert!(st.post_idle_queue.is_empty());
        });
    }

    /// Archive action against a session that isn't busy completes on the
    /// first attempt and archives the session tree.
    #[test]
    fn archive_with_retries_succeeds_when_not_busy() {
        smol::block_on(async {
            let state = mk_state();
            insert_session(&state, "sid-live");
            // No entry in live_sessions → archive should succeed.
            archive_with_retries(&state, "sid-live", fast_policy()).await;
            let st = lock_state(&state);
            let info = st
                .db
                .get_session("sid-live")
                .expect("db")
                .expect("session present");
            assert!(info.archived, "expected session archived");
        });
    }

    /// Session is initially busy, another task clears the live flag during
    /// the retry loop, and the archive succeeds on a subsequent attempt.
    #[test]
    fn archive_with_retries_succeeds_after_session_becomes_idle() {
        smol::block_on(async {
            let state = mk_state();
            insert_session(&state, "sid-racing");
            {
                let mut st = lock_state(&state);
                st.live_sessions.insert("sid-racing".into());
            }

            // Spawn a task that clears the live flag after ~15ms (so at
            // least one retry happens).
            let state_clear = state.clone();
            let clearer = smol::spawn(async move {
                smol::Timer::after(std::time::Duration::from_millis(15)).await;
                let mut st = lock_state(&state_clear);
                st.live_sessions.remove("sid-racing");
            });

            archive_with_retries(&state, "sid-racing", fast_policy()).await;
            clearer.await;

            let st = lock_state(&state);
            let info = st
                .db
                .get_session("sid-racing")
                .expect("db")
                .expect("session present");
            assert!(info.archived, "archive should have retried and succeeded");
        });
    }

    /// Persistently busy session → archive_with_retries gives up after
    /// policy.max_retries attempts without hanging.  Session remains
    /// un-archived.
    #[test]
    fn archive_with_retries_gives_up_when_persistently_busy() {
        smol::block_on(async {
            let state = mk_state();
            insert_session(&state, "sid-stuck");
            {
                let mut st = lock_state(&state);
                st.live_sessions.insert("sid-stuck".into());
            }

            let started = std::time::Instant::now();
            archive_with_retries(&state, "sid-stuck", fast_policy()).await;
            let elapsed = started.elapsed();

            // 3 attempts × 10ms ≈ 30ms, bounded by a generous ceiling so
            // we catch a hang (would be tens of seconds in prod).
            assert!(
                elapsed < std::time::Duration::from_secs(1),
                "gave up too slowly: {:?}",
                elapsed
            );
            let st = lock_state(&state);
            let info = st
                .db
                .get_session("sid-stuck")
                .expect("db")
                .expect("session present");
            assert!(
                !info.archived,
                "should not archive a persistently busy session"
            );
        });
    }

    /// End-to-end: feeding `execute_archive_task_sessions` the list of
    /// archivable task sessions results in each session being archived
    /// in the core DB, with archivable-role filtering applied.  Covers
    /// the spec's "Tier-3 archive" happy path — the action drained by
    /// the reviewer's turn exit archives the worker + reviewer sessions.
    ///
    /// We bypass `TasksDb::open_default()` (which reads a global on-disk
    /// path) by invoking the inner helper directly with an in-memory
    /// task session list.  The full `drain(…, ArchiveTaskSessions)` path
    /// is covered indirectly: its only side effects beyond
    /// `execute_archive_task_sessions` are DB-open + get_sessions, both
    /// straight-through helpers.
    #[test]
    fn archive_task_sessions_archives_worker_reviewer_skips_creator() {
        smol::block_on(async {
            let state = mk_state();
            insert_session(&state, "sid-worker");
            insert_session(&state, "sid-reviewer");
            insert_session(&state, "sid-creator"); // orchestrator — must NOT be archived

            let sessions = vec![
                crate::tasks_db::TaskSession {
                    task_id: 1,
                    session_id: "sid-worker".into(),
                    role: "worker".into(),
                    created_at: 0,
                },
                crate::tasks_db::TaskSession {
                    task_id: 1,
                    session_id: "sid-reviewer".into(),
                    role: "reviewer".into(),
                    created_at: 0,
                },
                crate::tasks_db::TaskSession {
                    task_id: 1,
                    session_id: "sid-creator".into(),
                    role: "creator".into(),
                    created_at: 0,
                },
            ];

            execute_archive_task_sessions(&state, &sessions, fast_policy()).await;

            let st = lock_state(&state);
            let w = st.db.get_session("sid-worker").expect("db").expect("w");
            let r = st.db.get_session("sid-reviewer").expect("db").expect("r");
            let c = st.db.get_session("sid-creator").expect("db").expect("c");
            assert!(w.archived, "worker should be archived");
            assert!(r.archived, "reviewer should be archived");
            assert!(!c.archived, "creator (orchestrator) role must be preserved");
        });
    }

    /// An `ArchiveTaskSessions` action enqueued in `post_idle_queue` is
    /// drained, and the queue entry is removed.  Uses a task_id that the
    /// on-disk tasks DB likely doesn't know about — the drain handles the
    /// "no sessions to archive" path without error and still clears the
    /// queue.
    #[test]
    fn drain_consumes_archive_task_sessions_entry() {
        smol::block_on(async {
            let state = mk_state();
            {
                let mut st = lock_state(&state);
                st.post_idle_queue.insert(
                    "s-caller".into(),
                    vec![PostIdleAction::ArchiveTaskSessions { task_id: i64::MAX }],
                );
            }
            drain_with_policy(&state, "s-caller", fast_policy()).await;
            let st = lock_state(&state);
            assert!(
                !st.post_idle_queue.contains_key("s-caller"),
                "queue entry should be drained"
            );
        });
    }
}
