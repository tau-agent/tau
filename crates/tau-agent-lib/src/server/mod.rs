//! Unix socket server — manages sessions and streams LLM responses.

mod agent_runner;
mod dispatch;
mod notifications;
mod post_idle;
mod registry;
mod state;
pub(crate) mod task_handlers;
mod tool_dispatch;

use std::collections::{HashMap, HashSet};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use smol::Async;

use state::{SessionLocks, SharedState, State, lock_state};

use crate::config;
use crate::db::Db;
use crate::provider::ProviderRegistry;
use crate::types::*;

/// Default drain window (seconds) for in-flight agent loops when a
/// graceful shutdown is requested. Can be overridden by the
/// `TAU_SHUTDOWN_DRAIN_SECS` environment variable.
///
/// 180s is the Phase 1 default (up from 60s) so that long-running
/// operations like `just test` and multi-minute LLM turns have a
/// realistic chance of finishing before the server bails.
pub(crate) const DEFAULT_SHUTDOWN_DRAIN_SECS: u64 = 180;

/// Read the configured drain window from `TAU_SHUTDOWN_DRAIN_SECS`,
/// falling back to [`DEFAULT_SHUTDOWN_DRAIN_SECS`]. A value of `0`
/// disables the drain (immediate exit on shutdown) and is respected.
pub(crate) fn shutdown_drain_secs() -> u64 {
    match std::env::var("TAU_SHUTDOWN_DRAIN_SECS") {
        Ok(v) => v.parse().unwrap_or(DEFAULT_SHUTDOWN_DRAIN_SECS),
        Err(_) => DEFAULT_SHUTDOWN_DRAIN_SECS,
    }
}

/// A sender that can deliver shutdown notifications to a connected client.
type ClientNotifier = smol::channel::Sender<crate::protocol::Response>;

/// Shutdown coordination shared across all tasks.
#[derive(Clone)]
struct ShutdownHandle {
    /// Set to true when shutdown is requested.
    flag: Arc<AtomicBool>,
    /// Whether this is a restart (clients should reconnect).
    restart: Arc<AtomicBool>,
    /// Number of in-flight agent loops.
    in_flight: Arc<AtomicUsize>,
    /// Registered client notifiers for broadcast.
    clients: Arc<Mutex<Vec<ClientNotifier>>>,
}

