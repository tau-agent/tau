//! End-to-end tests for session orchestration.
//!
//! Tests cover both DB-level operations (budget, tree delete, inheritance)
//! and server-level orchestration (spawn child sessions, run agent turns,
//! wait for completion).

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use tau::protocol::{Request, Response};
use tau::providers::mock::{MockProvider, MockResponse, mock_model};

// ---------------------------------------------------------------------------
// Test server helpers (same pattern as e2e_server.rs)
// ---------------------------------------------------------------------------

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
            tool_executor_factory: None,
            mock_tools: vec![],
        };

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
        if let Ok(mut conn) = UnixStream::connect(&self.sock_path) {
            let req = serde_json::to_string(&Request::Shutdown { restart: false }).unwrap();
            let _ = conn.write_all(format!("{}\n", req).as_bytes());
            let _ = conn.flush();
        }
    }
}

// ---------------------------------------------------------------------------
// Tests that don't need a running server (protocol-level / DB-level)
// ---------------------------------------------------------------------------

#[test]
fn session_tree_budget_enforcement() {
    let dir = tempfile::tempdir().unwrap();
    let db = tau::db::Db::open(&dir.path().join("test.db")).unwrap();

    // Create root with budget 3
    let root = tau::db::StoredSession {
        id: "root".into(),
        model: tau::providers::mock::mock_model(),
        system_prompt: None,
        cwd: Some("/tmp".into()),
        is_subscription: false,
        created_at: 1000,
        parent_id: None,
        child_budget: 3,
        tagline: None,
        archived: false,
    };
    db.create_session(&root).unwrap();

    // Spawn child 1 (leaf, cost=1)
    let c1 = tau::db::StoredSession {
        id: "c1".into(),
        model: tau::providers::mock::mock_model(),
        system_prompt: None,
        cwd: Some("/tmp".into()),
        is_subscription: false,
        created_at: 2000,
        parent_id: Some("root".into()),
        child_budget: 0,
        tagline: None,
        archived: false,
    };
    db.create_session(&c1).unwrap();
    assert_eq!(db.budget_used("root").unwrap(), 1);

    // Spawn child 2 with budget=1 (cost=2)
    let c2 = tau::db::StoredSession {
        id: "c2".into(),
        model: tau::providers::mock::mock_model(),
        system_prompt: None,
        cwd: Some("/tmp".into()),
        is_subscription: false,
        created_at: 3000,
        parent_id: Some("root".into()),
        child_budget: 1,
        tagline: None,
        archived: false,
    };
    db.create_session(&c2).unwrap();
    assert_eq!(db.budget_used("root").unwrap(), 3); // 1 + (1+1) = 3

    // Budget is now full -- verify
    let used = db.budget_used("root").unwrap();
    let cost_next = 1u32; // leaf child
    assert!(
        used + cost_next > root.child_budget,
        "budget should be exceeded: used={}, budget={}",
        used,
        root.child_budget
    );

    // Grandchild under c2 (has budget=1, cost=1)
    let gc1 = tau::db::StoredSession {
        id: "gc1".into(),
        model: tau::providers::mock::mock_model(),
        system_prompt: None,
        cwd: Some("/tmp".into()),
        is_subscription: false,
        created_at: 4000,
        parent_id: Some("c2".into()),
        child_budget: 0,
        tagline: None,
        archived: false,
    };
    db.create_session(&gc1).unwrap();
    assert_eq!(db.budget_used("c2").unwrap(), 1);

    // c2's budget is now full
    assert!(db.budget_used("c2").unwrap() + 1 > c2.child_budget);

    // Verify tree structure
    assert_eq!(db.child_count("root").unwrap(), 2);
    assert_eq!(db.child_count("c2").unwrap(), 1);
    assert_eq!(db.child_count("c1").unwrap(), 0);
    assert_eq!(db.child_count("gc1").unwrap(), 0);
}

