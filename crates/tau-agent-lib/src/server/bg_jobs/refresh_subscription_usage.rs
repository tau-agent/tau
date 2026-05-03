//! Periodic refresher for the cached Anthropic subscription usage.
//!
//! Replaces the previous "client polls every N seconds" model with a
//! server-driven refresh that:
//!
//! 1. Fetches `/usage` from Anthropic on a fixed interval.
//! 2. Updates `State.usage_cache` so on-demand
//!    [`crate::protocol::Request::GetSubscriptionUsage`] calls almost
//!    always hit a warm cache.
//! 3. Pushes the new value to every connected TUI as a
//!    [`Response::SubscriptionUsage`] broadcast — the existing TUI
//!    handler ([`tau_agent_tui`]) treats those as authoritative
//!    refreshes, so the status line updates without any client polling.
//!
//! When no Anthropic OAuth credential is configured the job no-ops
//! silently: tau installs that only use API keys (or only OpenAI) pay
//! no network cost and produce no warnings.
//!
//! Wired up in [`super::super::run`] / [`super::super::run_with_config`]
//! against [`BgTrigger::Periodic`].

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;

use super::super::bg_tasks::{BgJob, BgTaskScheduler, BgTrigger};
use super::super::notifications::broadcast_to_subscribers;
use super::super::state::{SharedState, lock_state};
use crate::auth::SubscriptionUsage;
use crate::protocol::Response;

/// Default delay before the first refresh runs, in seconds.
///
/// `0` means "fire immediately on startup": the first tick races the
/// initial `GetSubscriptionUsage` from any reconnecting TUI, so users
/// see a fresh value on launch instead of whatever was cached at
/// shutdown.
pub(crate) const DEFAULT_REFRESH_DELAY_SECS: u64 = 0;

/// Default interval between refresh ticks, in seconds.
///
/// 5 minutes is plenty for a status-line indicator: Anthropic's
/// `/usage` returns 5h and 7d window buckets, so finer granularity
/// just burns API budget. Once-per-minute polling triggered
/// account-scoped 429s in production (#940), so we deliberately back
/// off and let the [`USAGE_CACHE_TTL_MS`][crate::server::dispatch] in
/// `dispatch` match this interval — the bg job is the only thing
/// hitting the API in steady state, and on-demand requests from
/// reconnecting TUIs ride the cache.
pub(crate) const DEFAULT_REFRESH_INTERVAL_SECS: u64 = 300;

/// Initial backoff after the first 429 from `/usage`, in seconds.
///
/// Doubles on each consecutive 429 up to [`MAX_BACKOFF_SECS`]. Reset
/// to this value on the first non-rate-limited response (success *or*
/// non-429 failure — we only back off when Anthropic actually tells us
/// to slow down).
const INITIAL_BACKOFF_SECS: u64 = 5 * 60;

/// Cap on the exponential `/usage` backoff, in seconds. 30 minutes is
/// long enough that account-scoped rate limits clear; longer than that
/// just hides genuine outages from users.
const MAX_BACKOFF_SECS: u64 = 30 * 60;

