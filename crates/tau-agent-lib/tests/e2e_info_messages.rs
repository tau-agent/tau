//! Tests that externally-driven session mutations append persistent
//! [`Message::Info`] entries so the session's on-disk transcript records
//! them with optional caller attribution.

mod common;

use common::{TestServer, send_recv};
use tau_agent_lib::protocol::{Request, Response};
use tau_agent_lib::types::Message;

/// Extract the info-message texts from a message list in order.
fn info_texts(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .filter_map(|m| match m {
            Message::Info(i) => Some(i.text.clone()),
            _ => None,
        })
        .collect()
}

fn create_session(server: &TestServer) -> String {
    let conn = server.connect();
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
            auto_archive: false,
            notify_parent: true,
            project_name: None,
            sandbox_profile: None,
        },
    );
    match resp {
        Response::SessionCreated { session_id } => session_id,
        other => panic!("expected SessionCreated, got {:?}", other),
    }
}

fn create_child(server: &TestServer, parent: &str) -> String {
    let conn = server.connect();
    let resp = send_recv(
        &conn,
        &Request::CreateSession {
            model: None,
            provider: None,
            system_prompt: None,
            cwd: Some("/tmp".into()),
            parent_id: Some(parent.into()),
            child_budget: 1,
            tagline: None,
            auto_archive: false,
            notify_parent: true,
            project_name: None,
            sandbox_profile: None,
        },
    );
    match resp {
        Response::SessionCreated { session_id } => session_id,
        other => panic!("expected SessionCreated, got {:?}", other),
    }
}

fn get_messages(server: &TestServer, sid: &str) -> Vec<Message> {
    let conn = server.connect();
    let resp = send_recv(
        &conn,
        &Request::GetMessages {
            session_id: sid.into(),
        },
    );
    match resp {
        Response::Messages { messages } => messages,
        other => panic!("expected Messages, got {:?}", other),
    }
}

// ---------------------------------------------------------------------------
// CancelChat
// ---------------------------------------------------------------------------

#[test]
fn cancel_idle_session_without_caller_records_plain_info() {
    let server = TestServer::start(vec![]);
    let sid = create_session(&server);

    let conn = server.connect();
    let resp = send_recv(
        &conn,
        &Request::CancelChat {
            session_id: sid.clone(),
            caller_session_id: None,
        },
    );
    assert!(matches!(resp, Response::Ok));

    let infos = info_texts(&get_messages(&server, &sid));
    assert_eq!(infos, vec!["Session cancelled. (was idle)".to_string()]);

    server.shutdown();
}

#[test]
fn cancel_idle_session_with_caller_records_attributed_info() {
    let server = TestServer::start(vec![]);
    let sid = create_session(&server);

    let conn = server.connect();
    let resp = send_recv(
        &conn,
        &Request::CancelChat {
            session_id: sid.clone(),
            caller_session_id: Some("s42".into()),
        },
    );
    assert!(matches!(resp, Response::Ok));

    let infos = info_texts(&get_messages(&server, &sid));
    assert_eq!(
        infos,
        vec!["Session cancelled by s42. (was idle)".to_string()]
    );

    server.shutdown();
}

// ---------------------------------------------------------------------------
// SetCwd
// ---------------------------------------------------------------------------

#[test]
fn set_cwd_without_caller_records_plain_info() {
    let server = TestServer::start(vec![]);
    let sid = create_session(&server);

    let conn = server.connect();
    let resp = send_recv(
        &conn,
        &Request::SetCwd {
            session_id: sid.clone(),
            cwd: "/var/tmp".into(),
            caller_session_id: None,
        },
    );
    assert!(matches!(resp, Response::Ok));

    let infos = info_texts(&get_messages(&server, &sid));
    assert_eq!(infos, vec!["cwd changed to /var/tmp.".to_string()]);

    server.shutdown();
}

#[test]
fn set_cwd_with_caller_records_attributed_info() {
    let server = TestServer::start(vec![]);
    let sid = create_session(&server);

    let conn = server.connect();
    let resp = send_recv(
        &conn,
        &Request::SetCwd {
            session_id: sid.clone(),
            cwd: "/var/tmp".into(),
            caller_session_id: Some("s42".into()),
        },
    );
    assert!(matches!(resp, Response::Ok));

    let infos = info_texts(&get_messages(&server, &sid));
    assert_eq!(infos, vec!["cwd changed by s42 to /var/tmp.".to_string()]);

    server.shutdown();
}