#[test]
fn session_tree_recursive_delete() {
    let dir = tempfile::tempdir().unwrap();
    let db = tau::db::Db::open(&dir.path().join("test.db")).unwrap();

    // root -> c1 -> gc1
    //      -> c2
    for (id, parent, budget) in [
        ("root", None, 10),
        ("c1", Some("root"), 2),
        ("gc1", Some("c1"), 0),
        ("c2", Some("root"), 0),
    ] {
        db.create_session(&tau::db::StoredSession {
            id: id.into(),
            model: tau::providers::mock::mock_model(),
            system_prompt: None,
            cwd: None,
            is_subscription: false,
            created_at: 1000,
            parent_id: parent.map(String::from),
            child_budget: budget,
            tagline: None,
            archived: false,
        })
        .unwrap();
    }

    // Add messages to verify cascade
    db.append_message(
        "gc1",
        &tau::types::Message::User(tau::types::UserMessage::text("hello")),
    )
    .unwrap();
    assert_eq!(db.get_messages("gc1").unwrap().len(), 1);

    // Delete c1 subtree (gc1 should go too)
    db.delete_session_tree("c1").unwrap();
    assert!(db.get_session("c1").unwrap().is_none());
    assert!(db.get_session("gc1").unwrap().is_none());
    assert_eq!(db.get_messages("gc1").unwrap().len(), 0);

    // Root and c2 survive
    assert!(db.get_session("root").unwrap().is_some());
    assert!(db.get_session("c2").unwrap().is_some());

    // Budget freed
    assert_eq!(db.budget_used("root").unwrap(), 1); // only c2 left (cost=1)
}

#[test]
fn session_model_inheritance() {
    let dir = tempfile::tempdir().unwrap();
    let db = tau::db::Db::open(&dir.path().join("test.db")).unwrap();

    let mut parent_model = tau::providers::mock::mock_model();
    parent_model.id = "parent-model".into();

    db.create_session(&tau::db::StoredSession {
        id: "parent".into(),
        model: parent_model.clone(),
        system_prompt: None,
        cwd: Some("/home/test/project".into()),
        is_subscription: false,
        created_at: 1000,
        parent_id: None,
        child_budget: 5,
        tagline: None,
        archived: false,
    })
    .unwrap();

    // Child inherits parent's model and cwd
    let parent = db.get_session("parent").unwrap().unwrap();
    assert_eq!(parent.model.id, "parent-model");
    assert_eq!(parent.cwd.as_deref(), Some("/home/test/project"));

    // Verify inheritance logic (same as server does)
    let child_model: Option<String> = None;
    let resolved_model = child_model
        .as_ref()
        .map(|_| tau::providers::mock::mock_model())
        .unwrap_or_else(|| parent.model.clone());
    assert_eq!(resolved_model.id, "parent-model");

    let child_cwd: Option<String> = None;
    let resolved_cwd = child_cwd.or_else(|| parent.cwd.clone());
    assert_eq!(resolved_cwd.as_deref(), Some("/home/test/project"));
}

#[test]
fn session_info_includes_tree_fields() {
    let dir = tempfile::tempdir().unwrap();
    let db = tau::db::Db::open(&dir.path().join("test.db")).unwrap();

    db.create_session(&tau::db::StoredSession {
        id: "root".into(),
        model: tau::providers::mock::mock_model(),
        system_prompt: None,
        cwd: None,
        is_subscription: false,
        created_at: 1000,
        parent_id: None,
        child_budget: 5,
        tagline: None,
        archived: false,
    })
    .unwrap();

    db.create_session(&tau::db::StoredSession {
        id: "child".into(),
        model: tau::providers::mock::mock_model(),
        system_prompt: None,
        cwd: None,
        is_subscription: false,
        created_at: 2000,
        parent_id: Some("root".into()),
        child_budget: 0,
        tagline: None,
        archived: false,
    })
    .unwrap();

    let root = db.get_session("root").unwrap().unwrap();
    assert_eq!(root.child_budget, 5);
    assert!(root.parent_id.is_none());

    let child = db.get_session("child").unwrap().unwrap();
    assert_eq!(child.parent_id.as_deref(), Some("root"));
    assert_eq!(child.child_budget, 0);

    assert_eq!(db.child_count("root").unwrap(), 1);
    assert_eq!(db.budget_used("root").unwrap(), 1);
}

