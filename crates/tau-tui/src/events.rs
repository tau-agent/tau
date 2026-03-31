//! Unified event stream merging terminal input and server responses.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crossterm::event::{Event as CtEvent, EventStream};
use futures::StreamExt;
use smol::channel::Receiver;

use tau::protocol::Response;

/// Events the TUI reacts to.
#[derive(Debug)]
pub enum Event {
    /// A crossterm terminal event (key press, resize, etc.).
    Terminal(CtEvent),
    /// A response from the tau server.
    Server(Response),
    /// The server stream ended (connection closed / done).
    ServerDone,
    /// Tick for periodic UI updates (spinner animation, etc.).
    Tick,
}

/// Drives the event loop. Spawns background tasks for terminal events,
/// server responses, and a tick timer, then merges them onto a single channel.
pub struct EventLoop {
    rx: Receiver<Event>,
    _tasks: Vec<smol::Task<()>>,
    /// When true, the tick timer sends events. When false, it sleeps quietly.
    ticking: Arc<AtomicBool>,
}

impl EventLoop {
    /// Create a new event loop.
    ///
    /// `server_rx` receives `Response` values pushed by the chat sender task.
    pub fn new(server_rx: Receiver<Response>) -> Self {
        let (tx, rx) = smol::channel::bounded(256);
        let ticking = Arc::new(AtomicBool::new(false));

        let mut tasks = Vec::new();

        // Terminal events
        let tx_term = tx.clone();
        tasks.push(smol::spawn(async move {
            let mut stream = EventStream::new();
            while let Some(Ok(ev)) = stream.next().await {
                if tx_term.send(Event::Terminal(ev)).await.is_err() {
                    break;
                }
            }
        }));

        // Server responses
        let tx_srv = tx.clone();
        tasks.push(smol::spawn(async move {
            while let Ok(resp) = server_rx.recv().await {
                if tx_srv.send(Event::Server(resp)).await.is_err() {
                    break;
                }
            }
            let _ = tx_srv.send(Event::ServerDone).await;
        }));

        // Tick timer (~15 fps for spinner, only when ticking is true)
        let tx_tick = tx;
        let ticking_clone = ticking.clone();
        tasks.push(smol::spawn(async move {
            loop {
                if ticking_clone.load(Ordering::Relaxed) {
                    smol::Timer::after(std::time::Duration::from_millis(66)).await;
                    if tx_tick.send(Event::Tick).await.is_err() {
                        break;
                    }
                } else {
                    // Sleep longer when idle to avoid busy-waiting
                    smol::Timer::after(std::time::Duration::from_millis(500)).await;
                }
            }
        }));

        Self {
            rx,
            _tasks: tasks,
            ticking,
        }
    }

    /// Enable or disable tick events (for spinner animation).
    pub fn set_ticking(&self, enabled: bool) {
        self.ticking.store(enabled, Ordering::Relaxed);
    }

    /// Get the next event. Returns `None` when all senders are dropped.
    pub async fn next(&self) -> Option<Event> {
        self.rx.recv().await.ok()
    }
}
