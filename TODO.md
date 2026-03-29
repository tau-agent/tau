# tau TUI TODOs

## UI Polish

- [x] **Working spinner**: match pi-agent's spinner style and "Working..." text exactly
- [ ] **Shift+Enter in input**: code is wired up but doesn't work — most terminals send Shift+Enter as plain Enter. Need to enable Kitty keyboard protocol in crossterm to distinguish them.
- [ ] **Soft line wrapping in input**: tui-textarea doesn't wrap long lines — they scroll horizontally. Need custom widget or upstream support.

## Session Continuity

- [x] **Restore messages on session resume**: fetches message history via GetMessages request on startup.

## Local TUI Settings

- [x] **Persist theme selection**: saved to `~/.config/tau/settings.toml` under `[tui]`. Loaded on startup, `/theme <name>` writes it.

## Multi-Client Sessions

- [x] **Multiple connections to one session**: server broadcasts stream events + user messages to all subscribed clients. TUI subscribes on startup via long-lived connection.
  - [ ] Locking: concurrent sends not yet coordinated (last writer wins)
- [x] **Steering messages**: Alt+Enter queues a message while agent is working. Auto-sends after current turn. Can type during streaming.