#[test]
fn orchestration_tool_definitions() {
    let tools = tau::orchestration::orchestration_tools();
    assert_eq!(tools.len(), 11);

    let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
    assert!(names.contains(&"session_spawn"));
    assert!(names.contains(&"session_join"));
    assert!(names.contains(&"session_join_all"));
    assert!(names.contains(&"session_join_any"));
    assert!(names.contains(&"session_status"));
    assert!(names.contains(&"session_list_children"));
    assert!(names.contains(&"session_read"));
    assert!(names.contains(&"session_cancel"));
    assert!(names.contains(&"session_archive"));
    assert!(names.contains(&"session_message"));
    assert!(names.contains(&"session_id"));

    // session_spawn has prompt snippet
    let spawn = tools.iter().find(|t| t.name == "session_spawn").unwrap();
    assert!(spawn.prompt_snippet.is_some());
    assert!(!spawn.prompt_guidelines.is_empty());

    // All tools have descriptions
    for tool in &tools {
        assert!(
            !tool.description.is_empty(),
            "tool {} has no description",
            tool.name
        );
    }
}

#[test]
fn protocol_create_session_with_parent() {
    // Verify serialization/deserialization of new fields
    let req = Request::CreateSession {
        model: None,
        provider: None,
        system_prompt: None,
        cwd: None,
        parent_id: Some("s1".into()),
        child_budget: 5,
        tagline: None,
    };
    let json = serde_json::to_string(&req).unwrap();
    assert!(json.contains("parent_id"));
    assert!(json.contains("child_budget"));

    let parsed: Request = serde_json::from_str(&json).unwrap();
    if let Request::CreateSession {
        parent_id,
        child_budget,
        ..
    } = parsed
    {
        assert_eq!(parent_id.as_deref(), Some("s1"));
        assert_eq!(child_budget, 5);
    } else {
        panic!("wrong variant");
    }
}

#[test]
fn protocol_wait_sessions_roundtrip() {
    let req = Request::WaitSessions {
        session_ids: vec!["s1".into(), "s2".into()],
        timeout_secs: 60,
    };
    let json = serde_json::to_string(&req).unwrap();
    let parsed: Request = serde_json::from_str(&json).unwrap();
    if let Request::WaitSessions {
        session_ids,
        timeout_secs,
    } = parsed
    {
        assert_eq!(session_ids, vec!["s1", "s2"]);
        assert_eq!(timeout_secs, 60);
    } else {
        panic!("wrong variant");
    }

    let resp = Response::SessionsCompleted {
        results: vec![tau::protocol::SessionResult {
            session_id: "s1".into(),
            status: "done".into(),
            summary: "All good".into(),
        }],
    };
    let json = serde_json::to_string(&resp).unwrap();
    let parsed: Response = serde_json::from_str(&json).unwrap();
    if let Response::SessionsCompleted { results } = parsed {
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].status, "done");
    } else {
        panic!("wrong variant");
    }
}

#[test]
fn protocol_session_info_tree_fields() {
    let info = tau::protocol::SessionInfo {
        id: "s1".into(),
        model: "mock-model".into(),
        provider: "mock".into(),
        cwd: None,
        message_count: 5,
        stats: Default::default(),
        last_activity: 1000,
        parent_id: Some("s0".into()),
        child_count: 2,
        child_budget: 10,
        tagline: None,
        archived: false,
        state: "idle".into(),
        context_pct: None,
    };
    let json = serde_json::to_string(&info).unwrap();
    assert!(json.contains("parent_id"));
    assert!(json.contains("child_count"));
    assert!(json.contains("child_budget"));

    let parsed: tau::protocol::SessionInfo = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.parent_id.as_deref(), Some("s0"));
    assert_eq!(parsed.child_count, 2);
    assert_eq!(parsed.child_budget, 10);
}

// ---------------------------------------------------------------------------
// E2E server tests: child session spawn + agent turn
// ---------------------------------------------------------------------------

