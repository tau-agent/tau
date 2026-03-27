# tau TUI TODOs

## UI Polish

- [ ] **Working spinner**: match pi-agent's spinner style and "Working..." text exactly
- [ ] **Shift+Enter in input**: tui-textarea doesn't handle Shift+Enter for newline — wire it up or switch to a widget that does

## Session Continuity

- [ ] **Restore messages on session resume**: when resuming a session (`tau chat -s <id>`), fetch and display previous messages so the conversation continues where it left off. Needs a protocol request to retrieve message history from the server.

## Local TUI Settings

- [ ] **Persist theme selection**: save active theme name to `~/.config/tau/settings.toml` (local TUI setting, not per-session). Load on startup, `/theme <name>` writes it. Separate from provider/model config since it's client-side display preference.

## Multi-Client Sessions

- [ ] **Multiple connections to one session**: currently breaks when two clients connect to the same session. This would be a great feature — multiple TUI instances viewing/interacting with the same session in real time.
  - Server needs to broadcast stream events to all connected clients on a session
  - Clients need to handle messages they didn't send (new message arrives while idle)
  - Locking/coordination: only one client should be able to send at a time, or handle concurrent sends gracefully
  - Could enable pair-programming / monitoring use cases
