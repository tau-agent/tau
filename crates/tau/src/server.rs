//! Unix socket server — manages sessions and streams LLM responses.

use std::collections::HashMap;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use futures::StreamExt;
use futures::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use smol::Async;

use crate::auth::{AuthCredential, AuthStorage};
use crate::protocol::{Request, Response, SessionInfo, SessionStats, TokenStats};
use crate::provider::ProviderRegistry;
use crate::types::*;

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

struct Session {
    id: String,
    model: Model,
    system_prompt: Option<String>,
    messages: Vec<Message>,
    is_subscription: bool,
}

impl Session {
    fn info(&self) -> SessionInfo {
        SessionInfo {
            id: self.id.clone(),
            model: self.model.id.clone(),
            provider: self.model.provider.clone(),
            message_count: self.messages.len(),
            stats: self.stats(),
        }
    }

    fn stats(&self) -> SessionStats {
        let mut user_messages = 0usize;
        let mut assistant_messages = 0usize;
        let mut tool_calls = 0usize;
        let mut tool_results = 0usize;
        let mut tokens = TokenStats::default();
        let mut cost = 0.0f64;
        let mut last_input_tokens: Option<u64> = None;

        for msg in &self.messages {
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

                    // Track last successful assistant's input tokens as context estimate
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
            is_subscription: self.is_subscription,
            context_window: self.model.context_window,
            context_tokens: last_input_tokens,
        }
    }
}

// ---------------------------------------------------------------------------
// Server state
// ---------------------------------------------------------------------------

struct State {
    sessions: HashMap<String, Session>,
    registry: ProviderRegistry,
    auth: AuthStorage,
    default_model: Model,
    next_session_id: u64,
}

impl State {
    fn new_session_id(&mut self) -> String {
        self.next_session_id += 1;
        format!("s{}", self.next_session_id)
    }
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

/// Run the server (blocking). Call from `smol::block_on`.
pub async fn run(registry: ProviderRegistry, default_model: Model) -> crate::Result<()> {
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

    eprintln!("tau server listening on {}", sock.display());

    let state: SharedState = Arc::new(Mutex::new(State {
        sessions: HashMap::new(),
        registry,
        auth: AuthStorage::open_default(),
        default_model,
        next_session_id: 0,
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
                    let mut st = state.lock().unwrap();
                    let id = st.new_session_id();
                    let model = st.default_model.clone();
                    // Check if using OAuth subscription
                    let is_subscription = st
                        .auth
                        .get(&model.provider)
                        .ok()
                        .flatten()
                        .is_some_and(|c| matches!(c, AuthCredential::Oauth(_)));
                    st.sessions.insert(
                        id.clone(),
                        Session {
                            id: id.clone(),
                            model,
                            system_prompt,
                            messages: Vec::new(),
                            is_subscription,
                        },
                    );
                    id
                };
                send(&mut writer, &Response::SessionCreated { session_id: id }).await?;
            }
            Request::Chat { session_id, text } => {
                let chat_data = {
                    let mut st = state.lock().unwrap();
                    if let Some(session) = st.sessions.get_mut(&session_id) {
                        session
                            .messages
                            .push(Message::User(UserMessage::text(&text)));
                        let context = Context {
                            system_prompt: session.system_prompt.clone(),
                            messages: session.messages.clone(),
                            tools: Vec::new(),
                        };
                        let model = session.model.clone();
                        Some((context, model))
                    } else {
                        None
                    }
                };
                let Some((context, model)) = chat_data else {
                    send(
                        &mut writer,
                        &Response::Error {
                            message: format!("session not found: {}", session_id),
                        },
                    )
                    .await?;
                    continue;
                };

                // Resolve API key (auto-refreshes OAuth tokens)
                let api_key = {
                    let st = state.lock().unwrap();
                    st.auth.get_api_key(&model.provider)
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

                // Stream events to client
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

                // Store assistant message in session
                if let Some(msg) = final_message {
                    let mut st = state.lock().unwrap();
                    if let Some(session) = st.sessions.get_mut(&session_id) {
                        session.messages.push(msg);
                    }
                }
            }
            Request::ListSessions => {
                let sessions: Vec<SessionInfo> = {
                    let st = state.lock().unwrap();
                    st.sessions.values().map(|s| s.info()).collect()
                };
                send(&mut writer, &Response::Sessions { sessions }).await?;
            }
            Request::DeleteSession { session_id } => {
                {
                    let mut st = state.lock().unwrap();
                    st.sessions.remove(&session_id);
                }
                send(&mut writer, &Response::SessionDeleted).await?;
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
