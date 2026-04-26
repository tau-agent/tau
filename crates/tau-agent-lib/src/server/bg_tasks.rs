//! Background-task scheduler for deferred and periodic server work.
//!
//! Replaces and generalises the bespoke `post_idle.rs` queue with a
//! single layer that handles three kinds of triggers today:
//!
//! - [`BgTrigger::AfterSessionIdle`] — run a job once after the
//!   target session's agent loop exits.  Inline-runs immediately if
//!   the session is not currently live.  Used by `PostIdleAction`.
//! - [`BgTrigger::Periodic`] — fire on a fixed interval starting
//!   `delay` from now.  One owning [`smol::spawn`] task per registered
//!   periodic job; self-terminates when the [`ShutdownHandle`] flips.
//! - [`BgTrigger::OnStartup`] — runs sequentially during
//!   [`BgTaskScheduler::run_startup`], called by `Server::run` before
//!   the listener accepts connections.
//!
//! The other variants ([`BgTrigger::AfterDelay`], [`BgTrigger::OnShutdown`],
//! [`BgTrigger::WhenAllSessionsIdle`]) exist on the enum but are not
//! yet wired up — registration logs a `tracing::warn!` and otherwise
//! no-ops.  Add them when a real consumer needs them.
//!
//! # Persistence
//!
//! Pending background work is in-memory only.  `Periodic` jobs that
//! haven't fired yet are simply lost on server restart.  This is fine
//! for GC-class jobs (they run again on the next interval) but callers
//! that need at-least-once semantics across restarts must build their
//! own persistence on top.
//!
//! # Concurrency
//!
//! - `AfterSessionIdle` jobs queued for the same session id run
//!   sequentially in registration order.  The per-session pending list
//!   lives on [`State::bg_after_idle`] (not on the scheduler) so the
//!   "enqueue + check live" critical section stays atomic — same lock
//!   discipline as the previous `post_idle` implementation.
//! - `Periodic` jobs are guarded by a re-entrancy set keyed on
//!   [`BgJob::name`]: if the previous tick is still running when the
//!   next one fires, the new tick is skipped with a `debug!` log.
//!   This guard does **not** apply to `AfterSessionIdle` jobs.
//! - `BgJob::run` panics are caught and logged at `warn`; the
//!   scheduler stays alive.

use std::collections::HashSet;
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::FutureExt;

use super::ShutdownHandle;
use super::state::{SharedState, lock_state};

/// Maximum number of drain rounds for [`AfterSessionIdle`] jobs.  A
/// job that itself enqueues more `AfterSessionIdle` work for the same
/// session is picked up on the next round, capped to prevent infinite
/// loops.
///
/// Mirrors the constant of the same name in the legacy `post_idle`
/// module.
pub(super) const MAX_DRAIN_ROUNDS: usize = 5;

/// A unit of background work.
///
/// Implementations are stateless w.r.t. the scheduler: they receive a
/// [`SharedState`] handle and grab the lock the same way other server
/// code does.  Errors are logged inside `run`, never propagated.
#[async_trait]
pub(super) trait BgJob: Send + Sync + 'static {
    /// Stable name for logging and the periodic re-entrancy guard.
    ///
    /// Must be globally unique across all *periodic* registrations —
    /// two periodic jobs sharing a name will incorrectly suppress each
    /// other via the in-flight guard.  No uniqueness requirement for
    /// `AfterSessionIdle` jobs.
    fn name(&self) -> &'static str;

    /// Execute one run.  Errors are logged, not returned.
    async fn run(&self, state: &SharedState);
}

/// When a background job should fire.
#[allow(dead_code)] // OnShutdown / AfterDelay / WhenAllSessionsIdle are not
// wired up yet; the variants exist on the public surface
// so the first consumer doesn't have to widen the enum.
pub(super) enum BgTrigger {
    /// Run once after the next time `session_id`'s agent loop exits.
    /// Inline-runs immediately if the session is not currently live.
    AfterSessionIdle { session_id: String },
    /// Run once after `delay` (not yet implemented).
    AfterDelay { delay: Duration },
    /// Run on a fixed interval, starting `delay` from now.
    Periodic { delay: Duration, interval: Duration },
    /// Run once during [`BgTaskScheduler::run_startup`].
    OnStartup,
    /// Run once during clean server shutdown (not yet implemented).
    OnShutdown,
    /// Run when no session is live (not yet implemented).
    WhenAllSessionsIdle,
}