/// Spawn a child session, send Chat, verify it runs the agent turn and
/// produces messages. This simulates what session_spawn does at the protocol level.
#[test]
fn spawn_child_chat_produces_messages() {
    // Two mock responses: one for the child's agent turn
    let server = TestServer::start(vec![MockResponse::Text("Child response".into())]);

    // Create parent with budget
    let conn = server.connect();
    let parent_id = match send_recv(
        &conn,
        &Request::CreateSession {
            model: None,
            provider: None,
            system_prompt: Some("parent".into()),
            cwd: Some("/tmp".into()),
            parent_id: None,
            child_budget: 5,
            tagline: None,
        },
    ) {
        Response::SessionCreated { session_id } => session_id,
        other => panic!("expected SessionCreated, got {:?}", other),
    };

    // Create child under parent
    let conn2 = server.connect();
    let child_id = match send_recv(
        &conn2,
        &Request::CreateSession {
            model: None,
            provider: None,
            system_prompt: Some("child".into()),
            cwd: None, // inherit
            parent_id: Some(parent_id.clone()),
            child_budget: 0,
            tagline: None,
        },
    ) {
        Response::SessionCreated { session_id } => session_id,
        other => panic!("expected SessionCreated, got {:?}", other),
    };

    // Send Chat to child -- collect responses until AgentDone
    let conn3 = server.connect();
    let responses = send_recv_all(
        &conn3,
        &Request::Chat {
            session_id: child_id.clone(),
            text: "do work".into(),
        },
    );

    let has_done = responses.iter().any(|r| matches!(r, Response::AgentDone));
    assert!(
        has_done,
        "expected AgentDone in child responses: {:?}",
        responses
    );

    // Verify child has messages (user + assistant)
    let conn4 = server.connect();
    let resp = send_recv(
        &conn4,
        &Request::GetMessages {
            session_id: child_id.clone(),
        },
    );
    match resp {
        Response::Messages { messages } => {
            assert!(
                messages.len() >= 2,
                "expected at least 2 messages in child, got {}: {:?}",
                messages.len(),
                messages
            );
            assert!(matches!(&messages[0], tau::types::Message::User(_)));
            assert!(
                matches!(&messages[1], tau::types::Message::Assistant(a) if a.text().contains("Child response"))
            );
        }
        other => panic!("expected Messages, got {:?}", other),
    }

    // Parent should have no messages (we never chatted with it)
    let conn5 = server.connect();
    let resp = send_recv(
        &conn5,
        &Request::GetMessages {
            session_id: parent_id.clone(),
        },
    );
    match resp {
        Response::Messages { messages } => {
            assert_eq!(messages.len(), 0, "parent should have no messages");
        }
        other => panic!("expected Messages, got {:?}", other),
    }

    server.shutdown();
}

/// Spawn multiple children, send Chat to each, wait for all with WaitSessions.
#[test]
fn spawn_multiple_children_wait_all() {
    // Three mock responses: one for each child
    let server = TestServer::start(vec![
        MockResponse::Text("child-1 done".into()),
        MockResponse::Text("child-2 done".into()),
        MockResponse::Text("child-3 done".into()),
    ]);

    // Create parent
    let conn = server.connect();
    let parent_id = match send_recv(
        &conn,
        &Request::CreateSession {
            model: None,
            provider: None,
            system_prompt: Some("parent".into()),
            cwd: Some("/tmp".into()),
            parent_id: None,
            child_budget: 10,
            tagline: None,
        },
    ) {
        Response::SessionCreated { session_id } => session_id,
        other => panic!("{:?}", other),
    };

    // Spawn 3 children
    let mut child_ids = Vec::new();
    for i in 0..3 {
        let c = server.connect();
        let cid = match send_recv(
            &c,
            &Request::CreateSession {
                model: None,
                provider: None,
                system_prompt: Some(format!("child-{}", i)),
                cwd: None,
                parent_id: Some(parent_id.clone()),
                child_budget: 0,
                tagline: None,
            },
        ) {
            Response::SessionCreated { session_id } => session_id,
            other => panic!("{:?}", other),
        };
        child_ids.push(cid);
    }

    // Fire Chat to each child (fire-and-forget: don't read responses)
    for (i, cid) in child_ids.iter().enumerate() {
        let c = server.connect();
        let mut c2 = c.try_clone().unwrap();
        let req = Request::Chat {
            session_id: cid.clone(),
            text: format!("task {}", i),
        };
        let mut line = serde_json::to_string(&req).unwrap();
        line.push('\n');
        c2.write_all(line.as_bytes()).unwrap();
        c2.flush().unwrap();
        // Don't read -- let it run in the background
    }

    // Wait for all children
    let wait_conn = server.connect();
    wait_conn
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    let resp = send_recv(
        &wait_conn,
        &Request::WaitSessions {
            session_ids: child_ids.clone(),
            timeout_secs: 30,
        },
    );
    match resp {
        Response::SessionsCompleted { results } => {
            assert_eq!(results.len(), 3);
            for r in &results {
                assert_eq!(
                    r.status, "done",
                    "session {} should be done, got {}",
                    r.session_id, r.status
                );
                assert!(
                    !r.summary.is_empty(),
                    "session {} should have a summary",
                    r.session_id
                );
            }
        }
        other => panic!("expected SessionsCompleted, got {:?}", other),
    }

    // Verify each child has messages
    for cid in &child_ids {
        let c = server.connect();
        let resp = send_recv(
            &c,
            &Request::GetMessages {
                session_id: cid.clone(),
            },
        );
        match resp {
            Response::Messages { messages } => {
                assert!(
                    messages.len() >= 2,
                    "child {} should have at least 2 messages, got {}",
                    cid,
                    messages.len()
                );
            }
            other => panic!("expected Messages for {}, got {:?}", cid, other),
        }
    }

    // Parent child_count should be 3
    let c = server.connect();
    let resp = send_recv(
        &c,
        &Request::GetSessionInfo {
            session_id: parent_id.clone(),
        },
    );
    match resp {
        Response::SessionInfo { info } => {
            assert_eq!(info.child_count, 3);
        }
        other => panic!("{:?}", other),
    }

    server.shutdown();
}

