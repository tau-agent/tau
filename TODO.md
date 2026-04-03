# TODO

## Recently completed

- **Plugin async refactor** — eliminated `handle_server_request_sync`; plugin→server
  requests are fully async via channel bridge. Fixed sync-handler bugs
  (WaitSessions tracking, QueueMessage wake, ArchiveSession cleanup).
- **E2E test suite overhaul** — mock provider with context-aware responses, mock
  tool calls, session dump/replay for regression testing (phases 1–3).

## In progress

### SSE idle watchdog + retry alignment

90-second idle timeout on SSE streams (replaces the old per-read timeout idea).
Retry parameters aligned with Claude Code defaults: `max_retries=10`,
`base=500ms`, `max=32s`, 25% jitter. Auth errors retry up to 3×.
Work in progress on the `retry-and-watchdog` branch.

## Open

### Messaging: await/reply

Fire-and-forget messaging (`session_message` tool) is done. What remains:

- `msg send session=<id> content=<text> await_reply=true` — block sender
  until the target replies
- `msg send reply=<msg_id> content=<text>` — reply to a pending message
- Inject reply-awaiting messages with a `msg_id` so the recipient can respond

### FireHook: let plugins fire hooks

Plugins can register to receive hooks, but only the server can fire them.
Add `Request::FireHook { name, data }` to the protocol so plugins can
broadcast hooks to other plugins via the ServerRequest tunnel. Server calls
`pm.call_hook(session_id, &name, &data)` on all plugins that registered for
that hook name. Small, self-contained change. Prerequisite for ticket system
hooks.

### Ticket system

Rust global plugin, backed by project-local SQLite (`.tau/tickets.db`).
Explicit `ticket_init` per project. Design (2026-04-03):

**Architecture:** External global plugin subprocess. Receives `cwd` and
`session_id` on each tool call. Uses cwd to locate `.tau/tickets.db`.

**IDs:** Numeric `1, 2, 3, ...`. Message IDs are global auto-increment integers
(used by `ticket_message_edit`).

**States:** `backlog → active → review → done` (v1).
`ticket_assign` auto-transitions `backlog → active`.

**Schema:**
- `tickets`: id, title, state, priority, parent_id (tree), tags (JSON array),
  created_at, updated_at, assigned_session
- `ticket_messages`: ordered message list per ticket (first message = description).
  Individual messages editable. author = session_id or "user".
- `ticket_relations`: from_ticket, to_ticket, relation (depends_on, blocks, related)
- `ticket_sessions`: many-to-many link (ticket_id, session_id, role)
- `ticket_history`: audit trail of field changes

**Tools:**
- `ticket_init` — create `.tau/tickets.db` in cwd
- `ticket_create` title [priority] [parent] [tags] — creates ticket + optional first message
- `ticket_get` id — full ticket: metadata + messages + relations
- `ticket_list` [state] [parent] [tag] — filtered listing
- `ticket_update` id [title] [state] [priority] [tags] — update fields
- `ticket_assign` id [session_id] — assign to current session (auto backlog→active)
- `ticket_message` id content — append message
- `ticket_message_edit` id msg_id content — edit a message
- `ticket_relate` from to relation — add dependency/relation
- `ticket_search` query — full-text search across titles + messages

**Hooks:** Fires `ticket_state_changed` via FireHook (see above) on state
transitions. Deferred until FireHook is implemented.

**Orchestration pattern:** Parent session calls `ticket_list state=backlog`,
spawns child sessions per ticket, children `ticket_assign` + do work +
`ticket_update state=review`, parent joins and reports.

### Remini injection timing

Revisit injection of changed memories. Currently they are shown too often.
Memories should be shown once at session creation, then only *new since then*
(excluding memories added within the session itself).

### Prompt caching

tau doesn't use Anthropic's prompt caching (`cache_control` headers). Claude
Code does. Should implement for cost savings on long conversations and
repeated system prompts.

### 529 overloaded error handling

Claude Code detects HTTP 529 / `overloaded_error` and falls back to an
alternate model after 3 consecutive 529s. tau doesn't handle 529 at all.
Add detection, retry with backoff, and optional model fallback.

### Session orchestration as global plugin

Move `session_*` tools out of `worker.rs` into a `tau plugin sessions`
subprocess (global plugin). Design exists in project memory. Decouples
orchestration from the core worker loop.

### worker2 integration

Async worker exists (`worker2.rs`) but isn't the default yet. Needs testing,
validation against the current worker, then replacement of `worker.rs`.
