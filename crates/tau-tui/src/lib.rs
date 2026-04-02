//! Terminal UI for tau chat.

mod app;
mod events;
mod message;
mod render;
pub mod settings;
pub mod theme;
mod ui;

use std::io;

use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use smol::channel::{self, Sender};

use tau::client::Client;
use tau::protocol::{Request, Response};

use crate::app::{Action, App, AppMode};
use crate::events::EventLoop;

/// Run the TUI chat interface.
///
/// `session_id` is the active session.
/// The caller provides initial model/provider info.
pub async fn run(
    session_id: String,
    model: String,
    provider: String,
    context_window: u64,
    is_subscription: bool,
) -> tau::Result<()> {
    // Set up terminal
    enable_raw_mode().map_err(|e| tau::Error::Io(e.to_string()))?;
    let mut stdout = io::stdout();
    // Enable keyboard enhancement for Shift+Enter etc.
    // Send both Kitty protocol AND xterm modifyOtherKeys:
    // - Kitty protocol works on Ghostty, Kitty, WezTerm etc.
    // - modifyOtherKeys mode 2 works in tmux with extended-keys
    // Terminals ignore sequences they don't understand.
    execute!(
        stdout,
        PushKeyboardEnhancementFlags(
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
        )
    )
    .ok();
    {
        // xterm modifyOtherKeys mode 2
        use io::Write;
        let _ = stdout.write_all(b"\x1b[>4;2m");
        let _ = stdout.flush();
    }
    execute!(stdout, EnterAlternateScreen, EnableBracketedPaste)
        .map_err(|e| tau::Error::Io(e.to_string()))?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).map_err(|e| tau::Error::Io(e.to_string()))?;

    let result = run_inner(
        &mut terminal,
        session_id,
        model,
        provider,
        context_window,
        is_subscription,
    )
    .await;

    // Restore terminal
    // Restore terminal — pop keyboard enhancement before leaving alternate screen
    execute!(terminal.backend_mut(), PopKeyboardEnhancementFlags).ok();
    {
        // Disable xterm modifyOtherKeys
        use io::Write;
        let _ = io::stdout().write_all(b"\x1b[>4;0m");
        let _ = io::stdout().flush();
    }
    execute!(
        terminal.backend_mut(),
        DisableBracketedPaste,
        LeaveAlternateScreen
    )
    .ok();
    disable_raw_mode().ok();
    terminal.show_cursor().ok();

    // Print exit message with session resume hint
    if let Ok(Some(session_id)) = &result {
        eprintln!(
            "Thanks for using tau. Resume with: tau chat -s {}",
            session_id
        );
    }

    result.map(|_| ())
}

