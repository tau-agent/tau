//! End-to-end test: start a server with mock provider, spawn sessions.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use tau::protocol::{Request, Response};
use tau::providers::mock::{MockProvider, MockResponse, mock_model};

/// Send a request and read one response line.
fn send_recv(stream: &UnixStream, req: &Request) -> Response {
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
fn send_recv_all(stream: &UnixStream, req: &Request) -> Vec<Response> {
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

struct TestServer {
    sock_path: PathBuf,
    _dir: tempfile::TempDir,
}

impl TestServer {
    /// Start a test server with mock provider in a background thread.
    fn start(mock_responses: Vec<MockResponse>) -> Self {
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

    fn connect(&self) -> UnixStream {
        let conn = UnixStream::connect(&self.sock_path).unwrap();
        conn.set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        conn
    }

    fn shutdown(&self) {
        let conn = self.connect();
        send_recv(&conn, &Request::Shutdown { restart: false });
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        // Best-effort shutdown -- don't panic in drop
        if let Ok(mut conn) = UnixStream::connect(&self.sock_path) {
            let req = serde_json::to_string(&Request::Shutdown { restart: false }).unwrap();
            let _ = conn.write_all(format!("{}\n", req).as_bytes());
            let _ = conn.flush();
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn server_create_session_and_list() {
    let server = TestServer::start(vec![]);
    let conn = server.connect();

    // Create a session with child_budget
    let resp = send_recv(
        &conn,
        &Request::CreateSession {
            model: None,
            provider: None,
            system_prompt: None,
            cwd: Some("/tmp".into()),
            parent_id: None,
            child_budget: 5,
            tagline: None,
        },
    );
    let session_id = match resp {
        Response::SessionCreated { session_id } => session_id,
        other => panic!("expected SessionCreated, got {:?}", other),
    };

    // List sessions -- should show the one we created
    let conn2 = server.connect();
    let resp = send_recv(&conn2, &Request::ListSessions);
    match resp {
        Response::Sessions { sessions } => {
            assert_eq!(sessions.len(), 1);
            assert_eq!(sessions[0].id, session_id);
            assert_eq!(sessions[0].child_budget, 5);
            assert_eq!(sessions[0].child_count, 0);
            assert!(sessions[0].parent_id.is_none());
        }
        other => panic!("expected Sessions, got {:?}", other),
    }

    server.shutdown();
}

#[test]
fn server_create_child_session_with_budget() {
    let server = TestServer::start(vec![]);

    // Create parent
    let conn = server.connect();
    let parent_id = match send_recv(
        &conn,
        &Request::CreateSession {
            model: None,
            provider: None,
            system_prompt: None,
            cwd: Some("/tmp".into()),
            parent_id: None,
            child_budget: 3,
            tagline: None,
        },
    ) {
        Response::SessionCreated { session_id } => session_id,
        other => panic!("expected SessionCreated, got {:?}", other),
    };

    // Create child
    let conn2 = server.connect();
    let child_id = match send_recv(
        &conn2,
        &Request::CreateSession {
            model: None,
            provider: None,
            system_prompt: None,
            cwd: None, // should inherit /tmp
            parent_id: Some(parent_id.clone()),
            child_budget: 0,
            tagline: None,
        },
    ) {
        Response::SessionCreated { session_id } => session_id,
        other => panic!("expected SessionCreated, got {:?}", other),
    };

    // Verify parent-child relationship
    let conn3 = server.connect();
    let resp = send_recv(
        &conn3,
        &Request::GetSessionInfo {
            session_id: parent_id.clone(),
        },
    );
    match resp {
        Response::SessionInfo { info } => {
            assert_eq!(info.child_count, 1);
            assert_eq!(info.child_budget, 3);
        }
        other => panic!("expected SessionInfo, got {:?}", other),
    }

    let conn4 = server.connect();
    let resp = send_recv(
        &conn4,
        &Request::GetSessionInfo {
            session_id: child_id.clone(),
        },
    );
    match resp {
        Response::SessionInfo { info } => {
            assert_eq!(info.parent_id.as_deref(), Some(parent_id.as_str()));
            assert_eq!(info.cwd.as_deref(), Some("/tmp")); // inherited
        }
        other => panic!("expected SessionInfo, got {:?}", other),
    }

    server.shutdown();
}

#[test]
fn server_budget_exceeded() {
    let server = TestServer::start(vec![]);

    // Create parent with budget=1
    let conn = server.connect();
    let parent_id = match send_recv(
        &conn,
        &Request::CreateSession {
            model: None,
            provider: None,
            system_prompt: None,
            cwd: None,
            parent_id: None,
            child_budget: 1,
            tagline: None,
        },
    ) {
        Response::SessionCreated { session_id } => session_id,
        other => panic!("expected SessionCreated, got {:?}", other),
    };

    // Create first child (cost=1, fills budget)
    let conn2 = server.connect();
    match send_recv(
        &conn2,
        &Request::CreateSession {
            model: None,
            provider: None,
            system_prompt: None,
            cwd: None,
            parent_id: Some(parent_id.clone()),
            child_budget: 0,
            tagline: None,
        },
    ) {
        Response::SessionCreated { .. } => {}
        other => panic!("expected SessionCreated, got {:?}", other),
    }

    // Second child should fail -- budget exceeded
    let conn3 = server.connect();
    match send_recv(
        &conn3,
        &Request::CreateSession {
            model: None,
            provider: None,
            system_prompt: None,
            cwd: None,
            parent_id: Some(parent_id.clone()),
            child_budget: 0,
            tagline: None,
        },
    ) {
        Response::Error { message } => {
            assert!(
                message.contains("budget exceeded"),
                "expected budget error, got: {}",
                message
            );
        }
        other => panic!("expected Error, got {:?}", other),
    }

    server.shutdown();
}

#[test]
fn server_delete_session_tree() {
    let server = TestServer::start(vec![]);

    // Create parent -> child
    let conn = server.connect();
    let parent_id = match send_recv(
        &conn,
        &Request::CreateSession {
            model: None,
            provider: None,
            system_prompt: None,
            cwd: None,
            parent_id: None,
            child_budget: 5,
            tagline: None,
        },
    ) {
        Response::SessionCreated { session_id } => session_id,
        other => panic!("{:?}", other),
    };

    let conn2 = server.connect();
    let child_id = match send_recv(
        &conn2,
        &Request::CreateSession {
            model: None,
            provider: None,
            system_prompt: None,
            cwd: None,
            parent_id: Some(parent_id.clone()),
            child_budget: 0,
            tagline: None,
        },
    ) {
        Response::SessionCreated { session_id } => session_id,
        other => panic!("{:?}", other),
    };

    // Delete parent -- should cascade to child
    let conn3 = server.connect();
    match send_recv(
        &conn3,
        &Request::DeleteSession {
            session_id: parent_id.clone(),
        },
    ) {
        Response::SessionDeleted => {}
        other => panic!("{:?}", other),
    }

    // Both should be gone
    let conn4 = server.connect();
    match send_recv(
        &conn4,
        &Request::GetSessionInfo {
            session_id: parent_id,
        },
    ) {
        Response::Error { .. } => {} // expected
        other => panic!("expected error for deleted parent, got {:?}", other),
    }

    let conn5 = server.connect();
    match send_recv(
        &conn5,
        &Request::GetSessionInfo {
            session_id: child_id,
        },
    ) {
        Response::Error { .. } => {} // expected
        other => panic!("expected error for deleted child, got {:?}", other),
    }

    server.shutdown();
}

#[test]
fn server_wait_sessions_immediate() {
    let server = TestServer::start(vec![]);

    // Create a session (no agent turn running -- should be immediately "done")
    let conn = server.connect();
    let sid = match send_recv(
        &conn,
        &Request::CreateSession {
            model: None,
            provider: None,
            system_prompt: None,
            cwd: None,
            parent_id: None,
            child_budget: 0,
            tagline: None,
        },
    ) {
        Response::SessionCreated { session_id } => session_id,
        other => panic!("{:?}", other),
    };

    // WaitSessions should return immediately since no agent turn is running
    let conn2 = server.connect();
    let resp = send_recv(
        &conn2,
        &Request::WaitSessions {
            session_ids: vec![sid.clone()],
            timeout_secs: 5,
        },
    );
    match resp {
        Response::SessionsCompleted { results } => {
            assert_eq!(results.len(), 1);
            assert_eq!(results[0].session_id, sid);
            assert_eq!(results[0].status, "done");
        }
        other => panic!("expected SessionsCompleted, got {:?}", other),
    }

    server.shutdown();
}

#[test]
fn server_chat_simple_text() {
    let server = TestServer::start(vec![MockResponse::Text("Hello from mock!".into())]);

    // Create session
    let conn = server.connect();
    let sid = match send_recv(
        &conn,
        &Request::CreateSession {
            model: None,
            provider: None,
            system_prompt: Some("You are helpful.".into()),
            cwd: Some("/tmp".into()),
            parent_id: None,
            child_budget: 0,
            tagline: None,
        },
    ) {
        Response::SessionCreated { session_id } => session_id,
        other => panic!("{:?}", other),
    };

    // Send chat -- collect all responses until AgentDone
    let conn2 = server.connect();
    let responses = send_recv_all(
        &conn2,
        &Request::Chat {
            session_id: sid.clone(),
            text: "hello".into(),
        },
    );

    // Should have stream events and AgentDone
    let has_done = responses.iter().any(|r| matches!(r, Response::AgentDone));
    assert!(has_done, "expected AgentDone in responses: {:?}", responses);

    // Verify messages are persisted
    let conn3 = server.connect();
    let resp = send_recv(
        &conn3,
        &Request::GetMessages {
            session_id: sid.clone(),
        },
    );
    match resp {
        Response::Messages { messages } => {
            // Should have: user message + assistant message
            assert!(
                messages.len() >= 2,
                "expected at least 2 messages, got {}: {:?}",
                messages.len(),
                messages
            );
            assert!(matches!(&messages[0], tau::types::Message::User(_)));
            assert!(
                matches!(&messages[1], tau::types::Message::Assistant(a) if a.text().contains("Hello from mock!"))
            );
        }
        other => panic!("expected Messages, got {:?}", other),
    }

    server.shutdown();
}

#[test]
fn server_chat_tool_call_loop() {
    // Without a worker plugin, tool calls will error ("no plugin provides tool").
    // The important thing is that the server handles this gracefully and
    // persists all messages (including the error tool result).
    let server = TestServer::start(vec![
        MockResponse::ToolCalls(vec![tau::types::ToolCall {
            id: "tc1".into(),
            name: "nonexistent_tool".into(),
            arguments: serde_json::json!({"arg": "value"}),
        }]),
        MockResponse::Text("I see the tool wasn't found.".into()),
    ]);

    let conn = server.connect();
    let sid = match send_recv(
        &conn,
        &Request::CreateSession {
            model: None,
            provider: None,
            system_prompt: Some("You are helpful.".into()),
            cwd: Some("/tmp".into()),
            parent_id: None,
            child_budget: 0,
            tagline: None,
        },
    ) {
        Response::SessionCreated { session_id } => session_id,
        other => panic!("{:?}", other),
    };

    // Chat
    let conn2 = server.connect();
    let responses = send_recv_all(
        &conn2,
        &Request::Chat {
            session_id: sid.clone(),
            text: "run something".into(),
        },
    );

    let has_done = responses.iter().any(|r| matches!(r, Response::AgentDone));
    assert!(has_done, "expected AgentDone: {:?}", responses);

    // Verify messages persisted -- tool error result should be there too
    let conn3 = server.connect();
    let resp = send_recv(
        &conn3,
        &Request::GetMessages {
            session_id: sid.clone(),
        },
    );
    match resp {
        Response::Messages { messages } => {
            // user + assistant(tool_call) + tool_result(error) + assistant(response)
            assert!(
                messages.len() >= 4,
                "expected at least 4 messages, got {}: {:?}",
                messages.len(),
                messages
            );
            assert!(matches!(&messages[0], tau::types::Message::User(_)));
            assert!(matches!(&messages[1], tau::types::Message::Assistant(_)));
            assert!(matches!(&messages[2], tau::types::Message::ToolResult(tr) if tr.is_error));
            assert!(matches!(&messages[3], tau::types::Message::Assistant(_)));
        }
        other => panic!("expected Messages, got {:?}", other),
    }

    server.shutdown();
}

#[test]
fn server_chat_error_preserves_partial_messages() {
    // First response is a tool call (will error since no worker), second mock not reached.
    // The important thing: partial messages (assistant + error tool_result) are persisted.
    let server = TestServer::start(vec![
        MockResponse::ToolCalls(vec![tau::types::ToolCall {
            id: "tc1".into(),
            name: "some_tool".into(),
            arguments: serde_json::json!({"x": 1}),
        }]),
        MockResponse::Text("after tool".into()),
    ]);

    let conn = server.connect();
    let sid = match send_recv(
        &conn,
        &Request::CreateSession {
            model: None,
            provider: None,
            system_prompt: Some("test".into()),
            cwd: Some("/tmp".into()),
            parent_id: None,
            child_budget: 0,
            tagline: None,
        },
    ) {
        Response::SessionCreated { session_id } => session_id,
        other => panic!("{:?}", other),
    };

    // Chat
    let conn2 = server.connect();
    let responses = send_recv_all(
        &conn2,
        &Request::Chat {
            session_id: sid.clone(),
            text: "do something".into(),
        },
    );

    // Should end with AgentDone (even on error)
    let has_done = responses.iter().any(|r| matches!(r, Response::AgentDone));
    assert!(has_done, "expected AgentDone: {:?}", responses);

    // Verify partial messages are persisted
    let conn3 = server.connect();
    let resp = send_recv(
        &conn3,
        &Request::GetMessages {
            session_id: sid.clone(),
        },
    );
    match resp {
        Response::Messages { messages } => {
            // Should have at minimum: user + first assistant + tool_result(error)
            assert!(
                messages.len() >= 3,
                "expected at least 3 messages (user + assistant + tool_result), got {}: {:?}",
                messages.len(),
                messages
            );
            assert!(matches!(&messages[0], tau::types::Message::User(_)));
            assert!(matches!(&messages[1], tau::types::Message::Assistant(_)));
            assert!(matches!(&messages[2], tau::types::Message::ToolResult(_)));
        }
        other => panic!("expected Messages, got {:?}", other),
    }

    server.shutdown();
}

#[test]
fn server_session_resume_after_restart() {
    // Test that messages survive across server restarts
    let dir = tempfile::tempdir().unwrap();
    let sock_path = dir.path().join("test.sock");
    let db_path = dir.path().join("test.db");

    // Start server 1
    let model = mock_model();
    let mut registry = tau::provider::ProviderRegistry::new();
    registry.register(MockProvider::new(vec![MockResponse::Text(
        "first response".into(),
    )]));

    let config = tau::server::TestServerConfig {
        registry,
        models: vec![model],
        socket_path: sock_path.clone(),
        db_path: db_path.clone(),
    };

    let handle = std::thread::spawn(move || {
        smol::block_on(async {
            tau::server::run_with_config(config).await.ok();
        });
    });

    for _ in 0..50 {
        if sock_path.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    let conn = UnixStream::connect(&sock_path).unwrap();
    conn.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let sid = match send_recv(
        &conn,
        &Request::CreateSession {
            model: None,
            provider: None,
            system_prompt: Some("test".into()),
            cwd: Some("/tmp".into()),
            parent_id: None,
            child_budget: 0,
            tagline: None,
        },
    ) {
        Response::SessionCreated { session_id } => session_id,
        other => panic!("{:?}", other),
    };

    let conn2 = UnixStream::connect(&sock_path).unwrap();
    conn2
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let responses = send_recv_all(
        &conn2,
        &Request::Chat {
            session_id: sid.clone(),
            text: "hello".into(),
        },
    );
    assert!(responses.iter().any(|r| matches!(r, Response::AgentDone)));

    // Shutdown server 1
    let conn3 = UnixStream::connect(&sock_path).unwrap();
    conn3
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    send_recv(&conn3, &Request::Shutdown { restart: false });
    handle.join().ok();

    // Start server 2 with same DB
    std::fs::remove_file(&sock_path).ok();
    let model2 = mock_model();
    let mut registry2 = tau::provider::ProviderRegistry::new();
    registry2.register(MockProvider::new(vec![]));

    let config2 = tau::server::TestServerConfig {
        registry: registry2,
        models: vec![model2],
        socket_path: sock_path.clone(),
        db_path: db_path.clone(),
    };

    let _handle2 = std::thread::spawn(move || {
        smol::block_on(async {
            tau::server::run_with_config(config2).await.ok();
        });
    });

    for _ in 0..50 {
        if sock_path.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    // Verify messages survived
    let conn4 = UnixStream::connect(&sock_path).unwrap();
    conn4
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let resp = send_recv(
        &conn4,
        &Request::GetMessages {
            session_id: sid.clone(),
        },
    );
    match resp {
        Response::Messages { messages } => {
            assert!(
                messages.len() >= 2,
                "messages should survive restart, got {}: {:?}",
                messages.len(),
                messages
            );
        }
        other => panic!("{:?}", other),
    }

    // Shutdown server 2
    if let Ok(conn5) = UnixStream::connect(&sock_path) {
        let req = serde_json::to_string(&Request::Shutdown { restart: false }).unwrap();
        let mut c = conn5;
        let _ = c.write_all(format!("{}\n", req).as_bytes());
        let _ = c.flush();
    }
}
