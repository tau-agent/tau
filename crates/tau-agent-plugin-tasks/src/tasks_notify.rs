//! Task state-change notifications.
//!
//! Every time a task's `state` column actually changes we emit a one-line
//! [`InfoMessage`](tau_agent_base::types::InfoMessage) to every session that
//! participates in the task's lifecycle (worker, reviewer, planner,
//! refiner, creator, interactive, …) plus, for terminal or
//! user-attention-demanding transitions, the user's root session.
//!
//! Info messages are zero-token, display-only: they are persisted to the
//! target session's message history, shown in the TUI, but excluded from
//! LLM context and do **not** wake the agent loop.
//!
//! The public entry point is [`notify_state_change`]. Wire it into every
//! code site that mutates `task.state` (see call-site grep in
//! `handle_task_update`, `tasks_scheduler`, and `tasks_merge`).
//!
//! The existing specialised notifiers
//! ([`tasks_merge::notify_parent_of_subtask_done`](crate::tasks_merge::notify_parent_of_subtask_done),
//! [`tasks_merge::notify_parent_if_all_done`](crate::tasks_merge::notify_parent_if_all_done),
//! [`tasks_merge::notify_session_of_merge_failure`](crate::tasks_merge::notify_session_of_merge_failure),
//! and the `planning → interactive` scope-expansion QueueMessage) stay.
//! Those carry action-prompting content; this module is purely
//! observational.

use std::collections::HashSet;
use std::io::{BufRead, Write};

use crate::tasks_db::{Task, TasksDb};
use crate::tasks_scheduler::{find_root_session, server_request};
use crate::tasks_state::TaskState;

/// Truncate a title to at most `MAX_TITLE_LEN` chars, replacing the tail
/// with an ellipsis if it overflows. Character-count based (Unicode scalar
/// values), not byte-count, so the cap is stable across scripts.
const MAX_TITLE_LEN: usize = 120;

fn capped_title(title: &str) -> String {
    let char_count = title.chars().count();
    if char_count <= MAX_TITLE_LEN {
        title.to_string()
    } else {
        let head: String = title.chars().take(MAX_TITLE_LEN).collect();
        format!("{}…", head)
    }
}

// ---------------------------------------------------------------------------
// Session tagline helpers
// ---------------------------------------------------------------------------

/// Character cap for the `{title}` part of a task-session tagline. Chosen
/// to keep the full `[task N] role: title` line well under the TUI's
/// session-list width while preserving typical task titles in full.
const TAGLINE_TITLE_MAX: usize = 80;

/// Character-aware truncation with an ellipsis marker. UTF-8 safe: only
/// splits on character boundaries, never mid-codepoint.
fn truncate_title(title: &str, max: usize) -> String {
    if title.chars().count() <= max {
        return title.to_string();
    }
    let head_len = max.saturating_sub(1);
    let mut out: String = title.chars().take(head_len).collect();
    out.push('…');
    out
}

/// Format a unified task-session tagline.
///
/// All task sessions use the same format, so the TUI session tree can
/// tell at a glance whether a given session is driving spec refinement,
/// implementation, review, etc.:
///
/// ```text
/// [task {id}] {role}: {title}
/// ```
///
/// where `{role}` is one of the role strings recorded in
/// `task_sessions`: `interactive`, `planning`, `refining`, `worker`,
/// `review`, `merge`. The title is truncated to [`TAGLINE_TITLE_MAX`]
/// characters with an ellipsis if longer.
pub fn task_session_tagline(task: &Task, role: &str) -> String {
    let title = truncate_title(&task.title, TAGLINE_TITLE_MAX);
    format!("[task {}] {}: {}", task.id, role, title)
}

/// Tagline for a task's *placeholder* session — the non-LLM session that
/// groups every other session spawned for this task (planner, worker,
/// reviewer, refiner, merge, …). No role suffix because the placeholder
/// spans all roles.
///
/// ```text
/// [task {id}] {title}
/// ```
///
/// The title is truncated the same way as [`task_session_tagline`].
pub fn task_placeholder_tagline(task: &Task) -> String {
    let title = truncate_title(&task.title, TAGLINE_TITLE_MAX);
    format!("[task {}] {}", task.id, title)
}

/// Best-effort update of a session's tagline via [`Request::SetTagline`].
///
/// Call this when an existing session is reused across a role change
/// (e.g. scheduler reuses a planner session in a new refining cycle):
/// the tagline needs to reflect the session's *current* role so the TUI
/// session tree stays accurate.
///
/// Errors are swallowed. If the session has since been archived the
/// update is moot; if the server is unreachable we have bigger problems.
pub fn set_session_tagline(
    session_id: &str,
    new_tagline: &str,
    writer: &mut impl Write,
    reader: &mut impl BufRead,
) {
    let _ = server_request(
        writer,
        reader,
        tau_agent_plugin::Request::SetTagline {
            session_id: session_id.to_string(),
            tagline: new_tagline.to_string(),
        },
    );
}

/// Classify a transition target for root-broadcast purposes.
fn broadcasts_to_root(to: TaskState) -> bool {
    // Terminal states that the user should see regardless of where the
    // action happened.  `closed` is excluded on purpose: it's "we're
    // dropping this", not actionable.
    matches!(
        to,
        TaskState::Merged | TaskState::Failed | TaskState::Interactive
    )
}

/// Build the one-line info-message text for a transition.
///
/// * `* → merged`: `[task #{id}] {title}: merged`  (elides `from →`)
/// * otherwise:    `[task #{id}] {title}: {from} → {to}`
/// * with context: append ` ({context})` at the end.
fn format_message(task: &Task, from: TaskState, context: Option<&str>) -> String {
    let title = capped_title(&task.title);
    let body = if task.state == TaskState::Merged {
        format!("[task #{}] {}: merged", task.id, title)
    } else {
        format!("[task #{}] {}: {} → {}", task.id, title, from, task.state)
    };
    match context {
        Some(ctx) if !ctx.is_empty() => format!("{} ({})", body, ctx),
        _ => body,
    }
}

/// Collect the set of session IDs that should see a state-change info
/// message for `task` transitioning `from → task.state`.
///
/// Recipients, union-and-deduped:
/// * every session recorded in the task's `task_sessions` table,
/// * the parent task's `creator` and `interactive` sessions,
/// * for root-broadcast transitions ([`broadcasts_to_root`]), the root
///   session resolved from the task's current session / creator /
///   parent-task session (first available anchor wins).
///
/// Archived sessions are skipped (via per-session `GetSessionInfo`).
fn collect_recipients(
    db: &TasksDb,
    task: &Task,
    writer: &mut impl Write,
    reader: &mut impl BufRead,
) -> Vec<String> {
    let mut set: HashSet<String> = HashSet::new();

    // 1. Task's own session_id (may or may not be in task_sessions).
    if let Some(ref sid) = task.session_id {
        set.insert(sid.clone());
    }

    // 2. Every recorded task_sessions row.
    if let Ok(sessions) = db.get_sessions(task.id) {
        for ts in sessions {
            set.insert(ts.session_id);
        }
    }

    // 3. Parent task's creator + interactive rows.
    if let Some(parent_id) = task.parent_id {
        if let Ok(parent_sessions) = db.get_sessions(parent_id) {
            for ts in parent_sessions {
                if ts.role == "creator" || ts.role == "interactive" {
                    set.insert(ts.session_id);
                }
            }
        }
    }

    // 4. Root broadcast for terminal / interactive transitions.
    if broadcasts_to_root(task.state) {
        if let Some(root) = resolve_root_session(db, task, writer, reader) {
            set.insert(root);
        }
    }

    // 5. Placeholder session (task #574). The placeholder owns the
    //    task's session subtree and accumulates a timeline of the task's
    //    life; it receives every legitimate state transition.
    if let Some(ref sid) = task.placeholder_session_id {
        set.insert(sid.clone());
    }

    // 6. Filter archived sessions.
    set.into_iter()
        .filter(|sid| !is_session_archived(sid, writer, reader))
        .collect()
}

/// Find the session recorded with role `"creator"` on the task, if any.
///
/// Returns the first creator row's `session_id`. The role is recorded
/// at task creation time in `task_sessions`
/// (see `create_task` / `create_subtask` paths, which call
/// `db.record_session(task.id, creator_sid, "creator")`).
///
/// Returns `None` if the row isn't there (older tasks that predate the
/// role stamp, or tasks created via paths that don't record a creator).
fn find_creator_session(db: &TasksDb, task: &Task) -> Option<String> {
    db.get_sessions(task.id)
        .ok()?
        .into_iter()
        .find(|ts| ts.role == "creator")
        .map(|ts| ts.session_id)
}

/// Resolve the root session for root-broadcast transitions.  Tries the
/// task's current session first, then the `creator` row on the task, then
/// the parent task's session.  Returns `None` if none of the anchors
/// exist or `find_root_session` returns `None`.
fn resolve_root_session(
    db: &TasksDb,
    task: &Task,
    writer: &mut impl Write,
    reader: &mut impl BufRead,
) -> Option<String> {
    // Anchor 1: the task's current session_id.
    if let Some(ref sid) = task.session_id {
        if let Some(root) = find_root_session(sid, writer, reader) {
            return Some(root);
        }
    }

    // Anchor 2: the task's creator session (from task_sessions).
    if let Ok(sessions) = db.get_sessions(task.id) {
        for ts in &sessions {
            if ts.role == "creator" {
                if let Some(root) = find_root_session(&ts.session_id, writer, reader) {
                    return Some(root);
                }
            }
        }
    }

    // Anchor 3: the parent task's session.
    if let Some(parent_id) = task.parent_id {
        if let Ok(Some(parent)) = db.get_task(parent_id) {
            if let Some(ref parent_sid) = parent.session_id {
                if let Some(root) = find_root_session(parent_sid, writer, reader) {
                    return Some(root);
                }
            }
        }
    }

    None
}