/// Returns Ok(Some(session_id)) if the session was kept, Ok(None) if it was deleted (empty).
async fn run_inner(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    session_id: String,
    model: String,
    provider: String,
    context_window: u64,
    is_subscription: bool,
) -> tau::Result<Option<String>> {
    let saved_settings = settings::load();
    let theme = match saved_settings.tui.theme.as_deref() {
        Some(name) => theme::load_by_name(name).unwrap_or_else(|_| theme::dark()),
        None => theme::dark(),
    };
    let mut app = App::new(session_id, model, provider, theme);
    app.totals.context_window = context_window;
    app.totals.is_subscription = is_subscription;

    // Restore message history for session resume
    if let Ok(messages) = fetch_messages(&app.session_id).await {
        app.restore_messages(&messages);
    }

    // Fetch initial session info for parent/child metadata
    if let Ok(info) = fetch_session_info(&app.session_id).await {
        app.parent_id = info.parent_id;
        app.child_count = info.child_count;
    }

    // Channel for server responses — background recv tasks push here.
    let (server_tx, server_rx) = channel::bounded::<Response>(256);

    // Event loop merges terminal + server + tick
    let event_loop = EventLoop::new(server_rx);

    // Channel to tell the subscribe task which session to follow.
    // Sending a new session ID causes it to reconnect to that session.
    let (sub_switch_tx, sub_switch_rx) = channel::bounded::<String>(4);

    // Subscribe to session events with automatic reconnection.
    // This is the single source of truth for all session-related responses
    // (Stream, AgentDone, Cancelled, UserMessage). Chat requests are fire-and-forget.
    // If the connection drops, we reconnect after a short delay so the TUI
    // never permanently loses its event stream.
    let sub_tx = server_tx.clone();
    let initial_session_id = app.session_id.clone();
    smol::spawn(async move {
        let mut current_session = initial_session_id;
        loop {
            let connected = async {
                let mut client = Client::connect().await?;
                client
                    .send(&Request::Subscribe {
                        session_id: current_session.clone(),
                    })
                    .await?;
                let stream = client.response_stream();
                futures::pin_mut!(stream);
                loop {
                    // Race: next server event vs session switch signal
                    let either =
                        futures::future::select(stream.next(), Box::pin(sub_switch_rx.recv()))
                            .await;
                    match either {
                        futures::future::Either::Left((Some(Ok(resp)), _)) => {
                            if sub_tx.send(resp).await.is_err() {
                                return Err(tau::Error::Io("app closed".into()));
                            }
                        }
                        futures::future::Either::Left((_, _)) => {
                            // Stream ended
                            break;
                        }
                        futures::future::Either::Right((Ok(new_session), _)) => {
                            current_session = new_session;
                            // Break inner loop to reconnect with new session
                            break;
                        }
                        futures::future::Either::Right((Err(_), _)) => {
                            // Switch channel closed — TUI shutting down
                            return Err(tau::Error::Io("app closed".into()));
                        }
                    }
                }
                Ok::<(), tau::Error>(())
            }
            .await;

            match connected {
                Err(tau::Error::Io(ref msg)) if msg == "app closed" => break,
                _ => {}
            }

            // Connection lost — wait before reconnecting
            smol::Timer::after(std::time::Duration::from_millis(500)).await;
        }
    })
    .detach();

    // Initial draw
    terminal
        .draw(|f| ui::draw(f, &app, &app.theme))
        .map_err(|e| tau::Error::Io(e.to_string()))?;

    // Main loop
    loop {
        let Some(event) = event_loop.next().await else {
            break;
        };

        let action = app.handle_event(event);

        // Execute action
        if let Some(action) = action {
            let sid = app.session_id.clone();
            match action {
                Action::SendChat(text) | Action::SendQueued(text) => {
                    // Fire-and-forget: responses arrive via Subscribe connection
                    send_fire_and_forget(Request::Chat {
                        session_id: sid,
                        text,
                    })
                    .await?;
                }
                Action::CancelChat => {
                    // Fire-and-forget: Cancelled arrives via Subscribe connection
                    let sid_clone = sid;
                    smol::spawn(async move {
                        send_fire_and_forget(Request::CancelChat {
                            session_id: sid_clone,
                        })
                        .await
                        .ok();
                    })
                    .detach();
                }
                Action::Steer(text) => {
                    // Fire-and-forget: steering message injected into agent loop
                    send_fire_and_forget(Request::Steer {
                        session_id: sid,
                        text,
                    })
                    .await?;
                }
                Action::ListModels => {
                    send_request_and_recv(Request::ListModels, server_tx.clone()).await?;
                }
                Action::SetModel(model_id) => {
                    send_request_and_recv(
                        Request::SetModel {
                            session_id: sid,
                            model_id,
                        },
                        server_tx.clone(),
                    )
                    .await?;
                }
                Action::GetStatus => {
                    send_request_and_recv(
                        Request::GetSessionInfo { session_id: sid },
                        server_tx.clone(),
                    )
                    .await?;
                }
                Action::GetSubscriptionUsage => {
                    send_request_and_recv(Request::GetSubscriptionUsage, server_tx.clone()).await?;
                }
                Action::SetCwd(cwd) => {
                    send_request_and_recv(
                        Request::SetCwd {
                            session_id: sid,
                            cwd,
                        },
                        server_tx.clone(),
                    )
                    .await?;
                }
                Action::ReloadPlugins => {
                    send_request_and_recv(
                        Request::ReloadPlugins { session_id: sid },
                        server_tx.clone(),
                    )
                    .await?;
                    app.messages.push(crate::message::MessageItem::Status {
                        text: "Plugins reloaded".into(),
                    });
                }
                Action::OpenSessionPicker => {
                    app.picker_previous_mode = app.mode;
                    app.mode = AppMode::SessionPicker;
                    app.picker_sessions.clear();
                    app.picker_confirm_delete = None;
                    send_request_and_recv(
                        Request::ListSessions {
                            include_archived: false,
                        },
                        server_tx.clone(),
                    )
                    .await?;
                }
                Action::DeleteSession(session_id) => {
                    send_request_and_recv(
                        Request::DeleteSession {
                            session_id: session_id.clone(),
                        },
                        server_tx.clone(),
                    )
                    .await?;
                    app.messages.push(crate::message::MessageItem::Status {
                        text: format!("deleted session {}", &session_id[..session_id.len().min(8)]),
                    });
                }
                Action::ArchiveSession {
                    session_id,
                    switch_to,
                } => {
                    // If archiving the active session, switch to parent first
                    if let Some(target_id) = switch_to {
                        match fetch_session_info(&target_id).await {
                            Ok(info) => {
                                let messages = fetch_messages(&target_id).await.unwrap_or_default();
                                app.save_nav_state();
                                app.switch_to_session(&info, messages);
                                sub_switch_tx.send(target_id).await.ok();
                            }
                            Err(e) => {
                                app.messages.push(crate::message::MessageItem::Error {
                                    text: format!("failed to switch to parent: {}", e),
                                });
                            }
                        }
                    }
                    send_request_and_recv(
                        Request::ArchiveSession {
                            session_id: session_id.clone(),
                        },
                        server_tx.clone(),
                    )
                    .await?;
                    app.messages.push(crate::message::MessageItem::Status {
                        text: format!(
                            "archived session {}",
                            &session_id[..session_id.len().min(8)]
                        ),
                    });
                }
                Action::ListChildren => {
                    send_request_and_recv(
                        Request::ListSessions {
                            include_archived: false,
                        },
                        server_tx.clone(),
                    )
                    .await?;
                }
                Action::NavigateBack => {
                    app.navigate_back();
                    // Tell subscribe task to follow the restored session
                    sub_switch_tx.send(app.session_id.clone()).await.ok();
                }
                Action::SwitchSession(target_id) => {
                    // Navigate forward: fetch target info + messages, save current state
                    match fetch_session_info(&target_id).await {
                        Ok(info) => {
                            let messages = fetch_messages(&target_id).await.unwrap_or_default();
                            app.save_nav_state();
                            app.switch_to_session(&info, messages);
                            // Tell subscribe task to follow the new session
                            sub_switch_tx.send(target_id).await.ok();
                        }
                        Err(e) => {
                            app.messages.push(crate::message::MessageItem::Error {
                                text: format!("session not found: {}", e),
                            });
                        }
                    }
                }
            }
        }

        if app.should_quit {
            break;
        }

        // Only tick when streaming (spinner animation)
        event_loop.set_ticking(app.mode == AppMode::Streaming);

        // Draw
        terminal
            .draw(|f| ui::draw(f, &app, &app.theme))
            .map_err(|e| tau::Error::Io(e.to_string()))?;
    }

    // Cancel any in-flight agent turn so it doesn't keep running after exit
    if app.mode == AppMode::Streaming {
        send_fire_and_forget(Request::CancelChat {
            session_id: app.session_id.clone(),
        })
        .await
        .ok();
    }

    // Clean up empty sessions (no user messages sent)
    let has_user_messages = app
        .messages
        .iter()
        .any(|m| matches!(m, crate::message::MessageItem::User { .. }));
    if !has_user_messages {
        let sid = app.session_id.clone();
        // Use send+recv for delete since it's not broadcast via Subscribe
        send_request_and_recv(Request::DeleteSession { session_id: sid }, server_tx)
            .await
            .ok();
        Ok(None)
    } else {
        Ok(Some(app.session_id.clone()))
    }
}

