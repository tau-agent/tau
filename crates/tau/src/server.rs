//! Unix socket server — manages sessions and streams LLM responses.

use std::collections::HashMap;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use futures::StreamExt;
use futures::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use smol::Async;

use crate::protocol::{Request, Response, SessionInfo};
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
}

impl Session {
    fn info(&self) -> SessionInfo {
        SessionInfo {
            id: self.id.clone(),
            model: self.model.id.clone(),
            provider: self.model.provider.clone(),
            message_count: self.messages.len(),
        }
    }
}

// ---------------------------------------------------------------------------
// Server state
// ---------------------------------------------------------------------------

struct State {
    sessions: HashMap<String, Session>,
    registry: ProviderRegistry,
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
        // Fallback: use pid-based path (XDG_RUNTIME_DIR and HOME should always be set on Linux)
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

    // Clean up stale socket
    if sock.exists() {
        std::fs::remove_file(&sock).ok();
    }

    let listener = Async::<UnixListener>::bind(&sock)
        .map_err(|e| crate::Error::Io(format!("bind {}: {}", sock.display(), e)))?;

    // Write PID file
    let pid = std::process::id();
    std::fs::write(pid_path(), pid.to_string())
        .map_err(|e| crate::Error::Io(format!("write pidfile: {}", e)))?;

    eprintln!("tau server listening on {}", sock.display());

    let state: SharedState = Arc::new(Mutex::new(State {
        sessions: HashMap::new(),
        registry,
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
                    st.sessions.insert(
                        id.clone(),
                        Session {
                            id: id.clone(),
                            model,
                            system_prompt,
                            messages: Vec::new(),
                        },
                    );
                    id
                };
                send(&mut writer, &Response::SessionCreated { session_id: id }).await?;
            }
            Request::Chat { session_id, text } => {
                // Extract what we need from state, then release lock
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
                        Some((context, model, StreamOptions::default()))
                    } else {
                        None
                    }
                };
                let Some((context, model, options)) = chat_data else {
                    send(
                        &mut writer,
                        &Response::Error {
                            message: format!("session not found: {}", session_id),
                        },
                    )
                    .await?;
                    continue;
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
            Request::Shutdown => {
                send(&mut writer, &Response::Ok).await?;
                // Clean up and exit
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
