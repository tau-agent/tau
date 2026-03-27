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

fn session_info(stored: &StoredSession, messages: &[Message]) -> SessionInfo {
    SessionInfo {
        id: stored.id.clone(),
        model: stored.model.id.clone(),
        provider: stored.model.provider.clone(),
        cwd: stored.cwd.clone(),
        message_count: messages.len(),
        stats: compute_stats(messages, &stored.model, stored.is_subscription),
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
    /// Per-session broadcast subscribers.
    /// Other clients watching a session receive streamed responses.
    subscribers: HashMap<String, Vec<smol::channel::Sender<Response>>>,
}

type SharedState = Arc<Mutex<State>>;

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
            let _ = tx.try_send(msg.clone());
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

    let state: SharedState = Arc::new(Mutex::new(State {
        db,
        registry,
        auth: AuthStorage::open_default(),
        config: cfg,
        default_model,
        all_models,
        usage_cache: None,
        cancel_flags: HashMap::new(),
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
        let shutdown_handle = shutdown.clone();
        smol::spawn(async move {
            if let Err(e) = handle_client(stream, state, shutdown_handle).await {
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
    shutdown: ShutdownHandle,
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
            } => {
                let result = {
                    let st = state.lock().unwrap();
                    // Find requested model, or fall back to default
                    let model = match (&model_id, &provider_name) {
                        (Some(mid), Some(prov)) => st
                            .all_models
                            .iter()
                            .find(|m| m.id == *mid && m.provider == *prov)
                            .cloned(),
                        (Some(mid), None) => st.all_models.iter().find(|m| m.id == *mid).cloned(),
                        _ => None,
                    }
                    .unwrap_or_else(|| st.default_model.clone());

                    let is_subscription = st
                        .auth
                        .get(&model.provider)
                        .ok()
                        .flatten()
                        .is_some_and(|c| matches!(c, AuthCredential::Oauth(_)));
                    let id = st.db.next_session_id()?;
                    let system_prompt =
                        system_prompt.or_else(|| Some(crate::system_prompt::build(cwd.as_deref())));
                    let stored = StoredSession {
                        id: id.clone(),
                        model,
                        system_prompt,
                        cwd,
                        is_subscription,
                        created_at: crate::types::timestamp_ms() as i64,
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
                            Ok(session_info(&stored, &messages))
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
                    send(
                        &mut writer,
                        &Response::Error {
                            message: "server is shutting down".into(),
                        },
                    )
                    .await?;
                    continue;
                }
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
                // Load session
                let session_data = {
                    let st = state.lock().unwrap();
                    match st.db.get_session(&session_id) {
                        Ok(Some(stored)) => {
                            let messages = st.db.get_messages(&session_id)?;
                            let cwd = stored.cwd.clone().unwrap_or_else(|| "/tmp".to_string());
                            Ok((stored, messages, cwd))
                        }
                        Ok(None) => Err(format!("session not found: {}", session_id)),
                        Err(e) => Err(format!("db error: {}", e)),
                    }
                };
                let (stored, mut messages, cwd) = match session_data {
                    Ok(data) => data,
                    Err(msg) => {
                        send(&mut writer, &Response::Error { message: msg }).await?;
                        continue;
                    }
                };
                let model = stored.model.clone();

                // If session was interrupted mid-tool-call, continue first
                if crate::agent::needs_continuation(&messages) {
                    let mut context = Context {
                        system_prompt: stored.system_prompt.clone(),
                        messages: messages.clone(),
                        tools: Vec::new(),
                    };
                    let cont_result = run_agent_turn(
                        &state,
                        &shutdown,
                        cancel_flag.clone(),
                        &model,
                        &mut context,
                        &cwd,
                        &session_id,
                        &mut writer,
                    )
                    .await;
                    match cont_result {
                        Ok(new_msgs) => {
                            let st = state.lock().unwrap();
                            for msg in &new_msgs {
                                st.db.append_message(&session_id, msg)?;
                            }
                            messages = st.db.get_messages(&session_id)?;
                        }
                        Err(e) => {
                            eprintln!("continuation error: {}", e);
                        }
                    }
                }

                // Now append user message and run the main agent loop
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

                let context = Context {
                    system_prompt: stored.system_prompt.clone(),
                    messages,
                    tools: Vec::new(),
                };

                // Run agent loop
                let mut context = context;
                let result = run_agent_turn(
                    &state,
                    &shutdown,
                    cancel_flag.clone(),
                    &model,
                    &mut context,
                    &cwd,
                    &session_id,
                    &mut writer,
                )
                .await;

                match result {
                    Ok(new_msgs) => {
                        let st = state.lock().unwrap();
                        for msg in &new_msgs {
                            st.db.append_message(&session_id, msg)?;
                        }
                    }
                    Err(crate::Error::Cancelled) => {
                        // Stream was aborted mid-flight by the cancel flag;
                        // treat it the same as a normal cancellation.
                        cancel_flag.store(true, Ordering::Relaxed);
                    }
                    Err(e) => {
                        send(
                            &mut writer,
                            &Response::Error {
                                message: format!("agent error: {}", e),
                            },
                        )
                        .await?;
                        send(&mut writer, &Response::AgentDone).await?;
                        continue;
                    }
                }

                // Check compaction
                let was_cancelled = cancel_flag.load(Ordering::Relaxed);
                if !was_cancelled {
                    let should = {
                        let st = state.lock().unwrap();
                        let messages = st.db.get_messages(&session_id)?;
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

                if was_cancelled {
                    let resp = Response::Cancelled;
                    broadcast_to_subscribers(&state, &session_id, &resp);
                    send(&mut writer, &resp).await?;
                } else {
                    let resp = Response::AgentDone;
                    broadcast_to_subscribers(&state, &session_id, &resp);
                    send(&mut writer, &resp).await?;
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
                let (tx, rx) = smol::channel::bounded::<Response>(256);
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
                send(&mut writer, &Response::Ok).await?;
            }
            Request::ListSessions => {
                let sessions = {
                    let st = state.lock().unwrap();
                    let stored = st.db.list_sessions()?;
                    let mut infos = Vec::with_capacity(stored.len());
                    for s in &stored {
                        let messages = st.db.get_messages(&s.id)?;
                        infos.push(session_info(s, &messages));
                    }
                    infos
                };
                send(&mut writer, &Response::Sessions { sessions }).await?;
            }
            Request::DeleteSession { session_id } => {
                {
                    let st = state.lock().unwrap();
                    st.db.delete_session(&session_id)?;
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

/// Run an agent loop turn: resolve API key, stream, forward events, return new messages.
#[allow(clippy::too_many_arguments)]
async fn run_agent_turn<W: futures::io::AsyncWrite + Unpin>(
    state: &SharedState,
    shutdown: &ShutdownHandle,
    cancel_flag: Arc<AtomicBool>,
    model: &Model,
    context: &mut Context,
    cwd: &str,
    session_id: &str,
    writer: &mut W,
) -> crate::Result<Vec<Message>> {
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

    let shutdown_flag = shutdown.flag.clone();
    let cancel_flag_clone = cancel_flag.clone();
    let agent_config = crate::agent::AgentConfig {
        should_stop: Some(Box::new(move || {
            shutdown_flag.load(Ordering::Relaxed) || cancel_flag_clone.load(Ordering::Relaxed)
        })),
        ..Default::default()
    };

    let registry_clone = {
        let st = state.lock().unwrap();
        st.registry.clone()
    };

    let model_clone = model.clone();
    let options_clone = options;
    let cwd_clone = cwd.to_string();
    let mut context_clone = context.clone();

    let in_flight = shutdown.clone();
    let agent_handle = smol::unblock(move || {
        in_flight.enter();
        // Spawn worker subprocess for tool execution
        let mut worker = crate::worker::Worker::spawn(&cwd_clone)?;
        let result = crate::agent::run(
            &registry_clone,
            &model_clone,
            &mut context_clone,
            &mut worker,
            &options_clone,
            &agent_config,
            Box::new(move |event| {
                let _ = event_tx.send_blocking(event);
            }),
        );
        worker.kill();
        in_flight.leave();
        result
    });

    let state_clone = state.clone();
    let session_id_owned = session_id.to_string();
    let forward_handle = async {
        while let Ok(event) = event_rx.recv().await {
            let resp = Response::Stream {
                event: Box::new(event),
            };
            broadcast_to_subscribers(&state_clone, &session_id_owned, &resp);
            send(writer, &resp).await?;
        }
        Ok::<(), crate::Error>(())
    };

    let (agent_result, forward_result) = futures::future::join(agent_handle, forward_handle).await;
    if let Err(e) = forward_result {
        eprintln!("event forward error: {}", e);
    }

    let agent_result = agent_result?;
    Ok(agent_result.new_messages)
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
fn broadcast_to_subscribers(state: &SharedState, session_id: &str, resp: &Response) {
    let mut st = state.lock().unwrap();
    if let Some(subs) = st.subscribers.get_mut(session_id) {
        subs.retain(|tx| tx.try_send(resp.clone()).is_ok());
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