/// Fetch message history for a session (blocking request/response).
async fn fetch_messages(session_id: &str) -> tau::Result<Vec<tau::types::Message>> {
    let mut client = Client::connect().await?;
    client
        .send(&Request::GetMessages {
            session_id: session_id.to_string(),
        })
        .await?;

    let mut messages = None;
    client
        .recv_streaming(|resp| {
            if let Response::Messages { messages: msgs } = resp {
                messages = Some(msgs.clone());
            }
        })
        .await?;

    messages.ok_or_else(|| tau::Error::Io("no messages response".into()))
}

/// Fetch session info.
async fn fetch_session_info(session_id: &str) -> tau::Result<tau::protocol::SessionInfo> {
    let mut client = Client::connect().await?;
    client
        .send(&Request::GetSessionInfo {
            session_id: session_id.to_string(),
        })
        .await?;

    let mut info = None;
    client
        .recv_streaming(|resp| {
            if let Response::SessionInfo { info: i } = resp {
                info = Some(i.clone());
            }
        })
        .await?;

    info.ok_or_else(|| tau::Error::Io("no session info response".into()))
}

/// Send a request and forget — don't recv responses.
/// Used for Chat and CancelChat where responses arrive via the Subscribe connection.
async fn send_fire_and_forget(req: Request) -> tau::Result<()> {
    let mut client = Client::connect().await?;
    client.send(&req).await?;
    // Connection drops — server will still process the request and broadcast events.
    Ok(())
}

/// Open a fresh connection, send a request, and spawn a background task
/// that receives all streaming responses and forwards them to `tx`.
/// Used for point-to-point requests (ListModels, SetModel, etc.) that aren't broadcast.
async fn send_request_and_recv(req: Request, tx: Sender<Response>) -> tau::Result<()> {
    let mut client = Client::connect().await?;
    client.send(&req).await?;

    // Spawn background task for receiving
    smol::spawn(async move {
        let _ = client
            .recv_streaming(|resp| {
                if tx.try_send(resp.clone()).is_err() {
                    eprintln!("warning: try_send failed in send_request_and_recv");
                }
            })
            .await;
    })
    .detach();

    Ok(())
}