/// Owns per-session deferred and periodic background work.
///
/// One instance per `Server::run` invocation.  Held inside [`State`]
/// behind an `Arc` so dispatch handlers can reach it via
/// [`bg_scheduler`].  The cycle `State -> Arc<BgTaskScheduler> ->
/// SharedState -> State` is intentional and process-lifetime-scoped;
/// process exit reclaims the memory.
pub(super) struct BgTaskScheduler {
    /// Strong reference back to the server's `SharedState`.  Forms a
    /// cycle with [`State::bg_scheduler`]; see the type-level note.
    state: SharedState,
    /// Held so periodic loops can self-terminate when shutdown is
    /// requested.  Unused until the first `Periodic` job is
    /// registered (task #745 lands the first one).
    #[allow(dead_code)]
    shutdown: ShutdownHandle,
    /// Names of periodic jobs whose previous run is still in flight.
    /// Used to skip overlapping ticks.  Does **not** apply to
    /// `AfterSessionIdle` jobs.  Unused until the first `Periodic`
    /// job is registered.
    #[allow(dead_code)]
    periodic_in_flight: Mutex<HashSet<&'static str>>,
    /// `OnStartup` jobs queued by [`Self::register`]; consumed once by
    /// [`Self::run_startup`].
    startup: Mutex<Vec<Arc<dyn BgJob>>>,
}

impl BgTaskScheduler {
    pub(super) fn new(state: SharedState, shutdown: ShutdownHandle) -> Arc<Self> {
        Arc::new(Self {
            state,
            shutdown,
            periodic_in_flight: Mutex::new(HashSet::new()),
            startup: Mutex::new(Vec::new()),
        })
    }

    /// Register a job against a trigger.  Behaviour by variant:
    ///
    /// - `Periodic` — spawns a detached `smol` loop task.
    /// - `OnStartup` — pushes onto the startup queue.
    /// - `AfterSessionIdle` — appends to `State.bg_after_idle[session_id]`
    ///   and (if the session isn't live) drains inline immediately.
    ///   Most callers should prefer the explicit
    ///   [`Self::enqueue_for_session`] alias for clarity.
    /// - Others — logs a warning and returns.
    #[allow(dead_code)] // first production caller lands with #745
    pub(super) async fn register(self: &Arc<Self>, trigger: BgTrigger, job: Arc<dyn BgJob>) {
        match trigger {
            BgTrigger::AfterSessionIdle { session_id } => {
                self.enqueue_for_session(&session_id, job).await;
            }
            BgTrigger::Periodic { delay, interval } => {
                tracing::info!(
                    job = job.name(),
                    delay_ms = delay.as_millis() as u64,
                    interval_ms = interval.as_millis() as u64,
                    "registered periodic bg job"
                );
                let sched = self.clone();
                smol::spawn(async move {
                    sched.run_periodic(job, delay, interval).await;
                })
                .detach();
            }
            BgTrigger::OnStartup => {
                tracing::info!(job = job.name(), "registered startup bg job");
                self.startup
                    .lock()
                    .expect("bg_tasks startup mutex poisoned")
                    .push(job);
            }
            BgTrigger::AfterDelay { .. }
            | BgTrigger::OnShutdown
            | BgTrigger::WhenAllSessionsIdle => {
                tracing::warn!(
                    job = job.name(),
                    "BgTrigger variant not yet supported; registration ignored"
                );
            }
        }
    }

    /// Enqueue a job to run after `session_id`'s agent loop exits.
    /// Drains inline if the session is not currently live.
    ///
    /// The "push + check live" pair runs under a single `lock_state`
    /// to preserve atomicity; this is the moral equivalent of the
    /// legacy `post_idle::enqueue_and_maybe_drain`.
    pub(super) async fn enqueue_for_session(
        self: &Arc<Self>,
        session_id: &str,
        job: Arc<dyn BgJob>,
    ) {
        let drain_inline = {
            let mut st = lock_state(&self.state);
            st.bg_after_idle
                .entry(session_id.to_string())
                .or_default()
                .push(job);
            !st.live_sessions.contains(session_id)
        };
        if drain_inline {
            self.drain_for_session(session_id).await;
        }
    }

