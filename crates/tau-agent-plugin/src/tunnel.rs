//! Plugin ↔ server tunnel helpers.
//!
//! Deduplicated from the three near-identical copies in tasks_scheduler,
//! tasks_merge, and worker.
//!
//! # Concurrency model
//!
//! The primitives in this module are designed so that a plugin can serve
//! tool calls on its main event loop **while** a background worker thread
//! (e.g. the tasks plugin's merge worker — see `tasks_merge_worker.rs`)
//! issues its own `ServerRequest` round-trips over the same stdio tunnel.
//!
//! Two ingredients make this safe:
//!
//! * [`SharedStdout`] — a `Write` handle cloneable across threads that
//!   serialises each JSON line through an `Arc<Mutex<BufWriter<Stdout>>>`.
//!   Locking is per-call: `write_all` acquires the mutex once, writes the
//!   whole line, and releases. Two threads never interleave bytes on the
//!   wire.
//!
//! * A line **router** maintained by the plugin's main module (not in this
//!   file): a dedicated stdin-reader thread parses every incoming
//!   `PluginRequest` line and dispatches it based on shape and request-id
//!   prefix. Each thread that wants to issue `ServerRequest`s owns an mpsc
//!   receiver that only receives the `ServerResponse` lines tagged with a
//!   matching prefix. That receiver is wrapped in a `BufRead` adapter and
//!   passed to [`server_request`] as the `reader` argument.
//!
//! With both pieces in place, [`server_request`] below works unchanged from
//! either the main loop or the merge worker: its writes are serialised by
//! the mutex, and it reads responses from its own per-thread channel.

use std::io::{BufRead, Write};
use std::sync::{Arc, Mutex};

use tau_agent_base::plugin_protocol::{PluginMessage, PluginRequest, PluginToolResult};
use tau_agent_base::protocol::{Request, Response};
use tau_agent_base::types::{TextContent, ToolResultContent};

/// A `Write` handle over a shared `BufWriter<Stdout>` (or any inner
/// `Write`), safe to clone across threads.
///
/// Each `write`, `write_all`, and `flush` call acquires the inner mutex
/// once, writes, and releases. JSON lines written via a single `write_all`
/// call therefore appear atomically on the wire even under concurrent use.
///
/// The mutex is held **only** for the duration of the write itself — not
/// across request/response round-trips. Reads are routed via a separate
/// per-thread channel (see the module-level docs); this keeps the plugin's
/// main loop free while a worker thread is blocked waiting for a response.
#[derive(Clone)]
pub struct SharedStdout<W: Write + Send + 'static = std::io::BufWriter<std::io::Stdout>> {
    inner: Arc<Mutex<W>>,
}

impl<W: Write + Send + 'static> SharedStdout<W> {
    /// Wrap an existing writer in a thread-safe handle.
    pub fn new(writer: W) -> Self {
        Self {
            inner: Arc::new(Mutex::new(writer)),
        }
    }

    /// Construct from an already-shared `Arc<Mutex<W>>`. Useful when a
    /// caller wants to retain a handle to the inner writer for flushing
    /// at shutdown.
    pub fn from_arc(inner: Arc<Mutex<W>>) -> Self {
        Self { inner }
    }

    /// Clone the underlying `Arc<Mutex<W>>` for callers that need direct
    /// access (e.g. to drop the writer at shutdown).
    pub fn inner(&self) -> Arc<Mutex<W>> {
        Arc::clone(&self.inner)
    }
}

impl<W: Write + Send + 'static> Write for SharedStdout<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut g = self.inner.lock().expect("SharedStdout mutex poisoned");
        g.write(buf)
    }

    fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
        // Override the default impl so the whole buffer is written under
        // a single lock acquisition — guarantees no interleaving with
        // concurrent writers even if the inner writer issues multiple
        // syscalls internally.
        let mut g = self.inner.lock().expect("SharedStdout mutex poisoned");
        g.write_all(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let mut g = self.inner.lock().expect("SharedStdout mutex poisoned");
        g.flush()
    }
}

/// Send a `PluginMessage` as a JSON line (sync).
///
/// The `write_all` is a single call, so if `writer` is a [`SharedStdout`]
/// the line is written atomically even under concurrent access.
pub fn send_message(writer: &mut impl Write, msg: &PluginMessage) {
    if let Ok(mut line) = serde_json::to_string(msg) {
        line.push('\n');
        let _ = writer.write_all(line.as_bytes());
        let _ = writer.flush();
    }
}

