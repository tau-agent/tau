//! Unix socket server — manages sessions and streams LLM responses.

use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use futures::StreamExt;
use futures::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use smol::Async;

use crate::auth::{AuthCredential, AuthStorage};
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
}

type SharedState = Arc<Mutex<State>>;

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
    registry.register(Box::new(crate::providers::anthropic::Anthropic));
    registry.register(Box::new(crate::providers::openai::OpenAi));
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
    }));

    loop {
        let (stream, _) = listener
            .accept()
            .await
            .map_err(|e| crate::Error::Io(e.to_string()))?;
        let state = state.clone();
        smol::spawn(async move {
            if let Err(e) = handle_client(stream, state).await {
                eprintln!("client error: {}", e);
            }
        })
        .detach();
    }
}

/// Check if a server is already running by trying to connect.
pub fn is_running() -> bool {
    std::os::unix::net::UnixStream::connect(socket_path()).is_ok()
}

// ---------------------------------------------------------------------------
// Client handler
// ---------------------------------------------------------------------------

async fn handle_client(stream: Async<UnixStream>, state: SharedState) -> crate::Result<()> {
    let reader = BufReader::new(&stream);
    let mut writer = &stream;
    let mut lines = reader.lines();

    while let Some(line) = lines.next().await {
        let line = line.map_err(|e: std::io::Error| crate::Error::Io(e.to_string()))?;
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
                model: _model_name,
                provider: _provider_name,
                system_prompt,
            } => {
                let id = {
                    let st = state.lock().unwrap();
                    let model = st.default_model.clone();
                    let is_subscription = st
                        .auth
                        .get(&model.provider)
                        .ok()
                        .flatten()
                        .is_some_and(|c| matches!(c, AuthCredential::Oauth(_)));
                    let id = st.db.next_session_id()?;
                    let stored = StoredSession {
                        id: id.clone(),
                        model,
                        system_prompt,
                        is_subscription,
                        created_at: crate::types::timestamp_ms() as i64,
                    };
                    st.db.create_session(&stored)?;
                    id
                };
                send(&mut writer, &Response::SessionCreated { session_id: id }).await?;
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
                let chat_data = {
                    let st = state.lock().unwrap();
                    match st.db.get_session(&session_id) {
                        Ok(Some(stored)) => {
                            let user_msg = Message::User(UserMessage::text(&text));
                            st.db.append_message(&session_id, &user_msg)?;
                            let messages = st.db.get_messages(&session_id)?;
                            let context = Context {
                                system_prompt: stored.system_prompt.clone(),
                                messages,
                                tools: Vec::new(),
                            };
                            Ok((context, stored.model))
                        }
                        Ok(None) => Err(format!("session not found: {}", session_id)),
                        Err(e) => Err(format!("db error: {}", e)),
                    }
                };
                let (context, model) = match chat_data {
                    Ok(data) => data,
                    Err(msg) => {
                        send(&mut writer, &Response::Error { message: msg }).await?;
                        continue;
                    }
                };

                let api_key = {
                    let st = state.lock().unwrap();
                    resolve_api_key(&st.auth, &st.config, &model.provider)
                };
                let api_key = match api_key {
                    Ok(Some(key)) => key,
                    Ok(None) => {
                        send(
                            &mut writer,
                            &Response::Error {
                                message: format!(
                                    "no API key for {}. Run `tau login` to authenticate.",
                                    model.provider
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
                                message: format!("auth error: {}", e),
                            },
                        )
                        .await?;
                        continue;
                    }
                };

                let options = StreamOptions {
                    api_key: Some(api_key),
                    ..Default::default()
                };

                let stream_result = {
                    let st = state.lock().unwrap();
                    st.registry.stream(&model, &context, &options)
                };
                let rx = match stream_result {
                    Ok(rx) => rx,
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
                };

                let mut final_message = None;
                while let Ok(event) = rx.recv().await {
                    match &event {
                        StreamEvent::Done { message, .. } => {
                            final_message = Some(Message::Assistant(message.clone()));
                        }
                        StreamEvent::Error { error, .. } => {
                            final_message = Some(Message::Assistant(error.clone()));
                        }
                        _ => {}
                    }
                    send(
                        &mut writer,
                        &Response::Stream {
                            event: Box::new(event),
                        },
                    )
                    .await?;
                }

                if let Some(msg) = final_message {
                    let st = state.lock().unwrap();
                    st.db.append_message(&session_id, &msg)?;
                }
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
            Request::Shutdown => {
                send(&mut writer, &Response::Ok).await?;
                let sock = socket_path();
                std::fs::remove_file(&sock).ok();
                std::fs::remove_file(pid_path()).ok();
                std::process::exit(0);
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

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
