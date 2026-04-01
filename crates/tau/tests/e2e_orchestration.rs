//! End-to-end tests for tau server.
//!
//! These tests start a real server (with mock LLM provider), connect via
//! unix socket, and exercise the full protocol.

use tau::protocol::{Request, Response};

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
    assert_eq!(tools.len(), 6);

    let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
    assert!(names.contains(&"session_spawn"));
    assert!(names.contains(&"session_join"));
    assert!(names.contains(&"session_status"));
    assert!(names.contains(&"session_list_children"));
    assert!(names.contains(&"session_read"));
    assert!(names.contains(&"session_cancel"));

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
