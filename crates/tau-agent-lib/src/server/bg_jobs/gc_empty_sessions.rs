//! Periodic and on-startup garbage collector for empty sessions.
//!
//! See task #745 for the design rationale.  In short: sessions are
//! persisted at creation time, so any flow that creates a session but
//! never sends a first message (TUI picker abort, cancelled child
//! spawn, errored CLI invocation) leaves a row in `sessions` with
//! zero `messages` rows that nothing will ever clean up.  This job
//! deletes such rows after a configurable grace period, excluding
//! sessions that are currently live or referenced by a non-terminal
//! task.
//!
//! Wired up in [`super::super::run`] and
//! [`super::super::run_with_config`] against two triggers:
//!
//! - [`BgTrigger::OnStartup`] — catches empties left behind by a
//!   previous-run crash.
//! - [`BgTrigger::Periodic`] — catches in-run accumulation.
//!
//! Both registrations share the same [`BgJob::name`], which means the
//! periodic re-entrancy guard naturally serialises a periodic tick
//! against a still-running startup pass (desired behaviour).

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use super::super::bg_tasks::{BgJob, BgTaskScheduler, BgTrigger};
use super::super::state::{SharedState, lock_state};

/// Default grace period (seconds): empty sessions younger than this
/// are kept.  Race protection against just-created sessions whose
/// first message is in flight.
pub(crate) const DEFAULT_GC_EMPTY_GRACE_SECS: i64 = 60;

/// Default periodic interval between GC passes (minutes).
pub(crate) const DEFAULT_GC_EMPTY_INTERVAL_MINS: u64 = 30;

/// Default delay before the first periodic run (minutes).  Lets
/// startup settle before we do real work.
pub(crate) const DEFAULT_GC_EMPTY_DELAY_MINS: u64 = 5;

/// Read `TAU_GC_EMPTY_GRACE_SECS`, falling back to
/// [`DEFAULT_GC_EMPTY_GRACE_SECS`].  `0` is allowed and means "no
/// grace" (used by integration tests).
pub(crate) fn gc_empty_grace_secs() -> i64 {
    std::env::var("TAU_GC_EMPTY_GRACE_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_GC_EMPTY_GRACE_SECS)
}

/// Read `TAU_GC_EMPTY_INTERVAL_MINS`, falling back to
/// [`DEFAULT_GC_EMPTY_INTERVAL_MINS`].
pub(crate) fn gc_empty_interval_mins() -> u64 {
    std::env::var("TAU_GC_EMPTY_INTERVAL_MINS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_GC_EMPTY_INTERVAL_MINS)
}

/// Read `TAU_GC_EMPTY_DELAY_MINS`, falling back to
/// [`DEFAULT_GC_EMPTY_DELAY_MINS`].
pub(crate) fn gc_empty_delay_mins() -> u64 {
    std::env::var("TAU_GC_EMPTY_DELAY_MINS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_GC_EMPTY_DELAY_MINS)
}

/// Stateless `BgJob` that runs one [`crate::db::Db::gc_empty_sessions`]
/// pass per invocation.
pub(crate) struct GcEmptySessionsJob {
    grace_secs: i64,
}

impl GcEmptySessionsJob {
    pub(crate) fn new(grace_secs: i64) -> Self {
        Self { grace_secs }
    }
}

#[async_trait]
impl BgJob for GcEmptySessionsJob {
    fn name(&self) -> &'static str {
        "gc_empty_sessions"
    }

    async fn run(&self, state: &SharedState) {
        // 1. Open the tasks DB and gather the set of session ids
        //    referenced by any non-terminal task.  Done outside the
        //    main state lock — no shared resource needed.  On error,
        //    skip the entire pass: we don't want to nuke sessions we
        //    cannot verify.
        let task_owned: HashSet<String> = match crate::tasks_db::TasksDb::open_default() {
            Ok(db) => match db.list_protected_session_ids() {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "gc_empty_sessions: list_protected_session_ids failed; skipping pass"
                    );
                    return;
                }
            },
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "gc_empty_sessions: open tasks db failed; skipping pass"
                );
                return;
            }
        };

        // 2. Single critical section: snapshot `live_sessions` and
        //    run the GC against the same locked view.  Per-id deletes
        //    happen inside `gc_empty_sessions` and are quick (no
        //    recursive walk — by definition these sessions have no
        //    children).
        let deleted = {
            let st = lock_state(state);
            let live: HashSet<String> = st.live_sessions.iter().cloned().collect();
            match st.db.gc_empty_sessions(self.grace_secs, &live, &task_owned) {
                Ok(ids) => ids,
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "gc_empty_sessions: gc_empty_sessions failed"
                    );
                    return;
                }
            }
        };

        if deleted.is_empty() {
            tracing::debug!("gc_empty_sessions: no empties to delete");
        } else {
            tracing::info!(
                count = deleted.len(),
                ids = ?deleted,
                "gc_empty_sessions: deleted empty sessions"
            );
        }
    }
}

