use std::collections::{HashMap, HashSet};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use crate::auth::AuthStorage;
use crate::config;
use crate::db::Db;
use crate::protocol::Response;
use crate::provider::ProviderRegistry;
use crate::types::Model;

pub(super) struct State {
    pub(super) db: Db,
    pub(super) registry: ProviderRegistry,
    pub(super) auth: AuthStorage,
    pub(super) config: config::Config,
    /// Global model aliases loaded from `~/.config/tau/models.toml`
    /// (with a legacy fallback to `providers.toml [aliases]`).  See
    /// [`crate::models_config::load_global_aliases`].
    pub(super) global_aliases: HashMap<String, String>,
    pub(super) default_model: Model,
    /// All known models (for /model listing).
    pub(super) all_models: Vec<Model>,
    /// Cached subscription usage (value, fetched_at_ms).
    pub(super) usage_cache: Option<(crate::auth::SubscriptionUsage, u64)>,
    /// Per-session cancel flags.  Set by CancelChat, cleared on Chat start.
    pub(super) cancel_flags: HashMap<String, Arc<AtomicBool>>,
    /// Per-session flag indicating queued messages are pending.
    pub(super) has_queued: HashMap<String, Arc<AtomicBool>>,
    /// Per-session broadcast subscribers.
    /// Other clients watching a session receive streamed responses.
    pub(super) subscribers: HashMap<String, Vec<smol::channel::Sender<Response>>>,
    /// Current agent phase per session, for new subscribers.
    pub(super) phases: HashMap<String, crate::types::AgentPhase>,
    /// Sessions with an actively running agent turn in this process.
    /// Inserted at the start of each Chat/resume turn, removed on completion.
    /// This is the authoritative "is something happening right now" signal.
    pub(super) live_sessions: HashSet<String>,
    /// Sessions currently being waited on by WaitSessions/WaitAnySessions.
    /// Maps child_session_id -> parent_session_id. Used to suppress redundant
    /// completion notifications when parent is actively joining.
    pub(super) waited_sessions: HashSet<String>,
    /// Waiters notified when any session's agent turn completes.
    /// Each entry is a one-shot-ish sender; closed/full senders are pruned on notify.
    pub(super) session_done_waiters: Vec<smol::channel::Sender<()>>,
    /// Pending reply waiters for `await_reply` messages.
    /// Key is msg_id, value is a oneshot sender for the reply content.
    pub(super) reply_waiters: HashMap<String, smol::channel::Sender<String>>,
    /// Monotonic counter for generating unique msg_ids.
    pub(super) next_msg_id: u64,
}

pub(super) type SharedState = Arc<Mutex<State>>;

pub(super) fn lock_state(state: &SharedState) -> std::sync::MutexGuard<'_, State> {
    state.lock().unwrap_or_else(|e| {
        tracing::warn!("recovering from poisoned mutex");
        e.into_inner()
    })
}

/// Per-session async locks to serialize Chat requests.
/// The outer std::Mutex is only held briefly to get/create a lock.
/// The inner smol::lock::Mutex is held across the entire agent turn.
pub(super) type SessionLocks = Arc<Mutex<HashMap<String, Arc<smol::lock::Mutex<()>>>>>;

/// Get or create an async lock for a session.
pub(super) fn session_lock(locks: &SessionLocks, session_id: &str) -> Arc<smol::lock::Mutex<()>> {
    let mut map = locks.lock().expect("session locks mutex poisoned");
    map.entry(session_id.to_string())
        .or_insert_with(|| Arc::new(smol::lock::Mutex::new(())))
        .clone()
}

/// Called once at startup.  Non-idle persisted phases are **not** restored
/// into `state.phases` because no chat loops are running after a restart.
/// Instead, a diagnostic warning is logged listing sessions whose persisted
/// phase was non-idle — indicating the previous server instance may have
/// exited uncleanly while those sessions were mid-turn.
pub(super) fn log_stale_phases_at_startup(state: &SharedState) {
    let st = lock_state(state);
    let sessions = match st.db.list_sessions(false) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(%e, "failed to load sessions for phase check");
            return;
        }
    };
    let stale: Vec<_> = sessions
        .iter()
        .filter(|s| s.last_phase.as_deref().is_some_and(|p| p != "idle"))
        .collect();
    if !stale.is_empty() {
        let ids: Vec<_> = stale
            .iter()
            .map(|s| format!("{} ({})", s.id, s.last_phase.as_deref().unwrap_or("?")))
            .collect();
        tracing::warn!(
            count = stale.len(),
            sessions = %ids.join(", "),
            "sessions had non-idle persisted phases at startup (possibly unclean previous shutdown)"
        );
    }
}
