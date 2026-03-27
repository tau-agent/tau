//! Terminal UI for tau chat.

mod app;
mod events;
mod message;
mod settings;
pub mod theme;
mod ui;

use std::io;

use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use smol::channel::{self, Sender};

use tau::client::Client;
use tau::protocol::{Request, Response};

use crate::app::{Action, App};
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
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)
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
    disable_raw_mode().ok();
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )
    .ok();
    terminal.show_cursor().ok();

    result
}

async fn run_inner(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    session_id: String,
    model: String,
    provider: String,
    context_window: u64,
    is_subscription: bool,
) -> tau::Result<()> {
    let saved_settings = settings::load();
    let theme = match saved_settings.tui.theme.as_deref() {
        Some(name) => theme::load_by_name(name).unwrap_or_else(|_| theme::dark()),
        None => theme::dark(),
    };
    let mut app = App::new(session_id, model, provider, theme);
    app.totals.context_window = context_window;
    app.totals.is_subscription = is_subscription;

    // Channel for server responses — background recv tasks push here.
    let (server_tx, server_rx) = channel::bounded::<Response>(256);

    // Event loop merges terminal + server + tick
    let event_loop = EventLoop::new(server_rx);

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
                Action::SendChat(text) => {
                    send_request_and_recv(
                        Request::Chat {
                            session_id: sid,
                            text,
                        },
                        server_tx.clone(),
                    )
                    .await?;
                }
                Action::CancelChat => {
                    // Send cancel on a fresh connection (fire-and-forget)
                    smol::spawn(async move {
                        if let Ok(mut c) = Client::connect().await {
                            let _ = c.send(&Request::CancelChat { session_id: sid }).await;
                            let _ = c.recv_streaming(|_| {}).await;
                        }
                    })
                    .detach();
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
            }
        }

        if app.should_quit {
            break;
        }

        // Draw
        terminal
            .draw(|f| ui::draw(f, &app, &app.theme))
            .map_err(|e| tau::Error::Io(e.to_string()))?;
    }

    Ok(())
}

/// Open a fresh connection, send a request, and spawn a background task
/// that receives all streaming responses and forwards them to `tx`.
async fn send_request_and_recv(req: Request, tx: Sender<Response>) -> tau::Result<()> {
    let mut client = Client::connect().await?;
    client.send(&req).await?;

    // Spawn background task for receiving
    smol::spawn(async move {
        let _ = client
            .recv_streaming(|resp| {
                let _ = tx.try_send(resp.clone());
            })
            .await;
    })
    .detach();

    Ok(())
}