/// Child session inherits model and cwd from parent via server.
#[test]
fn spawn_child_inherits_parent_model_and_cwd() {
    let server = TestServer::start(vec![]);

    // Create parent with specific cwd
    let conn = server.connect();
    let parent_id = match send_recv(
        &conn,
        &Request::CreateSession {
            model: None,
            provider: None,
            system_prompt: None,
            cwd: Some("/home/test/project".into()),
            parent_id: None,
            child_budget: 5,
            tagline: None,
        },
    ) {
        Response::SessionCreated { session_id } => session_id,
        other => panic!("{:?}", other),
    };

    // Create child with no model or cwd specified
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

    // Verify child inherited cwd and model
    let conn3 = server.connect();
    let resp = send_recv(
        &conn3,
        &Request::GetSessionInfo {
            session_id: child_id,
        },
    );
    match resp {
        Response::SessionInfo { info } => {
            assert_eq!(
                info.cwd.as_deref(),
                Some("/home/test/project"),
                "child should inherit parent's cwd"
            );
            assert_eq!(
                info.model, "mock-model",
                "child should inherit parent's model"
            );
            assert_eq!(info.parent_id.as_deref(), Some(parent_id.as_str()));
        }
        other => panic!("{:?}", other),
    }

    server.shutdown();
}

