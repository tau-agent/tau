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

### Plugin background ServerRequests

Currently plugins can only send ServerRequests during tool call handling
(inline request/response). Add support for plugins to send ServerRequests
at any time (background). The async plugin I/O already reads from plugins
continuously — the constraint is artificial. The server just needs to
handle ServerRequests from plugins outside of tool call context.

This enables global plugins to run background loops: watch for state
changes, spawn sessions, process queues — without needing an LLM-driven
controller session. Key enabler for the task system's scheduler and merge
queue.

### FireHook: let plugins fire hooks

Plugins can register to receive hooks, but only the server can fire them.
Add `Request::FireHook { name, data }` to the protocol so plugins can
broadcast hooks to other plugins via the ServerRequest tunnel. Server calls
`pm.call_hook(session_id, &name, &data)` on all plugins that registered for
that hook name, excluding the plugin that fired the hook. Small,
self-contained change. Prerequisite for task system hooks.

### Task system

Rust global plugin, backed by project-local SQLite (`.tau/tasks.db`).
Explicit `task_init` per project. Design (2026-04-03, revised 2026-04-04):

**Terminology:** "Task" not "ticket" — these are work items for agents.

**Architecture:** External global plugin subprocess. Receives `cwd` and
`session_id` on each tool call. Uses cwd to locate `.tau/tasks.db`.

**IDs:** Numeric `1, 2, 3, ...`. Message IDs are global auto-increment
integers (used by `task_message_edit`).

**States:**
```
draft -> ready -> active -> review -> approved -> merging -> done
                    ^                               |
                    |                               v (conflict/test fail)
                    +-------- rework <------------ failed
```
- `draft`: spec being iterated (human + agent adding messages)
- `ready`: spec complete, affected_files declared. Human decision.
- `active`: agent session claimed it (`task_assign` auto-transitions
  `ready -> active`)
- `review`: agent finished, awaiting human review
- `approved`: human approved, enters merge queue
- `merging`: merge queue is processing (rebase + checklist)
- `done`: merged to main
- `failed`: merge failed (conflict or checklist). Needs rework.

**Subtasks:** Tasks form a tree via `parent_id`. Parent doesn't advance to
`review` until all subtasks are `done`.

**Schema:**
- `tasks`: id, title, state, priority, parent_id (tree), tags (JSON array),
  affected_files (JSON array, advisory), assigned_session, branch,
  created_at, updated_at
- `task_messages`: ordered message list per task (first message = description).
  Individual messages editable. author = session_id or "user".
- `task_relations`: from_task, to_task, relation (depends_on, blocks, related)
- `task_sessions`: many-to-many link (task_id, session_id, role)
- `task_history`: audit trail of field changes

**Per-project checklist** (`.tau/checklist.toml`):
```toml
[[check]]
name = "clippy"
command = "cargo clippy --all-targets -- -D warnings"

[[check]]
name = "tests"
command = "cargo test"

[[check]]
name = "fmt"
command = "cargo fmt --check"
```
Agent runs checklist before `active -> review`. Merge queue runs it again
after rebase.

**Tools:**
- `task_init` — create `.tau/tasks.db` in cwd
- `task_create` title [priority] [parent] [tags]
- `task_get` id — full task: metadata + messages + relations + subtasks
- `task_list` [state] [parent] [tag] — filtered listing
- `task_update` id [title] [state] [priority] [tags] [affected_files]
- `task_assign` id [session_id] — assign + auto `ready -> active`
- `task_message` id content — append message
- `task_message_edit` id msg_id content — edit a message
- `task_relate` from to relation — add dependency/relation
- `task_search` query — full-text search across titles + messages

**Conflict-aware scheduling:** Controller session uses `affected_files` to
pick non-overlapping `ready` tasks for parallel execution. Advisory only —
agents may touch additional files during implementation. Git worktrees
provide isolation; merge conflicts are resolved at merge time.

**Merge queue:** Serialized processing of `approved` tasks:
1. Rebase task branch onto current main
2. Run project checklist
3. If pass: fast-forward main, task -> `done`
4. If fail: task -> `failed`, needs rework

**Hooks:** Fires `task_state_changed` via FireHook on state transitions.
Deferred until FireHook is implemented.

**Orchestration pattern:**
```
Controller session:
  1. task_list state=ready (all have affected_files)
  2. Greedy-pick non-conflicting batch by file overlap
  3. For each: session_spawn in git worktree
     Child: task_get -> work -> run checklist -> task_update state=review
  4. join_all, report to human
  5. Human reviews, approves -> tasks enter merge queue
  6. Merge queue processes serially: rebase, checklist, merge
```

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


