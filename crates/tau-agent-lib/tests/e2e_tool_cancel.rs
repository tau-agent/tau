//! Integration test for task 535: Ctrl-C (CancelChat) during a bash tool
//! call must kill the subprocess promptly, not wait for the 120s watchdog.
//!
//! Unlike the unit test in `tau-agent-engine` (which exercises the
//! in-process worker path), this test exercises the full production path:
//!
//!   server  →  PluginExecutor (agent_runner.rs)
//!           →  `tau worker` subprocess (session plugin) over JSON-lines
//!           →  bash subprocess (PGID tracked)
//!
//! Specifically, this catches the bug where cancel-RPC delivery was
//! gated on `handle.background_write_tx()` — which is only installed for
//! *global* plugins, not session plugins. The worker plugin is a session
//! plugin, so cancel was silently dropped in production.

#![cfg(unix)]

mod common;

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use common::TestServer;
use tau_agent_lib::plugin::{PluginEntry, PluginsConfig};
use tau_agent_lib::protocol::{Request, Response};
use tau_agent_lib::providers::mock::MockResponse;

fn tau_binary() -> PathBuf {
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("manifest parent")
        .parent()
        .expect("manifest grandparent")
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

fn send(conn: &UnixStream, req: &Request) {
    let mut stream = conn.try_clone().expect("clone");
    let line = format!("{}\n", serde_json::to_string(req).expect("ser"));
    stream.write_all(line.as_bytes()).expect("write");
    stream.flush().expect("flush");
}

fn recv_line(reader: &mut BufReader<UnixStream>) -> Response {
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).expect("read");
        assert!(n > 0, "eof while waiting for response");
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        return serde_json::from_str(line).expect("parse");
    }
}

