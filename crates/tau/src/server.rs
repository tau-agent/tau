//! Unix socket server — manages sessions and streams LLM responses.

use std::collections::HashMap;
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
) -> SessionInfo {
    SessionInfo {
        id: stored.id.clone(),
        model: stored.model.id.clone(),
        provider: stored.model.provider.clone(),
        cwd: stored.cwd.clone(),
        message_count: messages.len(),
        stats: compute_stats(messages, &stored.model, stored.is_subscription),
        // Timestamps in DB are milliseconds; convert to seconds for display
        last_activity: last_message_time.unwrap_or(stored.created_at) / 1000,
        parent_id: stored.parent_id.clone(),
        child_count,
        child_budget: stored.child_budget,
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
    /// Per-session steering channels for injecting messages into running agent loops.
    steer_txs: HashMap<String, smol::channel::Sender<String>>,
    /// Per-session broadcast subscribers.
    /// Other clients watching a session receive streamed responses.
    subscribers: HashMap<String, Vec<smol::channel::Sender<Response>>>,
}

type SharedState = Arc<Mutex<State>>;

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
    registry
}

/// Configuration for a test server instance.
pub struct TestServerConfig {
    pub registry: ProviderRegistry,
    pub models: Vec<Model>,
    pub socket_path: PathBuf,
    pub db_path: PathBuf,
}

