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
use tau_agent_lib::plugin::{PluginEntry, PluginsConfig};
use tau_agent_lib::protocol::{Request, Response};
use tau_agent_lib::providers::mock::{MockProvider, MockResponse};

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

    // Create .tau/project.toml so the tasks plugin can discover the project
    // name from cwd (fallback when project_name is not in the protocol).
    let tau_dir = repo.join(".tau");
    std::fs::create_dir_all(&tau_dir).unwrap();
    std::fs::write(tau_dir.join("project.toml"), "name = \"e2e-test\"\n").unwrap();

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
fn create_session(server: &TestServer, cwd: Option<&str>, project_name: Option<&str>) -> String {
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
            project_name: project_name.map(String::from),
            sandbox_profile: None,
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

    // Create tau.db with a projects table so the tasks plugin can resolve
    // project_name → path. The tasks plugin opens tau.db read-only at startup.
    let tau_data_dir = data_home.join("tau");
    std::fs::create_dir_all(&tau_data_dir).unwrap();
    {
        let tau_db_path = tau_data_dir.join("tau.db");
        let conn = rusqlite::Connection::open(&tau_db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS projects (
                name TEXT PRIMARY KEY,
                path TEXT NOT NULL,
                created_at INTEGER NOT NULL DEFAULT 0,
                updated_at INTEGER NOT NULL DEFAULT 0
            )",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO projects (name, path) VALUES (?1, ?2)",
            rusqlite::params!["e2e-test", _repo_path.to_string_lossy().to_string()],
        )
        .unwrap();
    }

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
            let mut r = tau_agent_lib::provider::ProviderRegistry::new();
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
    let sid = create_session(&server, Some(&repo_str), Some("e2e-test"));

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
            "initial_state": "interactive",
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
            "initial_state": "planning",
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

    let sid = create_session(&server, Some(&repo_str), Some("e2e-test"));

    // Create parent
    let parent = exec_tool_ok(
        &server,
        &sid,
        "task_create",
        serde_json::json!({
            "title": "Parent for skip_planning test",
            "initial_state": "interactive",
        }),
    );
    let parent_id = parent["id"].as_i64().unwrap();

    // Create subtask with initial_state=ready. Pass affected_files so
    // the auto-downgrade (task #596) doesn't kick in — we want to test
    // the ready-state subtask path here.
    let subtask = exec_tool_ok(
        &server,
        &sid,
        "task_create",
        serde_json::json!({
            "title": "Skip planning subtask",
            "parent_id": parent_id,
            "initial_state": "ready",
            "affected_files": ["src/skip_planning_subtask.rs"],
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

    let sid = create_session(&server, Some(&repo_str), Some("e2e-test"));

    // Create parent + subtask
    let parent = exec_tool_ok(
        &server,
        &sid,
        "task_create",
        serde_json::json!({"title": "Parent", "initial_state": "interactive"}),
    );
    let parent_id = parent["id"].as_i64().unwrap();

    let subtask = exec_tool_ok(
        &server,
        &sid,
        "task_create",
        serde_json::json!({
            "title": "Subtask needs affected_files",
            "parent_id": parent_id,
            "initial_state": "planning",
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

    let sid = create_session(&server, Some(&repo_str), Some("e2e-test"));

    // Create parent + subtask
    let parent = exec_tool_ok(
        &server,
        &sid,
        "task_create",
        serde_json::json!({"title": "Parent", "initial_state": "interactive"}),
    );
    let parent_id = parent["id"].as_i64().unwrap();

    let subtask = exec_tool_ok(
        &server,
        &sid,
        "task_create",
        serde_json::json!({
            "title": "Subtask for refining→planning test",
            "parent_id": parent_id,
            "initial_state": "planning",
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

    let sid = create_session(&server, Some(&repo_str), Some("e2e-test"));

    // Create parent + subtask
    let parent = exec_tool_ok(
        &server,
        &sid,
        "task_create",
        serde_json::json!({"title": "Parent", "initial_state": "interactive"}),
    );
    let parent_id = parent["id"].as_i64().unwrap();

    let subtask = exec_tool_ok(
        &server,
        &sid,
        "task_create",
        serde_json::json!({
            "title": "Subtask for escalation test",
            "parent_id": parent_id,
            "initial_state": "planning",
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

    let sid = create_session(&server, Some(&repo_str), Some("e2e-test"));

    // Create parent + subtask (skip_planning=true to go straight to ready)
    let parent = exec_tool_ok(
        &server,
        &sid,
        "task_create",
        serde_json::json!({"title": "Parent", "initial_state": "interactive"}),
    );
    let parent_id = parent["id"].as_i64().unwrap();

    let subtask = exec_tool_ok(
        &server,
        &sid,
        "task_create",
        serde_json::json!({
            "title": "No rebase needed for review",
            "parent_id": parent_id,
            "initial_state": "ready",
            "affected_files": ["feature.txt"],
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

    let sid = create_session(&server, Some(&repo_str), Some("e2e-test"));

    // Create parent + subtask (skip_review=false)
    let parent = exec_tool_ok(
        &server,
        &sid,
        "task_create",
        serde_json::json!({"title": "Parent", "initial_state": "interactive"}),
    );
    let parent_id = parent["id"].as_i64().unwrap();

    let subtask = exec_tool_ok(
        &server,
        &sid,
        "task_create",
        serde_json::json!({
            "title": "No skip_review subtask",
            "parent_id": parent_id,
            "initial_state": "ready",
            "affected_files": ["src/no_skip_review.rs"],
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

// ---------------------------------------------------------------------------
// Auto-dispatch assertions (task #588).
//
// These tests guard against the silent-dispatch-failure regressions that
// shipped as #572, #577, #584, and #587. The flagship lifecycle test above
// drives state transitions manually via ExecuteTool and never asserts that
// `task.session_id` becomes non-null after a transition that *should*
// auto-spawn a worker — which is exactly how those bugs slipped through.
//
// Each test below files a task, waits for the scheduler to auto-dispatch a
// worker, then asserts:
//   * `task.session_id` is non-null (the dispatch path completed),
//   * the referenced session actually exists on the server,
//   * the session has the expected tagline prefix `[task N] worker:`,
//   * the session is parented under the task's placeholder.
// ---------------------------------------------------------------------------

/// Poll `task_get` until the task has a non-null `session_id`, then
/// return the parsed JSON.  Panics with the current task state on timeout.
fn wait_for_task_session_id(
    server: &TestServer,
    session_id: &str,
    task_id: i64,
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
        if task["task"]["session_id"].as_str().is_some() {
            return task;
        }
        if Instant::now() > deadline {
            panic!(
                "task {} did not get a non-null session_id within {:?}\ntask: {}",
                task_id,
                timeout,
                serde_json::to_string_pretty(&task).unwrap()
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Shared assertion: given a `task_get` payload for an auto-dispatched task,
/// verify the recorded session_id refers to a real worker session with
/// the expected tagline prefix and parent (the task's placeholder).
fn assert_worker_session_dispatched(server: &TestServer, task_payload: &serde_json::Value) {
    let task_id = task_payload["task"]["id"]
        .as_i64()
        .expect("task.id should be an integer");
    let session_id = task_payload["task"]["session_id"]
        .as_str()
        .unwrap_or_else(|| {
            panic!(
                "task {} has null session_id after auto-dispatch — \
                 this is the #572/#577/#584/#587 bug class.\ntask: {}",
                task_id,
                serde_json::to_string_pretty(task_payload).unwrap()
            )
        });
    assert!(
        !session_id.is_empty(),
        "task {} has empty session_id after auto-dispatch",
        task_id
    );

    let placeholder_session_id = task_payload["task"]["placeholder_session_id"]
        .as_str()
        .expect("every task should have a placeholder_session_id (task #561)")
        .to_string();

    let info = common::assert_session_exists(
        server,
        session_id,
        Some(&placeholder_session_id),
        Some(&format!("[task {}] worker:", task_id)),
    );

    // The session must NOT be the placeholder itself — the placeholder is a
    // grouping anchor, not the worker.
    assert_ne!(
        session_id, placeholder_session_id,
        "task {} session_id ({}) should be a worker, not the placeholder ({})",
        task_id, session_id, placeholder_session_id
    );

    // The auto-dispatched session should not have inherited the `log` model
    // from the placeholder (regression for the s2094 / log-provider chain).
    assert_ne!(
        info.model, "log",
        "task {} worker session {} unexpectedly uses the `log` model — \
         this would surface as a NoApiKey error at runtime",
        task_id, session_id
    );
    assert_ne!(
        info.provider, "log",
        "task {} worker session {} unexpectedly uses the `log` provider",
        task_id, session_id
    );
}

/// File a `ready` task with `affected_files` set and assert that the
/// scheduler actually creates a worker session.
///
/// Catches the #572/#577/#584/#587 bug class where a task transitioned to
/// `active` but no session was ever spawned.
#[test]
fn ready_to_active_transition_spawns_worker_session() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = init_git_repo(tmp.path());
    let repo_str = repo.to_string_lossy().to_string();

    // Generous mock-response budget for any agent turns the dispatched
    // session may execute before we shut down.
    let server = start_server_with_tasks(
        &repo,
        (0..16)
            .map(|_| MockResponse::Text("acknowledged".into()))
            .collect(),
    );
    std::thread::sleep(Duration::from_millis(500));

    let sid = create_session(&server, Some(&repo_str), Some("e2e-test"));

    // File a task directly as `ready` with affected_files set.
    let task = exec_tool_ok(
        &server,
        &sid,
        "task_create",
        serde_json::json!({
            "title": "Auto-dispatch worker session test",
            "initial_state": "ready",
            "message": "do the thing",
            "affected_files": ["dispatched.txt"],
        }),
    );
    let task_id = task["id"].as_i64().unwrap();
    assert_eq!(task["state"].as_str().unwrap(), "ready");

    // Wait for the scheduler to pick it up and reach `active`.
    let _ = wait_for_task_state(&server, &sid, task_id, "active", Duration::from_secs(10));

    // Then wait for the auto-dispatch to populate session_id.
    let task_payload = wait_for_task_session_id(&server, &sid, task_id, Duration::from_secs(10));

    // Critical assertions that would have caught #572/#577/#584/#587.
    assert_worker_session_dispatched(&server, &task_payload);

    server.shutdown();
}

/// File-less variant: file a `ready` task with no `affected_files` and
/// assert the scheduler still auto-dispatches a worker.
///
/// File-less tasks are governed by a separate scheduling rule that #584
/// briefly broke; this guards against future regressions of that path.
#[test]
fn ready_to_active_file_less_task_spawns_worker_session() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = init_git_repo(tmp.path());
    let repo_str = repo.to_string_lossy().to_string();

    let server = start_server_with_tasks(
        &repo,
        (0..16)
            .map(|_| MockResponse::Text("acknowledged".into()))
            .collect(),
    );
    std::thread::sleep(Duration::from_millis(500));

    let sid = create_session(&server, Some(&repo_str), Some("e2e-test"));

    // File a `ready` task with the explicit `["*"]` file-less marker
    // (task #596). The file-less scheduling rule only schedules one such
    // task at a time; we have a fresh DB so this task is the file-less
    // slot. Without the marker the task would be auto-routed through
    // planning (task #596) — we want to test the file-less ready path,
    // which is exactly what `["*"]` selects.
    let task = exec_tool_ok(
        &server,
        &sid,
        "task_create",
        serde_json::json!({
            "title": "File-less auto-dispatch test",
            "initial_state": "ready",
            "affected_files": ["*"],
            "message": "no files involved",
        }),
    );
    let task_id = task["id"].as_i64().unwrap();
    assert_eq!(task["state"].as_str().unwrap(), "ready");

    let _ = wait_for_task_state(&server, &sid, task_id, "active", Duration::from_secs(10));
    let task_payload = wait_for_task_session_id(&server, &sid, task_id, Duration::from_secs(10));

    assert_worker_session_dispatched(&server, &task_payload);

    // Confirm worktree was also created for a file-less task.
    let worktree = task_payload["task"]["worktree_path"]
        .as_str()
        .expect("file-less task should still get a worktree on active");
    assert!(
        std::path::Path::new(worktree).exists(),
        "file-less task worktree should exist at {}",
        worktree
    );

    server.shutdown();
}

/// Assert the placeholder session actually receives state-transition
/// info messages (#574, Phase-2 placeholder messaging).
///
/// Drives a task through ready → active and verifies the placeholder's
/// message history contains the expected `[task #N] ... ready → active`
/// transition line.  Catches regressions where the `collect_recipients`
/// fix from #574 gets reverted or a new transition path bypasses the
/// notify pipeline.
#[test]
fn placeholder_session_receives_state_transition_messages() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = init_git_repo(tmp.path());
    let repo_str = repo.to_string_lossy().to_string();

    let server = start_server_with_tasks(
        &repo,
        (0..16)
            .map(|_| MockResponse::Text("acknowledged".into()))
            .collect(),
    );
    std::thread::sleep(Duration::from_millis(500));

    let sid = create_session(&server, Some(&repo_str), Some("e2e-test"));

    let task = exec_tool_ok(
        &server,
        &sid,
        "task_create",
        serde_json::json!({
            "title": "Placeholder message test",
            "initial_state": "ready",
            "message": "drive the placeholder timeline",
            "affected_files": ["placeholder_test.txt"],
        }),
    );
    let task_id = task["id"].as_i64().unwrap();

    // Wait for scheduler to take it active.
    let task_payload =
        wait_for_task_state(&server, &sid, task_id, "active", Duration::from_secs(10));
    let placeholder_sid = task_payload["task"]["placeholder_session_id"]
        .as_str()
        .expect("every task should have a placeholder_session_id (task #561)")
        .to_string();

    // The state-transition `ready → active` notification is delivered as a
    // `QueueInfo` Tier-2 action; allow a brief window for it to land in the
    // placeholder's message history.
    let messages = common::poll_until(
        Duration::from_secs(5),
        Duration::from_millis(100),
        "placeholder did not receive any messages",
        || {
            let msgs = common::get_session_messages(&server, &placeholder_sid);
            if msgs.is_empty() { None } else { Some(msgs) }
        },
    );

    // Expect at least one info message tagged with the task id.
    let needle = format!("[task #{}]", task_id);
    let has_task_tag = messages.iter().any(|m| match m {
        tau_agent_lib::types::Message::Info(info) => info.text.contains(&needle),
        _ => false,
    });
    assert!(
        has_task_tag,
        "placeholder session {} message history should contain `{}` lines, got:\n{:#?}",
        placeholder_sid, needle, messages
    );

    // And specifically the ready → active transition line.
    let has_ready_to_active = messages.iter().any(|m| match m {
        tau_agent_lib::types::Message::Info(info) => {
            info.text.contains(&needle) && info.text.contains("ready → active")
        }
        _ => false,
    });
    assert!(
        has_ready_to_active,
        "placeholder session {} should have received the `ready → active` info line, got:\n{:#?}",
        placeholder_sid, messages
    );

    server.shutdown();
}

// ---------------------------------------------------------------------------
// Watchdog e2e (task #588 spec items 3 & 4).
//
// The unit-level watchdog tests in `tasks_scheduler.rs` use a fake DB and
// don't exercise the real RPC boundary.  This e2e variant drives a real
// server + tasks plugin through the full watchdog path:
//
//   1. File a `ready` task and let the scheduler dispatch a worker.
//   2. Reach into the plugin's tasks DB and forge a "stuck active task":
//      clear `session_id` and backdate `updated_at` past the 60-second
//      stuck-task threshold.
//   3. Trigger any tool call that enqueues a `ScheduleNeeded` event
//      (creating a second task does it) — `drain_scheduler_events` runs
//      `run_watchdog_pass_all`, which must spot the forged stuck task and
//      re-dispatch a fresh worker session.
//   4. Assert the recovered session: non-null, distinct from the original
//      one, real on the server, correct worker tagline, and *not* using
//      the `log` model (the s2094 / #582 / #590 regression class).
// ---------------------------------------------------------------------------

/// Like [`start_server_with_tasks`], but also registers the no-op `log`
/// provider and `log_model()` in the model list.  This is what makes the
/// model-inheritance assertions meaningful: with the log model registered,
/// the placeholder genuinely uses `model = "log"`, and a worker session
/// that incorrectly inherits from the placeholder will surface as
/// `info.model == "log"`.  Without this, the unknown-model fall-through
/// silently rewrites the placeholder's model to the default and the
/// regression assertion is a no-op.
fn start_server_with_tasks_and_log_provider(
    repo_path: &std::path::Path,
    mock_responses: Vec<MockResponse>,
) -> TestServer {
    let tau_bin = tau_binary();
    let tau_bin_str = tau_bin.to_string_lossy().to_string();

    let data_home = repo_path.parent().unwrap().join("xdg_data");
    std::fs::create_dir_all(&data_home).unwrap();
    let data_home_str = data_home.to_string_lossy().to_string();

    let tau_data_dir = data_home.join("tau");
    std::fs::create_dir_all(&tau_data_dir).unwrap();
    {
        let tau_db_path = tau_data_dir.join("tau.db");
        let conn = rusqlite::Connection::open(&tau_db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS projects (
                name TEXT PRIMARY KEY,
                path TEXT NOT NULL,
                created_at INTEGER NOT NULL DEFAULT 0,
                updated_at INTEGER NOT NULL DEFAULT 0
            )",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO projects (name, path) VALUES (?1, ?2)",
            rusqlite::params!["e2e-test", repo_path.to_string_lossy().to_string()],
        )
        .unwrap();
    }

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
        session: [(
            "worker".to_string(),
            PluginEntry {
                command: vec![tau_bin_str, "worker".into()],
                env: HashMap::new(),
            },
        )]
        .into_iter()
        .collect::<HashMap<_, _>>(),
        idle_timeout_secs: 300,
        ..Default::default()
    };

    let provider = MockProvider::new(mock_responses);

    TestServer::start_with_config(vec![], move |mut config| {
        config.registry = {
            let mut r = tau_agent_lib::provider::ProviderRegistry::new();
            r.register(provider);
            r.register(tau_agent_lib::providers::log::LogProvider);
            r
        };
        // Append the log model so resolve_model("log") finds it.  Mock
        // model stays first → server-wide default remains the mock model.
        config
            .models
            .push(tau_agent_lib::providers::log::log_model());
        config.plugins_config = Some(plugins_config);
        config
    })
}

/// Open the tasks plugin's sqlite DB at `xdg_data/tau/tasks.db` and apply
/// a "stuck task" mutation: clear `session_id` and backdate `updated_at`
/// to `now - age_ms` so the watchdog picks the task up on its next pass.
///
/// We poke the DB directly rather than going through the plugin because
/// the plugin's lifecycle paths intentionally make it hard to forge a
/// stuck state — and forging one is exactly the regression scenario we
/// need to test.
fn forge_stuck_active_task(repo_path: &std::path::Path, task_id: i64, age_ms: i64) {
    let tasks_db_path = repo_path
        .parent()
        .unwrap()
        .join("xdg_data")
        .join("tau")
        .join("tasks.db");
    let conn = rusqlite::Connection::open(&tasks_db_path)
        .expect("open plugin tasks.db for direct mutation");
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let backdated = now_ms - age_ms;
    let n = conn
        .execute(
            "UPDATE tasks SET session_id = NULL, updated_at = ?1 WHERE id = ?2",
            rusqlite::params![backdated, task_id],
        )
        .expect("forge stuck task UPDATE");
    assert_eq!(
        n, 1,
        "forge_stuck_active_task: expected to update exactly 1 row for task {}, got {}",
        task_id, n
    );
}

/// End-to-end regression test for the #572 watchdog.
///
/// Fakes a stuck `active` task (no session_id, `updated_at` past the
/// 60-second threshold) and asserts the watchdog re-dispatches a fresh
/// worker session for it.  Also catches the #590 regression: the
/// re-dispatched worker must NOT inherit `model = "log"` from the
/// task's placeholder.
#[test]
fn watchdog_recovers_task_stuck_without_session_id() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = init_git_repo(tmp.path());
    let repo_str = repo.to_string_lossy().to_string();

    // Use the log-aware test server so `log_model()` is registered: the
    // placeholder genuinely ends up with `model = "log"`, which is what
    // makes the #590 regression assertion meaningful.
    let server = start_server_with_tasks_and_log_provider(
        &repo,
        (0..32)
            .map(|_| MockResponse::Text("acknowledged".into()))
            .collect(),
    );
    std::thread::sleep(Duration::from_millis(500));

    let sid = create_session(&server, Some(&repo_str), Some("e2e-test"));

    // -----------------------------------------------------------------------
    // 1. File a ready task and let the scheduler dispatch the first worker.
    // -----------------------------------------------------------------------
    let task = exec_tool_ok(
        &server,
        &sid,
        "task_create",
        serde_json::json!({
            "title": "Watchdog recovery test",
            "initial_state": "ready",
            "message": "watchdog should rescue me",
            "affected_files": ["watchdog_test.txt"],
        }),
    );
    let task_id = task["id"].as_i64().unwrap();

    let original_payload =
        wait_for_task_session_id(&server, &sid, task_id, Duration::from_secs(10));
    let original_session_id = original_payload["task"]["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    let placeholder_sid = original_payload["task"]["placeholder_session_id"]
        .as_str()
        .expect("every task should have a placeholder_session_id (task #561)")
        .to_string();

    // Sanity: the placeholder really did get the `log` model.  If this
    // ever flips to `mock-model`, the model-inheritance assertion below
    // becomes a no-op and someone needs to look at why the log model
    // stopped being registered for tests.
    let placeholder_info = common::get_session_info(&server, &placeholder_sid)
        .expect("placeholder session must exist");
    assert_eq!(
        placeholder_info.model, "log",
        "test setup expects placeholder to use the `log` model so the \
         watchdog-path inheritance assertion is meaningful, got {:?}",
        placeholder_info.model
    );

    // -----------------------------------------------------------------------
    // 2. Archive the original worker session so the watchdog can't "reuse"
    //    it via find_reusable_session — that would skip the actual
    //    re-dispatch path we want to exercise.
    // -----------------------------------------------------------------------
    {
        let conn = server.connect();
        match common::send_recv(
            &conn,
            &tau_agent_lib::protocol::Request::ArchiveSession {
                session_id: original_session_id.clone(),
                require_ancestor: None,
            },
        ) {
            tau_agent_lib::protocol::Response::SessionArchived => {}
            other => panic!("failed to archive original worker session: {:?}", other),
        }
    }

    // -----------------------------------------------------------------------
    // 3. Forge a stuck-active state: clear session_id + backdate updated_at
    //    past the 60-second threshold so the watchdog picks it up.
    // -----------------------------------------------------------------------
    forge_stuck_active_task(&repo, task_id, 90_000);

    // Confirm the forge took: task_get must report session_id == null.
    let stuck = exec_tool_ok(
        &server,
        &sid,
        "task_get",
        serde_json::json!({"id": task_id}),
    );
    assert!(
        stuck["task"]["session_id"].is_null(),
        "after forge, task_get should report null session_id, got: {}",
        stuck["task"]["session_id"]
    );
    assert_eq!(stuck["task"]["state"].as_str().unwrap(), "active");

    // -----------------------------------------------------------------------
    // 4. Trigger drain_scheduler_events by enqueuing any scheduler event.
    //    Creating a second `ready` task pushes a ScheduleNeeded event; the
    //    drain runs the schedule pass and then run_watchdog_pass_all, which
    //    spots our forged stuck task and re-dispatches it.
    // -----------------------------------------------------------------------
    let _trigger = exec_tool_ok(
        &server,
        &sid,
        "task_create",
        serde_json::json!({
            "title": "Watchdog trigger task",
            "initial_state": "ready",
            "message": "your only purpose is to make a ScheduleNeeded event fire",
            "affected_files": ["watchdog_trigger.txt"],
        }),
    );

    // -----------------------------------------------------------------------
    // 5. Wait for the watchdog to repopulate session_id with a NEW value.
    // -----------------------------------------------------------------------
    let recovered = common::poll_until(
        Duration::from_secs(15),
        Duration::from_millis(100),
        "watchdog did not re-dispatch the stuck task",
        || {
            let payload = exec_tool_ok(
                &server,
                &sid,
                "task_get",
                serde_json::json!({"id": task_id}),
            );
            match payload["task"]["session_id"].as_str() {
                Some(s) if s != original_session_id => Some(payload),
                _ => None,
            }
        },
    );

    // Critical assertions: the recovered session must be a real, distinct,
    // non-`log` worker session parented under the task's placeholder.
    assert_worker_session_dispatched(&server, &recovered);

    let new_session_id = recovered["task"]["session_id"].as_str().unwrap();
    assert_ne!(
        new_session_id, original_session_id,
        "watchdog should re-dispatch with a new session, not reuse the cleared one"
    );

    // Spec item 4 (the #590 regression cover): the watchdog-dispatched
    // worker must NOT have inherited `model = log` from the placeholder.
    let recovered_info =
        common::get_session_info(&server, new_session_id).expect("recovered session must exist");
    assert_ne!(
        recovered_info.model, "log",
        "watchdog-dispatched worker session {} unexpectedly inherited \
         `model = log` from the placeholder — this is the #590 / s2094 \
         regression class.  Worker would surface a NoApiKey error at runtime.",
        new_session_id
    );
    assert_ne!(
        recovered_info.provider, "log",
        "watchdog-dispatched worker session {} unexpectedly inherited \
         `provider = log` from the placeholder",
        new_session_id
    );

    // The watchdog must also have logged its action as a task message
    // (via the WATCHDOG_ATTEMPT_MARKER), so a human reading the task
    // timeline can see that recovery happened.
    let messages = recovered["messages"].as_array().unwrap();
    let watchdog_marker = messages.iter().any(|m| {
        m["content"]
            .as_str()
            .map(|c| c.contains("[watchdog-dispatch-attempt]"))
            .unwrap_or(false)
    });
    assert!(
        watchdog_marker,
        "task timeline should record a [watchdog-dispatch-attempt] message, got: {}",
        serde_json::to_string_pretty(messages).unwrap()
    );

    server.shutdown();
}