/// Best-effort archived-session check via `GetSessionInfo`.  Returns
/// `false` (i.e. "not archived, do emit") on RPC failure — we'd rather
/// deliver a message to a live session than silently swallow it.
fn is_session_archived(
    session_id: &str,
    writer: &mut impl Write,
    reader: &mut impl BufRead,
) -> bool {
    match server_request(
        writer,
        reader,
        tau_agent_plugin::Request::GetSessionInfo {
            session_id: session_id.to_string(),
        },
    ) {
        Ok(tau_agent_plugin::Response::SessionInfo { info }) => info.archived,
        _ => false,
    }
}

/// Emit a state-change [`InfoMessage`](tau_agent_base::types::InfoMessage)
/// to every session that should see it.
///
/// Called from every code site where a task's `state` column actually
/// changes (`handle_task_update`, scheduler-driven transitions, merge
/// outcomes).
///
/// * `task`: the task **after** the state change.  `task.id`, `task.title`,
///   `task.state`, and `task.parent_id` must be populated.
/// * `from`: the previous state.  If this is equal to `task.state` the
///   call is a no-op (there was no real transition).
/// * `context`: optional free-form suffix appended as `(context)` to the
///   message (e.g. `"commit abc1234"`, `"rework requested"`).
/// * `writer` / `reader`: the plugin's stdout/stdin for server RPCs.
///
/// Delivery is best-effort: errors on individual recipients are swallowed
/// so a single broken RPC can't prevent the rest of the broadcast.
pub fn notify_state_change(
    db: &TasksDb,
    task: &Task,
    from: TaskState,
    context: Option<&str>,
    writer: &mut impl Write,
    reader: &mut impl BufRead,
) {
    notify_state_change_split(
        db,
        task,
        from,
        context,
        None,
        writer,
        reader,
        &mut Vec::new(),
    );
}

/// Like [`notify_state_change`] but splits the recipient list into
/// "others" (fired eagerly as Tier-1) and "self" (the `caller_session_id`,
/// if any, which is accumulated into `post_persist` as Tier-2 actions so
/// the info message renders *after* the tool result in the caller's
/// session history).
///
/// When `caller_session_id` is `None`, behaves identically to
/// [`notify_state_change`] — all recipients fire eagerly.
pub fn notify_state_change_split(
    db: &TasksDb,
    task: &Task,
    from: TaskState,
    context: Option<&str>,
    caller_session_id: Option<&str>,
    writer: &mut impl Write,
    reader: &mut impl BufRead,
    post_persist: &mut Vec<tau_agent_plugin::PostPersistAction>,
) {
    if from == task.state {
        // No actual transition — avoid noise.
        return;
    }

    let text = format_message(task, from, context);
    let recipients = collect_recipients(db, task, writer, reader);

    for sid in recipients {
        if caller_session_id == Some(sid.as_str()) {
            // Defer to Tier-2 so this renders after the tool result in the
            // caller's history.
            post_persist.push(tau_agent_plugin::PostPersistAction::EmitInfoMessage {
                target_session_id: sid,
                text: text.clone(),
            });
            continue;
        }
        let _ = server_request(
            writer,
            reader,
            tau_agent_plugin::Request::QueueInfo {
                target_session_id: sid,
                text: text.clone(),
            },
        );
    }

    // Terminal-state summary (task #574). When a task reaches a
    // terminal state, post a closing summary to the placeholder so the
    // timeline has a clean final line. Skip if the placeholder has
    // been archived externally (consistent with `collect_recipients`).
    if task.state.is_terminal() {
        if let Some(ref placeholder_sid) = task.placeholder_session_id {
            if !is_session_archived(placeholder_sid, writer, reader) {
                let summary = format_terminal_summary(db, task);
                if let Err(e) = server_request(
                    writer,
                    reader,
                    tau_agent_plugin::Request::QueueInfo {
                        target_session_id: placeholder_sid.clone(),
                        text: summary,
                    },
                ) {
                    eprintln!(
                        "tasks placeholder: failed to post terminal summary to {}: {}",
                        placeholder_sid, e
                    );
                }
            }
        }

        // Task #658: LLM-visible wake to the creator so the session
        // that spawned the task can react to completion in-context.
        // Other recipients continue to receive only the zero-token
        // QueueInfo line from the loop above — this extra message is
        // creator-only.
        if let Some(creator_sid) = find_creator_session(db, task) {
            // Archived creator: the user has dismissed that session.
            // Waking it would resurrect it in listings and burn tokens
            // — skip. (The QueueInfo line is already suppressed for
            // archived recipients by `collect_recipients` above.)
            if !is_session_archived(&creator_sid, writer, reader) {
                // Dedup with `tasks_merge::notify_parent_of_subtask_done`:
                // that notifier already sends a QueueMessage to the
                // parent task's current `session_id` on every terminal
                // transition. If this subtask's creator IS the parent's
                // current session, skip here so that session only gets
                // one LLM-visible wake per event.
                let parent_session = task
                    .parent_id
                    .and_then(|pid| db.get_task(pid).ok().flatten())
                    .and_then(|p| p.session_id);
                let dedup = parent_session.as_deref() == Some(creator_sid.as_str());
                if !dedup {
                    let content = format_terminal_llm_message(task, context);
                    let _ = server_request(
                        writer,
                        reader,
                        tau_agent_plugin::Request::QueueMessage {
                            target_session_id: creator_sid,
                            content,
                            sender_info: "task notifier".to_string(),
                            await_reply: false,
                            reply_to: None,
                        },
                    );
                }
            }
        }
    }
}

/// Format the LLM-visible terminal notification posted to the task's
/// creator session on `merged` / `closed` / `failed`.
///
/// The text is concise, mentions the task id / title / terminal state,
/// and appends `context` when present (commit hash for merges, failure
/// reason for failed, etc.). Top-level tasks are called `Task #N`;
/// tasks with a parent are called `Subtask #N`.
fn format_terminal_llm_message(task: &Task, context: Option<&str>) -> String {
    let title = capped_title(&task.title);
    let noun = if task.parent_id.is_some() {
        "Subtask"
    } else {
        "Task"
    };
    match task.state {
        TaskState::Merged => match context {
            Some(c) if !c.is_empty() => format!(
                "{} #{} \"{}\" has been merged ({}). If you were waiting on it, you can now proceed.",
                noun, task.id, title, c
            ),
            _ => format!(
                "{} #{} \"{}\" has been merged. If you were waiting on it, you can now proceed.",
                noun, task.id, title
            ),
        },
        TaskState::Closed => match context {
            Some(c) if !c.is_empty() => format!(
                "{} #{} \"{}\" has been closed ({}).",
                noun, task.id, title, c
            ),
            _ => format!("{} #{} \"{}\" has been closed.", noun, task.id, title),
        },
        TaskState::Failed => match context {
            Some(c) if !c.is_empty() => {
                format!("{} #{} \"{}\" failed: {}.", noun, task.id, title, c)
            }
            _ => format!("{} #{} \"{}\" failed.", noun, task.id, title),
        },
        // Unreachable: callers gate on `is_terminal()`. Keep a sane
        // fallback rather than panic so a future new terminal state
        // doesn't crash the notifier.
        _ => format!(
            "{} #{} \"{}\" reached terminal state {}.",
            noun, task.id, title, task.state
        ),
    }
}

/// Format the human-readable terminal summary posted to the placeholder
/// on transitions into `merged` / `closed` / `failed`.
///
/// ```text
/// Task #{id} {merged|closed|failed}. {count} sessions, {duration} elapsed.
/// ```
fn format_terminal_summary(db: &TasksDb, task: &Task) -> String {
    let session_count = db.get_sessions(task.id).map(|v| v.len()).unwrap_or(0);
    let elapsed_ms = task.updated_at.saturating_sub(task.created_at);
    let elapsed = format_duration_ms(elapsed_ms);
    format!(
        "Task #{} {}. {} session{}, {} elapsed.",
        task.id,
        task.state,
        session_count,
        if session_count == 1 { "" } else { "s" },
        elapsed,
    )
}

/// Format a duration in milliseconds as a short human string.
/// Examples: `42ms`, `3.4s`, `1m23s`, `2h05m`, `1d03h`.
fn format_duration_ms(ms: i64) -> String {
    if ms < 0 {
        return "0ms".to_string();
    }
    let ms = ms as u64;
    if ms < 1_000 {
        return format!("{}ms", ms);
    }
    let secs = ms / 1_000;
    if secs < 60 {
        let tenths = (ms % 1_000) / 100;
        return format!("{}.{}s", secs, tenths);
    }
    let mins = secs / 60;
    let rem_secs = secs % 60;
    if mins < 60 {
        return format!("{}m{:02}s", mins, rem_secs);
    }
    let hours = mins / 60;
    let rem_mins = mins % 60;
    if hours < 24 {
        return format!("{}h{:02}m", hours, rem_mins);
    }
    let days = hours / 24;
    let rem_hours = hours % 24;
    format!("{}d{:02}h", days, rem_hours)
}

/// Post the one-time "task created" info message to the task's
/// placeholder session, summarising the task at creation time.
///
/// Fires exactly once per task, right after
/// [`create_placeholder_session`](crate::tasks::create_placeholder_session)
/// records the placeholder sid on the task row. If the task has no
/// placeholder (placeholder creation failed) this is a no-op.
///
/// Delivery is best-effort: errors are swallowed so a failed RPC can't
/// prevent the surrounding task creation from succeeding.
pub fn notify_task_created(task: &Task, writer: &mut impl Write, reader: &mut impl BufRead) {
    let placeholder_sid = match task.placeholder_session_id.as_deref() {
        Some(sid) => sid,
        None => return,
    };
    let text = format_task_created(task);
    if let Err(e) = server_request(
        writer,
        reader,
        tau_agent_plugin::Request::QueueInfo {
            target_session_id: placeholder_sid.to_string(),
            text,
        },
    ) {
        eprintln!(
            "tasks placeholder: failed to post task-created message to {}: {}",
            placeholder_sid, e
        );
    }
}

