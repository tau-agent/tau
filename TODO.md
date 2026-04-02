# TODO

## Bugs (sync handler)

- **WaitSessions doesn't track `waited_sessions`**: the synchronous handler
  path doesn't record which sessions are being waited on, causing duplicate
  parent notifications when a child completes.
- **QueueMessage can't resume idle sessions**: messages queued through the
  sync handler don't wake up idle target sessions.
- **ArchiveSession skips plugin cleanup and busy check**: the sync handler
  archives the session without destroying plugins or checking whether it is
  still busy.

## Plugin async refactor

Eliminate `handle_server_request_sync` (~335 lines of duplicated logic). Make
plugin→server requests async via a channel bridge so the plugin thread never
blocks the threadpool. This fixes the three sync-handler bugs above plus the
issue of WaitSessions blocking a threadpool thread. Plan exists in project
memory.

## E2E test suite overhaul

- Mock providers with context-aware responses
- Mock tool calls
- Session dump/replay for regression testing
- Analysis exists in project memory

## Messaging: await/reply

Fire-and-forget messaging (`session_message` tool) is done. What remains:

- `msg send session=<id> content=<text> await_reply=true` — block sender
  until the target replies
- `msg send reply=<msg_id> content=<text>` — reply to a pending message
- Inject reply-awaiting messages with a `msg_id` so the recipient can respond

## SSE stream read timeout

ureq timeouts only cover connect/send/TTFB phases. Once SSE streaming starts,
a silently dead connection (no data, no TCP RST) will block
`BufReader::lines()` forever. ureq 3.x has no per-read timeout. Options: wrap
the reader with a custom `Read` impl that enforces a deadline per `read()`
call, or switch to a lower-level HTTP client that exposes the socket. A
reasonable per-read timeout would be ~120 s (Anthropic sends SSE keepalives).

## Ticket system

Needs design. Rough sketch:

- Rust tau plugin, backed by SQLite
- Global plugin scoped per project
- Tool functions: `ticket create`, `ticket assign`, `ticket get`, `ticket edit`
- States: backlog → planning → designing → design review → implementation → done
  (with back-edges for rework)

## Remini injection timing

Revisit injection of changed memories. Currently they are shown too often.
Memories should be shown once at session creation, then only *new since then*
(excluding memories added within the session itself).
