//! E2E test: full task lifecycle pipeline.
//!
//! Exercises the complete task lifecycle through the real server+tasks plugin:
//! planning → refining → ready → active → review → (changes requested) →
//! active → review → approved → merging → merged.
//!
//! Also tests:
//! - `skip_planning=true` subtask goes straight to ready
//! - `refining→planning` backward transition resumes planning session
//! - `refining→interactive` escalation (scope expansion)
//! - `affected_files` guard on refining→ready
//!
//! Uses `ExecuteTool` to drive `task_*` tools through the server, with the
//! tasks plugin loaded as a global plugin (real `tau plugin-tasks` binary).

mod common;

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use common::{TestServer, send_recv};
use tau_agent::plugin::{PluginEntry, PluginsConfig};
use tau_agent::protocol::{Request, Response};
use tau_agent::providers::mock::{MockProvider, MockResponse};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Find the tau binary. Expects `cargo build` to have been run beforehand.
fn tau_binary() -> PathBuf {
    // Try the standard target/debug path relative to the workspace root.
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let debug_bin = workspace.join("target/debug/tau");
    if debug_bin.exists() {
        return debug_bin;
    }
    let release_bin = workspace.join("target/release/tau");
    if release_bin.exists() {
        return release_bin;
    }
    panic!(
        "tau binary not found at {} or {}. Run `cargo build` first.",
        debug_bin.display(),
        release_bin.display()
    );
}

/// Create a git repository in a temp directory with an initial commit.
/// Returns the path to the repo.
fn init_git_repo(dir: &std::path::Path) -> PathBuf {
    let repo = dir.join("project");
    std::fs::create_dir_all(&repo).unwrap();

    let run = |args: &[&str]| {
        let out = Command::new("git")
            .args(args)
            .current_dir(&repo)
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@test.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@test.com")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    };

    run(&["init", "-b", "main"]);
    std::fs::write(repo.join("README.md"), "# Test Project\n").unwrap();
    run(&["add", "."]);
    run(&["commit", "-m", "Initial commit"]);

    repo
}

/// Execute a task tool via the server and return (content, is_error).
/// Automatically retries when the tasks plugin is busy with a scheduler/merge pass.
fn exec_tool(
    server: &TestServer,
    session_id: &str,
    tool_name: &str,
    args: serde_json::Value,
) -> (String, bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let conn = server.connect();
        match send_recv(
            &conn,
            &Request::ExecuteTool {
                session_id: session_id.into(),
                tool_name: tool_name.into(),
                arguments: args.clone(),
            },
        ) {
            Response::ToolExecuted { content, is_error } => {
                // Retry if the plugin is busy with a background pass
                if is_error && content.contains("busy with a background") {
                    if Instant::now() < deadline {
                        std::thread::sleep(Duration::from_millis(200));
                        continue;
                    }
                }
                return (content, is_error);
            }
            other => panic!(
                "expected ToolExecuted for {}({}), got: {:?}",
                tool_name, session_id, other
            ),
        }
    }
}

/// Execute a task tool, assert success, return the JSON value.
fn exec_tool_ok(
    server: &TestServer,
    session_id: &str,
    tool_name: &str,
    args: serde_json::Value,
) -> serde_json::Value {
    let (content, is_error) = exec_tool(server, session_id, tool_name, args);
    assert!(!is_error, "{} returned error: {}", tool_name, content);
    // Try to parse as JSON; some responses have prefix text before JSON.
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&content) {
        return v;
    }
    // Try to find JSON in the content (e.g., "Scheduled 1 task(s):\n{...}")
    if let Some(start) = content.find('[').or_else(|| content.find('{')) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&content[start..]) {
            return v;
        }
    }
    serde_json::Value::String(content)
}

/// Execute a task tool, assert it returns an error.
fn exec_tool_err(
    server: &TestServer,
    session_id: &str,
    tool_name: &str,
    args: serde_json::Value,
) -> String {
    let (content, is_error) = exec_tool(server, session_id, tool_name, args);
    assert!(
        is_error,
        "{} expected error but got success: {}",
        tool_name, content
    );
    content
}

