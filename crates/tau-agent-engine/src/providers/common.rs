//! Shared utilities for provider implementations.

use std::io::{BufRead, BufReader};
use std::time::Duration;

use crate::provider::EventSender;
use tau_agent_base::types::*;

/// Timeout for TCP + TLS connection establishment.
pub const TIMEOUT_CONNECT: Duration = Duration::from_secs(30);
/// Timeout for sending the request headers.
pub const TIMEOUT_SEND_REQUEST: Duration = Duration::from_secs(30);
/// Timeout for sending the request body (JSON payload).
pub const TIMEOUT_SEND_BODY: Duration = Duration::from_secs(30);
/// Default timeout for receiving response headers (time-to-first-byte).
///
/// Modest bump above ureq's default — non-thinking models normally reply
/// within seconds; this covers occasional slow turns without waiting forever
/// on a genuinely hung provider.
pub const TIMEOUT_RECV_RESPONSE: Duration = Duration::from_secs(180);
/// First-byte timeout for adaptive-thinking-capable models with thinking
/// turned on. Opus 4.7 and similar can spend several minutes reasoning
/// before emitting any SSE event, so we need a much larger budget here.
pub const TIMEOUT_RECV_RESPONSE_ADAPTIVE: Duration = Duration::from_secs(600);

/// Pick the time-to-first-byte timeout for a given model + options.
///
/// Any adaptive-thinking-capable Anthropic model gets
/// [`TIMEOUT_RECV_RESPONSE_ADAPTIVE`], regardless of whether the caller
/// announced thinking in [`StreamOptions`] — these models can delay
/// first-byte for minutes even when thinking isn't explicitly requested,
/// because Anthropic's server may still reason before replying. The
/// caller's `thinking_enabled` flag controls what we *ask for*, not whether
/// the server will reason before responding.
///
/// The one escape hatch is an explicit `thinking_enabled == Some(false)`:
/// callers who deliberately disable thinking (e.g. the review path) have
/// promised they don't want reasoning, so the short timeout is enough.
pub fn recv_timeout_for(model: &Model, options: &StreamOptions) -> Duration {
    if model.thinking == ThinkingStyle::Anthropic
        && crate::providers::anthropic::supports_adaptive_thinking(&model.id)
        && options.thinking_enabled != Some(false)
    {
        TIMEOUT_RECV_RESPONSE_ADAPTIVE
    } else {
        TIMEOUT_RECV_RESPONSE
    }
}

/// Common context carried into the streaming thread.
pub(crate) struct StreamCtx<'a> {
    pub base_url: &'a str,
    pub api_key: &'a str,
    pub api_id: &'a str,
    pub provider_name: &'a str,
    pub model_id: &'a str,
    pub model: &'a Model,
    /// Time-to-first-byte timeout. Providers apply this via ureq's
    /// `timeout_recv_response`. Computed at call time via
    /// [`recv_timeout_for`] so thinking-capable models get a larger budget.
    pub recv_response_timeout: Duration,
}

/// Send a [`StreamEvent`] over the channel, mapping send errors to
/// [`tau_agent_base::Error::ChannelClosed`].
pub(crate) fn send_event(tx: &EventSender, event: StreamEvent) -> tau_agent_base::Result<()> {
    tx.send_blocking(event)
        .map_err(|_| tau_agent_base::Error::ChannelClosed)
}

/// Prepared SSE stream returned by [`open_sse_stream`].
///
/// Owns the buffered body reader ready to be line-iterated, plus the initial
/// `AssistantMessage` seed already emitted as a `StreamEvent::Start`. Callers
/// continue to own the SSE parsing loop and the `output` mutation within it.
pub(crate) struct PreparedStream {
    pub body_reader: Box<dyn BufRead + Send>,
    pub initial_message: AssistantMessage,
}

impl std::fmt::Debug for PreparedStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedStream")
            .field("initial_message", &self.initial_message)
            .finish_non_exhaustive()
    }
}

