use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use super::agent_runner::resume_child_session;
use super::state::{SessionLocks, SharedState, lock_state, session_lock};
use super::{SharedTestOverrides, ShutdownHandle};
use crate::protocol::Response;
use crate::truncate_str;
use crate::types::*;

/// Queue a message for delivery to a target session.
/// Persists immediately and sets the has_queued flag for in-flight agent loops.
pub(super) fn queue_message_to_session(
    state: &SharedState,
    target: &str,
    content: &str,
    sender_info: &str,
) {
    let mut st = lock_state(state);
    st.db.queue_message(target, content, sender_info).ok();
    st.has_queued
        .entry(target.to_string())
        .or_insert_with(|| Arc::new(AtomicBool::new(false)))
        .store(true, Ordering::Release);
}

/// Persist an info message directly to a session's message history.
/// Unlike `queue_message_to_session`, this does NOT wake the agent loop —
/// the message is display-only and excluded from LLM context.
pub(super) fn queue_info_to_session(state: &SharedState, target: &str, text: &str) {
    let info_msg = Message::Info(crate::types::InfoMessage::new(text));
    let st = lock_state(state);
    if let Err(e) = st.db.append_message(target, &info_msg) {
        eprintln!("failed to persist info message: {}", e);
    }
}

/// Queue a message and, if the target session is idle, spawn a resume task so
/// the message is processed without waiting for the next user interaction.
#[allow(clippy::too_many_arguments)]
pub(super) fn queue_and_maybe_resume(
    state: &SharedState,
    session_locks: &SessionLocks,
    plugins: &Arc<Mutex<crate::plugin::PluginManager>>,
    shutdown: &ShutdownHandle,
    throttle: &crate::throttle::ProviderThrottle,
    target: &str,
    content: &str,
    sender_info: &str,
    test_overrides: &SharedTestOverrides,
) {
    queue_message_to_session(state, target, content, sender_info);
    // If the target session is idle (lock is free), spawn a resume task.
    let needs_resume = {
        let lock = session_lock(session_locks, target);
        lock.try_lock().is_some()
    };
    if needs_resume {
        let s = state.clone();
        let p = plugins.clone();
        let sh = shutdown.clone();
        let sl = session_locks.clone();
        let th = throttle.clone();
        let ov = test_overrides.clone();
        let sid = target.to_string();
        smol::spawn(async move {
            if let Err(e) = resume_child_session(s, p, sh, sl, th, sid.clone(), ov).await {
                eprintln!("resume session {} after queued message: {}", sid, e);
            }
        })
        .detach();
    }
}

/// Notify a child session's parent that the child has completed.
/// Skipped if the parent is actively waiting on this child via WaitSessions/WaitAnySessions.
#[allow(clippy::too_many_arguments)]
pub(super) fn notify_parent_of_child_completion(
    state: &SharedState,
    session_locks: &SessionLocks,
    plugins: &Arc<Mutex<crate::plugin::PluginManager>>,
    shutdown: &ShutdownHandle,
    throttle: &crate::throttle::ProviderThrottle,
    child_session_id: &str,
    status: &str,
    error_detail: Option<&str>,
    test_overrides: &SharedTestOverrides,
) {
    let (parent_id, summary) = {
        let st = lock_state(state);
        // Check if parent is already waiting on this child.
        if st.waited_sessions.contains(child_session_id) {
            return;
        }
        let child = st.db.get_session(child_session_id).ok().flatten();
        // Skip notification if the child session has notify_parent=false.
        if let Some(ref child_session) = child
            && !child_session.notify_parent
        {
            return;
        }
        let parent = child.and_then(|s| s.parent_id);
        let summary = st
            .db
            .get_messages(child_session_id)
            .ok()
            .map(|msgs| {
                let text = last_assistant_text(&msgs);
                if text.len() > 200 {
                    format!("{}...", truncate_str(&text, 200))
                } else {
                    text
                }
            })
            .unwrap_or_default();
        (parent, summary)
    };

    let pid = match parent_id {
        Some(pid) => pid,
        None => return,
    };

    let _ = error_detail; // reserved for future use
    let notice = format!(
        "Child session {} {}. Summary: {}",
        child_session_id,
        status,
        if summary.is_empty() {
            "(no output)".to_string()
        } else {
            summary
        }
    );

    queue_and_maybe_resume(
        state,
        session_locks,
        plugins,
        shutdown,
        throttle,
        &pid,
        &notice,
        &format!("child:{}", child_session_id),
        test_overrides,
    );
}