/// Read `TAU_REFRESH_USAGE_DELAY_SECS`, falling back to
/// [`DEFAULT_REFRESH_DELAY_SECS`].
pub(crate) fn refresh_delay_secs() -> u64 {
    std::env::var("TAU_REFRESH_USAGE_DELAY_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_REFRESH_DELAY_SECS)
}

/// Read `TAU_REFRESH_USAGE_INTERVAL_SECS`, falling back to
/// [`DEFAULT_REFRESH_INTERVAL_SECS`].
pub(crate) fn refresh_interval_secs() -> u64 {
    std::env::var("TAU_REFRESH_USAGE_INTERVAL_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_REFRESH_INTERVAL_SECS)
}

/// Pluggable network call.  Production uses the real Anthropic
/// endpoint; tests substitute a closure that returns canned data
/// without touching the wire.
type Fetcher = Arc<dyn Fn(String) -> crate::Result<SubscriptionUsage> + Send + Sync>;

/// Source of monotonic-ish wall-clock readings, in ms. Production uses
/// [`crate::types::timestamp_ms`]; tests inject a counter so backoff
/// boundaries fire deterministically without `sleep`.
type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

/// Mutable per-job state guarding the 429 backoff window. Wrapped in
/// `Mutex` because [`BgJob::run`] takes `&self` (the trait fans out
/// across periodic ticks) and we need to mutate the deadline + current
/// backoff width across calls.
#[derive(Debug, Default)]
struct BackoffState {
    /// Earliest wall-clock time (ms) at which the next fetch may run.
    /// `None` means "no backoff active".
    next_attempt_at_ms: Option<u64>,
    /// Current backoff width in seconds. Doubles on each consecutive
    /// 429, capped at [`MAX_BACKOFF_SECS`]. Reset to
    /// [`INITIAL_BACKOFF_SECS`] on the first non-429 response.
    current_secs: u64,
}

/// `BgJob` that refreshes the cached subscription usage and pushes the
/// fresh value to all connected TUIs.
pub(crate) struct RefreshSubscriptionUsageJob {
    fetcher: Fetcher,
    clock: Clock,
    backoff: Mutex<BackoffState>,
}

impl RefreshSubscriptionUsageJob {
    /// Construct the production job, calling
    /// [`crate::auth::fetch_subscription_usage`] on each tick.
    pub(crate) fn new() -> Self {
        Self {
            fetcher: Arc::new(|token: String| crate::auth::fetch_subscription_usage(&token)),
            clock: Arc::new(crate::types::timestamp_ms),
            backoff: Mutex::new(BackoffState::default()),
        }
    }

    /// Construct a job with a custom fetcher.  Test-only — allows
    /// asserting the cache + broadcast plumbing without making a real
    /// HTTPS call.
    #[cfg(test)]
    pub(crate) fn with_fetcher(fetcher: Fetcher) -> Self {
        Self {
            fetcher,
            clock: Arc::new(crate::types::timestamp_ms),
            backoff: Mutex::new(BackoffState::default()),
        }
    }

    /// Construct a job with a custom fetcher *and* a custom clock.
    /// Test-only — lets backoff tests advance "time" deterministically.
    #[cfg(test)]
    pub(crate) fn with_fetcher_and_clock(fetcher: Fetcher, clock: Clock) -> Self {
        Self {
            fetcher,
            clock,
            backoff: Mutex::new(BackoffState::default()),
        }
    }

    /// Compute the next backoff window in seconds and update internal
    /// state. `retry_after` (from the `Retry-After` header) wins when
    /// present — servers know better than our heuristic.
    fn record_rate_limit(&self, now_ms: u64, retry_after: Option<u64>) {
        let mut st = self.backoff.lock().expect("backoff mutex poisoned");
        // Grow exponentially from the current width. First 429 starts
        // at INITIAL_BACKOFF_SECS; subsequent 429s double up to
        // MAX_BACKOFF_SECS.
        let next_secs = if st.current_secs == 0 {
            INITIAL_BACKOFF_SECS
        } else {
            st.current_secs.saturating_mul(2).min(MAX_BACKOFF_SECS)
        };
        // Honor Retry-After when set — but don't let it shrink the
        // window below our exponential default; account-scoped limits
        // sometimes return a deceptively small value.
        let effective_secs = retry_after.unwrap_or(0).max(next_secs);
        st.current_secs = next_secs;
        st.next_attempt_at_ms = Some(now_ms.saturating_add(effective_secs.saturating_mul(1000)));
        tracing::warn!(
            backoff_secs = effective_secs,
            retry_after = ?retry_after,
            "refresh_subscription_usage: 429 from /usage; backing off"
        );
    }

    /// Clear any active backoff. Called after a successful fetch *or*
    /// after a non-429 failure (we only back off when Anthropic
    /// explicitly rate-limits us; transient 5xxs shouldn't slow our
    /// next attempt).
    fn clear_backoff(&self) {
        let mut st = self.backoff.lock().expect("backoff mutex poisoned");
        st.next_attempt_at_ms = None;
        st.current_secs = 0;
    }

    /// Returns `Some(remaining_ms)` if a backoff window is currently
    /// in effect, else `None`. Used by `run` to short-circuit ticks
    /// that fall inside the window.
    fn backoff_remaining_ms(&self, now_ms: u64) -> Option<u64> {
        let st = self.backoff.lock().expect("backoff mutex poisoned");
        st.next_attempt_at_ms
            .filter(|deadline| *deadline > now_ms)
            .map(|deadline| deadline - now_ms)
    }
}

#[async_trait]
impl BgJob for RefreshSubscriptionUsageJob {
    fn name(&self) -> &'static str {
        "refresh_subscription_usage"
    }

    async fn run(&self, state: &SharedState) {
        // 0. Respect any active 429 backoff. We don't even look up the
        //    token while in backoff: the rate limit is account-scoped,
        //    so a token rotation wouldn't help and we'd just leak more
        //    log noise.
        let now_ms = (self.clock)();
        if let Some(remaining) = self.backoff_remaining_ms(now_ms) {
            tracing::debug!(
                remaining_ms = remaining,
                "refresh_subscription_usage: in backoff window; skipping tick"
            );
            return;
        }

        // 1. Look up the Anthropic OAuth token.  Skip silently when
        //    none is configured — installs without an Anthropic
        //    subscription should pay no network cost here.
        let token = {
            let st = lock_state(state);
            match st.auth.get_api_key("anthropic") {
                Ok(Some(tok)) if crate::auth::is_oauth_token(&tok) => tok,
                Ok(_) => {
                    tracing::trace!(
                        "refresh_subscription_usage: no Anthropic OAuth token; skipping tick"
                    );
                    return;
                }
                Err(e) => {
                    tracing::debug!(
                        error = %e,
                        "refresh_subscription_usage: get_api_key failed; skipping tick"
                    );
                    return;
                }
            }
        };

        // 2. Fetch outside the lock.  In production this is a blocking
        //    `ureq::get` so we offload onto `smol::unblock`; tests
        //    install a synchronous closure that returns immediately.
        let fetcher = self.fetcher.clone();
        let result = smol::unblock(move || (fetcher)(token)).await;
        let usage = match result {
            Ok(u) => {
                // Success clears any prior backoff so the next tick
                // resumes the normal cadence.
                self.clear_backoff();
                u
            }
            Err(crate::Error::HttpStatus {
                status: 429,
                retry_after,
                ..
            }) => {
                let now_ms = (self.clock)();
                self.record_rate_limit(now_ms, retry_after);
                return;
            }
            Err(e) => {
                // Non-rate-limit failure (5xx, transport, parse). Don't
                // back off — the next tick should retry promptly — but
                // also don't reset an existing 429 backoff: a 502
                // mid-window doesn't mean Anthropic stopped throttling.
                tracing::warn!(
                    error = %e,
                    "refresh_subscription_usage: fetch failed; leaving cache untouched"
                );
                return;
            }
        };

        // 3. Update the cache and snapshot the subscriber session ids
        //    in one critical section, then broadcast outside the lock.
        //    `broadcast_to_subscribers` re-acquires the lock per call,
        //    so holding it across the loop would deadlock.
        let session_ids: Vec<String> = {
            let mut st = lock_state(state);
            st.usage_cache = Some((usage.clone(), crate::types::timestamp_ms()));
            st.subscribers.keys().cloned().collect()
        };

        let resp = Response::SubscriptionUsage {
            usage: usage.clone(),
        };
        for sid in &session_ids {
            broadcast_to_subscribers(state, sid, &resp);
        }

        tracing::debug!(
            sessions = session_ids.len(),
            "refresh_subscription_usage: cache refreshed and broadcast"
        );
    }
}

/// Register the periodic refresher against `bg`.  No-ops on
/// installations without an Anthropic OAuth token (the job itself
/// short-circuits each tick); registering unconditionally keeps the
/// setup code simple and lets users add an OAuth login at runtime
/// without restarting the server.
pub(crate) async fn register_all(bg: &Arc<BgTaskScheduler>) {
    bg.register(
        BgTrigger::Periodic {
            delay: Duration::from_secs(refresh_delay_secs()),
            interval: Duration::from_secs(refresh_interval_secs()),
        },
        Arc::new(RefreshSubscriptionUsageJob::new()),
    )
    .await;
}

#[cfg(test)]
mod tests {
    use super::super::super::ShutdownHandle;
    use super::super::super::state::State;
    use super::*;
    use crate::auth::{AuthCredential, AuthStorage, OAuthCredentials, UsageBucket};
    use crate::db::Db;
    use crate::provider::ProviderRegistry;
    use crate::types::{Model, ModelCost, ThinkingStyle};
    use std::collections::{HashMap, HashSet};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

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

    /// Build a `State` whose `AuthStorage` lives in a tempdir so tests
    /// never touch the user's real `~/.config/tau/auth.json`.
    fn mk_state_with_scheduler() -> (
        SharedState,
        Arc<BgTaskScheduler>,
        ShutdownHandle,
        tempfile::TempDir,
    ) {
        let db = Db::open_memory().expect("open memory db");
        let auth_dir = tempfile::tempdir().expect("tempdir for auth storage");
        let auth = AuthStorage::new(auth_dir.path().join("auth.json"));
        let state: SharedState = Arc::new(Mutex::new(State {
            db,
            registry: ProviderRegistry::new(),
            auth,
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
        (state, sched, shutdown, auth_dir)
    }

    /// Insert an OAuth credential for `anthropic` into `state.auth`.
    /// The bearer token starts with `sk-ant-oat` so `is_oauth_token`
    /// matches.
    fn install_anthropic_oauth(state: &SharedState) {
        let st = lock_state(state);
        st.auth
            .set(
                "anthropic",
                AuthCredential::Oauth(OAuthCredentials {
                    refresh: "refresh-stub".into(),
                    access: "sk-ant-oat-test-token".into(),
                    // 1h in the future so `get_api_key` returns the
                    // stored token without trying to refresh it.
                    expires: crate::types::timestamp_ms() + 60 * 60 * 1000,
                }),
            )
            .expect("install anthropic oauth credential");
    }

    fn mk_usage(value: f64) -> SubscriptionUsage {
        SubscriptionUsage {
            five_hour: Some(UsageBucket {
                utilization: Some(value),
                resets_at: Some("2026-01-01T00:00:00Z".into()),
            }),
            seven_day: None,
            seven_day_sonnet: None,
            seven_day_opus: None,
            extra_usage: None,
        }
    }

    /// Drain everything currently queued on `rx` without blocking.
    fn drain<T>(rx: &smol::channel::Receiver<T>) -> Vec<T> {
        let mut out = Vec::new();
        while let Ok(v) = rx.try_recv() {
            out.push(v);
        }
        out
    }

    /// With no Anthropic OAuth token configured the job must be a
    /// no-op: no fetch, no cache write, no broadcast.  This is the
    /// "API-key-only / OpenAI-only / fresh install" common case.
    #[test]
    fn no_op_without_anthropic_oauth_token() {
        smol::block_on(async {
            let (state, _sched, _shutdown, _auth_dir) = mk_state_with_scheduler();

            // Register a subscriber so we can prove no broadcast goes out.
            let (tx, rx) = smol::channel::bounded::<Response>(8);
            lock_state(&state)
                .subscribers
                .entry("s1".into())
                .or_default()
                .push(tx);

            let fetch_calls = Arc::new(AtomicUsize::new(0));
            let fetch_calls_clone = fetch_calls.clone();
            let job = RefreshSubscriptionUsageJob::with_fetcher(Arc::new(move |_token| {
                fetch_calls_clone.fetch_add(1, Ordering::SeqCst);
                Ok(mk_usage(1.0))
            }));

            job.run(&state).await;

            assert_eq!(
                fetch_calls.load(Ordering::SeqCst),
                0,
                "fetcher must not be invoked without an OAuth token"
            );
            let st = lock_state(&state);
            assert!(
                st.usage_cache.is_none(),
                "cache must remain unset on no-op tick"
            );
            drop(st);
            assert!(
                drain(&rx).is_empty(),
                "no broadcast should be sent on no-op tick"
            );
        });
    }

    /// On a successful fetch the job (a) updates `usage_cache` and
    /// (b) sends a `SubscriptionUsage` broadcast to every session id
    /// in `subscribers` — even sessions that have nothing to do with
    /// Anthropic, since the cache is global today.
    #[test]
    fn updates_cache_and_broadcasts_to_all_subscribers() {
        smol::block_on(async {
            let (state, _sched, _shutdown, _auth_dir) = mk_state_with_scheduler();
            install_anthropic_oauth(&state);

            // Two sessions, each with one subscriber.  A third session
            // with two subscribers covers the multi-tx case.
            let (tx_a, rx_a) = smol::channel::bounded::<Response>(8);
            let (tx_b, rx_b) = smol::channel::bounded::<Response>(8);
            let (tx_c1, rx_c1) = smol::channel::bounded::<Response>(8);
            let (tx_c2, rx_c2) = smol::channel::bounded::<Response>(8);
            {
                let mut st = lock_state(&state);
                st.subscribers.entry("a".into()).or_default().push(tx_a);
                st.subscribers.entry("b".into()).or_default().push(tx_b);
                let c = st.subscribers.entry("c".into()).or_default();
                c.push(tx_c1);
                c.push(tx_c2);
            }

            let fetch_calls = Arc::new(AtomicUsize::new(0));
            let fetch_calls_clone = fetch_calls.clone();
            let job = RefreshSubscriptionUsageJob::with_fetcher(Arc::new(move |_token| {
                fetch_calls_clone.fetch_add(1, Ordering::SeqCst);
                Ok(mk_usage(42.0))
            }));

            job.run(&state).await;

            assert_eq!(
                fetch_calls.load(Ordering::SeqCst),
                1,
                "fetcher should be invoked exactly once per tick"
            );

            // Cache is populated.
            {
                let st = lock_state(&state);
                let (cached, _ts) = st.usage_cache.as_ref().expect("cache populated");
                assert_eq!(
                    cached.five_hour.as_ref().and_then(|b| b.utilization),
                    Some(42.0)
                );
            }

            // Each subscriber receives exactly one SubscriptionUsage
            // broadcast.
            for (label, rx) in [("a", &rx_a), ("b", &rx_b), ("c1", &rx_c1), ("c2", &rx_c2)] {
                let msgs = drain(rx);
                assert_eq!(msgs.len(), 1, "subscriber {label} should receive one msg");
                match &msgs[0] {
                    Response::SubscriptionUsage { usage } => {
                        assert_eq!(
                            usage.five_hour.as_ref().and_then(|b| b.utilization),
                            Some(42.0),
                            "subscriber {label} got wrong payload"
                        );
                    }
                    other => panic!("subscriber {label} got unexpected response: {other:?}"),
                }
            }
        });
    }

    /// On a failed fetch the cache must be left untouched and no
    /// broadcast sent.  Avoids wiping good data on a transient 5xx.
    #[test]
    fn fetch_failure_leaves_cache_untouched() {
        smol::block_on(async {
            let (state, _sched, _shutdown, _auth_dir) = mk_state_with_scheduler();
            install_anthropic_oauth(&state);

            // Pre-populate the cache with a known value.
            let prior = mk_usage(7.0);
            lock_state(&state).usage_cache = Some((prior.clone(), 1234));

            let (tx, rx) = smol::channel::bounded::<Response>(8);
            lock_state(&state)
                .subscribers
                .entry("s1".into())
                .or_default()
                .push(tx);

            let job = RefreshSubscriptionUsageJob::with_fetcher(Arc::new(|_token| {
                Err(crate::Error::Http("simulated 502".into()))
            }));

            job.run(&state).await;

            // Cache untouched.
            {
                let st = lock_state(&state);
                let (cached, ts) = st.usage_cache.as_ref().expect("cache still populated");
                assert_eq!(
                    cached.five_hour.as_ref().and_then(|b| b.utilization),
                    Some(7.0),
                    "cache value must not be overwritten on fetch failure"
                );
                assert_eq!(*ts, 1234, "cache timestamp must not be bumped");
            }

            assert!(
                drain(&rx).is_empty(),
                "no broadcast should be sent on fetch failure"
            );
        });
    }

    /// Helper: a `Clock` whose value can be advanced from outside.
    fn fake_clock() -> (Clock, Arc<AtomicU64>) {
        let now = Arc::new(AtomicU64::new(1_000_000));
        let now_clone = now.clone();
        let clock: Clock = Arc::new(move || now_clone.load(Ordering::SeqCst));
        (clock, now)
    }

    /// 429 from /usage parks the job in a backoff window: subsequent
    /// ticks inside the window must NOT call the fetcher. Once the
    /// window expires the next tick fetches normally and a successful
    /// response resets backoff state.
    #[test]
    fn rate_limit_triggers_backoff_then_recovers() {
        smol::block_on(async {
            let (state, _sched, _shutdown, _auth_dir) = mk_state_with_scheduler();
            install_anthropic_oauth(&state);

            // Fetcher returns 429 the first call, then succeeds.
            let calls = Arc::new(AtomicUsize::new(0));
            let calls_clone = calls.clone();
            let fetcher: Fetcher = Arc::new(move |_token| {
                let n = calls_clone.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    Err(crate::Error::HttpStatus {
                        status: 429,
                        message: "rate limited".into(),
                        retry_after: None,
                    })
                } else {
                    Ok(mk_usage(99.0))
                }
            });

            let (clock, now) = fake_clock();
            let job = RefreshSubscriptionUsageJob::with_fetcher_and_clock(fetcher, clock);

            // Tick 1: fetcher invoked, returns 429, backoff installed.
            job.run(&state).await;
            assert_eq!(calls.load(Ordering::SeqCst), 1, "first tick fetches");
            assert!(
                lock_state(&state).usage_cache.is_none(),
                "cache must be empty after only-failed fetch"
            );

            // Tick 2: still inside the 5-minute window — fetcher must
            // not be invoked.
            now.fetch_add(60 * 1000, Ordering::SeqCst); // +60s
            job.run(&state).await;
            assert_eq!(
                calls.load(Ordering::SeqCst),
                1,
                "tick inside backoff window must not refetch"
            );

            // Tick 3: also inside the window (4 minutes in).
            now.fetch_add(3 * 60 * 1000, Ordering::SeqCst); // +3 min more
            job.run(&state).await;
            assert_eq!(
                calls.load(Ordering::SeqCst),
                1,
                "tick still inside backoff window must not refetch"
            );

            // Tick 4: past the 5-minute window — fetcher invoked,
            // succeeds, cache populated.
            now.fetch_add(2 * 60 * 1000 + 1, Ordering::SeqCst); // push past 5 min
            job.run(&state).await;
            assert_eq!(
                calls.load(Ordering::SeqCst),
                2,
                "tick past backoff window must refetch"
            );
            assert!(
                lock_state(&state).usage_cache.is_some(),
                "successful fetch populates cache"
            );

            // Tick 5: backoff was cleared on success, so the very next
            // tick fetches without waiting.
            job.run(&state).await;
            assert_eq!(
                calls.load(Ordering::SeqCst),
                3,
                "backoff cleared on success; immediate refetch allowed"
            );
        });
    }

    /// Consecutive 429s grow the backoff window exponentially up to
    /// the cap. We don't assert exact deadlines (that would test the
    /// constants) but we do assert the second window is strictly
    /// longer than the first.
    #[test]
    fn consecutive_rate_limits_extend_backoff() {
        smol::block_on(async {
            let (state, _sched, _shutdown, _auth_dir) = mk_state_with_scheduler();
            install_anthropic_oauth(&state);

            let fetcher: Fetcher = Arc::new(|_token| {
                Err(crate::Error::HttpStatus {
                    status: 429,
                    message: "rate limited".into(),
                    retry_after: None,
                })
            });
            let (clock, now) = fake_clock();
            let job = RefreshSubscriptionUsageJob::with_fetcher_and_clock(fetcher, clock);

            // First 429: install initial window.
            job.run(&state).await;
            let first_window = job
                .backoff
                .lock()
                .expect("backoff mutex")
                .next_attempt_at_ms
                .expect("deadline set")
                - now.load(Ordering::SeqCst);

            // Step past the first window so the next tick can fetch.
            now.fetch_add(first_window + 1, Ordering::SeqCst);
            job.run(&state).await;
            let second_deadline = job
                .backoff
                .lock()
                .expect("backoff mutex")
                .next_attempt_at_ms
                .expect("deadline set after second 429");
            let second_window = second_deadline - now.load(Ordering::SeqCst);

            assert!(
                second_window > first_window,
                "second backoff window ({second_window}ms) must exceed first ({first_window}ms)"
            );
        });
    }

    /// `Retry-After` from the server overrides the default exponential
    /// width when it's larger. (We deliberately don't shrink below the
    /// exponential default — see `record_rate_limit`.)
    #[test]
    fn retry_after_extends_backoff_window() {
        smol::block_on(async {
            let (state, _sched, _shutdown, _auth_dir) = mk_state_with_scheduler();
            install_anthropic_oauth(&state);

            // Server says "retry in 1 hour" — way longer than our
            // initial 5-minute default, so the deadline must reflect
            // the larger value.
            let fetcher: Fetcher = Arc::new(|_token| {
                Err(crate::Error::HttpStatus {
                    status: 429,
                    message: "rate limited".into(),
                    retry_after: Some(3600),
                })
            });
            let (clock, now) = fake_clock();
            let job = RefreshSubscriptionUsageJob::with_fetcher_and_clock(fetcher, clock);

            job.run(&state).await;

            let deadline = job
                .backoff
                .lock()
                .expect("backoff mutex")
                .next_attempt_at_ms
                .expect("deadline set");
            let window_ms = deadline - now.load(Ordering::SeqCst);
            assert!(
                window_ms >= 3600 * 1000,
                "Retry-After=3600s must produce at least 3.6e6 ms; got {window_ms}"
            );
        });
    }

    /// Non-429 errors (5xx, parse, transport) must NOT install a
    /// backoff window: those are transient and the next tick should
    /// retry promptly.
    #[test]
    fn non_rate_limit_errors_do_not_back_off() {
        smol::block_on(async {
            let (state, _sched, _shutdown, _auth_dir) = mk_state_with_scheduler();
            install_anthropic_oauth(&state);

            let calls = Arc::new(AtomicUsize::new(0));
            let calls_clone = calls.clone();
            let fetcher: Fetcher = Arc::new(move |_token| {
                calls_clone.fetch_add(1, Ordering::SeqCst);
                Err(crate::Error::HttpStatus {
                    status: 502,
                    message: "bad gateway".into(),
                    retry_after: None,
                })
            });
            let (clock, _now) = fake_clock();
            let job = RefreshSubscriptionUsageJob::with_fetcher_and_clock(fetcher, clock);

            job.run(&state).await;
            job.run(&state).await;
            job.run(&state).await;

            assert_eq!(
                calls.load(Ordering::SeqCst),
                3,
                "5xx errors must not gate subsequent ticks"
            );
            assert!(
                job.backoff
                    .lock()
                    .expect("backoff mutex")
                    .next_attempt_at_ms
                    .is_none(),
                "5xx errors must not install a backoff deadline"
            );
        });
    }
}