#[test]
fn cancel_chat_kills_bash_subprocess_through_worker_plugin() {
    // Build the tau binary (for the worker subprocess).
    // (cargo-test infrastructure guarantees debug binary is current.)
    let tau_bin = tau_binary();
    let tau_bin_str = tau_bin.to_string_lossy().to_string();

    // Mock provider: a single assistant response that calls bash with a long
    // sleep. The bash tool will hold the agent loop inside execute_tool; we
    // cancel mid-flight and expect the subprocess to die within ~1s.
    let mock = vec![MockResponse::ToolCalls(vec![
        tau_agent_lib::types::ToolCall {
            id: "tc-cancel".into(),
            name: "bash".into(),
            arguments: serde_json::json!({
                // Long enough to clearly distinguish cancellation from natural
                // exit, short of the default 120s tool watchdog.
                "command": "sleep 90",
                "timeout": 120,
            }),
        },
    ])];

    // Spawn the real `tau worker` as a session plugin so bash is actually
    // available. This is the production configuration.
    let plugins_config = PluginsConfig {
        no_default_worker: true,
        global: HashMap::new(),
        session: [(
            "worker".to_string(),
            PluginEntry {
                command: vec![tau_bin_str, "worker".into()],
                env: HashMap::new(),
            },
        )]
        .into_iter()
        .collect(),
        idle_timeout_secs: 300, // avoid idle-killing mid-test
        ..Default::default()
    };

    let server = TestServer::start_with_config(mock, move |mut config| {
        config.plugins_config = Some(plugins_config);
        config
    });

    // Create a session.
    let admin = server.connect();
    admin
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("read_timeout");
    let sid = {
        send(
            &admin,
            &Request::CreateSession {
                model: None,
                provider: None,
                system_prompt: Some("You are helpful.".into()),
                cwd: Some("/tmp".into()),
                parent_id: None,
                child_budget: 0,
                tagline: None,
                auto_archive: false,
                notify_parent: true,
                project_name: None,
                sandbox_profile: None,
            },
        );
        let mut reader = BufReader::new(admin.try_clone().expect("clone"));
        match recv_line(&mut reader) {
            Response::SessionCreated { session_id } => session_id,
            other => panic!("expected SessionCreated, got {:?}", other),
        }
    };

    // Subscribe on a separate connection so we can tell when the agent is
    // actually running (otherwise the cancel races ahead of the tool
    // dispatch and short-circuits via should_stop).
    let sub_conn = server.connect();
    sub_conn
        .set_read_timeout(Some(Duration::from_secs(30)))
        .expect("read_timeout");
    send(
        &sub_conn,
        &Request::Subscribe {
            session_id: sid.clone(),
        },
    );
    let mut sub_reader = BufReader::new(sub_conn.try_clone().expect("clone"));
    // Drain the initial Phase event.
    let _initial = recv_line(&mut sub_reader);

    // Start a chat (fires and forgets; events stream to the subscriber).
    let chat_conn = server.connect();
    chat_conn
        .set_read_timeout(Some(Duration::from_secs(30)))
        .expect("read_timeout");
    send(
        &chat_conn,
        &Request::Chat {
            session_id: sid.clone(),
            text: "please run the command".into(),
        },
    );

    // Wait for the bash subprocess to actually start. We watch the
    // tracked-PGID registry on the *worker subprocess*, which we don't
    // have direct access to — so instead we watch StreamEvent::Phase and
    // StreamEvent::ToolcallStart on the subscription, and then give the
    // subprocess a moment to spawn under setsid().
    //
    // (The PGID check happens below via the server-level cancel + timing
    // assertion; if cancel doesn't reach the subprocess, the test times
    // out well past the 1-2s budget.)
    let start_wait_deadline = Instant::now() + Duration::from_secs(10);
    let mut saw_tool_start = false;
    while Instant::now() < start_wait_deadline {
        let resp = recv_line(&mut sub_reader);
        if let Response::Stream { event } = &resp
            && matches!(
                event.as_ref(),
                tau_agent_lib::types::StreamEvent::ToolcallStart { .. }
                    | tau_agent_lib::types::StreamEvent::ToolcallEnd { .. }
            )
        {
            saw_tool_start = true;
            break;
        }
        if matches!(resp, Response::AgentDone | Response::Cancelled) {
            panic!("chat finished before tool started: {:?}", resp);
        }
    }
    assert!(saw_tool_start, "did not observe tool-call start event");

    // Give setsid() a moment after the toolcall_end event so the bash child
    // is definitely alive and tracked when we cancel.
    std::thread::sleep(Duration::from_millis(150));

    // Send CancelChat.
    let cancel_start = Instant::now();
    let cancel_conn = server.connect();
    cancel_conn
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("read_timeout");
    send(
        &cancel_conn,
        &Request::CancelChat {
            session_id: sid.clone(),
            caller_session_id: None,
        },
    );
    let mut cancel_reader = BufReader::new(cancel_conn.try_clone().expect("clone"));
    let cancel_resp = recv_line(&mut cancel_reader);
    assert!(
        matches!(cancel_resp, Response::Ok),
        "expected Ok from CancelChat, got {:?}",
        cancel_resp
    );

    // Subscriber should see the tool result arrive within a couple of
    // seconds — well before the 90s sleep or 120s watchdog would fire.
    // If the cancel RPC doesn't reach the worker plugin, the tool
    // wouldn't complete until ~120s (the watchdog), so this bounds the
    // end-to-end cancel latency.
    let deadline = cancel_start + Duration::from_secs(10);
    let mut tool_result_seen = false;
    let mut tool_result_text = String::new();
    let mut tool_result_is_error = false;
    while Instant::now() < deadline {
        let resp = recv_line(&mut sub_reader);
        match &resp {
            Response::Stream { event } => {
                if let tau_agent_lib::types::StreamEvent::ToolResult {
                    tool_call_id,
                    is_error,
                    content,
                    ..
                } = event.as_ref()
                    && tool_call_id == "tc-cancel"
                {
                    tool_result_seen = true;
                    tool_result_is_error = *is_error;
                    tool_result_text = content.clone();
                    break;
                }
            }
            Response::Cancelled | Response::AgentDone => break,
            _ => {}
        }
    }
    let elapsed = cancel_start.elapsed();

    assert!(
        tool_result_seen,
        "no ToolResult for tc-cancel within {:?} of CancelChat",
        elapsed
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "bash subprocess should die promptly after CancelChat, took {:?}",
        elapsed
    );
    assert!(
        tool_result_is_error,
        "cancelled bash should produce an error tool result"
    );
    assert!(
        tool_result_text.contains("cancelled"),
        "tool result should mention cancellation, got: {}",
        tool_result_text
    );

    server.shutdown();
}