/// Open an SSE stream against a provider endpoint.
///
/// This centralises the verbatim scaffolding shared by `anthropic::run_stream`
/// and `openai::run_stream`:
///
/// * applies the four ureq timeouts from [`StreamCtx`],
/// * sets `content-type: application/json` and then every `(key, value)` in
///   `extra_headers` (pure append — no merging, no replacing; caller controls
///   ordering, which matters for Anthropic's OAuth header sequence),
///   serialises `body` as JSON,
/// * turns off ureq's "status as error" behaviour so we can inspect 4xx/5xx,
/// * on status ≥ 400, parses integer-seconds `retry-after` (HTTP-date form is
///   intentionally *not* parsed — matches historical behaviour so the retry
///   policy in `agent.rs` keeps working unchanged), captures the response body
///   (best-effort; read errors are dropped into an empty message), and returns
///   [`tau_agent_base::Error::HttpStatus`],
/// * wraps the response body in a `BufReader` and takes ownership via
///   `Body::into_reader()` so the reader is `'static`,
/// * seeds `AssistantMessage::empty(ctx.api_id, ctx.provider_name,
///   ctx.model_id)` and emits exactly one `StreamEvent::Start { partial }`.
///
/// The caller owns the URL suffix (`/v1/messages` vs `/chat/completions`),
/// every wire-visible auth/vendor header (including `accept`, which differs
/// across providers), and the SSE event parsing loop that follows.
pub(crate) fn open_sse_stream<B: serde::Serialize>(
    ctx: &StreamCtx<'_>,
    url: &str,
    extra_headers: &[(&str, &str)],
    body: &B,
    tx: &EventSender,
) -> tau_agent_base::Result<PreparedStream> {
    let mut req = ureq::post(url).header("content-type", "application/json");
    for (k, v) in extra_headers {
        req = req.header(*k, *v);
    }

    let mut resp = req
        .config()
        .timeout_connect(Some(TIMEOUT_CONNECT))
        .timeout_send_request(Some(TIMEOUT_SEND_REQUEST))
        .timeout_send_body(Some(TIMEOUT_SEND_BODY))
        .timeout_recv_response(Some(ctx.recv_response_timeout))
        .http_status_as_error(false)
        .build()
        .send_json(body)
        .map_err(|e| tau_agent_base::Error::Http(e.to_string()))?;

    let status = resp.status().as_u16();
    if status >= 400 {
        let retry_after = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok());
        use std::io::Read;
        let mut body_text = String::new();
        let _ = resp.body_mut().as_reader().read_to_string(&mut body_text);
        return Err(tau_agent_base::Error::HttpStatus {
            status,
            message: body_text,
            retry_after,
        });
    }

    // Take ownership of the body so the reader is 'static + Send; boxing
    // behind `dyn BufRead + Send` hides the ureq reader type from callers.
    let reader: Box<dyn BufRead + Send> = Box::new(BufReader::new(resp.into_body().into_reader()));

    let initial_message = AssistantMessage::empty(ctx.api_id, ctx.provider_name, ctx.model_id);
    send_event(
        tx,
        StreamEvent::Start {
            partial: initial_message.clone(),
        },
    )?;

    Ok(PreparedStream {
        body_reader: reader,
        initial_message,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn mk_model(id: &str, style: ThinkingStyle) -> Model {
        Model {
            id: id.to_string(),
            name: id.to_string(),
            api: "anthropic".to_string(),
            provider: "anthropic".to_string(),
            base_url: "https://example.invalid".to_string(),
            thinking: style,
            cost: ModelCost::default(),
            context_window: 200_000,
            max_tokens: 8_192,
            headers: HashMap::new(),
        }
    }

    #[test]
    fn recv_timeout_uses_adaptive_for_thinking_on_adaptive_model() {
        let model = mk_model("claude-opus-4-7", ThinkingStyle::Anthropic);
        let options = StreamOptions {
            thinking_enabled: Some(true),
            ..StreamOptions::default()
        };
        assert_eq!(
            recv_timeout_for(&model, &options),
            TIMEOUT_RECV_RESPONSE_ADAPTIVE
        );
    }

    #[test]
    fn recv_timeout_uses_adaptive_when_inferred_from_effort() {
        let model = mk_model("claude-sonnet-4.6", ThinkingStyle::Anthropic);
        let options = StreamOptions {
            thinking_effort: Some(ThinkingEffort::High),
            ..StreamOptions::default()
        };
        // thinking_enabled is None but effort is set -> adaptive path enabled.
        assert_eq!(
            recv_timeout_for(&model, &options),
            TIMEOUT_RECV_RESPONSE_ADAPTIVE
        );
    }

    #[test]
    fn recv_timeout_default_when_thinking_disabled_explicitly() {
        let model = mk_model("claude-opus-4-7", ThinkingStyle::Anthropic);
        let options = StreamOptions {
            thinking_enabled: Some(false),
            thinking_effort: Some(ThinkingEffort::High),
            ..StreamOptions::default()
        };
        assert_eq!(recv_timeout_for(&model, &options), TIMEOUT_RECV_RESPONSE);
    }

    #[test]
    fn recv_timeout_default_for_non_adaptive_anthropic_model() {
        let model = mk_model("claude-sonnet-3-5", ThinkingStyle::Anthropic);
        let options = StreamOptions {
            thinking_enabled: Some(true),
            ..StreamOptions::default()
        };
        assert_eq!(recv_timeout_for(&model, &options), TIMEOUT_RECV_RESPONSE);
    }

    #[test]
    fn recv_timeout_default_for_non_anthropic_style() {
        let model = mk_model("claude-opus-4-7", ThinkingStyle::None);
        let options = StreamOptions {
            thinking_enabled: Some(true),
            ..StreamOptions::default()
        };
        // Even on an adaptive-capable id, ThinkingStyle::None disqualifies.
        assert_eq!(recv_timeout_for(&model, &options), TIMEOUT_RECV_RESPONSE);
    }

    #[test]
    fn recv_timeout_adaptive_when_no_thinking_signal() {
        // Regression test for task #569: planning/refining/merge-orchestration
        // paths call with `StreamOptions::default()` (all three thinking fields
        // None). Adaptive-capable models must still get the larger budget
        // because the server can reason for minutes before first byte.
        let model = mk_model("claude-opus-4-7", ThinkingStyle::Anthropic);
        let options = StreamOptions::default();
        assert_eq!(
            recv_timeout_for(&model, &options),
            TIMEOUT_RECV_RESPONSE_ADAPTIVE
        );
    }
}

#[cfg(test)]
mod http_tests {
    //! HTTP-level tests for [`open_sse_stream`].
    //!
    //! These spin up a tiny hand-rolled HTTP/1.1 server on `127.0.0.1:0`
    //! per test; no external mock crate needed. Each server accepts one
    //! connection, reads the request headers + body, runs an assertion
    //! closure, writes a scripted response, and exits.

    use super::*;
    use crate::provider::EventSender;
    use std::collections::HashMap;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    /// A captured request: the raw request-line + headers block, and the
    /// body bytes if any. Headers are lowercased for stable lookup.
    #[derive(Debug, Clone)]
    struct CapturedRequest {
        request_line: String,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    }

    impl CapturedRequest {
        fn header(&self, name: &str) -> Option<&str> {
            let name_lc = name.to_ascii_lowercase();
            self.headers
                .iter()
                .find(|(k, _)| k == &name_lc)
                .map(|(_, v)| v.as_str())
        }

        fn header_order(&self, names: &[&str]) -> Vec<String> {
            let wanted: std::collections::HashSet<String> =
                names.iter().map(|s| s.to_ascii_lowercase()).collect();
            self.headers
                .iter()
                .filter(|(k, _)| wanted.contains(k))
                .map(|(k, _)| k.clone())
                .collect()
        }
    }

    /// Read request line + headers up to \r\n\r\n, then body per
    /// content-length (0 if absent). No chunked-encoding support needed;
    /// our client always sends content-length.
    fn read_request(stream: &mut TcpStream) -> std::io::Result<CapturedRequest> {
        let mut buf = Vec::with_capacity(4096);
        let mut tmp = [0u8; 1024];
        loop {
            let n = stream.read(&mut tmp)?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n]);
            if let Some(hdr_end) = find_double_crlf(&buf) {
                // Parse headers.
                let header_block = std::str::from_utf8(&buf[..hdr_end])
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?
                    .to_string();
                let mut lines = header_block.split("\r\n");
                let request_line = lines.next().unwrap_or_default().to_string();
                let mut headers = Vec::new();
                for line in lines {
                    if let Some((k, v)) = line.split_once(':') {
                        headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
                    }
                }
                let content_length: usize = headers
                    .iter()
                    .find(|(k, _)| k == "content-length")
                    .and_then(|(_, v)| v.parse().ok())
                    .unwrap_or(0);
                // Read rest of body.
                let body_start = hdr_end + 4;
                let mut body = buf[body_start..].to_vec();
                while body.len() < content_length {
                    let n = stream.read(&mut tmp)?;
                    if n == 0 {
                        break;
                    }
                    body.extend_from_slice(&tmp[..n]);
                }
                body.truncate(content_length);
                return Ok(CapturedRequest {
                    request_line,
                    headers,
                    body,
                });
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "headers not terminated",
        ))
    }

    fn find_double_crlf(buf: &[u8]) -> Option<usize> {
        buf.windows(4).position(|w| w == b"\r\n\r\n")
    }

    /// Start a one-shot HTTP server.  The server binds to `127.0.0.1:0`,
    /// returns its URL and a handle that captures the request.
    fn spawn_server(response: Vec<u8>, delay: Option<Duration>) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local_addr");
        let (req_tx, req_rx) = mpsc::channel::<CapturedRequest>();

        let handle = thread::spawn(move || {
            // Accept exactly one connection.
            let (mut stream, _) = match listener.accept() {
                Ok(p) => p,
                Err(_) => return,
            };
            let req = match read_request(&mut stream) {
                Ok(r) => r,
                Err(_) => return,
            };
            let _ = req_tx.send(req);
            if let Some(d) = delay {
                thread::sleep(d);
            }
            let _ = stream.write_all(&response);
            let _ = stream.flush();
        });

        Server {
            url: format!("http://{addr}"),
            req_rx,
            _handle: handle,
        }
    }

    struct Server {
        url: String,
        req_rx: mpsc::Receiver<CapturedRequest>,
        _handle: thread::JoinHandle<()>,
    }

    impl Server {
        fn captured(&self) -> CapturedRequest {
            self.req_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("server captured request")
        }
    }

    fn mk_ctx_model(_recv: Duration) -> Model {
        Model {
            id: "test-model".to_string(),
            name: "test-model".to_string(),
            api: "test-api".to_string(),
            provider: "test-provider".to_string(),
            base_url: "http://unused".to_string(),
            thinking: ThinkingStyle::None,
            cost: ModelCost::default(),
            context_window: 200_000,
            max_tokens: 8_192,
            headers: HashMap::new(),
        }
    }

    fn mk_ctx<'a>(model: &'a Model, recv: Duration) -> StreamCtx<'a> {
        StreamCtx {
            base_url: "http://unused",
            api_key: "unused",
            api_id: "test-api",
            provider_name: "test-provider",
            model_id: "test-model",
            model,
            recv_response_timeout: recv,
        }
    }

    fn sender() -> (EventSender, smol::channel::Receiver<StreamEvent>) {
        smol::channel::unbounded::<StreamEvent>()
    }

    /// Drain pending events from the channel, non-blocking.
    fn drain(rx: &smol::channel::Receiver<StreamEvent>) -> Vec<StreamEvent> {
        let mut out = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            out.push(ev);
        }
        out
    }

    fn ok_response(body: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
        .into_bytes()
    }

    fn status_response(status_line: &str, extra_headers: &[(&str, &str)], body: &str) -> Vec<u8> {
        let mut out = format!("HTTP/1.1 {status_line}\r\n");
        out.push_str(&format!("Content-Length: {}\r\n", body.len()));
        out.push_str("Connection: close\r\n");
        for (k, v) in extra_headers {
            out.push_str(&format!("{k}: {v}\r\n"));
        }
        out.push_str("\r\n");
        out.push_str(body);
        out.into_bytes()
    }

    // ---- Tests ---------------------------------------------------------

    #[test]
    fn happy_path_returns_bufread_and_emits_start() {
        let sse_body = "event: message_start\ndata: {\"ok\":true}\n\n";
        let server = spawn_server(ok_response(sse_body), None);
        let model = mk_ctx_model(Duration::from_secs(5));
        let ctx = mk_ctx(&model, Duration::from_secs(5));
        let (tx, rx) = sender();
        let body = serde_json::json!({"hello": "world"});

        let prepared = open_sse_stream(
            &ctx,
            &server.url,
            &[("accept", "text/event-stream")],
            &body,
            &tx,
        )
        .expect("open_sse_stream ok");

        // Start event emitted exactly once with the right seed.
        let events = drain(&rx);
        assert_eq!(events.len(), 1, "exactly one Start event");
        match &events[0] {
            StreamEvent::Start { partial } => {
                assert_eq!(partial.api, "test-api");
                assert_eq!(partial.provider, "test-provider");
                assert_eq!(partial.model, "test-model");
            }
            other => panic!("expected Start event, got {other:?}"),
        }
        assert_eq!(prepared.initial_message.api, "test-api");

        // Reader yields the SSE payload lines intact.
        let mut reader = prepared.body_reader;
        let mut got = String::new();
        reader.read_to_string(&mut got).expect("read body");
        assert_eq!(got, sse_body);

        // Verify the request that reached the server.
        let req = server.captured();
        assert!(req.request_line.starts_with("POST "));
        assert_eq!(req.header("content-type"), Some("application/json"));
        assert_eq!(req.header("accept"), Some("text/event-stream"));
        let json: serde_json::Value = serde_json::from_slice(&req.body).expect("valid json body");
        assert_eq!(json, body);
    }

    #[test]
    fn status_429_captures_retry_after_integer() {
        let resp = status_response(
            "429 Too Many Requests",
            &[("Retry-After", "7")],
            "slow down",
        );
        let server = spawn_server(resp, None);
        let model = mk_ctx_model(Duration::from_secs(5));
        let ctx = mk_ctx(&model, Duration::from_secs(5));
        let (tx, _rx) = sender();

        let err = open_sse_stream(
            &ctx,
            &server.url,
            &[("accept", "application/json")],
            &serde_json::json!({}),
            &tx,
        )
        .expect_err("expected HttpStatus error");

        match err {
            tau_agent_base::Error::HttpStatus {
                status,
                message,
                retry_after,
            } => {
                assert_eq!(status, 429);
                assert_eq!(retry_after, Some(7));
                assert_eq!(message, "slow down");
            }
            other => panic!("expected HttpStatus, got {other:?}"),
        }
    }

    #[test]
    fn status_500_captures_body_without_retry_after() {
        let server = spawn_server(
            status_response("500 Internal Server Error", &[], "oops"),
            None,
        );
        let model = mk_ctx_model(Duration::from_secs(5));
        let ctx = mk_ctx(&model, Duration::from_secs(5));
        let (tx, _rx) = sender();

        let err = open_sse_stream(
            &ctx,
            &server.url,
            &[("accept", "application/json")],
            &serde_json::json!({}),
            &tx,
        )
        .expect_err("expected HttpStatus error");

        match err {
            tau_agent_base::Error::HttpStatus {
                status,
                message,
                retry_after,
            } => {
                assert_eq!(status, 500);
                assert_eq!(retry_after, None);
                assert_eq!(message, "oops");
            }
            other => panic!("expected HttpStatus, got {other:?}"),
        }
    }

    #[test]
    fn status_400_no_retry_after_header_yields_none() {
        let server = spawn_server(status_response("400 Bad Request", &[], "bad"), None);
        let model = mk_ctx_model(Duration::from_secs(5));
        let ctx = mk_ctx(&model, Duration::from_secs(5));
        let (tx, _rx) = sender();

        let err = open_sse_stream(&ctx, &server.url, &[], &serde_json::json!({}), &tx)
            .expect_err("expected HttpStatus error");

        match err {
            tau_agent_base::Error::HttpStatus { retry_after, .. } => {
                assert_eq!(retry_after, None);
            }
            other => panic!("expected HttpStatus, got {other:?}"),
        }
    }

    #[test]
    fn status_503_http_date_retry_after_is_none() {
        // Pins the historical behaviour: HTTP-date form is intentionally
        // *not* parsed. Only integer seconds are honoured. If you're
        // tempted to "fix" this, update agent.rs's retry policy first.
        let server = spawn_server(
            status_response(
                "503 Service Unavailable",
                &[("Retry-After", "Mon, 01 Jan 2030 00:00:00 GMT")],
                "",
            ),
            None,
        );
        let model = mk_ctx_model(Duration::from_secs(5));
        let ctx = mk_ctx(&model, Duration::from_secs(5));
        let (tx, _rx) = sender();

        let err = open_sse_stream(&ctx, &server.url, &[], &serde_json::json!({}), &tx)
            .expect_err("expected HttpStatus error");

        match err {
            tau_agent_base::Error::HttpStatus {
                status,
                retry_after,
                ..
            } => {
                assert_eq!(status, 503);
                assert_eq!(retry_after, None);
            }
            other => panic!("expected HttpStatus, got {other:?}"),
        }
    }

    #[test]
    fn extra_headers_reach_wire_in_caller_order_after_content_type() {
        let server = spawn_server(ok_response(""), None);
        let model = mk_ctx_model(Duration::from_secs(5));
        let ctx = mk_ctx(&model, Duration::from_secs(5));
        let (tx, _rx) = sender();

        let _ = open_sse_stream(
            &ctx,
            &server.url,
            &[
                ("accept", "application/json"),
                ("x-custom-a", "alpha"),
                ("x-custom-b", "beta"),
            ],
            &serde_json::json!({}),
            &tx,
        )
        .expect("open ok");

        let req = server.captured();
        assert_eq!(req.header("content-type"), Some("application/json"));
        assert_eq!(req.header("accept"), Some("application/json"));
        assert_eq!(req.header("x-custom-a"), Some("alpha"));
        assert_eq!(req.header("x-custom-b"), Some("beta"));

        // Relative order on the wire: content-type first, then the caller's
        // headers in the order supplied.
        let order = req.header_order(&["content-type", "accept", "x-custom-a", "x-custom-b"]);
        assert_eq!(
            order,
            vec![
                "content-type".to_string(),
                "accept".to_string(),
                "x-custom-a".to_string(),
                "x-custom-b".to_string(),
            ]
        );
    }

    #[test]
    fn dropped_tx_yields_channel_closed_on_start_emit() {
        let server = spawn_server(ok_response("event: x\ndata: 1\n\n"), None);
        let model = mk_ctx_model(Duration::from_secs(5));
        let ctx = mk_ctx(&model, Duration::from_secs(5));
        let (tx, rx) = sender();
        drop(rx); // receiver gone; send_blocking fails.

        let err = open_sse_stream(
            &ctx,
            &server.url,
            &[("accept", "text/event-stream")],
            &serde_json::json!({}),
            &tx,
        )
        .expect_err("expected ChannelClosed");

        assert!(
            matches!(err, tau_agent_base::Error::ChannelClosed),
            "expected ChannelClosed, got {err:?}"
        );
    }

    #[test]
    fn recv_timeout_plumbed_to_request() {
        // 2s server delay, 200ms recv-timeout → ureq errors out as
        // Error::Http(...) before the response header arrives.
        let server = spawn_server(
            ok_response("event: x\ndata: 1\n\n"),
            Some(Duration::from_secs(2)),
        );
        let model = mk_ctx_model(Duration::from_millis(200));
        let ctx = mk_ctx(&model, Duration::from_millis(200));
        let (tx, _rx) = sender();

        let err = open_sse_stream(
            &ctx,
            &server.url,
            &[("accept", "text/event-stream")],
            &serde_json::json!({}),
            &tx,
        )
        .expect_err("expected timeout error");

        assert!(
            matches!(err, tau_agent_base::Error::Http(_)),
            "expected Error::Http, got {err:?}"
        );
    }
}