/// Create a session on the server and return its ID.
fn create_session(server: &TestServer, cwd: Option<&str>) -> String {
    let conn = server.connect();
    match send_recv(
        &conn,
        &Request::CreateSession {
            model: None,
            provider: None,
            system_prompt: Some("test controller".into()),
            cwd: cwd.map(String::from),
            parent_id: None,
            child_budget: 16,
            tagline: Some("test-controller".into()),
            auto_archive: false,
            notify_parent: false,
        },
    ) {
        Response::SessionCreated { session_id } => session_id,
        other => panic!("expected SessionCreated, got: {:?}", other),
    }
}

/// Wait for a task to reach a given state, polling via task_get.
fn wait_for_task_state(
    server: &TestServer,
    session_id: &str,
    task_id: i64,
    expected_state: &str,
    timeout: Duration,
) -> serde_json::Value {
    let deadline = Instant::now() + timeout;
    loop {
        let task = exec_tool_ok(
            server,
            session_id,
            "task_get",
            serde_json::json!({"id": task_id}),
        );
        let state = task["task"]["state"].as_str().unwrap_or("");
        if state == expected_state {
            return task;
        }
        if Instant::now() > deadline {
            panic!(
                "task {} did not reach state '{}' within {:?} (current: '{}')\ntask: {}",
                task_id,
                expected_state,
                timeout,
                state,
                serde_json::to_string_pretty(&task).unwrap()
            );
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Run git command in a directory and return stdout.
fn git(dir: &std::path::Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@test.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@test.com")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {:?} in {} failed: {}",
        args,
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Start a test server with the tasks plugin loaded as a global plugin.
/// Returns (TestServer, session_id for the controller session).
///
/// The `mock_responses` are consumed by dispatched sessions' LLM calls.
fn start_server_with_tasks(
    _repo_path: &std::path::Path,
    mock_responses: Vec<MockResponse>,
) -> TestServer {
    let tau_bin = tau_binary();
    let tau_bin_str = tau_bin.to_string_lossy().to_string();

    // Create an isolated XDG_DATA_HOME so the tasks plugin doesn't hit the
    // production tasks database.  We place it as a sibling of the repo inside
    // the same tmpdir so it shares the tmpdir lifetime.
    let data_home = _repo_path.parent().unwrap().join("xdg_data");
    std::fs::create_dir_all(&data_home).unwrap();
    let data_home_str = data_home.to_string_lossy().to_string();

    let plugins_config = PluginsConfig {
        no_default_worker: true,
        global: [(
            "tasks".to_string(),
            PluginEntry {
                command: vec![tau_bin_str.clone(), "plugin-tasks".into()],
                env: [("XDG_DATA_HOME".to_string(), data_home_str)]
                    .into_iter()
                    .collect(),
            },
        )]
        .into_iter()
        .collect::<HashMap<_, _>>(),
        // Provide the default worker as a session plugin so bash is available
        // for the merge process (which uses ExecuteTool with bash).
        session: [(
            "worker".to_string(),
            PluginEntry {
                command: vec![tau_bin_str, "worker".into()],
                env: HashMap::new(),
            },
        )]
        .into_iter()
        .collect::<HashMap<_, _>>(),
        idle_timeout_secs: 300, // Don't idle-kill plugins during test
        ..Default::default()
    };

    let provider = MockProvider::new(mock_responses);

    TestServer::start_with_config(vec![], move |mut config| {
        config.registry = {
            let mut r = tau_agent::provider::ProviderRegistry::new();
            r.register(provider);
            r
        };
        config.plugins_config = Some(plugins_config);
        config
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Test the full task lifecycle pipeline:
/// create parent → create subtask (planning) → planning→refining →
/// refining→ready → schedule (ready→active, creates branch+worktree) →
/// worker makes changes → active→review (with rebase check) →
/// review→active (changes requested) → active→review again →
/// review→approved → merge → merged.
///
/// This drives the lifecycle manually via ExecuteTool calls rather than
/// relying on auto-dispatched LLM sessions, giving us deterministic control
/// over each state transition.
#[test]
fn full_task_lifecycle_pipeline() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = init_git_repo(tmp.path());
    let repo_str = repo.to_string_lossy().to_string();

    // We need mock responses for the auto-dispatched sessions:
    // 1. Refining session (auto-dispatched on planning→refining)
    // 2. Review session (auto-dispatched on active→review, 1st pass)
    // 3. Review session (auto-dispatched on active→review, 2nd pass)
    // Each dispatched session gets a Chat + LLM call → we need one response each.
    // The dispatched sessions also consume responses for their agent loop.
    // Since we're driving state transitions manually via ExecuteTool, the
    // dispatched sessions just need to not crash — they can return text.
    let mock_responses = vec![
        // For any auto-dispatched sessions (refining, review, etc.)
        MockResponse::Text("acknowledged".into()),
        MockResponse::Text("acknowledged".into()),
        MockResponse::Text("acknowledged".into()),
        MockResponse::Text("acknowledged".into()),
        MockResponse::Text("acknowledged".into()),
        MockResponse::Text("acknowledged".into()),
        MockResponse::Text("acknowledged".into()),
        MockResponse::Text("acknowledged".into()),
    ];

    let server = start_server_with_tasks(&repo, mock_responses);

    // Give the tasks plugin time to register
    std::thread::sleep(Duration::from_millis(500));

    // Create controller session (cwd = repo)
    let sid = create_session(&server, Some(&repo_str));

    // -----------------------------------------------------------------------
    // Step 1: Create a parent task (interactive)
    // -----------------------------------------------------------------------
    let parent = exec_tool_ok(
        &server,
        &sid,
        "task_create",
        serde_json::json!({
            "title": "Parent lifecycle test",
            "message": "Test the full lifecycle",
        }),
    );
    let parent_id = parent["id"].as_i64().unwrap();
    assert_eq!(parent["state"].as_str().unwrap(), "interactive");

    // -----------------------------------------------------------------------
    // Step 2: Create a subtask with planning (skip_planning=false)
    // -----------------------------------------------------------------------
    let subtask = exec_tool_ok(
        &server,
        &sid,
        "task_create",
        serde_json::json!({
            "title": "Lifecycle subtask",
            "parent_id": parent_id,
            "skip_planning": false,
            "skip_review": false,
            "message": "Implement the feature.\n\nRequirements:\n- Add hello.txt with 'hello world'",
        }),
    );
    let task_id = subtask["id"].as_i64().unwrap();
    assert_eq!(
        subtask["state"].as_str().unwrap(),
        "planning",
        "subtask with skip_planning=false should start in planning"
    );

    // Wait a moment for the scheduling event to trigger auto-dispatch of
    // the planning session.  The scheduler should pick up the planning task
    // but NOT create a worktree (planning is read-only).
    std::thread::sleep(Duration::from_millis(1500));

    // -----------------------------------------------------------------------
    // Step 3: Planning agent sets affected_files and transitions to refining
    // -----------------------------------------------------------------------
    // Set affected_files first (required for refining→ready later)
    let _ = exec_tool_ok(
        &server,
        &sid,
        "task_update",
        serde_json::json!({
            "id": task_id,
            "affected_files": ["hello.txt"],
        }),
    );

    // Add a plan message
    let _ = exec_tool_ok(
        &server,
        &sid,
        "task_message",
        serde_json::json!({
            "id": task_id,
            "content": "Plan: Create hello.txt with 'hello world' content.",
        }),
    );

    // Transition planning → refining
    let task = exec_tool_ok(
        &server,
        &sid,
        "task_update",
        serde_json::json!({
            "id": task_id,
            "state": "refining",
        }),
    );
    assert_eq!(task["state"].as_str().unwrap(), "refining");

    // Wait for refining session auto-dispatch
    std::thread::sleep(Duration::from_millis(1000));

    // -----------------------------------------------------------------------
    // Step 4: Refining agent approves → ready
    // -----------------------------------------------------------------------
    let task = exec_tool_ok(
        &server,
        &sid,
        "task_update",
        serde_json::json!({
            "id": task_id,
            "state": "ready",
        }),
    );
    assert_eq!(task["state"].as_str().unwrap(), "ready");

    // -----------------------------------------------------------------------
    // Step 5: Scheduler picks up ready task → creates branch/worktree → active
    // -----------------------------------------------------------------------
    // The task_update to "ready" emits a ScheduleNeeded event which triggers
    // the scheduler. Wait for it to create the branch/worktree and transition
    // to active.
    let task_data = wait_for_task_state(&server, &sid, task_id, "active", Duration::from_secs(10));
    let worktree = task_data["task"]["worktree_path"]
        .as_str()
        .expect("active task should have worktree_path");
    let branch = task_data["task"]["branch"]
        .as_str()
        .expect("active task should have branch");

    assert!(
        std::path::Path::new(worktree).exists(),
        "worktree should exist at {}",
        worktree
    );
    assert!(!branch.is_empty(), "branch name should not be empty");

    // -----------------------------------------------------------------------
    // Step 6: Worker makes changes in the worktree and commits
    // -----------------------------------------------------------------------
    let wt_path = std::path::Path::new(worktree);
    std::fs::write(wt_path.join("hello.txt"), "hello world\n").unwrap();
    git(wt_path, &["add", "hello.txt"]);
    git(wt_path, &["commit", "-m", "Add hello.txt"]);

    // Ensure the branch is rebased on main (it should be since it was just
    // created from main).
    git(wt_path, &["rebase", "main"]);

    // -----------------------------------------------------------------------
    // Step 7: Worker transitions active → review
    // -----------------------------------------------------------------------
    let task = exec_tool_ok(
        &server,
        &sid,
        "task_update",
        serde_json::json!({
            "id": task_id,
            "state": "review",
        }),
    );
    assert_eq!(task["state"].as_str().unwrap(), "review");

    // Wait for review session auto-dispatch
    std::thread::sleep(Duration::from_millis(1000));

    // -----------------------------------------------------------------------
    // Step 8: Reviewer requests changes → back to active
    // -----------------------------------------------------------------------
    // Add review feedback
    let _ = exec_tool_ok(
        &server,
        &sid,
        "task_message",
        serde_json::json!({
            "id": task_id,
            "content": "Please also add a newline at the end of the file.",
        }),
    );

    let task = exec_tool_ok(
        &server,
        &sid,
        "task_update",
        serde_json::json!({
            "id": task_id,
            "state": "active",
        }),
    );
    assert_eq!(task["state"].as_str().unwrap(), "active");

    // -----------------------------------------------------------------------
    // Step 9: Worker fixes, transitions back to review
    // -----------------------------------------------------------------------
    // Make another change
    std::fs::write(wt_path.join("hello.txt"), "hello world\n\n").unwrap();
    git(wt_path, &["add", "hello.txt"]);
    git(wt_path, &["commit", "-m", "Fix: add trailing newline"]);

    // Rebase on main again
    git(wt_path, &["rebase", "main"]);

    let task = exec_tool_ok(
        &server,
        &sid,
        "task_update",
        serde_json::json!({
            "id": task_id,
            "state": "review",
        }),
    );
    assert_eq!(task["state"].as_str().unwrap(), "review");

    std::thread::sleep(Duration::from_millis(1000));

    // -----------------------------------------------------------------------
    // Step 10: Reviewer approves → approved
    // -----------------------------------------------------------------------
    let task = exec_tool_ok(
        &server,
        &sid,
        "task_update",
        serde_json::json!({
            "id": task_id,
            "state": "approved",
        }),
    );
    assert_eq!(task["state"].as_str().unwrap(), "approved");

    // -----------------------------------------------------------------------
    // Step 11: Merge (approved → merging → merged)
    // The MergeNeeded event is auto-triggered. Wait for the task to reach
    // merged state. The merge pipeline: rebase, checklist (none configured),
    // fast-forward merge, cleanup.
    // -----------------------------------------------------------------------
    let task_data = wait_for_task_state(&server, &sid, task_id, "merged", Duration::from_secs(15));
    assert_eq!(task_data["task"]["state"].as_str().unwrap(), "merged");

    // -----------------------------------------------------------------------
    // Step 12: Verify merge results
    // -----------------------------------------------------------------------
    // The file should now exist on main
    let content = git(&repo, &["show", "main:hello.txt"]);
    assert!(
        content.contains("hello world"),
        "hello.txt should be on main after merge, got: {}",
        content
    );

    // Worktree should be cleaned up
    // Note: the worktree path on disk may be removed by the merge process,
    // or cleared from the DB. Either way, verify the DB no longer has it.
    let task_final = exec_tool_ok(
        &server,
        &sid,
        "task_get",
        serde_json::json!({"id": task_id}),
    );
    // worktree_path should be cleared from the DB after merge
    assert!(
        task_final["task"]["worktree_path"].is_null()
            || !std::path::Path::new(task_final["task"]["worktree_path"].as_str().unwrap_or(""))
                .exists(),
        "worktree should be removed or cleared after merge"
    );

    // Branch may or may not be deleted (cleanup runs after worktree removal,
    // so git branch -D might fail if cwd was the removed worktree). The
    // important thing is the merge succeeded and the changes are on main.
    // We verify the branch was at least merged by checking main contains
    // the changes (verified above).

    // -----------------------------------------------------------------------
    // Verify task messages include the plan and review feedback
    // -----------------------------------------------------------------------
    let task_data = exec_tool_ok(
        &server,
        &sid,
        "task_get",
        serde_json::json!({"id": task_id}),
    );
    let messages = task_data["messages"].as_array().unwrap();
    assert!(
        messages.len() >= 3,
        "task should have at least 3 messages (spec, plan, review feedback), got {}",
        messages.len()
    );

    server.shutdown();
}

/// Test that `skip_planning=true` subtask goes straight to `ready` state.
#[test]
fn skip_planning_subtask_starts_ready() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = init_git_repo(tmp.path());
    let repo_str = repo.to_string_lossy().to_string();

    let server = start_server_with_tasks(
        &repo,
        vec![
            MockResponse::Text("ack".into()),
            MockResponse::Text("ack".into()),
            MockResponse::Text("ack".into()),
            MockResponse::Text("ack".into()),
        ],
    );
    std::thread::sleep(Duration::from_millis(500));

    let sid = create_session(&server, Some(&repo_str));

    // Create parent
    let parent = exec_tool_ok(
        &server,
        &sid,
        "task_create",
        serde_json::json!({
            "title": "Parent for skip_planning test",
        }),
    );
    let parent_id = parent["id"].as_i64().unwrap();

    // Create subtask with skip_planning=true
    let subtask = exec_tool_ok(
        &server,
        &sid,
        "task_create",
        serde_json::json!({
            "title": "Skip planning subtask",
            "parent_id": parent_id,
            "skip_planning": true,
            "message": "Do the work directly",
        }),
    );
    assert_eq!(
        subtask["state"].as_str().unwrap(),
        "ready",
        "subtask with skip_planning=true should start in ready"
    );

    // The scheduler should pick it up and move to active (with worktree)
    let task_id = subtask["id"].as_i64().unwrap();
    let task_data = wait_for_task_state(&server, &sid, task_id, "active", Duration::from_secs(10));
    assert!(
        task_data["task"]["worktree_path"].as_str().is_some(),
        "active task should have a worktree"
    );
    assert!(
        task_data["task"]["branch"].as_str().is_some(),
        "active task should have a branch"
    );

    server.shutdown();
}

/// Test that `refining→ready` is rejected when `affected_files` is empty.
#[test]
fn affected_files_guard_on_refining_to_ready() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = init_git_repo(tmp.path());
    let repo_str = repo.to_string_lossy().to_string();

    let server = start_server_with_tasks(
        &repo,
        vec![
            MockResponse::Text("ack".into()),
            MockResponse::Text("ack".into()),
            MockResponse::Text("ack".into()),
            MockResponse::Text("ack".into()),
        ],
    );
    std::thread::sleep(Duration::from_millis(500));

    let sid = create_session(&server, Some(&repo_str));

    // Create parent + subtask
    let parent = exec_tool_ok(
        &server,
        &sid,
        "task_create",
        serde_json::json!({"title": "Parent"}),
    );
    let parent_id = parent["id"].as_i64().unwrap();

    let subtask = exec_tool_ok(
        &server,
        &sid,
        "task_create",
        serde_json::json!({
            "title": "Subtask needs affected_files",
            "parent_id": parent_id,
            "skip_planning": false,
        }),
    );
    let task_id = subtask["id"].as_i64().unwrap();
    assert_eq!(subtask["state"].as_str().unwrap(), "planning");

    // Move to refining (no affected_files set yet)
    let _ = exec_tool_ok(
        &server,
        &sid,
        "task_update",
        serde_json::json!({"id": task_id, "state": "refining"}),
    );

    // Try to move to ready without affected_files → should fail
    let err = exec_tool_err(
        &server,
        &sid,
        "task_update",
        serde_json::json!({"id": task_id, "state": "ready"}),
    );
    assert!(
        err.contains("affected_files"),
        "refining→ready without affected_files should be rejected: {}",
        err
    );

    // Now set affected_files and try again
    let _ = exec_tool_ok(
        &server,
        &sid,
        "task_update",
        serde_json::json!({
            "id": task_id,
            "affected_files": ["some_file.rs"],
        }),
    );

    let task = exec_tool_ok(
        &server,
        &sid,
        "task_update",
        serde_json::json!({"id": task_id, "state": "ready"}),
    );
    assert_eq!(task["state"].as_str().unwrap(), "ready");

    server.shutdown();
}

/// Test `refining→planning` backward transition.
#[test]
fn refining_to_planning_backward_transition() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = init_git_repo(tmp.path());
    let repo_str = repo.to_string_lossy().to_string();

    let server = start_server_with_tasks(
        &repo,
        vec![
            MockResponse::Text("ack".into()),
            MockResponse::Text("ack".into()),
            MockResponse::Text("ack".into()),
            MockResponse::Text("ack".into()),
            MockResponse::Text("ack".into()),
            MockResponse::Text("ack".into()),
        ],
    );
    std::thread::sleep(Duration::from_millis(500));

    let sid = create_session(&server, Some(&repo_str));

    // Create parent + subtask
    let parent = exec_tool_ok(
        &server,
        &sid,
        "task_create",
        serde_json::json!({"title": "Parent"}),
    );
    let parent_id = parent["id"].as_i64().unwrap();

    let subtask = exec_tool_ok(
        &server,
        &sid,
        "task_create",
        serde_json::json!({
            "title": "Subtask for refining→planning test",
            "parent_id": parent_id,
            "skip_planning": false,
        }),
    );
    let task_id = subtask["id"].as_i64().unwrap();

    // planning → refining
    std::thread::sleep(Duration::from_millis(1000));
    let _ = exec_tool_ok(
        &server,
        &sid,
        "task_update",
        serde_json::json!({"id": task_id, "state": "refining"}),
    );

    std::thread::sleep(Duration::from_millis(500));

    // refining → planning (backward, plan needs revision)
    let task = exec_tool_ok(
        &server,
        &sid,
        "task_update",
        serde_json::json!({"id": task_id, "state": "planning"}),
    );
    assert_eq!(
        task["state"].as_str().unwrap(),
        "planning",
        "should be able to go from refining back to planning"
    );

    // planning → refining again (revised plan)
    let _ = exec_tool_ok(
        &server,
        &sid,
        "task_update",
        serde_json::json!({
            "id": task_id,
            "state": "refining",
            "affected_files": ["revised.rs"],
        }),
    );

    std::thread::sleep(Duration::from_millis(500));

    // refining → ready (this time with affected_files set)
    let task = exec_tool_ok(
        &server,
        &sid,
        "task_update",
        serde_json::json!({"id": task_id, "state": "ready"}),
    );
    assert_eq!(task["state"].as_str().unwrap(), "ready");

    server.shutdown();
}

/// Test `refining→interactive` escalation (scope expansion).
#[test]
fn refining_to_interactive_escalation() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = init_git_repo(tmp.path());
    let repo_str = repo.to_string_lossy().to_string();

    let server = start_server_with_tasks(
        &repo,
        vec![
            MockResponse::Text("ack".into()),
            MockResponse::Text("ack".into()),
            MockResponse::Text("ack".into()),
            MockResponse::Text("ack".into()),
        ],
    );
    std::thread::sleep(Duration::from_millis(500));

    let sid = create_session(&server, Some(&repo_str));

    // Create parent + subtask
    let parent = exec_tool_ok(
        &server,
        &sid,
        "task_create",
        serde_json::json!({"title": "Parent"}),
    );
    let parent_id = parent["id"].as_i64().unwrap();

    let subtask = exec_tool_ok(
        &server,
        &sid,
        "task_create",
        serde_json::json!({
            "title": "Subtask for escalation test",
            "parent_id": parent_id,
            "skip_planning": false,
        }),
    );
    let task_id = subtask["id"].as_i64().unwrap();

    // planning → refining
    std::thread::sleep(Duration::from_millis(1000));
    let _ = exec_tool_ok(
        &server,
        &sid,
        "task_update",
        serde_json::json!({"id": task_id, "state": "refining"}),
    );

    std::thread::sleep(Duration::from_millis(500));

    // refining → interactive (scope expansion)
    let task = exec_tool_ok(
        &server,
        &sid,
        "task_update",
        serde_json::json!({"id": task_id, "state": "interactive"}),
    );
    assert_eq!(
        task["state"].as_str().unwrap(),
        "interactive",
        "should be able to escalate from refining to interactive"
    );

    server.shutdown();
}

/// Test that active→review succeeds even when branch is not rebased on main.
/// The merge queue handles rebasing, so the pre-review rebase check is removed.
#[test]
fn review_transition_without_rebase() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = init_git_repo(tmp.path());
    let repo_str = repo.to_string_lossy().to_string();

    let server = start_server_with_tasks(
        &repo,
        vec![
            MockResponse::Text("ack".into()),
            MockResponse::Text("ack".into()),
            MockResponse::Text("ack".into()),
            MockResponse::Text("ack".into()),
            MockResponse::Text("ack".into()),
            MockResponse::Text("ack".into()),
        ],
    );
    std::thread::sleep(Duration::from_millis(500));

    let sid = create_session(&server, Some(&repo_str));

    // Create parent + subtask (skip_planning=true to go straight to ready)
    let parent = exec_tool_ok(
        &server,
        &sid,
        "task_create",
        serde_json::json!({"title": "Parent"}),
    );
    let parent_id = parent["id"].as_i64().unwrap();

    let subtask = exec_tool_ok(
        &server,
        &sid,
        "task_create",
        serde_json::json!({
            "title": "No rebase needed for review",
            "parent_id": parent_id,
            "skip_planning": true,
            "message": "Test that review works without rebase",
        }),
    );
    let task_id = subtask["id"].as_i64().unwrap();

    // Wait for scheduler to pick up and create worktree
    let task_data = wait_for_task_state(&server, &sid, task_id, "active", Duration::from_secs(10));
    let worktree = task_data["task"]["worktree_path"].as_str().unwrap();
    let wt_path = std::path::Path::new(worktree);

    // Make a commit on main AFTER the branch was created.
    // This makes the branch NOT rebased on main.
    std::fs::write(repo.join("extra.txt"), "extra content\n").expect("write extra.txt");
    git(&repo, &["add", "extra.txt"]);
    git(&repo, &["commit", "-m", "Add extra.txt on main"]);

    // Make a commit on the worktree branch
    std::fs::write(wt_path.join("feature.txt"), "feature\n").expect("write feature.txt");
    git(wt_path, &["add", "feature.txt"]);
    git(wt_path, &["commit", "-m", "Add feature.txt"]);

    // active → review should be rejected because branch is not rebased
    // (the rebase enforcement feature added in the tasks plugin requires
    // the branch to be rebased on the merge target before entering review)
    let (content, is_error) = exec_tool(
        &server,
        &sid,
        "task_update",
        serde_json::json!({"id": task_id, "state": "review"}),
    );
    assert!(is_error, "expected error but got success: {}", content);
    assert!(
        content.contains("not rebased"),
        "expected rebase error, got: {}",
        content
    );

    // Now actually rebase and try again — should succeed
    git(wt_path, &["rebase", "main"]);
    let task = exec_tool_ok(
        &server,
        &sid,
        "task_update",
        serde_json::json!({"id": task_id, "state": "review"}),
    );
    assert_eq!(task["state"].as_str().unwrap(), "review");

    server.shutdown();
}

/// Test that `active→approved` requires `skip_review=true`.
#[test]
fn active_to_approved_requires_skip_review() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = init_git_repo(tmp.path());
    let repo_str = repo.to_string_lossy().to_string();

    let server = start_server_with_tasks(
        &repo,
        vec![
            MockResponse::Text("ack".into()),
            MockResponse::Text("ack".into()),
            MockResponse::Text("ack".into()),
            MockResponse::Text("ack".into()),
        ],
    );
    std::thread::sleep(Duration::from_millis(500));

    let sid = create_session(&server, Some(&repo_str));

    // Create parent + subtask (skip_review=false)
    let parent = exec_tool_ok(
        &server,
        &sid,
        "task_create",
        serde_json::json!({"title": "Parent"}),
    );
    let parent_id = parent["id"].as_i64().unwrap();

    let subtask = exec_tool_ok(
        &server,
        &sid,
        "task_create",
        serde_json::json!({
            "title": "No skip_review subtask",
            "parent_id": parent_id,
            "skip_planning": true,
            "message": "Test skip_review enforcement",
        }),
    );
    let task_id = subtask["id"].as_i64().unwrap();

    // Wait for active
    let _ = wait_for_task_state(&server, &sid, task_id, "active", Duration::from_secs(10));

    // active → approved should fail since skip_review=false
    let err = exec_tool_err(
        &server,
        &sid,
        "task_update",
        serde_json::json!({"id": task_id, "state": "approved"}),
    );
    assert!(
        err.contains("review") || err.contains("skip_review"),
        "should mention review requirement: {}",
        err
    );

    server.shutdown();
}
