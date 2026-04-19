//! Shared test helpers for e2e tests.
#![allow(dead_code)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use tau_agent_lib::protocol::{Request, Response, SessionInfo};
use tau_agent_lib::providers::mock::{MockProvider, MockResponse, mock_model};

/// Send a request and read one response line.
pub fn send_recv(stream: &UnixStream, req: &Request) -> Response {
    let mut stream = stream.try_clone().unwrap();
    let mut line = serde_json::to_string(req).unwrap();
    line.push('\n');
    stream.write_all(line.as_bytes()).unwrap();
    stream.flush().unwrap();

    let mut reader = BufReader::new(stream);
    let mut resp_line = String::new();
    reader.read_line(&mut resp_line).unwrap();
    serde_json::from_str(&resp_line).unwrap()
}

/// Read all response lines until a terminal one.
pub fn send_recv_all(stream: &UnixStream, req: &Request) -> Vec<Response> {
    let mut stream = stream.try_clone().unwrap();
    let mut line = serde_json::to_string(req).unwrap();
    line.push('\n');
    stream.write_all(line.as_bytes()).unwrap();
    stream.flush().unwrap();

    let mut reader = BufReader::new(stream);
    let mut responses = Vec::new();
    loop {
        let mut resp_line = String::new();
        reader.read_line(&mut resp_line).unwrap();
        if resp_line.trim().is_empty() {
            continue;
        }
        let resp: Response = serde_json::from_str(&resp_line).unwrap();
        let is_terminal = matches!(
            &resp,
            Response::SessionCreated { .. }
                | Response::SessionInfo { .. }
                | Response::SessionAncestors { .. }
                | Response::Sessions { .. }
                | Response::SessionDeleted
                | Response::SessionsCompleted { .. }
                | Response::AgentDone
                | Response::Cancelled
                | Response::MessageReply { .. }
                | Response::Ok
                | Response::OkWithNote { .. }
                | Response::Models { .. }
                | Response::Messages { .. }
                | Response::ToolExecuted { .. }
        );
        responses.push(resp);
        if is_terminal {
            break;
        }
    }
    responses
}

pub struct TestServer {
    pub sock_path: PathBuf,
    _dir: tempfile::TempDir,
}

