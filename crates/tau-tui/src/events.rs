//! Unified event stream merging terminal input and server responses.

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
}

impl EventLoop {
    /// Create a new event loop.
    ///
    /// `server_rx` receives `Response` values pushed by the chat sender task.
    pub fn new(server_rx: Receiver<Response>) -> Self {
        let (tx, rx) = smol::channel::unbounded();

        let mut tasks = Vec::new();

        // Terminal events
        let tx_term = tx.clone();
        tasks.push(smol::spawn(async move {
            let mut stream = EventStream::new();
            // EventStream is a futures::Stream, so we can use StreamExt
            // crossterm's EventStream uses tokio internally via `event-stream` feature,
            // but the futures Stream adapter works with any executor.
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

        // Tick timer (~15 fps for spinner)
        let tx_tick = tx;
        tasks.push(smol::spawn(async move {
            loop {
                smol::Timer::after(std::time::Duration::from_millis(66)).await;
                if tx_tick.send(Event::Tick).await.is_err() {
                    break;
                }
            }
        }));

        Self { rx, _tasks: tasks }
    }

    /// Get the next event. Returns `None` when all senders are dropped.
    pub async fn next(&self) -> Option<Event> {
        self.rx.recv().await.ok()
    }
}
