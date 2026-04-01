#!/usr/bin/env python3
"""Test plugin for tau's subprocess plugin system.

Registers:
- Tool "echo_tool": echoes back its arguments with a prefix
- Tool "slow_tool": sends streaming output deltas, then completes
- Tool "fail_tool": always returns an error
- Hook "before_agent_start": injects a context message
- Hook "session_start": records the session info
- Command "test-cmd": a test slash command

Protocol: JSON-lines on stdin/stdout.
"""

import json
import sys
import time


def send(msg):
    line = json.dumps(msg) + "\n"
    sys.stdout.write(line)
    sys.stdout.flush()


def recv():
    line = sys.stdin.readline()
    if not line:
        sys.exit(0)
    return json.loads(line.strip())


# --- Registration ---
send({
    "type": "register",
    "name": "test-plugin",
    "tools": [
        {
            "name": "echo_tool",
            "description": "Echoes back the input with a prefix.",
            "parameters": {
                "type": "object",
                "properties": {
                    "message": {"type": "string", "description": "Message to echo"}
                },
                "required": ["message"]
            },
            "prompt_snippet": "Echo back input for testing",
            "prompt_guidelines": ["Use echo_tool for testing purposes."]
        },
        {
            "name": "slow_tool",
            "description": "Produces streaming output line by line.",
            "parameters": {
                "type": "object",
                "properties": {
                    "lines": {"type": "integer", "description": "Number of lines to produce"}
                },
                "required": ["lines"]
            },
            "prompt_snippet": "Produce streaming output for testing"
        },
        {
            "name": "fail_tool",
            "description": "Always fails with an error.",
            "parameters": {
                "type": "object",
                "properties": {}
            }
        }
    ],
    "hooks": ["before_agent_start", "session_start", "after_tool_result"],
    "commands": [
        {"name": "test-cmd", "description": "A test command"}
    ]
})

# --- Main loop ---
session_info = None

while True:
    try:
        msg = recv()
    except (json.JSONDecodeError, EOFError):
        break

    msg_type = msg.get("type")

    if msg_type == "session_start":
        session_info = {"cwd": msg.get("cwd"), "session_id": msg.get("session_id")}
        send({"type": "hook_result"})

    elif msg_type == "hook":
        hook_name = msg.get("name")
        if hook_name == "before_agent_start":
            send({
                "type": "hook_result",
                "message": {
                    "content": "[TEST PLUGIN] Context injected by test plugin."
                }
            })
        elif hook_name == "after_tool_result":
            data = msg.get("data", {})
            tool_name = data.get("tool_name", "")
            send({
                "type": "hook_result",
                "tool_result_append": f"\n[TEST DIAGNOSTICS for {tool_name}]"
            })
        else:
            send({"type": "hook_result"})

    elif msg_type == "tool_call":
        tool_name = msg.get("name")
        tool_call_id = msg.get("tool_call_id", "")
        args = msg.get("arguments", {})

        if tool_name == "echo_tool":
            message = args.get("message", "")
            send({
                "type": "tool_result",
                "tool_call_id": tool_call_id,
                "content": [{"type": "text", "text": f"ECHO: {message}"}],
                "is_error": False
            })

        elif tool_name == "slow_tool":
            n = args.get("lines", 3)
            for i in range(1, n + 1):
                send({
                    "type": "output_delta",
                    "tool_call_id": tool_call_id,
                    "text": f"line {i}"
                })
                time.sleep(0.01)  # small delay for streaming
            send({
                "type": "tool_result",
                "tool_call_id": tool_call_id,
                "content": [{"type": "text", "text": f"produced {n} lines"}],
                "is_error": False
            })

        elif tool_name == "fail_tool":
            send({
                "type": "tool_result",
                "tool_call_id": tool_call_id,
                "content": [{"type": "text", "text": "intentional failure"}],
                "is_error": True
            })

        else:
            send({
                "type": "tool_result",
                "tool_call_id": tool_call_id,
                "content": [{"type": "text", "text": f"unknown tool: {tool_name}"}],
                "is_error": True
            })
    elif msg_type == "idle":
        # Plugin can choose to exit on idle. For testing, we exit.
        sys.exit(0)
    else:
        # Unknown message type, ignore
        pass