    /// Drain all pending `AfterSessionIdle` jobs for `session_id`.
    ///
    /// Call this *after* the session's lock has been released.  Jobs
    /// that themselves enqueue more work for the same session are
    /// picked up on the next round, capped at [`MAX_DRAIN_ROUNDS`].
    pub(super) async fn drain_for_session(self: &Arc<Self>, session_id: &str) {
        for _round in 0..MAX_DRAIN_ROUNDS {
            let batch = {
                let mut st = lock_state(&self.state);
                match st.bg_after_idle.remove(session_id) {
                    Some(v) if !v.is_empty() => v,
                    _ => return,
                }
            };
            for job in batch {
                self.run_job_safely(job).await;
            }
        }
        let remaining = {
            let mut st = lock_state(&self.state);
            st.bg_after_idle.remove(session_id).unwrap_or_default()
        };
        if !remaining.is_empty() {
            tracing::warn!(
                session_id = %session_id,
                remaining = remaining.len(),
                "bg_tasks drain exceeded max rounds, dropping remaining jobs"
            );
        }
    }

    /// Run all `OnStartup` jobs sequentially.  Called once by
    /// `Server::run` before the listener accepts connections.
    pub(super) async fn run_startup(self: &Arc<Self>) {
        let jobs: Vec<Arc<dyn BgJob>> = {
            let mut q = self
                .startup
                .lock()
                .expect("bg_tasks startup mutex poisoned");
            std::mem::take(&mut *q)
        };
        for job in jobs {
            tracing::info!(job = job.name(), "running startup bg job");
            self.run_job_safely(job).await;
        }
    }

    /// Periodic loop body: sleep for `delay`, then run-then-sleep on
    /// `interval`.  Self-terminates when the shutdown flag is set.
    #[allow(dead_code)] // exercised by tests + future #745 GC job
    async fn run_periodic(
        self: Arc<Self>,
        job: Arc<dyn BgJob>,
        delay: Duration,
        interval: Duration,
    ) {
        smol::Timer::after(delay).await;
        loop {
            if self.shutdown.is_shutting_down() {
                tracing::debug!(job = job.name(), "periodic bg job exiting (shutdown)");
                return;
            }
            // Re-entrancy guard.  If a previous tick is still running,
            // skip this one and try again next interval.
            let admitted = {
                let mut set = self
                    .periodic_in_flight
                    .lock()
                    .expect("bg_tasks in-flight mutex poisoned");
                set.insert(job.name())
            };
            if admitted {
                let job_clone = job.clone();
                let state_clone = self.state.clone();
                let name = job.name();
                tracing::debug!(job = name, "periodic bg job tick");
                let result = AssertUnwindSafe(job_clone.run(&state_clone))
                    .catch_unwind()
                    .await;
                {
                    let mut set = self
                        .periodic_in_flight
                        .lock()
                        .expect("bg_tasks in-flight mutex poisoned");
                    set.remove(name);
                }
                if result.is_err() {
                    tracing::warn!(job = name, "periodic bg job panicked");
                }
            } else {
                tracing::debug!(
                    job = job.name(),
                    "periodic bg job tick skipped (previous run still in flight)"
                );
            }
            smol::Timer::after(interval).await;
        }
    }

    /// Run a single job, catching panics so they don't propagate.
    async fn run_job_safely(self: &Arc<Self>, job: Arc<dyn BgJob>) {
        let name = job.name();
        tracing::debug!(job = name, "running bg job");
        let state = self.state.clone();
        let result = AssertUnwindSafe(async move { job.run(&state).await })
            .catch_unwind()
            .await;
        if result.is_err() {
            tracing::warn!(job = name, "bg job panicked");
        }
    }
}