/// Run a server with custom config (for testing).
pub async fn run_with_config(config: TestServerConfig) -> crate::Result<()> {
    let default_model = config
        .models
        .first()
        .cloned()
        .ok_or_else(|| crate::Error::Io("no models available".into()))?;
    let sock = &config.socket_path;

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
    let plugins_config = crate::plugin::PluginsConfig {
        no_default_worker: true,
        ..Default::default()
    };
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
        subscribers: HashMap::new(),
        steer_txs: HashMap::new(),
    }));

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
        smol::spawn(async move {
            if let Err(e) = handle_client(
                stream,
                state,
                plugins,
                shutdown_handle,
                session_locks,
                throttle,
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
        steer_txs: HashMap::new(),
        subscribers: HashMap::new(),
    }));

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
        smol::spawn(async move {
            if let Err(e) = handle_client(
                stream,
                state,
                plugins,
                shutdown_handle,
                session_locks,
                throttle,
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

async fn handle_client(
    stream: Async<UnixStream>,
    state: SharedState,
    plugins: Arc<Mutex<crate::plugin::PluginManager>>,
    shutdown: ShutdownHandle,
    session_locks: SessionLocks,
    throttle: crate::throttle::ProviderThrottle,
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
            } => {
                // Budget check (before acquiring state lock for creation)
                let budget_error = {
                    let st = state.lock().unwrap();
                    if let Some(ref pid) = parent_id {
                        match st.db.get_session(pid)? {
                            Some(parent) => {
                                let used = st.db.budget_used(&parent.id)?;
                                let cost = 1 + child_budget;
                                if used + cost > parent.child_budget {
                                    Some(format!(
                                        "child budget exceeded: need {} but only {} available (budget={}, used={})",
                                        cost,
                                        parent.child_budget.saturating_sub(used),
                                        parent.child_budget,
                                        used
                                    ))
                                } else {
                                    None
                                }
                            }
                            None => Some(format!("parent session not found: {}", pid)),
                        }
                    } else {
                        None
                    }
                };
                if let Some(msg) = budget_error {
                    send(&mut writer, &Response::Error { message: msg }).await?;
                    continue;
                }

                let result = {
                    let st = state.lock().unwrap();

                    // Load parent for inheritance
                    let parent = match &parent_id {
                        Some(pid) => st.db.get_session(pid)?,
                        None => None,
                    };

                    // Model inheritance: None = inherit parent's model
                    let model = match (&model_id, &provider_name) {
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

                    // CWD inheritance: None = inherit parent's cwd
                    let cwd = cwd.or_else(|| parent.as_ref().and_then(|p| p.cwd.clone()));

                    let is_subscription = st
                        .auth
                        .get(&model.provider)
                        .ok()
                        .flatten()
                        .is_some_and(|c| matches!(c, AuthCredential::Oauth(_)));
                    let id = st.db.next_session_id()?;
                    let system_prompt = system_prompt.or_else(|| {
                        let mut pm = plugins.lock().unwrap();
                        let cwd_str = cwd.as_deref().unwrap_or("/tmp");
                        if let Err(e) = pm.ensure_session_plugins(&id, cwd_str) {
                            eprintln!("failed to spawn session plugins: {}", e);
                        }
                        let tool_prompts = pm.tool_prompts(&id, child_budget);
                        Some(crate::system_prompt::build(
                            &crate::system_prompt::PromptOptions {
                                cwd: cwd.clone(),
                                tools: tool_prompts,
                                ..Default::default()
                            },
                        ))
                    });
                    let stored = StoredSession {
                        id: id.clone(),
                        model,
                        system_prompt,
                        cwd,
                        is_subscription,
                        created_at: crate::types::timestamp_ms() as i64,
                        parent_id: parent_id.clone(),
                        child_budget,
                    };
                    st.db.create_session(&stored)?;
                    Ok::<String, crate::Error>(id)
                };
                match result {
                    Ok(id) => {
                        send(&mut writer, &Response::SessionCreated { session_id: id }).await?;
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
            Request::GetSessionInfo { session_id } => {
                let result = {
                    let st = state.lock().unwrap();
                    match st.db.get_session(&session_id) {
                        Ok(Some(stored)) => {
                            let messages = st.db.get_messages(&session_id)?;
                            let last_msg = st.db.last_message_time(&session_id)?;
                            let children = st.db.child_count(&session_id)?;
                            Ok(session_info(&stored, &messages, last_msg, children))
                        }
                        Ok(None) => Err(format!("session not found: {}", session_id)),
                        Err(e) => Err(format!("db error: {}", e)),
                    }
                };
                match result {
                    Ok(info) => {
                        send(&mut writer, &Response::SessionInfo { info }).await?;
                    }
                    Err(msg) => {
                        send(&mut writer, &Response::Error { message: msg }).await?;
                    }
                }
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
                let _session_guard = session_lock(&session_locks, &session_id).lock_arc().await;

                // Reset (and create) the cancel flag for this session.
                let cancel_flag: Arc<AtomicBool> = {
                    let mut st = state.lock().unwrap();
                    let flag = st
                        .cancel_flags
                        .entry(session_id.clone())
                        .or_insert_with(|| Arc::new(AtomicBool::new(false)));
                    flag.store(false, Ordering::Relaxed);
                    flag.clone()
                };

                // Run the Chat handler body inside a closure so that any
                // error is caught and we *always* broadcast a terminal
                // response (AgentDone / Cancelled / Error) to subscribers.
                // Without this guarantee the TUI gets stuck in Streaming
                // mode forever when an internal error (e.g. DB write)
                // causes the handler to bail out early via `?`.
                let chat_result: Result<bool, crate::Error> = async {
                    // Load session
                    let session_data = {
                        let st = state.lock().unwrap();
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
                        let st = state.lock().unwrap();
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
                        )
                        .await;
                        match cont_result {
                            Ok(_new_msgs) => {
                                // Messages already persisted incrementally via on_message
                                let st = state.lock().unwrap();
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
                                    let st = state.lock().unwrap();
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
                        let st = state.lock().unwrap();
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
                    )
                    .await;

                    match result {
                        Ok(_new_msgs) => {
                            // Messages already persisted incrementally via on_message
                        }
                        Err(crate::Error::Cancelled) => {
                            cancel_flag.store(true, Ordering::Relaxed);
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
                    }

                    // Check compaction
                    let was_cancelled = cancel_flag.load(Ordering::Relaxed);
                    if !was_cancelled {
                        let should = {
                            let st = state.lock().unwrap();
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

                    Ok(was_cancelled)
                }
                .await;

                // Always broadcast a terminal response so subscribers
                // (especially the TUI) never get stuck in Streaming mode.
                match chat_result {
                    Ok(true) => {
                        // Cancelled
                        let resp = Response::Cancelled;
                        broadcast_to_subscribers(&state, &session_id, &resp);
                        send(&mut writer, &resp).await.ok();
                    }
                    Ok(false) => {
                        // Normal completion
                        let resp = Response::AgentDone;
                        broadcast_to_subscribers(&state, &session_id, &resp);
                        send(&mut writer, &resp).await.ok();
                    }
                    Err(e) => {
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
                    let mut st = state.lock().unwrap();
                    st.subscribers
                        .entry(session_id.clone())
                        .or_default()
                        .push(tx);
                }

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
                let messages = {
                    let st = state.lock().unwrap();
                    st.db.get_messages(&session_id)?
                };
                send(&mut writer, &Response::Messages { messages }).await?;
            }
            Request::CancelChat { session_id } => {
                {
                    let mut st = state.lock().unwrap();
                    if let Some(flag) = st.cancel_flags.get(&session_id) {
                        flag.store(true, Ordering::Relaxed);
                    } else {
                        // No active chat — create a pre-set flag so the next Chat
                        // is immediately cancelled (race-free).
                        st.cancel_flags
                            .insert(session_id, Arc::new(AtomicBool::new(true)));
                    }
                } // lock released before await
                send(&mut writer, &Response::Ok).await.ok();
            }
            Request::Steer { session_id, text } => {
                let sent = {
                    let st = state.lock().unwrap();
                    if let Some(tx) = st.steer_txs.get(&session_id) {
                        tx.try_send(text.clone()).is_ok()
                    } else {
                        false
                    }
                };
                if sent {
                    // Steering message will be picked up by the agent loop
                    send(&mut writer, &Response::Ok).await.ok();
                } else {
                    // No active agent loop — treat as a regular chat message
                    // Re-dispatch as Chat by pushing it back. We handle it inline
                    // to avoid recursion: just send an error telling the client
                    // to send a Chat instead.
                    send(
                        &mut writer,
                        &Response::Error {
                            message: "no active agent loop, use Chat instead".into(),
                        },
                    )
                    .await
                    .ok();
                }
            }
            Request::ListSessions => {
                let sessions = {
                    let st = state.lock().unwrap();
                    let stored = st.db.list_sessions()?;
                    let mut infos = Vec::with_capacity(stored.len());
                    for s in &stored {
                        let messages = st.db.get_messages(&s.id)?;
                        let last_msg = st.db.last_message_time(&s.id)?;
                        let children = st.db.child_count(&s.id)?;
                        infos.push(session_info(s, &messages, last_msg, children));
                    }
                    infos
                };
                send(&mut writer, &Response::Sessions { sessions }).await?;
            }
            Request::DeleteSession { session_id } => {
                {
                    let st = state.lock().unwrap();
                    // Delete session and all descendants
                    st.db.delete_session_tree(&session_id)?;
                }
                // Clean up session plugins for deleted sessions
                {
                    let mut pm = plugins.lock().unwrap();
                    pm.destroy_session_plugins(&session_id);
                }
                send(&mut writer, &Response::SessionDeleted).await?;
            }
            Request::ListModels => {
                let models = {
                    let st = state.lock().unwrap();
                    st.all_models.iter().map(model_info).collect::<Vec<_>>()
                };
                send(&mut writer, &Response::Models { models }).await?;
            }
            Request::SetCwd { session_id, cwd } => {
                let result = {
                    let st = state.lock().unwrap();
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
                    let st = state.lock().unwrap();
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
                            let st = state.lock().unwrap();
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
                    let st = state.lock().unwrap();
                    st.auth.list().unwrap_or_default()
                };
                send(&mut writer, &Response::AuthStatus { providers }).await?;
            }
            Request::GetSubscriptionUsage => {
                // Check cache, fetch if stale
                let cache_result = {
                    let st = state.lock().unwrap();
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
                        let st = state.lock().unwrap();
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
                            let mut st = state.lock().unwrap();
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
                            let st = state.lock().unwrap();
                            let summary = match st.db.get_session(sid) {
                                Ok(Some(_)) => {
                                    let msgs = st.db.get_messages(sid).unwrap_or_default();
                                    last_assistant_text(&msgs)
                                }
                                _ => String::new(),
                            };
                            results.push(crate::protocol::SessionResult {
                                session_id: sid.clone(),
                                status: "done".into(),
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

                    // Poll every second
                    smol::Timer::after(std::time::Duration::from_secs(1)).await;
                }

                send(&mut writer, &Response::SessionsCompleted { results }).await?;
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
    session_id: String,
    cwd: String,
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

        // Execute tool I/O without holding the PluginManager lock.
        let state = self.state.clone();
        let session_locks = self.session_locks.clone();
        let chat_spawn_tx = self.chat_spawn_tx.clone();
        let session_id = self.session_id.clone();
        let cwd = self.cwd.clone();
        let tool_call = tool_call.clone();
        let tool_call_for_hooks = tool_call.clone();
        let output_tx_clone = output_tx.clone();

        let (result, handle) = smol::unblock(move || {
            let mut server_handler =
                move |req: &crate::protocol::Request| -> crate::protocol::Response {
                    handle_server_request_sync(&state, &session_locks, &chat_spawn_tx, req)
                };
            let mut output_fn = |delta: &str| {
                let _ = output_tx_clone.send_blocking(delta.to_string());
            };
            let result = handle.execute_tool_with_server(
                &tool_call,
                Some(&cwd),
                Some(&session_id),
                &mut output_fn,
                Some(&mut server_handler),
            );
            (result, handle)
        })
        .await;

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
) -> std::pin::Pin<Box<dyn std::future::Future<Output = crate::Result<Vec<Message>>> + Send + 'a>> {
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
) -> crate::Result<Vec<Message>> {
    // Check provider throttle — sleep if rate limited
    if let Some(remaining) = throttle.check(&model.provider) {
        let secs = remaining.as_secs();
        eprintln!("provider '{}' throttled, waiting {}s", model.provider, secs);
        let msg = format!(
            "provider '{}' rate limited, retrying in {}s...",
            model.provider, secs
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
        let st = state.lock().unwrap();
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

    let (event_tx, event_rx) = smol::channel::unbounded::<StreamEvent>();

    // Create steering channel for this session's agent loop
    let (steer_tx, steer_rx) = smol::channel::bounded::<String>(16);
    {
        let mut st = state.lock().unwrap();
        st.steer_txs.insert(session_id.to_string(), steer_tx);
    }

    let shutdown_flag = shutdown.flag.clone();
    let cancel_flag_clone = cancel_flag.clone();
    let state_clone_persist = state.clone();
    let session_id_persist = session_id.to_string();
    let agent_config = crate::agent::AgentConfig {
        should_stop: Some(Box::new(move || {
            shutdown_flag.load(Ordering::Relaxed) || cancel_flag_clone.load(Ordering::Relaxed)
        })),
        steer_rx: Some(steer_rx),
        on_message: Some(std::sync::Mutex::new(Box::new(move |msg: &Message| {
            let st = state_clone_persist.lock().unwrap();
            if let Err(e) = st.db.append_message(&session_id_persist, msg) {
                eprintln!("db error persisting agent message: {}", e);
            }
        }))),
        ..Default::default()
    };

    let registry_clone = {
        let st = state.lock().unwrap();
        st.registry.clone()
    };
    let child_budget = {
        let st = state.lock().unwrap();
        st.db
            .get_session(session_id)
            .ok()
            .flatten()
            .map(|s| s.child_budget)
            .unwrap_or(0)
    };
    let plugin_tools = {
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
    let session_id_for_executor = session_id.to_string();

    // Channel for child Chat requests spawned by orchestration tools.
    // The receiver task spawns async agent turns for each queued chat.
    let (chat_spawn_tx, chat_spawn_rx) = smol::channel::unbounded::<(String, String)>();

    // Spawn a task that processes queued child chats.
    let spawn_state = state.clone();
    let spawn_plugins = plugins.clone();
    let spawn_shutdown = shutdown.clone();
    let spawn_session_locks = session_locks.clone();
    let spawn_throttle = throttle.clone();
    smol::spawn(async move {
        while let Ok((child_session_id, text)) = chat_spawn_rx.recv().await {
            // Each child chat gets its own async task (fire-and-forget).
            let s = spawn_state.clone();
            let p = spawn_plugins.clone();
            let sh = spawn_shutdown.clone();
            let sl = spawn_session_locks.clone();
            let th = spawn_throttle.clone();
            smol::spawn(async move {
                let sid = child_session_id;
                if let Err(e) = run_child_chat(s, p, sh, sl, th, sid.clone(), text).await {
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
            let mut executor = PluginExecutor {
                plugins: plugins_clone,
                state: state_clone_exec,
                session_locks: session_locks_clone,
                chat_spawn_tx,
                session_id: session_id_for_executor,
                cwd: cwd_clone,
            };
            let result = crate::agent::run(
                &registry_clone,
                &model_clone,
                &mut context_clone,
                &mut executor,
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

    // Clean up steering channel
    {
        let mut st = state.lock().unwrap();
        st.steer_txs.remove(session_id);
    }

    Ok(agent_result.new_messages)
}

/// Run an agent turn for a child session (spawned by orchestration tools).
/// This is a fire-and-forget async task -- output goes to subscribers only.
async fn run_child_chat(
    state: SharedState,
    plugins: Arc<Mutex<crate::plugin::PluginManager>>,
    shutdown: ShutdownHandle,
    session_locks: SessionLocks,
    throttle: crate::throttle::ProviderThrottle,
    session_id: String,
    text: String,
) -> crate::Result<()> {
    if shutdown.is_shutting_down() {
        return Ok(());
    }

    // Acquire per-session lock
    let _session_guard = session_lock(&session_locks, &session_id).lock_arc().await;

    // Set up cancel flag
    let cancel_flag: Arc<AtomicBool> = {
        let mut st = state.lock().unwrap();
        let flag = st
            .cancel_flags
            .entry(session_id.clone())
            .or_insert_with(|| Arc::new(AtomicBool::new(false)));
        flag.store(false, Ordering::Relaxed);
        flag.clone()
    };

    let chat_result: Result<bool, crate::Error> = async {
        // Load session
        let (stored, mut messages, cwd) = {
            let st = state.lock().unwrap();
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
            let st = state.lock().unwrap();
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
        )
        .await;

        match result {
            Ok(_) => {}
            Err(crate::Error::Cancelled) => {
                cancel_flag.store(true, Ordering::Relaxed);
            }
            Err(e) => return Err(e),
        }

        Ok(cancel_flag.load(Ordering::Relaxed))
    }
    .await;

    // Broadcast terminal response
    match chat_result {
        Ok(true) => {
            broadcast_to_subscribers(&state, &session_id, &Response::Cancelled);
        }
        Ok(false) => {
            broadcast_to_subscribers(&state, &session_id, &Response::AgentDone);
        }
        Err(e) => {
            broadcast_to_subscribers(
                &state,
                &session_id,
                &Response::Error {
                    message: format!("child agent error: {}", e),
                },
            );
            broadcast_to_subscribers(&state, &session_id, &Response::AgentDone);
        }
    }

    Ok(())
}

async fn run_compaction<W: futures::io::AsyncWrite + Unpin>(
    state: &SharedState,
    session_id: &str,
    model: &Model,
    writer: &mut W,
) -> crate::Result<()> {
    let settings = compaction::CompactionSettings::default();

    // Load messages and find cut point
    let (messages, cut_idx) = {
        let st = state.lock().unwrap();
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
        let st = state.lock().unwrap();
        resolve_api_key(&st.auth, &st.config, &model.provider)?
    };

    let options = StreamOptions {
        api_key,
        max_tokens: Some(settings.reserve_tokens),
        ..Default::default()
    };

    let rx = {
        let st = state.lock().unwrap();
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
        let st = state.lock().unwrap();
        st.db
            .get_message_row_id(session_id, cut_idx)?
            .ok_or_else(|| crate::Error::Io("cut point message not found".into()))?
    };

    // Perform compaction in DB
    {
        let st = state.lock().unwrap();
        st.db
            .compact_session(session_id, &summary, keep_from_id, ctx_before)?;
    }

    let after_tokens = {
        let st = state.lock().unwrap();
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

/// Broadcast a response to all subscribers of a session.
/// Removes disconnected subscribers.
/// Extract the last assistant message text from a message list.
/// Handle a server request synchronously (for plugin ServerRequest tunnel).
/// Only handles a subset of requests that make sense in a plugin context.
fn handle_server_request_sync(
    state: &SharedState,
    _session_locks: &SessionLocks,
    chat_spawn_tx: &smol::channel::Sender<(String, String)>,
    req: &crate::protocol::Request,
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
        } => {
            let st = state.lock().unwrap();

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
                child_budget: *child_budget,
            };
            match st.db.create_session(&stored) {
                Ok(()) => Response::SessionCreated { session_id: id },
                Err(e) => Response::Error {
                    message: e.to_string(),
                },
            }
        }
        Request::GetSessionInfo { session_id } => {
            let st = state.lock().unwrap();
            match st.db.get_session(session_id) {
                Ok(Some(stored)) => {
                    let messages = st.db.get_messages(session_id).unwrap_or_default();
                    let last_msg = st.db.last_message_time(session_id).unwrap_or(None);
                    let children = st.db.child_count(session_id).unwrap_or(0);
                    Response::SessionInfo {
                        info: session_info(&stored, &messages, last_msg, children),
                    }
                }
                Ok(None) => Response::Error {
                    message: format!("session not found: {}", session_id),
                },
                Err(e) => Response::Error {
                    message: e.to_string(),
                },
            }
        }
        Request::GetMessages { session_id } => {
            let st = state.lock().unwrap();
            match st.db.get_messages(session_id) {
                Ok(messages) => Response::Messages { messages },
                Err(e) => Response::Error {
                    message: e.to_string(),
                },
            }
        }
        Request::ListSessions => {
            let st = state.lock().unwrap();
            match st.db.list_sessions() {
                Ok(stored) => {
                    let mut infos = Vec::new();
                    for s in &stored {
                        let messages = st.db.get_messages(&s.id).unwrap_or_default();
                        let last_msg = st.db.last_message_time(&s.id).unwrap_or(None);
                        let children = st.db.child_count(&s.id).unwrap_or(0);
                        infos.push(session_info(s, &messages, last_msg, children));
                    }
                    Response::Sessions { sessions: infos }
                }
                Err(e) => Response::Error {
                    message: e.to_string(),
                },
            }
        }
        Request::CancelChat { session_id } => {
            let mut st = state.lock().unwrap();
            if let Some(flag) = st.cancel_flags.get(session_id) {
                flag.store(true, Ordering::Relaxed);
            } else {
                st.cancel_flags
                    .insert(session_id.clone(), Arc::new(AtomicBool::new(true)));
            }
            Response::Ok
        }
        Request::Chat { session_id, text } => {
            // Queue a Chat request for the server to process asynchronously.
            // The chat_spawn channel is received by the server's main loop
            // which spawns an agent turn task for the child session.
            match chat_spawn_tx.send_blocking((session_id.clone(), text.clone())) {
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
            // Synchronous poll -- check if sessions are idle via session locks.
            // A session is "done" if its async lock is not held (no active Chat).
            let deadline =
                std::time::Instant::now() + std::time::Duration::from_secs(*timeout_secs);
            loop {
                let mut all_done = true;
                let mut results = Vec::new();
                for sid in session_ids {
                    let lock = session_lock(_session_locks, sid);
                    let is_busy = lock.try_lock().is_none();

                    if is_busy {
                        all_done = false;
                        results.push(crate::protocol::SessionResult {
                            session_id: sid.clone(),
                            status: "busy".into(),
                            summary: String::new(),
                        });
                    } else {
                        let st = state.lock().unwrap();
                        let summary = match st.db.get_messages(sid) {
                            Ok(msgs) => last_assistant_text(&msgs),
                            Err(_) => String::new(),
                        };
                        drop(st);
                        results.push(crate::protocol::SessionResult {
                            session_id: sid.clone(),
                            status: "done".into(),
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
                    return Response::SessionsCompleted { results };
                }
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
        }
        _ => Response::Error {
            message: "request not supported in plugin context".into(),
        },
    }
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
                    format!("{}...", &text[..500])
                } else {
                    text
                };
            }
        }
    }
    String::new()
}

fn broadcast_to_subscribers(state: &SharedState, session_id: &str, resp: &Response) {
    let mut st = state.lock().unwrap();
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

fn prepare_socket_dir(sock: &Path) -> crate::Result<()> {
    if let Some(parent) = sock.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| crate::Error::Io(format!("mkdir {}: {}", parent.display(), e)))?;
    }
    Ok(())
}
