//! Adapter shim: expose `PostIdleAction` over the new `bg_tasks` layer.
//!
//! The protocol surface
//! ([`Request::EnqueuePostIdleAction`](crate::protocol::Request::EnqueuePostIdleAction)
//! and the [`PostIdleAction`] enum) is unchanged.  Internally each
//! action variant is converted into a concrete
//! [`BgJob`](super::bg_tasks::BgJob) impl and registered against the
//! caller's session id via
//! [`BgTaskScheduler::enqueue_for_session`](super::bg_tasks::BgTaskScheduler::enqueue_for_session).
//!
//! # Retry / give-up policy
//!
//! `ArchiveTaskSessions` retries individual session archives up to
//! [`ARCHIVE_MAX_RETRIES`] times on transient "busy" errors (session
//! still listed in `live_sessions`), with [`ARCHIVE_RETRY_DELAY_MS`]
//! between attempts.  This logic moved verbatim from the previous
//! `post_idle` module into [`ArchiveTaskSessionsJob`].

use std::sync::Arc;

use async_trait::async_trait;

use super::bg_tasks::{BgJob, bg_scheduler};
use super::state::{SharedState, lock_state};
use crate::types::PostIdleAction;

/// Max retries for transient "busy" failures when archiving a session.
const ARCHIVE_MAX_RETRIES: u32 = 20;

/// Delay between archive retry attempts.
const ARCHIVE_RETRY_DELAY_MS: u64 = 1000;

/// Tunables for archival retry behaviour.  In production we use the
/// constants above (20 attempts, 1s apart); tests override these to
/// avoid 20-second test durations.
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

/// `BgJob` implementation for [`PostIdleAction::ArchiveTaskSessions`].
struct ArchiveTaskSessionsJob {
    task_id: i64,
    policy: ArchiveRetryPolicy,
}

#[async_trait]
impl BgJob for ArchiveTaskSessionsJob {
    fn name(&self) -> &'static str {
        "post_idle.archive_task_sessions"
    }

    async fn run(&self, state: &SharedState) {
        execute_archive_task_sessions_action(state, self.task_id, self.policy).await;
    }
}

/// Convert a `PostIdleAction` into a concrete background job.
fn job_for(action: PostIdleAction) -> Arc<dyn BgJob> {
    match action {
        PostIdleAction::ArchiveTaskSessions { task_id } => Arc::new(ArchiveTaskSessionsJob {
            task_id,
            policy: ArchiveRetryPolicy::production(),
        }),
    }
}

/// Enqueue a post-idle action for `session_id`, then drain inline if the
/// session is not currently running an agent turn.
///
/// Background: the queue is normally drained when the target session's
/// own agent loop completes.  But callers like reviewer sessions that
/// only mark a task `approved` and then sit idle never get another turn,
/// so without an inline drain the action would sit forever (task #594).
/// The session being absent from `live_sessions` means there is no agent
/// loop to race with, so executing the action right now is safe.
pub(super) async fn enqueue_and_maybe_drain(
    state: &SharedState,
    session_id: &str,
    action: PostIdleAction,
) {
    match bg_scheduler(state) {
        Some(sched) => sched.enqueue_for_session(session_id, job_for(action)).await,
        None => {
            tracing::warn!(
                session_id = %session_id,
                "bg scheduler not initialised; dropping post-idle action"
            );
        }
    }
}

/// Drain all post-idle actions enqueued for `session_id`.  Call this
/// after the agent loop has exited and the session's lock has been
/// released.
pub(super) async fn drain(state: &SharedState, session_id: &str) {
    if let Some(sched) = bg_scheduler(state) {
        sched.drain_for_session(session_id).await;
    } else {
        tracing::warn!(
            session_id = %session_id,
            "bg scheduler not initialised; nothing to drain"
        );
    }
}