impl ShutdownHandle {
    fn new() -> Self {
        Self {
            flag: Arc::new(AtomicBool::new(false)),
            restart: Arc::new(AtomicBool::new(false)),
            in_flight: Arc::new(AtomicUsize::new(0)),
            clients: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn is_shutting_down(&self) -> bool {
        self.flag.load(Ordering::Relaxed)
    }

    fn request_shutdown(&self, restart: bool) {
        self.restart.store(restart, Ordering::Relaxed);
        self.flag.store(true, Ordering::Relaxed);
        // Broadcast to all connected clients
        let clients = self.clients.lock().expect("clients mutex poisoned");
        let msg = crate::protocol::Response::ServerShutdown { restart };
        for tx in clients.iter() {
            if tx.try_send(msg.clone()).is_err() {
                tracing::warn!("failed to send shutdown notification to client");
            }
        }
    }

    fn register_client(&self) -> smol::channel::Receiver<crate::protocol::Response> {
        let (tx, rx) = smol::channel::bounded(1);
        self.clients
            .lock()
            .expect("clients mutex poisoned")
            .push(tx);
        rx
    }

    fn enter(&self) {
        self.in_flight.fetch_add(1, Ordering::Relaxed);
    }

    fn leave(&self) {
        self.in_flight.fetch_sub(1, Ordering::Relaxed);
    }

    fn active_count(&self) -> usize {
        self.in_flight.load(Ordering::Relaxed)
    }
}

/// Optional test overrides that bypass the plugin executor.
#[derive(Default)]
pub struct TestOverrides {
    /// Factory for mock tool executors (per agent turn).
    pub tool_executor_factory:
        Option<Arc<dyn Fn() -> Box<dyn crate::worker::ToolExecutor> + Send + Sync>>,
    /// Tool schemas to pass to the agent loop when using a mock executor.
    pub mock_tools: Vec<Tool>,
}

type SharedTestOverrides = Arc<TestOverrides>;

/// Configuration for a test server instance.
pub struct TestServerConfig {
    pub registry: ProviderRegistry,
    pub models: Vec<Model>,
    pub socket_path: PathBuf,
    pub db_path: PathBuf,
    /// Optional: factory for mock tool executors (per agent turn).
    pub tool_executor_factory:
        Option<Arc<dyn Fn() -> Box<dyn crate::worker::ToolExecutor> + Send + Sync>>,
    /// Optional: tool schemas for mock executor.
    pub mock_tools: Vec<Tool>,
    /// Optional: plugins configuration (for testing global plugins).
    pub plugins_config: Option<crate::plugin::PluginsConfig>,
    /// Optional: global model aliases (mirrors what would normally be
    /// loaded from `~/.config/tau/models.toml` by
    /// [`crate::models_config::load_global_aliases`]).
    pub aliases: HashMap<String, String>,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Returns the default socket path.
pub fn socket_path() -> PathBuf {
    crate::paths::socket_path()
}

/// Returns the PID file path next to the socket.
pub fn pid_path() -> PathBuf {
    crate::paths::pid_path()
}

/// Check if a server is already running by trying to connect.
pub fn is_running() -> bool {
    crate::paths::is_running()
}

fn prepare_socket_dir(sock: &Path) -> crate::Result<()> {
    if let Some(parent) = sock.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| crate::Error::Io(format!("mkdir {}: {}", parent.display(), e)))?;
    }
    Ok(())
}

/// Bundled outputs of reading the runtime-mutable config files on disk.
///
/// Used by both server startup and the `ReloadConfig` request handler so
/// they stay in lock-step. Performs the disk I/O (which can fail for
/// `providers.toml`); callers are expected to merge this into `State`.
///
/// `default_model` is always `all_models[0]`; callers that want to
/// preserve a previously-selected default should pick a matching entry
/// from `all_models` and fall back to `default_model` when that model is
/// gone.
pub(crate) struct RuntimeConfig {
    pub config: crate::config::Config,
    pub all_models: Vec<Model>,
    pub global_aliases: HashMap<String, String>,
    pub default_model: Model,
}

/// Read `providers.toml` + global `models.toml` and resolve the model
/// table. Called from [`run`] at startup and from the `ReloadConfig`
/// request handler.
pub(crate) fn load_runtime_config() -> crate::Result<RuntimeConfig> {
    let cfg = config::load_config()?;
    let all_models = config::resolve_models(&cfg);
    let global_aliases = crate::models_config::load_global_aliases();
    let default_model = all_models
        .first()
        .cloned()
        .ok_or_else(|| crate::Error::Io("no models available".into()))?;
    Ok(RuntimeConfig {
        config: cfg,
        all_models,
        global_aliases,
        default_model,
    })
}

/// Emit an `InfoMessage` to a session informing the user / agent that
/// this session was restarted. Used by the startup auto-resume scan so
/// the transcript stays honest about the gap.
fn notify_resumed_session(state: &SharedState, session_id: &str) {
    notifications::queue_info_to_session(state, session_id, "Resumed after server restart.");
}

/// Detect sessions that were interrupted by the previous server
/// lifetime and re-dispatch their agent loops in the background. Also
/// picks up sessions that have pending queued messages. Called from
/// both the production `run()` path and the `run_with_config()` test
/// path so the two code paths stay in sync.
fn spawn_auto_resume_tasks(
    state: &SharedState,
    plugins: &Arc<Mutex<crate::plugin::PluginManager>>,
    shutdown: &ShutdownHandle,
    session_locks: &SessionLocks,
    throttle: &crate::throttle::ProviderThrottle,
    test_overrides: &SharedTestOverrides,
) {
    use agent_runner::resume_child_session;

    let resume_ids = {
        let st = lock_state(state);
        st.db.sessions_needing_resume().unwrap_or_else(|e| {
            tracing::warn!(%e, "failed to query sessions needing resume");
            Vec::new()
        })
    };
    let mut resuming: std::collections::HashSet<String> = resume_ids.iter().cloned().collect();
    for sid in resume_ids {
        tracing::info!(session_id = %sid, "auto-resuming session after restart");
        notify_resumed_session(state, &sid);
        let s = state.clone();
        let p = plugins.clone();
        let sh = shutdown.clone();
        let sl = session_locks.clone();
        let th = throttle.clone();
        let ov = test_overrides.clone();
        smol::spawn(async move {
            if let Err(e) = resume_child_session(s, p, sh, sl, th, sid.clone(), ov).await {
                tracing::warn!(session_id = %sid, %e, "auto-resume error");
            }
        })
        .detach();
    }

    // Also resume sessions that have pending queued messages but aren't
    // already being auto-resumed.
    let queued_ids = {
        let st = lock_state(state);
        st.db.sessions_with_queued_messages().unwrap_or_else(|e| {
            tracing::warn!(%e, "failed to query sessions with queued messages");
            Vec::new()
        })
    };
    for sid in queued_ids {
        if resuming.insert(sid.clone()) {
            tracing::info!(session_id = %sid, "auto-resuming session (has queued messages)");
            // Set the has_queued flag so the agent loop will drain them.
            {
                let mut st = lock_state(state);
                st.has_queued
                    .entry(sid.clone())
                    .or_insert_with(|| Arc::new(AtomicBool::new(false)))
                    .store(true, Ordering::Release);
            }
            let s = state.clone();
            let p = plugins.clone();
            let sh = shutdown.clone();
            let sl = session_locks.clone();
            let th = throttle.clone();
            let ov = test_overrides.clone();
            smol::spawn(async move {
                if let Err(e) = resume_child_session(s, p, sh, sl, th, sid.clone(), ov).await {
                    tracing::warn!(session_id = %sid, %e, "auto-resume error");
                }
            })
            .detach();
        }
    }
}

/// Run a server with custom config (for testing).
pub async fn run_with_config(config: TestServerConfig) -> crate::Result<()> {
    use crate::auth::AuthStorage;
    use registry::{
        spawn_bg_chat_receiver, spawn_global_plugin_background_tasks, spawn_idle_sweep,
    };
    use state::log_stale_phases_at_startup;

    let default_model = config
        .models
        .first()
        .cloned()
        .ok_or_else(|| crate::Error::Io("no models available".into()))?;
    let sock = &config.socket_path;

    let test_overrides: SharedTestOverrides = Arc::new(TestOverrides {
        tool_executor_factory: config.tool_executor_factory,
        mock_tools: config.mock_tools,
    });

    if let Some(parent) = sock.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    if sock.exists() {
        std::fs::remove_file(sock).ok();
    }

    let listener = Async::<UnixListener>::bind(sock)
        .map_err(|e| crate::Error::Io(format!("bind {}: {}", sock.display(), e)))?;

    let db = Db::open(&config.db_path)?;

    let mut cfg = crate::config::Config::default();
    // Add mock provider config with dummy API key so Chat requests don't fail
    cfg.providers.insert(
        "mock".into(),
        crate::config::ProviderConfig {
            api: "openai".into(),
            base_url: "http://mock".into(),
            api_key: Some("mock-key".into()),
            models: vec![],
        },
    );
    // Test-supplied global aliases (mirrors what would normally come
    // from `~/.config/tau/models.toml`).
    let global_aliases = config.aliases.clone();
    let plugins_config = config
        .plugins_config
        .unwrap_or(crate::plugin::PluginsConfig {
            no_default_worker: true,
            ..Default::default()
        });
    let mut plugins = crate::plugin::PluginManager::new(plugins_config);
    plugins.load_global_plugins("/tmp");
    let plugins: Arc<Mutex<crate::plugin::PluginManager>> = Arc::new(Mutex::new(plugins));

    let session_locks: SessionLocks = Arc::new(Mutex::new(HashMap::new()));
    let throttle = crate::throttle::ProviderThrottle::new();

    let state: SharedState = Arc::new(Mutex::new(State {
        db,
        registry: config.registry,
        auth: AuthStorage::open_default(),
        config: cfg,
        global_aliases,
        default_model,
        all_models: config.models,
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
    }));

    log_stale_phases_at_startup(&state);

    let shutdown = ShutdownHandle::new();

    let shutdown_watcher = shutdown.clone();
    let sock_clone = sock.to_path_buf();
    smol::spawn(async move {
        while !shutdown_watcher.is_shutting_down() {
            smol::Timer::after(std::time::Duration::from_millis(100)).await;
        }
        // Repeatedly self-connect so the accept loop wakes up and can
        // observe the drain counter reaching zero. We poke every 100ms
        // until in-flight work is done.
        loop {
            let _ = Async::<UnixStream>::connect(&sock_clone).await;
            if shutdown_watcher.active_count() == 0 {
                break;
            }
            smol::Timer::after(std::time::Duration::from_millis(100)).await;
        }
    })
    .detach();

    // Spawn idle sweep task for session plugins.
    spawn_idle_sweep(plugins.clone(), state.clone(), shutdown.clone());

    // Spawn background reader/writer tasks for global plugins.
    let bg_chat_spawn_tx = spawn_bg_chat_receiver(
        state.clone(),
        plugins.clone(),
        shutdown.clone(),
        session_locks.clone(),
        throttle.clone(),
    );
    spawn_global_plugin_background_tasks(
        &plugins,
        &state,
        &session_locks,
        &shutdown,
        &throttle,
        &bg_chat_spawn_tx,
        &test_overrides,
    );

    // Auto-resume interrupted sessions (mirrors the production `run()` path).
    spawn_auto_resume_tasks(
        &state,
        &plugins,
        &shutdown,
        &session_locks,
        &throttle,
        &test_overrides,
    );

    loop {
        let (stream, _) = listener
            .accept()
            .await
            .map_err(|e| crate::Error::Io(e.to_string()))?;

        if shutdown.is_shutting_down() {
            // Stop accepting *new* connections once the drain has
            // committed to exit — but keep the accept loop running
            // while in-flight turns are still draining so connected
            // clients can observe Response::Error { SHUTTING_DOWN }
            // for new chat attempts. We break only after the drain
            // has really ended (active_count==0) so nothing useful is
            // left to serve.
            if shutdown.active_count() == 0 {
                break;
            }
        }

        let state = state.clone();
        let plugins = plugins.clone();
        let shutdown_handle = shutdown.clone();
        let session_locks = session_locks.clone();
        let throttle = throttle.clone();
        let overrides = test_overrides.clone();
        let bg_chat = bg_chat_spawn_tx.clone();
        smol::spawn(async move {
            if let Err(e) = dispatch::handle_client(
                stream,
                state,
                plugins,
                shutdown_handle,
                session_locks,
                throttle,
                overrides,
                bg_chat,
            )
            .await
            {
                tracing::warn!(%e, "client error");
            }
        })
        .detach();
    }

    // Drain in-flight agent loops before resetting phases, mirroring
    // the production `run()` path. Tests override the window via
    // `TAU_SHUTDOWN_DRAIN_SECS` to keep the test suite fast.
    let drain_secs = shutdown_drain_secs();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(drain_secs);
    while shutdown.active_count() > 0 && std::time::Instant::now() < deadline {
        smol::Timer::after(std::time::Duration::from_millis(100)).await;
    }

    // Persist idle phase for all sessions on clean shutdown (same as production `run()`).
    {
        let st = lock_state(&state);
        if let Err(e) = st.db.reset_all_phases() {
            tracing::warn!(%e, "failed to reset phases on shutdown");
        }
    }

    Ok(())
}

/// Run the server (blocking). Call from `smol::block_on`.
pub async fn run() -> crate::Result<()> {
    use crate::auth::AuthStorage;
    use registry::{
        build_registry, spawn_bg_chat_receiver, spawn_global_plugin_background_tasks,
        spawn_idle_sweep,
    };
    use state::log_stale_phases_at_startup;

    // Initialise tracing before anything else so startup events land in the
    // log file. The returned guard must live for the entire server lifetime
    // so the non-blocking appender drains on shutdown.
    let _log_guard = crate::logging::init_tracing();
    tracing::info!(pid = std::process::id(), "tau server starting");

    let registry = build_registry();
    let RuntimeConfig {
        config: cfg,
        all_models,
        global_aliases,
        default_model,
    } = load_runtime_config()?;
    let sock = socket_path();
    prepare_socket_dir(&sock)?;

    if sock.exists() {
        std::fs::remove_file(&sock).ok();
    }

    let listener = Async::<UnixListener>::bind(&sock)
        .map_err(|e| crate::Error::Io(format!("bind {}: {}", sock.display(), e)))?;

    let pid = std::process::id();
    std::fs::write(pid_path(), pid.to_string())
        .map_err(|e| crate::Error::Io(format!("write pidfile: {}", e)))?;

    let db = Db::open_default()?;

    // Run one-time project migration if needed
    let tasks_db_path = crate::paths::data_dir().join("tasks.db");
    if let Err(e) = crate::migration::run_project_migration(&db, &tasks_db_path) {
        tracing::warn!(%e, "project migration failed; run `tau project migrate` to retry");
    }

    tracing::info!(socket = %sock.display(), "tau server listening");

    // Load plugins
    let plugins_config = crate::plugin::load_plugins_config();
    let mut plugins = crate::plugin::PluginManager::new(plugins_config);
    plugins.load_global_plugins("/tmp");
    let plugins: Arc<Mutex<crate::plugin::PluginManager>> = Arc::new(Mutex::new(plugins));

    let session_locks: SessionLocks = Arc::new(Mutex::new(HashMap::new()));
    let throttle = crate::throttle::ProviderThrottle::new();

    let state: SharedState = Arc::new(Mutex::new(State {
        db,
        registry,
        auth: AuthStorage::open_default(),
        config: cfg,
        global_aliases,
        default_model,
        all_models,
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
    }));

    log_stale_phases_at_startup(&state);

    let shutdown = ShutdownHandle::new();

    // Install signal-driven graceful shutdown.  SIGTERM (e.g. systemd
    // stop) and SIGHUP (parent shell closed) request the same drain path
    // as `tau server stop` would.  SIGINT keeps the process responsive to
    // Ctrl-C when run with `--foreground`.
    {
        let shutdown = shutdown.clone();
        if let Err(e) = crate::shutdown::install(move |sig| {
            tracing::info!(
                signal = crate::shutdown::signal_name(sig),
                "received signal, requesting shutdown",
            );
            shutdown.request_shutdown(false);
        }) {
            tracing::warn!(%e, "failed to install signal handlers");
        }
    }

    // Spawn a task that closes the listener when shutdown is requested.
    // This unblocks the accept() call below.
    let shutdown_watcher = shutdown.clone();
    let sock_clone = sock.clone();
    smol::spawn(async move {
        while !shutdown_watcher.is_shutting_down() {
            smol::Timer::after(std::time::Duration::from_millis(100)).await;
        }
        // Repeatedly self-connect so the accept loop wakes up and can
        // observe the drain counter reaching zero.
        loop {
            let _ = Async::<UnixStream>::connect(&sock_clone).await;
            if shutdown_watcher.active_count() == 0 {
                break;
            }
            smol::Timer::after(std::time::Duration::from_millis(100)).await;
        }
    })
    .detach();

    // Spawn idle sweep task for session plugins.
    spawn_idle_sweep(plugins.clone(), state.clone(), shutdown.clone());

    // Spawn background reader/writer tasks for global plugins so they can
    // send ServerRequests at any time, not just during tool calls.
    let bg_chat_spawn_tx = spawn_bg_chat_receiver(
        state.clone(),
        plugins.clone(),
        shutdown.clone(),
        session_locks.clone(),
        throttle.clone(),
    );
    spawn_global_plugin_background_tasks(
        &plugins,
        &state,
        &session_locks,
        &shutdown,
        &throttle,
        &bg_chat_spawn_tx,
        &Arc::new(TestOverrides::default()),
    );

    // Auto-resume interrupted sessions.
    //
    // Runs as a background task: connects come in on the listener above
    // without waiting on the resume scan. Each resumable session gets an
    // `InfoMessage("Resumed after server restart.")` in its transcript
    // so the history stays honest about the gap.
    spawn_auto_resume_tasks(
        &state,
        &plugins,
        &shutdown,
        &session_locks,
        &throttle,
        &Arc::new(TestOverrides::default()),
    );

    loop {
        let (stream, _) = listener
            .accept()
            .await
            .map_err(|e| crate::Error::Io(e.to_string()))?;

        if shutdown.is_shutting_down() {
            // Keep accepting connections while draining so connected
            // clients can still observe Response::Error { SHUTTING_DOWN }
            // for new chat attempts. See the matching comment in
            // `run_with_config` above.
            if shutdown.active_count() == 0 {
                break;
            }
        }

        let state = state.clone();
        let plugins = plugins.clone();
        let shutdown_handle = shutdown.clone();
        let session_locks = session_locks.clone();
        let throttle = throttle.clone();
        let no_overrides: SharedTestOverrides = Arc::new(TestOverrides::default());
        let bg_chat = bg_chat_spawn_tx.clone();
        smol::spawn(async move {
            if let Err(e) = dispatch::handle_client(
                stream,
                state,
                plugins,
                shutdown_handle,
                session_locks,
                throttle,
                no_overrides,
                bg_chat,
            )
            .await
            {
                tracing::warn!(%e, "client error");
            }
        })
        .detach();
    }

    // Wait for in-flight agent loops to finish (configurable window).
    let drain_secs = shutdown_drain_secs();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(drain_secs);
    tracing::info!(
        drain_secs,
        in_flight = shutdown.active_count(),
        "draining in-flight requests"
    );
    while shutdown.active_count() > 0 && std::time::Instant::now() < deadline {
        tracing::info!(
            in_flight = shutdown.active_count(),
            "waiting for in-flight requests to drain"
        );
        smol::Timer::after(std::time::Duration::from_secs(1)).await;
    }
    if shutdown.active_count() > 0 {
        tracing::warn!(
            in_flight = shutdown.active_count(),
            drain_secs,
            "shutdown timeout: requests still in flight, exiting anyway"
        );
    }

    // Persist idle phase for all sessions so a clean shutdown starts from
    // a clean slate.  Only crashes leave non-idle persisted phases.
    {
        let st = lock_state(&state);
        if let Err(e) = st.db.reset_all_phases() {
            tracing::warn!(%e, "failed to reset phases on shutdown");
        }
    }

    // Cleanup
    std::fs::remove_file(&sock).ok();
    std::fs::remove_file(pid_path()).ok();
    tracing::info!("tau server stopped");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Env-var manipulation races between parallel tests. Serialize the
    // drain-config tests with a single mutex so TAU_SHUTDOWN_DRAIN_SECS
    // never overlaps across threads.
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        use std::sync::{Mutex, OnceLock};
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .expect("env lock poisoned")
    }