pub(super) fn last_assistant_text(messages: &[Message]) -> String {
    for msg in messages.iter().rev() {
        if let Message::Assistant(a) = msg {
            let text: String = a
                .content
                .iter()
                .filter_map(|c| match c {
                    crate::types::AssistantContent::Text(t) if !t.text.is_empty() => {
                        Some(t.text.as_str())
                    }
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            if !text.is_empty() {
                // Truncate to ~500 chars for summary
                return if text.len() > 500 {
                    format!("{}...", truncate_str(&text, 500))
                } else {
                    text
                };
            }
        }
    }
    String::new()
}

/// Update the session's phase and broadcast a Phase event to subscribers.
/// Also persists the phase to DB for meaningful transitions so it survives restarts.
pub(super) fn emit_phase(state: &SharedState, session_id: &str, phase: crate::types::AgentPhase) {
    {
        let mut st = lock_state(state);
        st.phases.insert(session_id.to_string(), phase);
        // Persist meaningful phase transitions to DB.
        let label = phase.label().trim_end_matches("...");
        if let Err(e) = st.db.update_phase(session_id, label) {
            eprintln!("warning: failed to persist phase for {}: {}", session_id, e);
        }
    }
    let resp = Response::Stream {
        event: Box::new(crate::types::StreamEvent::Phase { phase }),
    };
    broadcast_to_subscribers(state, session_id, &resp);
}

/// Wake all registered session-done waiters so they re-check completion.
pub(super) fn notify_session_done_waiters(state: &SharedState) {
    let mut st = lock_state(state);
    st.session_done_waiters.retain(|tx| {
        // Try to send; drop closed channels.
        !tx.is_closed() && {
            let _ = tx.try_send(());
            true
        }
    });
}

pub(super) fn broadcast_to_subscribers(state: &SharedState, session_id: &str, resp: &Response) {
    let mut st = lock_state(state);
    if let Some(subs) = st.subscribers.get_mut(session_id) {
        subs.retain(|tx| {
            match tx.try_send(resp.clone()) {
                Ok(()) => true,
                Err(smol::channel::TrySendError::Closed(_)) => false,
                Err(smol::channel::TrySendError::Full(_)) => {
                    eprintln!("warning: subscriber channel full, dropping message");
                    true // keep subscriber, just drop this message
                }
            }
        });
        if subs.is_empty() {
            st.subscribers.remove(session_id);
        }
    }
}

/// Auto-archive completed sessions that have `auto_archive=true`.
/// Called after WaitSessions/WaitAnySessions collects results.
pub(super) fn auto_archive_done_sessions(
    state: &SharedState,
    results: &[crate::protocol::SessionResult],
) {
    // Collect session IDs to archive while holding the lock
    let to_archive: Vec<String> = {
        let st = lock_state(state);
        results
            .iter()
            .filter(|r| r.status == "done")
            .filter(|r| {
                st.db
                    .get_session(&r.session_id)
                    .ok()
                    .flatten()
                    .is_some_and(|s| s.auto_archive)
            })
            .map(|r| r.session_id.clone())
            .collect()
    };
    // Send info messages and archive (each call locks internally)
    for sid in &to_archive {
        // Get subtree IDs so we can send info to all sessions in the tree
        let subtree_ids = {
            let st = lock_state(state);
            st.db
                .get_subtree_ids(sid)
                .unwrap_or_else(|_| vec![sid.clone()])
        };
        for tree_sid in &subtree_ids {
            queue_info_to_session(state, tree_sid, "Session archived.");
        }
        let st = lock_state(state);
        if let Err(e) = st.db.archive_session_tree(sid) {
            eprintln!("auto-archive session {} failed: {}", sid, e);
        }
    }
}
