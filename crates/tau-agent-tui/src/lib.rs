//! Terminal UI for tau chat.

mod app;
mod events;
mod message;
mod render;
pub mod settings;
pub mod theme;
mod ui;

use std::io;

use crossterm::cursor::MoveTo;
use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use smol::channel::{self, Sender};

use tau_agent_lib::client::Client;
use tau_agent_lib::protocol::{Request, Response};

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
) -> tau_agent_lib::Result<()> {
    run_tui(
        session_id,
        model,
        provider,
        context_window,
        is_subscription,
        false,
    )
    .await
}

/// Run the TUI starting in session picker mode.
///
/// Like `run()`, but the session picker overlay opens immediately on startup.
/// Used when `tau` is invoked with no subcommand.
pub async fn run_with_picker(
    session_id: String,
    model: String,
    provider: String,
    context_window: u64,
    is_subscription: bool,
) -> tau_agent_lib::Result<()> {
    run_tui(
        session_id,
        model,
        provider,
        context_window,
        is_subscription,
        true,
    )
    .await
}

/// Common TUI entry point.
async fn run_tui(
    session_id: String,
    model: String,
    provider: String,
    context_window: u64,
    is_subscription: bool,
    start_in_picker: bool,
) -> tau_agent_lib::Result<()> {
    // Set up terminal
    enable_raw_mode().map_err(|e| tau_agent_lib::Error::Io(e.to_string()))?;
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
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableBracketedPaste,
        Clear(ClearType::All),
        MoveTo(0, 0),
    )
    .map_err(|e| tau_agent_lib::Error::Io(e.to_string()))?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal =
        Terminal::new(backend).map_err(|e| tau_agent_lib::Error::Io(e.to_string()))?;

    // Install TUI-aware shutdown handler.  On SIGTERM/SIGHUP (parent
    // shell closed, systemd stop) we restore the terminal so the user's
    // shell isn't left in raw mode / alt-screen, then exit.  SIGINT is
    // intentionally *not* handled here — it stays in the regular
    // crossterm event stream so the in-app Ctrl-C handling (cancel
    // current turn without quitting) keeps working.
    install_tui_signal_handler();

    let result = run_inner(
        &mut terminal,
        session_id,
        model,
        provider,
        context_window,
        is_subscription,
        start_in_picker,
    )
    .await;

    // Restore terminal — pop keyboard enhancement before leaving alternate screen
    restore_terminal();
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

/// Best-effort terminal restoration.  Used by both the normal exit path
/// and the SIGTERM/SIGHUP signal handler.  Safe to call multiple times.
fn restore_terminal() {
    use io::Write;
    let mut stdout = io::stdout();
    let _ = execute!(stdout, PopKeyboardEnhancementFlags);
    let _ = stdout.write_all(b"\x1b[>4;0m");
    let _ = stdout.flush();
    let _ = execute!(stdout, DisableBracketedPaste, LeaveAlternateScreen);
    let _ = disable_raw_mode();
}

/// Install a TUI-aware shutdown handler for SIGTERM / SIGHUP.
///
/// SIGINT is intentionally not handled here: in the TUI we want Ctrl-C
/// to reach the in-app event loop (which translates it into "cancel the
/// current chat turn"), not to tear down the process.
///
/// On SIGTERM/SIGHUP this handler restores the terminal and exits with
/// the conventional 128 + signo code.  Cleanup is best-effort — we can't
/// flush the agent session here without re-entering smol from a foreign
/// thread, so we accept that some state may be lost on hard signals.
fn install_tui_signal_handler() {
    let signals = [
        nix::sys::signal::Signal::SIGTERM,
        nix::sys::signal::Signal::SIGHUP,
    ];
    if let Err(e) = tau_agent_lib::shutdown::install_for(signals, |sig| {
        restore_terminal();
        eprintln!(
            "tau: received {}, exiting",
            tau_agent_lib::shutdown::signal_name(sig),
        );
        let code = match sig {
            nix::sys::signal::Signal::SIGHUP => 129,
            nix::sys::signal::Signal::SIGTERM => 143,
            _ => 1,
        };
        std::process::exit(code);
    }) {
        eprintln!("tau-tui: failed to install signal handler: {}", e);
    }
}