impl TestServer {
    /// Start a test server with mock provider in a background thread.
    pub fn start(mock_responses: Vec<MockResponse>) -> Self {
        // Keep shutdown snappy in tests; production defaults to 180s.
        // SAFETY: integration tests each run in their own process.
        unsafe {
            std::env::set_var("TAU_SHUTDOWN_DRAIN_SECS", "2");
        }
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("tau-test.sock");
        let db_path = dir.path().join("test.db");
        let sock_clone = sock_path.clone();

        let model = mock_model();
        let mut registry = tau_agent_lib::provider::ProviderRegistry::new();
        registry.register(MockProvider::new(mock_responses));

        let config = tau_agent_lib::server::TestServerConfig {
            registry,
            models: vec![model],
            socket_path: sock_clone,
            db_path,
            tool_executor_factory: None,
            mock_tools: vec![],
            plugins_config: None,
            aliases: std::collections::HashMap::new(),
        };

        std::thread::spawn(move || {
            smol::block_on(async {
                if let Err(e) = tau_agent_lib::server::run_with_config(config).await {
                    eprintln!("test server error: {}", e);
                }
            });
        });

        // Wait for socket to appear
        for _ in 0..50 {
            if sock_path.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(sock_path.exists(), "server socket did not appear");

        TestServer {
            sock_path,
            _dir: dir,
        }
    }

    /// Start a test server with custom config modifications.
    pub fn start_with_config<F>(mock_responses: Vec<MockResponse>, configure: F) -> Self
    where
        F: FnOnce(
            tau_agent_lib::server::TestServerConfig,
        ) -> tau_agent_lib::server::TestServerConfig,
    {
        // Keep shutdown snappy in tests; production defaults to 180s.
        // SAFETY: integration tests each run in their own process.
        unsafe {
            std::env::set_var("TAU_SHUTDOWN_DRAIN_SECS", "2");
        }
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("tau-test.sock");
        let db_path = dir.path().join("test.db");
        let sock_clone = sock_path.clone();

        let model = mock_model();
        let mut registry = tau_agent_lib::provider::ProviderRegistry::new();
        registry.register(MockProvider::new(mock_responses));

        let base_config = tau_agent_lib::server::TestServerConfig {
            registry,
            models: vec![model],
            socket_path: sock_clone,
            db_path,
            tool_executor_factory: None,
            mock_tools: vec![],
            plugins_config: None,
            aliases: std::collections::HashMap::new(),
        };
        let config = configure(base_config);

        std::thread::spawn(move || {
            smol::block_on(async {
                if let Err(e) = tau_agent_lib::server::run_with_config(config).await {
                    eprintln!("test server error: {}", e);
                }
            });
        });

        for _ in 0..50 {
            if sock_path.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(sock_path.exists(), "server socket did not appear");

        TestServer {
            sock_path,
            _dir: dir,
        }
    }

    pub fn connect(&self) -> UnixStream {
        let conn = UnixStream::connect(&self.sock_path).unwrap();
        conn.set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        conn
    }

    pub fn shutdown(&self) {
        let conn = self.connect();
        send_recv(&conn, &Request::Shutdown { restart: false });
    }
}

// ---------------------------------------------------------------------------
// Polling helpers for task-lifecycle assertions.
//
// These helpers underpin the e2e tests that assert auto-dispatch actually
// produced a worker session (would have caught #572/#577/#584/#587).
// ---------------------------------------------------------------------------

/// Issue a `GetSessionInfo` against the server and return the parsed
/// `SessionInfo`.  Returns `None` when the server reports an error
/// (e.g. session does not exist).
pub fn get_session_info(server: &TestServer, session_id: &str) -> Option<SessionInfo> {
    let conn = server.connect();
    match send_recv(
        &conn,
        &Request::GetSessionInfo {
            session_id: session_id.to_string(),
        },
    ) {
        Response::SessionInfo { info } => Some(info),
        Response::Error { .. } => None,
        other => panic!("expected SessionInfo for {}, got: {:?}", session_id, other),
    }
}

/// Assert a session exists on the server, optionally checking parent and
/// tagline-prefix invariants.  Returns the `SessionInfo` for further
/// assertions by the caller.
///
/// Panics with a descriptive message when any check fails — these
/// assertions are the whole point of tests built on top of this helper.
pub fn assert_session_exists(
    server: &TestServer,
    target_session_id: &str,
    expected_parent: Option<&str>,
    expected_tagline_prefix: Option<&str>,
) -> SessionInfo {
    let info = get_session_info(server, target_session_id).unwrap_or_else(|| {
        panic!(
            "expected session {} to exist on the server, but GetSessionInfo returned an error",
            target_session_id
        )
    });
    if let Some(parent) = expected_parent {
        let actual = info.parent_id.as_deref().unwrap_or("");
        assert_eq!(
            actual, parent,
            "session {} parent mismatch: expected {}, got {}",
            target_session_id, parent, actual
        );
    }
    if let Some(prefix) = expected_tagline_prefix {
        let tagline = info.tagline.as_deref().unwrap_or("");
        assert!(
            tagline.starts_with(prefix),
            "session {} tagline {:?} does not start with expected prefix {:?}",
            target_session_id,
            tagline,
            prefix
        );
    }
    info
}

/// Fetch all messages from a session via `GetMessages`.
pub fn get_session_messages(
    server: &TestServer,
    session_id: &str,
) -> Vec<tau_agent_lib::types::Message> {
    let conn = server.connect();
    match send_recv(
        &conn,
        &Request::GetMessages {
            session_id: session_id.to_string(),
        },
    ) {
        Response::Messages { messages } => messages,
        other => panic!("expected Messages for {}, got: {:?}", session_id, other),
    }
}

/// Poll the given closure every `interval` until it returns `Some(value)`
/// or `timeout` elapses.  On timeout, panics with `description`.
pub fn poll_until<T, F>(timeout: Duration, interval: Duration, description: &str, mut f: F) -> T
where
    F: FnMut() -> Option<T>,
{
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(v) = f() {
            return v;
        }
        if Instant::now() > deadline {
            panic!("poll_until timed out after {:?}: {}", timeout, description);
        }
        std::thread::sleep(interval);
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        if let Ok(mut conn) = UnixStream::connect(&self.sock_path) {
            let req = serde_json::to_string(&Request::Shutdown { restart: false }).unwrap();
            let _ = conn.write_all(format!("{}\n", req).as_bytes());
            let _ = conn.flush();
        }
    }
}
