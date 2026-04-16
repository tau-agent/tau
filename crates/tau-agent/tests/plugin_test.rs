//! Integration tests for the subprocess plugin system.

use std::collections::HashMap;
use std::path::PathBuf;
use tau_agent::plugin::*;
use tau_agent::types::ToolCall;

fn test_plugin_command() -> Vec<String> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let script = manifest_dir
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("tests")
        .join("test_plugin.py");
    vec!["python3".into(), script.to_string_lossy().into()]
}

#[test]
fn plugin_registration() {
    let cmd = test_plugin_command();
    let handle = PluginHandle::spawn(&cmd, "/tmp", &HashMap::new()).unwrap();

    assert_eq!(handle.name, "test-plugin");
    assert_eq!(handle.registration.tools.len(), 3);
    assert_eq!(handle.registration.hooks.len(), 3);
    assert_eq!(handle.registration.commands.len(), 1);

    // Check tool details
    let echo = handle
        .registration
        .tools
        .iter()
        .find(|t| t.name == "echo_tool")
        .unwrap();
    assert_eq!(
        echo.prompt_snippet.as_deref(),
        Some("Echo back input for testing")
    );
    assert_eq!(echo.prompt_guidelines.len(), 1);

    let slow = handle
        .registration
        .tools
        .iter()
        .find(|t| t.name == "slow_tool")
        .unwrap();
    assert_eq!(
        slow.prompt_snippet.as_deref(),
        Some("Produce streaming output for testing")
    );

    let fail = handle
        .registration
        .tools
        .iter()
        .find(|t| t.name == "fail_tool")
        .unwrap();
    assert!(fail.prompt_snippet.is_none());

    // Check hooks
    assert!(
        handle
            .registration
            .hooks
            .contains(&"before_agent_start".to_string())
    );
    assert!(
        handle
            .registration
            .hooks
            .contains(&"session_start".to_string())
    );

    // Check commands
    assert_eq!(handle.registration.commands[0].name, "test-cmd");
}

#[test]
fn plugin_tool_schemas() {
    let cmd = test_plugin_command();
    let handle = PluginHandle::spawn(&cmd, "/tmp", &HashMap::new()).unwrap();

    let schemas = handle.tool_schemas();
    assert_eq!(schemas.len(), 3);
    assert!(schemas.iter().any(|t| t.name == "echo_tool"));
    assert!(schemas.iter().any(|t| t.name == "slow_tool"));
    assert!(schemas.iter().any(|t| t.name == "fail_tool"));
}

#[test]
fn plugin_tool_prompts() {
    let cmd = test_plugin_command();
    let handle = PluginHandle::spawn(&cmd, "/tmp", &HashMap::new()).unwrap();

    let prompts = handle.tool_prompts();
    // Only tools with prompt_snippet get a ToolPrompt
    assert_eq!(prompts.len(), 2); // echo_tool and slow_tool
    assert!(prompts.iter().any(|p| p.name == "echo_tool"));
    assert!(prompts.iter().any(|p| p.name == "slow_tool"));
}

