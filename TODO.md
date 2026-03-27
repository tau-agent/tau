# tau TUI TODOs

## UI Polish

- [x] **Working spinner**: match pi-agent's spinner style and "Working..." text exactly
- [x] **Shift+Enter in input**: wired up (Alt+Enter also works). Note: Shift+Enter requires Kitty keyboard protocol support in the terminal — most terminals send it as plain Enter. Alt+Enter works universally.
- [ ] **Soft line wrapping in input**: tui-textarea doesn't wrap long lines — they scroll horizontally. Need custom widget or upstream support.

## Session Continuity

- [ ] **Restore messages on session resume**: when resuming a session (`tau chat -s <id>`), fetch and display previous messages so the conversation continues where it left off. Needs a protocol request to retrieve message history from the server.

## Local TUI Settings

- [x] **Persist theme selection**: saved to `~/.config/tau/settings.toml` under `[tui]`. Loaded on startup, `/theme <name>` writes it.

## Multi-Client Sessions

- [ ] **Multiple connections to one session**: currently breaks when two clients connect to the same session. This would be a great feature — multiple TUI instances viewing/interacting with the same session in real time.
  - Server needs to broadcast stream events to all connected clients on a session
  - Clients need to handle messages they didn't send (new message arrives while idle)
  - Locking/coordination: only one client should be able to send at a time, or handle concurrent sends gracefully
  - Could enable pair-programming / monitoring use cases
