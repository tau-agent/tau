//! Unix socket server — manages sessions and streams LLM responses.

use std::collections::{HashMap, HashSet};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use futures::StreamExt;
use futures::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use smol::Async;

use crate::auth::{AuthCredential, AuthStorage};
use crate::compaction;
use crate::config;
use crate::db::{Db, StoredSession};
use crate::protocol::{ModelInfo, Request, Response, SessionInfo, SessionStats, TokenStats};
use crate::provider::ProviderRegistry;
use crate::types::*;

// ---------------------------------------------------------------------------
// Compute stats from a message list
// ---------------------------------------------------------------------------

fn compute_stats(messages: &[Message], model: &Model, is_subscription: bool) -> SessionStats {
    let mut user_messages = 0usize;
    let mut assistant_messages = 0usize;
    let mut tool_calls = 0usize;
    let mut tool_results = 0usize;
    let mut tokens = TokenStats::default();
    let mut cost = 0.0f64;
    let mut last_input_tokens: Option<u64> = None;

    for msg in messages {
        match msg {
            Message::User(_) => user_messages += 1,
            Message::Assistant(a) => {
                assistant_messages += 1;
                for c in &a.content {
                    if matches!(c, AssistantContent::ToolCall(_)) {
                        tool_calls += 1;
                    }
                }
                tokens.input += a.usage.input;
                tokens.output += a.usage.output;
                tokens.cache_read += a.usage.cache_read;
                tokens.cache_write += a.usage.cache_write;
                cost += a.usage.cost.total;

                if a.stop_reason != StopReason::Error && a.stop_reason != StopReason::Aborted {
                    last_input_tokens =
                        Some(a.usage.input + a.usage.cache_read + a.usage.cache_write);
                }
            }
            Message::ToolResult(_) => tool_results += 1,
            Message::CompactionSummary(_) => {}
        }
    }

    SessionStats {
        user_messages,
        assistant_messages,
        tool_calls,
        tool_results,
        tokens,
        cost,
        is_subscription,
        context_window: model.context_window,
        context_tokens: last_input_tokens,
    }
}

fn session_info(
    stored: &StoredSession,
    messages: &[Message],
    last_message_time: Option<i64>,
    child_count: usize,
    phase: Option<&crate::types::AgentPhase>,
) -> SessionInfo {
    let stats = compute_stats(messages, &stored.model, stored.is_subscription);
    let context_pct = if stats.context_window > 0 {
        stats
            .context_tokens
            .map(|t| (t as f64 / stats.context_window as f64) * 100.0)
    } else {
        None
    };
    SessionInfo {
        id: stored.id.clone(),
        model: stored.model.id.clone(),
        provider: stored.model.provider.clone(),
        cwd: stored.cwd.clone(),
        message_count: messages.len(),
        stats,
        // Timestamps in DB are milliseconds; convert to seconds for display
        last_activity: last_message_time.unwrap_or(stored.created_at) / 1000,
        parent_id: stored.parent_id.clone(),
        child_count,
        child_budget: stored.child_budget,
        tagline: stored.tagline.clone(),
        state: phase
            .copied()
            .unwrap_or_default()
            .label()
            .trim_end_matches("...")
            .to_string(),
        context_pct,
        archived: stored.archived,
        last_exit_status: stored.last_exit_status.clone(),
    }
}

/// Build a `SessionInfo` from pre-computed DB-level stats (no message
/// deserialisation).  Used by `list_sessions_impl` for O(1)-per-session cost.
fn session_info_from_db_stats(
    stored: &StoredSession,
    db_stats: Option<&crate::db::DbSessionStats>,
    child_count: usize,
    phase: Option<&crate::types::AgentPhase>,
) -> SessionInfo {
    let empty = crate::db::DbSessionStats::default();
    let ds = db_stats.unwrap_or(&empty);

    let stats = SessionStats {
        user_messages: ds.user_messages,
        assistant_messages: ds.assistant_messages,
        tool_calls: ds.tool_calls,
        tool_results: ds.tool_results,
        tokens: TokenStats {
            input: ds.tokens_input,
            output: ds.tokens_output,
            cache_read: ds.tokens_cache_read,
            cache_write: ds.tokens_cache_write,
        },
        cost: ds.cost,
        is_subscription: stored.is_subscription,
        context_window: stored.model.context_window,
        context_tokens: ds.last_input_tokens,
    };

    let context_pct = if stats.context_window > 0 {
        stats
            .context_tokens
            .map(|t| (t as f64 / stats.context_window as f64) * 100.0)
    } else {
        None
    };

    SessionInfo {
        id: stored.id.clone(),
        model: stored.model.id.clone(),
        provider: stored.model.provider.clone(),
        cwd: stored.cwd.clone(),
        message_count: ds.message_count,
        stats,
        last_activity: ds.last_message_time.unwrap_or(stored.created_at) / 1000,
        parent_id: stored.parent_id.clone(),
        child_count,
        child_budget: stored.child_budget,
        tagline: stored.tagline.clone(),
        state: phase
            .copied()
            .unwrap_or_default()
            .label()
            .trim_end_matches("...")
            .to_string(),
        context_pct,
        archived: stored.archived,
        last_exit_status: stored.last_exit_status.clone(),
    }
}

fn model_info(m: &Model) -> ModelInfo {
    ModelInfo {
        id: m.id.clone(),
        name: m.name.clone(),
        provider: m.provider.clone(),
        thinking: m.thinking.clone(),
        context_window: m.context_window,
        max_tokens: m.max_tokens,
    }
}

// ---------------------------------------------------------------------------
// Server state
// ---------------------------------------------------------------------------

const USAGE_CACHE_TTL_MS: u64 = 5 * 60 * 1000;

/// Resolve API key: auth.json → config provider api_key → env var.
fn resolve_api_key(
    auth: &AuthStorage,
    cfg: &config::Config,
    provider: &str,
) -> crate::Result<Option<String>> {
    // First try auth storage (handles OAuth refresh, env vars, etc.)
    if let Ok(Some(key)) = auth.get_api_key(provider) {
        return Ok(Some(key));
    }
    // Then try config's inline api_key
    if let Some(pc) = cfg.providers.get(provider)
        && let Some(key) = config::resolve_provider_api_key(pc)
    {
        return Ok(Some(key));
    }
    Ok(None)
}

struct State {
    db: Db,
    registry: ProviderRegistry,
    auth: AuthStorage,
    config: config::Config,
    default_model: Model,
    /// All known models (for /model listing).
    all_models: Vec<Model>,
    /// Cached subscription usage (value, fetched_at_ms).
    usage_cache: Option<(crate::auth::SubscriptionUsage, u64)>,
    /// Per-session cancel flags.  Set by CancelChat, cleared on Chat start.
    cancel_flags: HashMap<String, Arc<AtomicBool>>,
    /// Per-session flag indicating queued messages are pending.
    has_queued: HashMap<String, Arc<AtomicBool>>,
    /// Per-session broadcast subscribers.
    /// Other clients watching a session receive streamed responses.
    subscribers: HashMap<String, Vec<smol::channel::Sender<Response>>>,
    /// Current agent phase per session, for new subscribers.
    phases: HashMap<String, crate::types::AgentPhase>,
    /// Sessions currently being waited on by WaitSessions/WaitAnySessions.
    /// Maps child_session_id -> parent_session_id. Used to suppress redundant
    /// completion notifications when parent is actively joining.
    waited_sessions: HashSet<String>,
    /// Waiters notified when any session's agent turn completes.
    /// Each entry is a one-shot-ish sender; closed/full senders are pruned on notify.
    session_done_waiters: Vec<smol::channel::Sender<()>>,
    /// Pending reply waiters for `await_reply` messages.
    /// Key is msg_id, value is a oneshot sender for the reply content.
    reply_waiters: HashMap<String, smol::channel::Sender<String>>,
    /// Monotonic counter for generating unique msg_ids.
    next_msg_id: u64,
}

type SharedState = Arc<Mutex<State>>;

use crate::truncate_str;

fn lock_state(state: &SharedState) -> std::sync::MutexGuard<'_, State> {
    state.lock().unwrap_or_else(|e| {
        eprintln!("warning: recovering from poisoned mutex");
        e.into_inner()
    })
}

/// Per-session async locks to serialize Chat requests.
/// The outer std::Mutex is only held briefly to get/create a lock.
/// The inner smol::lock::Mutex is held across the entire agent turn.
type SessionLocks = Arc<Mutex<HashMap<String, Arc<smol::lock::Mutex<()>>>>>;

/// Get or create an async lock for a session.
fn session_lock(locks: &SessionLocks, session_id: &str) -> Arc<smol::lock::Mutex<()>> {
    let mut map = locks.lock().unwrap();
    map.entry(session_id.to_string())
        .or_insert_with(|| Arc::new(smol::lock::Mutex::new(())))
        .clone()
}