#[test]
fn plugin_echo_tool() {
    let cmd = test_plugin_command();
    let mut handle = PluginHandle::spawn(&cmd, "/tmp", &HashMap::new()).unwrap();

    let tc = ToolCall {
        id: "tc1".into(),
        name: "echo_tool".into(),
        arguments: serde_json::json!({"message": "hello world"}),
    };

    let mut deltas = Vec::new();
    let result = handle
        .execute_tool(&tc, None, None, &mut |d| deltas.push(d.to_string()))
        .unwrap();

    assert!(!result.is_error);
    assert_eq!(result.tool_call_id, "tc1");
    assert!(deltas.is_empty()); // echo_tool doesn't stream

    let text = result
        .content
        .iter()
        .filter_map(|c| match c {
            tau_agent::types::ToolResultContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("");
    assert_eq!(text, "ECHO: hello world");
}

#[test]
fn plugin_slow_tool_streaming() {
    let cmd = test_plugin_command();
    let mut handle = PluginHandle::spawn(&cmd, "/tmp", &HashMap::new()).unwrap();

    let tc = ToolCall {
        id: "tc2".into(),
        name: "slow_tool".into(),
        arguments: serde_json::json!({"lines": 5}),
    };

    let mut deltas = Vec::new();
    let result = handle
        .execute_tool(&tc, None, None, &mut |d| deltas.push(d.to_string()))
        .unwrap();

    assert!(!result.is_error);
    assert_eq!(deltas.len(), 5);
    assert_eq!(deltas[0], "line 1");
    assert_eq!(deltas[4], "line 5");

    let text = result
        .content
        .iter()
        .filter_map(|c| match c {
            tau_agent::types::ToolResultContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("");
    assert_eq!(text, "produced 5 lines");
}

#[test]
fn plugin_fail_tool() {
    let cmd = test_plugin_command();
    let mut handle = PluginHandle::spawn(&cmd, "/tmp", &HashMap::new()).unwrap();

    let tc = ToolCall {
        id: "tc3".into(),
        name: "fail_tool".into(),
        arguments: serde_json::json!({}),
    };

    let mut deltas = Vec::new();
    let result = handle
        .execute_tool(&tc, None, None, &mut |d| deltas.push(d.to_string()))
        .unwrap();

    assert!(result.is_error);
    let text = result
        .content
        .iter()
        .filter_map(|c| match c {
            tau_agent::types::ToolResultContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("");
    assert_eq!(text, "intentional failure");
}

#[test]
fn plugin_before_agent_start_hook() {
    let cmd = test_plugin_command();
    let mut handle = PluginHandle::spawn(&cmd, "/tmp", &HashMap::new()).unwrap();

    let result = handle
        .call_hook(
            "before_agent_start",
            serde_json::json!({"prompt": "test prompt"}),
        )
        .unwrap();

    assert!(result.message.is_some());
    let msg = result.message.unwrap();
    assert!(msg.content.contains("test plugin"));
}

#[test]
fn plugin_session_start_hook() {
    let cmd = test_plugin_command();
    let mut handle = PluginHandle::spawn(&cmd, "/tmp", &HashMap::new()).unwrap();

    // session_start is sent as a SessionStart request, not a Hook
    handle
        .send(&PluginRequest::SessionStart {
            cwd: "/home/test".into(),
            session_id: "s123".into(),
            project_name: None,
        })
        .unwrap();

    let msg = handle.read_message().unwrap();
    assert!(matches!(msg, PluginMessage::HookResult(_)));
}

#[test]
fn plugin_multiple_tool_calls() {
    let cmd = test_plugin_command();
    let mut handle = PluginHandle::spawn(&cmd, "/tmp", &HashMap::new()).unwrap();

    // Call echo multiple times on the same plugin
    for i in 0..3 {
        let tc = ToolCall {
            id: format!("tc{}", i),
            name: "echo_tool".into(),
            arguments: serde_json::json!({"message": format!("msg {}", i)}),
        };
        let result = handle.execute_tool(&tc, None, None, &mut |_| {}).unwrap();
        assert!(!result.is_error);
        let text = result
            .content
            .iter()
            .filter_map(|c| match c {
                tau_agent::types::ToolResultContent::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");
        assert_eq!(text, format!("ECHO: msg {}", i));
    }
}

#[test]
fn plugin_manager_integration() {
    let cmd = test_plugin_command();
    let config = PluginsConfig {
        session_prefix: None,
        global: [(
            "test".into(),
            PluginEntry {
                command: cmd,
                env: HashMap::new(),
            },
        )]
        .into_iter()
        .collect(),
        session: Default::default(),
        no_default_worker: false,
        idle_timeout_secs: 30,
    };

    let mut manager = PluginManager::new(config);
    manager.load_global_plugins("/tmp");

    let session_id = "test-session";

    // Check tool schemas
    let schemas = manager.tool_schemas(session_id, 16);
    assert_eq!(schemas.len(), 3);

    // Check tool prompts
    let prompts = manager.tool_prompts(session_id, 16);
    assert_eq!(prompts.len(), 2);

    // Check commands
    let commands = manager.commands();
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].0, "test-cmd");

    // Execute a tool through the manager
    let tc = ToolCall {
        id: "mgr1".into(),
        name: "echo_tool".into(),
        arguments: serde_json::json!({"message": "via manager"}),
    };
    let result = manager
        .execute_tool(session_id, &tc, "/tmp", &mut |_| {})
        .unwrap();
    assert!(!result.is_error);

    // Call hook through the manager
    let results = manager.call_hook(
        session_id,
        "before_agent_start",
        &serde_json::json!({"prompt": "test"}),
    );
    assert_eq!(results.len(), 1);
    assert!(results[0].message.is_some());
}

/// Verify that tool_schemas returns global plugin tools even while a global
/// plugin handle is temporarily taken for tool execution.
///
/// This is a regression test for the race condition where task_dispatch
/// takes the tasks plugin handle, spawns a child session, and the child
/// session's tool_schemas call runs before the handle is returned — causing
/// task tools to be missing from the LLM context.
#[test]
fn tool_schemas_stable_during_take() {
    let cmd = test_plugin_command();
    let config = PluginsConfig {
        session_prefix: None,
        global: [(
            "test".into(),
            PluginEntry {
                command: cmd,
                env: HashMap::new(),
            },
        )]
        .into_iter()
        .collect(),
        session: Default::default(),
        no_default_worker: true,
        idle_timeout_secs: 30,
    };

    let mut manager = PluginManager::new(config);
    manager.load_global_plugins("/tmp");

    let session_id = "test-session";

    // Before take: tools are present
    let schemas_before = manager.tool_schemas(session_id, 16);
    assert!(!schemas_before.is_empty(), "should have tools before take");

    // Take the plugin handle (simulating PluginExecutor during a tool call)
    let taken = manager.take_tool_plugin(session_id, "echo_tool");
    assert!(taken.is_some(), "should be able to take echo_tool plugin");
    let (handle, source) = taken.unwrap();

    // While taken: tool_schemas must STILL return the same tools
    let schemas_during = manager.tool_schemas(session_id, 16);
    assert_eq!(
        schemas_before.len(),
        schemas_during.len(),
        "tool_schemas should return same tools even while plugin handle is taken; \
         before={:?}, during={:?}",
        schemas_before.iter().map(|t| &t.name).collect::<Vec<_>>(),
        schemas_during.iter().map(|t| &t.name).collect::<Vec<_>>(),
    );

    // tool_prompts should also be stable
    let prompts_during = manager.tool_prompts(session_id, 16);
    assert!(
        !prompts_during.is_empty(),
        "tool_prompts should return prompts even while plugin handle is taken"
    );

    // Return the handle
    manager.return_tool_plugin(source, handle);

    // After return: tools still present
    let schemas_after = manager.tool_schemas(session_id, 16);
    assert_eq!(schemas_before.len(), schemas_after.len());
}

#[test]
fn plugin_wants_hook() {
    let cmd = test_plugin_command();
    let handle = PluginHandle::spawn(&cmd, "/tmp", &HashMap::new()).unwrap();

    assert!(handle.wants_hook("before_agent_start"));
    assert!(handle.wants_hook("session_start"));
    assert!(handle.wants_hook("after_tool_result"));
    assert!(!handle.wants_hook("nonexistent_hook"));
}

#[test]
fn plugin_after_tool_result_hook() {
    let cmd = test_plugin_command();
    let mut handle = PluginHandle::spawn(&cmd, "/tmp", &HashMap::new()).unwrap();

    let result = handle
        .call_hook(
            "after_tool_result",
            serde_json::json!({
                "tool_name": "edit",
                "arguments": {"path": "src/main.rs"},
                "content": "Applied 1 edit",
                "is_error": false,
            }),
        )
        .unwrap();

    assert!(result.tool_result_append.is_some());
    let append = result.tool_result_append.unwrap();
    assert!(append.contains("TEST DIAGNOSTICS"));
    assert!(append.contains("edit"));
}

#[test]
fn plugin_send_idle_kills_worker() {
    // The built-in worker exits on Idle. Spawn it and verify.
    let exe = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("tau");

    // Only run if the binary exists (it may not in all test environments)
    if !exe.exists() {
        eprintln!("skipping: tau binary not found at {:?}", exe);
        return;
    }

    let cmd = vec![exe.to_string_lossy().to_string(), "worker".to_string()];
    let mut handle = PluginHandle::spawn(&cmd, "/tmp", &HashMap::new()).unwrap();
    assert!(handle.is_alive());

    handle.send_idle();
    // After idle, worker should have exited
    // Give it a moment to fully exit
    std::thread::sleep(std::time::Duration::from_millis(100));
    assert!(!handle.is_alive());
}

#[test]
fn plugin_is_alive_tracks_state() {
    let cmd = test_plugin_command();
    let mut handle = PluginHandle::spawn(&cmd, "/tmp", &HashMap::new()).unwrap();

    // Plugin should be alive after spawn
    assert!(handle.is_alive());

    // Kill it and check
    handle.kill();
    assert!(!handle.is_alive());
}

#[test]
fn plugin_respawn_after_kill() {
    let cmd = test_plugin_command();
    let mut handle = PluginHandle::spawn(&cmd, "/tmp", &HashMap::new()).unwrap();

    // Kill the plugin
    handle.kill();
    assert!(!handle.is_alive());

    // Respawn it
    handle.respawn().unwrap();
    assert!(handle.is_alive());

    // Verify it still works -- call a hook
    let result = handle
        .call_hook("before_agent_start", serde_json::json!({}))
        .unwrap();
    assert!(result.message.is_some());
    assert!(result.message.unwrap().content.contains("test plugin"));
}

#[test]
fn plugin_ensure_alive_no_op_when_running() {
    let cmd = test_plugin_command();
    let mut handle = PluginHandle::spawn(&cmd, "/tmp", &HashMap::new()).unwrap();

    // Should be a no-op when already alive
    handle.ensure_alive().unwrap();
    assert!(handle.is_alive());

    // Execute a tool to confirm it works
    let tool_call = ToolCall {
        id: "test-1".into(),
        name: "echo_tool".into(),
        arguments: serde_json::json!({"message": "alive"}),
    };
    let result = handle
        .execute_tool(&tool_call, None, None, &mut |_| {})
        .unwrap();
    assert!(!result.is_error);
}

#[test]
fn plugin_ensure_alive_respawns_when_dead() {
    let cmd = test_plugin_command();
    let mut handle = PluginHandle::spawn(&cmd, "/tmp", &HashMap::new()).unwrap();

    // Kill it
    handle.kill();
    assert!(!handle.is_alive());

    // ensure_alive should respawn
    handle.ensure_alive().unwrap();
    assert!(handle.is_alive());

    // Execute a tool to confirm it works after respawn
    let tool_call = ToolCall {
        id: "test-1".into(),
        name: "echo_tool".into(),
        arguments: serde_json::json!({"message": "respawned"}),
    };
    let result = handle
        .execute_tool(&tool_call, None, None, &mut |_| {})
        .unwrap();
    assert!(!result.is_error);
    let text: String = result
        .content
        .iter()
        .filter_map(|c| match c {
            tau_agent::types::ToolResultContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("");
    assert!(text.contains("respawned"));
}

#[test]
fn plugin_last_activity_updates_on_send() {
    let cmd = test_plugin_command();
    let mut handle = PluginHandle::spawn(&cmd, "/tmp", &HashMap::new()).unwrap();

    let t1 = handle.last_activity;
    std::thread::sleep(std::time::Duration::from_millis(50));

    // Sending a hook should update last_activity
    let _ = handle.call_hook("before_agent_start", serde_json::json!({}));
    let t2 = handle.last_activity;
    assert!(t2 > t1);
}

#[test]
fn session_plugins_idle_sweep() {
    let cmd = test_plugin_command();
    let config = PluginsConfig {
        session_prefix: None,
        global: Default::default(),
        session: [(
            "test".into(),
            PluginEntry {
                command: cmd,
                env: HashMap::new(),
            },
        )]
        .into_iter()
        .collect(),
        no_default_worker: true,
        idle_timeout_secs: 0, // immediate idle
    };

    let mut manager = PluginManager::new(config);
    let session_id = "idle-test";
    manager
        .ensure_session_plugins(session_id, "/tmp", None, None)
        .unwrap();

    // Set last_activity to the past so idle sweep triggers
    // We can't easily backdate, so use a zero idle_timeout
    let idle_timeout = std::time::Duration::from_secs(0);
    let no_subscribers = |_: &str| false;

    let idled = manager.idle_sweep(idle_timeout, &no_subscribers);
    assert!(idled.contains(&session_id.to_string()));
}

#[test]
fn session_plugins_idle_sweep_skips_subscribed() {
    let cmd = test_plugin_command();
    let config = PluginsConfig {
        session_prefix: None,
        global: Default::default(),
        session: [(
            "test".into(),
            PluginEntry {
                command: cmd,
                env: HashMap::new(),
            },
        )]
        .into_iter()
        .collect(),
        no_default_worker: true,
        idle_timeout_secs: 0,
    };

    let mut manager = PluginManager::new(config);
    let session_id = "subscribed-test";
    manager
        .ensure_session_plugins(session_id, "/tmp", None, None)
        .unwrap();

    let idle_timeout = std::time::Duration::from_secs(0);
    let has_subscriber = |sid: &str| sid == session_id;

    let idled = manager.idle_sweep(idle_timeout, &has_subscriber);
    assert!(idled.is_empty());
}

#[test]
fn plugins_config_default_idle_timeout() {
    let config = PluginsConfig::default();
    assert_eq!(config.idle_timeout_secs, 30);
}

#[test]
fn plugins_config_toml_idle_timeout() {
    let toml_str = r#"
idle_timeout_secs = 60
no_default_worker = true
"#;
    let config: PluginsConfig = toml::from_str(toml_str).unwrap();
    assert_eq!(config.idle_timeout_secs, 60);
}

#[test]
fn plugins_config_toml_default_idle_timeout() {
    // When idle_timeout_secs is not specified, should default to 30
    let toml_str = r#"
no_default_worker = true
"#;
    let config: PluginsConfig = toml::from_str(toml_str).unwrap();
    assert_eq!(config.idle_timeout_secs, 30);
}

/// Regression test for task 481:
///
/// After `PluginManager::setup_background_io` is called, the sync stdin /
/// stdout pipes on the global `PluginHandle` have been handed over to
/// background reader/writer tasks, leaving the sync handle without pipes.
///
/// `notify_session_start_once` and `call_hook` are both invoked from sync
/// call sites that still reach these global plugins. The commit that
/// introduced background I/O (5d0ae31) inadvertently broke those paths
/// because the sync `send`/`read_message` looked only at the (now absent)
/// sync pipes.
///
/// This test wires up a real background reader/writer for a global plugin
/// and verifies that both `notify_session_start_once` and `call_hook`
/// still deliver the request and receive the response.
#[test]
fn global_plugin_hooks_work_through_background_io() {
    use tau_agent::plugin::PluginMessage;

    let cmd = test_plugin_command();
    let config = PluginsConfig {
        session_prefix: None,
        global: [(
            "test".into(),
            PluginEntry {
                command: cmd,
                env: HashMap::new(),
            },
        )]
        .into_iter()
        .collect(),
        session: Default::default(),
        no_default_worker: true,
        idle_timeout_secs: 30,
    };

    let mut manager = PluginManager::new(config);
    manager.load_global_plugins("/tmp");

    // Install background I/O channels. This upgrades pipes to async and
    // hands them over to the caller, who is expected to spawn reader/writer
    // tasks. We mimic what `server::registry` does in production.
    let io_pairs = manager.setup_background_io();
    assert_eq!(io_pairs.len(), 1);

    for (_plugin_name, mut reader, mut writer, msg_tx, write_rx) in io_pairs {
        // Writer task: drain write_rx → stdin.
        smol::spawn(async move {
            use futures::io::AsyncWriteExt;
            while let Ok(req) = write_rx.recv().await {
                let mut line = serde_json::to_vec(&req).expect("serialize");
                line.push(b'\n');
                if writer.write_all(&line).await.is_err() {
                    break;
                }
                if writer.flush().await.is_err() {
                    break;
                }
            }
        })
        .detach();

        // Reader task: stdout → msg_tx.
        smol::spawn(async move {
            use futures::io::AsyncBufReadExt;
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) => break,
                    Ok(_) => {
                        let trimmed = line.trim_end();
                        if trimmed.is_empty() {
                            continue;
                        }
                        match serde_json::from_str::<PluginMessage>(trimmed) {
                            Ok(msg) => {
                                if msg_tx.send(msg).await.is_err() {
                                    break;
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    Err(_) => break,
                }
            }
        })
        .detach();
    }

    let session_id = "bg-io-session";

    // notify_session_start_once must reach the plugin via the bg channels.
    // This call would silently fail before the fix — we verify it below
    // by observing that the plugin's state advanced and subsequent hook
    // invocations still round-trip.
    manager.notify_session_start_once("/tmp", session_id, Some("proj"));

    // call_hook on a global plugin must also work. Before the fix, the
    // sync send() returned `plugin ... is not running` immediately and the
    // HookResult was never received, so the manager returned an empty
    // results vec. After the fix, we get the real hook result from the
    // test plugin (which injects a context message for before_agent_start).
    let results = manager.call_hook(
        session_id,
        "before_agent_start",
        &serde_json::json!({"prompt": "hello"}),
    );
    assert_eq!(
        results.len(),
        1,
        "expected one hook result from global plugin"
    );
    let msg = results[0]
        .message
        .as_ref()
        .expect("global plugin should have returned a context message");
    assert!(
        msg.content.contains("[TEST PLUGIN]"),
        "unexpected hook message: {:?}",
        msg
    );
}
