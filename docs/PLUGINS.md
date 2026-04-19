# tau Plugin API

Plugins are external processes that communicate with tau via JSON-lines on
stdin/stdout. Any language can implement a plugin — there is no SDK
requirement, just a JSON wire protocol.

A plugin can:

- Register **tools** that the LLM can call (alongside the built-in `bash`,
  `read`, `edit`, `write`).
- Subscribe to **hooks** to observe and modify the agent's behavior at
  specific points (`before_agent_start`, `after_tool_result`, `session_start`).
- Register **slash commands** as metadata (currently dead — see
  [Commands](#commands) below).
- Call back into the tau server via the **`ServerRequest` tunnel** to create
  child sessions, queue messages between sessions, fire hooks, archive
  sessions, execute tools directly, etc.

## Configuration

Plugins are configured in `~/.config/tau/plugins.toml` (or
`$XDG_CONFIG_HOME/tau/plugins.toml`). There are two scopes:

```toml
# Optional: prefix prepended to every session-plugin command (e.g. for
# sandboxing). The literal "{cwd}" is substituted with the session's working
# directory before spawning. Only applies to session plugins, not global ones.
session_prefix = ["sandbox", "run", "--root", "{cwd}", "--"]

# Optional: idle timeout for session plugins (seconds). After this many
# seconds of inactivity with no connected subscribers, session plugins
# receive an `Idle` notification and may exit. Default: 30. Set to 0 to
# disable.
idle_timeout_secs = 30

# Global plugins: spawned once at server start, shared across all sessions.
# They live for the lifetime of the server.
[global.tasks-extra]
command = ["python3", "/path/to/global_plugin.py"]
env = { LOG_LEVEL = "info" }

# Session plugins: spawned per session, killed when the session ends or
# goes idle. The session_prefix above is prepended to each command.
[session.lsp]
command = ["node", "/path/to/lsp-plugin.js"]
```

The `command` array is passed directly to the OS process spawner. The
optional `env` map adds environment variables to the plugin subprocess.

### Global vs session plugins

| Aspect | Global plugins | Session plugins |
|---|---|---|
| Spawned | Once at server start | Once per session, on first use |
| Lifetime | Server lifetime | Session lifetime (or until idle-exit) |
| Shared state | Yes (across sessions) | No |
| Receives `Idle` | No | Yes |
| Receives `SessionStart` | Yes (once per session) | Yes (once for its session) |
| Background `ServerRequest` | Yes | No (only during a tool call) |
| Sandbox prefix | No | Yes (`session_prefix`) |
| Auto-respawn | No | Yes (on next tool call after idle-exit) |

The built-in `tasks` plugin is auto-spawned as a global plugin if not
already configured. The default session plugin (when none are configured)
is the built-in `worker`, which runs the standard tools (`bash`, `read`,
`edit`, `write`).

## Wire protocol

Communication is **JSON-lines**: one JSON object per line, terminated by
`\n`, on stdin (tau → plugin) and stdout (plugin → tau). Stderr goes to
the tau server's stderr — use it for debug logging.

All message types use a `"type"` discriminator with `snake_case` variants
(e.g. `"tool_call"`, `"server_request"`).

### Lifecycle

```
Plugin process starts
   │
   ├──► Plugin sends Register (REQUIRED, must be first message)
   │
   │    ◄── tau sends SessionStart   (once per session, only if plugin
   │                                  registered the "session_start" hook)
   │    ◄── tau sends Hook(before_agent_start)
   │ ──► Plugin sends HookResult
   │    ◄── tau sends ToolCall
   │ ──► Plugin sends OutputDelta(s)  (optional, streamed)
   │ ──► Plugin sends ToolResult
   │    ◄── tau sends Hook(after_tool_result)
   │ ──► Plugin sends HookResult { tool_result_append: "..." }
   │    ...
   │
   │    ◄── tau sends Idle           (session plugins only, on inactivity)
   │ ──► Plugin may exit, or continue running
   │
   ▼
EOF on stdin → plugin should exit
```

In addition, plugins can spontaneously initiate `ServerRequest` messages
(see [Server requests](#server-requests-plugin--tau)).

## Registration

The plugin's first message on stdout **must** be a `register`:

```json
{
  "type": "register",
  "name": "my-plugin",
  "tools": [...],
  "hooks": [...],
  "commands": [...]
}
```

| Field | Required | Description |
|---|---|---|
| `name` | yes | Plugin name (used in logs and for `FireHook` exclusion) |
| `tools` | no | Tool definitions (default: `[]`) |
| `hooks` | no | Names of hooks the plugin wants to receive (default: `[]`) |
| `commands` | no | Slash commands provided by this plugin (default: `[]`) |

If the plugin's first message is anything other than `register`, tau will
kill it and log a registration failure.

## Tools

Register tools to make them callable by the LLM alongside built-in tools.

### Tool definition

```json
{
  "name": "search_docs",
  "description": "Full-text search the project documentation.",
  "parameters": {
    "type": "object",
    "properties": {
      "query":  {"type": "string",  "description": "Search query"},
      "limit":  {"type": "integer", "description": "Max results"}
    },
    "required": ["query"]
  },
  "prompt_snippet": "Search project docs by keyword",
  "prompt_guidelines": [
    "Prefer search_docs over grepping for documentation queries.",
    "Use limit=5 unless you need more."
  ]
}
```

| Field | Required | Description |
|---|---|---|
| `name` | yes | Tool name (used in tool calls) |
| `description` | yes | Description shown to the LLM |
| `parameters` | yes | JSON Schema for the tool's arguments |
| `prompt_snippet` | no | One-liner shown in the system prompt's "Available tools:" list. **If omitted, the tool is callable but won't appear in the system prompt.** |
| `prompt_guidelines` | no | Extra bullet points appended to the system prompt's "Guidelines:" section |

The system prompt is built dynamically from the per-session set of tools
(global + session). Both `prompt_snippet` and `prompt_guidelines` are
de-duplicated with the always-on guidelines.

### Handling tool calls

When the LLM calls your tool, tau sends:

```json
{
  "type": "tool_call",
  "tool_call_id": "tc_abc123",
  "name": "search_docs",
  "arguments": {"query": "compaction", "limit": 5},
  "cwd": "/home/user/project",
  "session_id": "s42"
}
```

| Field | Optional | Description |
|---|---|---|
| `tool_call_id` | no | Echo this back in `tool_result` and any `output_delta` |
| `name` | no | Tool name (matches what was registered) |
| `arguments` | no | JSON arguments matching the tool's `parameters` schema |
| `cwd` | yes | Working directory of the calling session |
| `session_id` | yes | ID of the calling session (useful for `ServerRequest` callbacks) |

The plugin **must** eventually respond with a single `tool_result` for the
matching `tool_call_id`. It may emit any number of `output_delta` messages
beforehand, and any number of `server_request` messages.

#### Simple response

```json
{
  "type": "tool_result",
  "tool_call_id": "tc_abc123",
  "content": [{"type": "text", "text": "found 3 matches"}],
  "is_error": false
}
```

#### Streaming output

For long-running tools, send `output_delta` messages before the final
`tool_result`. The TUI displays these incrementally:

```json
{"type": "output_delta", "tool_call_id": "tc_abc123", "text": "scanning file 1/100..."}
{"type": "output_delta", "tool_call_id": "tc_abc123", "text": "scanning file 2/100..."}
{"type": "tool_result",  "tool_call_id": "tc_abc123", "content": [{"type": "text", "text": "done"}], "is_error": false}
```

The TUI shows the first 10 delta lines, then collapses additional lines
to a `... N more lines` counter.

#### Error response

Set `is_error: true` — the LLM sees the error and can retry or report it:

```json
{
  "type": "tool_result",
  "tool_call_id": "tc_abc123",
  "content": [{"type": "text", "text": "connection refused"}],
  "is_error": true
}
```

### Tool result content types

The `content` array supports two variants (matching `ToolResultContent`
in `crates/tau-agent-base/src/types.rs`):

```json
{"type": "text",  "text": "plain text content"}
{"type": "image", "data": "<base64>", "mime_type": "image/png"}
```

## Hooks

Hooks let plugins observe and modify the agent's behavior at specific
points. Register the hook names you care about in the `hooks` array of
your `register` message.

The server fires exactly **two** named hooks via the `Hook` message
(`before_agent_start` and `after_tool_result`). The third "hook"
(`session_start`) is delivered as a dedicated `SessionStart` request
variant, not as a `Hook` — but a plugin still subscribes to it by listing
`"session_start"` in its `hooks` array. Plugins themselves can also fire
arbitrary hook names via `ServerRequest::FireHook` — these arrive as
`Hook` messages on every other plugin that subscribes to that name.

### `session_start`

Sent **once per session**, before the first interaction, if and only if
the plugin registered `"session_start"` in its `hooks` array.

```json
{"type": "session_start", "cwd": "/home/user/project", "session_id": "s42"}
```

Respond with an empty `hook_result` (the response is required so the
server can synchronize):

```json
{"type": "hook_result"}
```

### `before_agent_start`

Sent before each LLM turn. The plugin can inject a context message and/or
override the system prompt for that turn.

```json
{
  "type": "hook",
  "name": "before_agent_start",
  "data": {
    "prompt": "what does foo do?",
    "system_prompt": "<current system prompt>",
    "session_id": "s42",
    "message_count": 7
  }
}
```

Respond with any combination of:

```json
{
  "type": "hook_result",
  "message": {"content": "Context to inject before the LLM turn."},
  "system_prompt": "Replacement system prompt for this turn (optional)."
}
```

| Field | Optional | Description |
|---|---|---|
| `message.content` | yes | Injected as a user message before the LLM call |
| `system_prompt` | yes | Replaces the session's system prompt for this turn |

If you have nothing to inject, return `{"type": "hook_result"}`.

### `after_tool_result`

Fired after every tool execution (built-in or plugin). The plugin can
append text to the tool result that the LLM will see — typical use is
LSP plugins appending diagnostics after edits.

```json
{
  "type": "hook",
  "name": "after_tool_result",
  "data": {
    "tool_name": "edit",
    "arguments": {"path": "src/main.rs", "edits": [...]},
    "content": "Applied 1 edit to src/main.rs",
    "is_error": false
  }
}
```

Respond with optional `tool_result_append`:

```json
{
  "type": "hook_result",
  "tool_result_append": "\n<diagnostics>\nsrc/main.rs:42: unused variable\n</diagnostics>"
}
```

| Field | Optional | Description |
|---|---|---|
| `tool_result_append` | yes | Text appended to the tool result the LLM sees |

### Custom hooks via `FireHook`

A plugin can broadcast a hook to all *other* plugins by sending
`ServerRequest::FireHook { name, data }` (see
[Server requests](#server-requests-plugin--tau)). Each subscribed plugin
receives a normal `Hook` message and responds with `HookResult`. The
firing plugin does not receive the hook back (it's automatically excluded).

This is the plugin-to-plugin communication mechanism — for example, the
`tasks` plugin fires a `task_state_changed` hook, and any plugin that
subscribes to it can react.

## Idle notifications

After a configurable inactivity period (`idle_timeout_secs`, default 30s)
with no connected subscribers, **session plugins** receive an `Idle`
notification on stdin:

```json
{"type": "idle"}
```

The plugin may:

- **Exit**: simply call `sys.exit(0)`. tau will detect the exit and
  respawn the plugin lazily on the next tool call (the existing
  `Register` is preserved across respawns).
- **Stay alive**: ignore the message and keep running.

Global plugins **never** receive `Idle` — they live for the server's
lifetime.

## Server requests (plugin → tau)

Plugins can call back into the tau server through the **ServerRequest
tunnel**. This is how a plugin creates child sessions, queues messages,
archives sessions, fires hooks, etc.

The flow is request/response correlated by `request_id`:

```
Plugin                                tau
  │                                    │
  │ ── PluginMessage::ServerRequest ─► │
  │      { request_id, request }       │
  │                                    │ (handle request)
  │ ◄─ PluginRequest::ServerResponse ──│
  │      { request_id, response }      │
  │                                    │
```

The plugin **chooses** the `request_id` (any unique string) and matches
it to the `ServerResponse` it receives back.

```json
// plugin → tau
{
  "type": "server_request",
  "request_id": "req-1",
  "request": {
    "type": "create_session",
    "cwd": "/home/user/project",
    "child_budget": 16,
    "tagline": "background indexer",
    "auto_archive": true,
    "notify_parent": false
  }
}
```

```json
// tau → plugin
{
  "type": "server_response",
  "request_id": "req-1",
  "response": {
    "type": "session_created",
    "session_id": "s99"
  }
}
```

### When can a plugin send a ServerRequest?

- **During a tool call** (any plugin, global or session). The
  `ServerRequest` is read inline by the same loop that's waiting for
  `ToolResult`.
- **In the background, at any time** (global plugins only). When tau
  starts, it spawns a background reader/writer task pair for each global
  plugin so that `ServerRequest` messages arriving outside any tool call
  context are still handled. Background requests use an empty
  `session_id` context.

Session plugins can only send `ServerRequest` messages while a tool call
is active.

### Available request types

The `request` field accepts the same `Request` enum used by clients
(see `crates/tau-agent-base/src/protocol.rs`). The variants currently
supported in plugin context include:

| Variant | Purpose |
|---|---|
| `chat` | Send a chat message in another session |
| `create_session` | Create a (possibly child) session |
| `get_session_info` | Look up a session's metadata |
| `get_messages` | Fetch full message history of a session |
| `list_sessions` | List sessions (optionally including archived) |
| `archive_session` | Archive a session and its subtree |
| `restore_session` | Un-archive a session and its subtree |
| `cancel_chat` | Cancel a running agent loop |
| `wait_sessions` / `wait_any_sessions` | Block until other sessions complete |
| `queue_message` | Send a message to another session (optionally awaiting a reply) |
| `reply_to_message` | Reply to an awaited `queue_message` |
| `fire_hook` | Broadcast a hook to all other plugins |
| `execute_tool` | Run a tool on a specific session without an LLM loop |

Requests outside this set return `Response::Error { message: "request not supported in plugin context" }`.

### Child budget

`create_session` accepts a `child_budget: u32` field that caps the number
of direct children a session is allowed to spawn. Each `create_session`
with a `parent_id` increments the parent's child count by 1; if the count
exceeds the parent's budget, the request fails with an error.

The conventional default used by tau's own task tooling (worker, tasks
plugin) is **16**, but the wire-level default if you omit the field is
`0`. Plugins that spawn child sessions should pass an explicit value.

When a session has `child_budget == 0`, the session-orchestration tools
(everything whose name starts with `session_`) are filtered out of the
LLM's tool list and system prompt for that session — meaning the LLM
literally cannot create children when the budget is zero.

### Concurrent `ToolCall` while waiting for a `ServerResponse`

While a plugin is waiting for a `ServerResponse`, the server may dispatch
a concurrent `ToolCall` to the same plugin (e.g. another session calls
one of the plugin's tools at the same time). If you don't handle these,
the calling session will hang forever waiting for a `ToolResult` it
never gets.

**Recommended pattern**: in the read loop that waits for your
`ServerResponse`, also recognise `ToolCall` messages and immediately
answer them with an error `ToolResult`. The LLM can then retry.

```python
def server_request(req):
    request_id = f"req-{next_id()}"
    send({"type": "server_request", "request_id": request_id, "request": req})
    while True:
        msg = recv()
        if msg["type"] == "server_response" and msg["request_id"] == request_id:
            return msg["response"]
        if msg["type"] == "tool_call":
            # Concurrent tool call — answer with an error so the caller
            # doesn't hang while we're busy with our server request.
            send({
                "type": "tool_result",
                "tool_call_id": msg["tool_call_id"],
                "content": [{"type": "text", "text": "plugin busy, retry"}],
                "is_error": True,
            })
            continue
        # Ignore everything else (stale ServerResponse, etc.)
```

For Rust reference implementations, see
`crates/tau-agent-plugin-tasks/src/tasks_scheduler.rs` and
`crates/tau-agent-plugin-tasks/src/tasks_merge.rs` (both delegate to
`tau_agent_plugin::tunnel::server_request`).

## Commands

The `commands` field on `register` lets a plugin attach a list of slash
commands as metadata:

```json
{
  "type": "register",
  "name": "my-plugin",
  "commands": [
    {"name": "my-cmd", "description": "Does something useful"}
  ]
}
```

**⚠ This field is currently dead metadata.** As of today:

- The TUI's `/help` output is a hardcoded string in
  `crates/tau-agent-tui/src/app.rs` that does not include plugin-registered
  commands.
- There is no execution routing — typing `/my-cmd` in the TUI will not
  invoke the plugin.
- The only consumer of `PluginManager::commands()` in the codebase is a
  unit test.

The field is preserved on the wire and stored on the plugin handle for
future use, but **plugin authors should not rely on it being surfaced
anywhere user-visible**. To expose plugin functionality to users today,
register a **tool** instead — tools appear in the LLM's tool list and
can be invoked through normal conversation.

## Complete example

A minimal Python plugin that provides a `greet` tool, an
`after_tool_result` hook, and reacts to `idle`:

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

# 1. Registration (must be the first message)
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
    }],
    "hooks": ["after_tool_result", "session_start"]
})

# 2. Main loop
while True:
    msg = recv()
    t = msg["type"]

    if t == "tool_call" and msg["name"] == "greet":
        name = msg["arguments"].get("name", "world")
        send({
            "type": "tool_result",
            "tool_call_id": msg["tool_call_id"],
            "content": [{"type": "text", "text": f"Hello, {name}!"}],
            "is_error": False,
        })

    elif t == "session_start":
        # Empty hook_result to ack
        send({"type": "hook_result"})

    elif t == "hook" and msg["name"] == "after_tool_result":
        # Optionally append text to the tool result
        send({"type": "hook_result"})

    elif t == "idle":
        # Optional: exit on idle to free resources. tau will respawn us
        # lazily on the next tool call.
        sys.exit(0)
```

Configure it as a session plugin:

```toml
# ~/.config/tau/plugins.toml
[session.greeter]
command = ["python3", "/path/to/greeter.py"]
```

…or as a global plugin (shared across sessions, no `Idle` notifications):

```toml
[global.greeter]
command = ["python3", "/path/to/greeter.py"]
```

## Debugging

Plugin stderr is forwarded to the tau server's stderr — use it freely
for logging:

```python
print("debug: processing tool call", file=sys.stderr)
```

View the server's stderr by running it in the foreground:

```sh
tau server start --foreground
```

## Lifecycle summary

- **Global plugins** are spawned at server start and live for the server's
  lifetime. If a global plugin crashes, its tools/hooks become unavailable
  (no auto-restart).
- **Session plugins** are spawned on first use of a session, killed when
  the session ends, archived, or reloaded via `reload_plugins`.  If a
  session plugin exits in response to `Idle`, tau respawns it lazily on
  the next tool call (its `Register` is preserved across respawns).
- On server shutdown, all plugins receive EOF on stdin and should exit
  gracefully.

## Reference

- `crates/tau-agent-base/src/plugin_protocol.rs` — wire types
  (`PluginRequest`, `PluginMessage`, `PluginRegistration`, etc.).
- `crates/tau-agent-lib/src/plugin.rs` — server-side plugin management.
- `crates/tau-agent-base/src/protocol.rs` — `Request` / `Response` enums
  used inside `ServerRequest`.
- `crates/tau-agent-engine/src/system_prompt.rs` — `ToolPrompt`,
  `prompt_snippet`, `prompt_guidelines` weaving.
- `crates/tau-agent-lib/src/server/` — global plugin background tasks,
  hook firing, tool dispatch.
- `crates/tau-agent-plugin-tasks/src/tasks_scheduler.rs` — reference
  implementation of a Rust plugin that uses the `ServerRequest` tunnel
  and handles concurrent `ToolCall` messages mid-request.
- `tests/test_plugin.py` — minimal Python plugin exercising tools,
  streaming, hooks, and errors.
- `tau-remini` (sibling project) — a real-world Rust plugin; its
  consumer-side `protocol.rs` is a useful source of truth.
