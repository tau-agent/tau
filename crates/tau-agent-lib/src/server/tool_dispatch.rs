use std::sync::{Arc, Mutex};

use super::agent_runner::PluginExecutor;
use super::dispatch::{
    cancel_chat_impl, create_session_impl, get_messages_impl, get_session_ancestors_impl,
    get_session_info_impl, list_sessions_impl, project_stats_impl,
};
use super::notifications::{
    auto_archive_done_sessions, is_no_agent_loop_session, last_assistant_text,
    placeholder_no_agent_note, queue_and_maybe_resume, queue_info_to_session,
    record_message_to_log_session,
};
use super::state::{SessionLocks, SharedState, lock_state, session_lock};
use super::{SharedTestOverrides, ShutdownHandle};

/// Execute a tool directly on a session without triggering the agent loop.
/// Persists the tool call and result as messages for audit trail.
#[allow(clippy::too_many_arguments)]
pub(super) async fn execute_tool_impl(
    state: &SharedState,
    plugins: &Arc<Mutex<crate::plugin::PluginManager>>,
    session_locks: &SessionLocks,
    shutdown: &ShutdownHandle,
    throttle: &crate::throttle::ProviderThrottle,
    test_overrides: &SharedTestOverrides,
    session_id: &str,
    tool_name: &str,
    arguments: serde_json::Value,
    chat_spawn_tx: &smol::channel::Sender<(String, String)>,
) -> crate::protocol::Response {
    use crate::protocol::Response;
    use crate::types::*;

    // 1. Ensure session exists, get its cwd and project_name
    let (cwd, project_name) = {
        let st = lock_state(state);
        match st.db.get_session(session_id) {
            Ok(Some(stored)) => (
                stored.cwd.unwrap_or_else(|| "/tmp".to_string()),
                stored.project_name,
            ),
            Ok(None) => {
                return Response::Error {
                    message: format!("session not found: {}", session_id),
                };
            }
            Err(e) => {
                return Response::Error {
                    message: e.to_string(),
                };
            }
        }
    };

    // 2. Ensure session plugins are spawned
    {
        let mut pm = plugins.lock().expect("plugins mutex poisoned");
        match pm.ensure_session_plugins(session_id, &cwd, project_name.as_deref(), None) {
            Ok(failures) => {
                for msg in &failures {
                    queue_info_to_session(state, session_id, msg);
                }
            }
            Err(e) => tracing::warn!(%e, "execute_tool: failed to spawn session plugins"),
        }
    }

    // 3. Construct a ToolCall with a generated ID
    let tool_call = ToolCall {
        id: format!("et_{}", crate::types::timestamp_ms()),
        name: tool_name.to_string(),
        arguments: arguments.clone(),
    };

    // 4. Persist the assistant message containing the tool call
    let assistant_msg = Message::Assistant(AssistantMessage {
        content: vec![AssistantContent::ToolCall(tool_call.clone())],
        api: "execute_tool".to_string(),
        provider: "execute_tool".to_string(),
        model: "execute_tool".to_string(),
        response_id: None,
        usage: Usage::default(),
        stop_reason: StopReason::ToolUse,
        error_message: None,
        timestamp: timestamp_ms(),
    });
    {
        let st = lock_state(state);
        if let Err(e) = st.db.append_message(session_id, &assistant_msg) {
            tracing::warn!(%e, "execute_tool: db error persisting assistant message");
        }
    }

    // 5. Execute via the PluginExecutor (or mock)
    let (output_tx, _output_rx) = smol::channel::unbounded::<String>();
    // Resolve a cancel token: use the session's cancel_flag if one exists
    // *and* it isn't currently tripped, so Ctrl-C cancels this direct-execute
    // call the same way it would a tool call from the agent loop.
    //
    // If the session's flag happens to already be `true` (e.g. a prior
    // Chat was cancelled and the flag wasn't cleared), we don't want to
    // short-circuit this direct call — use a fresh never-cancelled token.
    // Callers that truly want cancellation to reach this path should
    // trip the flag *after* issuing the ExecuteTool request.
    let cancel_token = {
        let st = lock_state(state);
        match st.cancel_flags.get(session_id) {
            Some(flag) if !flag.load(std::sync::atomic::Ordering::Relaxed) => {
                tau_agent_base::types::CancelToken::from_flag(flag.clone())
            }
            _ => tau_agent_base::types::CancelToken::new(),
        }
    };
    let result = if let Some(ref factory) = test_overrides.tool_executor_factory {
        let mut executor = factory();
        executor
            .execute(&tool_call, &output_tx, &cancel_token)
            .await
    } else {
        let mut executor: Box<dyn crate::worker::ToolExecutor> = Box::new(PluginExecutor {
            plugins: plugins.clone(),
            state: state.clone(),
            session_locks: session_locks.clone(),
            chat_spawn_tx: chat_spawn_tx.clone(),
            shutdown: shutdown.clone(),
            throttle: throttle.clone(),
            session_id: session_id.to_string(),
            cwd: cwd.clone(),
            project_name,
            test_overrides: test_overrides.clone(),
        });
        executor
            .execute(&tool_call, &output_tx, &cancel_token)
            .await
    };

    // 6. Build result message and persist
    let tool_result_msg = match result {
        Ok(tr) => tr,
        Err(e) => ToolResultMessage {
            tool_call_id: tool_call.id.clone(),
            tool_name: tool_name.to_string(),
            content: vec![ToolResultContent::Text(TextContent {
                text: format!("executor error: {}", e),
                text_signature: None,
            })],
            details: None,
            is_error: true,
            timestamp: timestamp_ms(),
            duration_ms: None,
            summary: None,
            post_persist_actions: Vec::new(),
        },
    };

    let is_error = tool_result_msg.is_error;
    let content_text: String = tool_result_msg
        .content
        .iter()
        .filter_map(|c| match c {
            ToolResultContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");

    // Persist tool result
    {
        let st = lock_state(state);
        if let Err(e) = st
            .db
            .append_message(session_id, &Message::ToolResult(tool_result_msg))
        {
            tracing::warn!(%e, "execute_tool: db error persisting tool result");
        }
    }

    Response::ToolExecuted {
        content: content_text,
        is_error,
    }
}

/// Handle a server request asynchronously (for plugin ServerRequest tunnel).
/// Only handles the subset of requests that make sense in a plugin context.
#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_server_request(
    state: &SharedState,
    session_locks: &SessionLocks,
    plugins: &Arc<Mutex<crate::plugin::PluginManager>>,
    shutdown: &ShutdownHandle,
    throttle: &crate::throttle::ProviderThrottle,
    chat_spawn_tx: &smol::channel::Sender<(String, String)>,
    test_overrides: &SharedTestOverrides,
    req: &crate::protocol::Request,
    session_id: &str,
) -> crate::protocol::Response {
    use crate::protocol::{Request, Response};
    match req {
        Request::CreateSession {
            model: model_id,
            provider: provider_name,
            system_prompt,
            cwd,
            parent_id,
            child_budget,
            tagline,
            auto_archive,
            notify_parent,
            project_name,
            sandbox_profile: _,
        } => create_session_impl(
            state,
            model_id,
            provider_name,
            system_prompt,
            cwd,
            parent_id,
            *child_budget,
            tagline,
            *auto_archive,
            *notify_parent,
            project_name,
        ),
        Request::GetSessionInfo { session_id } => get_session_info_impl(state, session_id),
        Request::GetSessionAncestors { session_id } => {
            get_session_ancestors_impl(state, session_id)
        }
        Request::GetMessages { session_id } => get_messages_impl(state, session_id),
        Request::ListSessions {
            include_archived,
            project_name,
        } => list_sessions_impl(state, *include_archived, project_name.as_deref()),
        Request::CancelChat {
            session_id,
            caller_session_id,
        } => cancel_chat_impl(
            state,
            session_id,
            session_locks,
            caller_session_id.as_deref(),
        ),
        Request::Chat { session_id, text } => {
            match chat_spawn_tx.send((session_id.clone(), text.clone())).await {
                Ok(()) => Response::Ok,
                Err(e) => Response::Error {
                    message: format!("failed to queue chat: {}", e),
                },
            }
        }
        Request::WaitSessions {
            session_ids,
            timeout_secs,
        } => {
            let deadline =
                std::time::Instant::now() + std::time::Duration::from_secs(*timeout_secs);
            let mut results = Vec::new();

            // Register a waiter channel to be notified on session completion.
            let (notify_tx, notify_rx) = smol::channel::bounded::<()>(1);
            {
                let mut st = lock_state(state);
                for sid in session_ids {
                    st.waited_sessions.insert(sid.clone());
                }
                st.session_done_waiters.push(notify_tx);
            }

            loop {
                let mut all_done = true;
                results.clear();

                for sid in session_ids {
                    let lock = session_lock(session_locks, sid);
                    let is_busy = lock.try_lock().is_none();

                    if is_busy {
                        all_done = false;
                        results.push(crate::protocol::SessionResult {
                            session_id: sid.clone(),
                            status: "busy".into(),
                            summary: String::new(),
                        });
                    } else {
                        let st = lock_state(state);
                        let (status, summary) = match st.db.get_session(sid) {
                            Ok(Some(_)) => {
                                let msgs = st.db.get_messages(sid).unwrap_or_default();
                                ("done".to_string(), last_assistant_text(&msgs))
                            }
                            _ => ("deleted".to_string(), String::new()),
                        };
                        results.push(crate::protocol::SessionResult {
                            session_id: sid.clone(),
                            status,
                            summary,
                        });
                    }
                }

                if all_done || std::time::Instant::now() >= deadline {
                    if !all_done {
                        for r in &mut results {
                            if r.status == "busy" {
                                r.status = "timeout".into();
                            }
                        }
                    }
                    break;
                }

                // Wait for a session-done notification or timeout.
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                let _ = futures::future::select(
                    std::pin::pin!(notify_rx.recv()),
                    std::pin::pin!(smol::Timer::after(remaining)),
                )
                .await;
            }

            // Remove from waited set and drop our notifier.
            {
                let mut st = lock_state(state);
                for sid in session_ids {
                    st.waited_sessions.remove(sid);
                }
            }
            drop(notify_rx);

            auto_archive_done_sessions(state, &results);
            Response::SessionsCompleted { results }
        }
        Request::WaitAnySessions {
            session_ids,
            timeout_secs,
        } => {
            let deadline =
                std::time::Instant::now() + std::time::Duration::from_secs(*timeout_secs);
            let results;

            // Register a waiter channel to be notified on session completion.
            let (notify_tx, notify_rx) = smol::channel::bounded::<()>(1);
            {
                let mut st = lock_state(state);
                for sid in session_ids {
                    st.waited_sessions.insert(sid.clone());
                }
                st.session_done_waiters.push(notify_tx);
            }

            loop {
                let mut done = Vec::new();

                for sid in session_ids {
                    let lock = session_lock(session_locks, sid);
                    let is_busy = lock.try_lock().is_none();

                    if !is_busy {
                        let st = lock_state(state);
                        let (status, summary) = match st.db.get_session(sid) {
                            Ok(Some(_)) => {
                                let msgs = st.db.get_messages(sid).unwrap_or_default();
                                ("done".to_string(), last_assistant_text(&msgs))
                            }
                            _ => ("deleted".to_string(), String::new()),
                        };
                        done.push(crate::protocol::SessionResult {
                            session_id: sid.clone(),
                            status,
                            summary,
                        });
                    }
                }

                if !done.is_empty() || std::time::Instant::now() >= deadline {
                    if done.is_empty() {
                        results = session_ids
                            .iter()
                            .map(|sid| crate::protocol::SessionResult {
                                session_id: sid.clone(),
                                status: "timeout".into(),
                                summary: String::new(),
                            })
                            .collect();
                    } else {
                        results = done;
                    }
                    break;
                }

                // Wait for a session-done notification or timeout.
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                let _ = futures::future::select(
                    std::pin::pin!(notify_rx.recv()),
                    std::pin::pin!(smol::Timer::after(remaining)),
                )
                .await;
            }

            // Remove from waited set and drop our notifier.
            {
                let mut st = lock_state(state);
                for sid in session_ids {
                    st.waited_sessions.remove(sid);
                }
            }
            drop(notify_rx);

            auto_archive_done_sessions(state, &results);
            Response::SessionsCompleted { results }
        }
        Request::QueueMessage {
            target_session_id,
            content,
            sender_info,
            await_reply,
            reply_to: _,
        } => {
            // Short-circuit: placeholder (log-provider) sessions don't
            // run the agent loop. Record the message and return a note
            // via MessageReply (for await_reply) or OkWithNote (for
            // fire-and-forget). See task 582.
            if is_no_agent_loop_session(state, target_session_id) {
                record_message_to_log_session(state, target_session_id, content);
                let note = placeholder_no_agent_note(target_session_id);
                return if *await_reply {
                    Response::MessageReply { content: note }
                } else {
                    Response::OkWithNote { note }
                };
            }

            if *await_reply {
                let (msg_id, rx) = {
                    let mut st = lock_state(state);
                    st.next_msg_id += 1;
                    let id = format!("m{}", st.next_msg_id);
                    let (tx, rx) = smol::channel::bounded::<String>(1);
                    st.reply_waiters.insert(id.clone(), tx);
                    (id, rx)
                };

                let prefixed = format!(
                    "[Message from {}, msg_id={}, awaits reply]\n{}",
                    sender_info, msg_id, content
                );
                queue_and_maybe_resume(
                    state,
                    session_locks,
                    plugins,
                    shutdown,
                    throttle,
                    target_session_id,
                    &prefixed,
                    sender_info,
                    test_overrides,
                );

                // Wait with timeout.
                let timeout = std::time::Duration::from_secs(300);
                match futures::future::select(
                    std::pin::pin!(rx.recv()),
                    std::pin::pin!(smol::Timer::after(timeout)),
                )
                .await
                {
                    futures::future::Either::Left((Ok(reply), _)) => {
                        Response::MessageReply { content: reply }
                    }
                    _ => {
                        let mut st = lock_state(state);
                        st.reply_waiters.remove(&msg_id);
                        Response::Error {
                            message: format!("await_reply timed out (msg_id={})", msg_id),
                        }
                    }
                }
            } else {
                queue_and_maybe_resume(
                    state,
                    session_locks,
                    plugins,
                    shutdown,
                    throttle,
                    target_session_id,
                    content,
                    sender_info,
                    test_overrides,
                );
                Response::Ok
            }
        }
        Request::ReplyToMessage { msg_id, content } => {
            let result = {
                let mut st = lock_state(state);
                st.reply_waiters.remove(msg_id.as_str())
            };
            match result {
                Some(tx) => {
                    let _ = tx.send(content.clone()).await;
                    Response::Ok
                }
                None => Response::Error {
                    message: format!("no pending waiter for msg_id={}", msg_id),
                },
            }
        }
        Request::QueueInfo {
            target_session_id,
            text,
        } => {
            queue_info_to_session(state, target_session_id, text);
            Response::Ok
        }
        Request::EnqueuePostIdleAction { session_id, action } => {
            {
                let mut st = lock_state(state);
                st.post_idle_queue
                    .entry(session_id.clone())
                    .or_default()
                    .push(action.clone());
            }
            Response::Ok
        }
        Request::ArchiveSession {
            session_id,
            require_ancestor,
        } => {
            // If require_ancestor is set, verify the target is a descendant
            if let Some(ancestor) = require_ancestor {
                let st = lock_state(state);
                match st.db.is_descendant(session_id, ancestor) {
                    Ok(false) => {
                        return Response::Error {
                            message: format!(
                                "session {} is not a descendant of {}",
                                session_id, ancestor
                            ),
                        };
                    }
                    Err(e) => {
                        return Response::Error {
                            message: e.to_string(),
                        };
                    }
                    Ok(true) => {} // proceed
                }
            }

            // Validate: session must exist, get subtree IDs
            let subtree_ids = {
                let st = lock_state(state);
                match st.db.get_session(session_id) {
                    Ok(Some(_)) => match st.db.get_subtree_ids(session_id) {
                        Ok(ids) => ids,
                        Err(e) => {
                            return Response::Error {
                                message: e.to_string(),
                            };
                        }
                    },
                    Ok(None) => {
                        return Response::Error {
                            message: format!("session not found: {}", session_id),
                        };
                    }
                    Err(e) => {
                        return Response::Error {
                            message: e.to_string(),
                        };
                    }
                }
            };

            // Check all sessions in subtree are idle
            for sid in &subtree_ids {
                let lock = session_lock(session_locks, sid);
                if lock.try_lock().is_none() {
                    return Response::Error {
                        message: format!("cannot archive: session {} is busy", sid),
                    };
                }
            }

            // Send info messages before archiving
            for sid in &subtree_ids {
                queue_info_to_session(state, sid, "Session archived.");
            }

            // Archive in DB
            {
                let st = lock_state(state);
                if let Err(e) = st.db.archive_session_tree(session_id) {
                    return Response::Error {
                        message: e.to_string(),
                    };
                }
            }

            // Destroy session plugins for archived sessions
            {
                let mut pm = plugins.lock().expect("plugins mutex poisoned");
                for sid in &subtree_ids {
                    pm.destroy_session_plugins(sid);
                }
            }

            // Clean up in-memory state
            {
                let mut st = lock_state(state);
                for sid in &subtree_ids {
                    st.cancel_flags.remove(sid);
                    st.has_queued.remove(sid);
                    st.subscribers.remove(sid);
                    st.phases.remove(sid);
                }
            }

            Response::SessionArchived
        }
        Request::RestoreSession { session_id } => {
            // Validate: session must exist and be archived
            let st = lock_state(state);
            match st.db.get_session(session_id) {
                Ok(Some(s)) if !s.archived => {
                    return Response::Error {
                        message: format!("session {} is not archived", session_id),
                    };
                }
                Ok(Some(_)) => {}
                Ok(None) => {
                    return Response::Error {
                        message: format!("session not found: {}", session_id),
                    };
                }
                Err(e) => {
                    return Response::Error {
                        message: e.to_string(),
                    };
                }
            }
            // Restore in DB
            if let Err(e) = st.db.restore_session_tree(session_id) {
                return Response::Error {
                    message: e.to_string(),
                };
            }

            // Send info message to all sessions in the subtree after restoring
            let subtree_ids = match st.db.get_subtree_ids(session_id) {
                Ok(ids) => ids,
                Err(e) => {
                    tracing::warn!(%e, "failed to get subtree IDs for restore info");
                    vec![session_id.to_string()]
                }
            };
            drop(st);

            for sid in &subtree_ids {
                queue_info_to_session(state, sid, "Session restored.");
            }

            Response::SessionRestored
        }
        Request::FireHook { name, data } => {
            let mut pm = plugins.lock().expect("plugins mutex poisoned");
            pm.call_hook_excluding(session_id, name, data, None);
            Response::Ok
        }
        Request::ExecuteTool {
            session_id: target_session_id,
            tool_name,
            arguments,
        } => {
            execute_tool_impl(
                state,
                plugins,
                session_locks,
                shutdown,
                throttle,
                test_overrides,
                target_session_id,
                tool_name,
                arguments.clone(),
                chat_spawn_tx,
            )
            .await
        }
        Request::SetTagline {
            session_id: target_session_id,
            tagline,
        } => {
            let st = lock_state(state);
            match st.db.update_tagline(target_session_id, tagline) {
                Ok(()) => Response::Ok,
                Err(e) => Response::Error {
                    message: e.to_string(),
                },
            }
        }
        Request::TaskList {
            project,
            state: state_filter,
            parent_id,
        } => super::task_handlers::handle_task_list(
            state,
            project,
            state_filter.as_deref(),
            *parent_id,
        ),
        Request::TaskGet { id } => super::task_handlers::handle_task_get(state, *id),
        Request::TaskCreate {
            project,
            title,
            parent_id,
            priority,
            tags,
            sandbox_profile,
        } => super::task_handlers::handle_task_create(
            project,
            title,
            *parent_id,
            *priority,
            tags,
            sandbox_profile.as_deref(),
        ),
        Request::TaskUpdate {
            id,
            state: new_state,
            title,
            priority,
            tags,
            affected_files,
            skip_review,
            require_approval,
            sandbox_profile,
        } => super::task_handlers::handle_task_update(
            *id,
            new_state.clone(),
            title.clone(),
            *priority,
            tags.clone(),
            affected_files.clone(),
            *skip_review,
            *require_approval,
            sandbox_profile.clone(),
        ),
        Request::TaskSearch {
            project,
            query,
            state: state_filter,
        } => super::task_handlers::handle_task_search(project, query, state_filter.as_deref()),
        Request::TaskAssign {
            id,
            session_id: assign_session_id,
        } => super::task_handlers::handle_task_assign(*id, assign_session_id),
        Request::TaskStatus { project } => super::task_handlers::handle_task_status(project),
        Request::TaskOverview {
            project,
            recent_limit,
        } => super::task_handlers::handle_task_overview(state, project, *recent_limit),
        Request::TaskMergeQueue { project } => {
            super::task_handlers::handle_task_merge_queue(project)
        }
        Request::ProjectStats { project_name } => project_stats_impl(state, project_name),
        _ => Response::Error {
            message: "request not supported in plugin context".into(),
        },
    }
}
