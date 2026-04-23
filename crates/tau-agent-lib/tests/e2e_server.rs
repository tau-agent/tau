//! End-to-end test: start a server with mock provider, spawn sessions.

mod common;
use common::{CreateSessionBuilder, TestServer, send_recv, send_recv_all};

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::time::Duration;

use tau_agent_lib::protocol::{Request, Response};
use tau_agent_lib::providers::mock::{MockProvider, MockResponse, mock_model};

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn server_create_session_and_list() {
    let server = TestServer::start(vec![]);

    // Create a session with child_budget
    let resp = CreateSessionBuilder::new(&server)
        .cwd("/tmp")
        .child_budget(5)
        .send_raw();
    let session_id = match resp {
        Response::SessionCreated { session_id } => session_id,
        other => panic!("expected SessionCreated, got {:?}", other),
    };

    // List sessions -- should show the one we created
    let conn2 = server.connect();
    let resp = send_recv(
        &conn2,
        &Request::ListSessions {
            include_archived: false,
            project_name: None,
        },
    );
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
    let parent_id = match CreateSessionBuilder::new(&server)
        .cwd("/tmp")
        .child_budget(3)
        .send_raw()
    {
        Response::SessionCreated { session_id } => session_id,
        other => panic!("expected SessionCreated, got {:?}", other),
    };

    // Create child
    let child_id = match CreateSessionBuilder::new(&server)
        .parent(parent_id.clone())
        .send_raw()
    {
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
    let parent_id = match CreateSessionBuilder::new(&server)
        .child_budget(1)
        .send_raw()
    {
        Response::SessionCreated { session_id } => session_id,
        other => panic!("expected SessionCreated, got {:?}", other),
    };

    // Create first child (cost=1, fills budget)
    match CreateSessionBuilder::new(&server)
        .parent(parent_id.clone())
        .send_raw()
    {
        Response::SessionCreated { .. } => {}
        other => panic!("expected SessionCreated, got {:?}", other),
    }

    // Second child should fail -- budget exceeded
    match CreateSessionBuilder::new(&server)
        .parent(parent_id.clone())
        .send_raw()
    {
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
    let parent_id = match CreateSessionBuilder::new(&server)
        .child_budget(5)
        .send_raw()
    {
        Response::SessionCreated { session_id } => session_id,
        other => panic!("{:?}", other),
    };

    let child_id = match CreateSessionBuilder::new(&server)
        .parent(parent_id.clone())
        .send_raw()
    {
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
    let sid = match CreateSessionBuilder::new(&server).send_raw() {
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
    let sid = match CreateSessionBuilder::new(&server)
        .system_prompt("You are helpful.")
        .cwd("/tmp")
        .send_raw()
    {
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
            assert!(matches!(
                &messages[0],
                tau_agent_lib::types::Message::User(_)
            ));
            assert!(
                matches!(&messages[1], tau_agent_lib::types::Message::Assistant(a) if a.text().contains("Hello from mock!"))
            );
        }
        other => panic!("expected Messages, got {:?}", other),
    }

    server.shutdown();
}

/// Task 637: the server stamps `turn_started_at_ms` on every non-Idle
/// `StreamEvent::Phase` so the TUI can anchor its "Working... Xs"
/// counter to the true turn start. After the turn completes, the
/// anchor must be cleared (Phase(Idle) carries `None`).
#[test]
fn phase_events_carry_turn_started_at_ms() {
    use tau_agent_lib::types::{AgentPhase, StreamEvent, timestamp_ms};

    let server = TestServer::start(vec![MockResponse::Text("hello".into())]);
    let sid = match CreateSessionBuilder::new(&server).cwd("/tmp").send_raw() {
        Response::SessionCreated { session_id } => session_id,
        other => panic!("{:?}", other),
    };

    let before_ms = timestamp_ms();

    let conn = server.connect();
    let responses = send_recv_all(
        &conn,
        &Request::Chat {
            session_id: sid.clone(),
            text: "hi".into(),
        },
    );
    let after_ms = timestamp_ms();

    let mut non_idle_anchors: Vec<u64> = Vec::new();
    for r in &responses {
        if let Response::Stream { event } = r
            && let StreamEvent::Phase {
                phase,
                turn_started_at_ms,
            } = event.as_ref()
        {
            match phase {
                AgentPhase::Idle => {
                    assert!(
                        turn_started_at_ms.is_none(),
                        "Phase(Idle) must carry turn_started_at_ms=None, got {:?}",
                        turn_started_at_ms
                    );
                }
                _ => {
                    let ts = turn_started_at_ms.expect(
                        "non-Idle Phase events must carry a server-stamped turn_started_at_ms",
                    );
                    non_idle_anchors.push(ts);
                }
            }
        }
    }

    assert!(
        !non_idle_anchors.is_empty(),
        "expected at least one non-Idle Phase event carrying a turn anchor"
    );
    // All non-Idle events within a single turn share the same anchor,
    // and that anchor falls within the wall-clock window of the chat.
    let first = non_idle_anchors[0];
    for ts in &non_idle_anchors {
        assert_eq!(
            *ts, first,
            "all non-Idle Phase events in one turn must share the same anchor"
        );
    }
    assert!(
        first >= before_ms && first <= after_ms,
        "turn_started_at_ms={} outside chat window [{}, {}]",
        first,
        before_ms,
        after_ms
    );

    // After the turn completes, GetSessionInfo must report a None anchor.
    // Poll briefly in case the server is still finalising post-AgentDone
    // state (the terminal Phase(Idle) is emitted after AgentDone).
    let mut info = None;
    for _ in 0..40 {
        let info_resp = send_recv(
            &server.connect(),
            &Request::GetSessionInfo {
                session_id: sid.clone(),
            },
        );
        let si = match info_resp {
            Response::SessionInfo { info } => info,
            other => panic!("expected SessionInfo, got {:?}", other),
        };
        if si.turn_started_at_ms.is_none() {
            info = Some(si);
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let info = info.expect("SessionInfo::turn_started_at_ms did not reach None post-turn");
    assert!(
        info.turn_started_at_ms.is_none(),
        "SessionInfo::turn_started_at_ms must be None after AgentDone"
    );

    server.shutdown();
}

#[test]
fn server_chat_tool_call_loop() {
    // Without a worker plugin, tool calls will error ("no plugin provides tool").
    // The important thing is that the server handles this gracefully and
    // persists all messages (including the error tool result).
    let server = TestServer::start(vec![
        MockResponse::ToolCalls(vec![tau_agent_lib::types::ToolCall {
            id: "tc1".into(),
            name: "nonexistent_tool".into(),
            arguments: serde_json::json!({"arg": "value"}),
        }]),
        MockResponse::Text("I see the tool wasn't found.".into()),
    ]);

    let sid = match CreateSessionBuilder::new(&server)
        .system_prompt("You are helpful.")
        .cwd("/tmp")
        .send_raw()
    {
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
            assert!(matches!(
                &messages[0],
                tau_agent_lib::types::Message::User(_)
            ));
            assert!(matches!(
                &messages[1],
                tau_agent_lib::types::Message::Assistant(_)
            ));
            assert!(
                matches!(&messages[2], tau_agent_lib::types::Message::ToolResult(tr) if tr.is_error)
            );
            assert!(matches!(
                &messages[3],
                tau_agent_lib::types::Message::Assistant(_)
            ));
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
        MockResponse::ToolCalls(vec![tau_agent_lib::types::ToolCall {
            id: "tc1".into(),
            name: "some_tool".into(),
            arguments: serde_json::json!({"x": 1}),
        }]),
        MockResponse::Text("after tool".into()),
    ]);

    let sid = match CreateSessionBuilder::new(&server)
        .system_prompt("test")
        .cwd("/tmp")
        .send_raw()
    {
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
            assert!(matches!(
                &messages[0],
                tau_agent_lib::types::Message::User(_)
            ));
            assert!(matches!(
                &messages[1],
                tau_agent_lib::types::Message::Assistant(_)
            ));
            assert!(matches!(
                &messages[2],
                tau_agent_lib::types::Message::ToolResult(_)
            ));
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
    let mut registry = tau_agent_lib::provider::ProviderRegistry::new();
    registry.register(MockProvider::new(vec![MockResponse::Text(
        "first response".into(),
    )]));

    let config = tau_agent_lib::server::TestServerConfig {
        registry,
        models: vec![model],
        socket_path: sock_path.clone(),
        db_path: db_path.clone(),
        tool_executor_factory: None,
        mock_tools: vec![],
        plugins_config: None,
        aliases: std::collections::HashMap::new(),
    };

    let handle = std::thread::spawn(move || {
        smol::block_on(async {
            tau_agent_lib::server::run_with_config(config).await.ok();
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
        &CreateSessionBuilder::standalone()
            .system_prompt("test")
            .cwd("/tmp")
            .build(),
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
    let mut registry2 = tau_agent_lib::provider::ProviderRegistry::new();
    registry2.register(MockProvider::new(vec![]));

    let config2 = tau_agent_lib::server::TestServerConfig {
        registry: registry2,
        models: vec![model2],
        socket_path: sock_path.clone(),
        db_path: db_path.clone(),
        tool_executor_factory: None,
        mock_tools: vec![],
        plugins_config: None,
        aliases: std::collections::HashMap::new(),
    };

    let _handle2 = std::thread::spawn(move || {
        smol::block_on(async {
            tau_agent_lib::server::run_with_config(config2).await.ok();
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

// ---------------------------------------------------------------------------
// Message queue tests
// ---------------------------------------------------------------------------

#[test]
fn steer_queues_message_for_idle_session() {
    // Steer a message to an idle session.
    // With queue_and_maybe_resume, the steered message triggers an immediate
    // resume (agent turn) since the session is idle. We verify that:
    // 1. The steered message is persisted as a user message
    // 2. The resume processes it and produces an assistant response
    // 3. A subsequent Chat also works (messages accumulate)
    let server = TestServer::start(vec![
        // First response: consumed by the resume triggered by Steer on idle session
        MockResponse::Text("I processed the injected message.".into()),
        // Second response: consumed by the explicit Chat request
        MockResponse::Text("I see your hello.".into()),
    ]);
    let conn = server.connect();

    // Create session
    let sid = match CreateSessionBuilder::new(&server)
        .system_prompt("test")
        .send_raw()
    {
        Response::SessionCreated { session_id } => session_id,
        other => panic!("{:?}", other),
    };

    // Steer a message while session is idle -- triggers immediate resume
    let steer_resp = send_recv(
        &conn,
        &Request::Steer {
            session_id: sid.clone(),
            text: "injected message".into(),
        },
    );
    assert!(
        matches!(steer_resp, Response::Ok),
        "Steer should succeed for idle session, got: {:?}",
        steer_resp
    );

    // Wait for the resume to complete (session becomes idle again)
    let conn_wait = server.connect();
    let wait_resp = send_recv(
        &conn_wait,
        &Request::WaitSessions {
            session_ids: vec![sid.clone()],
            timeout_secs: 10,
        },
    );
    match wait_resp {
        Response::SessionsCompleted { results } => {
            assert_eq!(results[0].status, "done");
        }
        other => panic!("expected SessionsCompleted, got {:?}", other),
    }

    // Verify the steered message was persisted and processed
    let conn3 = server.connect();
    let resp = send_recv(
        &conn3,
        &Request::GetMessages {
            session_id: sid.clone(),
        },
    );
    match resp {
        Response::Messages { messages } => {
            assert!(
                messages.len() >= 2,
                "expected at least 2 messages after steer+resume, got {}: {:?}",
                messages.len(),
                messages
            );
            let has_injected = messages.iter().any(|m| {
                if let tau_agent_lib::types::Message::User(u) = m {
                    u.content.iter().any(|c| match c {
                        tau_agent_lib::types::UserContent::Text(t) => {
                            t.text.contains("injected message")
                        }
                        _ => false,
                    })
                } else {
                    false
                }
            });
            assert!(
                has_injected,
                "should contain injected message in persisted messages: {:?}",
                messages
            );
        }
        other => panic!("{:?}", other),
    }

    // Now send a Chat -- should work with accumulated history
    let conn2 = server.connect();
    let responses = send_recv_all(
        &conn2,
        &Request::Chat {
            session_id: sid.clone(),
            text: "hello".into(),
        },
    );
    let has_done = responses.iter().any(|r| matches!(r, Response::AgentDone));
    assert!(has_done, "expected AgentDone in responses: {:?}", responses);

    // Verify all messages: injected user + assistant + chat user + assistant = 4
    let conn4 = server.connect();
    let resp = send_recv(
        &conn4,
        &Request::GetMessages {
            session_id: sid.clone(),
        },
    );
    match resp {
        Response::Messages { messages } => {
            assert!(
                messages.len() >= 4,
                "expected at least 4 messages (steer+resume+chat+reply), got {}: {:?}",
                messages.len(),
                messages
            );
        }
        other => panic!("{:?}", other),
    }

    server.shutdown();
}

#[test]
fn queue_message_persists_across_operations() {
    // Queue a message, verify it's in the DB, drain it, verify it's gone.
    // This tests the DB methods directly without needing a server.
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let db = tau_agent_lib::db::Db::open(&db_path).unwrap();

    // Create a session
    let model = tau_agent_lib::providers::mock::mock_model();
    db.create_session(&tau_agent_lib::db::StoredSession {
        id: "s1".into(),
        model,
        system_prompt: None,
        cwd: None,
        is_subscription: false,
        created_at: 1000,
        parent_id: None,
        child_budget: 0,
        tagline: None,
        archived: false,
        last_exit_status: None,
        last_phase: None,
        auto_archive: false,
        notify_parent: true,
        project_name: None,
    })
    .unwrap();

    // Queue a message
    let id = db
        .queue_message("s1", "test content", "test_sender")
        .unwrap();
    assert!(id > 0);

    // Verify it's there
    assert!(db.has_queued_messages("s1").unwrap());
    let sessions = db.sessions_with_queued_messages().unwrap();
    assert_eq!(sessions, vec!["s1"]);

    // Drain it
    let messages = db.drain_queued_messages("s1").unwrap();
    assert_eq!(messages.len(), 1);

    // Verify queue is empty but message is persisted in messages table
    assert!(!db.has_queued_messages("s1").unwrap());
    let persisted = db.get_messages("s1").unwrap();
    assert_eq!(persisted.len(), 1);
}

// ---------------------------------------------------------------------------
// Mock tool executor tests
// ---------------------------------------------------------------------------

#[test]
fn server_chat_with_mock_tool_success() {
    use std::sync::Arc;
    use tau_agent_lib::providers::mock::{MockToolExecutor, MockToolResponse, mock_tool};

    let mock_executor = MockToolExecutor::new();
    let tool_handle = mock_executor.handle();
    let tool_handle_for_assert = mock_executor.handle();
    tool_handle.on_tool("read_file", MockToolResponse::Success("hello world".into()));

    let provider = MockProvider::new(vec![
        MockResponse::ToolCalls(vec![tau_agent_lib::types::ToolCall {
            id: "tc1".into(),
            name: "read_file".into(),
            arguments: serde_json::json!({"path": "/tmp/test.txt"}),
        }]),
        MockResponse::Text("The file contains hello world.".into()),
    ]);
    let provider_handle = provider.handle();

    let tool_factory: Arc<dyn Fn() -> Box<dyn tau_agent_lib::worker::ToolExecutor> + Send + Sync> =
        Arc::new(move || {
            let h = tool_handle.clone();
            Box::new(h.executor())
        });

    let dir = tempfile::tempdir().unwrap();
    let sock_path = dir.path().join("tau-test.sock");
    let db_path = dir.path().join("test.db");

    let model = mock_model();
    let mut registry = tau_agent_lib::provider::ProviderRegistry::new();
    registry.register(provider);

    let config = tau_agent_lib::server::TestServerConfig {
        registry,
        models: vec![model],
        socket_path: sock_path.clone(),
        db_path,
        tool_executor_factory: Some(tool_factory),
        mock_tools: vec![mock_tool("read_file", "Read a file")],
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

    for _ in 0..50 {
        if sock_path.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(sock_path.exists(), "server socket did not appear");

    let conn = UnixStream::connect(&sock_path).unwrap();
    conn.set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();

    // Create session
    let sid = match send_recv(
        &conn,
        &CreateSessionBuilder::standalone()
            .system_prompt("You are helpful.")
            .cwd("/tmp")
            .build(),
    ) {
        Response::SessionCreated { session_id } => session_id,
        other => panic!("expected SessionCreated, got {:?}", other),
    };

    // Chat -- triggers tool call → mock tool result → final response
    let conn2 = UnixStream::connect(&sock_path).unwrap();
    conn2
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    let responses = send_recv_all(
        &conn2,
        &Request::Chat {
            session_id: sid.clone(),
            text: "read /tmp/test.txt".into(),
        },
    );

    let has_done = responses.iter().any(|r| matches!(r, Response::AgentDone));
    assert!(has_done, "expected AgentDone in responses: {:?}", responses);

    // Verify messages: user + assistant(tool_call) + tool_result(success) + assistant(text)
    let conn3 = UnixStream::connect(&sock_path).unwrap();
    conn3
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let resp = send_recv(
        &conn3,
        &Request::GetMessages {
            session_id: sid.clone(),
        },
    );
    match resp {
        Response::Messages { messages } => {
            assert_eq!(
                messages.len(),
                4,
                "expected 4 messages (user + assistant + tool_result + assistant), got {}: {:?}",
                messages.len(),
                messages
            );
            assert!(matches!(
                &messages[0],
                tau_agent_lib::types::Message::User(_)
            ));
            assert!(matches!(
                &messages[1],
                tau_agent_lib::types::Message::Assistant(_)
            ));
            // Tool result should NOT be an error (mock returned Success)
            assert!(
                matches!(&messages[2], tau_agent_lib::types::Message::ToolResult(tr) if !tr.is_error)
            );
            assert!(
                matches!(&messages[3], tau_agent_lib::types::Message::Assistant(a) if a.text().contains("hello world"))
            );
        }
        other => panic!("expected Messages, got {:?}", other),
    }

    // Verify mock tool was called
    let tool_captures = tool_handle_for_assert.captures();
    assert_eq!(tool_captures.len(), 1);
    assert_eq!(tool_captures[0].tool_call.name, "read_file");

    // Verify provider saw tool result in context on second call
    let captures = provider_handle.captures();
    assert_eq!(captures.len(), 2);
    let second_ctx = &captures[1].context;
    assert!(second_ctx.messages.iter().any(
        |m| matches!(m, tau_agent_lib::types::Message::ToolResult(tr) if tr.content.iter().any(|c|
            matches!(c, tau_agent_lib::types::ToolResultContent::Text(t) if t.text.contains("hello world"))
        ))
    ));

    // Shutdown
    let conn4 = UnixStream::connect(&sock_path).unwrap();
    conn4
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    send_recv(&conn4, &Request::Shutdown { restart: false });
}

#[test]
fn server_chat_with_mock_tool_error() {
    // Test that a tool returning is_error=true is handled correctly:
    // the error result is passed back to the LLM which can respond.
    use std::sync::Arc;
    use tau_agent_lib::providers::mock::{MockToolExecutor, MockToolResponse, mock_tool};

    let mock_executor = MockToolExecutor::new();
    let tool_handle = mock_executor.handle();
    let tool_handle_for_assert = mock_executor.handle();
    tool_handle.on_tool(
        "read_file",
        MockToolResponse::ToolError("permission denied".into()),
    );

    let provider = MockProvider::new(vec![
        MockResponse::ToolCalls(vec![tau_agent_lib::types::ToolCall {
            id: "tc1".into(),
            name: "read_file".into(),
            arguments: serde_json::json!({"path": "/etc/shadow"}),
        }]),
        MockResponse::Text("Sorry, I can't read that file.".into()),
    ]);
    let provider_handle = provider.handle();

    let tool_factory: Arc<dyn Fn() -> Box<dyn tau_agent_lib::worker::ToolExecutor> + Send + Sync> =
        Arc::new(move || {
            let h = tool_handle.clone();
            Box::new(h.executor())
        });

    let server = TestServer::start_with_config(vec![], |mut config| {
        config.registry = {
            let mut r = tau_agent_lib::provider::ProviderRegistry::new();
            r.register(provider);
            r
        };
        config.tool_executor_factory = Some(tool_factory);
        config.mock_tools = vec![mock_tool("read_file", "Read a file")];
        config
    });

    let sid = match CreateSessionBuilder::new(&server)
        .system_prompt("test")
        .cwd("/tmp")
        .send_raw()
    {
        Response::SessionCreated { session_id } => session_id,
        other => panic!("{:?}", other),
    };

    let conn2 = server.connect();
    let responses = send_recv_all(
        &conn2,
        &Request::Chat {
            session_id: sid.clone(),
            text: "read /etc/shadow".into(),
        },
    );
    assert!(responses.iter().any(|r| matches!(r, Response::AgentDone)));

    // Verify tool result has is_error=true
    let conn3 = server.connect();
    let resp = send_recv(
        &conn3,
        &Request::GetMessages {
            session_id: sid.clone(),
        },
    );
    match resp {
        Response::Messages { messages } => {
            assert_eq!(messages.len(), 4);
            assert!(
                matches!(&messages[2], tau_agent_lib::types::Message::ToolResult(tr) if tr.is_error)
            );
        }
        other => panic!("{:?}", other),
    }

    // Verify provider saw the error in context
    let captures = provider_handle.captures();
    assert_eq!(captures.len(), 2);
    let second_ctx = &captures[1].context;
    assert!(second_ctx.messages.iter().any(|m|
        matches!(m, tau_agent_lib::types::Message::ToolResult(tr)
            if tr.is_error && tr.content.iter().any(|c|
                matches!(c, tau_agent_lib::types::ToolResultContent::Text(t) if t.text.contains("permission denied"))
            )
        )
    ));

    // Verify tool capture
    let tool_captures = tool_handle_for_assert.captures();
    assert_eq!(tool_captures.len(), 1);
    assert_eq!(tool_captures[0].tool_call.name, "read_file");

    server.shutdown();
}

#[test]
fn server_chat_multi_tool_calls() {
    // Test multiple tool calls in a single turn, each with different mock results.
    use std::sync::Arc;
    use tau_agent_lib::providers::mock::{MockToolExecutor, MockToolResponse, mock_tool};

    let mock_executor = MockToolExecutor::new();
    let tool_handle = mock_executor.handle();
    let tool_handle_for_assert = mock_executor.handle();
    tool_handle.on_tool(
        "read_file",
        MockToolResponse::Success("file content A".into()),
    );
    tool_handle.on_tool(
        "list_dir",
        MockToolResponse::Success("file1.txt\nfile2.txt".into()),
    );

    let provider = MockProvider::new(vec![
        MockResponse::ToolCalls(vec![
            tau_agent_lib::types::ToolCall {
                id: "tc1".into(),
                name: "read_file".into(),
                arguments: serde_json::json!({"path": "a.txt"}),
            },
            tau_agent_lib::types::ToolCall {
                id: "tc2".into(),
                name: "list_dir".into(),
                arguments: serde_json::json!({"path": "/tmp"}),
            },
        ]),
        MockResponse::Text("I found 2 files and read file A.".into()),
    ]);
    let provider_handle = provider.handle();

    let tool_factory: Arc<dyn Fn() -> Box<dyn tau_agent_lib::worker::ToolExecutor> + Send + Sync> =
        Arc::new(move || {
            let h = tool_handle.clone();
            Box::new(h.executor())
        });

    let server = TestServer::start_with_config(vec![], |mut config| {
        config.registry = {
            let mut r = tau_agent_lib::provider::ProviderRegistry::new();
            r.register(provider);
            r
        };
        config.tool_executor_factory = Some(tool_factory);
        config.mock_tools = vec![
            mock_tool("read_file", "Read a file"),
            mock_tool("list_dir", "List directory"),
        ];
        config
    });

    let sid = match CreateSessionBuilder::new(&server)
        .system_prompt("test")
        .cwd("/tmp")
        .send_raw()
    {
        Response::SessionCreated { session_id } => session_id,
        other => panic!("{:?}", other),
    };

    let conn2 = server.connect();
    let responses = send_recv_all(
        &conn2,
        &Request::Chat {
            session_id: sid.clone(),
            text: "read a.txt and list /tmp".into(),
        },
    );
    assert!(responses.iter().any(|r| matches!(r, Response::AgentDone)));

    // Verify messages: user + assistant(2 tool calls) + 2 tool results + assistant(text)
    let conn3 = server.connect();
    let resp = send_recv(
        &conn3,
        &Request::GetMessages {
            session_id: sid.clone(),
        },
    );
    match resp {
        Response::Messages { messages } => {
            // user + assistant + tool_result + tool_result + assistant = 5
            assert_eq!(messages.len(), 5, "got {:?}", messages);
            assert!(
                matches!(&messages[2], tau_agent_lib::types::Message::ToolResult(tr) if !tr.is_error)
            );
            assert!(
                matches!(&messages[3], tau_agent_lib::types::Message::ToolResult(tr) if !tr.is_error)
            );
        }
        other => panic!("{:?}", other),
    }

    // Both tools were called
    let tool_captures = tool_handle_for_assert.captures();
    assert_eq!(tool_captures.len(), 2);
    assert_eq!(tool_captures[0].tool_call.name, "read_file");
    assert_eq!(tool_captures[1].tool_call.name, "list_dir");

    // Provider's second call sees both tool results
    let captures = provider_handle.captures();
    assert_eq!(captures.len(), 2);
    let tool_results: Vec<_> = captures[1]
        .context
        .messages
        .iter()
        .filter(|m| matches!(m, tau_agent_lib::types::Message::ToolResult(_)))
        .collect();
    assert_eq!(tool_results.len(), 2);

    server.shutdown();
}

#[test]
fn server_chat_multi_turn_tool_loop() {
    // Test: LLM makes tool call → gets result → makes another tool call → gets result → text
    // This verifies the agent loop handles multiple consecutive tool turns correctly.
    use std::sync::Arc;
    use tau_agent_lib::providers::mock::{MockToolExecutor, MockToolResponse, mock_tool};

    let mock_executor = MockToolExecutor::new();
    let tool_handle = mock_executor.handle();
    let tool_handle_for_assert = mock_executor.handle();
    tool_handle.on_tool(
        "list_dir",
        MockToolResponse::Success("readme.md\nsrc/".into()),
    );
    tool_handle.on_tool(
        "read_file",
        MockToolResponse::Success("# My Project\nHello world".into()),
    );

    let provider = MockProvider::new(vec![
        // Turn 1: LLM calls list_dir
        MockResponse::ToolCalls(vec![tau_agent_lib::types::ToolCall {
            id: "tc1".into(),
            name: "list_dir".into(),
            arguments: serde_json::json!({"path": "."}),
        }]),
        // Turn 2: LLM sees directory listing, calls read_file
        MockResponse::ToolCalls(vec![tau_agent_lib::types::ToolCall {
            id: "tc2".into(),
            name: "read_file".into(),
            arguments: serde_json::json!({"path": "readme.md"}),
        }]),
        // Turn 3: LLM has all info, responds with text
        MockResponse::Text("The project README says Hello world.".into()),
    ]);
    let provider_handle = provider.handle();

    let tool_factory: Arc<dyn Fn() -> Box<dyn tau_agent_lib::worker::ToolExecutor> + Send + Sync> =
        Arc::new(move || {
            let h = tool_handle.clone();
            Box::new(h.executor())
        });

    let server = TestServer::start_with_config(vec![], |mut config| {
        config.registry = {
            let mut r = tau_agent_lib::provider::ProviderRegistry::new();
            r.register(provider);
            r
        };
        config.tool_executor_factory = Some(tool_factory);
        config.mock_tools = vec![
            mock_tool("list_dir", "List directory contents"),
            mock_tool("read_file", "Read a file"),
        ];
        config
    });

    let sid = match CreateSessionBuilder::new(&server)
        .system_prompt("test")
        .cwd("/tmp")
        .send_raw()
    {
        Response::SessionCreated { session_id } => session_id,
        other => panic!("{:?}", other),
    };

    let conn2 = server.connect();
    let responses = send_recv_all(
        &conn2,
        &Request::Chat {
            session_id: sid.clone(),
            text: "summarize the project".into(),
        },
    );
    assert!(responses.iter().any(|r| matches!(r, Response::AgentDone)));

    // Verify messages:
    // user + assistant(tc1) + tool_result_1 + assistant(tc2) + tool_result_2 + assistant(text) = 6
    let conn3 = server.connect();
    let resp = send_recv(
        &conn3,
        &Request::GetMessages {
            session_id: sid.clone(),
        },
    );
    match resp {
        Response::Messages { messages } => {
            assert_eq!(
                messages.len(),
                6,
                "expected 6 messages (user + 2*(assistant+tool_result) + final_assistant), got {}: {:?}",
                messages.len(),
                messages
            );
            assert!(matches!(
                &messages[0],
                tau_agent_lib::types::Message::User(_)
            ));
            assert!(matches!(
                &messages[1],
                tau_agent_lib::types::Message::Assistant(_)
            ));
            assert!(
                matches!(&messages[2], tau_agent_lib::types::Message::ToolResult(tr) if !tr.is_error)
            );
            assert!(matches!(
                &messages[3],
                tau_agent_lib::types::Message::Assistant(_)
            ));
            assert!(
                matches!(&messages[4], tau_agent_lib::types::Message::ToolResult(tr) if !tr.is_error)
            );
            assert!(
                matches!(&messages[5], tau_agent_lib::types::Message::Assistant(a) if a.text().contains("Hello world"))
            );
        }
        other => panic!("{:?}", other),
    }

    // Verify 3 LLM calls with growing context
    let captures = provider_handle.captures();
    assert_eq!(captures.len(), 3);

    // Call 1: just user message
    assert_eq!(captures[0].context.messages.len(), 1);

    // Call 2: user + assistant(tc1) + tool_result_1
    assert_eq!(captures[1].context.messages.len(), 3);
    assert!(matches!(&captures[1].context.messages[2],
        tau_agent_lib::types::Message::ToolResult(tr) if tr.tool_name == "list_dir"));

    // Call 3: user + assistant(tc1) + tool_result_1 + assistant(tc2) + tool_result_2
    assert_eq!(captures[2].context.messages.len(), 5);
    assert!(matches!(&captures[2].context.messages[4],
        tau_agent_lib::types::Message::ToolResult(tr) if tr.tool_name == "read_file"));

    // Verify both tools were called in order
    let tool_captures = tool_handle_for_assert.captures();
    assert_eq!(tool_captures.len(), 2);
    assert_eq!(tool_captures[0].tool_call.name, "list_dir");
    assert_eq!(tool_captures[1].tool_call.name, "read_file");

    server.shutdown();
}

#[test]
fn server_chat_tool_schemas_in_context() {
    // Verify that mock tool schemas appear in the Context.tools field
    // that the provider sees.
    use std::sync::Arc;
    use tau_agent_lib::providers::mock::{MockToolExecutor, MockToolResponse, mock_tool};

    let mock_executor = MockToolExecutor::new();
    let tool_handle = mock_executor.handle();
    tool_handle.set_default(MockToolResponse::Success("ok".into()));

    let provider = MockProvider::new(vec![MockResponse::Text("I see the tools.".into())]);
    let provider_handle = provider.handle();

    let tool_factory: Arc<dyn Fn() -> Box<dyn tau_agent_lib::worker::ToolExecutor> + Send + Sync> =
        Arc::new(move || {
            let h = tool_handle.clone();
            Box::new(h.executor())
        });

    let server = TestServer::start_with_config(vec![], |mut config| {
        config.registry = {
            let mut r = tau_agent_lib::provider::ProviderRegistry::new();
            r.register(provider);
            r
        };
        config.tool_executor_factory = Some(tool_factory);
        config.mock_tools = vec![
            mock_tool("bash", "Execute a shell command"),
            mock_tool("read_file", "Read contents of a file"),
            mock_tool("write_file", "Write contents to a file"),
        ];
        config
    });

    let sid = match CreateSessionBuilder::new(&server)
        .system_prompt("test")
        .cwd("/tmp")
        .send_raw()
    {
        Response::SessionCreated { session_id } => session_id,
        other => panic!("{:?}", other),
    };

    let conn2 = server.connect();
    let responses = send_recv_all(
        &conn2,
        &Request::Chat {
            session_id: sid.clone(),
            text: "hello".into(),
        },
    );
    assert!(responses.iter().any(|r| matches!(r, Response::AgentDone)));

    // Verify mock tools appeared in the context
    let captures = provider_handle.captures();
    assert_eq!(captures.len(), 1);
    let tools = &captures[0].context.tools;
    assert_eq!(
        tools.len(),
        3,
        "expected 3 mock tools, got {}: {:?}",
        tools.len(),
        tools
    );

    let tool_names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
    assert!(tool_names.contains(&"bash"), "tools: {:?}", tool_names);
    assert!(tool_names.contains(&"read_file"), "tools: {:?}", tool_names);
    assert!(
        tool_names.contains(&"write_file"),
        "tools: {:?}",
        tool_names
    );

    server.shutdown();
}

// ---------------------------------------------------------------------------
// Session dump and replay tests
// ---------------------------------------------------------------------------

#[test]
fn session_dump_and_replay() {
    use std::sync::Arc;
    use tau_agent_lib::providers::mock::{MockToolExecutor, MockToolResponse, mock_tool};

    // Set up a server with mock tools
    let mock_executor = MockToolExecutor::new();
    let tool_handle = mock_executor.handle();
    tool_handle.on_tool("bash", MockToolResponse::Success("hello world".into()));

    let tool_factory: Arc<dyn Fn() -> Box<dyn tau_agent_lib::worker::ToolExecutor> + Send + Sync> =
        Arc::new(move || {
            let h = tool_handle.clone();
            Box::new(h.executor())
        });

    let dir = tempfile::tempdir().unwrap();
    let sock_path = dir.path().join("tau-test.sock");
    let db_path = dir.path().join("test.db");

    let model = mock_model();
    let mut registry = tau_agent_lib::provider::ProviderRegistry::new();
    registry.register(MockProvider::new(vec![
        MockResponse::ToolCalls(vec![tau_agent_lib::types::ToolCall {
            id: "tc1".into(),
            name: "bash".into(),
            arguments: serde_json::json!({"command": "echo hello world"}),
        }]),
        MockResponse::Text("The command output hello world.".into()),
    ]));

    let config = tau_agent_lib::server::TestServerConfig {
        registry,
        models: vec![model],
        socket_path: sock_path.clone(),
        db_path: db_path.clone(),
        tool_executor_factory: Some(tool_factory),
        mock_tools: vec![mock_tool("bash", "Run a command")],
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

    for _ in 0..50 {
        if sock_path.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    // Create session and chat
    let conn = UnixStream::connect(&sock_path).unwrap();
    conn.set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    let sid = match send_recv(
        &conn,
        &CreateSessionBuilder::standalone()
            .system_prompt("You are helpful.")
            .cwd("/tmp")
            .build(),
    ) {
        Response::SessionCreated { session_id } => session_id,
        other => panic!("{:?}", other),
    };

    let conn2 = UnixStream::connect(&sock_path).unwrap();
    conn2
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    let responses = send_recv_all(
        &conn2,
        &Request::Chat {
            session_id: sid.clone(),
            text: "run echo hello world".into(),
        },
    );
    assert!(responses.iter().any(|r| matches!(r, Response::AgentDone)));

    // Shutdown server so we can access the DB directly
    let conn3 = UnixStream::connect(&sock_path).unwrap();
    conn3
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    send_recv(&conn3, &Request::Shutdown { restart: false });
    std::thread::sleep(Duration::from_millis(200));

    // Dump the session from DB
    let db = tau_agent_lib::db::Db::open(&db_path).unwrap();
    let recording = tau_agent_lib::replay::dump_session(&db, &sid).unwrap();

    // Verify recording structure
    assert_eq!(
        recording.turns.len(),
        2,
        "expected 2 turns (tool_call + text)"
    );
    assert_eq!(
        recording.turns[0].user_message.as_deref(),
        Some("run echo hello world")
    );
    assert_eq!(recording.turns[0].tool_results.len(), 1);
    assert!(!recording.turns[0].tool_results[0].is_error);
    assert!(recording.turns[1].user_message.is_none()); // continuation
    assert!(
        recording.turns[1]
            .assistant_message
            .text()
            .contains("hello world")
    );

    // Verify JSON roundtrip
    let json = serde_json::to_string_pretty(&recording).unwrap();
    let parsed: tau_agent_lib::replay::SessionRecording = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.turns.len(), 2);

    // Replay the recording
    let result = smol::block_on(tau_agent_lib::replay::replay_session(&recording));
    assert!(
        result.success,
        "replay should succeed: error={:?}, turns={:?}",
        result.error, result.turn_results
    );
    assert_eq!(result.turn_results.len(), 2);
    assert!(
        result.turn_results[0].tool_calls_match,
        "turn 0: {:?}",
        result.turn_results[0]
    );
    assert!(
        result.turn_results[0].tool_results_match,
        "turn 0: {:?}",
        result.turn_results[0]
    );
    assert!(
        result.turn_results[1].text_match,
        "turn 1: {:?}",
        result.turn_results[1]
    );
}

// ---------------------------------------------------------------------------
// Background ServerRequest e2e test
// ---------------------------------------------------------------------------

/// Test that a global plugin can send ServerRequests outside of tool calls.
///
/// Spawns a tiny bash-based global plugin that:
/// 1. Registers (no tools, no hooks).
/// 2. Sends three `ListSessions` ServerRequests immediately after startup.
/// 3. Collects the ServerResponse messages and writes them to a results file.
///
/// The test then verifies that all three responses arrived with the correct
/// type (`sessions`).
#[test]
fn global_plugin_background_server_requests() {
    let tmp = tempfile::tempdir().unwrap();
    let results_file = tmp.path().join("bg_results.txt");
    let plugin_script = tmp.path().join("bg_plugin.sh");

    // Write a bash plugin that sends background ServerRequests.
    // Protocol: JSON lines on stdin/stdout.
    //
    // Inside the format!() string, `{{` / `}}` produce literal braces in the
    // generated bash script.  `{results}` is the only interpolation point.
    let script = format!(
        r#"#!/bin/bash
set -eu
RESULTS="{results}"

# Registration: no tools, no hooks
echo '{{"type":"register","name":"bg-test","tools":[],"hooks":[],"commands":[]}}'

# Send three ListSessions ServerRequests (background, no tool call active)
for i in 1 2 3; do
    echo '{{"type":"server_request","request_id":"bg-'"$i"'","request":{{"type":"list_sessions","include_archived":false}}}}'
done

# Read responses and record them
COUNT=0
while IFS= read -r line; do
    case "$line" in
        *server_response*)
            COUNT=$((COUNT + 1))
            echo "$line" >> "$RESULTS"
            if [ "$COUNT" -ge 3 ]; then
                break
            fi
            ;;
        *idle*)
            exit 0
            ;;
    esac
done
# Keep reading so the process stays alive for the server
while IFS= read -r line; do
    case "$line" in
        *idle*) exit 0 ;;
    esac
done
"#,
        results = results_file.display()
    );
    std::fs::write(&plugin_script, &script).unwrap();

    // Make executable
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&plugin_script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let plugins_config = tau_agent_lib::plugin::PluginsConfig {
        no_default_worker: true,
        global: [(
            "bg-test".to_string(),
            tau_agent_lib::plugin::PluginEntry {
                command: vec!["bash".into(), plugin_script.to_string_lossy().into()],
                env: Default::default(),
            },
        )]
        .into_iter()
        .collect(),
        ..Default::default()
    };

    let server = TestServer::start_with_config(vec![], |mut config| {
        config.plugins_config = Some(plugins_config);
        config
    });

    // Poll until the results file has all three responses (up to 4s).
    for _ in 0..40 {
        if results_file.exists() {
            let contents = std::fs::read_to_string(&results_file).unwrap_or_default();
            if contents.lines().filter(|l| !l.is_empty()).count() >= 3 {
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    // Verify the results file was written with three responses.
    let contents = std::fs::read_to_string(&results_file).unwrap_or_else(|e| {
        panic!(
            "results file not found at {}: {}",
            results_file.display(),
            e
        )
    });
    let lines: Vec<&str> = contents.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(
        lines.len(),
        3,
        "expected 3 background responses, got {}: {:?}",
        lines.len(),
        lines
    );

    // Each line should be a server_response with type "sessions".
    for (i, line) in lines.iter().enumerate() {
        let parsed: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("line {} not valid JSON: {}\n  line: {}", i, e, line));
        assert_eq!(
            parsed["type"].as_str(),
            Some("server_response"),
            "line {}: expected server_response, got {:?}",
            i,
            parsed
        );
        assert_eq!(
            parsed["response"]["type"].as_str(),
            Some("sessions"),
            "line {}: expected sessions response, got {:?}",
            i,
            parsed["response"]
        );
    }

    server.shutdown();
}

/// Test that a global plugin can create a session via background ServerRequest
/// and that the session is visible via the normal client API.
#[test]
fn global_plugin_background_create_session() {
    let tmp = tempfile::tempdir().unwrap();
    let results_file = tmp.path().join("bg_create_results.txt");
    let plugin_script = tmp.path().join("bg_create_plugin.sh");

    let script = format!(
        r#"#!/bin/bash
set -eu
RESULTS="{results}"

# Registration
echo '{{"type":"register","name":"bg-create-test","tools":[],"hooks":[],"commands":[]}}'

# Create a session via background ServerRequest
echo '{{"type":"server_request","request_id":"create-1","request":{{"type":"create_session","model":null,"provider":null,"system_prompt":"bg-created","cwd":"/tmp","parent_id":null,"child_budget":0,"tagline":"background-test","auto_archive":false}}}}'

# Read the response
while IFS= read -r line; do
    case "$line" in
        *server_response*)
            echo "$line" >> "$RESULTS"
            break
            ;;
        *idle*)
            exit 0
            ;;
    esac
done
# Stay alive
while IFS= read -r line; do
    case "$line" in
        *idle*) exit 0 ;;
    esac
done
"#,
        results = results_file.display()
    );
    std::fs::write(&plugin_script, &script).unwrap();

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&plugin_script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let plugins_config = tau_agent_lib::plugin::PluginsConfig {
        no_default_worker: true,
        global: [(
            "bg-create-test".to_string(),
            tau_agent_lib::plugin::PluginEntry {
                command: vec!["bash".into(), plugin_script.to_string_lossy().into()],
                env: Default::default(),
            },
        )]
        .into_iter()
        .collect(),
        ..Default::default()
    };

    let server = TestServer::start_with_config(vec![], |mut config| {
        config.plugins_config = Some(plugins_config);
        config
    });

    // Wait for the results file.
    for _ in 0..40 {
        if results_file.exists() {
            let contents = std::fs::read_to_string(&results_file).unwrap_or_default();
            if contents.lines().filter(|l| !l.is_empty()).count() >= 1 {
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    // Parse the create-session response.
    let contents = std::fs::read_to_string(&results_file).unwrap_or_else(|e| {
        panic!(
            "results file not found at {}: {}",
            results_file.display(),
            e
        )
    });
    let line = contents.lines().next().unwrap();
    let parsed: serde_json::Value = serde_json::from_str(line).unwrap();
    assert_eq!(parsed["response"]["type"].as_str(), Some("session_created"));
    let session_id = parsed["response"]["session_id"].as_str().unwrap();

    // Verify the session is visible via the normal client ListSessions API.
    let conn = server.connect();
    let resp = send_recv(
        &conn,
        &Request::ListSessions {
            include_archived: false,
            project_name: None,
        },
    );
    match resp {
        Response::Sessions { sessions } => {
            assert!(
                sessions.iter().any(|s| s.id == session_id),
                "session {} created by background plugin not found in {:?}",
                session_id,
                sessions.iter().map(|s| &s.id).collect::<Vec<_>>()
            );
            let sess = sessions.iter().find(|s| s.id == session_id).unwrap();
            assert_eq!(sess.tagline.as_deref(), Some("background-test"));
        }
        other => panic!("expected Sessions, got {:?}", other),
    }

    server.shutdown();
}

/// Test that global plugin tools are included in the LLM context for sessions
/// created via a background ServerRequest (simulating task_dispatch behavior).
///
/// This is the regression test for the bug where dispatched task sessions
/// didn't have task tools in their LLM tool definitions, causing agents to
/// try to call task_get/task_assign as bash commands.
///
/// Scenario:
/// 1. Global plugin registers with a custom tool ("dispatch_test_tool")
/// 2. Plugin creates a new session via background ServerRequest
/// 3. Plugin sends a Chat request for that session
/// 4. We verify that "dispatch_test_tool" appears in the LLM context
#[test]
fn global_plugin_tools_in_dispatched_session_context() {
    use std::time::Duration;
    use tau_agent_lib::providers::mock::{MockProvider, MockResponse};

    let tmp = tempfile::tempdir().unwrap();
    let results_file = tmp.path().join("dispatch_test_results.txt");
    let plugin_script = tmp.path().join("dispatch_test_plugin.sh");

    // This plugin:
    // 1. Registers with one tool ("dispatch_test_tool")
    // 2. Creates a new session via background ServerRequest
    // 3. Sends a Chat request to that session
    // 4. Loops waiting for tool calls (the child session may call dispatch_test_tool)
    let script = format!(
        r#"#!/bin/bash
set -eu
RESULTS="{results}"
SESSION_ID=""

# Registration with one tool
echo '{{"type":"register","name":"dispatch-test","tools":[{{"name":"dispatch_test_tool","description":"Test tool for dispatch verification","parameters":{{"type":"object","properties":{{}}}},"prompt_snippet":"Use dispatch_test_tool for dispatch verification","prompt_guidelines":[]}}],"hooks":[],"commands":[]}}'

# Create a session via background ServerRequest
echo '{{"type":"server_request","request_id":"create-dispatch-1","request":{{"type":"create_session","model":null,"provider":null,"system_prompt":null,"cwd":"/tmp","parent_id":null,"child_budget":4,"tagline":"dispatch-test-session","auto_archive":false,"notify_parent":false}}}}'

# Main loop: handle messages
while IFS= read -r line; do
    case "$line" in
        *'"request_id":"create-dispatch-1"'*)
            # Extract session_id from create session response
            SESSION_ID=$(echo "$line" | grep -o '"session_id":"[^"]*"' | head -1 | cut -d'"' -f4)
            echo "$SESSION_ID" >> "$RESULTS"
            # Send a Chat request for the new session
            echo '{{"type":"server_request","request_id":"chat-dispatch-1","request":{{"type":"chat","session_id":"'"$SESSION_ID"'","text":"test dispatch tools"}}}}'
            ;;
        *'"request_id":"chat-dispatch-1"'*)
            echo "chat_sent" >> "$RESULTS"
            ;;
        *'"type":"tool_call"'*)
            TCID=$(echo "$line" | grep -o '"tool_call_id":"[^"]*"' | head -1 | cut -d'"' -f4)
            echo '{{"type":"tool_result","tool_call_id":"'"$TCID"'","content":[{{"type":"text","text":"DISPATCH_TOOL_RESULT"}}],"is_error":false}}'
            ;;
        *idle*)
            exit 0
            ;;
    esac
done
"#,
        results = results_file.display()
    );
    std::fs::write(&plugin_script, &script).unwrap();

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&plugin_script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let plugins_config = tau_agent_lib::plugin::PluginsConfig {
        no_default_worker: true,
        global: [(
            "dispatch-test".to_string(),
            tau_agent_lib::plugin::PluginEntry {
                command: vec!["bash".into(), plugin_script.to_string_lossy().into()],
                env: Default::default(),
            },
        )]
        .into_iter()
        .collect(),
        ..Default::default()
    };

    // Set up a mock provider that captures the LLM context
    let provider = MockProvider::new(vec![MockResponse::Text("done".into())]);
    let provider_handle = provider.handle();

    let server = TestServer::start_with_config(vec![], |mut config| {
        config.registry = {
            let mut r = tau_agent_lib::provider::ProviderRegistry::new();
            r.register(provider);
            r
        };
        config.plugins_config = Some(plugins_config);
        config
    });

    // Wait for the plugin to create a session and send a chat
    for _ in 0..60 {
        if results_file.exists() {
            let contents = std::fs::read_to_string(&results_file).unwrap_or_default();
            if contents.lines().filter(|l| l == &"chat_sent").count() >= 1 {
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    // Give the child chat time to complete
    std::thread::sleep(Duration::from_millis(500));

    // Verify the plugin created a session
    let contents = std::fs::read_to_string(&results_file)
        .unwrap_or_else(|e| panic!("results file not found: {}", e));
    assert!(
        contents.lines().any(|l| !l.is_empty() && l != "chat_sent"),
        "plugin should have created a session (got: {})",
        contents
    );

    // The key assertion: the global plugin's tool ("dispatch_test_tool")
    // should appear in the LLM context for the dispatched session.
    let captures = provider_handle.captures();
    assert!(
        !captures.is_empty(),
        "provider should have been called (dispatched session should have sent a chat to LLM)"
    );
    let tools = &captures[0].context.tools;
    assert!(
        tools.iter().any(|t| t.name == "dispatch_test_tool"),
        "dispatch_test_tool (from global plugin) should be in LLM tool list for dispatched session; \
         got tools: {:?}",
        tools.iter().map(|t| &t.name).collect::<Vec<_>>()
    );

    server.shutdown();
}

/// Test that a global plugin with background I/O can still handle tool calls.
///
/// This verifies the channel-mediated path: once background reader/writer
/// tasks own the async pipes, tool calls flow through the channels correctly.
/// The plugin provides an `echo_bg` tool and also sends a background
/// `ListSessions` ServerRequest after registration.
#[test]
fn global_plugin_background_io_with_tool_calls() {
    use tau_agent_lib::providers::mock::MockResponse;

    let tmp = tempfile::tempdir().unwrap();
    let bg_results_file = tmp.path().join("bg_tool_results.txt");
    let plugin_script = tmp.path().join("bg_tool_plugin.sh");

    // This plugin:
    //  - Registers with one tool ("echo_bg")
    //  - Sends a background ListSessions ServerRequest
    //  - In the main loop, handles tool_call and server_response messages
    let script = format!(
        r#"#!/bin/bash
set -eu
RESULTS="{results}"

# Registration with one tool
echo '{{"type":"register","name":"bg-tool-test","tools":[{{"name":"echo_bg","description":"Echo for bg test","parameters":{{"type":"object","properties":{{"msg":{{"type":"string"}}}},"required":["msg"]}}}}],"hooks":[],"commands":[]}}'

# Send a background ListSessions request immediately
echo '{{"type":"server_request","request_id":"bg-list-1","request":{{"type":"list_sessions","include_archived":false}}}}'

# Main loop: handle tool calls and server responses
while IFS= read -r line; do
    case "$line" in
        *'"type":"tool_call"'*)
            # Extract tool_call_id (simple grep - works for our test)
            TCID=$(echo "$line" | grep -o '"tool_call_id":"[^"]*"' | head -1 | cut -d'"' -f4)
            echo '{{"type":"tool_result","tool_call_id":"'"$TCID"'","content":[{{"type":"text","text":"BG_ECHO_OK"}}],"is_error":false}}'
            ;;
        *server_response*)
            echo "$line" >> "$RESULTS"
            ;;
        *idle*)
            exit 0
            ;;
    esac
done
"#,
        results = bg_results_file.display()
    );
    std::fs::write(&plugin_script, &script).unwrap();

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&plugin_script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let plugins_config = tau_agent_lib::plugin::PluginsConfig {
        no_default_worker: true,
        global: [(
            "bg-tool-test".to_string(),
            tau_agent_lib::plugin::PluginEntry {
                command: vec!["bash".into(), plugin_script.to_string_lossy().into()],
                env: Default::default(),
            },
        )]
        .into_iter()
        .collect(),
        ..Default::default()
    };

    // Mock LLM response: call the echo_bg tool, then produce final text.
    let mock_responses = vec![
        MockResponse::ToolCalls(vec![tau_agent_lib::types::ToolCall {
            id: "tc-bg-1".into(),
            name: "echo_bg".into(),
            arguments: serde_json::json!({"msg": "hello"}),
        }]),
        MockResponse::Text("done".into()),
    ];

    let server = TestServer::start_with_config(mock_responses, |mut config| {
        config.plugins_config = Some(plugins_config);
        config
    });

    // Wait for the background ServerRequest to be handled.
    for _ in 0..40 {
        if bg_results_file.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    // Verify background ListSessions response arrived.
    let bg_contents = std::fs::read_to_string(&bg_results_file)
        .unwrap_or_else(|e| panic!("bg results file missing: {}", e));
    assert!(
        bg_contents.contains("server_response"),
        "expected server_response in bg results: {}",
        bg_contents
    );

    // Now run a chat that triggers the echo_bg tool call through the
    // channel-mediated path.
    let sid = match CreateSessionBuilder::new(&server)
        .system_prompt("test")
        .cwd("/tmp")
        .send_raw()
    {
        Response::SessionCreated { session_id } => session_id,
        other => panic!("expected SessionCreated, got {:?}", other),
    };

    let conn2 = server.connect();
    let responses = send_recv_all(
        &conn2,
        &Request::Chat {
            session_id: sid.clone(),
            text: "test".into(),
        },
    );
    assert!(
        responses.iter().any(|r| matches!(r, Response::AgentDone)),
        "expected AgentDone in {:?}",
        responses
    );

    // Verify the tool result is in the messages.
    let conn3 = server.connect();
    let resp = send_recv(
        &conn3,
        &Request::GetMessages {
            session_id: sid.clone(),
        },
    );
    match resp {
        Response::Messages { messages } => {
            // Should have: User, Assistant(tool_call), ToolResult, Assistant(text)
            let tool_result = messages
                .iter()
                .find(|m| matches!(m, tau_agent_lib::types::Message::ToolResult(_)));
            assert!(
                tool_result.is_some(),
                "no tool result in messages: {:?}",
                messages
            );
            if let tau_agent_lib::types::Message::ToolResult(tr) = tool_result.unwrap() {
                let text: String = tr
                    .content
                    .iter()
                    .filter_map(|c| match c {
                        tau_agent_lib::types::ToolResultContent::Text(t) => Some(t.text.as_str()),
                        _ => None,
                    })
                    .collect();
                assert_eq!(text, "BG_ECHO_OK");
                assert!(!tr.is_error);
            }
        }
        other => panic!("expected Messages, got {:?}", other),
    }

    server.shutdown();
}

// ---------------------------------------------------------------------------
// ExecuteTool tests
// ---------------------------------------------------------------------------

#[test]
fn server_execute_tool_basic() {
    use std::sync::Arc;
    use tau_agent_lib::providers::mock::{MockToolExecutor, MockToolResponse, mock_tool};

    let mock_executor = MockToolExecutor::new();
    let tool_handle = mock_executor.handle();
    tool_handle.on_tool(
        "echo_tool",
        MockToolResponse::Success("hello from tool".into()),
    );

    let tool_factory: Arc<dyn Fn() -> Box<dyn tau_agent_lib::worker::ToolExecutor> + Send + Sync> =
        Arc::new(move || Box::new(tool_handle.clone().executor()));

    let server = TestServer::start_with_config(vec![], |mut config| {
        config.tool_executor_factory = Some(tool_factory);
        config.mock_tools = vec![mock_tool("echo_tool", "Echo tool")];
        config
    });

    let sid = match CreateSessionBuilder::new(&server)
        .system_prompt("test")
        .cwd("/tmp")
        .send_raw()
    {
        Response::SessionCreated { session_id } => session_id,
        other => panic!("expected SessionCreated, got {:?}", other),
    };

    let conn2 = server.connect();
    let resp = send_recv(
        &conn2,
        &Request::ExecuteTool {
            session_id: sid.clone(),
            tool_name: "echo_tool".into(),
            arguments: serde_json::json!({}),
        },
    );
    match resp {
        Response::ToolExecuted { content, is_error } => {
            assert_eq!(content, "hello from tool");
            assert!(!is_error);
        }
        other => panic!("expected ToolExecuted, got {:?}", other),
    }

    server.shutdown();
}

#[test]
fn server_execute_tool_error() {
    use std::sync::Arc;
    use tau_agent_lib::providers::mock::{MockToolExecutor, MockToolResponse, mock_tool};

    let mock_executor = MockToolExecutor::new();
    let tool_handle = mock_executor.handle();
    tool_handle.on_tool(
        "fail_tool",
        MockToolResponse::ToolError("something broke".into()),
    );

    let tool_factory: Arc<dyn Fn() -> Box<dyn tau_agent_lib::worker::ToolExecutor> + Send + Sync> =
        Arc::new(move || Box::new(tool_handle.clone().executor()));

    let server = TestServer::start_with_config(vec![], |mut config| {
        config.tool_executor_factory = Some(tool_factory);
        config.mock_tools = vec![mock_tool("fail_tool", "Failing tool")];
        config
    });

    let sid = match CreateSessionBuilder::new(&server)
        .system_prompt("test")
        .cwd("/tmp")
        .send_raw()
    {
        Response::SessionCreated { session_id } => session_id,
        other => panic!("expected SessionCreated, got {:?}", other),
    };

    let conn2 = server.connect();
    let resp = send_recv(
        &conn2,
        &Request::ExecuteTool {
            session_id: sid.clone(),
            tool_name: "fail_tool".into(),
            arguments: serde_json::json!({}),
        },
    );
    match resp {
        Response::ToolExecuted { content, is_error } => {
            assert_eq!(content, "something broke");
            assert!(is_error);
        }
        other => panic!("expected ToolExecuted, got {:?}", other),
    }

    server.shutdown();
}

#[test]
fn server_execute_tool_persistence() {
    use std::sync::Arc;
    use tau_agent_lib::providers::mock::{MockToolExecutor, MockToolResponse, mock_tool};

    let mock_executor = MockToolExecutor::new();
    let tool_handle = mock_executor.handle();
    tool_handle.on_tool(
        "my_tool",
        MockToolResponse::Success("persisted result".into()),
    );

    let tool_factory: Arc<dyn Fn() -> Box<dyn tau_agent_lib::worker::ToolExecutor> + Send + Sync> =
        Arc::new(move || Box::new(tool_handle.clone().executor()));

    let server = TestServer::start_with_config(vec![], |mut config| {
        config.tool_executor_factory = Some(tool_factory);
        config.mock_tools = vec![mock_tool("my_tool", "My tool")];
        config
    });

    let sid = match CreateSessionBuilder::new(&server)
        .system_prompt("test")
        .cwd("/tmp")
        .send_raw()
    {
        Response::SessionCreated { session_id } => session_id,
        other => panic!("expected SessionCreated, got {:?}", other),
    };

    // Execute tool
    let conn2 = server.connect();
    let _ = send_recv(
        &conn2,
        &Request::ExecuteTool {
            session_id: sid.clone(),
            tool_name: "my_tool".into(),
            arguments: serde_json::json!({"key": "value"}),
        },
    );

    // Verify messages persisted
    let conn3 = server.connect();
    let resp = send_recv(&conn3, &Request::GetMessages { session_id: sid });
    match resp {
        Response::Messages { messages } => {
            // Should have 2 messages: Assistant(ToolCall) + ToolResult
            assert_eq!(
                messages.len(),
                2,
                "expected 2 messages, got {}: {:?}",
                messages.len(),
                messages
            );
            // First: Assistant with ToolCall
            match &messages[0] {
                tau_agent_lib::types::Message::Assistant(a) => {
                    assert_eq!(a.stop_reason, tau_agent_lib::types::StopReason::ToolUse);
                    assert!(a.content.iter().any(|c| matches!(
                        c,
                        tau_agent_lib::types::AssistantContent::ToolCall(tc)
                            if tc.name == "my_tool"
                    )));
                }
                other => panic!("expected Assistant message, got {:?}", other),
            }
            // Second: ToolResult
            match &messages[1] {
                tau_agent_lib::types::Message::ToolResult(tr) => {
                    assert!(!tr.is_error);
                    assert_eq!(tr.tool_name, "my_tool");
                    let text: String = tr
                        .content
                        .iter()
                        .filter_map(|c| match c {
                            tau_agent_lib::types::ToolResultContent::Text(t) => {
                                Some(t.text.as_str())
                            }
                            _ => None,
                        })
                        .collect();
                    assert_eq!(text, "persisted result");
                }
                other => panic!("expected ToolResult message, got {:?}", other),
            }
        }
        other => panic!("expected Messages, got {:?}", other),
    }

    server.shutdown();
}

#[test]
fn server_execute_tool_nonexistent_session() {
    let server = TestServer::start(vec![]);
    let conn = server.connect();
    let resp = send_recv(
        &conn,
        &Request::ExecuteTool {
            session_id: "nonexistent".into(),
            tool_name: "bash".into(),
            arguments: serde_json::json!({}),
        },
    );
    match resp {
        Response::Error { message } => {
            assert!(message.contains("not found"), "got: {}", message);
        }
        other => panic!("expected Error, got {:?}", other),
    }
    server.shutdown();
}

// ---------------------------------------------------------------------------
// Log provider tests
// ---------------------------------------------------------------------------

fn create_log_session(sock_path: &std::path::Path) -> String {
    let conn = UnixStream::connect(sock_path).unwrap();
    conn.set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    match send_recv(
        &conn,
        &CreateSessionBuilder::standalone()
            .model("log")
            .provider("log")
            .system_prompt("test")
            .cwd("/tmp")
            .build(),
    ) {
        Response::SessionCreated { session_id } => session_id,
        other => panic!("expected SessionCreated, got {:?}", other),
    }
}

fn shutdown_log_server(sock_path: &std::path::Path) {
    let conn = UnixStream::connect(sock_path).unwrap();
    conn.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    send_recv(&conn, &Request::Shutdown { restart: false });
}

#[test]
fn server_log_provider_chat_returns_immediately() {
    use tau_agent_lib::providers::log::{LogProvider, log_model};

    let dir = tempfile::tempdir().unwrap();
    let sock_path = dir.path().join("tau-test.sock");
    let db_path = dir.path().join("test.db");
    let sock_clone = sock_path.clone();

    let model = log_model();
    let mut registry = tau_agent_lib::provider::ProviderRegistry::new();
    registry.register(LogProvider);

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

    for _ in 0..50 {
        if sock_path.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(sock_path.exists(), "server socket did not appear");

    let conn = UnixStream::connect(&sock_path).unwrap();
    conn.set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();

    let sid = match send_recv(
        &conn,
        &CreateSessionBuilder::standalone()
            .model("log")
            .provider("log")
            .system_prompt("test")
            .cwd("/tmp")
            .build(),
    ) {
        Response::SessionCreated { session_id } => session_id,
        other => panic!("expected SessionCreated, got {:?}", other),
    };

    // Chat — should return immediately with AgentDone (no LLM call)
    let conn2 = UnixStream::connect(&sock_path).unwrap();
    conn2
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let responses = send_recv_all(
        &conn2,
        &Request::Chat {
            session_id: sid.clone(),
            text: "hello".into(),
        },
    );

    assert!(
        responses.iter().any(|r| matches!(r, Response::AgentDone)),
        "expected AgentDone in responses: {:?}",
        responses
    );

    // Should NOT have any meaningful text deltas
    let has_text = responses.iter().any(|r| {
        if let Response::Stream { event } = r {
            if let tau_agent_lib::types::StreamEvent::TextDelta { delta, .. } = event.as_ref() {
                !delta.is_empty()
            } else {
                false
            }
        } else {
            false
        }
    });
    assert!(
        !has_text,
        "log provider should not produce text content: {:?}",
        responses
    );

    // Shutdown
    let conn3 = UnixStream::connect(&sock_path).unwrap();
    conn3
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    send_recv(&conn3, &Request::Shutdown { restart: false });
}

/// Task 582 — P2: `QueueMessage` (fire-and-forget) targeting a log-provider
/// session must record the message to history AND return `OkWithNote`
/// (the courtesy note explaining no agent loop ran). No `queued_messages`
/// row should survive (the message is handled synchronously, not queued),
/// and no agent-turn events (`AgentDone`, `AssistantChunk`, etc.) are
/// emitted.
#[test]
fn queue_message_to_log_session_records_without_agent_loop() {
    let server = TestServer::start_log_only();
    let sock_path = &server.sock_path;
    let sid = create_log_session(&sock_path);

    // Fire-and-forget QueueMessage to the log session.
    let conn = UnixStream::connect(&sock_path).unwrap();
    conn.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let resp = send_recv(
        &conn,
        &Request::QueueMessage {
            target_session_id: sid.clone(),
            content: "hello placeholder".into(),
            sender_info: "test".into(),
            await_reply: false,
            reply_to: None,
        },
    );
    match &resp {
        Response::OkWithNote { note } => {
            assert!(
                note.contains(&sid) && note.to_lowercase().contains("placeholder"),
                "expected note mentioning session id and placeholder semantics, got: {}",
                note
            );
        }
        other => panic!("expected OkWithNote, got {:?}", other),
    }

    // Verify the message landed in the session's history as a user message.
    let conn2 = UnixStream::connect(&sock_path).unwrap();
    conn2
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let msgs_resp = send_recv(
        &conn2,
        &Request::GetMessages {
            session_id: sid.clone(),
        },
    );
    let messages = match msgs_resp {
        Response::Messages { messages } => messages,
        other => panic!("expected Messages, got {:?}", other),
    };
    let user_found = messages.iter().any(|m| {
        matches!(
            m,
            tau_agent_lib::types::Message::User(u)
                if u.content.iter().any(|c| matches!(
                    c,
                    tau_agent_lib::types::UserContent::Text(t) if t.text.contains("hello placeholder")
                ))
        )
    });
    assert!(
        user_found,
        "user message 'hello placeholder' should be in session history, got: {:?}",
        messages
    );

    // And no AssistantMessage was appended — the agent loop did not run.
    let has_assistant = messages
        .iter()
        .any(|m| matches!(m, tau_agent_lib::types::Message::Assistant(_)));
    assert!(
        !has_assistant,
        "placeholder session must not produce an assistant message, got: {:?}",
        messages
    );

    shutdown_log_server(&sock_path);
}

/// Task 582 — P2: `Chat` on a log-provider session records the message,
/// emits an informational `Status` stream event with the courtesy note,
/// and terminates with `AgentDone` without running the agent loop.
#[test]
fn chat_to_log_session_emits_note_and_no_agent_loop() {
    let server = TestServer::start_log_only();
    let sock_path = &server.sock_path;
    let sid = create_log_session(&sock_path);

    let conn = UnixStream::connect(&sock_path).unwrap();
    conn.set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let responses = send_recv_all(
        &conn,
        &Request::Chat {
            session_id: sid.clone(),
            text: "hi there".into(),
        },
    );

    // AgentDone is the terminal.
    assert!(
        responses.iter().any(|r| matches!(r, Response::AgentDone)),
        "expected AgentDone, got {:?}",
        responses
    );
    // No Error response (the bug produced 'no API key for provider: log').
    let error_count = responses
        .iter()
        .filter(|r| matches!(r, Response::Error { .. }))
        .count();
    assert_eq!(
        error_count, 0,
        "placeholder Chat should not produce Error, got: {:?}",
        responses
    );
    // The Status note is present.
    let note_present = responses.iter().any(|r| {
        if let Response::Stream { event } = r {
            matches!(
                event.as_ref(),
                tau_agent_lib::types::StreamEvent::Status { message } if message.to_lowercase().contains("placeholder")
            )
        } else {
            false
        }
    });
    assert!(
        note_present,
        "expected Status event with placeholder note, got: {:?}",
        responses
    );
    // No text deltas — the log provider's stream() never ran.
    let has_text = responses.iter().any(|r| {
        if let Response::Stream { event } = r {
            matches!(
                event.as_ref(),
                tau_agent_lib::types::StreamEvent::TextDelta { delta, .. } if !delta.is_empty()
            )
        } else {
            false
        }
    });
    assert!(
        !has_text,
        "placeholder Chat must not emit text deltas, got: {:?}",
        responses
    );

    // The message is persisted.
    let conn2 = UnixStream::connect(&sock_path).unwrap();
    conn2
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let msgs_resp = send_recv(
        &conn2,
        &Request::GetMessages {
            session_id: sid.clone(),
        },
    );
    let messages = match msgs_resp {
        Response::Messages { messages } => messages,
        other => panic!("expected Messages, got {:?}", other),
    };
    let user_found = messages.iter().any(|m| {
        matches!(
            m,
            tau_agent_lib::types::Message::User(u)
                if u.content.iter().any(|c| matches!(
                    c,
                    tau_agent_lib::types::UserContent::Text(t) if t.text.contains("hi there")
                ))
        )
    });
    assert!(
        user_found,
        "user message 'hi there' should be in session history, got: {:?}",
        messages
    );

    shutdown_log_server(&sock_path);
}

/// Task 582 — P2: `QueueMessage` with `await_reply=true` against a log
/// session returns `MessageReply` (with the courtesy note as content)
/// immediately, instead of blocking and timing out.
#[test]
fn queue_message_await_reply_to_log_session_returns_note() {
    let server = TestServer::start_log_only();
    let sock_path = &server.sock_path;
    let sid = create_log_session(&sock_path);

    let conn = UnixStream::connect(&sock_path).unwrap();
    conn.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let resp = send_recv(
        &conn,
        &Request::QueueMessage {
            target_session_id: sid.clone(),
            content: "need a reply".into(),
            sender_info: "test".into(),
            await_reply: true,
            reply_to: None,
        },
    );
    match &resp {
        Response::MessageReply { content } => {
            assert!(
                content.to_lowercase().contains("placeholder"),
                "expected placeholder note, got: {}",
                content
            );
        }
        other => panic!("expected MessageReply with note, got {:?}", other),
    }

    shutdown_log_server(&sock_path);
}

// ---------------------------------------------------------------------------
// Model alias resolution (task 419)
// ---------------------------------------------------------------------------

/// Build a small set of mock models for alias tests: "fast" and "smart"
/// share the mock provider so a session can pick either via id.
fn alias_test_models() -> Vec<tau_agent_lib::Model> {
    let mut a = mock_model();
    a.id = "fast-model".into();
    a.name = "Fast".into();
    let mut b = mock_model();
    b.id = "smart-model".into();
    b.name = "Smart".into();
    vec![a, b]
}

fn alias_test_server(
    aliases: std::collections::HashMap<String, String>,
) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let sock_path = dir.path().join("alias-test.sock");
    let db_path = dir.path().join("alias-test.db");

    let mut registry = tau_agent_lib::provider::ProviderRegistry::new();
    registry.register(MockProvider::new(vec![]));

    let config = tau_agent_lib::server::TestServerConfig {
        registry,
        models: alias_test_models(),
        socket_path: sock_path.clone(),
        db_path,
        tool_executor_factory: None,
        mock_tools: vec![],
        plugins_config: None,
        aliases,
    };

    std::thread::spawn(move || {
        smol::block_on(async {
            tau_agent_lib::server::run_with_config(config).await.ok();
        });
    });

    for _ in 0..50 {
        if sock_path.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        sock_path.exists(),
        "alias test server socket did not appear"
    );

    (dir, sock_path)
}

fn create_session_with_model(
    sock_path: &std::path::Path,
    model: Option<&str>,
    cwd: Option<&str>,
) -> Response {
    let conn = UnixStream::connect(sock_path).unwrap();
    conn.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    send_recv(
        &conn,
        &CreateSessionBuilder::standalone()
            .model_opt(model.map(String::from))
            .system_prompt("test")
            .cwd_opt(cwd.map(String::from))
            .build(),
    )
}

fn session_model_id(sock_path: &std::path::Path, session_id: &str) -> String {
    let conn = UnixStream::connect(sock_path).unwrap();
    conn.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    match send_recv(
        &conn,
        &Request::GetSessionInfo {
            session_id: session_id.into(),
        },
    ) {
        Response::SessionInfo { info } => info.model,
        other => panic!("expected SessionInfo, got {:?}", other),
    }
}

fn shutdown(sock_path: &std::path::Path) {
    let conn = UnixStream::connect(sock_path).unwrap();
    conn.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    send_recv(&conn, &Request::Shutdown { restart: false });
}

#[test]
fn alias_global_resolves_to_target_model() {
    let mut aliases = std::collections::HashMap::new();
    aliases.insert("smart".into(), "smart-model".into());
    aliases.insert("fast".into(), "fast-model".into());
    let (_dir, sock) = alias_test_server(aliases);

    let resp = create_session_with_model(&sock, Some("smart"), None);
    let sid = match resp {
        Response::SessionCreated { session_id } => session_id,
        other => panic!("expected SessionCreated, got {:?}", other),
    };
    let model = session_model_id(&sock, &sid);
    assert_eq!(model, "smart-model", "alias 'smart' should resolve");

    let resp = create_session_with_model(&sock, Some("fast"), None);
    let sid = match resp {
        Response::SessionCreated { session_id } => session_id,
        other => panic!("expected SessionCreated, got {:?}", other),
    };
    let model = session_model_id(&sock, &sid);
    assert_eq!(model, "fast-model");

    shutdown(&sock);
}

#[test]
fn alias_unknown_target_returns_error() {
    let mut aliases = std::collections::HashMap::new();
    aliases.insert("ghost".into(), "no-such-model".into());
    let (_dir, sock) = alias_test_server(aliases);

    let resp = create_session_with_model(&sock, Some("ghost"), None);
    match resp {
        Response::Error { message } => {
            assert!(
                message.contains("ghost"),
                "error should mention alias name: {}",
                message
            );
            assert!(
                message.contains("no-such-model"),
                "error should mention target: {}",
                message
            );
        }
        other => panic!("expected Error response, got {:?}", other),
    }

    shutdown(&sock);
}

#[test]
fn alias_empty_maps_match_legacy_behavior() {
    // Regression: with no aliases configured, behavior must be identical
    // to before the alias layer existed:
    //   - explicit known model id → that model
    //   - explicit unknown model id → fall through to default
    //   - no model id → default
    let (_dir, sock) = alias_test_server(std::collections::HashMap::new());

    // Known id.
    let resp = create_session_with_model(&sock, Some("smart-model"), None);
    let sid = match resp {
        Response::SessionCreated { session_id } => session_id,
        other => panic!("{:?}", other),
    };
    assert_eq!(session_model_id(&sock, &sid), "smart-model");

    // Unknown id → default (the FIRST model in the list, "fast-model").
    let resp = create_session_with_model(&sock, Some("not-a-model"), None);
    let sid = match resp {
        Response::SessionCreated { session_id } => session_id,
        other => panic!("expected SessionCreated, got {:?}", other),
    };
    assert_eq!(
        session_model_id(&sock, &sid),
        "fast-model",
        "unknown id should fall through to default model"
    );

    // No id → default.
    let resp = create_session_with_model(&sock, None, None);
    let sid = match resp {
        Response::SessionCreated { session_id } => session_id,
        other => panic!("{:?}", other),
    };
    assert_eq!(session_model_id(&sock, &sid), "fast-model");

    shutdown(&sock);
}

#[test]
fn alias_project_overrides_global() {
    // Set up a project dir with .tau/models.toml that points "smart" at
    // "fast-model" while the global alias points "smart" at "smart-model".
    let proj_dir = tempfile::tempdir().unwrap();
    let tau_dir = proj_dir.path().join(".tau");
    std::fs::create_dir_all(&tau_dir).unwrap();
    std::fs::write(
        tau_dir.join("models.toml"),
        r#"[aliases]
smart = "fast-model"
"#,
    )
    .unwrap();

    let mut global = std::collections::HashMap::new();
    global.insert("smart".into(), "smart-model".into());
    let (_dir, sock) = alias_test_server(global);

    // Without cwd: global alias wins → smart-model.
    let resp = create_session_with_model(&sock, Some("smart"), None);
    let sid = match resp {
        Response::SessionCreated { session_id } => session_id,
        other => panic!("{:?}", other),
    };
    assert_eq!(session_model_id(&sock, &sid), "smart-model");

    // With cwd inside the project dir: project alias wins → fast-model.
    let cwd = proj_dir.path().to_str().unwrap();
    let resp = create_session_with_model(&sock, Some("smart"), Some(cwd));
    let sid = match resp {
        Response::SessionCreated { session_id } => session_id,
        other => panic!("{:?}", other),
    };
    assert_eq!(
        session_model_id(&sock, &sid),
        "fast-model",
        "project alias should override global"
    );

    shutdown(&sock);
}

#[test]
fn alias_set_model_routes_through_resolver() {
    let mut aliases = std::collections::HashMap::new();
    aliases.insert("smart".into(), "smart-model".into());
    let (_dir, sock) = alias_test_server(aliases);

    // Create a session on the default model (fast-model).
    let sid = match create_session_with_model(&sock, None, None) {
        Response::SessionCreated { session_id } => session_id,
        other => panic!("{:?}", other),
    };
    assert_eq!(session_model_id(&sock, &sid), "fast-model");

    // /model smart should switch via the alias.
    let conn = UnixStream::connect(&sock).unwrap();
    conn.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let resp = send_recv(
        &conn,
        &Request::SetModel {
            session_id: sid.clone(),
            model_id: "smart".into(),
            caller_session_id: None,
        },
    );
    match resp {
        Response::ModelChanged { model } => assert_eq!(model.id, "smart-model"),
        other => panic!("expected ModelChanged, got {:?}", other),
    }
    assert_eq!(session_model_id(&sock, &sid), "smart-model");

    shutdown(&sock);
}

#[test]
fn alias_list_aliases_request() {
    let mut aliases = std::collections::HashMap::new();
    aliases.insert("smart".into(), "smart-model".into());
    aliases.insert("fast".into(), "fast-model".into());
    let (_dir, sock) = alias_test_server(aliases);

    // No cwd → global only.
    let conn = UnixStream::connect(&sock).unwrap();
    conn.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let resp = send_recv(&conn, &Request::ListAliases { cwd: None });
    match resp {
        Response::Aliases { global, project } => {
            assert!(project.is_empty());
            assert_eq!(global.len(), 2);
            let names: Vec<&str> = global.iter().map(|a| a.name.as_str()).collect();
            assert!(names.contains(&"smart"));
            assert!(names.contains(&"fast"));
        }
        other => panic!("expected Aliases, got {:?}", other),
    }

    // With cwd containing .tau/models.toml.
    let proj_dir = tempfile::tempdir().unwrap();
    let tau_dir = proj_dir.path().join(".tau");
    std::fs::create_dir_all(&tau_dir).unwrap();
    std::fs::write(
        tau_dir.join("models.toml"),
        r#"[aliases]
worker = "fast-model"
"#,
    )
    .unwrap();
    let cwd = proj_dir.path().to_str().unwrap().to_string();

    let conn = UnixStream::connect(&sock).unwrap();
    conn.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let resp = send_recv(&conn, &Request::ListAliases { cwd: Some(cwd) });
    match resp {
        Response::Aliases { global, project } => {
            assert_eq!(global.len(), 2);
            assert_eq!(project.len(), 1);
            assert_eq!(project[0].name, "worker");
            assert_eq!(project[0].target, "fast-model");
        }
        other => panic!("expected Aliases, got {:?}", other),
    }

    shutdown(&sock);
}

/// `SessionInfo.is_live` must be false for a freshly created session (no turn),
/// and false after a completed chat turn.  During a turn it should be true,
/// but we verify that indirectly via the post-completion false assertion.
#[test]
fn session_info_is_live_false_when_idle() {
    let server = TestServer::start(vec![MockResponse::Text("reply".into())]);

    // Create session
    let sid = match CreateSessionBuilder::new(&server).cwd("/tmp").send_raw() {
        Response::SessionCreated { session_id } => session_id,
        other => panic!("{:?}", other),
    };

    // Before any chat: is_live should be false
    let conn2 = server.connect();
    let resp = send_recv(
        &conn2,
        &Request::GetSessionInfo {
            session_id: sid.clone(),
        },
    );
    match resp {
        Response::SessionInfo { info } => {
            assert!(!info.is_live, "new session should not be live");
            assert_eq!(info.state, "idle");
        }
        other => panic!("{:?}", other),
    }

    // Run a chat to completion
    let conn3 = server.connect();
    let responses = send_recv_all(
        &conn3,
        &Request::Chat {
            session_id: sid.clone(),
            text: "hello".into(),
        },
    );
    assert!(responses.iter().any(|r| matches!(r, Response::AgentDone)));

    // After chat: is_live should be false again
    let conn4 = server.connect();
    let resp2 = send_recv(
        &conn4,
        &Request::GetSessionInfo {
            session_id: sid.clone(),
        },
    );
    match resp2 {
        Response::SessionInfo { info } => {
            assert!(
                !info.is_live,
                "session should not be live after turn completes"
            );
            assert_eq!(info.state, "idle");
            assert_eq!(info.last_exit_status.as_deref(), Some("completed"));
        }
        other => panic!("{:?}", other),
    }

    server.shutdown();
}

/// After a restart with a poisoned last_phase in the DB, is_live must be false
/// (no chat loop running) even though the DB says "sending request".
#[test]
fn session_info_is_live_false_after_restart_with_stale_phase() {
    let dir = tempfile::tempdir().unwrap();
    let sock_path = dir.path().join("test.sock");
    let db_path = dir.path().join("test.db");

    // -- Server 1: create a session, chat, shutdown --
    let model = mock_model();
    let mut registry = tau_agent_lib::provider::ProviderRegistry::new();
    registry.register(MockProvider::new(vec![MockResponse::Text("r1".into())]));

    let config = tau_agent_lib::server::TestServerConfig {
        registry,
        models: vec![model],
        socket_path: sock_path.clone(),
        db_path: db_path.clone(),
        tool_executor_factory: None,
        mock_tools: vec![],
        plugins_config: None,
        aliases: std::collections::HashMap::new(),
    };

    let handle = std::thread::spawn(move || {
        smol::block_on(async {
            tau_agent_lib::server::run_with_config(config).await.ok();
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
        &CreateSessionBuilder::standalone().cwd("/tmp").build(),
    ) {
        Response::SessionCreated { session_id } => session_id,
        other => panic!("{:?}", other),
    };

    // Chat to completion
    let conn2 = UnixStream::connect(&sock_path).unwrap();
    conn2
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let resps = send_recv_all(
        &conn2,
        &Request::Chat {
            session_id: sid.clone(),
            text: "hello".into(),
        },
    );
    assert!(resps.iter().any(|r| matches!(r, Response::AgentDone)));

    // Shutdown server 1
    let conn3 = UnixStream::connect(&sock_path).unwrap();
    conn3
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    send_recv(&conn3, &Request::Shutdown { restart: false });
    handle.join().ok();

    // Poison the DB: set last_phase to non-idle
    {
        let db = tau_agent_lib::db::Db::open(&db_path).unwrap();
        db.update_phase(&sid, "sending request").unwrap();
    }

    // -- Server 2: restart with same DB --
    std::fs::remove_file(&sock_path).ok();
    let model2 = mock_model();
    let mut registry2 = tau_agent_lib::provider::ProviderRegistry::new();
    registry2.register(MockProvider::new(vec![]));

    let config2 = tau_agent_lib::server::TestServerConfig {
        registry: registry2,
        models: vec![model2],
        socket_path: sock_path.clone(),
        db_path: db_path.clone(),
        tool_executor_factory: None,
        mock_tools: vec![],
        plugins_config: None,
        aliases: std::collections::HashMap::new(),
    };

    let _handle2 = std::thread::spawn(move || {
        smol::block_on(async {
            tau_agent_lib::server::run_with_config(config2).await.ok();
        });
    });

    for _ in 0..50 {
        if sock_path.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    // GetSessionInfo: is_live must be false despite stale DB phase
    let conn4 = UnixStream::connect(&sock_path).unwrap();
    conn4
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let resp = send_recv(
        &conn4,
        &Request::GetSessionInfo {
            session_id: sid.clone(),
        },
    );
    match resp {
        Response::SessionInfo { info } => {
            assert!(
                !info.is_live,
                "is_live must be false after restart with stale phase"
            );
            assert_eq!(
                info.state, "idle",
                "state must be idle (phases map is empty after restart)"
            );
        }
        other => panic!("{:?}", other),
    }

    // Shutdown
    if let Ok(c) = UnixStream::connect(&sock_path) {
        let req = serde_json::to_string(&Request::Shutdown { restart: false }).unwrap();
        let mut c = c;
        let _ = c.write_all(format!("{}\n", req).as_bytes());
        let _ = c.flush();
    }
}

/// Clean shutdown writes last_phase="idle" for all sessions.
#[test]
fn clean_shutdown_resets_phases_to_idle() {
    let dir = tempfile::tempdir().unwrap();
    let sock_path = dir.path().join("test.sock");
    let db_path = dir.path().join("test.db");

    let model = mock_model();
    let mut registry = tau_agent_lib::provider::ProviderRegistry::new();
    registry.register(MockProvider::new(vec![MockResponse::Text("r".into())]));

    let config = tau_agent_lib::server::TestServerConfig {
        registry,
        models: vec![model],
        socket_path: sock_path.clone(),
        db_path: db_path.clone(),
        tool_executor_factory: None,
        mock_tools: vec![],
        plugins_config: None,
        aliases: std::collections::HashMap::new(),
    };

    let handle = std::thread::spawn(move || {
        smol::block_on(async {
            tau_agent_lib::server::run_with_config(config).await.ok();
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
        &CreateSessionBuilder::standalone().cwd("/tmp").build(),
    ) {
        Response::SessionCreated { session_id } => session_id,
        other => panic!("{:?}", other),
    };

    // Chat to completion — this sets last_phase to something, then idle
    let conn2 = UnixStream::connect(&sock_path).unwrap();
    conn2
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let resps = send_recv_all(
        &conn2,
        &Request::Chat {
            session_id: sid.clone(),
            text: "hello".into(),
        },
    );
    assert!(resps.iter().any(|r| matches!(r, Response::AgentDone)));

    // Manually poison the phase in DB to simulate mid-turn state
    {
        let db = tau_agent_lib::db::Db::open(&db_path).unwrap();
        db.update_phase(&sid, "thinking").unwrap();
        // Verify it was written
        let s = db.get_session(&sid).unwrap().unwrap();
        assert_eq!(s.last_phase.as_deref(), Some("thinking"));
    }

    // Clean shutdown
    let conn3 = UnixStream::connect(&sock_path).unwrap();
    conn3
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    send_recv(&conn3, &Request::Shutdown { restart: false });
    handle.join().ok();

    // Verify DB was cleaned up
    {
        let db = tau_agent_lib::db::Db::open(&db_path).unwrap();
        let s = db.get_session(&sid).unwrap().unwrap();
        assert_eq!(
            s.last_phase.as_deref(),
            Some("idle"),
            "clean shutdown must write last_phase='idle' for all sessions"
        );
    }
}

/// CancelChat on an idle session (no active chat loop) must immediately
/// emit Cancelled + Phase(Idle) to subscribers so the TUI never gets stuck.
#[test]
fn cancel_chat_without_active_loop_emits_cancelled() {
    use std::io::{BufRead, BufReader};
    use tau_agent_lib::types::StreamEvent;

    let server = TestServer::start(vec![]);

    // Create a session (no chat — session is idle)
    let sid = match CreateSessionBuilder::new(&server).cwd("/tmp").send_raw() {
        Response::SessionCreated { session_id } => session_id,
        other => panic!("expected SessionCreated, got {:?}", other),
    };

    // Subscribe on a separate connection
    let sub_conn = server.connect();
    sub_conn
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    {
        let mut sub_w = sub_conn.try_clone().unwrap();
        let req = serde_json::to_string(&Request::Subscribe {
            session_id: sid.clone(),
        })
        .unwrap();
        std::io::Write::write_all(&mut sub_w, format!("{}\n", req).as_bytes()).unwrap();
        std::io::Write::flush(&mut sub_w).unwrap();
    }
    let mut sub_reader = BufReader::new(sub_conn.try_clone().unwrap());

    // Read the initial phase event that Subscribe always sends
    let mut line = String::new();
    sub_reader.read_line(&mut line).unwrap();
    let initial: Response = serde_json::from_str(line.trim()).unwrap();
    match &initial {
        Response::Stream { event } => match event.as_ref() {
            StreamEvent::Phase { phase, .. } => {
                assert_eq!(*phase, tau_agent_lib::types::AgentPhase::Idle);
            }
            other => panic!("expected Phase event, got {:?}", other),
        },
        other => panic!("expected Stream, got {:?}", other),
    }

    // Send CancelChat
    let cancel_conn = server.connect();
    let resp = send_recv(
        &cancel_conn,
        &Request::CancelChat {
            session_id: sid.clone(),
            caller_session_id: None,
        },
    );
    assert!(matches!(resp, Response::Ok), "expected Ok, got {:?}", resp);

    // Subscriber should receive Cancelled
    let mut line2 = String::new();
    sub_reader.read_line(&mut line2).unwrap();
    let r2: Response = serde_json::from_str(line2.trim()).unwrap();
    assert!(
        matches!(r2, Response::Cancelled),
        "expected Cancelled, got {:?}",
        r2
    );

    // Subscriber should receive Phase(Idle)
    let mut line3 = String::new();
    sub_reader.read_line(&mut line3).unwrap();
    let r3: Response = serde_json::from_str(line3.trim()).unwrap();
    match &r3 {
        Response::Stream { event } => match event.as_ref() {
            StreamEvent::Phase { phase, .. } => {
                assert_eq!(
                    *phase,
                    tau_agent_lib::types::AgentPhase::Idle,
                    "expected Phase(Idle)"
                );
            }
            other => panic!("expected Phase event, got {:?}", other),
        },
        other => panic!("expected Stream(Phase), got {:?}", other),
    }

    server.shutdown();
}

/// After a server restart with a stale (non-idle) phase in the DB,
/// subscribers must see Idle and new chats must complete normally.
#[test]
fn server_restart_clears_stale_phases() {
    let dir = tempfile::tempdir().unwrap();
    let sock_path = dir.path().join("test.sock");
    let db_path = dir.path().join("test.db");

    // -- Server 1: create a session, chat, shutdown --
    let model = mock_model();
    let mut registry = tau_agent_lib::provider::ProviderRegistry::new();
    registry.register(MockProvider::new(vec![MockResponse::Text("r1".into())]));

    let config = tau_agent_lib::server::TestServerConfig {
        registry,
        models: vec![model],
        socket_path: sock_path.clone(),
        db_path: db_path.clone(),
        tool_executor_factory: None,
        mock_tools: vec![],
        plugins_config: None,
        aliases: std::collections::HashMap::new(),
    };

    let handle = std::thread::spawn(move || {
        smol::block_on(async {
            tau_agent_lib::server::run_with_config(config).await.ok();
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
        &CreateSessionBuilder::standalone().cwd("/tmp").build(),
    ) {
        Response::SessionCreated { session_id } => session_id,
        other => panic!("{:?}", other),
    };

    // Chat to completion
    let conn2 = UnixStream::connect(&sock_path).unwrap();
    conn2
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let resps = send_recv_all(
        &conn2,
        &Request::Chat {
            session_id: sid.clone(),
            text: "hello".into(),
        },
    );
    assert!(resps.iter().any(|r| matches!(r, Response::AgentDone)));

    // Shutdown server 1
    let conn3 = UnixStream::connect(&sock_path).unwrap();
    conn3
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    send_recv(&conn3, &Request::Shutdown { restart: false });
    handle.join().ok();

    // Poison the DB: set last_phase to non-idle
    {
        let db = tau_agent_lib::db::Db::open(&db_path).unwrap();
        db.update_phase(&sid, "sending request").unwrap();
    }

    // -- Server 2: restart with same DB --
    std::fs::remove_file(&sock_path).ok();
    let model2 = mock_model();
    let mut registry2 = tau_agent_lib::provider::ProviderRegistry::new();
    registry2.register(MockProvider::new(vec![MockResponse::Text("r2".into())]));

    let config2 = tau_agent_lib::server::TestServerConfig {
        registry: registry2,
        models: vec![model2],
        socket_path: sock_path.clone(),
        db_path: db_path.clone(),
        tool_executor_factory: None,
        mock_tools: vec![],
        plugins_config: None,
        aliases: std::collections::HashMap::new(),
    };

    let handle2 = std::thread::spawn(move || {
        smol::block_on(async {
            tau_agent_lib::server::run_with_config(config2).await.ok();
        });
    });

    for _ in 0..50 {
        if sock_path.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    // Subscribe — initial phase must be Idle (not the stale "sending request")
    {
        use std::io::{BufRead, BufReader};
        use tau_agent_lib::types::StreamEvent;

        let sub_conn = UnixStream::connect(&sock_path).unwrap();
        sub_conn
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        {
            let mut sub_w = sub_conn.try_clone().unwrap();
            let req = serde_json::to_string(&Request::Subscribe {
                session_id: sid.clone(),
            })
            .unwrap();
            std::io::Write::write_all(&mut sub_w, format!("{}\n", req).as_bytes()).unwrap();
            std::io::Write::flush(&mut sub_w).unwrap();
        }
        let mut sub_reader = BufReader::new(sub_conn.try_clone().unwrap());
        let mut line = String::new();
        sub_reader.read_line(&mut line).unwrap();
        let initial: Response = serde_json::from_str(line.trim()).unwrap();
        match &initial {
            Response::Stream { event } => match event.as_ref() {
                StreamEvent::Phase { phase, .. } => {
                    assert_eq!(
                        *phase,
                        tau_agent_lib::types::AgentPhase::Idle,
                        "initial phase after restart must be Idle, not stale"
                    );
                }
                other => panic!("expected Phase event, got {:?}", other),
            },
            other => panic!("expected Stream, got {:?}", other),
        }
    }

    // Send a new Chat — must complete with AgentDone
    let conn4 = UnixStream::connect(&sock_path).unwrap();
    conn4
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let resps2 = send_recv_all(
        &conn4,
        &Request::Chat {
            session_id: sid.clone(),
            text: "hello again".into(),
        },
    );
    assert!(
        resps2.iter().any(|r| matches!(r, Response::AgentDone)),
        "new chat must complete with AgentDone after restart"
    );

    // Shutdown server 2
    if let Ok(c) = UnixStream::connect(&sock_path) {
        let req = serde_json::to_string(&Request::Shutdown { restart: false }).unwrap();
        let mut c = c;
        let _ = c.write_all(format!("{}\n", req).as_bytes());
        let _ = c.flush();
    }
    handle2.join().ok();
}

// ---------------------------------------------------------------------------
// GetSessionAncestors
// ---------------------------------------------------------------------------

/// Helper: request ancestors for a session, returning the Vec.
fn get_ancestors(
    server: &TestServer,
    session_id: &str,
) -> Vec<tau_agent_lib::protocol::SessionInfo> {
    let conn = server.connect();
    match send_recv(
        &conn,
        &Request::GetSessionAncestors {
            session_id: session_id.to_string(),
        },
    ) {
        Response::SessionAncestors { sessions } => sessions,
        other => panic!("expected SessionAncestors, got {:?}", other),
    }
}

#[test]
fn get_session_ancestors_root() {
    let server = TestServer::start(vec![]);
    let root = CreateSessionBuilder::new(&server).send();

    let ancestors = get_ancestors(&server, &root);
    assert_eq!(
        ancestors.len(),
        1,
        "root alone is still a chain of length 1"
    );
    assert_eq!(ancestors[0].id, root);
    assert!(ancestors[0].parent_id.is_none());

    server.shutdown();
}

#[test]
fn get_session_ancestors_chain() {
    let server = TestServer::start(vec![]);
    // Build a→b→c (a is root; c is leaf).  Each parent needs budget=1 for
    // its single child.
    let a = CreateSessionBuilder::new(&server).child_budget(1).send();
    let b = CreateSessionBuilder::new(&server)
        .parent(a.clone())
        .child_budget(1)
        .send();
    let c = CreateSessionBuilder::new(&server).parent(b.clone()).send();

    let ancestors = get_ancestors(&server, &c);
    let ids: Vec<&str> = ancestors.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(ids, vec![c.as_str(), b.as_str(), a.as_str()]);
    // Verify parent_id threading is intact.
    assert_eq!(ancestors[0].parent_id.as_deref(), Some(b.as_str()));
    assert_eq!(ancestors[1].parent_id.as_deref(), Some(a.as_str()));
    assert!(ancestors[2].parent_id.is_none());

    server.shutdown();
}

#[test]
fn get_session_ancestors_unknown() {
    let server = TestServer::start(vec![]);

    let ancestors = get_ancestors(&server, "does-not-exist");
    assert!(
        ancestors.is_empty(),
        "unknown id must return empty, not error"
    );

    server.shutdown();
}

/// Insert a synthetic `StoredSession` row directly.
fn insert_stored(db: &tau_agent_lib::db::Db, id: &str, parent_id: Option<&str>, archived: bool) {
    db.create_session(&tau_agent_lib::db::StoredSession {
        id: id.into(),
        model: mock_model(),
        system_prompt: None,
        cwd: None,
        is_subscription: false,
        created_at: 1000,
        parent_id: parent_id.map(|s| s.to_string()),
        child_budget: 1,
        tagline: None,
        archived,
        last_exit_status: None,
        last_phase: None,
        auto_archive: false,
        notify_parent: true,
        project_name: None,
    })
    .expect("create_session");
}

#[test]
fn get_session_ancestors_depth_guard() {
    // Build a 70-deep chain directly in the DB before the server starts,
    // to avoid spinning up 70 real sessions through the API.
    let dir = tempfile::tempdir().unwrap();
    let sock_path = dir.path().join("tau-test.sock");
    let db_path = dir.path().join("test.db");

    {
        let db = tau_agent_lib::db::Db::open(&db_path).unwrap();
        // Root first (no parent), then 1..=69 each parented to the prior.
        insert_stored(&db, "s0", None, false);
        for i in 1..70 {
            let parent = format!("s{}", i - 1);
            let id = format!("s{}", i);
            insert_stored(&db, &id, Some(&parent), false);
        }
    }

    let model = mock_model();
    let mut registry = tau_agent_lib::provider::ProviderRegistry::new();
    registry.register(MockProvider::new(vec![]));

    let config = tau_agent_lib::server::TestServerConfig {
        registry,
        models: vec![model],
        socket_path: sock_path.clone(),
        db_path,
        tool_executor_factory: None,
        mock_tools: vec![],
        plugins_config: None,
        aliases: std::collections::HashMap::new(),
    };

    let handle = std::thread::spawn(move || {
        smol::block_on(async {
            tau_agent_lib::server::run_with_config(config).await.ok();
        });
    });
    for _ in 0..50 {
        if sock_path.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(sock_path.exists());

    // Request from the leaf (s69).  The guard caps at 64.
    let start = std::time::Instant::now();
    let conn = UnixStream::connect(&sock_path).unwrap();
    conn.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let resp = send_recv(
        &conn,
        &Request::GetSessionAncestors {
            session_id: "s69".into(),
        },
    );
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(5),
        "call took too long: {:?}",
        elapsed
    );

    let ancestors = match resp {
        Response::SessionAncestors { sessions } => sessions,
        other => panic!("expected SessionAncestors, got {:?}", other),
    };
    assert_eq!(ancestors.len(), 64, "depth guard should cap at 64");
    assert_eq!(ancestors[0].id, "s69", "leaf-first ordering");
    // Last entry is s69 - 63 = s6, and still has a parent (so caller can
    // detect truncation by combining len==64 with parent_id.is_some()).
    assert_eq!(ancestors[63].id, "s6");
    assert!(
        ancestors[63].parent_id.is_some(),
        "truncation point still has an unresolved parent_id"
    );

    if let Ok(c) = UnixStream::connect(&sock_path) {
        let req = serde_json::to_string(&Request::Shutdown { restart: false }).unwrap();
        let mut c = c;
        let _ = c.write_all(format!("{}\n", req).as_bytes());
        let _ = c.flush();
    }
    handle.join().ok();
}

#[test]
fn get_session_ancestors_includes_archived() {
    // Chain a→b→c where b is archived.  We need archived=true on a
    // single mid-chain row, which the server's `ArchiveSession` would
    // cascade over subtree — so write it directly via the DB.
    let dir = tempfile::tempdir().unwrap();
    let sock_path = dir.path().join("tau-test.sock");
    let db_path = dir.path().join("test.db");

    {
        let db = tau_agent_lib::db::Db::open(&db_path).unwrap();
        insert_stored(&db, "a", None, false);
        insert_stored(&db, "b", Some("a"), /* archived */ true);
        insert_stored(&db, "c", Some("b"), false);
    }

    let model = mock_model();
    let mut registry = tau_agent_lib::provider::ProviderRegistry::new();
    registry.register(MockProvider::new(vec![]));
    let config = tau_agent_lib::server::TestServerConfig {
        registry,
        models: vec![model],
        socket_path: sock_path.clone(),
        db_path,
        tool_executor_factory: None,
        mock_tools: vec![],
        plugins_config: None,
        aliases: std::collections::HashMap::new(),
    };
    let handle = std::thread::spawn(move || {
        smol::block_on(async {
            tau_agent_lib::server::run_with_config(config).await.ok();
        });
    });
    for _ in 0..50 {
        if sock_path.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(sock_path.exists());

    let conn = UnixStream::connect(&sock_path).unwrap();
    conn.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let resp = send_recv(
        &conn,
        &Request::GetSessionAncestors {
            session_id: "c".into(),
        },
    );
    let ancestors = match resp {
        Response::SessionAncestors { sessions } => sessions,
        other => panic!("expected SessionAncestors, got {:?}", other),
    };
    let ids: Vec<&str> = ancestors.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(ids, vec!["c", "b", "a"]);
    assert!(!ancestors[0].archived, "c is not archived");
    assert!(
        ancestors[1].archived,
        "b must still appear, flagged archived"
    );
    assert!(!ancestors[2].archived, "a is not archived");

    if let Ok(c) = UnixStream::connect(&sock_path) {
        let req = serde_json::to_string(&Request::Shutdown { restart: false }).unwrap();
        let mut c = c;
        let _ = c.write_all(format!("{}\n", req).as_bytes());
        let _ = c.flush();
    }
    handle.join().ok();
}

#[test]
fn get_session_ancestors_missing_mid_parent() {
    // Chain a→b→c; then delete b.  Walking from c should return [c] only,
    // because c.parent_id="b" is a stale FK after b is gone.
    let dir = tempfile::tempdir().unwrap();
    let sock_path = dir.path().join("tau-test.sock");
    let db_path = dir.path().join("test.db");

    {
        let db = tau_agent_lib::db::Db::open(&db_path).unwrap();
        insert_stored(&db, "a", None, false);
        insert_stored(&db, "b", Some("a"), false);
        insert_stored(&db, "c", Some("b"), false);
        // SQLite FKs are on: delete_session refuses to drop a parent with
        // children.  We need a raw DELETE that bypasses FK enforcement so
        // we can simulate the stale-FK race the docstring refers to.
        let raw = rusqlite::Connection::open(&db_path).unwrap();
        raw.execute_batch("PRAGMA foreign_keys=OFF;").unwrap();
        raw.execute("DELETE FROM sessions WHERE id = 'b'", [])
            .unwrap();
    }

    let model = mock_model();
    let mut registry = tau_agent_lib::provider::ProviderRegistry::new();
    registry.register(MockProvider::new(vec![]));
    let config = tau_agent_lib::server::TestServerConfig {
        registry,
        models: vec![model],
        socket_path: sock_path.clone(),
        db_path,
        tool_executor_factory: None,
        mock_tools: vec![],
        plugins_config: None,
        aliases: std::collections::HashMap::new(),
    };
    let handle = std::thread::spawn(move || {
        smol::block_on(async {
            tau_agent_lib::server::run_with_config(config).await.ok();
        });
    });
    for _ in 0..50 {
        if sock_path.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(sock_path.exists());

    let conn = UnixStream::connect(&sock_path).unwrap();
    conn.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let resp = send_recv(
        &conn,
        &Request::GetSessionAncestors {
            session_id: "c".into(),
        },
    );
    let ancestors = match resp {
        Response::SessionAncestors { sessions } => sessions,
        other => panic!("expected SessionAncestors, got {:?}", other),
    };
    let ids: Vec<&str> = ancestors.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(
        ids,
        vec!["c"],
        "walk stops at missing mid-chain parent (does not include a)"
    );
    assert_eq!(ancestors[0].parent_id.as_deref(), Some("b"));

    if let Ok(c) = UnixStream::connect(&sock_path) {
        let req = serde_json::to_string(&Request::Shutdown { restart: false }).unwrap();
        let mut c = c;
        let _ = c.write_all(format!("{}\n", req).as_bytes());
        let _ = c.flush();
    }
    handle.join().ok();
}

// ---------------------------------------------------------------------------
// Seamless-restart (task 549) tests
// ---------------------------------------------------------------------------

/// Helper: start a server bound to the given socket+db with the given
/// queued mock responses. Returns the join handle of the server task.
///
/// Deliberately **not** routed through `TestServer::start_with_config`
/// (catalog #10, task #609): the seamless-restart tests need to reuse
/// the same socket+db across multiple server lifetimes, so the caller
/// owns the tempdir and paths. `start_with_config`'s "create tempdir +
/// return owning `TestServer`" contract is incompatible with that.
fn start_restart_server(
    sock_path: &std::path::Path,
    db_path: &std::path::Path,
    responses: Vec<MockResponse>,
) -> std::thread::JoinHandle<()> {
    // Keep the drain window short for tests so shutdown doesn't block
    // on the 180s production default.
    // SAFETY: the test harness runs each integration test in its own
    // process, so env-var mutation here is scoped to that process.
    unsafe {
        std::env::set_var("TAU_SHUTDOWN_DRAIN_SECS", "2");
    }
    let model = mock_model();
    let mut registry = tau_agent_lib::provider::ProviderRegistry::new();
    registry.register(MockProvider::new(responses));
    let config = tau_agent_lib::server::TestServerConfig {
        registry,
        models: vec![model],
        socket_path: sock_path.to_path_buf(),
        db_path: db_path.to_path_buf(),
        tool_executor_factory: None,
        mock_tools: vec![],
        plugins_config: None,
        aliases: std::collections::HashMap::new(),
    };
    std::thread::spawn(move || {
        smol::block_on(async {
            tau_agent_lib::server::run_with_config(config).await.ok();
        });
    })
}

fn wait_for_socket(path: &std::path::Path) {
    for _ in 0..100 {
        if path.exists() && UnixStream::connect(path).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("server socket did not appear: {}", path.display());
}

fn shutdown_server(sock_path: &std::path::Path, handle: std::thread::JoinHandle<()>) {
    if let Ok(mut c) = UnixStream::connect(sock_path) {
        let req = serde_json::to_string(&Request::Shutdown { restart: false }).unwrap();
        let _ = c.write_all(format!("{}\n", req).as_bytes());
        let _ = c.flush();
    }
    handle.join().ok();
}

#[test]
fn seamless_restart_resumes_session_with_trailing_tool_result() {
    // Arrange: persist a session whose last message is a `tool_result`
    // (the server was about to fire the next LLM call when it died).
    // On restart the auto-resume scan should pick it up and produce at
    // least one new assistant message.
    let dir = tempfile::tempdir().unwrap();
    let sock_path = dir.path().join("tau-restart.sock");
    let db_path = dir.path().join("tau-restart.db");

    {
        // Seed the DB directly — avoids the churn of driving a full
        // agent turn that fails mid-tool-call.
        use tau_agent_lib::db::{Db, StoredSession};
        use tau_agent_lib::types::*;
        let db = Db::open(&db_path).unwrap();
        let sid = "resume-test";
        db.create_session(&StoredSession {
            id: sid.into(),
            model: mock_model(),
            system_prompt: Some("test".into()),
            cwd: Some("/tmp".into()),
            is_subscription: false,
            created_at: tau_agent_lib::types::timestamp_ms() as i64,
            parent_id: None,
            child_budget: 0,
            tagline: None,
            archived: false,
            last_exit_status: None,
            last_phase: None,
            auto_archive: false,
            notify_parent: true,
            project_name: None,
        })
        .unwrap();
        db.append_message(sid, &Message::User(UserMessage::text("kick off")))
            .unwrap();
        // A tool-result as the tail signals "agent was about to make the
        // next LLM call when shutdown hit".
        db.append_message(
            sid,
            &Message::ToolResult(ToolResultMessage {
                tool_call_id: "tc1".into(),
                tool_name: "bash".into(),
                content: vec![ToolResultContent::Text(TextContent {
                    text: "ok".into(),
                    text_signature: None,
                })],
                details: None,
                is_error: false,
                timestamp: tau_agent_lib::types::timestamp_ms(),
                duration_ms: None,
                summary: None,
                post_persist_actions: Vec::new(),
            }),
        )
        .unwrap();
    }

    // Act: start the server. Auto-resume should pick up the seeded
    // session in the background and fire an LLM call that the mock
    // answers with "resumed response".
    let handle = start_restart_server(
        &sock_path,
        &db_path,
        vec![MockResponse::Text("resumed response".into())],
    );
    wait_for_socket(&sock_path);

    // Poll GetMessages until we see the resumed assistant reply or time out.
    let sid = "resume-test";
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut saw_resume = false;
    while std::time::Instant::now() < deadline {
        let conn = UnixStream::connect(&sock_path).unwrap();
        conn.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        if let Response::Messages { messages } = send_recv(
            &conn,
            &Request::GetMessages {
                session_id: sid.into(),
            },
        ) {
            let has_resumed_asst = messages.iter().any(|m| {
                if let tau_agent_lib::types::Message::Assistant(a) = m {
                    a.content
                        .iter()
                        .any(|c| matches!(c, tau_agent_lib::types::AssistantContent::Text(t) if t.text.contains("resumed")))
                } else {
                    false
                }
            });
            if has_resumed_asst {
                saw_resume = true;
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    shutdown_server(&sock_path, handle);
    assert!(
        saw_resume,
        "expected an assistant message containing 'resumed' after auto-resume"
    );
}

#[test]
fn seamless_restart_skips_completed_session() {
    // Regression: a session with last_exit_status == "completed" must NOT
    // be auto-resumed on startup, even if its tail looks incomplete.
    let dir = tempfile::tempdir().unwrap();
    let sock_path = dir.path().join("tau-completed.sock");
    let db_path = dir.path().join("tau-completed.db");

    {
        use tau_agent_lib::db::{Db, StoredSession};
        use tau_agent_lib::types::*;
        let db = Db::open(&db_path).unwrap();
        let sid = "completed-test";
        db.create_session(&StoredSession {
            id: sid.into(),
            model: mock_model(),
            system_prompt: Some("test".into()),
            cwd: Some("/tmp".into()),
            is_subscription: false,
            created_at: tau_agent_lib::types::timestamp_ms() as i64,
            parent_id: None,
            child_budget: 0,
            tagline: None,
            archived: false,
            last_exit_status: Some("completed".into()),
            last_phase: None,
            auto_archive: false,
            notify_parent: true,
            project_name: None,
        })
        .unwrap();
        db.append_message(sid, &Message::User(UserMessage::text("hi")))
            .unwrap();
    }

    // If the server wrongly resumes, it will try to consume a mock
    // response. We give it zero mock responses, so a wrong resume would
    // produce an error assistant message. Correct behaviour: no assistant
    // message ever appears.
    let handle = start_restart_server(&sock_path, &db_path, vec![]);
    wait_for_socket(&sock_path);

    // Give the auto-resume scan plenty of time to run if it was going to.
    std::thread::sleep(Duration::from_millis(500));

    let sid = "completed-test";
    let conn = UnixStream::connect(&sock_path).unwrap();
    conn.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let messages = match send_recv(
        &conn,
        &Request::GetMessages {
            session_id: sid.into(),
        },
    ) {
        Response::Messages { messages } => messages,
        other => panic!("{:?}", other),
    };
    let has_assistant = messages
        .iter()
        .any(|m| matches!(m, tau_agent_lib::types::Message::Assistant(_)));
    assert!(
        !has_assistant,
        "completed session must not be auto-resumed; got messages: {:?}",
        messages,
    );

    shutdown_server(&sock_path, handle);
}

#[test]
fn seamless_restart_skips_archived_session() {
    // Regression: archived sessions must not be auto-resumed.
    let dir = tempfile::tempdir().unwrap();
    let sock_path = dir.path().join("tau-archived.sock");
    let db_path = dir.path().join("tau-archived.db");

    {
        use tau_agent_lib::db::{Db, StoredSession};
        use tau_agent_lib::types::*;
        let db = Db::open(&db_path).unwrap();
        let sid = "archived-test";
        db.create_session(&StoredSession {
            id: sid.into(),
            model: mock_model(),
            system_prompt: Some("test".into()),
            cwd: Some("/tmp".into()),
            is_subscription: false,
            created_at: tau_agent_lib::types::timestamp_ms() as i64,
            parent_id: None,
            child_budget: 0,
            tagline: None,
            archived: true,
            last_exit_status: None,
            last_phase: None,
            auto_archive: false,
            notify_parent: true,
            project_name: None,
        })
        .unwrap();
        db.append_message(sid, &Message::User(UserMessage::text("hi")))
            .unwrap();
    }

    let handle = start_restart_server(&sock_path, &db_path, vec![]);
    wait_for_socket(&sock_path);
    std::thread::sleep(Duration::from_millis(500));

    let sid = "archived-test";
    let conn = UnixStream::connect(&sock_path).unwrap();
    conn.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let messages = match send_recv(
        &conn,
        &Request::GetMessages {
            session_id: sid.into(),
        },
    ) {
        Response::Messages { messages } => messages,
        other => panic!("{:?}", other),
    };
    let has_assistant = messages
        .iter()
        .any(|m| matches!(m, tau_agent_lib::types::Message::Assistant(_)));
    assert!(!has_assistant, "archived session must not be auto-resumed");

    shutdown_server(&sock_path, handle);
}

#[test]
fn seamless_restart_rejects_new_chat_during_drain() {
    // While the server is in the shutdown-drain window, a fresh Chat
    // request must return the distinctive ServerShuttingDown error so
    // clients know to reconnect rather than surfacing the failure.
    //
    // Keep the drain window long enough for the test to latch: fire an
    // in-flight Delayed chat, then request shutdown. The server
    // continues accepting connections (so our new Chat lands) but flips
    // `is_shutting_down=true`, which is what rejects the new turn.
    let server = TestServer::start(vec![
        MockResponse::Delayed {
            delay_ms: 3_000,
            response: Box::new(MockResponse::Text("slow".into())),
        },
        MockResponse::Text("never used".into()),
    ]);
    let sid = match CreateSessionBuilder::new(&server)
        .system_prompt("t")
        .cwd("/tmp")
        .send_raw()
    {
        Response::SessionCreated { session_id } => session_id,
        other => panic!("{:?}", other),
    };

    // Kick off a slow chat in a background thread so the server has an
    // in-flight turn to drain around.
    let chat_sock = server.sock_path.clone();
    let chat_sid = sid.clone();
    let chat_handle = std::thread::spawn(move || {
        let chat_conn = UnixStream::connect(&chat_sock).unwrap();
        chat_conn
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let _ = send_recv_all(
            &chat_conn,
            &Request::Chat {
                session_id: chat_sid,
                text: "go".into(),
            },
        );
    });
    // Give the chat a moment to register as in-flight.
    std::thread::sleep(Duration::from_millis(300));

    // Shut down the server. request_shutdown flips the flag; the drain
    // loop then waits for in-flight turns. Use a dedicated connection
    // and don't wait for the ack (the server may close as soon as
    // request_shutdown returns).
    let mut shutdown_conn = server.connect();
    let req = serde_json::to_string(&Request::Shutdown { restart: true }).unwrap();
    let _ = shutdown_conn.write_all(format!("{}\n", req).as_bytes());
    let _ = shutdown_conn.flush();
    std::thread::sleep(Duration::from_millis(200));

    // Try to start a new Chat on the draining server. The server is
    // still accepting connections (its accept loop only exits after
    // drain) but must reject Chat with the distinctive signal.
    let mut signalled = false;
    if let Ok(conn2) = UnixStream::connect(&server.sock_path) {
        conn2
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut conn2 = conn2;
        let chat = serde_json::to_string(&Request::Chat {
            session_id: sid.clone(),
            text: "hi".into(),
        })
        .unwrap();
        let _ = conn2.write_all(format!("{}\n", chat).as_bytes());
        let _ = conn2.flush();
        use std::io::{BufRead, BufReader};
        let reader = BufReader::new(conn2);
        for line_res in reader.lines() {
            let Ok(line) = line_res else { break };
            if line.trim().is_empty() {
                continue;
            }
            let Ok(resp) = serde_json::from_str::<Response>(&line) else {
                continue;
            };
            if matches!(resp, Response::ServerShutdown { .. }) {
                signalled = true;
                break;
            }
            if let Response::Error { message } = &resp
                && tau_agent_lib::protocol::is_shutting_down_error(message)
            {
                signalled = true;
                break;
            }
            if matches!(resp, Response::AgentDone | Response::Cancelled) {
                break;
            }
        }
    }
    chat_handle.join().ok();
    assert!(signalled, "expected server-shutting-down signal");
}

// ---------------------------------------------------------------------------
// Bug #583 regression: every `Err` return from the agent runner must emit
// both `Response::Error` and `Response::AgentDone` so the TUI / other
// subscribers never get stuck waiting for a terminal event.
// ---------------------------------------------------------------------------

/// A `Chat` request on a session whose provider has no API key must emit
/// **both** `Response::Error` and `Response::AgentDone` (in that order),
/// and the Error must precede AgentDone on the subscription stream. This
/// is the server-side invariant the TUI relies on to leave Streaming
/// mode.
#[test]
fn chat_no_api_key_emits_error_and_agent_done_in_order() {
    let server = TestServer::start_without_api_key();
    let sock_path = &server.sock_path;

    // Create a session using the key-less model.
    let conn = UnixStream::connect(&sock_path).unwrap();
    conn.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let sid = match send_recv(
        &conn,
        &CreateSessionBuilder::standalone()
            .model("needs-key-model-583")
            .provider("bogus-provider-583-no-such-key")
            .system_prompt("t")
            .cwd("/tmp")
            .build(),
    ) {
        Response::SessionCreated { session_id } => session_id,
        other => panic!("expected SessionCreated, got {:?}", other),
    };

    // Chat — should return Error + AgentDone.
    let conn = UnixStream::connect(&sock_path).unwrap();
    conn.set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let responses = send_recv_all(
        &conn,
        &Request::Chat {
            session_id: sid,
            text: "hi".into(),
        },
    );

    let err_idx = responses
        .iter()
        .position(|r| matches!(r, Response::Error { .. }));
    let done_idx = responses
        .iter()
        .position(|r| matches!(r, Response::AgentDone));

    assert!(
        err_idx.is_some(),
        "expected Response::Error for NoApiKey, got: {:?}",
        responses
    );
    assert!(
        done_idx.is_some(),
        "expected Response::AgentDone after Error, got: {:?}",
        responses
    );
    assert!(
        err_idx.unwrap() < done_idx.unwrap(),
        "Error must precede AgentDone, got: {:?}",
        responses
    );

    // The Error message mentions the missing API key — confirm it's
    // the right error and not some other failure.
    let err_msg = responses
        .iter()
        .find_map(|r| match r {
            Response::Error { message } => Some(message.clone()),
            _ => None,
        })
        .expect("error message");
    assert!(
        err_msg.to_lowercase().contains("api key"),
        "error should be about missing API key, got: {}",
        err_msg
    );
}

/// Same invariant verified via a subscribed side channel: a second
/// connection subscribed to the session must observe Error then
/// AgentDone even though Chat itself is what triggered the failure.
/// This is the code path the TUI actually uses (Subscribe + fire
/// Chat on a separate connection).
#[test]
fn subscriber_sees_error_then_agent_done_on_no_api_key() {
    let server = TestServer::start_without_api_key();
    let sock_path = &server.sock_path;

    let conn_create = UnixStream::connect(&sock_path).unwrap();
    conn_create
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let sid = match send_recv(
        &conn_create,
        &CreateSessionBuilder::standalone()
            .model("needs-key-model-583")
            .provider("bogus-provider-583-no-such-key")
            .system_prompt("t")
            .cwd("/tmp")
            .build(),
    ) {
        Response::SessionCreated { session_id } => session_id,
        other => panic!("expected SessionCreated, got {:?}", other),
    };

    // Subscriber connection (TUI analogue).
    use std::io::{BufRead, BufReader};
    let sub_conn = UnixStream::connect(&sock_path).unwrap();
    sub_conn
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let sub_sid = sid.clone();
    let sub_handle = std::thread::spawn(move || {
        let mut s = sub_conn;
        let sub_req = Request::Subscribe {
            session_id: sub_sid,
        };
        let line = format!("{}\n", serde_json::to_string(&sub_req).unwrap());
        s.write_all(line.as_bytes()).unwrap();
        s.flush().unwrap();
        let reader = BufReader::new(s);
        let mut collected: Vec<Response> = Vec::new();
        for line_res in reader.lines() {
            let Ok(line) = line_res else { break };
            if line.trim().is_empty() {
                continue;
            }
            let Ok(resp) = serde_json::from_str::<Response>(&line) else {
                continue;
            };
            let is_done = matches!(resp, Response::AgentDone);
            collected.push(resp);
            if is_done {
                break;
            }
        }
        collected
    });

    // Give the subscriber a moment to register.
    std::thread::sleep(Duration::from_millis(100));

    // Trigger a Chat on a separate connection — fire-and-forget style.
    let chat_conn = UnixStream::connect(&sock_path).unwrap();
    chat_conn
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let _ = send_recv_all(
        &chat_conn,
        &Request::Chat {
            session_id: sid,
            text: "hi".into(),
        },
    );

    let events = sub_handle.join().expect("subscriber thread panicked");
    let err_idx = events
        .iter()
        .position(|r| matches!(r, Response::Error { .. }));
    let done_idx = events.iter().position(|r| matches!(r, Response::AgentDone));
    assert!(
        err_idx.is_some(),
        "subscriber should observe Error, got: {:?}",
        events
    );
    assert!(
        done_idx.is_some(),
        "subscriber should observe AgentDone, got: {:?}",
        events
    );
    assert!(
        err_idx.unwrap() < done_idx.unwrap(),
        "subscriber must see Error *before* AgentDone, got: {:?}",
        events
    );
}

// ---------------------------------------------------------------------------
// Task #590 regression: CreateSession must not silently inherit a
// no-agent-loop (log) model from its parent.
// ---------------------------------------------------------------------------

/// Regression for task #590.
///
/// Before the fix, creating a session with `model = None` and a `log`-
/// provider parent silently inherited `log` as the new session's model.
/// That made the child a second placeholder (no-agent-loop), silently
/// bricking worker dispatches when the scheduler ran without a triggering
/// session id.
///
/// After the fix, inheriting from a `needs_api_key == false` parent is
/// suppressed server-side: the new session falls back to the server's
/// configured default model instead.
#[test]
fn create_session_with_none_model_and_log_parent_uses_default() {
    use tau_agent_lib::providers::log::log_model;

    let server = TestServer::start_mock_plus_log();
    let sock_path = &server.sock_path;

    // Create a log-provider parent session (simulates the task placeholder).
    let parent_conn = UnixStream::connect(&sock_path).unwrap();
    parent_conn
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let parent_id = match send_recv(
        &parent_conn,
        &CreateSessionBuilder::standalone()
            .model("log")
            .provider("log")
            .cwd("/tmp")
            .child_budget(4)
            .tagline("placeholder")
            .notify_parent(false)
            .build(),
    ) {
        Response::SessionCreated { session_id } => session_id,
        other => panic!("expected SessionCreated, got {:?}", other),
    };

    // Sanity-check: the parent really is the log model.
    let info_conn = UnixStream::connect(&sock_path).unwrap();
    info_conn
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    match send_recv(
        &info_conn,
        &Request::GetSessionInfo {
            session_id: parent_id.clone(),
        },
    ) {
        Response::SessionInfo { info } => assert_eq!(info.model, log_model().id),
        other => panic!("expected SessionInfo, got {:?}", other),
    }

    // Create a child with no explicit model. Historically this inherited
    // the parent's model (log) and bricked the child.
    let child_conn = UnixStream::connect(&sock_path).unwrap();
    child_conn
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let child_id = match send_recv(
        &child_conn,
        &CreateSessionBuilder::standalone()
            .parent(parent_id.clone())
            .cwd("/tmp")
            .tagline("worker")
            .notify_parent(false)
            .build(),
    ) {
        Response::SessionCreated { session_id } => session_id,
        other => panic!("expected SessionCreated, got {:?}", other),
    };

    // The child must NOT be a log session: it should have fallen back to
    // the server's default model (the mock model).
    let info_conn = UnixStream::connect(&sock_path).unwrap();
    info_conn
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    match send_recv(
        &info_conn,
        &Request::GetSessionInfo {
            session_id: child_id.clone(),
        },
    ) {
        Response::SessionInfo { info } => {
            assert_ne!(
                info.model, "log",
                "child must not inherit log model from placeholder parent"
            );
            assert_eq!(
                info.model,
                mock_model().id,
                "child should fall back to default model when parent is a no-agent-loop session"
            );
        }
        other => panic!("expected SessionInfo, got {:?}", other),
    }

    // Shutdown.
    let conn = UnixStream::connect(&sock_path).unwrap();
    conn.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    send_recv(&conn, &Request::Shutdown { restart: false });
}