/// Convenience accessor: clone the scheduler `Arc` out of the locked
/// state.  Returns `None` if the server constructed `State` without
/// attaching a scheduler (test fixtures, defensive only — production
/// code paths attach one synchronously after wrapping `State` into
/// `SharedState`).
pub(super) fn bg_scheduler(state: &SharedState) -> Option<Arc<BgTaskScheduler>> {
    let st = lock_state(state);
    st.bg_scheduler.clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::provider::ProviderRegistry;
    use crate::server::state::State;
    use crate::types::{Model, ModelCost, ThinkingStyle};
    use std::collections::{HashMap, HashSet as StdHashSet};
    use std::sync::atomic::{AtomicUsize, Ordering};

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

    fn mk_state_with_scheduler() -> (SharedState, Arc<BgTaskScheduler>, ShutdownHandle) {
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
            has_queued: HashMap::new(),
            subscribers: HashMap::new(),
            phases: HashMap::new(),
            live_sessions: StdHashSet::new(),
            waited_sessions: StdHashSet::new(),
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

    /// Increment-on-run job for cadence / re-entrancy assertions.
    struct CountingJob {
        name: &'static str,
        count: Arc<AtomicUsize>,
        sleep_ms: u64,
    }

    #[async_trait]
    impl BgJob for CountingJob {
        fn name(&self) -> &'static str {
            self.name
        }
        async fn run(&self, _state: &SharedState) {
            self.count.fetch_add(1, Ordering::SeqCst);
            if self.sleep_ms > 0 {
                smol::Timer::after(Duration::from_millis(self.sleep_ms)).await;
            }
        }
    }

    /// Panics on first run, increments on subsequent runs.
    struct PanicOnceJob {
        runs: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl BgJob for PanicOnceJob {
        fn name(&self) -> &'static str {
            "panic-once"
        }
        async fn run(&self, _state: &SharedState) {
            let n = self.runs.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                panic!("first run panics by design");
            }
        }
    }

    /// Records its name in a shared list when run.
    struct RecordingJob {
        name: &'static str,
        log: Arc<Mutex<Vec<&'static str>>>,
    }

    #[async_trait]
    impl BgJob for RecordingJob {
        fn name(&self) -> &'static str {
            self.name
        }
        async fn run(&self, _state: &SharedState) {
            self.log.lock().expect("log mutex poisoned").push(self.name);
        }
    }

    #[test]
    fn periodic_fires_on_cadence() {
        smol::block_on(async {
            let (_state, sched, shutdown) = mk_state_with_scheduler();
            let count = Arc::new(AtomicUsize::new(0));
            let job = Arc::new(CountingJob {
                name: "cadence",
                count: count.clone(),
                sleep_ms: 0,
            });
            sched
                .register(
                    BgTrigger::Periodic {
                        delay: Duration::from_millis(10),
                        interval: Duration::from_millis(10),
                    },
                    job,
                )
                .await;
            smol::Timer::after(Duration::from_millis(80)).await;
            shutdown.request_shutdown(false);
            let n = count.load(Ordering::SeqCst);
            assert!(n >= 3, "expected ≥3 ticks in ~80ms, got {n}");
        });
    }

    #[test]
    fn periodic_skips_when_in_flight() {
        smol::block_on(async {
            let (_state, sched, shutdown) = mk_state_with_scheduler();
            let count = Arc::new(AtomicUsize::new(0));
            // Job sleeps 100ms; ticks fire every 10ms — most ticks
            // should be skipped by the re-entrancy guard.
            let job = Arc::new(CountingJob {
                name: "slow",
                count: count.clone(),
                sleep_ms: 100,
            });
            sched
                .register(
                    BgTrigger::Periodic {
                        delay: Duration::from_millis(5),
                        interval: Duration::from_millis(10),
                    },
                    job,
                )
                .await;
            smol::Timer::after(Duration::from_millis(60)).await;
            shutdown.request_shutdown(false);
            let n = count.load(Ordering::SeqCst);
            // At t=60ms we've fired the timer ~5 times but only one run
            // could be in flight at any moment, and that single run is
            // still sleeping (100ms > 60ms).  Allow up to 2 in case of
            // scheduler slop.
            assert!(n <= 2, "expected ≤2 starts, got {n}");
            assert!(n >= 1, "expected ≥1 start, got {n}");
        });
    }

    #[test]
    fn panicking_job_does_not_kill_scheduler() {
        smol::block_on(async {
            let (_state, sched, shutdown) = mk_state_with_scheduler();
            let runs = Arc::new(AtomicUsize::new(0));
            let job = Arc::new(PanicOnceJob { runs: runs.clone() });
            sched
                .register(
                    BgTrigger::Periodic {
                        delay: Duration::from_millis(5),
                        interval: Duration::from_millis(10),
                    },
                    job,
                )
                .await;
            smol::Timer::after(Duration::from_millis(80)).await;
            shutdown.request_shutdown(false);
            let n = runs.load(Ordering::SeqCst);
            assert!(
                n >= 2,
                "loop should have continued past the panic; runs = {n}"
            );
        });
    }

    #[test]
    fn after_session_idle_runs_inline_when_idle() {
        smol::block_on(async {
            let (state, sched, _shutdown) = mk_state_with_scheduler();
            let count = Arc::new(AtomicUsize::new(0));
            let job = Arc::new(CountingJob {
                name: "inline",
                count: count.clone(),
                sleep_ms: 0,
            });
            sched.enqueue_for_session("s-idle", job).await;
            assert_eq!(count.load(Ordering::SeqCst), 1);
            let st = lock_state(&state);
            assert!(
                !st.bg_after_idle.contains_key("s-idle"),
                "queue should be drained inline"
            );
        });
    }

    #[test]
    fn after_session_idle_defers_when_live() {
        smol::block_on(async {
            let (state, sched, _shutdown) = mk_state_with_scheduler();
            {
                let mut st = lock_state(&state);
                st.live_sessions.insert("s-busy".into());
            }
            let count = Arc::new(AtomicUsize::new(0));
            let job = Arc::new(CountingJob {
                name: "deferred",
                count: count.clone(),
                sleep_ms: 0,
            });
            sched.enqueue_for_session("s-busy", job).await;
            assert_eq!(count.load(Ordering::SeqCst), 0, "should not run inline");
            let st = lock_state(&state);
            let queued = st
                .bg_after_idle
                .get("s-busy")
                .expect("entry present for live session");
            assert_eq!(queued.len(), 1);
        });
    }

    #[test]
    fn drain_runs_jobs_in_order() {
        smol::block_on(async {
            let (state, sched, _shutdown) = mk_state_with_scheduler();
            // Mark live so enqueues defer; we'll drain manually after.
            {
                let mut st = lock_state(&state);
                st.live_sessions.insert("s-order".into());
            }
            let log = Arc::new(Mutex::new(Vec::<&'static str>::new()));
            sched
                .enqueue_for_session(
                    "s-order",
                    Arc::new(RecordingJob {
                        name: "first",
                        log: log.clone(),
                    }),
                )
                .await;
            sched
                .enqueue_for_session(
                    "s-order",
                    Arc::new(RecordingJob {
                        name: "second",
                        log: log.clone(),
                    }),
                )
                .await;
            sched
                .enqueue_for_session(
                    "s-order",
                    Arc::new(RecordingJob {
                        name: "third",
                        log: log.clone(),
                    }),
                )
                .await;
            // Now drop "live" and drain.
            {
                let mut st = lock_state(&state);
                st.live_sessions.remove("s-order");
            }
            sched.drain_for_session("s-order").await;
            let log = log.lock().expect("log mutex poisoned");
            assert_eq!(*log, vec!["first", "second", "third"]);
        });
    }

    #[test]
    fn on_startup_runs_each_job_once() {
        smol::block_on(async {
            let (_state, sched, _shutdown) = mk_state_with_scheduler();
            let log = Arc::new(Mutex::new(Vec::<&'static str>::new()));
            sched
                .register(
                    BgTrigger::OnStartup,
                    Arc::new(RecordingJob {
                        name: "boot-1",
                        log: log.clone(),
                    }),
                )
                .await;
            sched
                .register(
                    BgTrigger::OnStartup,
                    Arc::new(RecordingJob {
                        name: "boot-2",
                        log: log.clone(),
                    }),
                )
                .await;
            sched.run_startup().await;
            assert_eq!(
                *log.lock().expect("log mutex poisoned"),
                vec!["boot-1", "boot-2"]
            );
            // Second call is a no-op (queue was drained).
            sched.run_startup().await;
            assert_eq!(log.lock().expect("log mutex poisoned").len(), 2);
        });
    }

    #[test]
    fn unsupported_trigger_logs_and_no_ops() {
        smol::block_on(async {
            let (_state, sched, _shutdown) = mk_state_with_scheduler();
            let count = Arc::new(AtomicUsize::new(0));
            sched
                .register(
                    BgTrigger::OnShutdown,
                    Arc::new(CountingJob {
                        name: "noop-shutdown",
                        count: count.clone(),
                        sleep_ms: 0,
                    }),
                )
                .await;
            sched
                .register(
                    BgTrigger::AfterDelay {
                        delay: Duration::from_millis(0),
                    },
                    Arc::new(CountingJob {
                        name: "noop-delay",
                        count: count.clone(),
                        sleep_ms: 0,
                    }),
                )
                .await;
            sched
                .register(
                    BgTrigger::WhenAllSessionsIdle,
                    Arc::new(CountingJob {
                        name: "noop-allidle",
                        count: count.clone(),
                        sleep_ms: 0,
                    }),
                )
                .await;
            // None of these should run; no startup queue either.
            sched.run_startup().await;
            assert_eq!(count.load(Ordering::SeqCst), 0);
        });
    }
}