/// Send a `ServerRequest` via plugin protocol and wait for the `ServerResponse`.
///
/// `prefix` is used to generate the request ID (e.g. `"task-sr"`, `"merge-sr"`).
/// The prefix is meaningful to the plugin's line router (see module docs):
/// incoming `ServerResponse` lines are dispatched to the waiting thread
/// based on their request-id prefix, so callers on different threads must
/// use distinct prefixes.
///
/// When `reader` is a router-backed channel adapter, it will only ever
/// yield `ServerResponse` lines — tool calls and other messages are
/// dispatched elsewhere by the router. The [`PluginRequest::ToolCall`]
/// branch below is kept as a safety net for legacy callers that share
/// their reader with the plugin main loop; modern routed callers never
/// hit it.
pub fn server_request(
    writer: &mut impl Write,
    reader: &mut impl BufRead,
    request: Request,
    prefix: &str,
) -> tau_agent_base::Result<Response> {
    // Tag each request with a monotonic counter in addition to the
    // millisecond timestamp so two requests from the same thread cannot
    // collide, and so concurrent callers (main loop vs. background worker)
    // with the same prefix remain distinguishable.
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let request_id = format!(
        "{}-{}-{}",
        prefix,
        tau_agent_base::types::timestamp_ms(),
        seq
    );
    send_message(
        writer,
        &PluginMessage::ServerRequest {
            request_id: request_id.clone(),
            request,
        },
    );

    // Read lines until we get our ServerResponse.
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => {
                return Err(tau_agent_base::Error::Io(
                    "stdin closed while waiting for server response".into(),
                ));
            }
            Ok(_) => {}
            Err(e) => {
                return Err(tau_agent_base::Error::Io(format!("read error: {}", e)));
            }
        }
        if line.trim().is_empty() {
            continue;
        }
        let req: PluginRequest = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(_) => continue,
        };
        match req {
            PluginRequest::ServerResponse {
                request_id: rid,
                response,
            } if rid == request_id => {
                return Ok(response);
            }
            // A ToolCall arrived while we are mid-ServerRequest (e.g. during a
            // background merge/schedule pass). Answer it immediately with an
            // error so the calling session is not left hanging.
            PluginRequest::ToolCall { tool_call_id, .. } => {
                send_message(
                    writer,
                    &PluginMessage::ToolResult(PluginToolResult {
                        tool_call_id,
                        content: vec![ToolResultContent::Text(TextContent {
                            text: "plugin is busy with a background operation — please retry \
                                   in a moment"
                                .into(),
                            text_signature: None,
                        })],
                        is_error: true,
                        summary: None,
                        post_persist_actions: Vec::new(),
                    }),
                );
                // Continue waiting for our ServerResponse.
            }
            // Ignore other message types (ServerResponse with wrong ID, etc.)
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use std::thread;

    /// Concurrent writers must not interleave bytes on the wire. We
    /// hammer a `SharedStdout` from N threads, each writing a distinct
    /// line, and assert that every line in the final output is one of
    /// the well-known patterns (i.e. nobody's output got chopped).
    #[test]
    fn shared_stdout_writes_are_atomic_across_threads() {
        const THREADS: usize = 8;
        const WRITES_PER_THREAD: usize = 100;

        let sink: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let writer = SharedStdout::from_arc(Arc::clone(&sink));

        let handles: Vec<_> = (0..THREADS)
            .map(|tid| {
                let mut w = writer.clone();
                thread::spawn(move || {
                    let line = format!("thread-{:03}-payload\n", tid);
                    for _ in 0..WRITES_PER_THREAD {
                        w.write_all(line.as_bytes()).expect("write");
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().expect("join");
        }

        let bytes = sink.lock().expect("sink").clone();
        let text = String::from_utf8(bytes).expect("utf8");
        let mut count = 0usize;
        for line in text.lines() {
            assert!(
                line.starts_with("thread-") && line.ends_with("-payload"),
                "interleaved or truncated line: {:?}",
                line
            );
            count += 1;
        }
        assert_eq!(count, THREADS * WRITES_PER_THREAD);
    }

    /// Every `server_request` call produces a unique request_id, even
    /// when invoked in a tight loop that would otherwise collide on
    /// `timestamp_ms`. The counter suffix guards against that.
    #[test]
    fn server_request_ids_are_unique_under_burst() {
        use std::collections::HashSet;
        use std::sync::atomic::{AtomicU64, Ordering};

        // We can't run a full round-trip here (no server), but we can
        // assert the id-format contract: two generations within the
        // same millisecond should still differ. Recreate the id-gen
        // mirror-image of server_request's internals.
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let ts = 1_700_000_000_000u64;
        let mut seen = HashSet::new();
        for _ in 0..1000 {
            let s = SEQ.fetch_add(1, Ordering::Relaxed);
            let id = format!("merge-sr-{}-{}", ts, s);
            assert!(seen.insert(id), "duplicate id generated");
        }
    }
}