/// Returns Ok(Some(session_id)) if the session was kept, Ok(None) if it was deleted (empty).
async fn run_inner(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    session_id: String,
    model: String,
    provider: String,
    context_window: u64,
    is_subscription: bool,
    start_in_picker: bool,
) -> tau_agent_lib::Result<Option<String>> {
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
        app.session_cwd = info.cwd;
        app.session_project_name = info.project_name;
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
    let refetch_tx = server_tx.clone();
    smol::spawn(async move {
        let mut current_session = initial_session_id;
        let mut is_reconnect = false;
        loop {
            let connected = async {
                let mut client = Client::connect().await?;
                client
                    .send(&Request::Subscribe {
                        session_id: current_session.clone(),
                    })
                    .await?;
                // After reconnecting, catch up on any messages the server
                // persisted while we were disconnected (e.g. during a
                // seamless restart). Fire and forget: responses arrive on
                // a dedicated short-lived connection.
                if is_reconnect {
                    let sid = current_session.clone();
                    let tx = refetch_tx.clone();
                    smol::spawn(async move {
                        if let Ok(mut fetch) = Client::connect().await
                            && fetch
                                .send(&Request::GetMessages { session_id: sid })
                                .await
                                .is_ok()
                        {
                            fetch
                                .recv_streaming(|resp| {
                                    let _ = tx.try_send(resp.clone());
                                })
                                .await
                                .ok();
                        }
                    })
                    .detach();
                }
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
                                return Err(tau_agent_lib::Error::Io("app closed".into()));
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
                            return Err(tau_agent_lib::Error::Io("app closed".into()));
                        }
                    }
                }
                Ok::<(), tau_agent_lib::Error>(())
            }
            .await;

            match connected {
                Err(tau_agent_lib::Error::Io(ref msg)) if msg == "app closed" => break,
                _ => {}
            }

            // Connection lost — wait before reconnecting
            smol::Timer::after(std::time::Duration::from_millis(500)).await;
            is_reconnect = true;
        }
    })
    .detach();

    // If starting in picker mode, open the session picker immediately
    if start_in_picker {
        app.picker_previous_mode = AppMode::Input;
        app.mode = AppMode::SessionPicker;
        app.picker_sessions.clear();
        app.picker_confirm_delete = None;
        app.picker_edit_tagline = None;
        app.picker_filter.clear();
        app.picker_filter_mode = false;
        app.picker_project_filter = app.session_project_name.clone();
        app.picker_show_all_projects = false;
        send_request_and_recv(
            Request::ListSessions {
                include_archived: false,
                project_name: None,
            },
            server_tx.clone(),
        )
        .await?;
    }

    // Fetch initial subscription usage if applicable
    if is_subscription {
        send_request_and_recv(Request::GetSubscriptionUsage, server_tx.clone()).await?;
        app.last_usage_fetch = std::time::Instant::now();
    }

    // Initial draw
    terminal
        .draw(|f| ui::draw(f, &app, &app.theme))
        .map_err(|e| tau_agent_lib::Error::Io(e.to_string()))?;

    // Render throttling: under streaming load (one event per token) the
    // naive "draw on every event" approach redraws hundreds of times per
    // second, wasting CPU and producing visible flicker. Coalesce draw
    // requests to ~30 FPS while streaming; in Input/picker modes always
    // draw immediately so keystrokes and resizes remain snappy.
    // Ref: pi-mono 6f5f37f8.
    const MIN_DRAW_INTERVAL: std::time::Duration = std::time::Duration::from_millis(33);
    let mut last_draw = std::time::Instant::now();
    // `dirty` is always flipped true by every event we receive and reset
    // false after a successful draw.  In the current design every loop
    // iteration starts with an event which marks it dirty, so the flag is
    // effectively always true when checked — but keeping it explicit
    // makes the throttling logic easier to reason about and matches the
    // upstream pi-mono pattern.
    #[allow(unused_assignments)]
    let mut dirty = false;

    // Main loop
    loop {
        let Some(event) = event_loop.next().await else {
            break;
        };

        // Terminal resize and user keystrokes must redraw immediately —
        // throttling them makes the TUI feel laggy. Stream deltas and
        // server responses participate in throttling.
        let force_draw = matches!(
            &event,
            crate::events::Event::Terminal(crossterm::event::Event::Resize(_, _))
                | crate::events::Event::Terminal(crossterm::event::Event::Key(_))
                | crate::events::Event::Terminal(crossterm::event::Event::Paste(_))
        );

        let action = app.handle_event(event);

        // Every event is potentially visually relevant. Instead of trying
        // to classify events, mark dirty unconditionally and rely on the
        // throttle below to coalesce during streaming.
        dirty = true;

        // Execute action
        if let Some(action) = action {
            let sid = app.session_id.clone();
            match action {
                Action::SendChat(text) => {
                    // Fire-and-forget: responses arrive via Subscribe connection
                    send_fire_and_forget(Request::Chat {
                        session_id: sid,
                        text,
                    })
                    .await?;
                }
                Action::QueueMessage(text) => {
                    // Send to the server immediately; it will process the message
                    // once the current agent turn finishes (no client-side buffering).
                    send_fire_and_forget(Request::QueueMessage {
                        target_session_id: sid,
                        content: text,
                        sender_info: "user".into(),
                        await_reply: false,
                        reply_to: None,
                    })
                    .await?;
                }
                Action::CancelChat => {
                    // Fire-and-forget: Cancelled arrives via Subscribe connection
                    let sid_clone = sid;
                    smol::spawn(async move {
                        send_fire_and_forget(Request::CancelChat {
                            session_id: sid_clone,
                            caller_session_id: None,
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
                            caller_session_id: None,
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
                            caller_session_id: None,
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
                    app.picker_edit_tagline = None;
                    app.picker_filter.clear();
                    app.picker_filter_mode = false;
                    app.picker_project_filter = app.session_project_name.clone();
                    app.picker_show_all_projects = false;
                    send_request_and_recv(
                        Request::ListSessions {
                            include_archived: false,
                            project_name: None,
                        },
                        server_tx.clone(),
                    )
                    .await?;
                }
                Action::SetTagline {
                    session_id,
                    tagline,
                } => {
                    send_request_and_recv(
                        Request::SetTagline {
                            session_id,
                            tagline,
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
                    let archiving_current = session_id == app.session_id;
                    if let Some(target_id) = switch_to {
                        if archiving_current
                            && app.nav_stack.last().map(|e| &e.session_id) == Some(&target_id)
                        {
                            // Navigate back to the previous session from the nav stack
                            // (don't save current state — we're archiving it)
                            app.navigate_back();
                            sub_switch_tx.send(app.session_id.clone()).await.ok();
                        } else {
                            match fetch_session_info(&target_id).await {
                                Ok(info) => {
                                    let messages =
                                        fetch_messages(&target_id).await.unwrap_or_default();
                                    if !archiving_current {
                                        app.save_nav_state();
                                    }
                                    app.switch_to_session(&info, messages);
                                    sub_switch_tx.send(target_id).await.ok();
                                }
                                Err(e) => {
                                    app.messages.push(crate::message::MessageItem::Error {
                                        text: format!("failed to switch session: {}", e),
                                    });
                                }
                            }
                        }
                    }
                    send_request_and_recv(
                        Request::ArchiveSession {
                            session_id: session_id.clone(),
                            require_ancestor: None,
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
                Action::RestoreSession { session_id } => {
                    send_request_and_recv(
                        Request::RestoreSession {
                            session_id: session_id.clone(),
                        },
                        server_tx.clone(),
                    )
                    .await?;
                    app.messages.push(crate::message::MessageItem::Status {
                        text: format!(
                            "restored session {}",
                            &session_id[..session_id.len().min(8)]
                        ),
                    });
                }
                Action::ListChildren => {
                    send_request_and_recv(
                        Request::ListSessions {
                            include_archived: false,
                            project_name: None,
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
                Action::ForkSession => {
                    // Create a new session inheriting model/cwd from the current session
                    let info = fetch_session_info(&sid).await.ok();
                    let model = info.as_ref().map(|i| i.model.clone());
                    let cwd = info.as_ref().and_then(|i| i.cwd.clone());
                    match create_session(model, cwd, Some(sid.clone())).await {
                        Ok(new_id) => match fetch_session_info(&new_id).await {
                            Ok(new_info) => {
                                app.save_nav_state();
                                app.switch_to_session(&new_info, vec![]);
                                sub_switch_tx.send(new_id.clone()).await.ok();
                                app.messages.push(crate::message::MessageItem::Status {
                                    text: format!(
                                        "Forked to session {}",
                                        &new_id[..new_id.len().min(8)]
                                    ),
                                });
                            }
                            Err(e) => {
                                app.messages.push(crate::message::MessageItem::Error {
                                    text: format!("fork: failed to fetch new session: {}", e),
                                });
                            }
                        },
                        Err(e) => {
                            app.messages.push(crate::message::MessageItem::Error {
                                text: format!("fork: {}", e),
                            });
                        }
                    }
                }
                Action::NewSession => {
                    // Create a fresh session with defaults
                    let model = crate::settings::default_model();
                    let cwd = std::env::current_dir()
                        .ok()
                        .and_then(|p| p.to_str().map(String::from));
                    match create_session(Some(model), cwd, None).await {
                        Ok(new_id) => match fetch_session_info(&new_id).await {
                            Ok(new_info) => {
                                app.save_nav_state();
                                app.switch_to_session(&new_info, vec![]);
                                sub_switch_tx.send(new_id.clone()).await.ok();
                                app.messages.push(crate::message::MessageItem::Status {
                                    text: format!("New session {}", &new_id[..new_id.len().min(8)]),
                                });
                            }
                            Err(e) => {
                                app.messages.push(crate::message::MessageItem::Error {
                                    text: format!("new: failed to fetch session: {}", e),
                                });
                            }
                        },
                        Err(e) => {
                            app.messages.push(crate::message::MessageItem::Error {
                                text: format!("new: {}", e),
                            });
                        }
                    }
                }
                Action::FireHook { name, data } => {
                    // Best-effort: fire the hook on the server so plugins
                    // (e.g. the task scheduler) can react. If it fails we
                    // just log — the DB state change already succeeded.
                    if let Err(e) = send_fire_and_forget(Request::FireHook { name, data }).await {
                        app.messages.push(crate::message::MessageItem::Error {
                            text: format!("hook: {}", e),
                        });
                    }
                }
                Action::TaskList { project, state } => {
                    send_request_and_recv(
                        Request::TaskList {
                            project,
                            state,
                            parent_id: None,
                        },
                        server_tx.clone(),
                    )
                    .await?;
                }
                Action::TaskGet { id } => {
                    send_request_and_recv(Request::TaskGet { id }, server_tx.clone()).await?;
                }
                Action::TaskCreate { project, title } => {
                    send_request_and_recv(
                        Request::TaskCreate {
                            project,
                            title,
                            parent_id: None,
                            priority: None,
                            tags: vec![],
                            sandbox_profile: None,
                        },
                        server_tx.clone(),
                    )
                    .await?;
                }
                Action::TaskSearch { project, query } => {
                    send_request_and_recv(
                        Request::TaskSearch {
                            project,
                            query,
                            state: None,
                        },
                        server_tx.clone(),
                    )
                    .await?;
                }
                Action::TaskUpdate { id, state } => {
                    send_request_and_recv(
                        Request::TaskUpdate {
                            id,
                            state: Some(state),
                            title: None,
                            priority: None,
                            tags: None,
                            affected_files: None,
                            skip_review: None,
                            require_approval: None,
                            sandbox_profile: None,
                        },
                        server_tx.clone(),
                    )
                    .await?;
                }
                Action::TaskAssign { id, session_id } => {
                    send_request_and_recv(
                        Request::TaskAssign { id, session_id },
                        server_tx.clone(),
                    )
                    .await?;
                }
                Action::TaskStatus { project } => {
                    send_request_and_recv(Request::TaskStatus { project }, server_tx.clone())
                        .await?;
                }
                Action::TaskOverview {
                    project,
                    recent_limit,
                } => {
                    send_request_and_recv(
                        Request::TaskOverview {
                            project,
                            recent_limit,
                        },
                        server_tx.clone(),
                    )
                    .await?;
                }
                Action::TaskMergeQueue { project } => {
                    send_request_and_recv(Request::TaskMergeQueue { project }, server_tx.clone())
                        .await?;
                }
                Action::ProjectStats { project_name } => {
                    send_request_and_recv(
                        Request::ProjectStats { project_name },
                        server_tx.clone(),
                    )
                    .await?;
                }
                Action::OpenTaskPicker => {
                    app.task_picker_previous_mode = app.mode;
                    app.mode = AppMode::TaskPicker;
                    app.picker_tasks.clear();
                    app.picker_groups = crate::app::PickerGroups::default();
                    app.picker_view = crate::app::PickerView::SchedulerState;
                    app.task_picker_confirm = None;
                    app.task_picker_detail = None;
                    app.task_picker_filter.clear();
                    app.task_picker_filter_mode = false;
                    app.task_picker_create_mode = false;
                    app.task_picker_scroll_offset.set(0);
                    let project = app.task_project();
                    send_request_and_recv(
                        Request::TaskOverview {
                            project,
                            recent_limit: 10,
                        },
                        server_tx.clone(),
                    )
                    .await?;
                }
                Action::OpenTaskPickerWithState { state } => {
                    app.task_picker_previous_mode = app.mode;
                    app.mode = AppMode::TaskPicker;
                    app.picker_tasks.clear();
                    app.task_picker_confirm = None;
                    app.task_picker_detail = None;
                    app.task_picker_filter.clear();
                    app.task_picker_filter_mode = false;
                    app.task_picker_create_mode = false;
                    let project = app.task_project();
                    send_request_and_recv(
                        Request::TaskList {
                            project,
                            state: Some(state),
                            parent_id: None,
                        },
                        server_tx.clone(),
                    )
                    .await?;
                }
                Action::TaskDispatch { id } => {
                    send_request_and_recv(
                        Request::ExecuteTool {
                            session_id: sid.clone(),
                            tool_name: "task_dispatch".into(),
                            arguments: serde_json::json!({"id": id}),
                        },
                        server_tx.clone(),
                    )
                    .await?;
                }
                Action::TaskSchedule { project } => {
                    send_request_and_recv(
                        Request::ExecuteTool {
                            session_id: sid.clone(),
                            tool_name: "task_schedule".into(),
                            arguments: serde_json::json!({"project": project}),
                        },
                        server_tx.clone(),
                    )
                    .await?;
                }
                Action::TaskMerge { id } => {
                    send_request_and_recv(
                        Request::ExecuteTool {
                            session_id: sid.clone(),
                            tool_name: "task_merge".into(),
                            arguments: serde_json::json!({"id": id}),
                        },
                        server_tx.clone(),
                    )
                    .await?;
                }
            }
        }

        if app.should_quit {
            break;
        }

        // Only tick when streaming (spinner animation + throttled redraws).
        event_loop.set_ticking(app.mode == AppMode::Streaming);

        // Periodic subscription usage refresh (every 60s)
        if app.totals.is_subscription
            && app.last_usage_fetch.elapsed() >= std::time::Duration::from_secs(60)
        {
            app.last_usage_fetch = std::time::Instant::now();
            send_request_and_recv(Request::GetSubscriptionUsage, server_tx.clone()).await?;
        }

        // Draw, with streaming-mode throttling.
        let throttle_active = app.mode == AppMode::Streaming;
        let interval_ok = last_draw.elapsed() >= MIN_DRAW_INTERVAL;
        if dirty && (force_draw || !throttle_active || interval_ok) {
            terminal
                .draw(|f| ui::draw(f, &app, &app.theme))
                .map_err(|e| tau_agent_lib::Error::Io(e.to_string()))?;
            last_draw = std::time::Instant::now();
            #[allow(unused_assignments)]
            {
                dirty = false;
            }
        }
    }

    // Cancel any in-flight agent turn so it doesn't keep running after exit
    if app.mode == AppMode::Streaming {
        send_fire_and_forget(Request::CancelChat {
            session_id: app.session_id.clone(),
            caller_session_id: None,
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

/// Create a new session and return its ID.
async fn create_session(
    model: Option<String>,
    cwd: Option<String>,
    parent_id: Option<String>,
) -> tau_agent_lib::Result<String> {
    let mut client = Client::connect().await?;
    client
        .send(&Request::CreateSession {
            model,
            provider: None,
            system_prompt: None,
            cwd,
            parent_id,
            child_budget: 0,
            tagline: None,
            auto_archive: false,
            notify_parent: true,
            project_name: None,
            sandbox_profile: None,
        })
        .await?;

    let mut created_id = None;
    client
        .recv_streaming(|resp| {
            if let Response::SessionCreated { session_id } = resp {
                created_id = Some(session_id.clone());
            }
        })
        .await?;

    created_id.ok_or_else(|| tau_agent_lib::Error::Io("failed to create session".into()))
}

/// Fetch message history for a session (blocking request/response).
async fn fetch_messages(
    session_id: &str,
) -> tau_agent_lib::Result<Vec<tau_agent_lib::types::Message>> {
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

    messages.ok_or_else(|| tau_agent_lib::Error::Io("no messages response".into()))
}

/// Fetch session info.
async fn fetch_session_info(
    session_id: &str,
) -> tau_agent_lib::Result<tau_agent_lib::protocol::SessionInfo> {
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

    info.ok_or_else(|| tau_agent_lib::Error::Io("no session info response".into()))
}

/// Send a request and forget — don't recv responses.
/// Used for Chat and CancelChat where responses arrive via the Subscribe connection.
async fn send_fire_and_forget(req: Request) -> tau_agent_lib::Result<()> {
    let mut client = Client::connect().await?;
    client.send(&req).await?;
    // Connection drops — server will still process the request and broadcast events.
    Ok(())
}

/// Open a fresh connection, send a request, and spawn a background task
/// that receives all streaming responses and forwards them to `tx`.
/// Used for point-to-point requests (ListModels, SetModel, etc.) that aren't broadcast.
async fn send_request_and_recv(req: Request, tx: Sender<Response>) -> tau_agent_lib::Result<()> {
    let mut client = Client::connect().await?;
    client.send(&req).await?;

    // Spawn background task for receiving
    smol::spawn(async move {
        let _ = client
            .recv_streaming(|resp| {
                // Best-effort forward into the TUI's event channel.
                // If the TUI-side receiver is gone (closed/dropped), we
                // silently drop the message — the TUI owns the alt-screen,
                // so eprintln! would garble the UI, and tracing has no
                // subscriber in the CLI/TUI process anyway.
                let _ = tx.try_send(resp.clone());
            })
            .await;
    })
    .detach();

    Ok(())
}
