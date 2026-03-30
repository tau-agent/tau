# tau Plugin API

Plugins are external processes that communicate with tau via JSON-lines on stdin/stdout. Any language can implement a plugin.

## Configuration

Register plugins in `~/.config/tau/plugins.toml`:

```toml
[plugins.my-plugin]
command = ["python3", "/path/to/my_plugin.py"]

[plugins.remini]
command = ["node", "/path/to/remini-plugin.js"]
```

Each plugin is spawned by the tau server on startup. The `command` array is passed to the OS process spawner.

## Protocol

Communication is JSON-lines (one JSON object per line, terminated by `\n`) on stdin (tau → plugin) and stdout (plugin → tau). Stderr goes to the tau server's stderr (use it for debug logging).

### Startup: Registration

On startup, the plugin MUST send a single `register` message on stdout:

```json
{
  "type": "register",
  "name": "my-plugin",
  "tools": [...],
  "hooks": [...],
  "commands": [...]
}
```

All fields except `type` and `name` are optional (default to empty arrays).

### Message Flow

```
Plugin starts → sends Register
                                    ← tau sends SessionStart
                                    ← tau sends Hook (before_agent_start)
Plugin sends HookResult →
                                    ← tau sends ToolCall
Plugin sends OutputDelta(s) →
Plugin sends ToolResult →
                                    ← tau sends ToolCall
Plugin sends ToolResult →
                                    ...
```

## Tools

Register tools to make them available to the LLM alongside built-in tools (bash, read, edit, write).

### Tool Definition

```json
{
  "name": "my_tool",
  "description": "What this tool does (shown to the LLM).",
  "parameters": {
    "type": "object",
    "properties": {
      "query": {
        "type": "string",
        "description": "Search query"
      },
      "limit": {
        "type": "integer",
        "description": "Max results"
      }
    },
    "required": ["query"]
  },
  "prompt_snippet": "One-line description for the Available Tools list",
  "prompt_guidelines": [
    "Guideline bullet for the system prompt Guidelines section.",
    "Another guideline."
  ]
}
```

| Field | Required | Description |
|---|---|---|
| `name` | yes | Tool name (used in tool calls) |
| `description` | yes | Full description for the LLM |
| `parameters` | yes | JSON Schema for the tool's arguments |
| `prompt_snippet` | no | One-liner for the "Available tools:" list in the system prompt. If omitted, tool is not listed (but still callable). |
| `prompt_guidelines` | no | Extra bullet points for the "Guidelines:" section in the system prompt. |

### Handling Tool Calls

When the LLM calls your tool, tau sends:

```json
{
  "type": "tool_call",
  "tool_call_id": "tc_abc123",
  "name": "my_tool",
  "arguments": {"query": "search term", "limit": 10}
}
```

#### Simple Response

Return a single result:

```json
{
  "type": "tool_result",
  "tool_call_id": "tc_abc123",
  "content": [{"type": "text", "text": "result text here"}],
  "is_error": false
}
```

#### Streaming Response

For long-running tools, send output deltas before the final result. The TUI displays these lines incrementally:

```json
{"type": "output_delta", "tool_call_id": "tc_abc123", "text": "Processing item 1..."}
{"type": "output_delta", "tool_call_id": "tc_abc123", "text": "Processing item 2..."}
{"type": "output_delta", "tool_call_id": "tc_abc123", "text": "Processing item 3..."}
{"type": "tool_result", "tool_call_id": "tc_abc123", "content": [{"type": "text", "text": "processed 3 items"}], "is_error": false}
```

The TUI shows the first 10 delta lines, then `... N more lines` with a live counter.

#### Error Response

Set `is_error: true` — the LLM will see the error and can retry or report it:

```json
{
  "type": "tool_result",
  "tool_call_id": "tc_abc123",
  "content": [{"type": "text", "text": "connection refused"}],
  "is_error": true
}
```

### Content Types

The `content` array in `tool_result` supports:

```json
{"type": "text", "text": "plain text content"}
{"type": "image", "data": "<base64>", "mime_type": "image/png"}
```

## Hooks

Hooks let plugins observe and modify the agent's behavior at specific points.