    #[test]
    fn drain_default_is_180_seconds() {
        let _g = env_lock();
        // SAFETY: serialized via env_lock; other tests cannot observe the
        // intermediate empty state.
        unsafe {
            std::env::remove_var("TAU_SHUTDOWN_DRAIN_SECS");
        }
        assert_eq!(shutdown_drain_secs(), 180);
    }

    #[test]
    fn drain_honours_env_override() {
        let _g = env_lock();
        // SAFETY: serialized via env_lock.
        unsafe {
            std::env::set_var("TAU_SHUTDOWN_DRAIN_SECS", "5");
        }
        assert_eq!(shutdown_drain_secs(), 5);
        // SAFETY: serialized via env_lock.
        unsafe {
            std::env::remove_var("TAU_SHUTDOWN_DRAIN_SECS");
        }
    }

    #[test]
    fn drain_falls_back_on_garbage_env() {
        let _g = env_lock();
        // SAFETY: serialized via env_lock.
        unsafe {
            std::env::set_var("TAU_SHUTDOWN_DRAIN_SECS", "not-a-number");
        }
        assert_eq!(shutdown_drain_secs(), DEFAULT_SHUTDOWN_DRAIN_SECS);
        // SAFETY: serialized via env_lock.
        unsafe {
            std::env::remove_var("TAU_SHUTDOWN_DRAIN_SECS");
        }
    }

    #[test]
    fn shutdown_handle_tracks_in_flight() {
        let h = ShutdownHandle::new();
        assert_eq!(h.active_count(), 0);
        h.enter();
        h.enter();
        assert_eq!(h.active_count(), 2);
        h.leave();
        assert_eq!(h.active_count(), 1);
        assert!(!h.is_shutting_down());
        h.request_shutdown(true);
        assert!(h.is_shutting_down());
    }
}
