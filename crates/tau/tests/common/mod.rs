//! Shared test helpers for e2e tests.
#![allow(dead_code)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use tau::protocol::{Request, Response};
use tau::providers::mock::{MockProvider, MockResponse, mock_model};

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
                | Response::Sessions { .. }
                | Response::SessionDeleted
                | Response::SessionsCompleted { .. }
                | Response::AgentDone
                | Response::Cancelled
                | Response::MessageReply { .. }
                | Response::Ok
                | Response::Models { .. }
                | Response::Messages { .. }
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
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("tau-test.sock");
        let db_path = dir.path().join("test.db");
        let sock_clone = sock_path.clone();

        let model = mock_model();
        let mut registry = tau::provider::ProviderRegistry::new();
        registry.register(MockProvider::new(mock_responses));

        let config = tau::server::TestServerConfig {
            registry,
            models: vec![model],
            socket_path: sock_clone,
            db_path,
            tool_executor_factory: None,
            mock_tools: vec![],
            plugins_config: None,
        };

        std::thread::spawn(move || {
            smol::block_on(async {
                if let Err(e) = tau::server::run_with_config(config).await {
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
        F: FnOnce(tau::server::TestServerConfig) -> tau::server::TestServerConfig,
    {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("tau-test.sock");
        let db_path = dir.path().join("test.db");
        let sock_clone = sock_path.clone();

        let model = mock_model();
        let mut registry = tau::provider::ProviderRegistry::new();
        registry.register(MockProvider::new(mock_responses));

        let base_config = tau::server::TestServerConfig {
            registry,
            models: vec![model],
            socket_path: sock_clone,
            db_path,
            tool_executor_factory: None,
            mock_tools: vec![],
            plugins_config: None,
        };
        let config = configure(base_config);

        std::thread::spawn(move || {
            smol::block_on(async {
                if let Err(e) = tau::server::run_with_config(config).await {
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

impl Drop for TestServer {
    fn drop(&mut self) {
        if let Ok(mut conn) = UnixStream::connect(&self.sock_path) {
            let req = serde_json::to_string(&Request::Shutdown { restart: false }).unwrap();
            let _ = conn.write_all(format!("{}\n", req).as_bytes());
            let _ = conn.flush();
        }
    }
}
