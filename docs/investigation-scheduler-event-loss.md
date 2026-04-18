# Investigation: scheduler intermittently fails to auto-dispatch

**Task:** #534
**Status:** Investigation complete; two concrete bugs identified (no fix applied — follow-up task(s) should fix).

---

## TL;DR

The observed symptom ("task stuck in `ready` / `planning` with stale session
id, no worker dispatched") is explained by **two concrete event-loss
paths**, both of which can drop `ScheduleNeeded` without any retry:

1. **Hook delivered while the tasks plugin is mid-ToolCall is silently
   skipped.** When the server processes a `FireHook` request, it iterates
   the `global_plugins` list and calls `plugin.call_hook()` on each plugin
   that registered for that hook. But while a plugin is executing a
   `ToolCall`, the server removes it from `global_plugins` via
   `take_tool_plugin()` (see `crates/tau-agent/src/server/agent_runner.rs`
   ~line 45 and `crates/tau-agent/src/plugin.rs::take_tool_plugin`). During
   that window `call_hook_excluding()` simply does not see the plugin, so
   the `task_state_changed` hook is **never delivered** for that event.
   No error, no retry, no queued event on the plugin side.

2. **`run_schedule_pass`'s dispatch-failure revert path does not
   re-queue a retry.** When `tasks_scheduler::dispatch()` returns `Err`
   for a ready-state task, `run_schedule_pass` reverts the task to
   `ready` (correct) but does **not** push a fresh `ScheduleNeeded`
   event into `pending_events`. The task now sits in `ready` with no
   scheduled pass coming. The next trigger is whatever external event
   happens to fire a schedule pass for the same project — which can be a
   long time (or never, in a session that is idle between tool calls).
   See `crates/tau-agent-plugin-tasks/src/tasks.rs::run_schedule_pass`
   ~line 2660.

Both bugs produce the same observable: "task in ready/planning with
stale (pre-worker) `session_id`, no worktree, no worker dispatched".
Bug #1 explains symptoms across projects and intermittent timing
(exactly matches "#521 in iris — planner finished, no worker ever
spawned" and "#512/#514/… on tau — manual `task_schedule` needed"). Bug
#2 compounds: even when the event fires, a single transient failure
from `CreateSession`/`Chat` leaves the task stuck until the next
unrelated event.

The prior fix for merged/closed scheduling (#530, already merged on
this branch) partially addresses a third path — `ScheduleNeeded` on
terminal transitions — but does not address either of the bugs above.

---

## How the scheduler is wired (as currently implemented)

### Event queueing path (tool-call-driven)

`handle_task_update` and `handle_task_create` live inside the tasks
plugin subprocess. They run on the main thread of the plugin. When a
state transition happens, they push a `SchedulerEvent` into the
plugin-local `pending_events: Vec<SchedulerEvent>`:

- `approved` → `MergeNeeded`
- `ready` / `planning` (if not `held`) → `ScheduleNeeded(project, session)`
- `merged` / `closed` → `ScheduleNeeded(project, session)` (added by #530)
- `hold=false` release on a schedulable state → `ScheduleNeeded` if not
  already queued

See `crates/tau-agent-plugin-tasks/src/tasks.rs` ~line 1220.

After every `ToolCall` request and every `Hook` request, the main loop
unconditionally calls `drain_scheduler_events()` (lines 2350 and 2396).
Drain runs `run_merge_pass` and `run_schedule_pass` for each unique
pending event.

### Event queueing path (hook-driven)

The tasks plugin registers for the `task_state_changed` hook (`hooks:
vec!["task_state_changed".to_string()]` at line 2147). The server's
`FireHook` handler (`crates/tau-agent/src/server/tool_dispatch.rs` ~line
608) calls `plugins.lock().call_hook_excluding(session_id, name, data,
None)`. The tasks plugin's `PluginRequest::Hook` branch (~line 2376)
pushes `ScheduleNeeded` / `MergeNeeded` and drains.

Only the TUI currently fires the `task_state_changed` hook (at
`crates/tau-agent-tui/src/app.rs` ~line 2894), and only for `approved`
and `ready` transitions. Worker/planner/refiner sessions don't fire
hooks on task updates — their plugin-tool path uses `pending_events`
directly.

### Dispatch error-handling

`run_schedule_pass` → `tasks_scheduler::schedule()` → `prepare_task()`
(transitions ready→active, creates branch+worktree) → `dispatch()`
(creates session via `CreateSession` + sends initial `Chat`). If any
step of `dispatch()` fails, the outer `run_schedule_pass` catches the
error, writes a warning task message, sends a `QueueMessage` to the
triggering session, and **reverts** the task back to `ready` for retry.

---

## Bug 1 — plugin taken-out during tool call loses hooks

### Evidence trail

```rust
// crates/tau-agent/src/server/agent_runner.rs::PluginExecutor::execute
let taken = {
    let mut pm = self.plugins.lock().expect("plugins mutex poisoned");
    pm.take_tool_plugin(&self.session_id, &tool_call.name)
};
// ... tool call loop runs here, plugin is NOT in pm.global_plugins ...
{
    let mut pm = self.plugins.lock().expect("plugins mutex poisoned");
    pm.return_tool_plugin(source, handle);
}
```

```rust
// crates/tau-agent/src/plugin.rs::PluginManager::call_hook_excluding
pub fn call_hook_excluding(
    &mut self,
    session_id: &str,
    name: &str,
    data: &serde_json::Value,
    exclude_plugin: Option<&str>,
) -> Vec<HookResult> {
    // Iterates self.global_plugins — taken-out plugins are NOT in this list
    let mut results =
        call_hook_all_excluding(&mut self.global_plugins, name, data, exclude_plugin);
    // ...
}
```

`call_hook_all_excluding` iterates a slice and calls each plugin. If the
tasks plugin is not in the slice (taken out for a tool call), the hook
is simply skipped with no record anywhere.

### Reproduction sketch

1. Session A calls `task_update state=ready` via the plugin. The tasks
   plugin handle is removed from `global_plugins` for the duration of
   that ToolCall. That handler queues its own `ScheduleNeeded` via
   `pending_events` — this works correctly.
2. Meanwhile, in the TUI, the user presses `M-r` to move a different
   task B in a different project (or the same project) to `ready`. The
   TUI fires `task_state_changed` via `Action::FireHook`.
3. Server's `FireHook` handler runs, acquires the plugins lock, calls
   `call_hook_excluding`. **The tasks plugin is not in `global_plugins`
   at this moment**, so the call silently does nothing for tasks. No
   queued event anywhere in the plugin. Task B is now in ready state
   with no scheduler event pending.
4. The tasks plugin eventually finishes Session A's ToolCall, is
   returned to the pool, and drains *Session A's* pending events. Task
   B's hook event has been lost.

### Why the symptom is intermittent

The race window is small but not tiny:
- `CreateSession` + `Chat` inside `dispatch()` round-trips to the
  server twice and can take tens to hundreds of milliseconds.
- Any agent-driven tool call (especially ones that themselves invoke
  scheduler dispatch) holds the plugin for its full duration.
- In parallel-session work (the common case in multi-task sessions),
  hook firing from one session easily overlaps a tool call from
  another.

In sessions with sparse tool activity this may never reproduce; in
sessions with many concurrent tasks it reproduces regularly — matching
the task's description.

### Why the problem is broader than described in H5

The investigation's H5 speculated "plugin-per-project routing". That's
not the issue: there's exactly one global tasks plugin. The issue is
that the global plugin is **removed from the dispatch list for the
duration of each tool call** — a design choice to prevent deadlocks
when tool handlers make `ServerRequest` calls (see
`PluginManager::take_tool_plugin` docstring).

### Proposed fix direction (do not implement in this task)

Two viable directions:

- **Queue hooks on the plugin handle itself.** When a hook arrives for
  a plugin that is taken-out, stash the hook in a pending queue on the
  handle, and deliver it when the handle is returned to the pool. This
  preserves the deadlock-avoidance property of `take_tool_plugin`.
- **Make the plugin handle directly send the hook (bypassing the pool).**
  Since the plugin process is still alive and reading from its stdin,
  we could `send(Request::Hook { ... })` directly on the taken-out
  handle. The tricky part: the handle is owned by the executor; the
  server thread handling `FireHook` would need access. Plus, the
  plugin's `server_request` tunnel currently drops Hook messages that
  arrive while it's waiting for a ServerResponse (see Bug 1b below),
  so this path alone is not enough.

A hybrid is probably cleanest: (a) fix the tunnel to forward Hook
events to a pending queue the main loop can drain after the
ServerRequest completes, and (b) fix the take-out to queue hooks and
replay them when the handle is returned.

### Bug 1b (adjacent) — tunnel drops Hook requests too

`crates/tau-agent-plugin/src/tunnel.rs::server_request` has explicit
cases for `ServerResponse` (match) and `ToolCall` (error-reply), but
falls into `_ => {}` for `Hook` — silently dropping it. In current
routing this is unreachable (because a taken-out plugin doesn't receive
hooks), but if Bug 1's routing is fixed so hooks can arrive
mid-ServerRequest, the tunnel must be updated to requeue them.

---

## Bug 2 — dispatch-failure revert does not re-queue

### Evidence

```rust
// crates/tau-agent-plugin-tasks/src/tasks.rs::run_schedule_pass ~line 2655
if let Err(e) =
    tasks_scheduler::dispatch(db, st.id, session_id, &project_path, writer, reader)
{
    eprintln!("tasks scheduler: dispatch failed for task {}: {}", st.id, e);

    // For non-planning tasks, schedule() already transitioned
    // to active via prepare_task().  Revert to ready so the
    // scheduler can retry on the next pass.
    let is_planning = st.branch.is_empty();
    if !is_planning {
        let _ = db.update_task(
            st.id,
            &TaskUpdate {
                state: Some("ready".to_string()),
                ..Default::default()
            },
            None,
        );
    }
    // ... warning message, QueueMessage to triggering session ...
    // ❌ No pending_events.push(ScheduleNeeded(...)) here.
}
```

The comment says "will be retried" but the retry depends on something
else pushing a `ScheduleNeeded` later. In a multi-task session that
next event often comes quickly (any subsequent `task_update` or
`task_create` fires one), but in a quiescent session it never comes —
and in combination with Bug 1, even the next tool-call hook path may
be lost.

### Reproduction sketch

1. Task in ready state; triggering `task_update` queues ScheduleNeeded.
2. `run_schedule_pass` runs: `schedule()` → `prepare_task()`
   (ready→active, worktree created) → `dispatch()` → `CreateSession`
   RPC succeeds → `Chat` RPC fails (transient server error, OOM,
   connection blip).
3. Revert path fires: task goes back to `ready`. Planner's session_id
   still on the task. Branch/worktree already on disk. No new
   `ScheduleNeeded` queued.
4. Nothing ever retries until an unrelated event fires a schedule pass
   for the same project.

### Proposed fix direction

Inside `run_schedule_pass`, after a successful revert, retry once
inline (simpler) or push a `ScheduleNeeded` onto a caller-provided
event queue that `drain_scheduler_events` will pick up. The former
is simpler but risks tight-looping on persistent failures; the
latter is cleaner but requires threading `pending_events` into
`run_schedule_pass`'s signature.

Also worth considering: bounded retry count on the task itself to
avoid tight-looping on genuinely-broken state (e.g. merge target
branch removed). Since the task's own revert path already writes
warnings to the task, a retry limit is discoverable in the task
message log.

---

## Hypotheses from the task spec, resolved

- **H1 (event queue not drained on all code paths)** — **partially
  confirmed**, with a different root cause than predicted. It's not
  that the drain site is skipped; it's that the event is never pushed
  into the queue in the first place when the plugin is taken-out for a
  tool call (Bug 1). Also, the tunnel drops Hook messages that arrive
  mid-ServerRequest (Bug 1b).

- **H2 (`run_schedule_pass` returns early on project-resolve error)**
  — plausible but not the primary cause for the observed symptoms. The
  `ProjectResolver` is global (reads `tau.db`'s `projects` table), so
  cross-project resolution works as long as the target project is
  registered. The worst that happens is a silent skip for tasks
  belonging to projects that haven't been registered yet — rare.

- **H3 (dispatch partially fails and doesn't revert)** — **half
  confirmed**. The revert itself works, but no retry event is queued
  (Bug 2). The narrower form "dispatch error itself fails to update
  DB" is not the observed failure mode.

- **H4 (plugin process lifecycle)** — not the primary cause; no
  evidence of plugin restarts during the observed failures, and the
  global tasks plugin is not restarted by session lifecycle.

- **H5 (concurrent plugin processes per project)** — not applicable:
  there is a single global tasks plugin across all projects. But the
  underlying concern (routing) is real, just manifested differently
  (Bug 1, the take-out routing hole).

---

## Relationship to in-flight fixes

- **#530 (fire ScheduleNeeded on merged/closed)** — already merged on
  this branch; does not address Bugs 1 or 2.
- **#532 (session-affecting info messages)** — orthogonal.
- **#533 (three-tier deferral)** — orthogonal to event loss; concerns
  dispatch prioritisation under budget pressure.

---

## Recommended follow-up

Two narrowly-scoped fix tasks:

1. **Fix hook routing for taken-out plugins** (Bug 1 + 1b). Plugin-side
   change in the tunnel to forward hooks into a pending queue; server
   side change to queue hooks on taken-out handles and drain on
   return. Test: concurrent tool-call + FireHook scenario verifies the
   hook is delivered.

2. **Re-queue `ScheduleNeeded` on dispatch revert** (Bug 2). Pass
   `pending_events` into `run_schedule_pass` (or return an
   events-to-queue list), and on the revert path push a retry. Add a
   bounded retry counter on the task to avoid tight loops on genuinely
   broken state. Test: simulate `Chat` failure once → confirm next
   schedule pass re-dispatches.

Neither fix is in scope for this investigation task (per spec: "don't
fix anything on speculation"). A one-line fix for Bug 2 (push
`ScheduleNeeded` in the revert path) was considered for inline
application, but requires changing the signature of
`run_schedule_pass` to take `&mut Vec<SchedulerEvent>`, which is a
small but non-trivial refactor best done in a dedicated fix task with
tests.

---

## Observability gaps noted during investigation

- No log line when `call_hook_excluding` skips a plugin because it's
  not in `global_plugins`. Adding a `tracing::debug!` there would
  immediately make Bug 1 visible in server logs.
- `run_schedule_pass`'s dispatch-failure revert logs via `eprintln!`
  rather than structured tracing; harder to filter in production logs.
- `drain_scheduler_events` doesn't log the event batch — hard to
  confirm "did ScheduleNeeded fire for project X?" from logs alone.

These are low-hanging observability wins independent of the fixes.