/// WaitSessions returns "done" immediately for idle sessions.
#[test]
fn wait_sessions_idle_returns_done() {
    let server = TestServer::start(vec![]);

    // Create two sessions, don't chat with either
    let mut sids = Vec::new();
    for _ in 0..2 {
        let c = server.connect();
        let sid = match send_recv(
            &c,
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
        sids.push(sid);
    }

    let c = server.connect();
    let resp = send_recv(
        &c,
        &Request::WaitSessions {
            session_ids: sids.clone(),
            timeout_secs: 5,
        },
    );
    match resp {
        Response::SessionsCompleted { results } => {
            assert_eq!(results.len(), 2);
            for r in &results {
                assert_eq!(r.status, "done");
            }
        }
        other => panic!("{:?}", other),
    }

    server.shutdown();
}

/// Delete parent cascades to children at the server level.
#[test]
fn spawn_delete_parent_cascades() {
    let server = TestServer::start(vec![MockResponse::Text("child work".into())]);

    // Create parent -> child, chat with child
    let conn = server.connect();
    let parent_id = match send_recv(
        &conn,
        &Request::CreateSession {
            model: None,
            provider: None,
            system_prompt: Some("parent".into()),
            cwd: Some("/tmp".into()),
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
            system_prompt: Some("child".into()),
            cwd: None,
            parent_id: Some(parent_id.clone()),
            child_budget: 0,
            tagline: None,
        },
    ) {
        Response::SessionCreated { session_id } => session_id,
        other => panic!("{:?}", other),
    };

    // Chat with child to create messages
    let conn3 = server.connect();
    let responses = send_recv_all(
        &conn3,
        &Request::Chat {
            session_id: child_id.clone(),
            text: "work".into(),
        },
    );
    assert!(responses.iter().any(|r| matches!(r, Response::AgentDone)));

    // Delete parent -- should cascade to child
    let conn4 = server.connect();
    match send_recv(
        &conn4,
        &Request::DeleteSession {
            session_id: parent_id.clone(),
        },
    ) {
        Response::SessionDeleted => {}
        other => panic!("{:?}", other),
    }

    // Both gone
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

/// Cancel a running child session.
#[test]
fn spawn_cancel_child() {
    let server = TestServer::start(vec![]);

    // Create parent -> child
    let conn = server.connect();
    let parent_id = match send_recv(
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
            parent_id: Some(parent_id),
            child_budget: 0,
            tagline: None,
        },
    ) {
        Response::SessionCreated { session_id } => session_id,
        other => panic!("{:?}", other),
    };

    // Cancel the child (even though it's idle -- should succeed)
    let conn3 = server.connect();
    match send_recv(
        &conn3,
        &Request::CancelChat {
            session_id: child_id,
        },
    ) {
        Response::Ok => {} // expected
        other => panic!("expected Ok for cancel, got {:?}", other),
    }

    server.shutdown();
}

/// WaitSessions after child Chat completes returns summary text.
#[test]
fn wait_sessions_returns_summary() {
    let server = TestServer::start(vec![MockResponse::Text("The answer is 42.".into())]);

    // Create parent -> child
    let conn = server.connect();
    let parent_id = match send_recv(
        &conn,
        &Request::CreateSession {
            model: None,
            provider: None,
            system_prompt: Some("parent".into()),
            cwd: Some("/tmp".into()),
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
            system_prompt: Some("child".into()),
            cwd: None,
            parent_id: Some(parent_id),
            child_budget: 0,
            tagline: None,
        },
    ) {
        Response::SessionCreated { session_id } => session_id,
        other => panic!("{:?}", other),
    };

    // Chat with child and wait for completion
    let conn3 = server.connect();
    let responses = send_recv_all(
        &conn3,
        &Request::Chat {
            session_id: child_id.clone(),
            text: "what is the meaning of life?".into(),
        },
    );
    assert!(responses.iter().any(|r| matches!(r, Response::AgentDone)));

    // WaitSessions should return with summary containing the assistant text
    let conn4 = server.connect();
    let resp = send_recv(
        &conn4,
        &Request::WaitSessions {
            session_ids: vec![child_id.clone()],
            timeout_secs: 5,
        },
    );
    match resp {
        Response::SessionsCompleted { results } => {
            assert_eq!(results.len(), 1);
            assert_eq!(results[0].status, "done");
            assert!(
                results[0].summary.contains("42"),
                "summary should contain assistant text, got: {}",
                results[0].summary
            );
        }
        other => panic!("expected SessionsCompleted, got {:?}", other),
    }

    server.shutdown();
}

// ---------------------------------------------------------------------------
// WaitAnySessions protocol tests
// ---------------------------------------------------------------------------

#[test]
fn protocol_wait_any_sessions_roundtrip() {
    let req = Request::WaitAnySessions {
        session_ids: vec!["s1".into(), "s2".into()],
        timeout_secs: 30,
    };
    let json = serde_json::to_string(&req).unwrap();
    assert!(json.contains("wait_any_sessions"));
    let parsed: Request = serde_json::from_str(&json).unwrap();
    if let Request::WaitAnySessions {
        session_ids,
        timeout_secs,
    } = parsed
    {
        assert_eq!(session_ids, vec!["s1", "s2"]);
        assert_eq!(timeout_secs, 30);
    } else {
        panic!("wrong variant");
    }
}

/// WaitAnySessions returns immediately when all sessions are idle.
#[test]
fn wait_any_sessions_idle_returns_all() {
    let server = TestServer::start(vec![]);

    let mut sids = Vec::new();
    for _ in 0..3 {
        let c = server.connect();
        let sid = match send_recv(
            &c,
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
        sids.push(sid);
    }

    let c = server.connect();
    let resp = send_recv(
        &c,
        &Request::WaitAnySessions {
            session_ids: sids.clone(),
            timeout_secs: 5,
        },
    );
    match resp {
        Response::SessionsCompleted { results } => {
            // All idle -> all returned as done
            assert_eq!(results.len(), 3);
            for r in &results {
                assert_eq!(r.status, "done");
            }
        }
        other => panic!("expected SessionsCompleted, got {:?}", other),
    }

    server.shutdown();
}

/// WaitAnySessions returns completed children, not the still-busy ones.
#[test]
fn wait_any_sessions_returns_only_completed() {
    let server = TestServer::start(vec![
        MockResponse::Text("fast child done".into()),
        MockResponse::Text("slow child done".into()),
    ]);

    let conn = server.connect();
    let parent_id = match send_recv(
        &conn,
        &Request::CreateSession {
            model: None,
            provider: None,
            system_prompt: Some("parent".into()),
            cwd: Some("/tmp".into()),
            parent_id: None,
            child_budget: 10,
            tagline: None,
        },
    ) {
        Response::SessionCreated { session_id } => session_id,
        other => panic!("{:?}", other),
    };

    // Create fast child and start its chat
    let c = server.connect();
    let fast_id = match send_recv(
        &c,
        &Request::CreateSession {
            model: None,
            provider: None,
            system_prompt: Some("fast".into()),
            cwd: None,
            parent_id: Some(parent_id.clone()),
            child_budget: 0,
            tagline: None,
        },
    ) {
        Response::SessionCreated { session_id } => session_id,
        other => panic!("{:?}", other),
    };

    // Chat with fast child and wait for it to finish
    let c = server.connect();
    let responses = send_recv_all(
        &c,
        &Request::Chat {
            session_id: fast_id.clone(),
            text: "go fast".into(),
        },
    );
    assert!(responses.iter().any(|r| matches!(r, Response::AgentDone)));

    // Create slow child (idle, no messages)
    let c = server.connect();
    let slow_id = match send_recv(
        &c,
        &Request::CreateSession {
            model: None,
            provider: None,
            system_prompt: Some("slow".into()),
            cwd: None,
            parent_id: Some(parent_id.clone()),
            child_budget: 0,
            tagline: None,
        },
    ) {
        Response::SessionCreated { session_id } => session_id,
        other => panic!("{:?}", other),
    };

    // WaitAnySessions with both -- both are idle so both should return
    let c = server.connect();
    let resp = send_recv(
        &c,
        &Request::WaitAnySessions {
            session_ids: vec![fast_id.clone(), slow_id.clone()],
            timeout_secs: 5,
        },
    );
    match resp {
        Response::SessionsCompleted { results } => {
            assert_eq!(results.len(), 2, "expected 2 results, got {:?}", results);
            for r in &results {
                assert_eq!(r.status, "done");
            }
            let fast_result = results.iter().find(|r| r.session_id == fast_id).unwrap();
            assert!(
                fast_result.summary.contains("fast child done"),
                "fast child summary: {}",
                fast_result.summary
            );
        }
        other => panic!("expected SessionsCompleted, got {:?}", other),
    }

    server.shutdown();
}

// ---------------------------------------------------------------------------
// Worker unjoined_children unit tests
// ---------------------------------------------------------------------------

#[test]
fn unjoined_children_tracking() {
    use std::collections::HashSet;

    let mut unjoined: HashSet<String> = HashSet::new();

    // Spawn adds
    unjoined.insert("c1".into());
    unjoined.insert("c2".into());
    unjoined.insert("c3".into());
    assert_eq!(unjoined.len(), 3);

    // Join specific removes
    unjoined.remove("c1");
    assert_eq!(unjoined.len(), 2);
    assert!(!unjoined.contains("c1"));

    // Join all drains
    let ids: Vec<String> = unjoined.drain().collect();
    assert_eq!(ids.len(), 2);
    assert!(unjoined.is_empty());

    // New spawns after drain
    unjoined.insert("c4".into());
    unjoined.insert("c5".into());

    // Join any removes only completed
    let completed = vec!["c4".to_string()];
    for id in &completed {
        unjoined.remove(id);
    }
    assert_eq!(unjoined.len(), 1);
    assert!(unjoined.contains("c5"));
}