// ---------------------------------------------------------------------------
// SetModel
// ---------------------------------------------------------------------------

#[test]
fn set_model_without_caller_records_plain_info() {
    let server = TestServer::start(vec![]);
    let sid = create_session(&server);

    let conn = server.connect();
    // The test server registers a single mock model; use its id to change to
    // the same model. The info message reports the model name + provider.
    let mock_id = tau_agent_lib::providers::mock::mock_model().id.clone();
    let resp = send_recv(
        &conn,
        &Request::SetModel {
            session_id: sid.clone(),
            model_id: mock_id.clone(),
            caller_session_id: None,
        },
    );
    let model = match resp {
        Response::ModelChanged { model } => model,
        other => panic!("expected ModelChanged, got {:?}", other),
    };

    let expected = format!("Model changed to {} ({}).", model.name, model.provider);
    let infos = info_texts(&get_messages(&server, &sid));
    assert_eq!(infos, vec![expected]);

    server.shutdown();
}

#[test]
fn set_model_with_caller_records_attributed_info() {
    let server = TestServer::start(vec![]);
    let sid = create_session(&server);

    let conn = server.connect();
    let mock_id = tau_agent_lib::providers::mock::mock_model().id.clone();
    let resp = send_recv(
        &conn,
        &Request::SetModel {
            session_id: sid.clone(),
            model_id: mock_id.clone(),
            caller_session_id: Some("s42".into()),
        },
    );
    let model = match resp {
        Response::ModelChanged { model } => model,
        other => panic!("expected ModelChanged, got {:?}", other),
    };

    let expected = format!(
        "Model changed by s42 to {} ({}).",
        model.name, model.provider
    );
    let infos = info_texts(&get_messages(&server, &sid));
    assert_eq!(infos, vec![expected]);

    server.shutdown();
}

// ---------------------------------------------------------------------------
// ReparentChildren — only the children get the info message, not the parents.
// ---------------------------------------------------------------------------

#[test]
fn reparent_children_annotates_each_child_only() {
    let server = TestServer::start(vec![]);
    let old_parent = create_session(&server);
    let new_parent = create_session(&server);
    let c1 = create_child(&server, &old_parent);
    let c2 = create_child(&server, &old_parent);
    let c3 = create_child(&server, &old_parent);

    let conn = server.connect();
    let resp = send_recv(
        &conn,
        &Request::ReparentChildren {
            old_parent_id: old_parent.clone(),
            new_parent_id: new_parent.clone(),
        },
    );
    assert!(matches!(resp, Response::Ok));

    let expected = format!("Parent session changed from {old_parent} to {new_parent}.");
    for child in [&c1, &c2, &c3] {
        let infos = info_texts(&get_messages(&server, child));
        assert_eq!(
            infos,
            vec![expected.clone()],
            "child {child} should have exactly the reparent info line"
        );
    }

    // Old and new parents must not have received the info line.
    for p in [&old_parent, &new_parent] {
        let infos = info_texts(&get_messages(&server, p));
        assert!(
            infos.is_empty(),
            "parent {p} should have no reparent info messages, got {:?}",
            infos
        );
    }

    server.shutdown();
}

// ---------------------------------------------------------------------------
// ReloadPlugins — success line comes first, then any per-plugin failure
// lines. Without any configured plugins there are no failures, so we just
// assert the success line is present.
// ---------------------------------------------------------------------------

#[test]
fn reload_plugins_emits_success_info() {
    let server = TestServer::start(vec![]);
    let sid = create_session(&server);

    let conn = server.connect();
    let resp = send_recv(
        &conn,
        &Request::ReloadPlugins {
            session_id: sid.clone(),
        },
    );
    assert!(matches!(resp, Response::Ok));

    let infos = info_texts(&get_messages(&server, &sid));
    assert!(
        infos.iter().any(|t| t == "Plugins reloaded."),
        "expected 'Plugins reloaded.' in info messages, got {:?}",
        infos
    );
    // The success line should be first among info messages so readers see
    // "reload happened; here's what broke" in order.
    assert_eq!(infos.first().map(String::as_str), Some("Plugins reloaded."));

    server.shutdown();
}