Register hooks by name in the `hooks` array:

```json
{
  "type": "register",
  "name": "my-plugin",
  "hooks": ["session_start", "before_agent_start"]
}
```

### `session_start`

Sent when a session is created or resumed:

```json
{"type": "session_start", "cwd": "/home/user/project", "session_id": "abc123"}
```

Respond with an empty hook result:

```json
{"type": "hook_result"}
```

### `before_agent_start`

Sent before each LLM turn. The plugin can inject a context message and/or modify the system prompt:

```json
{"type": "hook", "name": "before_agent_start", "data": {"prompt": "user's message", "system_prompt": "current system prompt"}}
```

Respond with any combination of:

```json
{
  "type": "hook_result",
  "message": {"content": "Context to inject before the LLM turn."},
  "system_prompt": "Replacement system prompt (optional)."
}
```

| Field | Optional | Description |
|---|---|---|
| `message` | yes | Injected as a system/context message before the LLM call |
| `system_prompt` | yes | Replaces the session's system prompt for this turn |

If you have nothing to inject, return `{"type": "hook_result"}`.

### `after_tool_result`

Sent after every tool execution (built-in or plugin). The plugin can append content to the tool result (e.g., LSP diagnostics).

```json
{"type": "hook", "name": "after_tool_result", "data": {
  "tool_name": "edit",
  "arguments": {"path": "src/main.rs", "edits": [...]},
  "content": "Applied 1 edit to src/main.rs",
  "is_error": false
}}
```

Respond with optional text to append:

```json
{
  "type": "hook_result",
  "tool_result_append": "\n<file_diagnostics>\nError: src/main.rs:42:5 unused variable\n</file_diagnostics>"
}
```

| Field | Optional | Description |
|---|---|---|
| `tool_result_append` | yes | Text appended to the tool result content (LLM sees it) |

This hook enables LSP plugins to automatically append diagnostics after file edits.

## Commands

Register slash commands that users can invoke from the TUI:

```json
{
  "type": "register",
  "name": "my-plugin",
  "commands": [
    {"name": "my-cmd", "description": "Does something useful"}
  ]
}
```

The command appears in `/help`. (Command execution routing is planned but not yet implemented — currently commands are informational only.)

## Complete Example

A minimal Python plugin that provides a `greet` tool:

```python
#!/usr/bin/env python3
import json, sys

def send(msg):
    sys.stdout.write(json.dumps(msg) + "\n")
    sys.stdout.flush()

def recv():
    line = sys.stdin.readline()
    if not line:
        sys.exit(0)
    return json.loads(line.strip())

# Register
send({
    "type": "register",
    "name": "greeter",
    "tools": [{
        "name": "greet",
        "description": "Greet someone by name.",
        "parameters": {
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "Name to greet"}
            },
            "required": ["name"]
        },
        "prompt_snippet": "Greet someone by name"
    }]
})

# Handle requests
while True:
    msg = recv()
    if msg["type"] == "tool_call" and msg["name"] == "greet":
        name = msg["arguments"].get("name", "world")
        send({
            "type": "tool_result",
            "tool_call_id": msg["tool_call_id"],
            "content": [{"type": "text", "text": f"Hello, {name}!"}],
            "is_error": False
        })
    elif msg["type"] in ("hook", "session_start"):
        send({"type": "hook_result"})
```

Configure it:

```toml
# ~/.config/tau/plugins.toml
[plugins.greeter]
command = ["python3", "/path/to/greeter.py"]
```

Then in tau:

```
> greet Alice
Hello, Alice!
```

## Debugging

Plugin stderr goes to the tau server's stderr. Use it for logging:

```python
print("debug: processing request", file=sys.stderr)
```

View with `tau server start --foreground`.

## Lifecycle

- Plugins are spawned when the tau server starts
- They stay alive for the lifetime of the server
- If a plugin crashes, its tools/hooks become unavailable (no auto-restart yet)
- On server shutdown, plugins receive EOF on stdin and should exit

## Reference

See `tests/test_plugin.py` for a complete example exercising all features (tools, streaming, hooks, errors).

See `crates/tau/src/plugin.rs` for the Rust implementation and protocol types.
