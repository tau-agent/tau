//! Integration tests for the subprocess plugin system.

use std::path::PathBuf;
use tau::plugin::*;
use tau::types::ToolCall;

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
    let handle = PluginHandle::spawn(&cmd, "/tmp").unwrap();

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
    let handle = PluginHandle::spawn(&cmd, "/tmp").unwrap();

    let schemas = handle.tool_schemas();
    assert_eq!(schemas.len(), 3);
    assert!(schemas.iter().any(|t| t.name == "echo_tool"));
    assert!(schemas.iter().any(|t| t.name == "slow_tool"));
    assert!(schemas.iter().any(|t| t.name == "fail_tool"));
}

#[test]
fn plugin_tool_prompts() {
    let cmd = test_plugin_command();
    let handle = PluginHandle::spawn(&cmd, "/tmp").unwrap();

    let prompts = handle.tool_prompts();
    // Only tools with prompt_snippet get a ToolPrompt
    assert_eq!(prompts.len(), 2); // echo_tool and slow_tool
    assert!(prompts.iter().any(|p| p.name == "echo_tool"));
    assert!(prompts.iter().any(|p| p.name == "slow_tool"));
}

#[test]
fn plugin_echo_tool() {
    let cmd = test_plugin_command();
    let mut handle = PluginHandle::spawn(&cmd, "/tmp").unwrap();

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
            tau::types::ToolResultContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("");
    assert_eq!(text, "ECHO: hello world");
}

#[test]
fn plugin_slow_tool_streaming() {
    let cmd = test_plugin_command();
    let mut handle = PluginHandle::spawn(&cmd, "/tmp").unwrap();

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
            tau::types::ToolResultContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("");
    assert_eq!(text, "produced 5 lines");
}

#[test]
fn plugin_fail_tool() {
    let cmd = test_plugin_command();
    let mut handle = PluginHandle::spawn(&cmd, "/tmp").unwrap();

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
            tau::types::ToolResultContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("");
    assert_eq!(text, "intentional failure");
}

#[test]
fn plugin_before_agent_start_hook() {
    let cmd = test_plugin_command();
    let mut handle = PluginHandle::spawn(&cmd, "/tmp").unwrap();

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
    let mut handle = PluginHandle::spawn(&cmd, "/tmp").unwrap();

    // session_start is sent as a SessionStart request, not a Hook
    handle
        .send(&PluginRequest::SessionStart {
            cwd: "/home/test".into(),
            session_id: "s123".into(),
        })
        .unwrap();

    let msg = handle.read_message().unwrap();
    assert!(matches!(msg, PluginMessage::HookResult(_)));
}

#[test]
fn plugin_multiple_tool_calls() {
    let cmd = test_plugin_command();
    let mut handle = PluginHandle::spawn(&cmd, "/tmp").unwrap();

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
                tau::types::ToolResultContent::Text(t) => Some(t.text.as_str()),
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
        global: [("test".into(), PluginEntry { command: cmd })]
            .into_iter()
            .collect(),
        session: Default::default(),
        no_default_worker: false,
    };

    let mut manager = PluginManager::new(config);
    manager.load_global_plugins("/tmp");

    let session_id = "test-session";

    // Check tool schemas
    let schemas = manager.tool_schemas(session_id);
    assert_eq!(schemas.len(), 3);

    // Check tool prompts
    let prompts = manager.tool_prompts(session_id);
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

#[test]
fn plugin_wants_hook() {
    let cmd = test_plugin_command();
    let handle = PluginHandle::spawn(&cmd, "/tmp").unwrap();

    assert!(handle.wants_hook("before_agent_start"));
    assert!(handle.wants_hook("session_start"));
    assert!(handle.wants_hook("after_tool_result"));
    assert!(!handle.wants_hook("nonexistent_hook"));
}

#[test]
fn plugin_after_tool_result_hook() {
    let cmd = test_plugin_command();
    let mut handle = PluginHandle::spawn(&cmd, "/tmp").unwrap();

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