/// Format a task's `tags` field (stored as JSON) as a comma-separated
/// list. Returns `"—"` when empty/missing.
fn format_tags(tags: &Option<serde_json::Value>) -> String {
    let arr = match tags.as_ref().and_then(|v| v.as_array()) {
        Some(a) if !a.is_empty() => a,
        _ => return "—".to_string(),
    };
    let parts: Vec<String> = arr
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();
    if parts.is_empty() {
        "—".to_string()
    } else {
        parts.join(", ")
    }
}

/// Build the multi-line "Task #N created" info-message body.
fn format_task_created(task: &Task) -> String {
    let title = capped_title(&task.title);
    let mut out = format!("Task #{} created: {}\n\n", task.id, title);
    out.push_str(&format!("Priority: {}\n", task.priority));
    let tags_str = format_tags(&task.tags);
    out.push_str(&format!("Tags: {}\n", tags_str));
    let parent_str = match task.parent_id {
        Some(pid) => format!("#{}", pid),
        None => "(top-level)".to_string(),
    };
    out.push_str(&format!("Parent task: {}\n", parent_str));
    out.push_str(&format!(
        "Require approval: {}\n",
        if task.require_approval { "yes" } else { "no" },
    ));
    out.push_str(&format!(
        "Skip review: {}\n",
        if task.skip_review { "yes" } else { "no" },
    ));
    let merge_target = task.merge_target.as_deref().unwrap_or("(default)");
    out.push_str(&format!("Merge target: {}\n", merge_target));
    out.push_str(&format!("Initial state: {}", task.state));
    out
}