/// Body of `ArchiveTaskSessionsJob::run`, factored out so tests can
/// invoke it directly with a fast retry policy.
async fn execute_archive_task_sessions_action(
    state: &SharedState,
    task_id: i64,
    policy: ArchiveRetryPolicy,
) {
    // Task #561: prefer the placeholder-subtree cascade when the task
    // has a placeholder session.  The server-side
    // `archive_session_tree` archives the placeholder and every
    // descendant in one hop, so we never need the per-role loop for
    // tasks created post-561.  Pre-561 tasks (no placeholder) fall
    // back to the legacy role-filter archival.
    let (placeholder_sid, sessions) = match crate::tasks_db::TasksDb::open_default() {
        Ok(db) => {
            let placeholder = db
                .get_task(task_id)
                .ok()
                .flatten()
                .and_then(|t| t.placeholder_session_id);
            let sessions = match db.get_sessions(task_id) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(task_id, %e, "post-idle: get_sessions failed");
                    return;
                }
            };
            (placeholder, sessions)
        }
        Err(e) => {
            tracing::warn!(task_id, %e, "post-idle: open tasks db failed");
            return;
        }
    };
    if let Some(sid) = placeholder_sid {
        archive_with_retries(state, &sid, policy).await;
    } else {
        execute_archive_task_sessions(state, &sessions, policy).await;
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
/// keeps us honest.  After `policy.max_retries` attempts we give up
/// with a warning rather than hang.
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
    use crate::server::ShutdownHandle;
    use crate::server::bg_tasks::BgTaskScheduler;
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

    /// Build a `SharedState` with an attached `BgTaskScheduler`, so
    /// the shim functions (`enqueue_and_maybe_drain`, `drain`) work.
    fn mk_state_with_scheduler() -> SharedState {
        let db = Db::open_memory().expect("open memory db");
        let state: SharedState = Arc::new(Mutex::new(State {
            db,
            registry: ProviderRegistry::new(),
            auth: crate::auth::AuthStorage::open_default(),
            config: crate::config::Config::default(),
            global_aliases: HashMap::new(),
            default_model: mk_model(),
            all_models: vec![mk_model()],
            usage_cache: None,
            cancel_flags: HashMap::new(),
            stop_after_tool_flags: HashMap::new(),
            has_queued: HashMap::new(),
            subscribers: HashMap::new(),
            phases: HashMap::new(),
            live_sessions: HashSet::new(),
            waited_sessions: HashSet::new(),
            session_done_waiters: Vec::new(),
            reply_waiters: HashMap::new(),
            next_msg_id: 0,
            bg_after_idle: HashMap::new(),
            bg_scheduler: None,
        }));
        let shutdown = ShutdownHandle::new();
        let sched = BgTaskScheduler::new(state.clone(), shutdown);
        lock_state(&state).bg_scheduler = Some(sched);
        state
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
                successor_id: None,
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

    /// Drain on an empty queue is a cheap no-op.
    #[test]
    fn drain_empty_queue_is_noop() {
        smol::block_on(async {
            let state = mk_state_with_scheduler();
            drain(&state, "s-nobody").await;
            let st = lock_state(&state);
            assert!(st.bg_after_idle.is_empty());
        });
    }

    /// Archive action against a session that isn't busy completes on
    /// the first attempt and archives the session tree.
    #[test]
    fn archive_with_retries_succeeds_when_not_busy() {
        smol::block_on(async {
            let state = mk_state_with_scheduler();
            insert_session(&state, "sid-live");
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

    /// Session is initially busy, another task clears the live flag
    /// during the retry loop, and the archive succeeds on a subsequent
    /// attempt.
    #[test]
    fn archive_with_retries_succeeds_after_session_becomes_idle() {
        smol::block_on(async {
            let state = mk_state_with_scheduler();
            insert_session(&state, "sid-racing");
            {
                let mut st = lock_state(&state);
                st.live_sessions.insert("sid-racing".into());
            }
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

    /// Persistently busy session → `archive_with_retries` gives up
    /// after `policy.max_retries` attempts without hanging.
    #[test]
    fn archive_with_retries_gives_up_when_persistently_busy() {
        smol::block_on(async {
            let state = mk_state_with_scheduler();
            insert_session(&state, "sid-stuck");
            {
                let mut st = lock_state(&state);
                st.live_sessions.insert("sid-stuck".into());
            }
            let started = std::time::Instant::now();
            archive_with_retries(&state, "sid-stuck", fast_policy()).await;
            let elapsed = started.elapsed();
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
    /// archivable task sessions results in each session being
    /// archived in the core DB, with archivable-role filtering
    /// applied.
    #[test]
    fn archive_task_sessions_archives_worker_reviewer_skips_creator() {
        smol::block_on(async {
            let state = mk_state_with_scheduler();
            insert_session(&state, "sid-worker");
            insert_session(&state, "sid-reviewer");
            insert_session(&state, "sid-creator");
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

    /// `enqueue_and_maybe_drain` drains inline when the target session
    /// is not currently running an agent turn (the task #594 fix
    /// path).  We use a task_id with no sessions in the on-disk tasks
    /// DB — the archive action becomes a no-op but the queue entry is
    /// still drained, proving the inline drain ran.
    #[test]
    fn enqueue_and_maybe_drain_runs_inline_when_session_idle() {
        smol::block_on(async {
            let state = mk_state_with_scheduler();
            enqueue_and_maybe_drain(
                &state,
                "s-idle",
                PostIdleAction::ArchiveTaskSessions { task_id: i64::MAX },
            )
            .await;
            let st = lock_state(&state);
            assert!(
                !st.bg_after_idle.contains_key("s-idle"),
                "queue should have been drained inline"
            );
        });
    }

    /// `enqueue_and_maybe_drain` defers to the normal turn-completion
    /// drain when the target session IS currently running a turn.
    /// The queue entry must remain so the running agent loop's exit
    /// drain picks it up.
    #[test]
    fn enqueue_and_maybe_drain_defers_when_session_live() {
        smol::block_on(async {
            let state = mk_state_with_scheduler();
            {
                let mut st = lock_state(&state);
                st.live_sessions.insert("s-busy".into());
            }
            enqueue_and_maybe_drain(
                &state,
                "s-busy",
                PostIdleAction::ArchiveTaskSessions { task_id: i64::MAX },
            )
            .await;
            let st = lock_state(&state);
            let queued = st
                .bg_after_idle
                .get("s-busy")
                .expect("entry should remain queued for live session");
            assert_eq!(queued.len(), 1, "exactly one action queued");
        });
    }

    /// An `ArchiveTaskSessions` action enqueued via the shim is
    /// drained when the session goes idle, and the queue entry is
    /// removed.  Uses `i64::MAX` so the on-disk tasks DB has no
    /// matching task — the archive becomes a no-op but the queue
    /// entry is still drained, proving the round-trip.
    #[test]
    fn drain_consumes_archive_task_sessions_entry() {
        smol::block_on(async {
            let state = mk_state_with_scheduler();
            {
                let mut st = lock_state(&state);
                st.live_sessions.insert("s-caller".into());
            }
            enqueue_and_maybe_drain(
                &state,
                "s-caller",
                PostIdleAction::ArchiveTaskSessions { task_id: i64::MAX },
            )
            .await;
            assert_eq!(
                lock_state(&state)
                    .bg_after_idle
                    .get("s-caller")
                    .map(|v| v.len())
                    .unwrap_or(0),
                1,
                "action should be queued while session is live"
            );
            {
                let mut st = lock_state(&state);
                st.live_sessions.remove("s-caller");
            }
            drain(&state, "s-caller").await;
            let st = lock_state(&state);
            assert!(
                !st.bg_after_idle.contains_key("s-caller"),
                "queue entry should be drained"
            );
        });
    }
}