/// A sender that can deliver shutdown notifications to a connected client.
type ClientNotifier = smol::channel::Sender<Response>;

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
        let clients = self.clients.lock().unwrap();
        let msg = Response::ServerShutdown { restart };
        for tx in clients.iter() {
            if tx.try_send(msg.clone()).is_err() {
                eprintln!("warning: failed to send shutdown notification to client");
            }
        }
    }

    fn register_client(&self) -> smol::channel::Receiver<Response> {
        let (tx, rx) = smol::channel::bounded(1);
        self.clients.lock().unwrap().push(tx);
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

// ---------------------------------------------------------------------------
// Idle sweep for session plugins
// ---------------------------------------------------------------------------

/// Spawn a background task that periodically sends idle notifications
/// to session plugins that have no active subscribers.
fn spawn_idle_sweep(
    plugins: Arc<Mutex<crate::plugin::PluginManager>>,
    state: SharedState,
    shutdown: ShutdownHandle,
) {
    let idle_timeout = plugins.lock().unwrap().idle_timeout();
    if idle_timeout.is_zero() {
        return; // Idle sweep disabled
    }
    // Sweep interval: half the idle timeout, minimum 5s
    let interval = std::cmp::max(idle_timeout / 2, std::time::Duration::from_secs(5));
    smol::spawn(async move {
        loop {
            smol::Timer::after(interval).await;
            if shutdown.is_shutting_down() {
                break;
            }
            // Collect subscriber info on the async side (cheap, just lock state briefly)
            let subscribed_sessions: std::collections::HashSet<String> = {
                let st = lock_state(&state);
                st.subscribers
                    .iter()
                    .filter(|(_, subs)| !subs.is_empty())
                    .map(|(id, _)| id.clone())
                    .collect()
            };
            // Run sweep on a blocking thread since plugin I/O is synchronous
            let plugins_clone = plugins.clone();
            let _ = smol::unblock(move || {
                let mut pm = plugins_clone.lock().unwrap();
                pm.idle_sweep(idle_timeout, &|session_id: &str| {
                    subscribed_sessions.contains(session_id)
                });
            })
            .await;
        }
    })
    .detach();
}

// ---------------------------------------------------------------------------
// Background reader/writer tasks for global plugins
// ---------------------------------------------------------------------------

/// Read one `PluginMessage` from an async stdout reader.
async fn read_plugin_message(
    reader: &mut crate::plugin::AsyncPluginReader,
) -> crate::Result<crate::plugin::PluginMessage> {
    let mut line = String::new();
    let n = reader
        .read_line(&mut line)
        .await
        .map_err(|e| crate::Error::Io(format!("read from plugin: {}", e)))?;
    if n == 0 {
        return Err(crate::Error::Io("plugin closed stdout".into()));
    }
    serde_json::from_str(&line).map_err(|e| crate::Error::Parse(format!("plugin message: {}", e)))
}

/// Write a `PluginRequest` to an async stdin writer.
async fn write_plugin_request(
    writer: &mut crate::plugin::AsyncPluginWriter,
    req: &crate::plugin::PluginRequest,
) -> crate::Result<()> {
    use futures::io::AsyncWriteExt;
    let mut line = serde_json::to_string(req).map_err(|e| crate::Error::Parse(e.to_string()))?;
    line.push('\n');
    writer
        .write_all(line.as_bytes())
        .await
        .map_err(|e| crate::Error::Io(format!("write to plugin: {}", e)))?;
    writer
        .flush()
        .await
        .map_err(|e| crate::Error::Io(format!("flush plugin: {}", e)))?;
    Ok(())
}

/// Create a chat-spawn channel with a receiver task that fires off
/// `run_child_chat` for each `(session_id, text)` pair.
///
/// Used by `spawn_global_plugin_background_tasks` so that background
/// `ServerRequest::Chat` calls can spawn agent turns.
fn spawn_bg_chat_receiver(
    state: SharedState,
    plugins: Arc<Mutex<crate::plugin::PluginManager>>,
    shutdown: ShutdownHandle,
    session_locks: SessionLocks,
    throttle: crate::throttle::ProviderThrottle,
) -> smol::channel::Sender<(String, String)> {
    let (tx, rx) = smol::channel::unbounded::<(String, String)>();
    smol::spawn(async move {
        while let Ok((child_session_id, text)) = rx.recv().await {
            let s = state.clone();
            let p = plugins.clone();
            let sh = shutdown.clone();
            let sl = session_locks.clone();
            let th = throttle.clone();
            let ov: SharedTestOverrides = Arc::new(TestOverrides::default());
            smol::spawn(async move {
                let sid = child_session_id;
                if let Err(e) = run_child_chat(s, p, sh, sl, th, sid.clone(), text, ov).await {
                    eprintln!("bg child chat {} error: {}", sid, e);
                }
            })
            .detach();
        }
    })
    .detach();
    tx
}

/// Spawn background reader/writer tasks for all global plugins.
///
/// For each global plugin:
/// - A **reader task** reads messages from the plugin's stdout.
///   `ServerRequest` messages are handled inline (via `handle_server_request`);
///   all other messages (e.g. `ToolResult`, `OutputDelta`) are forwarded to the
///   plugin handle through a channel so that `PluginExecutor` can consume them
///   during tool calls.
/// - A **writer task** drains a channel of `PluginRequest` messages and writes
///   them to the plugin's stdin.  Both the `PluginExecutor` (via `send_async`)
///   and the reader task (to send `ServerResponse`) share this channel.
///
/// These tasks are detached and run until the plugin dies or the server shuts
/// down.
#[allow(clippy::too_many_arguments)]
fn spawn_global_plugin_background_tasks(
    plugins: &Arc<Mutex<crate::plugin::PluginManager>>,
    state: &SharedState,
    session_locks: &SessionLocks,
    shutdown: &ShutdownHandle,
    throttle: &crate::throttle::ProviderThrottle,
    chat_spawn_tx: &smol::channel::Sender<(String, String)>,
    test_overrides: &SharedTestOverrides,
) {
    let io_pairs = {
        let mut pm = plugins.lock().unwrap();
        pm.setup_background_io()
    };

    for (plugin_name, mut reader, mut writer, msg_tx, write_rx) in io_pairs {
        // --- Writer task: drain write_rx → stdin ---
        let writer_plugin_name = plugin_name.clone();
        smol::spawn(async move {
            while let Ok(req) = write_rx.recv().await {
                if let Err(e) = write_plugin_request(&mut writer, &req).await {
                    eprintln!(
                        "global plugin '{}' background writer error: {}",
                        writer_plugin_name, e
                    );
                    break;
                }
            }
        })
        .detach();

        // --- Reader task: stdout → route messages ---
        let reader_state = state.clone();
        let reader_session_locks = session_locks.clone();
        let reader_plugins = plugins.clone();
        let reader_shutdown = shutdown.clone();
        let reader_throttle = throttle.clone();
        let reader_chat_tx = chat_spawn_tx.clone();
        let reader_test_overrides = test_overrides.clone();
        // Get a sender clone for the writer channel so the reader task can
        // send ServerResponse messages back to the plugin.
        let resp_tx = {
            let pm = plugins.lock().unwrap();
            pm.get_global_write_tx(&plugin_name)
        };
        let resp_tx = match resp_tx {
            Some(tx) => tx,
            None => {
                eprintln!(
                    "global plugin '{}': no write channel for background reader",
                    plugin_name
                );
                continue;
            }
        };

        smol::spawn(async move {
            loop {
                let msg = match read_plugin_message(&mut reader).await {
                    Ok(msg) => msg,
                    Err(e) => {
                        // Don't log during shutdown — plugin may have been killed.
                        if !reader_shutdown.is_shutting_down() {
                            eprintln!("global plugin '{}' background reader: {}", plugin_name, e);
                        }
                        break;
                    }
                };

                match msg {
                    crate::plugin::PluginMessage::ServerRequest {
                        request_id,
                        request,
                    } => {
                        let response = handle_server_request(
                            &reader_state,
                            &reader_session_locks,
                            &reader_plugins,
                            &reader_shutdown,
                            &reader_throttle,
                            &reader_chat_tx,
                            &reader_test_overrides,
                            &request,
                            // Background requests have no specific session context;
                            // use an empty session ID.
                            "",
                        )
                        .await;
                        let resp_req = crate::plugin::PluginRequest::ServerResponse {
                            request_id,
                            response,
                        };
                        if resp_tx.send(resp_req).await.is_err() {
                            eprintln!(
                                "global plugin '{}' background reader: write channel closed",
                                plugin_name
                            );
                            break;
                        }
                    }
                    other => {
                        // Forward to plugin handle for tool-call consumption.
                        if msg_tx.send(other).await.is_err() {
                            // Handle was dropped (plugin killed / reloaded).
                            break;
                        }
                    }
                }
            }
        })
        .detach();
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Returns the default socket path.
pub fn socket_path() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        PathBuf::from(dir).join("tau").join("tau.sock")
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(".tau").join("tau.sock")
    } else {
        PathBuf::from("/tmp")
            .join(format!("tau-{}", std::process::id()))
            .join("tau.sock")
    }
}

/// Returns the PID file path next to the socket.
pub fn pid_path() -> PathBuf {
    let mut p = socket_path();
    p.set_file_name("tau.pid");
    p
}

/// Build registry with all known API providers.
fn build_registry() -> ProviderRegistry {
    let mut registry = ProviderRegistry::new();
    registry.register(crate::providers::anthropic::Anthropic);
    registry.register(crate::providers::openai::OpenAi);
    registry.register(crate::providers::log::LogProvider);
    registry
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
}

/// Run a server with custom config (for testing).
pub async fn run_with_config(config: TestServerConfig) -> crate::Result<()> {
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
            if let Err(e) = handle_client(
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
    let registry = build_registry();
    let cfg = config::load_config()?;
    let all_models = config::resolve_models(&cfg);
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
            if let Err(e) = handle_client(
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

/// Check if a server is already running by trying to connect.
pub fn is_running() -> bool {
    std::os::unix::net::UnixStream::connect(socket_path()).is_ok()
}

// ---------------------------------------------------------------------------
// Client handler
// ---------------------------------------------------------------------------

/// Check whether a session has pending queued messages and, if so, spawn a
/// `resume_child_session` task so they are processed.
///
/// This must be called while the session lock is still held — just before it is
/// about to be dropped.  When `queue_and_maybe_resume` sees the lock as held it
/// skips spawning a resume, expecting the current lock-holder to drain the
/// messages.  But if the agent turn has already finished and we are in post-turn
/// cleanup, the drain callback will never fire again.  Calling this function
/// closes that gap: the spawned task will block on `lock_arc().await` and pick
/// up the messages as soon as the current guard drops.
fn maybe_respawn_for_queued(
    state: &SharedState,
    session_locks: &SessionLocks,
    plugins: &Arc<Mutex<crate::plugin::PluginManager>>,
    shutdown: &ShutdownHandle,
    throttle: &crate::throttle::ProviderThrottle,
    session_id: &str,
    test_overrides: &SharedTestOverrides,
) {
    let has_pending = {
        let st = lock_state(state);
        st.has_queued
            .get(session_id)
            .map(|f| f.load(Ordering::Acquire))
            .unwrap_or(false)
    };
    if has_pending {
        let s = state.clone();
        let p = plugins.clone();
        let sh = shutdown.clone();
        let sl = session_locks.clone();
        let th = throttle.clone();
        let ov = test_overrides.clone();
        let sid = session_id.to_string();
        smol::spawn(async move {
            if let Err(e) = resume_child_session(s, p, sh, sl, th, sid.clone(), ov).await {
                eprintln!("resume session {} for late-queued message: {}", sid, e);
            }
        })
        .detach();
    }
}

/// Queue a message for delivery to a target session.
/// Persists immediately and sets the has_queued flag for in-flight agent loops.
fn queue_message_to_session(state: &SharedState, target: &str, content: &str, sender_info: &str) {
    let mut st = lock_state(state);
    st.db.queue_message(target, content, sender_info).ok();
    st.has_queued
        .entry(target.to_string())
        .or_insert_with(|| Arc::new(AtomicBool::new(false)))
        .store(true, Ordering::Release);
}

/// Queue a message and, if the target session is idle, spawn a resume task so
/// the message is processed without waiting for the next user interaction.
#[allow(clippy::too_many_arguments)]
fn queue_and_maybe_resume(
    state: &SharedState,
    session_locks: &SessionLocks,
    plugins: &Arc<Mutex<crate::plugin::PluginManager>>,
    shutdown: &ShutdownHandle,
    throttle: &crate::throttle::ProviderThrottle,
    target: &str,
    content: &str,
    sender_info: &str,
    test_overrides: &SharedTestOverrides,
) {
    queue_message_to_session(state, target, content, sender_info);
    // If the target session is idle (lock is free), spawn a resume task.
    let needs_resume = {
        let lock = session_lock(session_locks, target);
        lock.try_lock().is_some()
    };
    if needs_resume {
        let s = state.clone();
        let p = plugins.clone();
        let sh = shutdown.clone();
        let sl = session_locks.clone();
        let th = throttle.clone();
        let ov = test_overrides.clone();
        let sid = target.to_string();
        smol::spawn(async move {
            if let Err(e) = resume_child_session(s, p, sh, sl, th, sid.clone(), ov).await {
                eprintln!("resume session {} after queued message: {}", sid, e);
            }
        })
        .detach();
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_client(
    stream: Async<UnixStream>,
    state: SharedState,
    plugins: Arc<Mutex<crate::plugin::PluginManager>>,
    shutdown: ShutdownHandle,
    session_locks: SessionLocks,
    throttle: crate::throttle::ProviderThrottle,
    test_overrides: SharedTestOverrides,
    bg_chat_spawn_tx: smol::channel::Sender<(String, String)>,
) -> crate::Result<()> {
    // Register for shutdown notifications
    let shutdown_rx = shutdown.register_client();
    let reader = BufReader::new(&stream);
    let mut writer = &stream;
    let mut lines = reader.lines();

    loop {
        // Wait for either a request line or a shutdown notification
        let line = {
            let line_fut = lines.next();
            let shutdown_fut = shutdown_rx.recv();

            match futures::future::select(std::pin::pin!(line_fut), std::pin::pin!(shutdown_fut))
                .await
            {
                futures::future::Either::Left((Some(line), _)) => {
                    line.map_err(|e: std::io::Error| crate::Error::Io(e.to_string()))?
                }
                futures::future::Either::Left((None, _)) => break, // client disconnected
                futures::future::Either::Right((Ok(msg), _)) => {
                    // Shutdown notification — send to client and exit
                    send(&mut writer, &msg).await.ok();
                    break;
                }
                futures::future::Either::Right((Err(_), _)) => break,
            }
        };
        if line.trim().is_empty() {
            continue;
        }

        let req: Request = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                send(
                    &mut writer,
                    &Response::Error {
                        message: format!("bad request: {}", e),
                    },
                )
                .await?;
                continue;
            }
        };

        match req {
            Request::CreateSession {
                model: model_id,
                provider: provider_name,
                system_prompt,
                cwd,
                parent_id,
                child_budget,
                tagline,
                auto_archive,
            } => {
                // Atomic budget check + session creation (single lock hold)
                let resp = create_session_impl(
                    &state,
                    &model_id,
                    &provider_name,
                    &system_prompt,
                    &cwd,
                    &parent_id,
                    child_budget,
                    &tagline,
                    auto_archive,
                );

                // If created and no explicit system prompt, set up plugins
                // and update the prompt post-creation.
                if let Response::SessionCreated { ref session_id } = resp
                    && system_prompt.is_none()
                {
                    let id = session_id.clone();
                    let cwd_resolved = {
                        let st = lock_state(&state);
                        st.db.get_session(&id).ok().flatten().and_then(|s| s.cwd)
                    };
                    let cwd_str = cwd_resolved.as_deref().unwrap_or("/tmp");
                    let mut pm = plugins.lock().unwrap();
                    if let Err(e) = pm.ensure_session_plugins(&id, cwd_str) {
                        eprintln!("failed to spawn session plugins: {}", e);
                    }
                    let tool_prompts = pm.tool_prompts(&id, child_budget);
                    let prompt =
                        crate::system_prompt::build(&crate::system_prompt::PromptOptions {
                            cwd: cwd_resolved,
                            tools: tool_prompts,
                            ..Default::default()
                        });
                    let st = lock_state(&state);
                    if let Err(e) = st.db.update_system_prompt(&id, &prompt) {
                        eprintln!("failed to update system prompt: {}", e);
                    }
                }

                send(&mut writer, &resp).await?;
            }
            Request::GetSessionInfo { session_id } => {
                let resp = get_session_info_impl(&state, &session_id);
                send(&mut writer, &resp).await?;
            }
            Request::Chat { session_id, text } => {
                if shutdown.is_shutting_down() {
                    let resp = Response::Error {
                        message: "server is shutting down".into(),
                    };
                    broadcast_to_subscribers(&state, &session_id, &resp);
                    send(&mut writer, &resp).await.ok();
                    continue;
                }
                // Acquire per-session lock — serializes concurrent Chat requests.
                // If another agent turn is running, this awaits until it finishes.
                // Try non-blocking lock first; if contended, notify and then block.
                let session_mutex = session_lock(&session_locks, &session_id);
                let _session_guard = match session_mutex.try_lock_arc() {
                    Some(guard) => guard,
                    None => {
                        emit_phase(&state, &session_id, crate::types::AgentPhase::Waiting);
                        session_mutex.lock_arc().await
                    }
                };

                // Reset (and create) the cancel flag for this session.
                let cancel_flag: Arc<AtomicBool> = {
                    let mut st = lock_state(&state);
                    let flag = st
                        .cancel_flags
                        .entry(session_id.clone())
                        .or_insert_with(|| Arc::new(AtomicBool::new(false)));
                    flag.store(false, Ordering::Relaxed);
                    flag.clone()
                };

                emit_phase(&state, &session_id, crate::types::AgentPhase::Preparing);

                // Run the Chat handler body inside a closure so that any
                // error is caught and we *always* broadcast a terminal
                // response (AgentDone / Cancelled / Error) to subscribers.
                // Without this guarantee the TUI gets stuck in Streaming
                // mode forever when an internal error (e.g. DB write)
                // causes the handler to bail out early via `?`.
                let chat_result: Result<(bool, bool), crate::Error> = async {
                    // Load session
                    let session_data = {
                        let st = lock_state(&state);
                        match st.db.get_session(&session_id) {
                            Ok(Some(stored)) => {
                                let messages = st.db.get_messages(&session_id)?;
                                let cwd = stored.cwd.clone().unwrap_or_else(|| "/tmp".to_string());
                                Ok((stored, messages, cwd))
                            }
                            Ok(None) => Err(crate::Error::Io(format!(
                                "session not found: {}",
                                session_id
                            ))),
                            Err(e) => Err(e),
                        }
                    };
                    let (stored, mut messages, cwd) = session_data?;
                    let model = stored.model.clone();

                    // Ensure session plugins are spawned and notify session start
                    {
                        let mut pm = plugins.lock().unwrap();
                        if let Err(e) = pm.ensure_session_plugins(&session_id, &cwd) {
                            eprintln!("failed to spawn session plugins: {}", e);
                        }
                        pm.notify_session_start_once(&cwd, &session_id);
                    }

                    // Repair any corrupted message history (e.g. daemon killed
                    // mid-tool-execution, leaving tool_use without tool_result).
                    let repair_stubs = crate::agent::repair_messages(&messages);
                    if !repair_stubs.is_empty() {
                        eprintln!(
                            "session {}: repaired {} missing tool_result message(s)",
                            session_id,
                            repair_stubs.len()
                        );
                        let st = lock_state(&state);
                        for stub in &repair_stubs {
                            if let Err(e) = st.db.append_message(&session_id, stub) {
                                eprintln!("db error persisting repair stub: {}", e);
                            }
                        }
                        messages.extend(repair_stubs);
                    }

                    // If session was interrupted mid-tool-call, continue first
                    if crate::agent::needs_continuation(&messages) {
                        let mut context = Context {
                            system_prompt: stored.system_prompt.clone(),
                            messages: messages.clone(),
                            tools: Vec::new(),
                        };
                        let cont_result = run_agent_turn(
                            &state,
                            &plugins,
                            &shutdown,
                            cancel_flag.clone(),
                            &model,
                            &mut context,
                            &cwd,
                            &session_id,
                            &mut writer,
                            &throttle,
                            &session_locks,
                            &test_overrides,
                        )
                        .await;
                        match cont_result {
                            Ok(_agent_result) => {
                                // Messages already persisted incrementally via on_message
                                let st = lock_state(&state);
                                messages = st.db.get_messages(&session_id)?;
                            }
                            Err(e) => {
                                eprintln!("continuation error: {}", e);
                            }
                        }
                    }

                    // Call before_agent_start hooks (plugins inject context)
                    let mut system_prompt = stored.system_prompt.clone();
                    {
                        let mut pm = plugins.lock().unwrap();
                        let hook_data = serde_json::json!({
                            "prompt": &text,
                            "system_prompt": &system_prompt,
                            "session_id": &session_id,
                            "message_count": messages.len(),
                        });
                        let results = pm.call_hook(&session_id, "before_agent_start", &hook_data);
                        for result in results {
                            if let Some(msg) = result.message {
                                let ctx_msg = Message::User(UserMessage::text(&msg.content));
                                {
                                    let st = lock_state(&state);
                                    if let Err(e) = st.db.append_message(&session_id, &ctx_msg) {
                                        eprintln!("db error persisting hook context: {}", e);
                                    }
                                }
                                messages.push(ctx_msg);
                            }
                            if let Some(sp) = result.system_prompt {
                                system_prompt = Some(sp);
                            }
                        }
                    }

                    // Append user message (persisted to DB)
                    let user_msg = Message::User(UserMessage::text(&text));
                    {
                        let st = lock_state(&state);
                        st.db.append_message(&session_id, &user_msg)?;
                        // Auto-derive tagline from first user message if not set
                        if stored.tagline.is_none() {
                            let tagline = text.replace('\n', " ");
                            let tagline = if tagline.len() > 80 {
                                format!("{}...", truncate_str(&tagline, 77))
                            } else {
                                tagline
                            };
                            let _ = st.db.update_tagline(&session_id, &tagline);
                        }
                    }
                    messages.push(user_msg);

                    // Broadcast user message to subscribers
                    broadcast_to_subscribers(
                        &state,
                        &session_id,
                        &Response::UserMessage { text: text.clone() },
                    );

                    let mut context = Context {
                        system_prompt,
                        messages,
                        tools: Vec::new(),
                    };
                    let result = run_agent_turn(
                        &state,
                        &plugins,
                        &shutdown,
                        cancel_flag.clone(),
                        &model,
                        &mut context,
                        &cwd,
                        &session_id,
                        &mut writer,
                        &throttle,
                        &session_locks,
                        &test_overrides,
                    )
                    .await;

                    let max_turns_reached = match result {
                        Ok(ref agent_result) => {
                            // Messages already persisted incrementally via on_message
                            agent_result.max_turns_reached
                        }
                        Err(crate::Error::Cancelled) => {
                            cancel_flag.store(true, Ordering::Relaxed);
                            false
                        }
                        Err(e) => {
                            // Update throttle on rate limit errors
                            if let crate::Error::Http(ref msg) = e {
                                throttle.handle_error(&model.provider, msg, None);
                            }
                            let err_msg = format!("agent error: {}", e);
                            throttle.handle_error(&model.provider, &err_msg, None);
                            return Err(e);
                        }
                    };

                    // Check compaction
                    let was_cancelled = cancel_flag.load(Ordering::Relaxed);
                    if !was_cancelled {
                        let should = {
                            let st = lock_state(&state);
                            let messages = st.db.get_messages(&session_id).unwrap_or_default();
                            let ctx_tokens = compaction::estimate_context_tokens(&messages);
                            compaction::should_compact(
                                ctx_tokens,
                                model.context_window,
                                &compaction::CompactionSettings::default(),
                            )
                        };
                        if should
                            && let Err(e) =
                                run_compaction(&state, &session_id, &model, &mut writer).await
                        {
                            eprintln!("compaction error: {}", e);
                        }
                    }

                    Ok((was_cancelled, max_turns_reached))
                }
                .await;

                // Always broadcast a terminal response so subscribers
                // (especially the TUI) never get stuck in Streaming mode.
                match chat_result {
                    Ok((true, _)) => {
                        // Cancelled
                        {
                            let st = lock_state(&state);
                            let _ = st.db.update_exit_status(&session_id, "cancelled");
                        }
                        let resp = Response::Cancelled;
                        broadcast_to_subscribers(&state, &session_id, &resp);
                        send(&mut writer, &resp).await.ok();
                    }
                    Ok((false, max_turns_reached)) => {
                        // Normal completion (or max turns reached)
                        {
                            let st = lock_state(&state);
                            let status = if max_turns_reached {
                                "max_turns"
                            } else {
                                "completed"
                            };
                            let _ = st.db.update_exit_status(&session_id, status);
                        }
                        if max_turns_reached {
                            let status_resp = Response::Stream {
                                event: Box::new(StreamEvent::Status {
                                    message: "Reached tool use limit. Send a message to continue."
                                        .to_string(),
                                }),
                            };
                            broadcast_to_subscribers(&state, &session_id, &status_resp);
                            send(&mut writer, &status_resp).await.ok();
                        }
                        let resp = Response::AgentDone;
                        broadcast_to_subscribers(&state, &session_id, &resp);
                        send(&mut writer, &resp).await.ok();
                    }
                    Err(e) => {
                        {
                            let st = lock_state(&state);
                            let _ = st.db.update_exit_status(&session_id, "error");
                        }
                        let err_resp = Response::Error {
                            message: format!("agent error: {}", e),
                        };
                        let done_resp = Response::AgentDone;
                        broadcast_to_subscribers(&state, &session_id, &err_resp);
                        broadcast_to_subscribers(&state, &session_id, &done_resp);
                        send(&mut writer, &err_resp).await.ok();
                        send(&mut writer, &done_resp).await.ok();
                    }
                }

                emit_phase(&state, &session_id, crate::types::AgentPhase::Idle);
                notify_session_done_waiters(&state);

                // Before the session lock drops, check whether new messages
                // arrived during post-turn cleanup.  See doc on the function.
                maybe_respawn_for_queued(
                    &state,
                    &session_locks,
                    &plugins,
                    &shutdown,
                    &throttle,
                    &session_id,
                    &test_overrides,
                );

                if shutdown.is_shutting_down() {
                    send(
                        &mut writer,
                        &Response::ServerShutdown {
                            restart: shutdown.restart.load(Ordering::Relaxed),
                        },
                    )
                    .await
                    .ok();
                }
            }
            Request::Subscribe { session_id } => {
                // Register this client as a subscriber for the session.
                // The connection stays open — we forward events via the channel.
                // No ack is sent; the client waits for Stream/AgentDone/Cancelled.
                let (tx, rx) = smol::channel::unbounded::<Response>();
                {
                    let mut st = lock_state(&state);
                    st.subscribers
                        .entry(session_id.clone())
                        .or_default()
                        .push(tx);
                }

                // Send current agent phase so newly connected TUI shows correct state.
                let phase_resp = {
                    let st = lock_state(&state);
                    let phase = st.phases.get(&session_id).copied().unwrap_or_default();
                    Response::Stream {
                        event: Box::new(crate::types::StreamEvent::Phase { phase }),
                    }
                };
                send(&mut writer, &phase_resp).await.ok();

                // Forward events until the channel closes or client disconnects.
                loop {
                    let resp = {
                        let recv_fut = rx.recv();
                        let shutdown_fut = shutdown_rx.recv();
                        match futures::future::select(
                            std::pin::pin!(recv_fut),
                            std::pin::pin!(shutdown_fut),
                        )
                        .await
                        {
                            futures::future::Either::Left((Ok(resp), _)) => resp,
                            futures::future::Either::Left((Err(_), _)) => break, // channel closed
                            futures::future::Either::Right((Ok(msg), _)) => {
                                send(&mut writer, &msg).await.ok();
                                break;
                            }
                            futures::future::Either::Right((Err(_), _)) => break,
                        }
                    };
                    if send(&mut writer, &resp).await.is_err() {
                        break; // client disconnected
                    }
                }
                break; // Subscribe consumes the connection
            }
            Request::GetMessages { session_id } => {
                let resp = get_messages_impl(&state, &session_id);
                send(&mut writer, &resp).await?;
            }
            Request::CancelChat { session_id } => {
                cancel_chat_impl(&state, &session_id);
                send(&mut writer, &Response::Ok).await.ok();
            }
            Request::Steer { session_id, text } => {
                // Queue the message persistently; if the session is idle,
                // spawn a resume so it gets processed immediately.
                queue_and_maybe_resume(
                    &state,
                    &session_locks,
                    &plugins,
                    &shutdown,
                    &throttle,
                    &session_id,
                    &text,
                    "steer",
                    &test_overrides,
                );
                send(&mut writer, &Response::Ok).await.ok();
            }
            Request::ListSessions { include_archived } => {
                let resp = list_sessions_impl(&state, include_archived);
                send(&mut writer, &resp).await?;
            }
            Request::ArchiveSession {
                session_id,
                require_ancestor,
            } => {
                // If require_ancestor is set, verify the target is a descendant
                if let Some(ref ancestor) = require_ancestor {
                    let is_desc = {
                        let st = lock_state(&state);
                        st.db.is_descendant(&session_id, ancestor)
                    };
                    match is_desc {
                        Ok(false) => {
                            send(
                                &mut writer,
                                &Response::Error {
                                    message: format!(
                                        "session {} is not a descendant of {}",
                                        session_id, ancestor
                                    ),
                                },
                            )
                            .await?;
                            continue;
                        }
                        Err(e) => {
                            send(
                                &mut writer,
                                &Response::Error {
                                    message: e.to_string(),
                                },
                            )
                            .await?;
                            continue;
                        }
                        Ok(true) => {} // proceed
                    }
                }

                // Validate: session must exist and all sessions in the subtree must be idle
                let subtree_ids = {
                    let st = lock_state(&state);
                    match st.db.get_session(&session_id)? {
                        Some(_) => Some(st.db.get_subtree_ids(&session_id)?),
                        None => None,
                    }
                };
                let subtree_ids = match subtree_ids {
                    Some(ids) => ids,
                    None => {
                        send(
                            &mut writer,
                            &Response::Error {
                                message: format!("session not found: {}", session_id),
                            },
                        )
                        .await?;
                        continue;
                    }
                };

                // Check all sessions in the subtree are idle (not locked)
                let mut busy_id = None;
                for sid in &subtree_ids {
                    let lock = session_lock(&session_locks, sid);
                    if lock.try_lock().is_none() {
                        busy_id = Some(sid.clone());
                        break;
                    }
                }
                if let Some(busy) = busy_id {
                    send(
                        &mut writer,
                        &Response::Error {
                            message: format!("cannot archive: session {} is busy", busy),
                        },
                    )
                    .await?;
                    continue;
                }

                // Archive in DB
                {
                    let st = lock_state(&state);
                    st.db.archive_session_tree(&session_id)?;
                }

                // Idle all session plugins for archived sessions
                {
                    let mut pm = plugins.lock().unwrap();
                    for sid in &subtree_ids {
                        pm.destroy_session_plugins(sid);
                    }
                }

                // Clean up in-memory state for archived sessions
                {
                    let mut st = lock_state(&state);
                    for sid in &subtree_ids {
                        st.cancel_flags.remove(sid);
                        st.has_queued.remove(sid);
                        st.subscribers.remove(sid);
                        st.phases.remove(sid);
                        st.waited_sessions.remove(sid);
                    }
                }

                send(&mut writer, &Response::SessionArchived).await?;
            }
            Request::RestoreSession { session_id } => {
                // Validate: session must exist and be archived
                let exists = {
                    let st = lock_state(&state);
                    st.db.get_session(&session_id)?
                };
                match exists {
                    None => {
                        send(
                            &mut writer,
                            &Response::Error {
                                message: format!("session not found: {}", session_id),
                            },
                        )
                        .await?;
                        continue;
                    }
                    Some(ref s) if !s.archived => {
                        send(
                            &mut writer,
                            &Response::Error {
                                message: format!("session {} is not archived", session_id),
                            },
                        )
                        .await?;
                        continue;
                    }
                    _ => {}
                }

                // Restore in DB
                {
                    let st = lock_state(&state);
                    st.db.restore_session_tree(&session_id)?;
                }

                send(&mut writer, &Response::SessionRestored).await?;
            }
            Request::DeleteSession { session_id } => {
                // Collect all session IDs in the subtree before deleting
                let subtree_ids = {
                    let st = lock_state(&state);
                    let ids = st.db.get_subtree_ids(&session_id)?;
                    // Delete session and all descendants
                    st.db.delete_session_tree(&session_id)?;
                    // Clean up waited_sessions for deleted IDs
                    ids
                };
                {
                    let mut st = lock_state(&state);
                    for id in &subtree_ids {
                        st.waited_sessions.remove(id);
                    }
                }
                // Clean up session plugins for all deleted sessions
                {
                    let mut pm = plugins.lock().unwrap();
                    for id in &subtree_ids {
                        pm.destroy_session_plugins(id);
                    }
                }
                send(&mut writer, &Response::SessionDeleted).await?;
            }
            Request::ListModels => {
                let models = {
                    let st = lock_state(&state);
                    st.all_models.iter().map(model_info).collect::<Vec<_>>()
                };
                send(&mut writer, &Response::Models { models }).await?;
            }
            Request::SetCwd { session_id, cwd } => {
                let result = {
                    let st = lock_state(&state);
                    st.db.update_cwd(&session_id, &cwd)
                };
                match result {
                    Ok(()) => {
                        send(&mut writer, &Response::Ok).await?;
                    }
                    Err(e) => {
                        send(
                            &mut writer,
                            &Response::Error {
                                message: e.to_string(),
                            },
                        )
                        .await?;
                    }
                }
            }
            Request::SetModel {
                session_id,
                model_id,
            } => {
                let result = {
                    let st = lock_state(&state);
                    if let Some(model) = st.all_models.iter().find(|m| m.id == model_id) {
                        st.db.update_model(&session_id, model)?;
                        Ok(model_info(model))
                    } else {
                        Err(format!(
                            "unknown model: {}. Use /model to list available models.",
                            model_id
                        ))
                    }
                };
                match result {
                    Ok(info) => {
                        send(&mut writer, &Response::ModelChanged { model: info }).await?;
                    }
                    Err(msg) => {
                        send(&mut writer, &Response::Error { message: msg }).await?;
                    }
                }
            }
            Request::Login { provider } => {
                let result = smol::unblock(move || {
                    if provider == "anthropic" {
                        crate::auth::login_anthropic()
                    } else {
                        Err(crate::Error::Io(format!(
                            "unknown OAuth provider: {}",
                            provider
                        )))
                    }
                })
                .await;

                match result {
                    Ok(creds) => {
                        let provider_name = "anthropic".to_string();
                        let save_result = {
                            let st = lock_state(&state);
                            st.auth.set(&provider_name, AuthCredential::Oauth(creds))
                        };
                        if let Err(e) = save_result {
                            send(
                                &mut writer,
                                &Response::Error {
                                    message: format!("failed to save credentials: {}", e),
                                },
                            )
                            .await?;
                        } else {
                            send(
                                &mut writer,
                                &Response::LoginSuccess {
                                    provider: provider_name,
                                },
                            )
                            .await?;
                        }
                    }
                    Err(e) => {
                        send(
                            &mut writer,
                            &Response::Error {
                                message: format!("login failed: {}", e),
                            },
                        )
                        .await?;
                    }
                }
            }
            Request::AuthStatus => {
                let providers = {
                    let st = lock_state(&state);
                    st.auth.list().unwrap_or_default()
                };
                send(&mut writer, &Response::AuthStatus { providers }).await?;
            }
            Request::GetSubscriptionUsage => {
                // Check cache, fetch if stale
                let cache_result = {
                    let st = lock_state(&state);
                    let now = crate::types::timestamp_ms();
                    if let Some((ref usage, fetched_at)) = st.usage_cache {
                        if now.saturating_sub(fetched_at) < USAGE_CACHE_TTL_MS {
                            Some(Ok(usage.clone()))
                        } else {
                            None // stale
                        }
                    } else {
                        None // not yet fetched
                    }
                };

                let result = if let Some(cached) = cache_result {
                    cached
                } else {
                    // Fetch outside the lock
                    let token = {
                        let st = lock_state(&state);
                        st.auth.get_api_key("anthropic")
                    };
                    match token {
                        Ok(Some(tok)) if crate::auth::is_oauth_token(&tok) => {
                            smol::unblock(move || crate::auth::fetch_subscription_usage(&tok)).await
                        }
                        _ => Err(crate::Error::NoApiKey(
                            "subscription usage requires OAuth login".into(),
                        )),
                    }
                };

                match result {
                    Ok(usage) => {
                        // Update cache
                        {
                            let mut st = lock_state(&state);
                            st.usage_cache = Some((usage.clone(), crate::types::timestamp_ms()));
                        }
                        send(&mut writer, &Response::SubscriptionUsage { usage }).await?;
                    }
                    Err(e) => {
                        send(
                            &mut writer,
                            &Response::Error {
                                message: e.to_string(),
                            },
                        )
                        .await?;
                    }
                }
            }
            Request::WaitSessions {
                session_ids,
                timeout_secs,
            } => {
                // Wait for all specified sessions to have no active agent turn.
                // A session is "done" if it's not currently locked (no active Chat).
                let deadline =
                    std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
                let mut results = Vec::new();

                // Register a waiter channel to be notified on session completion.
                let (notify_tx, notify_rx) = smol::channel::bounded::<()>(1);
                {
                    let mut st = lock_state(&state);
                    for sid in &session_ids {
                        st.waited_sessions.insert(sid.clone());
                    }
                    st.session_done_waiters.push(notify_tx);
                }

                loop {
                    let mut all_done = true;
                    results.clear();

                    for sid in &session_ids {
                        // Check if session has an active agent turn by trying the lock
                        let lock = session_lock(&session_locks, sid);
                        let is_busy = lock.try_lock().is_none();

                        if is_busy {
                            all_done = false;
                            results.push(crate::protocol::SessionResult {
                                session_id: sid.clone(),
                                status: "busy".into(),
                                summary: String::new(),
                            });
                        } else {
                            // Session is idle -- get its last assistant message as summary
                            let st = lock_state(&state);
                            let (status, summary) = match st.db.get_session(sid) {
                                Ok(Some(_)) => {
                                    let msgs = st.db.get_messages(sid).unwrap_or_default();
                                    ("done".to_string(), last_assistant_text(&msgs))
                                }
                                _ => ("deleted".to_string(), String::new()),
                            };
                            results.push(crate::protocol::SessionResult {
                                session_id: sid.clone(),
                                status,
                                summary,
                            });
                        }
                    }

                    if all_done || std::time::Instant::now() >= deadline {
                        // Mark timed-out sessions
                        if !all_done {
                            for r in &mut results {
                                if r.status == "busy" {
                                    r.status = "timeout".into();
                                }
                            }
                        }
                        break;
                    }

                    // Wait for a session-done notification or timeout.
                    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                    let _ = futures::future::select(
                        std::pin::pin!(notify_rx.recv()),
                        std::pin::pin!(smol::Timer::after(remaining)),
                    )
                    .await;
                }

                // Remove from waited set and drop our notifier.
                {
                    let mut st = lock_state(&state);
                    for sid in &session_ids {
                        st.waited_sessions.remove(sid);
                    }
                }
                // Close receiver so the sender is pruned on next notify.
                drop(notify_rx);

                auto_archive_done_sessions(&state, &results);
                send(&mut writer, &Response::SessionsCompleted { results }).await?;
            }
            Request::WaitAnySessions {
                session_ids,
                timeout_secs,
            } => {
                // Wait until at least one session completes.
                // Returns results for all sessions that are done at that point.
                let deadline =
                    std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
                let results;

                // Register a waiter channel to be notified on session completion.
                let (notify_tx, notify_rx) = smol::channel::bounded::<()>(1);
                {
                    let mut st = lock_state(&state);
                    for sid in &session_ids {
                        st.waited_sessions.insert(sid.clone());
                    }
                    st.session_done_waiters.push(notify_tx);
                }

                loop {
                    let mut done = Vec::new();

                    for sid in &session_ids {
                        let lock = session_lock(&session_locks, sid);
                        let is_busy = lock.try_lock().is_none();

                        if !is_busy {
                            let st = lock_state(&state);
                            let (status, summary) = match st.db.get_session(sid) {
                                Ok(Some(_)) => {
                                    let msgs = st.db.get_messages(sid).unwrap_or_default();
                                    ("done".to_string(), last_assistant_text(&msgs))
                                }
                                _ => ("deleted".to_string(), String::new()),
                            };
                            done.push(crate::protocol::SessionResult {
                                session_id: sid.clone(),
                                status,
                                summary,
                            });
                        }
                    }

                    if !done.is_empty() || std::time::Instant::now() >= deadline {
                        if done.is_empty() {
                            // Timeout -- mark all as timeout
                            results = session_ids
                                .iter()
                                .map(|sid| crate::protocol::SessionResult {
                                    session_id: sid.clone(),
                                    status: "timeout".into(),
                                    summary: String::new(),
                                })
                                .collect();
                        } else {
                            results = done;
                        }
                        break;
                    }

                    // Wait for a session-done notification or timeout.
                    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                    let _ = futures::future::select(
                        std::pin::pin!(notify_rx.recv()),
                        std::pin::pin!(smol::Timer::after(remaining)),
                    )
                    .await;
                }

                // Remove from waited set and drop our notifier.
                {
                    let mut st = lock_state(&state);
                    for sid in &session_ids {
                        st.waited_sessions.remove(sid);
                    }
                }
                drop(notify_rx);

                auto_archive_done_sessions(&state, &results);
                send(&mut writer, &Response::SessionsCompleted { results }).await?;
            }
            Request::QueueMessage {
                target_session_id,
                content,
                sender_info,
                await_reply,
                reply_to: _,
            } => {
                if await_reply {
                    // Generate a unique msg_id, create a oneshot channel,
                    // prefix the message so the target knows to reply.
                    let (msg_id, rx) = {
                        let mut st = lock_state(&state);
                        st.next_msg_id += 1;
                        let id = format!("m{}", st.next_msg_id);
                        let (tx, rx) = smol::channel::bounded::<String>(1);
                        st.reply_waiters.insert(id.clone(), tx);
                        (id, rx)
                    };

                    let prefixed = format!(
                        "[Message from {}, msg_id={}, awaits reply]\n{}",
                        sender_info, msg_id, content
                    );
                    queue_and_maybe_resume(
                        &state,
                        &session_locks,
                        &plugins,
                        &shutdown,
                        &throttle,
                        &target_session_id,
                        &prefixed,
                        &sender_info,
                        &test_overrides,
                    );

                    // Wait for reply with a timeout (default 5 min).
                    let timeout = std::time::Duration::from_secs(300);
                    match futures::future::select(
                        std::pin::pin!(rx.recv()),
                        std::pin::pin!(smol::Timer::after(timeout)),
                    )
                    .await
                    {
                        futures::future::Either::Left((Ok(reply), _)) => {
                            send(&mut writer, &Response::MessageReply { content: reply }).await?;
                        }
                        _ => {
                            // Timeout or channel closed — clean up waiter.
                            {
                                let mut st = lock_state(&state);
                                st.reply_waiters.remove(&msg_id);
                            }
                            send(
                                &mut writer,
                                &Response::Error {
                                    message: format!("await_reply timed out (msg_id={})", msg_id),
                                },
                            )
                            .await?;
                        }
                    }
                } else {
                    queue_and_maybe_resume(
                        &state,
                        &session_locks,
                        &plugins,
                        &shutdown,
                        &throttle,
                        &target_session_id,
                        &content,
                        &sender_info,
                        &test_overrides,
                    );
                    send(&mut writer, &Response::Ok).await?;
                }
            }
            Request::ReplyToMessage { msg_id, content } => {
                let result = {
                    let mut st = lock_state(&state);
                    st.reply_waiters.remove(&msg_id)
                };
                match result {
                    Some(tx) => {
                        let _ = tx.send(content).await;
                        send(&mut writer, &Response::Ok).await?;
                    }
                    None => {
                        send(
                            &mut writer,
                            &Response::Error {
                                message: format!("no pending waiter for msg_id={}", msg_id),
                            },
                        )
                        .await?;
                    }
                }
            }
            Request::ReloadPlugins { session_id } => {
                let cwd = {
                    let st = lock_state(&state);
                    st.db
                        .get_session(&session_id)
                        .ok()
                        .flatten()
                        .and_then(|s| s.cwd)
                        .unwrap_or_else(|| "/tmp".to_string())
                };
                let result = {
                    let mut pm = plugins.lock().unwrap();
                    pm.reload_config();
                    pm.destroy_session_plugins(&session_id);
                    pm.ensure_session_plugins(&session_id, &cwd)
                        .map(|()| pm.load_global_plugins(&cwd))
                };
                match result {
                    Ok(()) => {
                        // Restart background tasks for the new global plugins.
                        spawn_global_plugin_background_tasks(
                            &plugins,
                            &state,
                            &session_locks,
                            &shutdown,
                            &throttle,
                            &bg_chat_spawn_tx,
                            &test_overrides,
                        );
                        queue_message_to_session(
                            &state,
                            &session_id,
                            "[System: plugins reloaded. Tool definitions updated.]",
                            "system",
                        );
                        send(&mut writer, &Response::Ok).await?;
                    }
                    Err(e) => {
                        send(
                            &mut writer,
                            &Response::Error {
                                message: format!("reload session plugins: {}", e),
                            },
                        )
                        .await?;
                    }
                }
            }
            Request::GcSessions { older_than_days } => {
                let older_than_ms = {
                    let now = crate::types::timestamp_ms();
                    now.saturating_sub(older_than_days * 24 * 60 * 60 * 1000)
                };
                let result = {
                    let st = lock_state(&state);
                    st.db.gc_archived_sessions(older_than_ms)
                };
                match result {
                    Ok(deleted) => {
                        send(&mut writer, &Response::GcComplete { deleted }).await?;
                    }
                    Err(e) => {
                        send(
                            &mut writer,
                            &Response::Error {
                                message: e.to_string(),
                            },
                        )
                        .await?;
                    }
                }
            }
            Request::ExecuteTool {
                session_id,
                tool_name,
                arguments,
            } => {
                let resp = execute_tool_impl(
                    &state,
                    &plugins,
                    &session_locks,
                    &shutdown,
                    &throttle,
                    &test_overrides,
                    &session_id,
                    &tool_name,
                    arguments,
                    &bg_chat_spawn_tx,
                )
                .await;
                send(&mut writer, &resp).await?;
            }
            Request::FireHook { .. } => {
                // FireHook is only valid from the plugin ServerRequest tunnel,
                // not from direct client connections.
                send(
                    &mut writer,
                    &Response::Error {
                        message: "fire_hook is only available from plugins".into(),
                    },
                )
                .await?;
            }
            Request::Shutdown { restart } => {
                shutdown.request_shutdown(restart);
                send(&mut writer, &Response::Ok).await?;
                return Ok(());
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Compaction
// ---------------------------------------------------------------------------

/// Plugin-based tool executor for the agent loop.
struct PluginExecutor {
    plugins: Arc<Mutex<crate::plugin::PluginManager>>,
    state: SharedState,
    session_locks: SessionLocks,
    /// Channel for spawning child Chat requests (session_id, text).
    /// Received by the server to spawn async agent turns.
    chat_spawn_tx: smol::channel::Sender<(String, String)>,
    shutdown: ShutdownHandle,
    throttle: crate::throttle::ProviderThrottle,
    session_id: String,
    cwd: String,
    test_overrides: SharedTestOverrides,
}

#[async_trait::async_trait]
impl crate::worker::ToolExecutor for PluginExecutor {
    async fn execute(
        &mut self,
        tool_call: &ToolCall,
        output_tx: &smol::channel::Sender<String>,
    ) -> crate::Result<ToolResultMessage> {
        // Take the plugin handle out of the manager (brief lock).
        // This lets us execute tool I/O without holding the PluginManager lock,
        // preventing deadlocks when tools make ServerRequest calls that need
        // to interact with other sessions (which also need plugin access).
        let taken = {
            let mut pm = self.plugins.lock().unwrap();
            pm.take_tool_plugin(&self.session_id, &tool_call.name)
        };
        let (mut handle, source) = match taken {
            Some(t) => t,
            None => {
                return Err(crate::Error::Io(format!(
                    "no plugin provides tool '{}'",
                    tool_call.name
                )));
            }
        };

        // Upgrade sync pipes to async for non-blocking I/O on the executor.
        if !handle.has_async_io()
            && let Err(e) = handle.upgrade_to_async()
        {
            // Return the (broken) handle before propagating error.
            let mut pm = self.plugins.lock().unwrap();
            pm.return_tool_plugin(source, handle);
            return Err(e);
        }

        // Send tool call to plugin.
        handle
            .send_async(&crate::plugin::PluginRequest::ToolCall {
                tool_call_id: tool_call.id.clone(),
                name: tool_call.name.clone(),
                arguments: tool_call.arguments.clone(),
                cwd: Some(self.cwd.clone()),
                session_id: Some(self.session_id.clone()),
            })
            .await?;

        // Read messages from plugin until we get a ToolResult.
        let tool_call_for_hooks = tool_call.clone();
        let result = loop {
            let msg = handle.read_message_async().await?;
            match msg {
                crate::plugin::PluginMessage::OutputDelta { text, .. } => {
                    let _ = output_tx.send(text).await;
                }
                crate::plugin::PluginMessage::ToolResult(result) => {
                    break Ok(crate::types::ToolResultMessage {
                        tool_call_id: result.tool_call_id,
                        tool_name: tool_call.name.clone(),
                        content: result.content,
                        details: None,
                        is_error: result.is_error,
                        timestamp: crate::types::timestamp_ms(),
                    });
                }
                crate::plugin::PluginMessage::ServerRequest {
                    request_id,
                    request,
                } => {
                    let response = handle_server_request(
                        &self.state,
                        &self.session_locks,
                        &self.plugins,
                        &self.shutdown,
                        &self.throttle,
                        &self.chat_spawn_tx,
                        &self.test_overrides,
                        &request,
                        &self.session_id,
                    )
                    .await;
                    handle
                        .send_async(&crate::plugin::PluginRequest::ServerResponse {
                            request_id,
                            response,
                        })
                        .await?;
                }
                _ => {
                    // Ignore unexpected messages during tool execution
                }
            }
        };

        // Always return the plugin handle, even on error (brief lock).
        {
            let mut pm = self.plugins.lock().unwrap();
            pm.return_tool_plugin(source, handle);
        }

        // Run after_tool_hooks only on success.
        let mut result = result?;
        {
            let mut pm = self.plugins.lock().unwrap();
            pm.run_after_tool_hooks(&self.session_id, &tool_call_for_hooks, &mut result);
        }

        Ok(result)
    }
}

#[allow(clippy::too_many_arguments)]
fn run_agent_turn<'a, W: futures::io::AsyncWrite + Unpin + Send + 'a>(
    state: &'a SharedState,
    plugins: &'a Arc<Mutex<crate::plugin::PluginManager>>,
    shutdown: &'a ShutdownHandle,
    cancel_flag: Arc<AtomicBool>,
    model: &'a Model,
    context: &'a mut Context,
    cwd: &'a str,
    session_id: &'a str,
    writer: &'a mut W,
    throttle: &'a crate::throttle::ProviderThrottle,
    session_locks: &'a SessionLocks,
    test_overrides: &'a SharedTestOverrides,
) -> std::pin::Pin<
    Box<dyn std::future::Future<Output = crate::Result<crate::agent::AgentResult>> + Send + 'a>,
> {
    Box::pin(run_agent_turn_inner(
        state,
        plugins,
        shutdown,
        cancel_flag,
        model,
        context,
        cwd,
        session_id,
        writer,
        throttle,
        session_locks,
        test_overrides,
    ))
}

#[allow(clippy::too_many_arguments)]
async fn run_agent_turn_inner<W: futures::io::AsyncWrite + Unpin + Send>(
    state: &SharedState,
    plugins: &Arc<Mutex<crate::plugin::PluginManager>>,
    shutdown: &ShutdownHandle,
    cancel_flag: Arc<AtomicBool>,
    model: &Model,
    context: &mut Context,
    cwd: &str,
    session_id: &str,
    writer: &mut W,
    throttle: &crate::throttle::ProviderThrottle,
    session_locks: &SessionLocks,
    test_overrides: &SharedTestOverrides,
) -> crate::Result<crate::agent::AgentResult> {
    // Check provider throttle — sleep if rate limited
    if let Some(remaining) = throttle.check(&model.provider) {
        let human = crate::agent::format_duration_human(remaining.as_millis() as u64);
        eprintln!("provider '{}' throttled, waiting {}", model.provider, human);
        let msg = format!(
            "provider '{}' rate limited, retrying in {}...",
            model.provider, human
        );
        // Notify as a non-fatal status (not Error — Error would cause the TUI
        // to switch out of Streaming mode prematurely).
        let status_resp = Response::Stream {
            event: Box::new(StreamEvent::Status {
                message: msg.clone(),
            }),
        };
        send(writer, &status_resp).await.ok();
        broadcast_to_subscribers(state, session_id, &status_resp);
        // Emit rate-limited phase
        emit_phase(state, session_id, crate::types::AgentPhase::RateLimited);
        // Sleep with periodic cancellation checks
        let deadline = std::time::Instant::now() + remaining;
        while std::time::Instant::now() < deadline {
            if cancel_flag.load(Ordering::Relaxed) || shutdown.is_shutting_down() {
                return Err(crate::Error::Cancelled);
            }
            smol::Timer::after(std::time::Duration::from_secs(1)).await;
        }
    }

    let api_key = {
        let st = lock_state(state);
        resolve_api_key(&st.auth, &st.config, &model.provider)?
    };
    let api_key = match api_key {
        Some(key) => key,
        None => {
            return Err(crate::Error::NoApiKey(model.provider.clone()));
        }
    };

    let options = StreamOptions {
        api_key: Some(api_key),
        ..Default::default()
    };

    emit_phase(state, session_id, crate::types::AgentPhase::Connecting);

    let (event_tx, event_rx) = smol::channel::unbounded::<StreamEvent>();

    // Set up has_queued flag for this session
    let has_queued_flag = {
        let mut st = lock_state(state);
        st.has_queued
            .entry(session_id.to_string())
            .or_insert_with(|| Arc::new(AtomicBool::new(false)))
            .clone()
    };

    let shutdown_flag = shutdown.flag.clone();
    let cancel_flag_clone = cancel_flag.clone();
    let state_clone_persist = state.clone();
    let session_id_persist = session_id.to_string();
    let state_clone_drain = state.clone();
    let session_id_drain = session_id.to_string();
    let has_queued_clone = has_queued_flag.clone();
    let agent_config = crate::agent::AgentConfig {
        should_stop: Some(Box::new(move || {
            shutdown_flag.load(Ordering::Relaxed) || cancel_flag_clone.load(Ordering::Relaxed)
        })),
        drain_queued: Some(Box::new(move || {
            if has_queued_clone.swap(false, Ordering::Acquire) {
                let st = state_clone_drain.lock().unwrap();
                st.db
                    .drain_queued_messages(&session_id_drain)
                    .unwrap_or_default()
            } else {
                Vec::new()
            }
        })),
        on_message: Some(std::sync::Mutex::new(Box::new(move |msg: &Message| {
            let st = state_clone_persist.lock().unwrap();
            if let Err(e) = st.db.append_message(&session_id_persist, msg) {
                eprintln!("db error persisting agent message: {}", e);
            }
        }))),
        refresh_api_key: {
            let state_clone_refresh = state.clone();
            let provider_name = model.provider.clone();
            Some(Box::new(move || {
                let st = state_clone_refresh.lock().unwrap();
                resolve_api_key(&st.auth, &st.config, &provider_name)
                    .ok()
                    .flatten()
            }))
        },
        idle_timeout_secs: std::env::var("TAU_STREAM_IDLE_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(crate::agent::AgentConfig::default().idle_timeout_secs),
        ..Default::default()
    };

    let registry_clone = {
        let st = lock_state(state);
        st.registry.clone()
    };
    let child_budget = {
        let st = lock_state(state);
        st.db
            .get_session(session_id)
            .ok()
            .flatten()
            .map(|s| s.child_budget)
            .unwrap_or(0)
    };
    let plugin_tools = if !test_overrides.mock_tools.is_empty() {
        test_overrides.mock_tools.clone()
    } else {
        let pm = plugins.lock().unwrap();
        pm.tool_schemas(session_id, child_budget)
    };

    let model_clone = model.clone();
    let options_clone = options;
    let cwd_clone = cwd.to_string();
    let mut context_clone = context.clone();

    let plugins_clone = plugins.clone();
    let state_clone_exec = state.clone();
    let session_locks_clone = session_locks.clone();
    let in_flight = shutdown.clone();
    let shutdown_clone = shutdown.clone();
    let throttle_clone = throttle.clone();
    let session_id_for_executor = session_id.to_string();
    let test_overrides_clone = test_overrides.clone();

    // Channel for child Chat requests spawned by orchestration tools.
    // The receiver task spawns async agent turns for each queued chat.
    let (chat_spawn_tx, chat_spawn_rx) = smol::channel::unbounded::<(String, String)>();

    // Spawn a task that processes queued child chats.
    let spawn_state = state.clone();
    let spawn_plugins = plugins.clone();
    let spawn_shutdown = shutdown.clone();
    let spawn_session_locks = session_locks.clone();
    let spawn_throttle = throttle.clone();
    let spawn_overrides = test_overrides.clone();
    smol::spawn(async move {
        while let Ok((child_session_id, text)) = chat_spawn_rx.recv().await {
            // Each child chat gets its own async task (fire-and-forget).
            let s = spawn_state.clone();
            let p = spawn_plugins.clone();
            let sh = spawn_shutdown.clone();
            let sl = spawn_session_locks.clone();
            let th = spawn_throttle.clone();
            let ov = spawn_overrides.clone();
            smol::spawn(async move {
                let sid = child_session_id;
                if let Err(e) = run_child_chat(s, p, sh, sl, th, sid.clone(), text, ov).await {
                    eprintln!("child chat {} error: {}", sid, e);
                }
            })
            .detach();
        }
    })
    .detach();

    let agent_handle = {
        async move {
            in_flight.enter();
            let mut executor: Box<dyn crate::worker::ToolExecutor> =
                if let Some(ref factory) = test_overrides_clone.tool_executor_factory {
                    factory()
                } else {
                    Box::new(PluginExecutor {
                        plugins: plugins_clone,
                        state: state_clone_exec,
                        session_locks: session_locks_clone,
                        chat_spawn_tx,
                        shutdown: shutdown_clone,
                        throttle: throttle_clone,
                        session_id: session_id_for_executor,
                        cwd: cwd_clone,
                        test_overrides: test_overrides_clone.clone(),
                    })
                };
            let result = crate::agent::run(
                &registry_clone,
                &model_clone,
                &mut context_clone,
                &mut *executor,
                &options_clone,
                &agent_config,
                &plugin_tools,
                event_tx,
            )
            .await;
            in_flight.leave();
            result
        }
    };

    let state_clone = state.clone();
    let session_id_owned = session_id.to_string();
    let forward_handle = async {
        let mut writer_alive = true;
        while let Ok(event) = event_rx.recv().await {
            // Broadcast steering messages as UserMessage (persistence handled by on_message)
            if let StreamEvent::SteerMessage { ref message } = event {
                let text = message
                    .content
                    .iter()
                    .filter_map(|c| match c {
                        UserContent::Text(t) => Some(t.text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("");
                let user_resp = Response::UserMessage { text };
                broadcast_to_subscribers(&state_clone, &session_id_owned, &user_resp);
                if writer_alive && send(writer, &user_resp).await.is_err() {
                    writer_alive = false;
                }
                continue;
            }
            // Update stored phase from implicit stream events.
            match &event {
                StreamEvent::ThinkingStart { .. } | StreamEvent::ThinkingDelta { .. } => {
                    let mut st = state_clone.lock().unwrap();
                    st.phases
                        .insert(session_id_owned.clone(), crate::types::AgentPhase::Thinking);
                }
                StreamEvent::TextStart { .. }
                | StreamEvent::TextDelta { .. }
                | StreamEvent::ToolcallStart { .. } => {
                    let mut st = state_clone.lock().unwrap();
                    st.phases.insert(
                        session_id_owned.clone(),
                        crate::types::AgentPhase::Responding,
                    );
                }
                StreamEvent::ToolcallEnd { .. } | StreamEvent::ToolResult { .. } => {
                    let mut st = state_clone.lock().unwrap();
                    st.phases
                        .insert(session_id_owned.clone(), crate::types::AgentPhase::ToolExec);
                }
                _ => {}
            }
            let resp = Response::Stream {
                event: Box::new(event),
            };
            broadcast_to_subscribers(&state_clone, &session_id_owned, &resp);
            // Keep broadcasting even if the direct writer disconnected
            // (fire-and-forget clients close immediately).
            if writer_alive && send(writer, &resp).await.is_err() {
                writer_alive = false;
            }
        }
        Ok::<(), crate::Error>(())
    };

    let (agent_result, forward_result) = futures::future::join(agent_handle, forward_handle).await;
    if let Err(e) = forward_result {
        eprintln!("event forward error: {}", e);
    }

    let agent_result = agent_result?;

    Ok(agent_result)
}

/// Run an agent turn for a child session (spawned by orchestration tools).
/// This is a fire-and-forget async task -- output goes to subscribers only.
#[allow(clippy::too_many_arguments)]
async fn run_child_chat(
    state: SharedState,
    plugins: Arc<Mutex<crate::plugin::PluginManager>>,
    shutdown: ShutdownHandle,
    session_locks: SessionLocks,
    throttle: crate::throttle::ProviderThrottle,
    session_id: String,
    text: String,
    test_overrides: SharedTestOverrides,
) -> crate::Result<()> {
    if shutdown.is_shutting_down() {
        return Ok(());
    }

    // Acquire per-session lock
    let _session_guard = session_lock(&session_locks, &session_id).lock_arc().await;

    // Set up cancel flag
    let cancel_flag: Arc<AtomicBool> = {
        let mut st = lock_state(&state);
        let flag = st
            .cancel_flags
            .entry(session_id.clone())
            .or_insert_with(|| Arc::new(AtomicBool::new(false)));
        flag.store(false, Ordering::Relaxed);
        flag.clone()
    };

    let chat_result: Result<(bool, bool), crate::Error> = async {
        // Load session
        let (stored, mut messages, cwd) = {
            let st = lock_state(&state);
            match st.db.get_session(&session_id) {
                Ok(Some(stored)) => {
                    let messages = st.db.get_messages(&session_id)?;
                    let cwd = stored.cwd.clone().unwrap_or_else(|| "/tmp".to_string());
                    Ok((stored, messages, cwd))
                }
                Ok(None) => Err(crate::Error::Io(format!(
                    "session not found: {}",
                    session_id
                ))),
                Err(e) => Err(e),
            }
        }?;
        let model = stored.model.clone();

        // Ensure session plugins
        {
            let mut pm = plugins.lock().unwrap();
            if let Err(e) = pm.ensure_session_plugins(&session_id, &cwd) {
                eprintln!("child session {} plugin spawn error: {}", session_id, e);
            }
            pm.notify_session_start_once(&cwd, &session_id);
        }

        // Build system prompt if not set
        let system_prompt = stored.system_prompt.clone().or_else(|| {
            let pm = plugins.lock().unwrap();
            let tool_prompts = pm.tool_prompts(&session_id, stored.child_budget);
            Some(crate::system_prompt::build(
                &crate::system_prompt::PromptOptions {
                    cwd: Some(cwd.clone()),
                    tools: tool_prompts,
                    ..Default::default()
                },
            ))
        });

        // Append user message
        let user_msg = Message::User(UserMessage::text(&text));
        {
            let st = lock_state(&state);
            st.db.append_message(&session_id, &user_msg)?;
        }
        messages.push(user_msg);

        // Broadcast user message to subscribers
        broadcast_to_subscribers(
            &state,
            &session_id,
            &Response::UserMessage { text: text.clone() },
        );

        let mut context = Context {
            system_prompt,
            messages,
            tools: Vec::new(),
        };

        // Use a sink writer that discards output (no direct client connection).
        let mut sink = futures::io::sink();
        let result = run_agent_turn(
            &state,
            &plugins,
            &shutdown,
            cancel_flag.clone(),
            &model,
            &mut context,
            &cwd,
            &session_id,
            &mut sink,
            &throttle,
            &session_locks,
            &test_overrides,
        )
        .await;

        let max_turns_reached = match result {
            Ok(ref agent_result) => agent_result.max_turns_reached,
            Err(crate::Error::Cancelled) => {
                cancel_flag.store(true, Ordering::Relaxed);
                false
            }
            Err(e) => return Err(e),
        };

        Ok((cancel_flag.load(Ordering::Relaxed), max_turns_reached))
    }
    .await;

    // Broadcast terminal response and notify parent.
    match chat_result {
        Ok((true, _)) => {
            broadcast_to_subscribers(&state, &session_id, &Response::Cancelled);
            // Notify parent about cancellation.
            notify_parent_of_child_completion(
                &state,
                &session_locks,
                &plugins,
                &shutdown,
                &throttle,
                &session_id,
                "cancelled",
                None,
                &test_overrides,
            );
        }
        Ok((false, max_turns_reached)) => {
            if max_turns_reached {
                // Notify the parent session that this child hit its step limit.
                let parent_id = {
                    let st = lock_state(&state);
                    st.db
                        .get_session(&session_id)
                        .ok()
                        .flatten()
                        .and_then(|s| s.parent_id)
                };
                if let Some(pid) = parent_id {
                    let notice = format!(
                        "Child session {} reached its tool use limit. \
                         Use session_read to check progress and send a follow-up message to continue, \
                         or session_cancel to stop it.",
                        session_id
                    );
                    queue_and_maybe_resume(
                        &state,
                        &session_locks,
                        &plugins,
                        &shutdown,
                        &throttle,
                        &pid,
                        &notice,
                        &format!("child:{}", session_id),
                        &test_overrides,
                    );
                }
            } else {
                // Normal completion -- notify parent.
                notify_parent_of_child_completion(
                    &state,
                    &session_locks,
                    &plugins,
                    &shutdown,
                    &throttle,
                    &session_id,
                    "completed",
                    None,
                    &test_overrides,
                );
            }
            broadcast_to_subscribers(&state, &session_id, &Response::AgentDone);
        }
        Err(ref e) => {
            let err_msg = format!("child agent error: {}", e);
            broadcast_to_subscribers(
                &state,
                &session_id,
                &Response::Error {
                    message: err_msg.clone(),
                },
            );
            broadcast_to_subscribers(&state, &session_id, &Response::AgentDone);
            // Notify parent about error.
            notify_parent_of_child_completion(
                &state,
                &session_locks,
                &plugins,
                &shutdown,
                &throttle,
                &session_id,
                &format!("error: {}", e),
                None,
                &test_overrides,
            );
        }
    }

    emit_phase(&state, &session_id, crate::types::AgentPhase::Idle);
    notify_session_done_waiters(&state);

    // Before the session lock drops, check whether new messages arrived while
    // we were in post-turn cleanup (broadcast / notify / emit_phase).  If so,
    // spawn a resume task — it will acquire the lock as soon as we drop ours.
    maybe_respawn_for_queued(
        &state,
        &session_locks,
        &plugins,
        &shutdown,
        &throttle,
        &session_id,
        &test_overrides,
    );

    Ok(())
}

/// Resume an interrupted child session. Unlike `run_child_chat`, this does
/// not append a new user message — it just runs the agent on the existing
/// message history. Used for auto-resume on server restart.
async fn resume_child_session(
    state: SharedState,
    plugins: Arc<Mutex<crate::plugin::PluginManager>>,
    shutdown: ShutdownHandle,
    session_locks: SessionLocks,
    throttle: crate::throttle::ProviderThrottle,
    session_id: String,
    test_overrides: SharedTestOverrides,
) -> crate::Result<()> {
    if shutdown.is_shutting_down() {
        return Ok(());
    }

    // Acquire per-session lock
    let _session_guard = session_lock(&session_locks, &session_id).lock_arc().await;

    // Set up cancel flag
    let cancel_flag: Arc<AtomicBool> = {
        let mut st = lock_state(&state);
        let flag = st
            .cancel_flags
            .entry(session_id.clone())
            .or_insert_with(|| Arc::new(AtomicBool::new(false)));
        flag.store(false, Ordering::Relaxed);
        flag.clone()
    };

    let chat_result: Result<(bool, bool), crate::Error> = async {
        // Load session
        let (stored, mut messages, cwd) = {
            let st = lock_state(&state);
            match st.db.get_session(&session_id) {
                Ok(Some(stored)) => {
                    let messages = st.db.get_messages(&session_id)?;
                    let cwd = stored.cwd.clone().unwrap_or_else(|| "/tmp".to_string());
                    Ok((stored, messages, cwd))
                }
                Ok(None) => Err(crate::Error::Io(format!(
                    "session not found: {}",
                    session_id
                ))),
                Err(e) => Err(e),
            }
        }?;
        let model = stored.model.clone();

        // Ensure session plugins
        {
            let mut pm = plugins.lock().unwrap();
            if let Err(e) = pm.ensure_session_plugins(&session_id, &cwd) {
                eprintln!("resume session {} plugin spawn error: {}", session_id, e);
            }
            pm.notify_session_start_once(&cwd, &session_id);
        }

        // Repair any corrupted message history
        let repair_stubs = crate::agent::repair_messages(&messages);
        if !repair_stubs.is_empty() {
            eprintln!(
                "session {}: repaired {} missing tool_result message(s)",
                session_id,
                repair_stubs.len()
            );
            let st = lock_state(&state);
            for stub in &repair_stubs {
                if let Err(e) = st.db.append_message(&session_id, stub) {
                    eprintln!("db error persisting repair stub: {}", e);
                }
            }
            messages.extend(repair_stubs);
        }

        // Build system prompt if not set
        let system_prompt = stored.system_prompt.clone().or_else(|| {
            let pm = plugins.lock().unwrap();
            let tool_prompts = pm.tool_prompts(&session_id, stored.child_budget);
            Some(crate::system_prompt::build(
                &crate::system_prompt::PromptOptions {
                    cwd: Some(cwd.clone()),
                    tools: tool_prompts,
                    ..Default::default()
                },
            ))
        });

        // No user message appended — resume on existing messages.
        let mut context = Context {
            system_prompt,
            messages,
            tools: Vec::new(),
        };

        // Use a sink writer (no direct client connection).
        let mut sink = futures::io::sink();
        let result = run_agent_turn(
            &state,
            &plugins,
            &shutdown,
            cancel_flag.clone(),
            &model,
            &mut context,
            &cwd,
            &session_id,
            &mut sink,
            &throttle,
            &session_locks,
            &test_overrides,
        )
        .await;

        let max_turns_reached = match result {
            Ok(ref agent_result) => agent_result.max_turns_reached,
            Err(crate::Error::Cancelled) => {
                cancel_flag.store(true, Ordering::Relaxed);
                false
            }
            Err(e) => return Err(e),
        };

        Ok((cancel_flag.load(Ordering::Relaxed), max_turns_reached))
    }
    .await;

    // Broadcast terminal response and notify parent (same as run_child_chat).
    match chat_result {
        Ok((true, _)) => {
            broadcast_to_subscribers(&state, &session_id, &Response::Cancelled);
            notify_parent_of_child_completion(
                &state,
                &session_locks,
                &plugins,
                &shutdown,
                &throttle,
                &session_id,
                "cancelled",
                None,
                &test_overrides,
            );
        }
        Ok((false, max_turns_reached)) => {
            if max_turns_reached {
                let parent_id = {
                    let st = lock_state(&state);
                    st.db
                        .get_session(&session_id)
                        .ok()
                        .flatten()
                        .and_then(|s| s.parent_id)
                };
                if let Some(pid) = parent_id {
                    let notice = format!(
                        "Child session {} reached its tool use limit. \
                         Use session_read to check progress and send a follow-up message to continue, \
                         or session_cancel to stop it.",
                        session_id
                    );
                    queue_and_maybe_resume(
                        &state,
                        &session_locks,
                        &plugins,
                        &shutdown,
                        &throttle,
                        &pid,
                        &notice,
                        &format!("child:{}", session_id),
                        &test_overrides,
                    );
                }
            } else {
                notify_parent_of_child_completion(
                    &state,
                    &session_locks,
                    &plugins,
                    &shutdown,
                    &throttle,
                    &session_id,
                    "completed",
                    None,
                    &test_overrides,
                );
            }
            broadcast_to_subscribers(&state, &session_id, &Response::AgentDone);
        }
        Err(ref e) => {
            let err_msg = format!("child agent error: {}", e);
            broadcast_to_subscribers(
                &state,
                &session_id,
                &Response::Error {
                    message: err_msg.clone(),
                },
            );
            broadcast_to_subscribers(&state, &session_id, &Response::AgentDone);
            notify_parent_of_child_completion(
                &state,
                &session_locks,
                &plugins,
                &shutdown,
                &throttle,
                &session_id,
                &format!("error: {}", e),
                None,
                &test_overrides,
            );
        }
    }

    emit_phase(&state, &session_id, crate::types::AgentPhase::Idle);
    notify_session_done_waiters(&state);

    // Before the session lock drops, check whether new messages arrived while
    // we were in post-turn cleanup.  See `maybe_respawn_for_queued` doc.
    maybe_respawn_for_queued(
        &state,
        &session_locks,
        &plugins,
        &shutdown,
        &throttle,
        &session_id,
        &test_overrides,
    );

    Ok(())
}

async fn run_compaction<W: futures::io::AsyncWrite + Unpin>(
    state: &SharedState,
    session_id: &str,
    model: &Model,
    writer: &mut W,
) -> crate::Result<()> {
    emit_phase(state, session_id, crate::types::AgentPhase::Compacting);

    let settings = compaction::CompactionSettings::default();

    // Load messages and find cut point
    let (messages, cut_idx) = {
        let st = lock_state(state);
        let messages = st.db.get_messages(session_id)?;
        let cut = compaction::find_cut_point(&messages, settings.keep_recent_tokens);
        (messages, cut)
    };

    if cut_idx == 0 {
        return Ok(()); // Nothing to compact
    }

    let messages_to_summarize = &messages[..cut_idx];
    let ctx_before = compaction::estimate_context_tokens(&messages);

    // Notify client
    send(
        writer,
        &Response::Error {
            message: format!(
                "compacting session ({} messages → summary)...",
                messages_to_summarize.len()
            ),
        },
    )
    .await?;

    // Build summarization context and call LLM
    let summary_ctx = compaction::build_summarization_context(messages_to_summarize);

    let api_key = {
        let st = lock_state(state);
        resolve_api_key(&st.auth, &st.config, &model.provider)?
    };

    let options = StreamOptions {
        api_key,
        max_tokens: Some(settings.reserve_tokens),
        ..Default::default()
    };

    let rx = {
        let st = lock_state(state);
        st.registry.stream(model, &summary_ctx, &options)?
    };

    // Wait for summary (blocking on the channel)
    let summary = smol::unblock({
        let rx = rx.clone();
        move || compaction::extract_summary(&rx)
    })
    .await?;

    // Get the DB row ID of the first kept message
    let keep_from_id = {
        let st = lock_state(state);
        st.db
            .get_message_row_id(session_id, cut_idx)?
            .ok_or_else(|| crate::Error::Io("cut point message not found".into()))?
    };

    // Perform compaction in DB
    {
        let st = lock_state(state);
        st.db
            .compact_session(session_id, &summary, keep_from_id, ctx_before)?;
    }

    let after_tokens = {
        let st = lock_state(state);
        let messages = st.db.get_messages(session_id)?;
        compaction::estimate_context_tokens(&messages)
    };

    send(
        writer,
        &Response::Error {
            message: format!("compaction done: {} → {} tokens", ctx_before, after_tokens),
        },
    )
    .await?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Shared request handler helpers (used by both handle_client and
// handle_server_request)
// ---------------------------------------------------------------------------

/// Create a session (pure DB logic, no plugin setup).
#[allow(clippy::too_many_arguments)]
fn create_session_impl(
    state: &SharedState,
    model_id: &Option<String>,
    provider_name: &Option<String>,
    system_prompt: &Option<String>,
    cwd: &Option<String>,
    parent_id: &Option<String>,
    child_budget: u32,
    tagline: &Option<String>,
    auto_archive: bool,
) -> crate::protocol::Response {
    use crate::protocol::Response;
    let st = lock_state(state);

    // Budget check
    if let Some(pid) = parent_id {
        match st.db.get_session(pid) {
            Ok(Some(parent)) => {
                let used = st.db.budget_used(&parent.id).unwrap_or(0);
                let cost = 1 + child_budget;
                if used + cost > parent.child_budget {
                    return Response::Error {
                        message: format!(
                            "child budget exceeded: need {} but only {} available",
                            cost,
                            parent.child_budget.saturating_sub(used)
                        ),
                    };
                }
            }
            Ok(None) => {
                return Response::Error {
                    message: format!("parent session not found: {}", pid),
                };
            }
            Err(e) => {
                return Response::Error {
                    message: e.to_string(),
                };
            }
        }
    }

    // Load parent for inheritance
    let parent = parent_id
        .as_ref()
        .and_then(|pid| st.db.get_session(pid).ok().flatten());

    let model = match (model_id, provider_name) {
        (Some(mid), Some(prov)) => st
            .all_models
            .iter()
            .find(|m| m.id == *mid && m.provider == *prov)
            .cloned(),
        (Some(mid), None) => st.all_models.iter().find(|m| m.id == *mid).cloned(),
        _ => None,
    }
    .or_else(|| parent.as_ref().map(|p| p.model.clone()))
    .unwrap_or_else(|| st.default_model.clone());

    let cwd = cwd
        .clone()
        .or_else(|| parent.as_ref().and_then(|p| p.cwd.clone()));

    let id = match st.db.next_session_id() {
        Ok(id) => id,
        Err(e) => {
            return Response::Error {
                message: e.to_string(),
            };
        }
    };

    let is_subscription = st
        .auth
        .get(&model.provider)
        .ok()
        .flatten()
        .is_some_and(|c| matches!(c, crate::auth::AuthCredential::Oauth(_)));

    let stored = crate::db::StoredSession {
        id: id.clone(),
        model,
        system_prompt: system_prompt.clone(),
        cwd,
        is_subscription,
        created_at: crate::types::timestamp_ms() as i64,
        parent_id: parent_id.clone(),
        child_budget,
        tagline: tagline.clone(),
        archived: false,
        last_exit_status: None,
        last_phase: None,
        auto_archive,
    };
    match st.db.create_session(&stored) {
        Ok(()) => Response::SessionCreated { session_id: id },
        Err(e) => Response::Error {
            message: e.to_string(),
        },
    }
}

fn get_session_info_impl(state: &SharedState, session_id: &str) -> crate::protocol::Response {
    let st = lock_state(state);
    match st.db.get_session(session_id) {
        Ok(Some(stored)) => {
            let messages = st.db.get_messages(session_id).unwrap_or_default();
            let last_msg = st.db.last_message_time(session_id).unwrap_or(None);
            let children = st.db.child_count(session_id).unwrap_or(0);
            crate::protocol::Response::SessionInfo {
                info: session_info(
                    &stored,
                    &messages,
                    last_msg,
                    children,
                    st.phases.get(session_id),
                ),
            }
        }
        Ok(None) => crate::protocol::Response::Error {
            message: format!("session not found: {}", session_id),
        },
        Err(e) => crate::protocol::Response::Error {
            message: e.to_string(),
        },
    }
}

fn get_messages_impl(state: &SharedState, session_id: &str) -> crate::protocol::Response {
    let st = lock_state(state);
    match st.db.get_messages(session_id) {
        Ok(messages) => crate::protocol::Response::Messages { messages },
        Err(e) => crate::protocol::Response::Error {
            message: e.to_string(),
        },
    }
}

fn list_sessions_impl(state: &SharedState, include_archived: bool) -> crate::protocol::Response {
    let st = lock_state(state);
    match st.db.list_sessions(include_archived) {
        Ok(stored) => {
            let mut infos = Vec::new();
            for s in &stored {
                let db_stats = st.db.session_stats(&s.id).unwrap_or(None);
                let children = st.db.child_count(&s.id).unwrap_or(0);
                infos.push(session_info_from_db_stats(
                    s,
                    db_stats.as_ref(),
                    children,
                    st.phases.get(&s.id),
                ));
            }
            crate::protocol::Response::Sessions { sessions: infos }
        }
        Err(e) => crate::protocol::Response::Error {
            message: e.to_string(),
        },
    }
}

fn cancel_chat_impl(state: &SharedState, session_id: &str) -> crate::protocol::Response {
    let mut st = lock_state(state);
    if let Some(flag) = st.cancel_flags.get(session_id) {
        flag.store(true, Ordering::Relaxed);
    } else {
        st.cancel_flags
            .insert(session_id.to_string(), Arc::new(AtomicBool::new(true)));
    }
    crate::protocol::Response::Ok
}

/// Execute a tool directly on a session without triggering the agent loop.
/// Persists the tool call and result as messages for audit trail.
#[allow(clippy::too_many_arguments)]
async fn execute_tool_impl(
    state: &SharedState,
    plugins: &Arc<Mutex<crate::plugin::PluginManager>>,
    session_locks: &SessionLocks,
    shutdown: &ShutdownHandle,
    throttle: &crate::throttle::ProviderThrottle,
    test_overrides: &SharedTestOverrides,
    session_id: &str,
    tool_name: &str,
    arguments: serde_json::Value,
    chat_spawn_tx: &smol::channel::Sender<(String, String)>,
) -> crate::protocol::Response {
    use crate::protocol::Response;
    use crate::types::*;

    // 1. Ensure session exists, get its cwd
    let cwd = {
        let st = lock_state(state);
        match st.db.get_session(session_id) {
            Ok(Some(stored)) => stored.cwd.unwrap_or_else(|| "/tmp".to_string()),
            Ok(None) => {
                return Response::Error {
                    message: format!("session not found: {}", session_id),
                };
            }
            Err(e) => {
                return Response::Error {
                    message: e.to_string(),
                };
            }
        }
    };

    // 2. Ensure session plugins are spawned
    {
        let mut pm = plugins.lock().unwrap();
        if let Err(e) = pm.ensure_session_plugins(session_id, &cwd) {
            eprintln!("execute_tool: failed to spawn session plugins: {}", e);
        }
    }

    // 3. Construct a ToolCall with a generated ID
    let tool_call = ToolCall {
        id: format!("et_{}", crate::types::timestamp_ms()),
        name: tool_name.to_string(),
        arguments: arguments.clone(),
    };

    // 4. Persist the assistant message containing the tool call
    let assistant_msg = Message::Assistant(AssistantMessage {
        content: vec![AssistantContent::ToolCall(tool_call.clone())],
        api: "execute_tool".to_string(),
        provider: "execute_tool".to_string(),
        model: "execute_tool".to_string(),
        response_id: None,
        usage: Usage::default(),
        stop_reason: StopReason::ToolUse,
        error_message: None,
        timestamp: timestamp_ms(),
    });
    {
        let st = lock_state(state);
        if let Err(e) = st.db.append_message(session_id, &assistant_msg) {
            eprintln!("execute_tool: db error persisting assistant message: {}", e);
        }
    }

    // 5. Execute via the PluginExecutor (or mock)
    let (output_tx, _output_rx) = smol::channel::unbounded::<String>();
    let result = if let Some(ref factory) = test_overrides.tool_executor_factory {
        let mut executor = factory();
        executor.execute(&tool_call, &output_tx).await
    } else {
        let mut executor: Box<dyn crate::worker::ToolExecutor> = Box::new(PluginExecutor {
            plugins: plugins.clone(),
            state: state.clone(),
            session_locks: session_locks.clone(),
            chat_spawn_tx: chat_spawn_tx.clone(),
            shutdown: shutdown.clone(),
            throttle: throttle.clone(),
            session_id: session_id.to_string(),
            cwd: cwd.clone(),
            test_overrides: test_overrides.clone(),
        });
        executor.execute(&tool_call, &output_tx).await
    };

    // 6. Build result message and persist
    let tool_result_msg = match result {
        Ok(tr) => tr,
        Err(e) => ToolResultMessage {
            tool_call_id: tool_call.id.clone(),
            tool_name: tool_name.to_string(),
            content: vec![ToolResultContent::Text(TextContent {
                text: format!("executor error: {}", e),
                text_signature: None,
            })],
            details: None,
            is_error: true,
            timestamp: timestamp_ms(),
        },
    };

    let is_error = tool_result_msg.is_error;
    let content_text: String = tool_result_msg
        .content
        .iter()
        .filter_map(|c| match c {
            ToolResultContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");

    // Persist tool result
    {
        let st = lock_state(state);
        if let Err(e) = st
            .db
            .append_message(session_id, &Message::ToolResult(tool_result_msg))
        {
            eprintln!("execute_tool: db error persisting tool result: {}", e);
        }
    }

    Response::ToolExecuted {
        content: content_text,
        is_error,
    }
}

// ---------------------------------------------------------------------------
// Async server request handler (for plugin ServerRequest tunnel)
// ---------------------------------------------------------------------------

/// Handle a server request asynchronously (for plugin ServerRequest tunnel).
/// Only handles the subset of requests that make sense in a plugin context.
#[allow(clippy::too_many_arguments)]
async fn handle_server_request(
    state: &SharedState,
    session_locks: &SessionLocks,
    plugins: &Arc<Mutex<crate::plugin::PluginManager>>,
    shutdown: &ShutdownHandle,
    throttle: &crate::throttle::ProviderThrottle,
    chat_spawn_tx: &smol::channel::Sender<(String, String)>,
    test_overrides: &SharedTestOverrides,
    req: &crate::protocol::Request,
    session_id: &str,
) -> crate::protocol::Response {
    use crate::protocol::{Request, Response};
    match req {
        Request::CreateSession {
            model: model_id,
            provider: provider_name,
            system_prompt,
            cwd,
            parent_id,
            child_budget,
            tagline,
            auto_archive,
        } => create_session_impl(
            state,
            model_id,
            provider_name,
            system_prompt,
            cwd,
            parent_id,
            *child_budget,
            tagline,
            *auto_archive,
        ),
        Request::GetSessionInfo { session_id } => get_session_info_impl(state, session_id),
        Request::GetMessages { session_id } => get_messages_impl(state, session_id),
        Request::ListSessions { include_archived } => list_sessions_impl(state, *include_archived),
        Request::CancelChat { session_id } => cancel_chat_impl(state, session_id),
        Request::Chat { session_id, text } => {
            match chat_spawn_tx.send((session_id.clone(), text.clone())).await {
                Ok(()) => Response::Ok,
                Err(e) => Response::Error {
                    message: format!("failed to queue chat: {}", e),
                },
            }
        }
        Request::WaitSessions {
            session_ids,
            timeout_secs,
        } => {
            let deadline =
                std::time::Instant::now() + std::time::Duration::from_secs(*timeout_secs);
            let mut results = Vec::new();

            // Register a waiter channel to be notified on session completion.
            let (notify_tx, notify_rx) = smol::channel::bounded::<()>(1);
            {
                let mut st = lock_state(state);
                for sid in session_ids {
                    st.waited_sessions.insert(sid.clone());
                }
                st.session_done_waiters.push(notify_tx);
            }

            loop {
                let mut all_done = true;
                results.clear();

                for sid in session_ids {
                    let lock = session_lock(session_locks, sid);
                    let is_busy = lock.try_lock().is_none();

                    if is_busy {
                        all_done = false;
                        results.push(crate::protocol::SessionResult {
                            session_id: sid.clone(),
                            status: "busy".into(),
                            summary: String::new(),
                        });
                    } else {
                        let st = lock_state(state);
                        let (status, summary) = match st.db.get_session(sid) {
                            Ok(Some(_)) => {
                                let msgs = st.db.get_messages(sid).unwrap_or_default();
                                ("done".to_string(), last_assistant_text(&msgs))
                            }
                            _ => ("deleted".to_string(), String::new()),
                        };
                        results.push(crate::protocol::SessionResult {
                            session_id: sid.clone(),
                            status,
                            summary,
                        });
                    }
                }

                if all_done || std::time::Instant::now() >= deadline {
                    if !all_done {
                        for r in &mut results {
                            if r.status == "busy" {
                                r.status = "timeout".into();
                            }
                        }
                    }
                    break;
                }

                // Wait for a session-done notification or timeout.
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                let _ = futures::future::select(
                    std::pin::pin!(notify_rx.recv()),
                    std::pin::pin!(smol::Timer::after(remaining)),
                )
                .await;
            }

            // Remove from waited set and drop our notifier.
            {
                let mut st = lock_state(state);
                for sid in session_ids {
                    st.waited_sessions.remove(sid);
                }
            }
            drop(notify_rx);

            auto_archive_done_sessions(state, &results);
            Response::SessionsCompleted { results }
        }
        Request::WaitAnySessions {
            session_ids,
            timeout_secs,
        } => {
            let deadline =
                std::time::Instant::now() + std::time::Duration::from_secs(*timeout_secs);
            let results;

            // Register a waiter channel to be notified on session completion.
            let (notify_tx, notify_rx) = smol::channel::bounded::<()>(1);
            {
                let mut st = lock_state(state);
                for sid in session_ids {
                    st.waited_sessions.insert(sid.clone());
                }
                st.session_done_waiters.push(notify_tx);
            }

            loop {
                let mut done = Vec::new();

                for sid in session_ids {
                    let lock = session_lock(session_locks, sid);
                    let is_busy = lock.try_lock().is_none();

                    if !is_busy {
                        let st = lock_state(state);
                        let (status, summary) = match st.db.get_session(sid) {
                            Ok(Some(_)) => {
                                let msgs = st.db.get_messages(sid).unwrap_or_default();
                                ("done".to_string(), last_assistant_text(&msgs))
                            }
                            _ => ("deleted".to_string(), String::new()),
                        };
                        done.push(crate::protocol::SessionResult {
                            session_id: sid.clone(),
                            status,
                            summary,
                        });
                    }
                }

                if !done.is_empty() || std::time::Instant::now() >= deadline {
                    if done.is_empty() {
                        results = session_ids
                            .iter()
                            .map(|sid| crate::protocol::SessionResult {
                                session_id: sid.clone(),
                                status: "timeout".into(),
                                summary: String::new(),
                            })
                            .collect();
                    } else {
                        results = done;
                    }
                    break;
                }

                // Wait for a session-done notification or timeout.
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                let _ = futures::future::select(
                    std::pin::pin!(notify_rx.recv()),
                    std::pin::pin!(smol::Timer::after(remaining)),
                )
                .await;
            }

            // Remove from waited set and drop our notifier.
            {
                let mut st = lock_state(state);
                for sid in session_ids {
                    st.waited_sessions.remove(sid);
                }
            }
            drop(notify_rx);

            auto_archive_done_sessions(state, &results);
            Response::SessionsCompleted { results }
        }
        Request::QueueMessage {
            target_session_id,
            content,
            sender_info,
            await_reply,
            reply_to: _,
        } => {
            if *await_reply {
                let (msg_id, rx) = {
                    let mut st = lock_state(state);
                    st.next_msg_id += 1;
                    let id = format!("m{}", st.next_msg_id);
                    let (tx, rx) = smol::channel::bounded::<String>(1);
                    st.reply_waiters.insert(id.clone(), tx);
                    (id, rx)
                };

                let prefixed = format!(
                    "[Message from {}, msg_id={}, awaits reply]\n{}",
                    sender_info, msg_id, content
                );
                queue_and_maybe_resume(
                    state,
                    session_locks,
                    plugins,
                    shutdown,
                    throttle,
                    target_session_id,
                    &prefixed,
                    sender_info,
                    test_overrides,
                );

                // Wait with timeout.
                let timeout = std::time::Duration::from_secs(300);
                match futures::future::select(
                    std::pin::pin!(rx.recv()),
                    std::pin::pin!(smol::Timer::after(timeout)),
                )
                .await
                {
                    futures::future::Either::Left((Ok(reply), _)) => {
                        Response::MessageReply { content: reply }
                    }
                    _ => {
                        let mut st = lock_state(state);
                        st.reply_waiters.remove(&msg_id);
                        Response::Error {
                            message: format!("await_reply timed out (msg_id={})", msg_id),
                        }
                    }
                }
            } else {
                queue_and_maybe_resume(
                    state,
                    session_locks,
                    plugins,
                    shutdown,
                    throttle,
                    target_session_id,
                    content,
                    sender_info,
                    test_overrides,
                );
                Response::Ok
            }
        }
        Request::ReplyToMessage { msg_id, content } => {
            let result = {
                let mut st = lock_state(state);
                st.reply_waiters.remove(msg_id.as_str())
            };
            match result {
                Some(tx) => {
                    let _ = tx.send(content.clone()).await;
                    Response::Ok
                }
                None => Response::Error {
                    message: format!("no pending waiter for msg_id={}", msg_id),
                },
            }
        }
        Request::ArchiveSession {
            session_id,
            require_ancestor,
        } => {
            // If require_ancestor is set, verify the target is a descendant
            if let Some(ancestor) = require_ancestor {
                let st = lock_state(state);
                match st.db.is_descendant(session_id, ancestor) {
                    Ok(false) => {
                        return Response::Error {
                            message: format!(
                                "session {} is not a descendant of {}",
                                session_id, ancestor
                            ),
                        };
                    }
                    Err(e) => {
                        return Response::Error {
                            message: e.to_string(),
                        };
                    }
                    Ok(true) => {} // proceed
                }
            }

            // Validate: session must exist, get subtree IDs
            let subtree_ids = {
                let st = lock_state(state);
                match st.db.get_session(session_id) {
                    Ok(Some(_)) => match st.db.get_subtree_ids(session_id) {
                        Ok(ids) => ids,
                        Err(e) => {
                            return Response::Error {
                                message: e.to_string(),
                            };
                        }
                    },
                    Ok(None) => {
                        return Response::Error {
                            message: format!("session not found: {}", session_id),
                        };
                    }
                    Err(e) => {
                        return Response::Error {
                            message: e.to_string(),
                        };
                    }
                }
            };

            // Check all sessions in subtree are idle
            for sid in &subtree_ids {
                let lock = session_lock(session_locks, sid);
                if lock.try_lock().is_none() {
                    return Response::Error {
                        message: format!("cannot archive: session {} is busy", sid),
                    };
                }
            }

            // Archive in DB
            {
                let st = lock_state(state);
                if let Err(e) = st.db.archive_session_tree(session_id) {
                    return Response::Error {
                        message: e.to_string(),
                    };
                }
            }

            // Destroy session plugins for archived sessions
            {
                let mut pm = plugins.lock().unwrap();
                for sid in &subtree_ids {
                    pm.destroy_session_plugins(sid);
                }
            }

            // Clean up in-memory state
            {
                let mut st = lock_state(state);
                for sid in &subtree_ids {
                    st.cancel_flags.remove(sid);
                    st.has_queued.remove(sid);
                    st.subscribers.remove(sid);
                    st.phases.remove(sid);
                }
            }

            Response::SessionArchived
        }
        Request::RestoreSession { session_id } => {
            // Validate: session must exist and be archived
            let st = lock_state(state);
            match st.db.get_session(session_id) {
                Ok(Some(s)) if !s.archived => {
                    return Response::Error {
                        message: format!("session {} is not archived", session_id),
                    };
                }
                Ok(Some(_)) => {}
                Ok(None) => {
                    return Response::Error {
                        message: format!("session not found: {}", session_id),
                    };
                }
                Err(e) => {
                    return Response::Error {
                        message: e.to_string(),
                    };
                }
            }
            // Restore in DB
            if let Err(e) = st.db.restore_session_tree(session_id) {
                return Response::Error {
                    message: e.to_string(),
                };
            }
            drop(st);
            Response::SessionRestored
        }
        Request::FireHook { name, data } => {
            let mut pm = plugins.lock().unwrap();
            pm.call_hook_excluding(session_id, name, data, None);
            Response::Ok
        }
        Request::ExecuteTool {
            session_id: target_session_id,
            tool_name,
            arguments,
        } => {
            execute_tool_impl(
                state,
                plugins,
                session_locks,
                shutdown,
                throttle,
                test_overrides,
                target_session_id,
                tool_name,
                arguments.clone(),
                chat_spawn_tx,
            )
            .await
        }
        _ => Response::Error {
            message: "request not supported in plugin context".into(),
        },
    }
}

/// Notify a child session's parent that the child has completed.
/// Skipped if the parent is actively waiting on this child via WaitSessions/WaitAnySessions.
#[allow(clippy::too_many_arguments)]
fn notify_parent_of_child_completion(
    state: &SharedState,
    session_locks: &SessionLocks,
    plugins: &Arc<Mutex<crate::plugin::PluginManager>>,
    shutdown: &ShutdownHandle,
    throttle: &crate::throttle::ProviderThrottle,
    child_session_id: &str,
    status: &str,
    error_detail: Option<&str>,
    test_overrides: &SharedTestOverrides,
) {
    let (parent_id, summary) = {
        let st = lock_state(state);
        // Check if parent is already waiting on this child.
        if st.waited_sessions.contains(child_session_id) {
            return;
        }
        let parent = st
            .db
            .get_session(child_session_id)
            .ok()
            .flatten()
            .and_then(|s| s.parent_id);
        let summary = st
            .db
            .get_messages(child_session_id)
            .ok()
            .map(|msgs| {
                let text = last_assistant_text(&msgs);
                if text.len() > 200 {
                    format!("{}...", truncate_str(&text, 200))
                } else {
                    text
                }
            })
            .unwrap_or_default();
        (parent, summary)
    };

    let pid = match parent_id {
        Some(pid) => pid,
        None => return,
    };

    let _ = error_detail; // reserved for future use
    let notice = format!(
        "Child session {} {}. Summary: {}",
        child_session_id,
        status,
        if summary.is_empty() {
            "(no output)".to_string()
        } else {
            summary
        }
    );

    queue_and_maybe_resume(
        state,
        session_locks,
        plugins,
        shutdown,
        throttle,
        &pid,
        &notice,
        &format!("child:{}", child_session_id),
        test_overrides,
    );
}

fn last_assistant_text(messages: &[Message]) -> String {
    for msg in messages.iter().rev() {
        if let Message::Assistant(a) = msg {
            let text: String = a
                .content
                .iter()
                .filter_map(|c| match c {
                    crate::types::AssistantContent::Text(t) if !t.text.is_empty() => {
                        Some(t.text.as_str())
                    }
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            if !text.is_empty() {
                // Truncate to ~500 chars for summary
                return if text.len() > 500 {
                    format!("{}...", truncate_str(&text, 500))
                } else {
                    text
                };
            }
        }
    }
    String::new()
}

/// Update the session's phase and broadcast a Phase event to subscribers.
/// Also persists the phase to DB for meaningful transitions so it survives restarts.
fn emit_phase(state: &SharedState, session_id: &str, phase: crate::types::AgentPhase) {
    {
        let mut st = lock_state(state);
        st.phases.insert(session_id.to_string(), phase);
        // Persist meaningful phase transitions to DB.
        let label = phase.label().trim_end_matches("...");
        if let Err(e) = st.db.update_phase(session_id, label) {
            eprintln!("warning: failed to persist phase for {}: {}", session_id, e);
        }
    }
    let resp = Response::Stream {
        event: Box::new(crate::types::StreamEvent::Phase { phase }),
    };
    broadcast_to_subscribers(state, session_id, &resp);
}

/// Wake all registered session-done waiters so they re-check completion.
fn notify_session_done_waiters(state: &SharedState) {
    let mut st = lock_state(state);
    st.session_done_waiters.retain(|tx| {
        // Try to send; drop closed channels.
        !tx.is_closed() && {
            let _ = tx.try_send(());
            true
        }
    });
}

fn broadcast_to_subscribers(state: &SharedState, session_id: &str, resp: &Response) {
    let mut st = lock_state(state);
    if let Some(subs) = st.subscribers.get_mut(session_id) {
        subs.retain(|tx| {
            match tx.try_send(resp.clone()) {
                Ok(()) => true,
                Err(smol::channel::TrySendError::Closed(_)) => false,
                Err(smol::channel::TrySendError::Full(_)) => {
                    eprintln!("warning: subscriber channel full, dropping message");
                    true // keep subscriber, just drop this message
                }
            }
        });
        if subs.is_empty() {
            st.subscribers.remove(session_id);
        }
    }
}

async fn send<W: futures::io::AsyncWrite + Unpin>(
    writer: &mut W,
    resp: &Response,
) -> crate::Result<()> {
    let mut line = serde_json::to_string(resp).map_err(|e| crate::Error::Parse(e.to_string()))?;
    line.push('\n');
    writer
        .write_all(line.as_bytes())
        .await
        .map_err(|e| crate::Error::Io(e.to_string()))?;
    writer
        .flush()
        .await
        .map_err(|e| crate::Error::Io(e.to_string()))?;
    Ok(())
}

/// Auto-archive completed sessions that have `auto_archive=true`.
/// Called after WaitSessions/WaitAnySessions collects results.
fn auto_archive_done_sessions(state: &SharedState, results: &[crate::protocol::SessionResult]) {
    let st = lock_state(state);
    for r in results {
        if r.status != "done" {
            continue;
        }
        let should_archive = st
            .db
            .get_session(&r.session_id)
            .ok()
            .flatten()
            .is_some_and(|s| s.auto_archive);
        if should_archive && let Err(e) = st.db.archive_session_tree(&r.session_id) {
            eprintln!("auto-archive session {} failed: {}", r.session_id, e);
        }
    }
}

/// Restore `State.phases` from persisted `last_phase` values in the database.
/// Called once at startup so sessions show their last-known state instead of
/// defaulting to "idle".
fn restore_phases_from_db(state: &SharedState) {
    let mut st = lock_state(state);
    let sessions = match st.db.list_sessions(false) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("warning: failed to load sessions for phase restore: {}", e);
            return;
        }
    };
    for s in sessions {
        if let Some(ref phase_str) = s.last_phase {
            let phase = match phase_str.as_str() {
                "idle" => crate::types::AgentPhase::Idle,
                "thinking" => crate::types::AgentPhase::Thinking,
                "working" => crate::types::AgentPhase::Responding,
                "running tools" => crate::types::AgentPhase::ToolExec,
                "sending request" => crate::types::AgentPhase::Connecting,
                "preparing" => crate::types::AgentPhase::Preparing,
                "compacting" => crate::types::AgentPhase::Compacting,
                "rate limited" => crate::types::AgentPhase::RateLimited,
                "waiting" => crate::types::AgentPhase::Waiting,
                _ => crate::types::AgentPhase::Idle,
            };
            if phase != crate::types::AgentPhase::Idle {
                st.phases.insert(s.id.clone(), phase);
            }
        }
    }
}

fn prepare_socket_dir(sock: &Path) -> crate::Result<()> {
    if let Some(parent) = sock.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| crate::Error::Io(format!("mkdir {}: {}", parent.display(), e)))?;
    }
    Ok(())
}