/// Post a scheduler-wait info message to the task's placeholder.
///
/// Called by the scheduler tracker (see [`WaitTracker`]) when a task's
/// wait reason changes. Fires only on transitions (newly-present,
/// changed, or newly-cleared) — the tracker handles deduplication.
///
/// `text` is the rendered wait line (e.g. `"Waiting: file conflict with
/// task #42"` or `"Wait cleared after 2m31s. Dispatching."`).
pub fn notify_placeholder_wait(
    task: &Task,
    text: &str,
    writer: &mut impl Write,
    reader: &mut impl BufRead,
) {
    let placeholder_sid = match task.placeholder_session_id.as_deref() {
        Some(sid) => sid,
        None => return,
    };
    if let Err(e) = server_request(
        writer,
        reader,
        tau_agent_plugin::Request::QueueInfo {
            target_session_id: placeholder_sid.to_string(),
            text: text.to_string(),
        },
    ) {
        eprintln!(
            "tasks placeholder: failed to post wait message to {}: {}",
            placeholder_sid, e
        );
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks_db::TasksDb;
    use std::collections::HashMap;
    use std::io::BufReader;
    use std::sync::{Arc, Mutex};
    use tau_agent_plugin::{PluginMessage, PluginRequest, Request, Response, SessionInfo};

    // -----------------------------------------------------------------
    // Mock IO.  Responds to GetSessionInfo / GetSessionAncestors /
    // QueueInfo.  Captures every QueueInfo so tests can assert on the
    // recipient set and message text.
    // -----------------------------------------------------------------

    struct MockShared {
        write_buf: Vec<u8>,
        read_buf: Vec<u8>,
        archived_sessions: HashSet<String>,
        ancestors: HashMap<String, Vec<SessionInfo>>,
        /// Captured (target_session_id, text) pairs from QueueInfo.
        queue_info_calls: Vec<(String, String)>,
        /// Captured (target, content, sender_info, await_reply) tuples
        /// from QueueMessage.
        queue_message_calls: Vec<(String, String, String, bool)>,
        /// Captured (session_id, tagline) pairs from SetTagline.
        set_tagline_calls: Vec<(String, String)>,
    }

    impl MockShared {
        fn new() -> Self {
            Self {
                write_buf: Vec::new(),
                read_buf: Vec::new(),
                archived_sessions: HashSet::new(),
                ancestors: HashMap::new(),
                queue_info_calls: Vec::new(),
                queue_message_calls: Vec::new(),
                set_tagline_calls: Vec::new(),
            }
        }

        fn process_pending(&mut self) {
            let buf = std::mem::take(&mut self.write_buf);
            let text = String::from_utf8_lossy(&buf);
            for line in text.lines() {
                if line.trim().is_empty() {
                    continue;
                }
                let msg: PluginMessage = match serde_json::from_str(line) {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                let (request_id, request) = match msg {
                    PluginMessage::ServerRequest {
                        request_id,
                        request,
                    } => (request_id, request),
                    _ => continue,
                };
                let response = self.respond(&request);
                let reply = PluginRequest::ServerResponse {
                    request_id,
                    response,
                };
                if let Ok(mut json) = serde_json::to_string(&reply) {
                    json.push('\n');
                    self.read_buf.extend_from_slice(json.as_bytes());
                }
            }
        }

        fn respond(&mut self, request: &Request) -> Response {
            match request {
                Request::QueueInfo {
                    target_session_id,
                    text,
                } => {
                    self.queue_info_calls
                        .push((target_session_id.clone(), text.clone()));
                    Response::Ok
                }
                Request::QueueMessage {
                    target_session_id,
                    content,
                    sender_info,
                    await_reply,
                    ..
                } => {
                    self.queue_message_calls.push((
                        target_session_id.clone(),
                        content.clone(),
                        sender_info.clone(),
                        *await_reply,
                    ));
                    Response::Ok
                }
                Request::GetSessionInfo { session_id } => Response::SessionInfo {
                    info: fake_session(
                        session_id,
                        None,
                        self.archived_sessions.contains(session_id),
                    ),
                },
                Request::SetTagline {
                    session_id,
                    tagline,
                } => {
                    self.set_tagline_calls
                        .push((session_id.clone(), tagline.clone()));
                    Response::Ok
                }
                Request::GetSessionAncestors { session_id } => {
                    let sessions = self.ancestors.get(session_id).cloned().unwrap_or_else(|| {
                        vec![fake_session(
                            session_id,
                            None,
                            self.archived_sessions.contains(session_id),
                        )]
                    });
                    Response::SessionAncestors { sessions }
                }
                _ => Response::Ok,
            }
        }
    }

    fn fake_session(id: &str, parent_id: Option<&str>, archived: bool) -> SessionInfo {
        SessionInfo {
            id: id.to_string(),
            model: "mock".into(),
            provider: "mock".into(),
            cwd: None,
            message_count: 0,
            stats: Default::default(),
            last_activity: 0,
            parent_id: parent_id.map(str::to_string),
            child_count: 0,
            child_budget: 16,
            tagline: None,
            state: "idle".into(),
            context_pct: None,
            archived,
            last_exit_status: None,
            is_live: false,
            turn_started_at_ms: None,
            phase_started_at_ms: None,
            project_name: None,
        }
    }

    struct MockWriter {
        shared: Arc<Mutex<MockShared>>,
    }
    impl std::io::Write for MockWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.shared
                .lock()
                .expect("mock writer lock")
                .write_buf
                .extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    struct MockReader {
        shared: Arc<Mutex<MockShared>>,
    }
    impl std::io::Read for MockReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let mut shared = self.shared.lock().expect("mock reader lock");
            shared.process_pending();
            if shared.read_buf.is_empty() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "no mock responses left",
                ));
            }
            let n = std::cmp::min(buf.len(), shared.read_buf.len());
            buf[..n].copy_from_slice(&shared.read_buf[..n]);
            shared.read_buf.drain(..n);
            Ok(n)
        }
    }

    fn make_io() -> (Arc<Mutex<MockShared>>, MockWriter, BufReader<MockReader>) {
        let shared = Arc::new(Mutex::new(MockShared::new()));
        let writer = MockWriter {
            shared: shared.clone(),
        };
        let reader = BufReader::new(MockReader {
            shared: shared.clone(),
        });
        (shared, writer, reader)
    }

    /// Get the captured (sid, text) pairs in deterministic (sid) order so
    /// tests can make exact assertions regardless of HashSet iteration
    /// order.
    fn captured_sorted(shared: &Arc<Mutex<MockShared>>) -> Vec<(String, String)> {
        let mut calls = shared
            .lock()
            .expect("mock shared lock")
            .queue_info_calls
            .clone();
        calls.sort();
        calls
    }

    fn create_task(db: &TasksDb, title: &str, parent_id: Option<i64>, initial: &str) -> Task {
        db.create_task(
            "test-project",
            title,
            None,
            parent_id,
            None,
            false,
            initial,
            false,
            None,
            None,
            false,
            None,
            false,
        )
        .expect("create task")
    }

    // -----------------------------------------------------------------
    // format_message
    // -----------------------------------------------------------------

    #[test]
    fn format_non_terminal_transition() {
        let db = TasksDb::open_memory().expect("db");
        let mut task = create_task(&db, "Demo", None, "ready");
        task.state = TaskState::Active;
        assert_eq!(
            format_message(&task, TaskState::Ready, None),
            format!("[task #{}] Demo: ready → active", task.id)
        );
    }

    #[test]
    fn format_terminal_merged_elides_from() {
        let db = TasksDb::open_memory().expect("db");
        let mut task = create_task(&db, "Demo", None, "ready");
        task.state = TaskState::Merged;
        assert_eq!(
            format_message(&task, TaskState::Merging, None),
            format!("[task #{}] Demo: merged", task.id)
        );
    }

    #[test]
    fn format_with_context_appends_suffix() {
        let db = TasksDb::open_memory().expect("db");
        let mut task = create_task(&db, "Demo", None, "ready");
        task.state = TaskState::Merged;
        assert_eq!(
            format_message(&task, TaskState::Merging, Some("commit abc1234")),
            format!("[task #{}] Demo: merged (commit abc1234)", task.id)
        );
        task.state = TaskState::Active;
        assert_eq!(
            format_message(&task, TaskState::Review, Some("rework requested")),
            format!(
                "[task #{}] Demo: review → active (rework requested)",
                task.id
            )
        );
    }

    // -----------------------------------------------------------------
    // Title-length cap
    // -----------------------------------------------------------------

    #[test]
    fn capped_title_leaves_short_titles_unchanged() {
        let t = "x".repeat(50);
        assert_eq!(capped_title(&t), t);
    }

    #[test]
    fn capped_title_truncates_long_titles_with_ellipsis() {
        let t = "x".repeat(125);
        let capped = capped_title(&t);
        assert_eq!(capped.chars().count(), MAX_TITLE_LEN + 1); // 120 + '…'
        assert!(capped.ends_with('…'));
        assert_eq!(
            capped.chars().take(MAX_TITLE_LEN).collect::<String>(),
            "x".repeat(MAX_TITLE_LEN)
        );
    }

    // -----------------------------------------------------------------
    // Recipient selection / dedup / archived filtering
    // -----------------------------------------------------------------

    #[test]
    fn notify_fires_on_non_terminal_transition() {
        let db = TasksDb::open_memory().expect("db");
        let mut task = create_task(&db, "T1", None, "ready");
        db.record_session(task.id, "s-worker", "worker")
            .expect("rec");
        db.set_session_id(task.id, "s-worker").expect("sid");
        task.state = TaskState::Active;
        task.session_id = Some("s-worker".into());

        let (shared, mut w, mut r) = make_io();
        notify_state_change(&db, &task, TaskState::Ready, None, &mut w, &mut r);

        let calls = captured_sorted(&shared);
        assert_eq!(calls.len(), 1, "calls: {:?}", calls);
        assert_eq!(calls[0].0, "s-worker");
        assert_eq!(
            calls[0].1,
            format!("[task #{}] T1: ready → active", task.id)
        );
    }

    /// `notify_state_change_split` with `caller_session_id == Some("s-worker")`
    /// sends the message to the caller via `post_persist` (Tier-2), not via
    /// the eager QueueInfo wire path.
    #[test]
    fn notify_split_defers_caller_to_post_persist() {
        let db = TasksDb::open_memory().expect("db");
        let mut task = create_task(&db, "Split", None, "ready");
        db.record_session(task.id, "s-worker", "worker")
            .expect("rec worker");
        db.record_session(task.id, "s-other", "refiner")
            .expect("rec other");
        db.set_session_id(task.id, "s-worker").expect("sid");
        task.state = TaskState::Active;
        task.session_id = Some("s-worker".into());

        let (shared, mut w, mut r) = make_io();
        let mut post_persist: Vec<tau_agent_plugin::PostPersistAction> = Vec::new();
        notify_state_change_split(
            &db,
            &task,
            TaskState::Ready,
            None,
            Some("s-worker"),
            &mut w,
            &mut r,
            &mut post_persist,
        );

        // Caller (s-worker) must NOT appear in queue_info calls — it's
        // deferred to post_persist.
        let calls = captured_sorted(&shared);
        let eager_sids: Vec<&str> = calls.iter().map(|(s, _)| s.as_str()).collect();
        assert!(
            !eager_sids.contains(&"s-worker"),
            "caller should not be in eager QueueInfo: {:?}",
            calls
        );
        // Non-caller (s-other) still fires eagerly.
        assert!(
            eager_sids.contains(&"s-other"),
            "non-caller should fire eagerly: {:?}",
            calls
        );

        // Caller's info message is in post_persist with the right text.
        let expected = format!("[task #{}] Split: ready → active", task.id);
        assert_eq!(post_persist.len(), 1, "post_persist: {:?}", post_persist);
        match &post_persist[0] {
            tau_agent_plugin::PostPersistAction::EmitInfoMessage {
                target_session_id,
                text,
            } => {
                assert_eq!(target_session_id, "s-worker");
                assert_eq!(text, &expected);
            }
        }
    }

    /// `notify_state_change_split` with `caller_session_id == None` is
    /// indistinguishable from `notify_state_change` — everyone fires
    /// eagerly.
    #[test]
    fn notify_split_caller_none_fires_everything_eagerly() {
        let db = TasksDb::open_memory().expect("db");
        let mut task = create_task(&db, "NoCaller", None, "ready");
        db.record_session(task.id, "s-worker", "worker")
            .expect("rec");
        db.record_session(task.id, "s-other", "refiner")
            .expect("rec2");
        task.state = TaskState::Active;

        let (shared, mut w, mut r) = make_io();
        let mut post_persist: Vec<tau_agent_plugin::PostPersistAction> = Vec::new();
        notify_state_change_split(
            &db,
            &task,
            TaskState::Ready,
            None,
            None,
            &mut w,
            &mut r,
            &mut post_persist,
        );

        let calls = captured_sorted(&shared);
        let sids: Vec<&str> = calls.iter().map(|(s, _)| s.as_str()).collect();
        assert!(sids.contains(&"s-worker"), "sids: {:?}", sids);
        assert!(sids.contains(&"s-other"), "sids: {:?}", sids);
        assert!(
            post_persist.is_empty(),
            "no caller → post_persist empty: {:?}",
            post_persist
        );
    }

    #[test]
    fn notify_deduplicates_recipients() {
        // Single session is both creator and worker — gets one message.
        let db = TasksDb::open_memory().expect("db");
        let mut task = create_task(&db, "Dup", None, "ready");
        db.record_session(task.id, "s-one", "creator")
            .expect("rec1");
        db.record_session(task.id, "s-one", "worker").expect("rec2");
        db.set_session_id(task.id, "s-one").expect("sid");
        task.session_id = Some("s-one".into());
        task.state = TaskState::Active;

        let (shared, mut w, mut r) = make_io();
        notify_state_change(&db, &task, TaskState::Ready, None, &mut w, &mut r);

        let calls = captured_sorted(&shared);
        assert_eq!(calls.len(), 1, "expected one dedup'd call, got {:?}", calls);
        assert_eq!(calls[0].0, "s-one");
    }

    #[test]
    fn notify_skips_archived_recipient() {
        let db = TasksDb::open_memory().expect("db");
        let mut task = create_task(&db, "Arc", None, "ready");
        db.record_session(task.id, "s-live", "worker")
            .expect("rec1");
        db.record_session(task.id, "s-dead", "reviewer")
            .expect("rec2");
        task.state = TaskState::Review;

        let (shared, mut w, mut r) = make_io();
        shared
            .lock()
            .expect("mock shared lock")
            .archived_sessions
            .insert("s-dead".into());

        notify_state_change(&db, &task, TaskState::Active, None, &mut w, &mut r);

        let calls = captured_sorted(&shared);
        let sids: Vec<&str> = calls.iter().map(|(s, _)| s.as_str()).collect();
        assert!(sids.contains(&"s-live"), "sids: {:?}", sids);
        assert!(
            !sids.contains(&"s-dead"),
            "archived session leaked: {:?}",
            sids
        );
    }

    #[test]
    fn notify_no_op_on_identical_state() {
        let db = TasksDb::open_memory().expect("db");
        let mut task = create_task(&db, "Same", None, "ready");
        db.record_session(task.id, "s-any", "worker").expect("rec");
        task.state = TaskState::Ready;

        let (shared, mut w, mut r) = make_io();
        notify_state_change(&db, &task, TaskState::Ready, None, &mut w, &mut r);

        assert!(captured_sorted(&shared).is_empty());
    }

    #[test]
    fn notify_includes_parent_creator_and_interactive() {
        // Parent task has a creator + an interactive row.  Both should be
        // on the recipient list when a child transitions.
        let db = TasksDb::open_memory().expect("db");
        let parent = create_task(&db, "Parent", None, "interactive");
        db.record_session(parent.id, "s-creator", "creator")
            .expect("rec c");
        db.record_session(parent.id, "s-ia", "interactive")
            .expect("rec i");
        db.record_session(parent.id, "s-unrelated", "worker")
            .expect("rec w"); // should NOT receive

        let mut child = create_task(&db, "Child", Some(parent.id), "ready");
        db.record_session(child.id, "s-worker", "worker")
            .expect("rec");
        db.set_session_id(child.id, "s-worker").expect("sid");
        child.session_id = Some("s-worker".into());
        child.state = TaskState::Active;

        let (shared, mut w, mut r) = make_io();
        notify_state_change(&db, &child, TaskState::Ready, None, &mut w, &mut r);

        let sids: Vec<String> = captured_sorted(&shared)
            .into_iter()
            .map(|(s, _)| s)
            .collect();
        assert!(sids.contains(&"s-worker".into()), "{:?}", sids);
        assert!(sids.contains(&"s-creator".into()), "{:?}", sids);
        assert!(sids.contains(&"s-ia".into()), "{:?}", sids);
        assert!(
            !sids.contains(&"s-unrelated".into()),
            "parent worker leaked: {:?}",
            sids
        );
    }

    // -----------------------------------------------------------------
    // Root-broadcast rules
    // -----------------------------------------------------------------

    #[test]
    fn notify_root_broadcast_on_merged() {
        let db = TasksDb::open_memory().expect("db");
        let mut task = create_task(&db, "M", None, "ready");
        db.record_session(task.id, "s-worker", "worker")
            .expect("rec");
        db.set_session_id(task.id, "s-worker").expect("sid");
        task.session_id = Some("s-worker".into());
        task.state = TaskState::Merged;

        let (shared, mut w, mut r) = make_io();
        shared.lock().expect("lock").ancestors.insert(
            "s-worker".into(),
            vec![
                fake_session("s-worker", Some("s-root"), false),
                fake_session("s-root", None, false),
            ],
        );

        notify_state_change(
            &db,
            &task,
            TaskState::Merging,
            Some("commit deadbeef"),
            &mut w,
            &mut r,
        );

        let calls = captured_sorted(&shared);
        let sids: Vec<&str> = calls.iter().map(|(s, _)| s.as_str()).collect();
        assert!(sids.contains(&"s-worker"), "{:?}", sids);
        assert!(sids.contains(&"s-root"), "root not notified: {:?}", sids);
        // Message text check
        let expected = format!("[task #{}] M: merged (commit deadbeef)", task.id);
        for (_, text) in &calls {
            assert_eq!(text, &expected);
        }
    }

    #[test]
    fn notify_root_broadcast_on_failed() {
        let db = TasksDb::open_memory().expect("db");
        let mut task = create_task(&db, "F", None, "ready");
        db.record_session(task.id, "s-worker", "worker")
            .expect("rec");
        db.set_session_id(task.id, "s-worker").expect("sid");
        task.session_id = Some("s-worker".into());
        task.state = TaskState::Failed;

        let (shared, mut w, mut r) = make_io();
        shared.lock().expect("lock").ancestors.insert(
            "s-worker".into(),
            vec![
                fake_session("s-worker", Some("s-root"), false),
                fake_session("s-root", None, false),
            ],
        );

        notify_state_change(
            &db,
            &task,
            TaskState::Merging,
            Some("checklist failed"),
            &mut w,
            &mut r,
        );

        let sids: Vec<String> = captured_sorted(&shared)
            .into_iter()
            .map(|(s, _)| s)
            .collect();
        assert!(sids.contains(&"s-root".to_string()), "{:?}", sids);
    }

    #[test]
    fn notify_root_broadcast_on_interactive() {
        let db = TasksDb::open_memory().expect("db");
        let mut task = create_task(&db, "I", None, "planning");
        // Transition task to refining so we can fake "refining → interactive".
        db.update_task(
            task.id,
            &crate::tasks_db::TaskUpdate {
                state: Some(TaskState::Refining),
                ..Default::default()
            },
            None,
        )
        .expect("to refining");
        db.record_session(task.id, "s-refiner", "refiner")
            .expect("rec");
        db.set_session_id(task.id, "s-refiner").expect("sid");
        task.session_id = Some("s-refiner".into());
        task.state = TaskState::Interactive;

        let (shared, mut w, mut r) = make_io();
        shared.lock().expect("lock").ancestors.insert(
            "s-refiner".into(),
            vec![
                fake_session("s-refiner", Some("s-root"), false),
                fake_session("s-root", None, false),
            ],
        );

        notify_state_change(
            &db,
            &task,
            TaskState::Refining,
            Some("scope expansion"),
            &mut w,
            &mut r,
        );

        let sids: Vec<String> = captured_sorted(&shared)
            .into_iter()
            .map(|(s, _)| s)
            .collect();
        assert!(sids.contains(&"s-root".to_string()), "{:?}", sids);
    }

    #[test]
    fn notify_no_root_broadcast_on_closed() {
        let db = TasksDb::open_memory().expect("db");
        let mut task = create_task(&db, "C", None, "ready");
        db.record_session(task.id, "s-worker", "worker")
            .expect("rec");
        db.set_session_id(task.id, "s-worker").expect("sid");
        task.session_id = Some("s-worker".into());
        task.state = TaskState::Closed;

        let (shared, mut w, mut r) = make_io();
        shared.lock().expect("lock").ancestors.insert(
            "s-worker".into(),
            vec![
                fake_session("s-worker", Some("s-root"), false),
                fake_session("s-root", None, false),
            ],
        );

        notify_state_change(&db, &task, TaskState::Active, None, &mut w, &mut r);

        let sids: Vec<String> = captured_sorted(&shared)
            .into_iter()
            .map(|(s, _)| s)
            .collect();
        assert!(
            !sids.contains(&"s-root".to_string()),
            "root leaked on closed: {:?}",
            sids
        );
        assert!(sids.contains(&"s-worker".to_string()), "{:?}", sids);
    }

    #[test]
    fn notify_root_broadcast_skipped_when_root_archived() {
        let db = TasksDb::open_memory().expect("db");
        let mut task = create_task(&db, "AR", None, "ready");
        db.record_session(task.id, "s-worker", "worker")
            .expect("rec");
        db.set_session_id(task.id, "s-worker").expect("sid");
        task.session_id = Some("s-worker".into());
        task.state = TaskState::Merged;

        let (shared, mut w, mut r) = make_io();
        {
            let mut s = shared.lock().expect("lock");
            s.archived_sessions.insert("s-root".into());
            s.ancestors.insert(
                "s-worker".into(),
                vec![
                    fake_session("s-worker", Some("s-root"), false),
                    fake_session("s-root", None, /* archived */ true),
                ],
            );
        }

        notify_state_change(&db, &task, TaskState::Merging, None, &mut w, &mut r);

        let sids: Vec<String> = captured_sorted(&shared)
            .into_iter()
            .map(|(s, _)| s)
            .collect();
        assert!(
            !sids.contains(&"s-root".to_string()),
            "archived root leaked: {:?}",
            sids
        );
        // s-worker still notified (the broadcast didn't abort).
        assert!(sids.contains(&"s-worker".to_string()), "{:?}", sids);
    }

    // -----------------------------------------------------------------
    // Robustness
    // -----------------------------------------------------------------

    #[test]
    fn notify_survives_missing_sessions() {
        // Parent task reference points at a deleted row: the lookup
        // silently returns an empty list and the rest of the broadcast
        // still happens.
        let db = TasksDb::open_memory().expect("db");
        let mut task = create_task(&db, "Mid", None, "ready");
        db.record_session(task.id, "s-a", "worker").expect("rec a");
        db.record_session(task.id, "s-b", "reviewer")
            .expect("rec b");
        task.state = TaskState::Review;
        // Fabricate a parent_id pointing at nothing — get_sessions for
        // non-existent task returns Ok(empty) in the current impl, so
        // this exercises the "chain-element missing" path.
        task.parent_id = Some(999_999);

        let (shared, mut w, mut r) = make_io();
        notify_state_change(&db, &task, TaskState::Active, None, &mut w, &mut r);

        let sids: Vec<String> = captured_sorted(&shared)
            .into_iter()
            .map(|(s, _)| s)
            .collect();
        assert!(sids.contains(&"s-a".to_string()), "{:?}", sids);
        assert!(sids.contains(&"s-b".to_string()), "{:?}", sids);
    }

    // -----------------------------------------------------------------
    // Integration-style: full lifecycle
    // -----------------------------------------------------------------

    /// A task goes through
    ///   planning → refining → ready → active → review → approved → merging → merged
    /// with a fixed creator session that stays recorded throughout.  The
    /// creator should observe exactly seven info-messages in order.
    #[test]
    fn notify_lifecycle_delivers_seven_messages_in_order() {
        let db = TasksDb::open_memory().expect("db");
        let mut task = create_task(&db, "Life", None, "planning");
        db.record_session(task.id, "s-creator", "creator")
            .expect("rec");

        let (shared, mut w, mut r) = make_io();

        let steps = [
            (TaskState::Planning, TaskState::Refining, None),
            (TaskState::Refining, TaskState::Ready, None),
            (TaskState::Ready, TaskState::Active, None),
            (TaskState::Active, TaskState::Review, None),
            (TaskState::Review, TaskState::Approved, None),
            (TaskState::Approved, TaskState::Merging, None),
            (
                TaskState::Merging,
                TaskState::Merged,
                Some("commit cafef00d"),
            ),
        ];

        for (from, to, ctx) in steps {
            task.state = to;
            notify_state_change(&db, &task, from, ctx, &mut w, &mut r);
        }

        // Only s-creator was ever recorded against the task; it should
        // receive one message per step.  (merged also broadcasts to the
        // root but since we didn't configure ancestors, the default root
        // is s-creator itself — which dedups.)
        let calls = shared.lock().expect("lock").queue_info_calls.clone();
        let creator_msgs: Vec<&str> = calls
            .iter()
            .filter(|(sid, _)| sid == "s-creator")
            .map(|(_, t)| t.as_str())
            .collect();

        let id = task.id;
        let expected: Vec<String> = vec![
            format!("[task #{}] Life: planning → refining", id),
            format!("[task #{}] Life: refining → ready", id),
            format!("[task #{}] Life: ready → active", id),
            format!("[task #{}] Life: active → review", id),
            format!("[task #{}] Life: review → approved", id),
            format!("[task #{}] Life: approved → merging", id),
            format!("[task #{}] Life: merged (commit cafef00d)", id),
        ];
        let expected_refs: Vec<&str> = expected.iter().map(String::as_str).collect();

        assert_eq!(creator_msgs, expected_refs, "full message log mismatch");
    }

    // -----------------------------------------------------------------
    // Creator QueueMessage on terminal transitions (task #658)
    // -----------------------------------------------------------------

    /// Helper: get captured QueueMessage calls in deterministic order.
    fn captured_messages_sorted(
        shared: &Arc<Mutex<MockShared>>,
    ) -> Vec<(String, String, String, bool)> {
        let mut calls = shared
            .lock()
            .expect("mock shared lock")
            .queue_message_calls
            .clone();
        calls.sort();
        calls
    }

    /// Terminal `Merged` transition: creator receives both the existing
    /// QueueInfo line AND a new LLM-visible QueueMessage. Top-level task
    /// wording is "Task #N" (no parent).
    #[test]
    fn terminal_merge_sends_queue_message_to_creator() {
        let db = TasksDb::open_memory().expect("db");
        let mut task = create_task(&db, "Working spinner", None, "ready");
        db.record_session(task.id, "s-creator", "creator")
            .expect("rec creator");
        task.state = TaskState::Merged;

        let (shared, mut w, mut r) = make_io();
        notify_state_change(
            &db,
            &task,
            TaskState::Merging,
            Some("commit cafef00d"),
            &mut w,
            &mut r,
        );

        // Existing fanout: creator receives the QueueInfo line.
        let info_calls = captured_sorted(&shared);
        let info_to_creator: Vec<&str> = info_calls
            .iter()
            .filter(|(sid, _)| sid == "s-creator")
            .map(|(_, t)| t.as_str())
            .collect();
        assert_eq!(
            info_to_creator.len(),
            1,
            "creator should get one QueueInfo: {:?}",
            info_calls
        );

        // New behaviour: exactly one QueueMessage to the creator.
        let msg_calls = captured_messages_sorted(&shared);
        let to_creator: Vec<&(String, String, String, bool)> = msg_calls
            .iter()
            .filter(|(sid, _, _, _)| sid == "s-creator")
            .collect();
        assert_eq!(
            to_creator.len(),
            1,
            "creator should get exactly one QueueMessage: {:?}",
            msg_calls
        );
        let (_, content, sender_info, await_reply) = to_creator[0];
        assert!(
            content.contains(&format!("Task #{}", task.id)),
            "content missing task id: {:?}",
            content
        );
        assert!(
            content.contains("merged"),
            "content missing 'merged': {:?}",
            content
        );
        assert!(
            content.contains("commit cafef00d"),
            "content missing commit context: {:?}",
            content
        );
        assert!(
            content.contains("Working spinner"),
            "content missing title: {:?}",
            content
        );
        assert_eq!(sender_info, "task notifier");
        assert!(!*await_reply, "await_reply should be false");
    }

    /// Terminal `Closed`: creator QueueMessage uses "closed" wording.
    /// Subtask wording (parent_id set) is "Subtask #N".
    #[test]
    fn terminal_closed_sends_queue_message_to_creator() {
        let db = TasksDb::open_memory().expect("db");
        let parent = create_task(&db, "Parent", None, "ready");
        let mut task = create_task(&db, "Renaming foo", Some(parent.id), "ready");
        db.record_session(task.id, "s-creator", "creator")
            .expect("rec creator");
        task.state = TaskState::Closed;

        let (shared, mut w, mut r) = make_io();
        notify_state_change(&db, &task, TaskState::Ready, None, &mut w, &mut r);

        let msg_calls = captured_messages_sorted(&shared);
        let to_creator: Vec<&(String, String, String, bool)> = msg_calls
            .iter()
            .filter(|(sid, _, _, _)| sid == "s-creator")
            .collect();
        assert_eq!(
            to_creator.len(),
            1,
            "creator should get one QueueMessage: {:?}",
            msg_calls
        );
        let content = &to_creator[0].1;
        assert!(
            content.contains(&format!("Subtask #{}", task.id)),
            "expected 'Subtask #N': {:?}",
            content
        );
        assert!(
            content.contains("closed"),
            "content missing 'closed': {:?}",
            content
        );
    }

    /// Terminal `Failed`: creator QueueMessage includes the failure
    /// reason after a colon.
    #[test]
    fn terminal_failed_sends_queue_message_to_creator() {
        let db = TasksDb::open_memory().expect("db");
        let parent = create_task(&db, "Parent", None, "ready");
        let mut task = create_task(&db, "Refactor x", Some(parent.id), "ready");
        db.record_session(task.id, "s-creator", "creator")
            .expect("rec creator");
        task.state = TaskState::Failed;

        let (shared, mut w, mut r) = make_io();
        notify_state_change(
            &db,
            &task,
            TaskState::Merging,
            Some("checklist failed"),
            &mut w,
            &mut r,
        );

        let msg_calls = captured_messages_sorted(&shared);
        let to_creator: Vec<&(String, String, String, bool)> = msg_calls
            .iter()
            .filter(|(sid, _, _, _)| sid == "s-creator")
            .collect();
        assert_eq!(to_creator.len(), 1, "{:?}", msg_calls);
        let content = &to_creator[0].1;
        assert!(
            content.contains("failed: checklist failed"),
            "content: {:?}",
            content
        );
        assert!(
            content.contains(&format!("Subtask #{}", task.id)),
            "content: {:?}",
            content
        );
    }

    /// Non-terminal transitions never send a QueueMessage — only the
    /// existing QueueInfo fanout.
    #[test]
    fn non_terminal_transitions_do_not_send_queue_message() {
        let db = TasksDb::open_memory().expect("db");
        let mut task = create_task(&db, "NT", None, "planning");
        db.record_session(task.id, "s-creator", "creator")
            .expect("rec creator");

        let (shared, mut w, mut r) = make_io();

        let steps = [
            (TaskState::Planning, TaskState::Refining),
            (TaskState::Refining, TaskState::Ready),
            (TaskState::Ready, TaskState::Active),
            (TaskState::Active, TaskState::Review),
            (TaskState::Review, TaskState::Approved),
            (TaskState::Approved, TaskState::Merging),
        ];
        for (from, to) in steps {
            task.state = to;
            notify_state_change(&db, &task, from, None, &mut w, &mut r);
        }

        // Sanity: QueueInfo fired.
        assert!(!captured_sorted(&shared).is_empty(), "expected info calls");
        // The point of the test: no QueueMessage on any non-terminal
        // transition.
        assert!(
            captured_messages_sorted(&shared).is_empty(),
            "unexpected QueueMessage: {:?}",
            captured_messages_sorted(&shared)
        );
    }

    /// Archived creator: neither QueueInfo nor QueueMessage should
    /// reach it. Pin the existing archived-filter behaviour too so a
    /// future refactor can't silently leak info lines to dismissed
    /// sessions.
    #[test]
    fn queue_message_not_sent_when_creator_archived() {
        let db = TasksDb::open_memory().expect("db");
        let mut task = create_task(&db, "ArcCreator", None, "ready");
        db.record_session(task.id, "s-creator", "creator")
            .expect("rec");
        task.state = TaskState::Merged;

        let (shared, mut w, mut r) = make_io();
        shared
            .lock()
            .expect("mock lock")
            .archived_sessions
            .insert("s-creator".into());

        notify_state_change(&db, &task, TaskState::Merging, None, &mut w, &mut r);

        // No QueueMessage for the archived creator (new behaviour).
        let msg_calls = captured_messages_sorted(&shared);
        assert!(
            msg_calls.iter().all(|(sid, _, _, _)| sid != "s-creator"),
            "archived creator got a QueueMessage: {:?}",
            msg_calls
        );
        // No QueueInfo either (existing collect_recipients filter).
        let info_calls = captured_sorted(&shared);
        assert!(
            info_calls.iter().all(|(sid, _)| sid != "s-creator"),
            "archived creator got a QueueInfo: {:?}",
            info_calls
        );
    }

    /// Task with no recorded creator (older task, or task created by a
    /// path that doesn't stamp the role): skip silently — no
    /// QueueMessage sent at all.
    #[test]
    fn queue_message_not_sent_when_no_creator_recorded() {
        let db = TasksDb::open_memory().expect("db");
        let mut task = create_task(&db, "NoCreator", None, "ready");
        db.record_session(task.id, "s-worker", "worker")
            .expect("rec w");
        db.record_session(task.id, "s-reviewer", "reviewer")
            .expect("rec r");
        task.state = TaskState::Merged;

        let (shared, mut w, mut r) = make_io();
        notify_state_change(&db, &task, TaskState::Merging, None, &mut w, &mut r);

        let msg_calls = captured_messages_sorted(&shared);
        assert!(
            msg_calls.is_empty(),
            "expected no QueueMessage: {:?}",
            msg_calls
        );
    }

    /// Other recipients (worker, reviewer, parent creator) get
    /// QueueInfo only; no QueueMessage is sent to them. Only the
    /// creator receives the new LLM-visible wake.
    #[test]
    fn other_recipients_get_info_only_on_terminal() {
        let db = TasksDb::open_memory().expect("db");
        let parent = create_task(&db, "Parent", None, "ready");
        db.record_session(parent.id, "s-pcreator", "creator")
            .expect("rec pc");
        // Give parent a distinct session_id so the dedup path doesn't
        // suppress the new creator QueueMessage.
        db.set_session_id(parent.id, "s-parent-worker")
            .expect("psid");

        let mut child = create_task(&db, "Child", Some(parent.id), "ready");
        db.record_session(child.id, "s-creator", "creator")
            .expect("rec c");
        db.record_session(child.id, "s-worker", "worker")
            .expect("rec w");
        db.record_session(child.id, "s-reviewer", "reviewer")
            .expect("rec r");
        child.state = TaskState::Merged;

        let (shared, mut w, mut r) = make_io();
        notify_state_change(&db, &child, TaskState::Merging, None, &mut w, &mut r);

        // QueueMessage: exactly one, to s-creator.
        let msg_calls = captured_messages_sorted(&shared);
        assert_eq!(
            msg_calls.len(),
            1,
            "expected exactly one QueueMessage: {:?}",
            msg_calls
        );
        assert_eq!(msg_calls[0].0, "s-creator");

        // QueueInfo: covers worker, reviewer, parent creator, creator.
        let info_sids: Vec<String> = captured_sorted(&shared)
            .into_iter()
            .map(|(s, _)| s)
            .collect();
        assert!(info_sids.contains(&"s-creator".into()), "{:?}", info_sids);
        assert!(info_sids.contains(&"s-worker".into()), "{:?}", info_sids);
        assert!(info_sids.contains(&"s-reviewer".into()), "{:?}", info_sids);
        assert!(info_sids.contains(&"s-pcreator".into()), "{:?}", info_sids);
    }

    /// Dedup: when the subtask's creator session is identical to the
    /// parent task's current `session_id`, skip the new QueueMessage.
    /// `tasks_merge::notify_parent_of_subtask_done` already covers that
    /// session via its own QueueMessage, so we'd be sending two
    /// LLM-visible wakes for the same event otherwise.
    #[test]
    fn queue_message_deduped_when_creator_is_parent_session() {
        let db = TasksDb::open_memory().expect("db");
        let parent = create_task(&db, "Parent", None, "ready");
        db.set_session_id(parent.id, "s-shared").expect("psid");

        let mut child = create_task(&db, "Child", Some(parent.id), "ready");
        db.record_session(child.id, "s-shared", "creator")
            .expect("rec c");
        child.state = TaskState::Merged;

        let (shared, mut w, mut r) = make_io();
        notify_state_change(&db, &child, TaskState::Merging, None, &mut w, &mut r);

        let msg_calls = captured_messages_sorted(&shared);
        assert!(
            msg_calls.is_empty(),
            "creator==parent.session_id should dedup, got: {:?}",
            msg_calls
        );
    }

    /// Non-dedup path: creator distinct from parent.session_id, so the
    /// new QueueMessage fires normally.
    #[test]
    fn queue_message_sent_when_creator_differs_from_parent_session() {
        let db = TasksDb::open_memory().expect("db");
        let parent = create_task(&db, "Parent", None, "ready");
        db.set_session_id(parent.id, "s-parent-worker")
            .expect("psid");

        let mut child = create_task(&db, "Child", Some(parent.id), "ready");
        db.record_session(child.id, "s-creator", "creator")
            .expect("rec c");
        child.state = TaskState::Merged;

        let (shared, mut w, mut r) = make_io();
        notify_state_change(&db, &child, TaskState::Merging, None, &mut w, &mut r);

        let msg_calls = captured_messages_sorted(&shared);
        let to_creator: Vec<&(String, String, String, bool)> = msg_calls
            .iter()
            .filter(|(sid, _, _, _)| sid == "s-creator")
            .collect();
        assert_eq!(to_creator.len(), 1, "{:?}", msg_calls);
    }

    /// Top-level (no parent) terminal transition: the dedup lookup is
    /// safe on `parent_id = None` (no false match, no panic), and the
    /// content uses "Task #N" wording.
    #[test]
    fn queue_message_sent_for_top_level_task_with_no_parent() {
        let db = TasksDb::open_memory().expect("db");
        let mut task = create_task(&db, "Top", None, "ready");
        db.record_session(task.id, "s-creator", "creator")
            .expect("rec");
        task.state = TaskState::Merged;

        let (shared, mut w, mut r) = make_io();
        notify_state_change(&db, &task, TaskState::Merging, None, &mut w, &mut r);

        let msg_calls = captured_messages_sorted(&shared);
        let to_creator: Vec<&(String, String, String, bool)> = msg_calls
            .iter()
            .filter(|(sid, _, _, _)| sid == "s-creator")
            .collect();
        assert_eq!(to_creator.len(), 1, "{:?}", msg_calls);
        let content = &to_creator[0].1;
        assert!(
            content.contains(&format!("Task #{}", task.id)),
            "expected 'Task #N' for top-level: {:?}",
            content
        );
        assert!(
            !content.contains("Subtask #"),
            "top-level task should not say 'Subtask': {:?}",
            content
        );
    }

    // -----------------------------------------------------------------
    // task_session_tagline
    // -----------------------------------------------------------------

    #[test]
    fn task_session_tagline_basic() {
        let db = TasksDb::open_memory().expect("db");
        let task = create_task(&db, "Short title", None, "ready");
        for role in [
            "interactive",
            "planning",
            "refining",
            "worker",
            "review",
            "merge",
        ] {
            assert_eq!(
                task_session_tagline(&task, role),
                format!("[task {}] {}: Short title", task.id, role),
                "role={}",
                role
            );
        }
    }

    #[test]
    fn task_session_tagline_title_cap_unchanged_at_limit() {
        // Exactly TAGLINE_TITLE_MAX chars → passes through untouched.
        let db = TasksDb::open_memory().expect("db");
        let t = "x".repeat(TAGLINE_TITLE_MAX);
        let task = create_task(&db, &t, None, "ready");
        let line = task_session_tagline(&task, "worker");
        assert_eq!(
            line,
            format!(
                "[task {}] worker: {}",
                task.id,
                "x".repeat(TAGLINE_TITLE_MAX)
            )
        );
    }

    #[test]
    fn task_session_tagline_title_cap_truncates_long_titles() {
        // 100 chars, cap is TAGLINE_TITLE_MAX=80 → 79 'x' + '…'.
        let db = TasksDb::open_memory().expect("db");
        let t = "x".repeat(100);
        let task = create_task(&db, &t, None, "ready");
        let line = task_session_tagline(&task, "worker");
        let expected_title = {
            let mut s: String = "x".repeat(TAGLINE_TITLE_MAX - 1);
            s.push('…');
            s
        };
        assert_eq!(
            line,
            format!("[task {}] worker: {}", task.id, expected_title)
        );
        // The rendered title portion has exactly TAGLINE_TITLE_MAX chars
        // (head + ellipsis).
        assert_eq!(expected_title.chars().count(), TAGLINE_TITLE_MAX);
    }

    #[test]
    fn task_session_tagline_utf8_respects_char_boundaries() {
        // Each '☃' is 3 bytes but 1 char. 100 chars → 300 bytes.
        // Must truncate at char boundary without producing broken UTF-8.
        let db = TasksDb::open_memory().expect("db");
        let t = "☃".repeat(100);
        let task = create_task(&db, &t, None, "ready");
        let line = task_session_tagline(&task, "refining");
        // Still valid UTF-8 (implicit: String guarantees it), and the
        // tagline's title portion is TAGLINE_TITLE_MAX chars.
        let prefix = format!("[task {}] refining: ", task.id);
        let title_part = &line[prefix.len()..];
        assert_eq!(title_part.chars().count(), TAGLINE_TITLE_MAX);
        assert!(title_part.ends_with('…'));
        assert!(title_part.starts_with('☃'));
        // All snowmen except the last char.
        let snow_count = title_part.chars().filter(|c| *c == '☃').count();
        assert_eq!(snow_count, TAGLINE_TITLE_MAX - 1);
    }

    // -----------------------------------------------------------------
    // task_placeholder_tagline
    // -----------------------------------------------------------------

    #[test]
    fn task_placeholder_tagline_basic() {
        let db = TasksDb::open_memory().expect("db");
        let task = create_task(&db, "Short title", None, "ready");
        assert_eq!(
            task_placeholder_tagline(&task),
            format!("[task {}] Short title", task.id),
        );
    }

    #[test]
    fn task_placeholder_tagline_truncates_long_titles() {
        let db = TasksDb::open_memory().expect("db");
        let t = "x".repeat(100);
        let task = create_task(&db, &t, None, "ready");
        let line = task_placeholder_tagline(&task);
        let prefix = format!("[task {}] ", task.id);
        let title_part = &line[prefix.len()..];
        assert_eq!(title_part.chars().count(), TAGLINE_TITLE_MAX);
        assert!(title_part.ends_with('…'));
    }

    #[test]
    fn truncate_title_short_is_unchanged() {
        assert_eq!(truncate_title("hello", 80), "hello");
    }

    #[test]
    fn truncate_title_exact_is_unchanged() {
        let s = "x".repeat(80);
        assert_eq!(truncate_title(&s, 80), s);
    }

    #[test]
    fn truncate_title_over_limit_gets_ellipsis() {
        let s = "x".repeat(81);
        let out = truncate_title(&s, 80);
        assert_eq!(out.chars().count(), 80);
        assert!(out.ends_with('…'));
        let head: String = "x".repeat(79);
        assert_eq!(out, format!("{}…", head));
    }

    // -----------------------------------------------------------------
    // set_session_tagline (RPC wiring)
    // -----------------------------------------------------------------

    /// set_session_tagline sends a SetTagline request with the given
    /// session id and tagline text. The mock captures the request so we
    /// can assert on it directly.
    #[test]
    fn set_session_tagline_sends_rpc() {
        let (shared, mut w, mut r) = make_io();
        set_session_tagline("s-abc", "[task 7] refining: x", &mut w, &mut r);

        let calls = shared.lock().expect("lock").set_tagline_calls.clone();
        assert_eq!(calls.len(), 1, "calls: {:?}", calls);
        assert_eq!(calls[0].0, "s-abc");
        assert_eq!(calls[0].1, "[task 7] refining: x");
    }

    // -----------------------------------------------------------------
    // Role-transition tagline updates (scheduler integration)
    // -----------------------------------------------------------------

    /// Simulate the scheduler-level reuse of a planner session in a new
    /// refining cycle: when dispatch_refining (conceptually) reuses an
    /// existing session, it must update that session's tagline so the
    /// TUI session tree reflects the new role.
    ///
    /// We exercise this at the unit level: call
    /// `set_session_tagline` with the tagline the scheduler would use
    /// for a refiner role, and assert the captured RPC has the
    /// `refining:` prefix (not the stale `planning:`).
    #[test]
    fn tagline_updated_on_role_transition() {
        let db = TasksDb::open_memory().expect("db");
        let task = create_task(&db, "Example", None, "planning");
        let reused_sid = "s-reused";

        let (shared, mut w, mut r) = make_io();
        // Emulate scheduler.dispatch_refining finding a reusable session
        // and refreshing its tagline with the new role.
        let new_tagline = task_session_tagline(&task, "refining");
        set_session_tagline(reused_sid, &new_tagline, &mut w, &mut r);

        let calls = shared.lock().expect("lock").set_tagline_calls.clone();
        assert_eq!(calls.len(), 1, "calls: {:?}", calls);
        assert_eq!(calls[0].0, reused_sid);
        assert_eq!(calls[0].1, format!("[task {}] refining: Example", task.id));
        // Sanity check: it is not the old planning tagline.
        assert!(!calls[0].1.contains("planning:"));
    }

    // -----------------------------------------------------------------
    // Placeholder session messages (task #574)
    // -----------------------------------------------------------------

    /// `collect_recipients` / `notify_state_change` must include the
    /// task's placeholder session in every broadcast.
    #[test]
    fn placeholder_receives_state_transition_message() {
        let db = TasksDb::open_memory().expect("db");
        let mut task = create_task(&db, "PH", None, "ready");
        db.set_placeholder_session_id(task.id, "s-ph").expect("ph");
        db.record_session(task.id, "s-worker", "worker")
            .expect("rec worker");
        task.placeholder_session_id = Some("s-ph".into());
        task.state = TaskState::Active;

        let (shared, mut w, mut r) = make_io();
        notify_state_change(&db, &task, TaskState::Ready, None, &mut w, &mut r);

        let calls = captured_sorted(&shared);
        let sids: Vec<&str> = calls.iter().map(|(s, _)| s.as_str()).collect();
        assert!(
            sids.contains(&"s-ph"),
            "placeholder missing from recipients: {:?}",
            sids
        );
        // Text is the normal transition line.
        let expected = format!("[task #{}] PH: ready → active", task.id);
        for (sid, text) in &calls {
            if sid == "s-ph" {
                assert_eq!(text, &expected);
            }
        }
    }

    /// Identical-state "transitions" remain a no-op even with the
    /// placeholder in the recipient set.
    #[test]
    fn placeholder_no_message_on_identical_state() {
        let db = TasksDb::open_memory().expect("db");
        let mut task = create_task(&db, "Same", None, "ready");
        db.set_placeholder_session_id(task.id, "s-ph").expect("ph");
        task.placeholder_session_id = Some("s-ph".into());
        task.state = TaskState::Ready;

        let (shared, mut w, mut r) = make_io();
        notify_state_change(&db, &task, TaskState::Ready, None, &mut w, &mut r);

        assert!(captured_sorted(&shared).is_empty());
    }

    /// Terminal-state transitions post both the normal transition line
    /// AND a summary line to the placeholder.
    #[test]
    fn placeholder_terminal_transition_posts_summary() {
        let db = TasksDb::open_memory().expect("db");
        let mut task = create_task(&db, "Term", None, "ready");
        db.set_placeholder_session_id(task.id, "s-ph").expect("ph");
        db.record_session(task.id, "s-worker", "worker")
            .expect("rec");
        task.placeholder_session_id = Some("s-ph".into());
        task.state = TaskState::Merged;

        let (shared, mut w, mut r) = make_io();
        notify_state_change(&db, &task, TaskState::Merging, None, &mut w, &mut r);

        let calls = shared.lock().expect("lock").queue_info_calls.clone();
        let ph_msgs: Vec<&str> = calls
            .iter()
            .filter(|(sid, _)| sid == "s-ph")
            .map(|(_, t)| t.as_str())
            .collect();
        assert_eq!(
            ph_msgs.len(),
            2,
            "expected transition + summary on placeholder: {:?}",
            ph_msgs
        );
        assert!(
            ph_msgs
                .iter()
                .any(|m| m.contains("merged") && m.contains(&format!("[task #{}]", task.id))),
            "transition line missing: {:?}",
            ph_msgs
        );
        assert!(
            ph_msgs
                .iter()
                .any(|m| m.starts_with(&format!("Task #{} merged.", task.id))),
            "summary line missing: {:?}",
            ph_msgs
        );
    }

    /// Non-terminal transitions do NOT post a summary line.
    #[test]
    fn placeholder_non_terminal_transition_no_summary() {
        let db = TasksDb::open_memory().expect("db");
        let mut task = create_task(&db, "NT", None, "ready");
        db.set_placeholder_session_id(task.id, "s-ph").expect("ph");
        task.placeholder_session_id = Some("s-ph".into());
        task.state = TaskState::Active;

        let (shared, mut w, mut r) = make_io();
        notify_state_change(&db, &task, TaskState::Ready, None, &mut w, &mut r);

        let calls = shared.lock().expect("lock").queue_info_calls.clone();
        let ph_msgs: Vec<&str> = calls
            .iter()
            .filter(|(sid, _)| sid == "s-ph")
            .map(|(_, t)| t.as_str())
            .collect();
        assert_eq!(ph_msgs.len(), 1, "{:?}", ph_msgs);
    }

    /// When the placeholder session has been archived externally, the
    /// terminal-state summary is suppressed — consistent with
    /// `collect_recipients` which also filters archived targets.
    #[test]
    fn placeholder_archived_skips_terminal_summary() {
        let db = TasksDb::open_memory().expect("db");
        let mut task = create_task(&db, "ArcTerm", None, "ready");
        db.set_placeholder_session_id(task.id, "s-ph").expect("ph");
        task.placeholder_session_id = Some("s-ph".into());
        task.state = TaskState::Merged;

        let (shared, mut w, mut r) = make_io();
        shared
            .lock()
            .expect("mock shared lock")
            .archived_sessions
            .insert("s-ph".into());

        notify_state_change(&db, &task, TaskState::Merging, None, &mut w, &mut r);

        let calls = shared.lock().expect("lock").queue_info_calls.clone();
        let ph_msgs: Vec<&str> = calls
            .iter()
            .filter(|(sid, _)| sid == "s-ph")
            .map(|(_, t)| t.as_str())
            .collect();
        // collect_recipients filters the archived placeholder out of the
        // fan-out, and the terminal-summary branch honours the same
        // check — so zero messages land on s-ph.
        assert!(
            ph_msgs.is_empty(),
            "no messages should reach archived placeholder: {:?}",
            ph_msgs
        );
    }

    /// `notify_task_created` posts exactly one message to the
    /// placeholder, formatted with the task metadata.
    #[test]
    fn task_created_posts_initial_message_to_placeholder() {
        let db = TasksDb::open_memory().expect("db");
        let mut task = create_task(&db, "Created", None, "planning");
        db.set_placeholder_session_id(task.id, "s-ph").expect("ph");
        task.placeholder_session_id = Some("s-ph".into());

        let (shared, mut w, mut r) = make_io();
        notify_task_created(&task, &mut w, &mut r);

        let calls = captured_sorted(&shared);
        assert_eq!(calls.len(), 1, "{:?}", calls);
        assert_eq!(calls[0].0, "s-ph");
        let text = &calls[0].1;
        assert!(
            text.starts_with(&format!("Task #{} created: Created", task.id)),
            "text: {:?}",
            text
        );
        // Metadata lines are present.
        assert!(text.contains("\nPriority: "), "text: {:?}", text);
        assert!(text.contains("\nTags: "), "text: {:?}", text);
        assert!(
            text.contains("\nParent task: (top-level)"),
            "text: {:?}",
            text
        );
        assert!(text.contains("\nRequire approval: no"), "text: {:?}", text);
        assert!(text.contains("\nSkip review: no"), "text: {:?}", text);
        assert!(
            text.contains("\nMerge target: (default)"),
            "text: {:?}",
            text
        );
        assert!(
            text.contains("\nInitial state: planning"),
            "text: {:?}",
            text
        );
    }

    /// `notify_task_created` is a no-op when the placeholder id is
    /// missing (creation failed upstream). No QueueInfo is sent.
    #[test]
    fn task_created_no_placeholder_is_noop() {
        let db = TasksDb::open_memory().expect("db");
        let task = create_task(&db, "NoPH", None, "ready");
        assert!(task.placeholder_session_id.is_none());

        let (shared, mut w, mut r) = make_io();
        notify_task_created(&task, &mut w, &mut r);

        assert!(shared.lock().expect("lock").queue_info_calls.is_empty());
    }

    /// `format_task_created` renders subtasks with a `#<parent>` marker
    /// and surfaces tags / non-default settings.
    #[test]
    fn format_task_created_subtask_with_tags_and_flags() {
        let db = TasksDb::open_memory().expect("db");
        let parent = create_task(&db, "Parent", None, "planning");
        let mut task = create_task(&db, "Child", Some(parent.id), "ready");
        // Adjust fields the create_task helper doesn't set directly.
        task.tags = Some(serde_json::json!(["backend", "urgent"]));
        task.require_approval = true;
        task.skip_review = true;
        task.merge_target = Some("release".into());
        task.placeholder_session_id = Some("s-ph".into());

        let text = format_task_created(&task);
        assert!(text.starts_with(&format!("Task #{} created: Child", task.id)));
        assert!(text.contains(&format!("\nParent task: #{}\n", parent.id)));
        assert!(text.contains("\nTags: backend, urgent\n"));
        assert!(text.contains("\nRequire approval: yes\n"));
        assert!(text.contains("\nSkip review: yes\n"));
        assert!(text.contains("\nMerge target: release\n"));
        assert!(text.ends_with("\nInitial state: ready"));
    }

    /// `notify_placeholder_wait` fires a single QueueInfo to the task's
    /// placeholder with the given text; no-op if no placeholder.
    #[test]
    fn notify_placeholder_wait_posts_single_message() {
        let db = TasksDb::open_memory().expect("db");
        let mut task = create_task(&db, "Wait", None, "ready");
        task.placeholder_session_id = Some("s-ph".into());

        let (shared, mut w, mut r) = make_io();
        notify_placeholder_wait(&task, "Waiting: test reason", &mut w, &mut r);

        let calls = shared.lock().expect("lock").queue_info_calls.clone();
        assert_eq!(calls.len(), 1, "{:?}", calls);
        assert_eq!(calls[0].0, "s-ph");
        assert_eq!(calls[0].1, "Waiting: test reason");

        // Missing placeholder → no-op.
        let task2 = create_task(&db, "Wait2", None, "ready");
        let (shared2, mut w2, mut r2) = make_io();
        notify_placeholder_wait(&task2, "Waiting: x", &mut w2, &mut r2);
        assert!(shared2.lock().expect("lock").queue_info_calls.is_empty());
    }

    // -----------------------------------------------------------------
    // format_duration_ms
    // -----------------------------------------------------------------

    #[test]
    fn format_duration_ms_ranges() {
        assert_eq!(format_duration_ms(0), "0ms");
        assert_eq!(format_duration_ms(42), "42ms");
        assert_eq!(format_duration_ms(1_500), "1.5s");
        assert_eq!(format_duration_ms(45 * 1_000), "45.0s");
        assert_eq!(format_duration_ms(90 * 1_000), "1m30s");
        assert_eq!(format_duration_ms(3_600_000 + 5_000), "1h00m");
        assert_eq!(format_duration_ms(86_400_000 * 2), "2d00h");
    }

    /// Negative values coming from clock skew map to "0ms" (defensive).
    #[test]
    fn format_duration_ms_negative_clamps_to_zero() {
        assert_eq!(format_duration_ms(-5), "0ms");
    }
}