/// Register both the on-startup and periodic instances of the GC
/// job against `bg`.  Two separate `Arc` instances are fine — the
/// job is stateless apart from its `grace_secs` field, and sharing
/// the same [`BgJob::name`] is intentional (it serialises a periodic
/// tick against a still-running startup pass via the in-flight
/// guard).
pub(crate) async fn register_all(bg: &Arc<BgTaskScheduler>) {
    let grace = gc_empty_grace_secs();
    bg.register(
        BgTrigger::OnStartup,
        Arc::new(GcEmptySessionsJob::new(grace)),
    )
    .await;
    bg.register(
        BgTrigger::Periodic {
            delay: Duration::from_secs(gc_empty_delay_mins().saturating_mul(60)),
            interval: Duration::from_secs(gc_empty_interval_mins().saturating_mul(60)),
        },
        Arc::new(GcEmptySessionsJob::new(grace)),
    )
    .await;
}

#[cfg(test)]
mod tests {
    use super::super::super::ShutdownHandle;
    use super::super::super::state::State;
    use super::*;
    use crate::db::Db;
    use crate::db::StoredSession;
    use crate::provider::ProviderRegistry;
    use crate::types::{Model, ModelCost, ThinkingStyle};
    use std::collections::{HashMap, HashSet};
    use std::sync::Mutex;

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

    fn mk_state_with_scheduler(db: Db) -> (SharedState, Arc<BgTaskScheduler>, ShutdownHandle) {
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
        let sched = BgTaskScheduler::new(state.clone(), shutdown.clone());
        lock_state(&state).bg_scheduler = Some(sched.clone());
        (state, sched, shutdown)
    }

    fn empty_session(id: &str) -> StoredSession {
        StoredSession {
            id: id.into(),
            model: mk_model(),
            system_prompt: None,
            cwd: None,
            is_subscription: false,
            // Old enough to be past any grace window the test cares about.
            created_at: 1,
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
        }
    }

    #[test]
    fn job_run_deletes_empty_session_via_startup() {
        smol::block_on(async {
            let db = Db::open_memory().expect("open db");
            db.create_session(&empty_session("s-empty"))
                .expect("insert session");
            let (state, sched, _shutdown) = mk_state_with_scheduler(db);

            // Register and run the OnStartup pass once.
            sched
                .register(BgTrigger::OnStartup, Arc::new(GcEmptySessionsJob::new(0)))
                .await;
            sched.run_startup().await;

            let st = lock_state(&state);
            assert!(
                st.db.get_session("s-empty").expect("get").is_none(),
                "empty session should have been GC'd by the startup pass"
            );
        });
    }

    #[test]
    fn job_run_skips_live_session() {
        smol::block_on(async {
            let db = Db::open_memory().expect("open db");
            db.create_session(&empty_session("s-live"))
                .expect("insert session");
            let (state, sched, _shutdown) = mk_state_with_scheduler(db);

            // Mark it live before running the job.
            {
                let mut st = lock_state(&state);
                st.live_sessions.insert("s-live".into());
            }

            sched
                .register(BgTrigger::OnStartup, Arc::new(GcEmptySessionsJob::new(0)))
                .await;
            sched.run_startup().await;

            let st = lock_state(&state);
            assert!(
                st.db.get_session("s-live").expect("get").is_some(),
                "live empty session must be preserved"
            );
        });
    }
}
