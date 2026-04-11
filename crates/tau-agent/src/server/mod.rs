//! Unix socket server — manages sessions and streams LLM responses.

mod agent_runner;
mod dispatch;
mod notifications;
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
                eprintln!("warning: failed to send shutdown notification to client");
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
    crate::paths::runtime_dir().join("tau.sock")
}

/// Returns the PID file path next to the socket.
pub fn pid_path() -> PathBuf {
    let mut p = socket_path();
    p.set_file_name("tau.pid");
    p
}

/// Check if a server is already running by trying to connect.
pub fn is_running() -> bool {
    std::os::unix::net::UnixStream::connect(socket_path()).is_ok()
}

fn prepare_socket_dir(sock: &Path) -> crate::Result<()> {
    if let Some(parent) = sock.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| crate::Error::Io(format!("mkdir {}: {}", parent.display(), e)))?;
    }
    Ok(())
}

/// Run a server with custom config (for testing).
pub async fn run_with_config(config: TestServerConfig) -> crate::Result<()> {
    use crate::auth::AuthStorage;
    use registry::{
        spawn_bg_chat_receiver, spawn_global_plugin_background_tasks, spawn_idle_sweep,
    };
    use state::restore_phases_from_db;

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
        waited_sessions: HashSet::new(),
        session_done_waiters: Vec::new(),
        reply_waiters: HashMap::new(),
        next_msg_id: 0,
    }));

    restore_phases_from_db(&state);

    let shutdown = ShutdownHandle::new();

    let shutdown_watcher = shutdown.clone();
    let sock_clone = sock.to_path_buf();
    smol::spawn(async move {
        while !shutdown_watcher.is_shutting_down() {
            smol::Timer::after(std::time::Duration::from_millis(100)).await;
        }
        let _ = Async::<UnixStream>::connect(&sock_clone).await;
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

    loop {
        let (stream, _) = listener
            .accept()
            .await
            .map_err(|e| crate::Error::Io(e.to_string()))?;

        if shutdown.is_shutting_down() {
            break;
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
                eprintln!("client error: {}", e);
            }
        })
        .detach();
    }

    Ok(())
}

/// Run the server (blocking). Call from `smol::block_on`.
pub async fn run() -> crate::Result<()> {
    use crate::auth::AuthStorage;
    use agent_runner::resume_child_session;
    use registry::{
        build_registry, spawn_bg_chat_receiver, spawn_global_plugin_background_tasks,
        spawn_idle_sweep,
    };
    use state::restore_phases_from_db;

    let registry = build_registry();
    let cfg = config::load_config()?;
    let all_models = config::resolve_models(&cfg);
    let global_aliases = crate::models_config::load_global_aliases();
    let default_model = all_models
        .first()
        .cloned()
        .ok_or_else(|| crate::Error::Io("no models available".into()))?;
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
    eprintln!("tau server listening on {}", sock.display());

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
        waited_sessions: HashSet::new(),
        session_done_waiters: Vec::new(),
        reply_waiters: HashMap::new(),
        next_msg_id: 0,
    }));

    restore_phases_from_db(&state);

    let shutdown = ShutdownHandle::new();

    // Spawn a task that closes the listener when shutdown is requested.
    // This unblocks the accept() call below.
    let shutdown_watcher = shutdown.clone();
    let sock_clone = sock.clone();
    smol::spawn(async move {
        while !shutdown_watcher.is_shutting_down() {
            smol::Timer::after(std::time::Duration::from_millis(100)).await;
        }
        // Connect to ourselves to unblock accept()
        let _ = Async::<UnixStream>::connect(&sock_clone).await;
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

    // Auto-resume interrupted child sessions.
    {
        let resume_ids = {
            let st = lock_state(&state);
            st.db.sessions_needing_resume().unwrap_or_else(|e| {
                eprintln!("failed to query sessions needing resume: {}", e);
                Vec::new()
            })
        };
        let mut resuming: std::collections::HashSet<String> = resume_ids.iter().cloned().collect();
        for sid in resume_ids {
            eprintln!("auto-resuming session {}", sid);
            let s = state.clone();
            let p = plugins.clone();
            let sh = shutdown.clone();
            let sl = session_locks.clone();
            let th = throttle.clone();
            let ov: SharedTestOverrides = Arc::new(TestOverrides::default());
            smol::spawn(async move {
                if let Err(e) = resume_child_session(s, p, sh, sl, th, sid.clone(), ov).await {
                    eprintln!("auto-resume session {} error: {}", sid, e);
                }
            })
            .detach();
        }

        // Also resume sessions that have pending queued messages but aren't
        // already being auto-resumed.
        let queued_ids = {
            let st = lock_state(&state);
            st.db.sessions_with_queued_messages().unwrap_or_else(|e| {
                eprintln!("failed to query sessions with queued messages: {}", e);
                Vec::new()
            })
        };
        for sid in queued_ids {
            if resuming.insert(sid.clone()) {
                eprintln!("auto-resuming session {} (has queued messages)", sid);
                // Set the has_queued flag so the agent loop will drain them.
                {
                    let mut st = lock_state(&state);
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
                let ov: SharedTestOverrides = Arc::new(TestOverrides::default());
                smol::spawn(async move {
                    if let Err(e) = resume_child_session(s, p, sh, sl, th, sid.clone(), ov).await {
                        eprintln!("auto-resume session {} error: {}", sid, e);
                    }
                })
                .detach();
            }
        }
    }

    loop {
        let (stream, _) = listener
            .accept()
            .await
            .map_err(|e| crate::Error::Io(e.to_string()))?;

        if shutdown.is_shutting_down() {
            break;
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
                eprintln!("client error: {}", e);
            }
        })
        .detach();
    }

    // Wait for in-flight agent loops to finish (up to 60s)
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while shutdown.active_count() > 0 && std::time::Instant::now() < deadline {
        eprintln!(
            "waiting for {} in-flight request(s)...",
            shutdown.active_count()
        );
        smol::Timer::after(std::time::Duration::from_secs(1)).await;
    }
    if shutdown.active_count() > 0 {
        eprintln!(
            "timeout: {} request(s) still in flight, exiting anyway",
            shutdown.active_count()
        );
    }

    // Cleanup
    std::fs::remove_file(&sock).ok();
    std::fs::remove_file(pid_path()).ok();
    eprintln!("tau server stopped");
    Ok(())
}
